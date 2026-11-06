//! Remote indexes describe locations; only the owning Hive grants session authority.

use super::*;
use hive_core::identity::NodeId;
use hive_protocol::remote::{RemoteError, SessionLocation, SessionReference};
use serde::{Deserialize, de::DeserializeOwned};
use serde_json::Value;

#[derive(Clone, Serialize, Deserialize)]
pub(super) struct Route {
    alias: String,
    session: SessionReference,
    cwd: v2::AbsolutePath,
    #[serde(default = "retained_presence")]
    present: bool,
    // Local observation order prevents an older listing from revoking a newer positive lookup.
    #[serde(skip)]
    observed: u64,
}

fn retained_presence() -> bool {
    true
}

#[derive(Default)]
pub(super) struct Routes(Mutex<Vec<Route>>);

impl Routes {
    pub fn load(store: &Store) -> Result<Self, persistence::StoreError> {
        let routes: Vec<Route> = store
            .resource("remote_routes")
            .map_err(|_| persistence::StoreError::Routing)?
            .unwrap_or_default();
        let mut aliases = std::collections::BTreeSet::new();
        let mut references = std::collections::BTreeSet::new();
        for route in &routes {
            // Corrupt routing metadata must never make an alias select a local session or another owner.
            if route
                .alias
                .strip_prefix("hive-route-")
                .is_none_or(|suffix| uuid::Uuid::parse_str(suffix).is_err())
                || !aliases.insert(&route.alias)
                || !references.insert((&route.session.owner, &route.session.id))
                || route.session.validate().is_err()
                || !hive_protocol::remote::owner_path_is_absolute(&route.cwd.0)
            {
                return Err(persistence::StoreError::Routing);
            }
        }
        Ok(Self(Mutex::new(routes)))
    }

    fn get(&self, alias: &str) -> Result<Route, Error> {
        self.0
            .lock()
            .map_err(|_| Error::internal_error())?
            .iter()
            .find(|route| route.alias == alias)
            .cloned()
            .ok_or_else(|| RemoteError::StaleRoute.wire())
    }

    fn remember(
        &self,
        broker: &Broker,
        location: SessionLocation,
        alias: Option<String>,
    ) -> Result<Route, Error> {
        location.session.validate()?;
        if !hive_protocol::remote::owner_path_is_absolute(&location.cwd.0) {
            return Err(RemoteError::StaleRoute.wire());
        }
        let mut current = self.0.lock().map_err(|_| Error::internal_error())?;
        if let Some(route) = current
            .iter_mut()
            .find(|route| route.session == location.session)
        {
            // Re-listing unchanged sessions must not rewrite the entire durable route cache.
            if route.cwd == location.cwd && route.present {
                route.observed = route.observed.wrapping_add(1);
                return Ok(route.clone());
            }
        }
        let mut routes = current.clone();
        if let Some(route) = routes
            .iter_mut()
            .find(|route| route.session == location.session)
        {
            route.cwd = location.cwd;
            route.present = true;
            route.observed = route.observed.wrapping_add(1);
            let route = route.clone();
            if let Some(store) = &broker.store {
                store
                    .save_resource("remote_routes", &routes)
                    .map_err(|_| storage_error(persistence::StoreError::Routing))?;
            }
            *current = routes;
            return Ok(route);
        }
        let route = Route {
            alias: alias.unwrap_or_else(|| format!("hive-route-{}", uuid::Uuid::new_v4())),
            session: location.session,
            cwd: location.cwd,
            present: true,
            observed: 0,
        };
        routes.push(route.clone());
        // Route cache writes never touch local worker records or recovery journals.
        if let Some(store) = &broker.store {
            store
                .save_resource("remote_routes", &routes)
                .map_err(|_| storage_error(persistence::StoreError::Routing))?;
        }
        // Publish only after persistence succeeds, so a failed lookup cannot leave a usable alias.
        *current = routes;
        Ok(route)
    }

    fn reconcile(
        &self,
        broker: &Broker,
        baseline: &[Route],
        seen: &std::collections::BTreeSet<String>,
    ) -> Result<(), Error> {
        let mut current = self.0.lock().map_err(|_| Error::internal_error())?;
        let mut next = current.clone();
        let mut changed = false;
        for route in &mut next {
            if route.present
                && !seen.contains(&route.session.id)
                && baseline
                    .iter()
                    .any(|old| old.alias == route.alias && old.observed == route.observed)
            {
                route.present = false;
                route.observed = route.observed.wrapping_add(1);
                changed = true;
            }
        }
        if changed {
            if let Some(store) = &broker.store {
                store
                    .save_resource("remote_routes", &next)
                    .map_err(|_| storage_error(persistence::StoreError::Routing))?;
            }
            *current = next;
        }
        Ok(())
    }

