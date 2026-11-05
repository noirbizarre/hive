//! A coalesced projection of execution evidence, independent of workspace cleanup holds.

use crate::{Broker, Control};
use serde::{Deserialize, Serialize};
use std::sync::{
    Mutex,
    atomic::{AtomicU64, Ordering},
};

static ORDER: AtomicU64 = AtomicU64::new(1);

pub(crate) fn local_profile(profile: &crate::config::Profile) -> bool {
    profile.command.is_some()
        || profile.endpoint.as_ref().is_some_and(|endpoint| {
            reqwest::Url::parse(endpoint)
                .ok()
                .and_then(|url| url.host_str().map(str::to_owned))
                .is_some_and(|host| {
                    host == "localhost"
                        || host
                            .trim_matches(['[', ']'])
                            .parse::<std::net::IpAddr>()
                            .is_ok_and(|ip| ip.is_loopback())
                })
        })
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub(crate) enum Status {
    Unknown,
    Running,
    Waiting,
    Completed,
    Cancelled,
    Failed,
}

impl Status {
    pub(crate) fn marker(self) -> Option<&'static str> {
        match self {
            Self::Unknown => None,
            Self::Running => Some("🤖 Hive"),
            Self::Waiting => Some("💬 Hive"),
            Self::Completed => Some("✅ Hive"),
            Self::Cancelled => Some("⏹ Hive"),
            Self::Failed => Some("❌ Hive"),
        }
    }
}

pub(crate) fn terminal(
    reason: Option<&agent_client_protocol::schema::v2::StopReason>,
) -> Option<(u64, Status)> {
    use agent_client_protocol::schema::v2::StopReason;
    let status = match reason {
        Some(StopReason::EndTurn) => Status::Completed,
        Some(StopReason::Cancelled) => Status::Cancelled,
        Some(StopReason::Refusal | StopReason::Error(_)) => Status::Failed,
        Some(StopReason::Other(value))
            if matches!(value.stop_reason.as_str(), "_v1_error" | "_opencode_error") =>
        {
            Status::Failed
        }
        _ => return None,
    };
    Some((ORDER.fetch_add(1, Ordering::Relaxed), status))
}

pub(crate) fn project(state: &Control) -> (Status, u64) {
    if state.hydrating || state.activity.session == "ended" {
        return (Status::Unknown, 0);
    }
    if state.pending_interactions > 0 || state.foreground() == "requires_action" {
        return (Status::Waiting, 0);
    }
    if state.foreground() == "running" {
        return (Status::Running, 0);
    }
    if state.recovery_required || state.foreground() != "idle" {
        return (Status::Unknown, 0);
    }
    state
        .worktrunk_terminal
        .map_or((Status::Unknown, 0), |(order, status)| (status, order))
}

pub(crate) fn aggregate(values: impl IntoIterator<Item = (Status, u64)>) -> Status {
    let values: Vec<_> = values.into_iter().collect();
    if values.iter().any(|(s, _)| *s == Status::Waiting) {
        return Status::Waiting;
    }
    if values.iter().any(|(s, _)| *s == Status::Running) {
        return Status::Running;
    }
    // Unknown work cannot be advertised as an earlier successful turn from another session.
    if values.iter().any(|(s, _)| *s == Status::Unknown) {
        return Status::Unknown;
    }
    values
        .into_iter()
        .max_by_key(|(_, order)| *order)
        .map_or(Status::Unknown, |(s, _)| s)
}

#[derive(Default)]
pub(crate) struct Publisher {
    task: Mutex<Option<tokio::task::JoinHandle<()>>>,
    stop: tokio::sync::Notify,
}

