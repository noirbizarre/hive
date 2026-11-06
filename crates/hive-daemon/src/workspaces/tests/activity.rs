use super::*;
use crate::{workspaces::activity::Snapshot, worktrunk::Status};
use std::os::unix::fs::PermissionsExt;

fn install_marker_cli(fixture: &Fixture) {
    let path = fixture._directory.path().join("fake-wt");
    // This fake persists the same shared Git config key as wt, including ownership timestamps.
    std::fs::write(&path, r#"#!/bin/sh
printf '%s\n' "$*" >> "$HOME/wt-calls"
case "$*" in
  *--help) printf '%s' 'get set clear --format --branch'; exit 0;;
esac
for arg in "$@"; do
  case "$arg" in --branch=*) branch="${arg#--branch=}";; esac
done
key="worktrunk.state.$branch.marker"
case "$*" in
  'config state marker --format=json get '*) git config --get "$key" || printf null;;
  'config state marker set '*) git config "$key" "{\"branch\":\"$branch\",\"marker\":\"$5\",\"set_at\":1}";;
  'config state marker clear '*)
    if test -f "$HOME/fail-clear"; then printf secret-sentinel >&2; exit 1; fi
    git config --unset-all "$key";;
  *) printf 'secret-sentinel' >&2; exit 1;;
esac
"#).unwrap();
    std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o755)).unwrap();
}

fn snapshot(fixture: &Fixture, id: &str, status: Status, order: u64) -> Snapshot {
    Snapshot {
        session: id.into(),
        cwd: fixture.seed.clone(),
        workspace: None,
        status,
        order,
        restored_idle: false,
    }
}

fn marker(fixture: &Fixture) -> serde_json::Value {
    let value = git(
        &fixture.seed,
        &["config", "--get", "worktrunk.state.main.marker"],
        "test",
    );
    value.map_or(serde_json::Value::Null, |value| {
        serde_json::from_str(&value).unwrap()
    })
}

async fn publish(fixture: &Fixture, values: Vec<Snapshot>) {
    fixture
        .service
        .publish_activity(
            values,
            WorktrunkConfig {
                enabled: Some(true),
            },
        )
        .await
        .unwrap();
}

#[tokio::test]
async fn worktrunk_aggregates_aliases_and_keeps_other_sessions_active_until_the_last_close() {
    let fixture = Fixture::new();
    install_marker_cli(&fixture);
    let subdirectory = fixture.seed.join("subdirectory");
    std::fs::create_dir(&subdirectory).unwrap();
    let mut waiting = snapshot(&fixture, "waiting", Status::Waiting, 0);
    waiting.cwd = subdirectory;
    publish(
        &fixture,
        vec![snapshot(&fixture, "running", Status::Running, 0), waiting],
    )
    .await;
    assert!(
        marker(&fixture)["marker"]
            .as_str()
            .unwrap()
            .starts_with("💬 Hive")
    );
    publish(
        &fixture,
        vec![snapshot(&fixture, "running", Status::Running, 0)],
    )
    .await;
    assert!(
        marker(&fixture)["marker"]
            .as_str()
            .unwrap()
            .starts_with("🤖 Hive")
    );
    publish(
        &fixture,
        vec![snapshot(&fixture, "running", Status::Completed, 1)],
    )
    .await;
    assert!(
        marker(&fixture)["marker"]
            .as_str()
            .unwrap()
            .starts_with("✅ Hive")
    );
    publish(&fixture, vec![]).await;
    assert!(marker(&fixture).is_null());
}

#[tokio::test]
async fn worktrunk_restart_clears_stale_activity_without_releasing_workspace_holds() {
    let fixture = Fixture::new();
    install_marker_cli(&fixture);
    let entry = fixture.create(fixture.request()).await.unwrap();
    fixture
        .service
        .attach(entry.workspace.id().clone(), "session".into())
        .await
        .unwrap();
    publish(
        &fixture,
        vec![snapshot(&fixture, "session", Status::Running, 0)],
    )
    .await;
    let restarted = Service::new(fixture.service.store.clone());
    restarted
        .publish_activity(
            vec![],
            WorktrunkConfig {
                enabled: Some(true),
            },
        )
        .await
        .unwrap();
    assert!(marker(&fixture).is_null());
    assert!(
        fixture
            .service
            .remove(entry.workspace.id().clone(), OperationId::new(), false)
            .await
            .is_err()
    );
}