    fn absent(&self, broker: &Broker, session: &SessionReference) -> Result<(), Error> {
        let baseline = self
            .0
            .lock()
            .map_err(|_| Error::internal_error())?
            .iter()
            .filter(|route| &route.session == session)
            .cloned()
            .collect::<Vec<_>>();
        self.reconcile(broker, &baseline, &Default::default())
    }
}

pub(super) struct Bridge {
    pub upstream: V2ConnectionTo<Client>,
    pub capabilities: v2::ClientCapabilities,
    alias: String,
    owner_token: Option<Value>,
    gateway_token: Option<Value>,
    generation: Option<Value>,
    parents: HashMap<v1::RequestId, v1::RequestId>,
    urls: HashMap<String, (String, tokio::time::Instant)>,
    creation: Option<(Broker, NodeId, v2::AbsolutePath)>,
}

impl Bridge {
    pub fn callback(&mut self, params: &mut Value) -> Result<(), Error> {
        if params.get("sessionId").is_some() {
            if let Some((broker, owner, cwd)) = &self.creation {
                let id = params["sessionId"]
                    .as_str()
                    .filter(|id| !id.is_empty())
                    .ok_or_else(|| RemoteError::StaleRoute.wire())?;
                // Discovery and early startup events must agree on one alias before publication.
                self.alias = broker
                    .routes
                    .remember(
                        broker,
                        SessionLocation {
                            session: SessionReference {
                                owner: owner.clone(),
                                id: id.into(),
                            },
                            session_id: id.into(),
                            cwd: cwd.clone(),
                        },
                        None,
                    )?
                    .alias;
            }
            params["sessionId"] = self.alias.clone().into();
        }
        // ACP flattens elicitation scope into the request, rather than a nested `scope` object.
        if let Some(request_id) = params.get_mut("requestId") {
            let id = serde_json::from_value::<v1::RequestId>(request_id.clone())
                .map_err(|_| RemoteError::InvalidParams.wire())?;
            *request_id =
                serde_json::to_value(self.parents.get(&id).ok_or_else(|| {
                    Error::invalid_request().data("the parent request has ended")
                })?)
                .map_err(Error::into_internal_error)?;
        }
        if let Some(id) = params.get_mut("elicitationId") {
            let now = tokio::time::Instant::now();
            self.urls.retain(|_, (_, expires)| *expires > now);
            let original = id
                .as_str()
                .ok_or_else(|| RemoteError::InvalidParams.wire())?;
            if !self.urls.contains_key(original) && self.urls.len() >= 128 {
                return Err(Error::invalid_request()
                    .data("wait for outstanding URL interactions to settle"));
            }
            let mapped = self.urls.entry(original.to_owned()).or_insert_with(|| {
                (
                    uuid::Uuid::new_v4().to_string(),
                    now + std::time::Duration::from_secs(120),
                )
            });
            *id = mapped.0.clone().into();
        }
        Ok(())
    }

    fn response(&mut self, value: &mut Value) {
        if value.get("sessionId").is_some() {
            value["sessionId"] = self.alias.clone().into();
        }
        if let Some(hive) = value.pointer_mut("/_meta/hive") {
            self.owner_token = hive.get("token").cloned();
            self.generation = hive.get("generation").cloned();
            self.gateway_token = self
                .owner_token
                .as_ref()
                .map(|_| Value::from(uuid::Uuid::new_v4().to_string()));
            if let Some(token) = &self.gateway_token {
                hive["token"] = token.clone();
            }
        }
    }

    fn authority(&self, params: &mut Value) -> Result<(), Error> {
        let hive = &mut params["_meta"]["hive"];
        if self.gateway_token.is_none()
            || hive.get("token") != self.gateway_token.as_ref()
            || hive.get("generation") != self.generation.as_ref()
        {
            return Err(Error::invalid_request()
                .data("resume as pilot and use this attachment's current generation and token"));
        }
        hive["token"] = self.owner_token.clone().unwrap_or(Value::Null);
        Ok(())
    }
}

struct Attachment {
    link: Arc<peers::Link>,
    bridge: Arc<Mutex<Bridge>>,
}