impl Publisher {
    pub(crate) fn start(broker: &Broker) {
        let Some(store) = broker.store.clone() else {
            return;
        };
        if broker
            .registry
            .lock()
            .expect("configuration lock poisoned")
            .stopping
        {
            return;
        }
        let mut task = broker
            .worktrunk
            .task
            .lock()
            .expect("publisher lock poisoned");
        if task.as_ref().is_some_and(|task| !task.is_finished()) {
            return;
        }
        let sessions = std::sync::Arc::downgrade(&broker.sessions);
        let registry = std::sync::Arc::downgrade(&broker.registry);
        let publisher = std::sync::Arc::downgrade(&broker.worktrunk);
        let mut changed = broker.catalog.subscribe();
        *task = Some(tokio::spawn(async move {
            let service = crate::workspaces::Service::new(store);
            let mut previous = None;
            let mut verified_at = None;
            let mut retry_at = tokio::time::Instant::now();
            loop {
                // Streaming events share the catalog signal; coalesce bursts before Git inspection.
                tokio::time::sleep(std::time::Duration::from_millis(100)).await;
                changed.borrow_and_update();
                let (Some(sessions), Some(registry), Some(publisher)) =
                    (sessions.upgrade(), registry.upgrade(), publisher.upgrade())
                else {
                    break;
                };
                let current = registry
                    .lock()
                    .expect("configuration lock poisoned")
                    .current
                    .clone();
                let config = current.worktrunk.unwrap_or_default();
                // Snapshot under short-lived session locks; no resource or subprocess work occurs here.
                let mut snapshots: Vec<_> = sessions
                    .lock()
                    .expect("sessions lock poisoned")
                    .iter()
                    .filter_map(|(id, entry)| {
                        // Remote provider cwd spelling is not proof that its filesystem belongs to this node.
                        let state = entry.control.lock().ok()?;
                        if !state.worktrunk_local
                            || state.activity.session == "ended"
                            || state.hydrating
                        {
                            return None;
                        }
                        let (status, order) = project(&state);
                        Some(crate::workspaces::activity::Snapshot {
                            session: id.clone(),
                            cwd: entry.cwd.0.clone(),
                            workspace: state
                                .workspace_context
                                .as_ref()
                                .map(|c| c.workspace.clone()),
                            status,
                            order,
                            restored_idle: state.worktrunk_restore
                                && !state.hydrating
                                && !state.recovery_required
                                && state.foreground() == "idle",
                        })
                    })
                    .collect();
                snapshots.sort_by(|a, b| a.session.cmp(&b.session));
                let now = tokio::time::Instant::now();
                // Token streaming changes the catalog, but should not spawn Git/wt for unchanged activity.
                let due = previous
                    .as_ref()
                    .is_none_or(|(values, policy)| values != &snapshots || policy != &config)
                    || verified_at.is_none_or(|at: tokio::time::Instant| {
                        now.duration_since(at) >= std::time::Duration::from_secs(5)
                    });
                if due && now >= retry_at {
                    match service.publish_activity(snapshots.clone(), config).await {
                        Ok(()) => {
                            previous = Some((snapshots, config));
                            verified_at = Some(now);
                        }
                        Err(error) => {
                            retry_at =
                                tokio::time::Instant::now() + std::time::Duration::from_secs(5);
                            tracing::warn!(diagnostic_code = "hive::workspace::activity", %error, "WorkTrunk activity could not be reconciled; verify wt availability and repository access");
                        }
                    }
                }
                drop(sessions);
                drop(registry);
                tokio::select! {
                    _ = publisher.stop.notified() => {
                        // Shutdown withdraws execution claims, never workspace cleanup protection.
                        if let Err(error) = service.publish_activity(vec![], config).await {
                            tracing::warn!(diagnostic_code = "hive::workspace::activity", %error, "WorkTrunk marker cleanup remains pending for the next daemon start");
                        }
                        break;
                    }
                    result = changed.changed() => { if result.is_err() { break; } }
                    // Retries and branch/path verification also progress without new ACP events.
                    _ = tokio::time::sleep(std::time::Duration::from_secs(5)) => {}
                }
            }
        }));
    }