#[tokio::test]
async fn worktrunk_preserves_foreign_markers_even_when_their_emoji_matches_hive() {
    let fixture = Fixture::new();
    install_marker_cli(&fixture);
    publish(
        &fixture,
        vec![snapshot(&fixture, "session", Status::Running, 0)],
    )
    .await;
    let foreign =
        serde_json::json!({"branch":"main","marker":"🤖 Hive [another-owner]","set_at":99});
    git(
        &fixture.seed,
        &[
            "config",
            "worktrunk.state.main.marker",
            &foreign.to_string(),
        ],
        "test",
    )
    .unwrap();
    publish(
        &fixture,
        vec![snapshot(&fixture, "session", Status::Waiting, 0)],
    )
    .await;
    publish(&fixture, vec![]).await;
    assert_eq!(marker(&fixture), foreign);
}

#[tokio::test]
async fn worktrunk_recovers_a_lost_marker_set_response_from_its_unique_durable_intent() {
    let fixture = Fixture::new();
    install_marker_cli(&fixture);
    publish(
        &fixture,
        vec![snapshot(&fixture, "session", Status::Running, 0)],
    )
    .await;
    let actual = marker(&fixture);
    let mut ledger: serde_json::Value = fixture
        .service
        .store
        .resource("worktrunk.activity")
        .unwrap()
        .unwrap();
    let record = ledger["markers"]
        .as_object_mut()
        .unwrap()
        .values_mut()
        .next()
        .unwrap();
    record["pending"] = actual["marker"].clone();
    record["confirmed"] = serde_json::Value::Null;
    fixture
        .service
        .store
        .save_resource("worktrunk.activity", &ledger)
        .unwrap();
    publish(&fixture, vec![]).await;
    assert!(marker(&fixture).is_null());
}

#[tokio::test]
async fn disabled_worktrunk_defers_marker_cleanup_without_invoking_the_executable() {
    let fixture = Fixture::new();
    install_marker_cli(&fixture);
    publish(
        &fixture,
        vec![snapshot(&fixture, "session", Status::Running, 0)],
    )
    .await;
    let actual = marker(&fixture);
    std::fs::write(
        fixture._directory.path().join("fake-wt"),
        "#!/bin/sh\nexit 99\n",
    )
    .unwrap();
    fixture
        .service
        .publish_activity(
            vec![],
            WorktrunkConfig {
                enabled: Some(false),
            },
        )
        .await
        .unwrap();
    assert_eq!(marker(&fixture), actual);
    install_marker_cli(&fixture);
    publish(&fixture, vec![]).await;
    assert!(marker(&fixture).is_null());
}

#[tokio::test]
async fn a_real_worktrunk_list_observes_hive_markers_and_their_cleanup() {
    let fixture = Fixture::new();
    if activity::probe_activity() != WorktrunkAvailability::Usable {
        eprintln!("skipping WorkTrunk marker smoke test: compatible wt unavailable");
        return;
    }
    for status in [
        Status::Running,
        Status::Waiting,
        Status::Completed,
        Status::Cancelled,
        Status::Failed,
    ] {
        publish(&fixture, vec![snapshot(&fixture, "session", status, 1)]).await;
        let list = execute("wt", Some(&fixture.seed), &["list", "--format=json"], true).unwrap();
        assert!(list.contains(status.marker().unwrap()));
    }
    publish(&fixture, vec![]).await;
    let list = execute("wt", Some(&fixture.seed), &["list", "--format=json"], true).unwrap();
    assert!(!list.contains("Hive ["));
}