#[derive(Default)]
pub(super) struct ClientRoutes {
    attachments: Arc<Mutex<HashMap<String, Arc<Attachment>>>>,
    setup: Mutex<HashMap<String, std::sync::Weak<tokio::sync::Mutex<()>>>>,
    detached: Arc<std::sync::atomic::AtomicBool>,
    pub discovery: peers::Discovery,
}

pub(super) fn routed(id: &v2::SessionId) -> bool {
    id.to_string().starts_with("hive-route-")
}

impl Broker {
    pub(super) async fn listed(
        &self,
        request: v2::ListSessionsRequest,
        negotiated: bool,
    ) -> Result<v2::ListSessionsResponse, Error> {
        let node = self.owner_node().await?;
        let local_only = match request
            .meta
            .as_ref()
            .and_then(|meta| meta.get("hive"))
            .and_then(|hive| hive.get("ownerOnly"))
        {
            None => false,
            Some(Value::Bool(value)) => *value,
            Some(_) => return Err(RemoteError::InvalidParams.wire()),
        };
        if local_only && !negotiated {
            return Err(RemoteError::Unsupported.wire());
        }
        let snapshot = self.snapshot(request.cwd.as_ref())?;
        let mut entries = snapshot
            .sessions
            .into_iter()
            .map(|summary| {
                let mut hive =
                    serde_json::to_value(&summary).map_err(Error::into_internal_error)?;
                hive["running"] = matches!(summary.activity, "running" | "requires_action").into();
                hive["pilot"] = summary.control_owner.is_some().into();
                if negotiated {
                    hive["session"] = serde_json::json!({"owner": node, "id": summary.session_id});
                }
                let mut meta = v2::Meta::new();
                meta.insert("hive".into(), hive);
                Ok(
                    v2::SessionInfo::new(v2::SessionId::new(summary.session_id), summary.cwd)
                        .meta(meta),
                )
            })
            .collect::<Result<Vec<_>, Error>>()?;
        let mut unavailable = Vec::new();
        if !local_only && negotiated {
            let config = self.config();
            for registration in config.peers.values() {
                let baseline = self
                    .routes
                    .0
                    .lock()
                    .map_err(|_| Error::internal_error())?
                    .iter()
                    .filter(|route| {
                        route.session.owner == registration.owner
                            && request.cwd.as_ref().is_none_or(|cwd| cwd == &route.cwd)
                    })
                    .cloned()
                    .collect::<Vec<_>>();
                let result = async {
                    let link = peers::connect(self, &registration.owner, None).await?;
                    if !link.peer.can_route() {
                        return Err(RemoteError::Unsupported.wire());
                    }
                    let mut owner_request = request.clone();
                    owner_request
                        .meta
                        .get_or_insert_default()
                        .insert("hive".into(), serde_json::json!({"ownerOnly":true}));
                    let response = tokio::time::timeout(
                        std::time::Duration::from_secs(20),
                        link.peer
                            .connection()
                            .prepare_request(owner_request)
                            .block_task(),
                    )
                    .await
                    .map_err(|_| RemoteError::OwnerUnavailable.wire())??;
                    let mut remote = Vec::new();
                    let mut seen = std::collections::BTreeSet::new();
                    let complete = response.next_cursor.is_none() && request.cursor.is_none();
                    for mut entry in response.sessions {
                        let session: SessionReference = serde_json::from_value(
                            entry
                                .meta
                                .as_ref()
                                .and_then(|meta| meta.get("hive"))
                                .and_then(|hive| hive.get("session"))
                                .cloned()
                                .unwrap_or(Value::Null),
                        )
                        .map_err(|_| RemoteError::StaleRoute.wire())?;
                        if session.owner != registration.owner
                            || session.id != entry.session_id.to_string()
                        {
                            return Err(RemoteError::StaleRoute.wire());
                        }
                        seen.insert(session.id.clone());
                        let route = self.routes.remember(
                            self,
                            SessionLocation {
                                session,
                                session_id: entry.session_id.to_string(),
                                cwd: entry.cwd.clone(),
                            },
                            None,
                        )?;
                        entry.session_id = v2::SessionId::new(route.alias);
                        remote.push(entry);
                    }
                    // Only a complete authoritative scope proves absence; aliases themselves remain stable.
                    if complete {
                        self.routes.reconcile(self, &baseline, &seen)?;
                    }
                    Ok::<_, Error>(remote)
                }
                .await;
                match result {
                    Ok(remote) => entries.extend(remote),
                    Err(_) => {
                        unavailable.push(registration.owner.clone());
                        // Cached presence survives a partition; it is never authoritative liveness.
                        for route in self
                            .routes
                            .0
                            .lock()
                            .map_err(|_| Error::internal_error())?
                            .iter()
                            .filter(|route| {
                                route.present && route.session.owner == registration.owner
                            })
                        {
                            if request.cwd.as_ref().is_some_and(|cwd| cwd != &route.cwd) {
                                continue;
                            }
                            let mut meta = v2::Meta::new();
                            meta.insert("hive".into(), serde_json::json!({"session":route.session,"availability":"owner_unavailable","cached":true}));
                            entries.push(
                                v2::SessionInfo::new(
                                    v2::SessionId::new(route.alias.clone()),
                                    route.cwd.clone(),
                                )
                                .meta(meta),
                            );
                        }
                    }
                }
            }
            for route in self
                .routes
                .0
                .lock()
                .map_err(|_| Error::internal_error())?
                .iter()
                .filter(|route| {
                    route.present
                        && !config
                            .peers
                            .values()
                            .any(|peer| peer.owner == route.session.owner)
                })
            {
                if request.cwd.as_ref().is_some_and(|cwd| cwd != &route.cwd) {
                    continue;
                }
                unavailable.push(route.session.owner.clone());
                let mut meta = v2::Meta::new();
                meta.insert("hive".into(), serde_json::json!({"session":route.session,"availability":"owner_unavailable","cached":true}));
                entries.push(
                    v2::SessionInfo::new(
                        v2::SessionId::new(route.alias.clone()),
                        route.cwd.clone(),
                    )
                    .meta(meta),
                );
            }
        }
        entries.sort_by_key(|entry| entry.session_id.to_string());
        entries.dedup_by(|left, right| left.session_id == right.session_id);
        let mut response = v2::ListSessionsResponse::new(entries);
        if !unavailable.is_empty() {
            unavailable.sort();
            unavailable.dedup();
            response.meta.get_or_insert_default().insert(
                "hive".into(),
                serde_json::json!({"partial":true,"unavailableOwners":unavailable}),
            );
        }
        Ok(response)
    }

