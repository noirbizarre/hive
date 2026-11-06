//! Static registration and verified connection drivers do not own agent sessions.

use super::*;
use hive_core::identity::NodeId;
use hive_protocol::{
    remote::{Peer, RemoteError},
    transport::Credentials,
};

pub(super) struct Link {
    pub peer: Peer,
    task: tokio::task::JoinHandle<()>,
}

impl Drop for Link {
    fn drop(&mut self) {
        // Ending an attachment must never be translated into session/close.
        self.task.abort();
    }
}

impl Link {
    pub fn disconnect(&self) {
        self.task.abort();
    }
    pub fn alive(&self) -> bool {
        !self.task.is_finished()
    }
}

pub(super) async fn connect(
    broker: &Broker,
    owner: &NodeId,
    bridge: Option<Arc<Mutex<routing::Bridge>>>,
) -> Result<Arc<Link>, Error> {
    if broker
        .registry
        .lock()
        .map_err(|_| Error::internal_error())?
        .stopping
    {
        return Err(RemoteError::OwnerUnavailable.wire());
    }
    let registration = broker
        .config()
        .peers
        .values()
        .find(|peer| &peer.owner == owner)
        .cloned()
        .ok_or_else(|| RemoteError::OwnerUnavailable.wire())?;
    let transport = Credentials::from_env(registration.auth_token_env.as_deref())
        .and_then(|credentials| credentials.peer(&registration.endpoint))
        .map_err(|_| RemoteError::OwnerUnavailable.wire())?;
    let capabilities = bridge
        .as_ref()
        .and_then(|bridge| bridge.lock().ok().map(|state| state.capabilities.clone()))
        .unwrap_or_default();
    let (ready, receive) = oneshot::channel();
    let expected = owner.clone();
    // SDK child tasks propagate failures to their parent; supervise peers independently.
    let task = tokio::spawn(async move {
        let requests = bridge.clone();
        let notifications = bridge.clone();
        let _ = Client
            .v2()
            .on_receive_request(
                async move |mut request: agent_client_protocol::UntypedMessage,
                            responder: Responder<serde_json::Value>,
                            _: V2ConnectionTo<Agent>| {
                    if !matches!(
                        request.method.as_str(),
                        "session/request_permission" | "elicitation/create"
                    ) {
                        return responder.respond_with_error(Error::method_not_found());
                    }
                    let Some(bridge) = &requests else {
                        return responder.respond_with_error(RemoteError::Unsupported.wire());
                    };
                    let upstream = {
                        let mut state = bridge.lock().map_err(|_| Error::internal_error())?;
                        state.callback(&mut request.params)?;
                        state.upstream.clone()
                    };
                    upstream
                        .prepare_request(request)
                        .forward_response_to(responder)
                },
                agent_client_protocol::on_receive_request!(),
            )
            .on_receive_notification(
                async move |mut notification: agent_client_protocol::UntypedMessage,
                            _: V2ConnectionTo<Agent>| {
                    if !matches!(
                        notification.method.as_str(),
                        "session/update" | "elicitation/complete"
                    ) {
                        return Ok(());
                    }
                    if let Some(bridge) = &notifications {
                        let upstream = {
                            let mut state = bridge.lock().map_err(|_| Error::internal_error())?;
                            state.callback(&mut notification.params)?;
                            state.upstream.clone()
                        };
                        // Upstream detach is normal; it must not kill the owner session.
                        let _ = upstream.send_notification(notification);
                    }
                    Ok(())
                },
                agent_client_protocol::on_receive_notification!(),
            )
            .connect_with(transport, async move |cx| {
                let initialized = Peer::initialize(
                    cx.clone(),
                    v2::InitializeRequest::new(
                        ProtocolVersion::V2,
                        v2::Implementation::new("hive-peer", env!("CARGO_PKG_VERSION")),
                    )
                    .capabilities(capabilities),
                )
                .await;
                let initialized = initialized.and_then(|peer| {
                    if peer.node != expected.as_str() {
                        Err(RemoteError::StaleRoute.wire())
                    } else if bridge.is_some() && !peer.can_route() {
                        Err(RemoteError::Unsupported.wire())
                    } else {
                        Ok(peer)
                    }
                });
                let success = initialized.is_ok();
                let _ = ready.send(initialized);
                if success {
                    tokio::select! { _ = cx.incoming_closed() => {}, _ = cx.terminated() => {} }
                }
                Ok(())
            })
            .await;
    });
    // Cancellation during initialization otherwise drops a JoinHandle without ending its driver.
    let mut pending = AbortOnDrop(Some(task.abort_handle()));
    match tokio::time::timeout(std::time::Duration::from_secs(20), receive).await {
        Ok(Ok(Ok(peer))) => {
            // Publish under admission so shutdown cannot drain just before this link appears.
            let registry = broker
                .registry
                .lock()
                .map_err(|_| Error::internal_error())?;
            if registry.stopping {
                return Err(RemoteError::OwnerUnavailable.wire());
            }
            pending.0 = None;
            let link = Arc::new(Link { peer, task });
            let mut links = broker
                .peer_links
                .lock()
                .map_err(|_| Error::internal_error())?;
            links.retain(|link| link.strong_count() > 0);
            links.push(Arc::downgrade(&link));
            drop(registry);
            Ok(link)
        }
        result => {
            task.abort();
            Err(match result {
                Ok(Ok(Err(error))) => error,
                _ => RemoteError::OwnerUnavailable.wire(),
            })
        }
    }
}

/// Each upstream connection owns its discovery links and their cursor snapshots.
#[derive(Default)]
pub(super) struct Discovery {
    links: tokio::sync::Mutex<BTreeMap<NodeId, Arc<Link>>>,
}

impl Discovery {
    pub async fn detach(&self) {
        self.links.lock().await.clear();
    }
    pub async fn inventory(
        &self,
        broker: &Broker,
        owner: NodeId,
        profiles: bool,
        raw: serde_json::Value,
    ) -> Result<serde_json::Value, Error> {
        if ["cursor", "limit"]
            .iter()
            .any(|key| raw.get(key).is_some_and(serde_json::Value::is_null))
        {
            return Err(RemoteError::InvalidParams.wire());
        }
        let params: hive_protocol::remote::ListParams =
            serde_json::from_value(raw).map_err(|_| RemoteError::InvalidParams.wire())?;
        params.validate()?;
        let mut links = self.links.lock().await;
        if links.get(&owner).is_some_and(|link| !link.alive()) {
            links.remove(&owner);
        }
        if !links.contains_key(&owner) {
            if params.cursor.is_some() {
                return Err(RemoteError::InvalidParams.wire());
            }
            links.insert(owner.clone(), connect(broker, &owner, None).await?);
        }
        let peer = &links[&owner].peer;
        tokio::time::timeout(std::time::Duration::from_secs(20), async {
            if profiles {
                serde_json::to_value(peer.list_profiles(params).await?)
            } else {
                serde_json::to_value(peer.list(params).await?)
            }
            .map_err(Error::into_internal_error)
        })
        .await
        .map_err(|_| RemoteError::OwnerUnavailable.wire())?
    }
}