#[tokio::test]
async fn the_broker_publisher_observes_interaction_only_waits_and_drains_on_shutdown() {
    let fixture = Fixture::new();
    install_marker_cli(&fixture);
    let mut config = crate::config::Config {
        worktrunk: Some(WorktrunkConfig {
            enabled: Some(true),
        }),
        ..Default::default()
    };
    config.agents.insert(
        "local".into(),
        toml::from_str("command = 'unused'").unwrap(),
    );
    let mut broker = crate::Broker::new(config).unwrap();
    broker.store = Some(fixture.service.store.clone());
    let mut state = crate::control_tests::detached();
    state.worktrunk_local = true;
    state.observe_state(&agent_client_protocol::schema::v2::StateUpdate::Running(
        agent_client_protocol::schema::v2::RunningStateUpdate::new(),
    ));
    let control = Arc::new(std::sync::Mutex::new(state));
    let task = tokio::spawn(std::future::pending::<()>());
    let (commands, _received) = tokio::sync::mpsc::unbounded_channel();
    broker.sessions.lock().unwrap().insert(
        "session".into(),
        crate::Entry {
            cwd: agent_client_protocol::schema::v2::AbsolutePath::new(fixture.seed.clone()),
            profile: "local".into(),
            abort: task.abort_handle(),
            commands,
            control: control.clone(),
        },
    );
    crate::worktrunk::Publisher::start(&broker);
    async fn wait_for(fixture: &Fixture, label: &str) {
        tokio::time::timeout(Duration::from_secs(5), async {
            loop {
                if marker(fixture)["marker"]
                    .as_str()
                    .is_some_and(|marker| marker.starts_with(label))
                {
                    break;
                }
                tokio::time::sleep(Duration::from_millis(20)).await;
            }
        })
        .await
        .unwrap();
    }
    wait_for(&fixture, "🤖 Hive").await;
    tokio::time::sleep(Duration::from_millis(150)).await;
    let calls = fixture._directory.path().join("wt-calls");
    let before = std::fs::read_to_string(&calls).unwrap();
    broker.catalog.send_modify(|revision| *revision += 1);
    tokio::time::sleep(Duration::from_millis(250)).await;
    assert_eq!(std::fs::read_to_string(&calls).unwrap(), before);
    {
        // Reloaded definitions cannot change the locality of an already-running worker.
        let mut registry = broker.registry.lock().unwrap();
        let mut current = (*registry.current).clone();
        current.agents.clear();
        registry.current = Arc::new(crate::reload::PreparedConfig::new(current).unwrap());
    }
    control.lock().unwrap().pending_interactions = 1;
    broker.catalog.send_modify(|revision| *revision += 1);
    wait_for(&fixture, "💬 Hive").await;
    broker.shutdown().await;
    assert!(marker(&fixture).is_null());
}

#[tokio::test]
async fn foreign_node_activity_is_rejected_before_wt_or_path_inspection() {
    let fixture = Fixture::new();
    install_marker_cli(&fixture);
    let entry = fixture.create(fixture.request()).await.unwrap();
    fixture
        .service
        .run(|runtime| {
            runtime.state.node = NodeId::new();
            runtime.save()
        })
        .await
        .unwrap();
    let mut value = snapshot(&fixture, "foreign", Status::Running, 0);
    value.workspace = Some(entry.workspace.id().to_string());
    value.cwd = PathBuf::from("/nonexistent/foreign-node");
    assert!(
        fixture
            .service
            .publish_activity(
                vec![value],
                WorktrunkConfig {
                    enabled: Some(true)
                }
            )
            .await
            .is_err()
    );
    assert!(!fixture._directory.path().join("wt-calls").exists());
}

#[tokio::test]
async fn colocated_jj_directories_never_probe_worktrunk_for_adopted_sessions() {
    let fixture = Fixture::new();
    install_marker_cli(&fixture);
    std::fs::create_dir(fixture.seed.join(".jj")).unwrap();
    let subdirectory = fixture.seed.join("subdirectory");
    std::fs::create_dir(&subdirectory).unwrap();
    let mut value = snapshot(&fixture, "adopted-jj", Status::Running, 0);
    value.cwd = subdirectory;
    publish(&fixture, vec![value]).await;
    assert!(!fixture._directory.path().join("wt-calls").exists());
}