    pub(super) async fn owner_node(&self) -> Result<NodeId, Error> {
        match &self.store {
            Some(store) => workspaces::Service::new(store.clone())
                .node()
                .await
                .map_err(|_| Error::internal_error()),
            None => Ok(self.mcp_ephemeral_node.clone()),
        }
    }

    pub(super) async fn location(
        &self,
        session: SessionReference,
    ) -> Result<SessionLocation, Error> {
        session.validate()?;
        if session.owner == self.owner_node().await? {
            let snapshot = self.snapshot(None)?;
            let summary = snapshot
                .sessions
                .into_iter()
                .find(|summary| summary.session_id == session.id)
                .ok_or_else(|| RemoteError::SessionNotFound.wire())?;
            return Ok(SessionLocation {
                session_id: session.id.clone(),
                session,
                cwd: summary.cwd,
            });
        }
        let link = peers::connect(self, &session.owner, None).await?;
        let location = tokio::time::timeout(
            std::time::Duration::from_secs(20),
            link.peer.resolve(session.clone()),
        )
        .await
        .map_err(|_| RemoteError::OwnerUnavailable.wire())?;
        let location = match location {
            Ok(location) => location,
            Err(error) => {
                if error
                    .data
                    .as_ref()
                    .and_then(|data| data.get("code"))
                    .and_then(Value::as_str)
                    == Some(RemoteError::SessionNotFound.code())
                {
                    self.routes.absent(self, &session)?;
                }
                return Err(error);
            }
        };
        if location.session != session || location.session_id != session.id {
            return Err(RemoteError::StaleRoute.wire());
        }
        let route = self.routes.remember(self, location, None)?;
        Ok(SessionLocation {
            session,
            session_id: route.alias,
            cwd: route.cwd,
        })
    }
}

impl ClientRoutes {
    fn session_gate(&self, alias: &str) -> Result<Arc<tokio::sync::Mutex<()>>, Error> {
        let mut gates = self.setup.lock().map_err(|_| Error::internal_error())?;
        gates.retain(|_, gate| gate.strong_count() > 0);
        if let Some(gate) = gates.get(alias).and_then(std::sync::Weak::upgrade) {
            return Ok(gate);
        }
        let gate = Arc::new(tokio::sync::Mutex::new(()));
        gates.insert(alias.into(), Arc::downgrade(&gate));
        Ok(gate)
    }