    pub(crate) async fn shutdown(&self) {
        let task = self.task.lock().expect("publisher lock poisoned").take();
        if let Some(mut task) = task {
            self.stop.notify_one();
            if tokio::time::timeout(std::time::Duration::from_secs(12), &mut task)
                .await
                .is_err()
            {
                // Blocking command effects retain durable intent if the shutdown budget expires.
                task.abort();
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use agent_client_protocol::schema::v2;

    #[test]
    fn remote_provider_paths_cannot_nominate_a_gateway_local_marker_target() {
        for endpoint in [
            "https://agent.example.test",
            "https://192.0.2.1",
            "https://[2001:db8::1]",
        ] {
            let profile = serde_json::from_value(serde_json::json!({"endpoint":endpoint})).unwrap();
            assert!(!local_profile(&profile));
        }
        let local = serde_json::from_value(serde_json::json!({"command":"agent"})).unwrap();
        assert!(local_profile(&local));
    }

    #[test]
    fn waiting_and_running_outrank_terminal_results_and_unknown_work_hides_success() {
        assert_eq!(
            aggregate([(Status::Completed, 7), (Status::Running, 0)]),
            Status::Running
        );
        assert_eq!(
            aggregate([(Status::Waiting, 0), (Status::Running, 0)]),
            Status::Waiting
        );
        assert_eq!(
            aggregate([(Status::Completed, 7), (Status::Unknown, 0)]),
            Status::Unknown
        );
        assert_eq!(
            aggregate([(Status::Failed, 8), (Status::Completed, 7)]),
            Status::Failed
        );
        assert_eq!(aggregate([]), Status::Unknown);
    }

    #[test]
    fn adapter_failure_wrappers_and_reasonless_idle_never_claim_success() {
        for reason in ["_v1_error", "_opencode_error"] {
            let reason: v2::StopReason =
                serde_json::from_value(serde_json::json!({"stopReason":reason})).unwrap();
            assert_eq!(terminal(Some(&reason)).unwrap().1, Status::Failed);
        }
        assert_eq!(terminal(None), None);
        assert_eq!(
            terminal(Some(&v2::StopReason::EndTurn)).unwrap().1,
            Status::Completed
        );
        assert_eq!(
            terminal(Some(&v2::StopReason::Cancelled)).unwrap().1,
            Status::Cancelled
        );
    }

    #[tokio::test]
    async fn session_projection_keeps_permission_waits_and_does_not_promote_history_or_receipts() {
        let mut state = crate::control_tests::detached();
        let id = v2::SessionId::new("session");
        let idle = || {
            v2::SessionUpdate::StateUpdate(v2::StateUpdate::Idle(
                v2::IdleStateUpdate::new().stop_reason(v2::StopReason::EndTurn),
            ))
        };
        state.hydrating = true;
        crate::forward_locked(&mut state, &id, Some(idle())).unwrap();
        state.finish_history();
        assert_eq!(project(&state).0, Status::Unknown);
        state.foreground_reservation = Some("prompt".into());
        // A delayed idle rejected by normal correlation cannot clear the new turn's marker.
        crate::forward_locked(&mut state, &id, Some(idle())).unwrap();
        assert_eq!(project(&state).0, Status::Running);
        state.pending_interactions = 2;
        assert_eq!(project(&state).0, Status::Waiting);
        state.pending_interactions -= 1;
        assert_eq!(project(&state).0, Status::Waiting);
        state.pending_interactions = 0;
        assert_eq!(project(&state).0, Status::Running);
        state.observe_state(&v2::StateUpdate::Running(v2::RunningStateUpdate::new()));
        crate::forward_locked(&mut state, &id, Some(idle())).unwrap();
        assert_eq!(project(&state).0, Status::Completed);
        state.observe_state(&v2::StateUpdate::Idle(v2::IdleStateUpdate::new()));
        assert_eq!(project(&state).0, Status::Completed);
        state.activity.session = "ended".into();
        assert_eq!(project(&state).0, Status::Unknown);
    }

    #[tokio::test]
    async fn recovered_idle_synchronization_preserves_terminal_restoration_until_new_work() {
        let mut state = crate::control_tests::detached();
        state.hydrating = true;
        state.worktrunk_restore = true;
        state.observe_state(&v2::StateUpdate::Idle(v2::IdleStateUpdate::new()));
        state.finish_history();
        let id = v2::SessionId::new("recovered");
        for _ in 0..2 {
            crate::forward_locked(
                &mut state,
                &id,
                Some(v2::SessionUpdate::StateUpdate(v2::StateUpdate::Idle(
                    v2::IdleStateUpdate::new(),
                ))),
            )
            .unwrap();
            assert!(state.worktrunk_restore);
        }
        state.observe_state(&v2::StateUpdate::Running(v2::RunningStateUpdate::new()));
        assert!(!state.worktrunk_restore);
        state.observe_state(&v2::StateUpdate::Idle(v2::IdleStateUpdate::new()));
        assert!(!state.worktrunk_restore);
        assert_eq!(project(&state).0, Status::Unknown);
    }
}