#[tokio::test]
async fn missing_marker_capabilities_fail_required_mode_and_auto_mode_leaves_markers_alone() {
    let fixture = Fixture::new();
    let executable = fixture._directory.path().join("fake-wt");
    std::fs::write(
        &executable,
        "#!/bin/sh\nprintf 'secret-sentinel' >&2\nexit 1\n",
    )
    .unwrap();
    std::fs::set_permissions(&executable, std::fs::Permissions::from_mode(0o755)).unwrap();
    let error = fixture
        .service
        .publish_activity(
            vec![snapshot(&fixture, "session", Status::Running, 0)],
            WorktrunkConfig {
                enabled: Some(true),
            },
        )
        .await
        .unwrap_err();
    assert_eq!(error.code(), "hive::workspace::worktrunk_required");
    assert!(!error.to_string().contains("secret-sentinel"));
    fixture
        .service
        .publish_activity(
            vec![snapshot(&fixture, "session", Status::Running, 0)],
            WorktrunkConfig::default(),
        )
        .await
        .unwrap();
    assert!(marker(&fixture).is_null());
}

#[tokio::test]
async fn recovered_idle_retains_a_known_terminal_but_a_new_unknown_turn_cannot_reuse_it() {
    let fixture = Fixture::new();
    install_marker_cli(&fixture);
    let record: crate::persistence::Record = serde_json::from_value(serde_json::json!({
        "id":"session", "downstream_id":"native", "cwd":fixture.seed,
        "profile":"local", "version":"v2", "created_at":1, "last_activity_at":null,
        "title":null, "next_seq":1, "reserved_seq":1, "generation":1,
        "outcome":"active", "reason":null, "uncertain":false
    }))
    .unwrap();
    fixture.service.store.save(&record).unwrap();
    publish(
        &fixture,
        vec![snapshot(&fixture, "session", Status::Failed, 5)],
    )
    .await;
    publish(&fixture, vec![]).await;
    let mut restored = snapshot(&fixture, "session", Status::Unknown, 0);
    restored.restored_idle = true;
    publish(&fixture, vec![restored]).await;
    assert!(
        marker(&fixture)["marker"]
            .as_str()
            .unwrap()
            .starts_with("❌ Hive")
    );
    publish(
        &fixture,
        vec![snapshot(&fixture, "session", Status::Running, 0)],
    )
    .await;
    publish(
        &fixture,
        vec![snapshot(&fixture, "session", Status::Unknown, 0)],
    )
    .await;
    assert!(marker(&fixture).is_null());
}

#[tokio::test]
async fn worktrunk_cleanup_refuses_dirty_work_without_removing_the_checkout() {
    let fixture = Fixture::new();
    if probe_worktrunk() != WorktrunkAvailability::Usable {
        eprintln!("skipping WorkTrunk dirty cleanup smoke test: compatible wt unavailable");
        return;
    }
    let entry = fixture
        .service
        .create(
            fixture.request(),
            fixture.definition.clone(),
            WorktrunkConfig {
                enabled: Some(true),
            },
            None,
        )
        .await
        .unwrap();
    let root = PathBuf::from(entry.workspace.location().unwrap().root.as_str());
    std::fs::write(root.join("uncommitted"), "keep this work").unwrap();
    assert!(
        fixture
            .service
            .remove(entry.workspace.id().clone(), OperationId::new(), false)
            .await
            .is_err()
    );
    assert_eq!(
        std::fs::read_to_string(root.join("uncommitted")).unwrap(),
        "keep this work"
    );
    fixture
        .service
        .remove(entry.workspace.id().clone(), OperationId::new(), true)
        .await
        .unwrap();
    assert!(!root.exists());
}

#[tokio::test]
async fn failed_marker_cleanup_retains_ownership_for_a_sanitized_retry_after_restart() {
    let fixture = Fixture::new();
    install_marker_cli(&fixture);
    publish(
        &fixture,
        vec![snapshot(&fixture, "session", Status::Running, 0)],
    )
    .await;
    let actual = marker(&fixture);
    let failure = fixture._directory.path().join("fail-clear");
    std::fs::write(&failure, "").unwrap();
    let error = fixture
        .service
        .publish_activity(
            vec![],
            WorktrunkConfig {
                enabled: Some(true),
            },
        )
        .await
        .unwrap_err();
    assert!(!error.to_string().contains("secret-sentinel"));
    assert_eq!(marker(&fixture), actual);
    std::fs::remove_file(failure).unwrap();
    Service::new(fixture.service.store.clone())
        .publish_activity(
            vec![],
            WorktrunkConfig {
                enabled: Some(true),
            },
        )
        .await
        .unwrap();
    assert!(marker(&fixture).is_null());
}