    fn retire(&self, attachment: &Arc<Attachment>) {
        if let Ok(mut attachments) = self.attachments.lock() {
            attachments.retain(|_, current| !Arc::ptr_eq(current, attachment));
        }
        if let Ok(mut bridge) = attachment.bridge.lock() {
            bridge.owner_token = None;
            bridge.gateway_token = None;
            bridge.generation = None;
            bridge.creation = None;
        }
        // Retiring a hop releases its resources without sending another session/close.
        attachment.link.disconnect();
    }
    pub async fn detach(&self) {
        self.detached
            .store(true, std::sync::atomic::Ordering::Release);
        if let Ok(mut attachments) = self.attachments.lock() {
            attachments.clear();
        }
        self.discovery.detach().await;
    }
    fn bridge(
        &self,
        broker: &Broker,
        client: &str,
        upstream: V2ConnectionTo<Client>,
        alias: String,
    ) -> Result<Arc<Mutex<Bridge>>, Error> {
        let capabilities = broker
            .client_capabilities
            .lock()
            .map_err(|_| Error::internal_error())?
            .get(client)
            .cloned()
            .unwrap_or_default();
        Ok(Arc::new(Mutex::new(Bridge {
            upstream,
            capabilities,
            alias,
            owner_token: None,
            gateway_token: None,
            generation: None,
            parents: HashMap::new(),
            urls: HashMap::new(),
            creation: None,
        })))
    }

    pub async fn create(
        &self,
        broker: &Broker,
        client: &str,
        upstream: V2ConnectionTo<Client>,
        mut raw: Value,
        responder: Responder<v2::NewSessionResponse>,
    ) -> Result<(), Error> {
        let setup = async {
            let owner: NodeId = serde_json::from_value(raw["_meta"]["hive"]["peer"].clone())
                .map_err(|_| RemoteError::InvalidParams.wire())?;
            raw["_meta"]["hive"]
                .as_object_mut()
                .ok_or_else(|| RemoteError::InvalidParams.wire())?
                .remove("peer");
            // Validate selectors here, but let the owner interpret its own filesystem paths.
            let request = remote::launch_remote(raw)?;
            let hive = request
                .meta
                .as_ref()
                .and_then(|meta| meta.get("hive"))
                .ok_or_else(|| RemoteError::InvalidParams.wire())?;
            let agent = hive.get("agent").and_then(Value::as_str);
            let profile = hive.get("profile").and_then(Value::as_str);
            if agent.is_none() && profile.is_none() {
                return Err(RemoteError::InvalidParams.wire());
            }
            let alias = format!("hive-route-{}", uuid::Uuid::new_v4());
            let bridge = self.bridge(broker, client, upstream, alias.clone())?;
            bridge.lock().map_err(|_| Error::internal_error())?.creation =
                Some((broker.clone(), owner.clone(), request.cwd.clone()));
            let link = peers::connect(broker, &owner, Some(bridge.clone())).await?;
            let prepared = if let Some(profile) = profile {
                link.peer
                    .prepare_start_profile(profile, agent, request.clone())?
            } else {
                link.peer
                    .prepare_start(agent.unwrap_or_default(), request.clone())?
            };
            Ok::<_, Error>((owner, request, bridge, link, prepared))
        }
        .await;
        let (owner, request, bridge, link, prepared) = match setup {
            Ok(setup) => setup,
            Err(error) => {
                let _ = responder.respond_with_error(error);
                return Ok(());
            }
        };
        let owner_connection = link.peer.connection().clone();
        let upstream_connection = bridge
            .lock()
            .map_err(|_| Error::internal_error())?
            .upstream
            .clone();
        let attachment = Arc::new(Attachment {
            link,
            bridge: bridge.clone(),
        });
        let publication = std::sync::Arc::downgrade(&attachment);
        let attachments = self.attachments.clone();
        let detached = self.detached.clone();
        let outgoing = prepared.id().clone();
        bridge
            .lock()
            .map_err(|_| Error::internal_error())?
            .parents
            .insert(outgoing.clone(), responder.id().clone());
        let (ready, receive) = oneshot::channel();
        let promoted = bridge.clone();
        let routes = broker.routes.clone();
        let broker = broker.clone();
        let registered = prepared
            .forward_cancellation_from(responder.cancellation())
            .on_receiving_result(async move |result| {
                let result = result
                    .and_then(|response| {
                        let mut value =
                            serde_json::to_value(response).map_err(Error::into_internal_error)?;
                        let session: SessionReference =
                            serde_json::from_value(value["_meta"]["hive"]["session"].clone())
                                .map_err(|_| RemoteError::StaleRoute.wire())?;
                        if session.owner != owner
                            || value["sessionId"].as_str() != Some(session.id.as_str())
                        {
                            return Err(RemoteError::StaleRoute.wire());
                        }
                        let cwd = value
                            .pointer("/_meta/hive/workspace/effectiveCwd")
                            .cloned()
                            .map(serde_json::from_value)
                            .transpose()
                            .map_err(|_| RemoteError::StaleRoute.wire())?
                            .unwrap_or(request.cwd);
                        let canonical = routes.remember(
                            &broker,
                            SessionLocation {
                                session,
                                session_id: value["sessionId"].as_str().unwrap_or_default().into(),
                                cwd,
                            },
                            None,
                        )?;
                        {
                            let mut bridge =
                                promoted.lock().map_err(|_| Error::internal_error())?;
                            bridge.alias = canonical.alias;
                            bridge.creation = None;
                            bridge.response(&mut value);
                        }
                        publish_attachment(&attachments, &detached, &publication)?;
                        serde_json::from_value(value).map_err(Error::into_internal_error)
                    })
                    .map_err(|error| RemoteError::sanitized(error, RemoteError::LaunchUncertain));
                // The upstream task owns its responder; dropping this callback closes the channel.
                let _ = ready.send(result);
                Ok(())
            });
        let result = match registered {
            Ok(()) => {
                owner_result(
                    receive,
                    &owner_connection,
                    &upstream_connection,
                    RemoteError::LaunchUncertain,
                )
                .await
            }
            Err(error) => Err(RemoteError::sanitized(error, RemoteError::OwnerUnavailable)),
        };
        bridge
            .lock()
            .map_err(|_| Error::internal_error())?
            .parents
            .remove(&outgoing);
        if result.is_err() {
            self.retire(&attachment);
        }
        let _ = responder.respond_with_result(result);
        Ok(())
    }

    pub async fn forward<T: agent_client_protocol::JsonRpcRequest + Serialize>(
        &self,
        broker: &Broker,
        client: &str,
        upstream: V2ConnectionTo<Client>,
        request: T,
        responder: Responder<T::Response>,
    ) -> Result<(), Error>
    where
        T::Response: DeserializeOwned,
    {
        let setup = async {
            let method = request.method().to_owned();
            let mut params = serde_json::to_value(request).map_err(Error::into_internal_error)?;
            let alias = params["sessionId"]
                .as_str()
                .ok_or_else(|| RemoteError::InvalidParams.wire())?
                .to_owned();
            let route = broker.routes.get(&alias)?;
            let resume = method == "session/resume";
            let close = method == "session/close";
            // Only competing resumes of this session share a gate; healthy sessions remain cancellable.
            let gate = if resume {
                Some(self.session_gate(&alias)?.lock_owned().await)
            } else {
                None
            };
            let existing = {
                let mut attachments = self
                    .attachments
                    .lock()
                    .map_err(|_| Error::internal_error())?;
                if attachments
                    .get(&alias)
                    .is_some_and(|attachment| !attachment.link.alive())
                {
                    attachments.remove(&alias);
                    if !resume {
                        return Err(RemoteError::OwnerUnavailable.wire());
                    }
                }
                attachments.get(&alias).cloned()
            };
            let fresh = existing.is_none();
            let attachment = if let Some(attachment) = existing {
                attachment
            } else {
                if !resume {
                    return Err(RemoteError::StaleRoute.wire());
                }
                let location = broker.location(route.session.clone()).await?;
                if location.session != route.session {
                    return Err(RemoteError::StaleRoute.wire());
                }
                let bridge = self.bridge(broker, client, upstream, alias.clone())?;
                let link =
                    peers::connect(broker, &route.session.owner, Some(bridge.clone())).await?;
                Arc::new(Attachment { link, bridge })
            };
            params["sessionId"] = route.session.id.clone().into();
            // Observe/takeover do not carry authority; relinquish does and must be translated.
            if !resume
                || params.pointer("/_meta/hive/mode").and_then(Value::as_str) == Some("relinquish")
            {
                attachment
                    .bridge
                    .lock()
                    .map_err(|_| Error::internal_error())?
                    .authority(&mut params)?;
            }
            let prepared = attachment
                .link
                .peer
                .connection()
                .prepare_request(agent_client_protocol::UntypedMessage::new(&method, params)?);
            Ok::<_, Error>((attachment, resume, close, fresh, route, prepared, gate))
        }
        .await;
        let (attachment, resume, close, fresh, route, prepared, _gate) = match setup {
            Ok(setup) => setup,
            Err(error) => {
                let _ = responder.respond_with_error(error);
                return Ok(());
            }
        };
        let owner_connection = attachment.link.peer.connection().clone();
        let upstream_connection = attachment
            .bridge
            .lock()
            .map_err(|_| Error::internal_error())?
            .upstream
            .clone();
        let outgoing = prepared.id().clone();
        attachment
            .bridge
            .lock()
            .map_err(|_| Error::internal_error())?
            .parents
            .insert(outgoing.clone(), responder.id().clone());
        let bridge = attachment.bridge.clone();
        let publication = Arc::downgrade(&attachment);
        let attachments = self.attachments.clone();
        let detached = self.detached.clone();
        let expected = route.session.clone();
        let (send, receive) = oneshot::channel();
        let registered = prepared
            .forward_cancellation_from(responder.cancellation())
            .on_receiving_result(async move |result| {
                let result = result.map_err(sanitize_mutation).and_then(|mut value| {
                    if resume {
                        let reference: SessionReference =
                            serde_json::from_value(value["_meta"]["hive"]["session"].clone())
                                .map_err(|_| RemoteError::StaleRoute.wire())?;
                        if reference != expected {
                            return Err(RemoteError::StaleRoute.wire());
                        }
                        bridge
                            .lock()
                            .map_err(|_| Error::internal_error())?
                            .response(&mut value);
                        if fresh {
                            publish_attachment(&attachments, &detached, &publication)?;
                        }
                    }
                    serde_json::from_value(value).map_err(Error::into_internal_error)
                });
                let _ = send.send(result);
                Ok(())
            });
        let mut result = match registered {
            Ok(()) => {
                owner_result(
                    receive,
                    &owner_connection,
                    &upstream_connection,
                    RemoteError::OwnerUnavailable,
                )
                .await
            }
            Err(error) => Err(sanitize_mutation(error)),
        };
        if let Ok(mut bridge) = attachment.bridge.lock() {
            bridge.parents.remove(&outgoing);
        }
        if (close && result.is_ok()) || (fresh && result.is_err()) {
            self.retire(&attachment);
        }
        if close
            && result.is_ok()
            && let Err(error) = broker.routes.absent(broker, &route.session)
        {
            result = Err(error);
        }
        let _ = responder.respond_with_result(result);
        Ok(())
    }

    pub async fn cancel(
        &self,
        broker: &Broker,
        mut request: v2::CancelSessionNotification,
    ) -> Result<(), Error> {
        let route = broker.routes.get(&request.session_id.to_string())?;
        let attachments = self
            .attachments
            .lock()
            .map_err(|_| Error::internal_error())?;
        if let Some(attachment) = attachments.get(&route.alias) {
            let mut params = serde_json::to_value(&request).map_err(Error::into_internal_error)?;
            if attachment
                .bridge
                .lock()
                .map_err(|_| Error::internal_error())?
                .authority(&mut params)
                .is_err()
            {
                return Ok(());
            }
            request = serde_json::from_value(params).map_err(Error::into_internal_error)?;
            request.session_id = v2::SessionId::new(route.session.id);
            let _ = attachment.link.peer.connection().send_notification(request);
        }
        Ok(())
    }
}

fn publish_attachment(
    attachments: &Mutex<HashMap<String, Arc<Attachment>>>,
    detached: &std::sync::atomic::AtomicBool,
    publication: &std::sync::Weak<Attachment>,
) -> Result<(), Error> {
    let attachment = publication
        .upgrade()
        .ok_or_else(|| RemoteError::OwnerUnavailable.wire())?;
    let alias = attachment
        .bridge
        .lock()
        .map_err(|_| Error::internal_error())?
        .alias
        .clone();
    let mut attachments = attachments.lock().map_err(|_| Error::internal_error())?;
    // Check while holding the map lock so detach cannot clear the map just before publication.
    if detached.load(std::sync::atomic::Ordering::Acquire) {
        return Err(RemoteError::OwnerUnavailable.wire());
    }
    attachments.insert(alias, attachment);
    Ok(())
}

async fn owner_result<R>(
    mut receive: oneshot::Receiver<Result<R, Error>>,
    owner: &V2ConnectionTo<Agent>,
    upstream: &V2ConnectionTo<Client>,
    failure: RemoteError,
) -> Result<R, Error> {
    // The responder lives on the upstream task, independently of the owner's callback task actor.
    tokio::select! {
        biased;
        result = &mut receive => result.unwrap_or_else(|_| Err(failure.wire())),
        _ = owner.terminated() => {
            // A response can win concurrently with termination; preserve already-promoted evidence.
            receive.try_recv().unwrap_or_else(|_| Err(failure.wire()))
        }
        // Clean upstream EOF can leave SDK tasks draining; release this hop without closing its session.
        _ = upstream.incoming_closed() => receive.try_recv().unwrap_or_else(|_| Err(failure.wire())),
    }
}

/// Preserve typed recovery evidence without accepting peer-supplied messages or arbitrary data.
fn sanitize_mutation(error: Error) -> Error {
    use hive_protocol::recovery::RecoveryError;
    let recovery = error.data.as_ref().and_then(|data| data.get("hive"));
    for kind in [
        RecoveryError::Uncertain,
        RecoveryError::NotDispatched,
        RecoveryError::KeyConflict,
        RecoveryError::Required,
        RecoveryError::Storage,
    ] {
        if recovery
            .and_then(|hive| hive.get("code"))
            .and_then(Value::as_str)
            == Some(kind.code())
        {
            return kind.wire(
                recovery
                    .and_then(|hive| hive.get("recoveryRequired"))
                    .and_then(Value::as_bool)
                    .unwrap_or(true),
            );
        }
    }
    if error
        .data
        .as_ref()
        .and_then(|data| data.get("code"))
        .and_then(Value::as_str)
        .is_some_and(|code| code.starts_with("hive::remote::"))
    {
        return RemoteError::sanitized(error, RemoteError::OwnerUnavailable);
    }
    let code: i32 = error.code.into();
    if matches!(code, -32800 | -32700 | -32602..=-32600 | -32000 | -32002) {
        Error::new(code, "The owner rejected this session request").data(serde_json::json!({"help":"Inspect current owner session state and attachment authority before explicitly retrying."}))
    } else {
        RemoteError::OwnerUnavailable.wire()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn corrupted_route_aliases_and_duplicate_references_are_rejected_before_gateway_dispatch() {
        let temporary = tempfile::tempdir().unwrap();
        let store = Store::open(temporary.path().join("state/catalog.sqlite3")).unwrap();
        let mut route = Route {
            alias: "hive-local-session".into(),
            session: SessionReference {
                owner: NodeId::new(),
                id: "retained".into(),
            },
            cwd: v2::AbsolutePath::new(std::env::current_dir().unwrap()),
            present: true,
            observed: 0,
        };
        store
            .save_resource("remote_routes", &vec![route.clone()])
            .unwrap();
        assert!(matches!(
            Routes::load(&store),
            Err(persistence::StoreError::Routing)
        ));
        route.alias = format!("hive-route-{}", uuid::Uuid::new_v4());
        let mut duplicate = route.clone();
        duplicate.alias = format!("hive-route-{}", uuid::Uuid::new_v4());
        store
            .save_resource("remote_routes", &vec![route, duplicate])
            .unwrap();
        assert!(matches!(
            Routes::load(&store),
            Err(persistence::StoreError::Routing)
        ));
    }

    #[test]
    fn identical_owner_local_ids_never_share_a_gateway_alias() {
        let broker = Broker::new(Config::default()).unwrap();
        let first = SessionReference {
            owner: NodeId::new(),
            id: "opaque-native-looking-id".into(),
        };
        let second = SessionReference {
            owner: NodeId::new(),
            id: first.id.clone(),
        };
        let location = |session| SessionLocation {
            session,
            session_id: "ignored".into(),
            cwd: v2::AbsolutePath::new(std::env::current_dir().unwrap()),
        };
        let left = broker
            .routes
            .remember(&broker, location(first.clone()), None)
            .unwrap();
        let right = broker
            .routes
            .remember(&broker, location(second.clone()), None)
            .unwrap();
        assert_ne!(left.alias, right.alias);
        assert_eq!(broker.routes.get(&left.alias).unwrap().session, first);
        assert_eq!(broker.routes.get(&right.alias).unwrap().session, second);
        assert_eq!(
            broker
                .routes
                .remember(&broker, location(first), None)
                .unwrap()
                .alias,
            left.alias
        );
        assert!(broker.sessions.lock().unwrap().is_empty());
    }
}

#[cfg(test)]
#[path = "routing/tests.rs"]
mod regression;
