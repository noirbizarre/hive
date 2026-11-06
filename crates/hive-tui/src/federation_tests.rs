//! Exercise owner selection through the same discovery and key controller used by the terminal.
use super::*;
use serde_json::json;

const LOCAL: &str = "node-11111111-1111-4111-8111-111111111111";
const REMOTE: &str = "node-22222222-2222-4222-8222-222222222222";

#[tokio::test]
async fn owner_directory_edits_and_owner_changes_preserve_the_conversation_and_reject_stale_inventory()
 {
    interaction_tests::connected(async |cx| {
        let mut app = App { page: Page::Profiles, input: "unsent conversation".into(), cwd: "/local".into(), explicit_cwd: true, owners: vec![json!({"owner":REMOTE,"alias":"workstation"})], ..App::default() };
        let (acks, _) = mpsc::unbounded_channel();
        let (created, _) = mpsc::unbounded_channel();
        let mut interaction = None;
        let mut key = async |app: &mut App, code| {
            assert!(!handle_key(&cx, "", app, event::KeyEvent::new(code, KeyModifiers::NONE), &mut interaction, acks.clone(), created.clone()).await);
        };
        key(&mut app, KeyCode::Char('o')).await;
        assert_eq!(app.launch_owner.as_ref().unwrap().as_str(), REMOTE);
        assert!(!app.explicit_cwd);
        key(&mut app, KeyCode::Char('d')).await;
        paste(&mut app, &None, "C:\\owner-only");
        key(&mut app, KeyCode::Enter).await;
        assert_eq!(app.cwd, std::path::Path::new("C:\\owner-only"));
        assert!(app.explicit_cwd);
        assert!(matches!(&app.workspace_launch, Some(hive_protocol::workspace::Launch::Existing { directory }) if directory == "C:\\owner-only"));
        assert_eq!(app.input, "unsent conversation");
        app.launch_cwd_input = Some("relative".into());
        key(&mut app, KeyCode::Enter).await;
        assert_eq!(app.launch_cwd_input.as_deref(), Some("relative"));
        assert_eq!(app.cwd, std::path::Path::new("C:\\owner-only"));
        key(&mut app, KeyCode::Backspace).await;
        key(&mut app, KeyCode::Char('x')).await;
        key(&mut app, KeyCode::Esc).await;
        assert!(app.launch_cwd_input.is_none());
        key(&mut app, KeyCode::Char('o')).await;
        assert!(app.launch_owner.is_none());
        assert!(app.workspace_launch.is_none());
        assert!(!app.explicit_cwd);
        update_profiles(&mut app, &json!({"agents":[{"id":"local-agent"}]}));
        let owners = app.owners.clone();
        apply_catalog(&mut app, Catalog { snapshot: SessionSnapshot { sessions: Vec::new() }, providers: Ok(json!({"agents":[{"id":"stale-remote-agent"}]})), owners, references: BTreeMap::new(), availability: BTreeMap::new(), owner: Some(hive_core::identity::NodeId::try_from(REMOTE.to_owned()).unwrap()), partial: false });
        assert_eq!(app.profiles, ["local-agent"]);
        assert_eq!(app.input, "unsent conversation");
        assert!(app.session_operation.is_none());
        Ok(())
    }).await;
}

#[tokio::test]
async fn paginated_owner_inventories_are_complete_and_repeated_cursors_cannot_loop_forever() {
    let agent = Agent.v2()
        .on_receive_request(async |_: hive_protocol::remote::Initialize, responder: agent_client_protocol::Responder<serde_json::Value>, _: V2ConnectionTo<Client>| {
            responder.respond(json!({"protocolVersion":2,"info":{"name":"fixture","version":"1"},"capabilities":{"session":{},"_meta":{"hive":{"version":1,"node":LOCAL,"features":{"agents":{},"selection":{},"control":{},"location":{},"peers":{},"profiles":{}}}}}}))
        }, agent_client_protocol::on_receive_request!())
        .on_receive_request(async |request: hive_protocol::remote::ListAgents, responder: agent_client_protocol::Responder<hive_protocol::remote::AgentsPage>, _: V2ConnectionTo<Client>| {
            let repeated = request.0["peer"] == "node-33333333-3333-4333-8333-333333333333";
            let id = if request.0["cursor"].is_string() { "second" } else { "first" };
            responder.respond(serde_json::from_value(json!({"agents":[{"id":id,"name":id,"kind":"acp","transport":"stdio","versions":[],"isDefault":false,"capabilities":{},"launch":{"cwd":"required","workspace":"optional"}}],"defaultAgent":"first","nextCursor":if repeated || id == "first" { Some("agents-next") } else { None }})).unwrap())
        }, agent_client_protocol::on_receive_request!())
        .on_receive_request(async |request: hive_protocol::remote::ListProfiles, responder: agent_client_protocol::Responder<hive_protocol::remote::ProfilesPage>, _: V2ConnectionTo<Client>| {
            let id = if request.0["cursor"].is_string() { "second" } else { "first" };
            responder.respond(serde_json::from_value(json!({"profiles":[{"id":id,"name":id,"agents":[id],"projects":[],"defaultAgent":id,"defaultProject":null,"isDefault":false,"workspaces":{}}],"defaultProfile":"first","nextCursor":if id == "first" { Some("profiles-next") } else { None }})).unwrap())
        }, agent_client_protocol::on_receive_request!())
        .on_receive_request(async |_: hive_protocol::remote::ListPeers, responder: agent_client_protocol::Responder<serde_json::Value>, _: V2ConnectionTo<Client>| { responder.respond(json!({"peers":[{"owner":REMOTE,"alias":"workstation"}]})) }, agent_client_protocol::on_receive_request!())
        .on_receive_request(async |request: v2::ListSessionsRequest, responder: agent_client_protocol::Responder<v2::ListSessionsResponse>, _: V2ConnectionTo<Client>| {
            let second = request.cursor.is_some();
            let owner = if second { REMOTE } else { LOCAL };
            let id = if second { "remote-route" } else { "local-route" };
            responder.respond(serde_json::from_value(json!({"sessions":[{"sessionId":id,"cwd":"/same-directory","_meta":{"hive":{"title":"Retained title","session":{"owner":owner,"id":"native"}}}}],"nextCursor":if second { None } else { Some("sessions-next") },"_meta":{"hive":{"partial":!second,"unavailableOwners":[REMOTE]}}})).unwrap())
        }, agent_client_protocol::on_receive_request!());
    tokio::time::timeout(
        Duration::from_secs(5),
        Client.v2().connect_with(agent, async |cx| {
            let peer = hive_protocol::remote::Peer::initialize(
                cx,
                v2::InitializeRequest::new(
                    ProtocolVersion::V2,
                    v2::Implementation::new("test", "1"),
                ),
            )
            .await?;
            for owner in [
                None,
                Some(hive_core::identity::NodeId::try_from(REMOTE.to_owned()).unwrap()),
            ] {
                let catalog = load_catalog(&peer, owner.clone()).await.unwrap();
                assert_eq!(catalog.snapshot.sessions.len(), 2);
                let providers = catalog.providers.as_ref().unwrap();
                assert_eq!(providers["agents"].as_array().unwrap().len(), 2);
                assert_eq!(providers["profiles"].as_array().unwrap().len(), 2);
                assert_eq!(providers["defaultAgent"], "first");
                assert!(catalog.partial);
                let mut app = App {
                    peer: Some(peer.clone()),
                    launch_owner: owner,
                    cwd: "/same-directory".into(),
                    ..App::default()
                };
                apply_catalog(&mut app, catalog);
                assert_eq!(
                    app.sessions.len(),
                    1,
                    "identical paths on different owners must not merge directory scope"
                );
                assert!(app.status.contains("incomplete"));
            }
            let repeated = hive_core::identity::NodeId::try_from(
                "node-33333333-3333-4333-8333-333333333333".to_owned(),
            )
            .unwrap();
            assert!(
                load_inventory(&peer, Some(&repeated))
                    .await
                    .unwrap_err()
                    .contains("repeated a cursor")
            );
            Ok(())
        }),
    )
    .await
    .unwrap()
    .unwrap();
}

#[tokio::test]
async fn typed_launch_failures_distinguish_safe_rejection_from_uncertain_owner_execution() {
    let agent = Agent
        .v2()
        .on_receive_request(
            async |request: v2::InitializeRequest,
                   responder: agent_client_protocol::Responder<v2::InitializeResponse>,
                   _: V2ConnectionTo<Client>| {
                responder.respond(v2::InitializeResponse::new(
                    request.protocol_version,
                    v2::Implementation::new("fixture", "1"),
                ))
            },
            agent_client_protocol::on_receive_request!(),
        )
        .on_receive_request(
            async |request: v2::NewSessionRequest,
                   responder: agent_client_protocol::Responder<v2::NewSessionResponse>,
                   _: V2ConnectionTo<Client>| {
                let path = request.cwd.0.to_string_lossy();
                let error = match path.as_ref() {
                    "/uncertain" => hive_protocol::remote::RemoteError::LaunchUncertain,
                    "/provisioning" => hive_protocol::remote::RemoteError::ProvisioningUncertain,
                    _ => hive_protocol::remote::RemoteError::LaunchFailed,
                };
                responder.respond_with_error(error.wire())
            },
            agent_client_protocol::on_receive_request!(),
        );
    tokio::time::timeout(
        Duration::from_secs(5),
        Client.v2().connect_with(agent, async |cx| {
            cx.send_request(v2::InitializeRequest::new(
                ProtocolVersion::V2,
                v2::Implementation::new("test", "1"),
            ))
            .block_task()
            .await?;
            for (path, uncertain) in [
                ("/uncertain", true),
                ("/provisioning", true),
                ("/failed", false),
            ] {
                let mut app = App {
                    cwd: path.into(),
                    launch_owner: Some(
                        hive_core::identity::NodeId::try_from(REMOTE.to_owned()).unwrap(),
                    ),
                    explicit_cwd: true,
                    ..App::default()
                };
                start_creation(&cx, &mut app, "coder".into());
                let result = operation_completion(&mut app.session_operation)
                    .await
                    .unwrap();
                app.session_operation = None;
                let created = apply_session_result(&mut app, result, "Creating session").unwrap();
                apply_created(&mut app, created);
                assert_eq!(app.creation_uncertain, uncertain);
                assert!(app.status.contains("Could not start session"));
                assert!(
                    app.status
                        .contains(if uncertain { "Inspect" } else { "Check" })
                );
                if uncertain {
                    start_creation(&cx, &mut app, "coder".into());
                    assert!(
                        app.session_operation.is_none(),
                        "uncertainty must prevent a duplicate dispatch"
                    );
                }
            }
            Ok(())
        }),
    )
    .await
    .unwrap()
    .unwrap();
}

#[tokio::test]
async fn explicit_owner_attachment_cannot_capture_colliding_state_and_an_unavailable_owner_keeps_navigation_usable()
 {
    let agent = Agent.v2()
        .on_receive_request(async |_: hive_protocol::remote::Initialize, responder: agent_client_protocol::Responder<serde_json::Value>, _: V2ConnectionTo<Client>| {
            responder.respond(json!({"protocolVersion":2,"info":{"name":"fixture","version":"1"},"capabilities":{"session":{},"_meta":{"hive":{"version":1,"node":LOCAL,"features":{"agents":{},"selection":{},"control":{},"location":{},"peers":{}}}}}}))
        }, agent_client_protocol::on_receive_request!())
        .on_receive_request(async |request: hive_protocol::remote::ResolveSession, responder: agent_client_protocol::Responder<hive_protocol::remote::SessionLocation>, _: V2ConnectionTo<Client>| {
            assert_eq!(request.0["session"]["owner"], REMOTE);
            if request.0["session"]["id"] == "unavailable" { return responder.respond_with_error(hive_protocol::remote::RemoteError::OwnerUnavailable.wire()); }
            assert_eq!(request.0["session"]["id"], "same");
            responder.respond(serde_json::from_value(json!({"session":request.0["session"],"sessionId":"remote-route","cwd":"/owner-only"})).unwrap())
        }, agent_client_protocol::on_receive_request!())
        .on_receive_request(async |request: v2::ResumeSessionRequest, responder: agent_client_protocol::Responder<v2::ResumeSessionResponse>, _: V2ConnectionTo<Client>| {
            assert_eq!(request.session_id.to_string(), "remote-route");
            responder.respond(v2::ResumeSessionResponse::new().meta(v2::Meta::from_iter([("hive".into(), json!({"role":"pilot","token":"remote-token","generation":1,"session":{"owner":REMOTE,"id":"same"}}))])))
        }, agent_client_protocol::on_receive_request!());
    tokio::time::timeout(
        Duration::from_secs(5),
        Client.v2().connect_with(agent, async |cx| {
            let peer = hive_protocol::remote::Peer::initialize(
                cx.clone(),
                v2::InitializeRequest::new(
                    ProtocolVersion::V2,
                    v2::Implementation::new("test", "1"),
                ),
            )
            .await?;
            let remote = hive_core::identity::NodeId::try_from(REMOTE.to_owned()).unwrap();
            let mut app = App {
                creation_uncertain: true,
                connected: true,
                ..App::default()
            };
            app.references.insert(
                "same".into(),
                hive_protocol::remote::SessionReference {
                    owner: hive_core::identity::NodeId::try_from(LOCAL.to_owned()).unwrap(),
                    id: "same".into(),
                },
            );
            app.drafts.insert("same".into(), "local draft".into());
            app.conversations.entry("same".into()).or_default().seq = 12;
            restore_attachment(&peer, &mut app, Some("same"), Some(&remote), false).await;
            assert_eq!(app.active.as_deref(), Some("remote-route"));
            assert_eq!(app.references["same"].owner.as_str(), LOCAL);
            assert_eq!(app.drafts["same"], "local draft");
            assert_eq!(app.conversations["same"].seq, 12);
            assert!(
                app.creation_uncertain,
                "resuming another session cannot reconcile an interrupted launch"
            );
            app.references.get_mut("remote-route").unwrap().id = "unavailable".into();
            restore_attachment(&peer, &mut app, Some("same"), Some(&remote), true).await;
            assert!(app.connected);
            assert!(app.owner.is_none());
            assert_eq!(app.active.as_deref(), Some("remote-route"));
            assert!(app.status.contains("Could not attach"));
            let (acks, _) = mpsc::unbounded_channel();
            let (created, _) = mpsc::unbounded_channel();
            assert!(
                !handle_key(
                    &cx,
                    "",
                    &mut app,
                    event::KeyEvent::new(KeyCode::Tab, KeyModifiers::NONE),
                    &mut None,
                    acks.clone(),
                    created.clone()
                )
                .await
            );
            assert!(app.page == Page::Sessions);
            app.page = Page::Profiles;
            assert!(
                !handle_key(
                    &cx,
                    "",
                    &mut app,
                    event::KeyEvent::new(KeyCode::Char('u'), KeyModifiers::NONE),
                    &mut None,
                    acks.clone(),
                    created.clone()
                )
                .await
            );
            assert!(!app.creation_uncertain);
            assert!(
                handle_key(
                    &cx,
                    "",
                    &mut app,
                    event::KeyEvent::new(KeyCode::Char('q'), KeyModifiers::CONTROL),
                    &mut None,
                    acks,
                    created
                )
                .await
            );
            Ok(())
        }),
    )
    .await
    .unwrap()
    .unwrap();
}

#[test]
fn explicit_initial_selectors_are_revalidated_after_inventory_becomes_available() {
    let mut app = App::default();
    assert!(matches!(
        apply_initial_selection(&mut app, Some("personal"), None, false),
        Err(InitialSelectionError::Profile(_))
    ));
    assert!(matches!(
        apply_initial_selection(&mut app, None, Some("coder"), false),
        Err(InitialSelectionError::Agent(_))
    ));
    update_profiles(
        &mut app,
        &json!({"agents":[{"id":"other"},{"id":"coder"}],"profiles":[{"id":"personal","name":"Personal","agents":["coder"],"projects":[],"defaultAgent":"coder","defaultProject":null,"isDefault":false,"workspaces":{}}]}),
    );
    apply_initial_selection(&mut app, Some("personal"), Some("coder"), false).unwrap();
    assert_eq!(app.execution_profile.as_deref(), Some("personal"));
    assert_eq!(app.profiles[app.profile_selected], "coder");
    assert!(app.page == Page::Profiles);
}

#[tokio::test]
async fn remote_discovery_and_creation_preserve_owner_identity_and_owner_only_directories() {
    let agent = Agent.v2()
        .on_receive_request(async |_: hive_protocol::remote::Initialize, responder: agent_client_protocol::Responder<serde_json::Value>, _: V2ConnectionTo<Client>| {
            responder.respond(json!({"protocolVersion":2,"info":{"name":"fixture","version":"1"},"capabilities":{"session":{},"_meta":{"hive":{"version":1,"node":LOCAL,"features":{"agents":{},"selection":{},"control":{},"location":{},"peers":{},"profiles":{}}}}}}))
        }, agent_client_protocol::on_receive_request!())
        .on_receive_request(async |request: hive_protocol::remote::ListAgents, responder: agent_client_protocol::Responder<hive_protocol::remote::AgentsPage>, _: V2ConnectionTo<Client>| {
            assert_eq!(request.0["peer"], REMOTE);
            responder.respond(serde_json::from_value(json!({"agents":[{"id":"coder","name":"Remote coder","kind":"acp","transport":"stdio","versions":[],"isDefault":true,"capabilities":{},"launch":{"cwd":"required","workspace":"optional"}}],"defaultAgent":"coder"})).unwrap())
        }, agent_client_protocol::on_receive_request!())
        .on_receive_request(async |_: hive_protocol::remote::ListProfiles, responder: agent_client_protocol::Responder<hive_protocol::remote::ProfilesPage>, _: V2ConnectionTo<Client>| {
            responder.respond(hive_protocol::remote::ProfilesPage { profiles: Vec::new(), default_profile: None, next_cursor: None })
        }, agent_client_protocol::on_receive_request!())
        .on_receive_request(async |_: hive_protocol::remote::ListPeers, responder: agent_client_protocol::Responder<serde_json::Value>, _: V2ConnectionTo<Client>| {
            responder.respond(json!({"peers":[{"owner":REMOTE,"alias":"workstation","name":"Workstation"}]}))
        }, agent_client_protocol::on_receive_request!())
        .on_receive_request(async |_: v2::ListSessionsRequest, responder: agent_client_protocol::Responder<v2::ListSessionsResponse>, _: V2ConnectionTo<Client>| {
            responder.respond(serde_json::from_value(json!({"sessions":[
                {"sessionId":"local-route","cwd":"/work","_meta":{"hive":{"agent":"coder","lifecycle":"active","activity":"idle","session":{"owner":LOCAL,"id":"same"}}}},
                {"sessionId":"remote-route","cwd":"C:\\owner-only","_meta":{"hive":{"sessionId":"same","cached":true,"availability":"owner_unavailable","session":{"owner":REMOTE,"id":"same"}}}}
            ]})).unwrap())
        }, agent_client_protocol::on_receive_request!())
        .on_receive_request(async |request: v2::NewSessionRequest, responder: agent_client_protocol::Responder<v2::NewSessionResponse>, _: V2ConnectionTo<Client>| {
            assert_eq!(request.cwd.0, std::path::Path::new("C:\\owner-only"));
            let hive = &request.meta.as_ref().unwrap()["hive"];
            assert_eq!(hive["peer"], REMOTE);
            assert_eq!(hive["agent"], "coder");
            responder.respond(v2::NewSessionResponse::new("created-route").meta(v2::Meta::from_iter([("hive".into(),json!({"role":"pilot","token":"new","generation":1,"session":{"owner":REMOTE,"id":"created-native"}}))])))
        }, agent_client_protocol::on_receive_request!());
    tokio::time::timeout(
        Duration::from_secs(5),
        Client.v2().connect_with(agent, async |cx| {
            let peer = hive_protocol::remote::Peer::initialize(
                cx.clone(),
                v2::InitializeRequest::new(
                    ProtocolVersion::V2,
                    v2::Implementation::new("test", "1"),
                ),
            )
            .await?;
            let owner = hive_core::identity::NodeId::try_from(REMOTE.to_owned()).unwrap();
            let mut app = App {
                launch_owner: Some(owner.clone()),
                all: true,
                ..App::default()
            };
            let catalog = load_catalog(&peer, Some(owner)).await.unwrap();
            apply_catalog(&mut app, catalog);
            assert_eq!(app.sessions.len(), 2);
            assert_eq!(app.sessions[1].session_id, "remote-route");
            assert_eq!(app.sessions[1].activity, "unknown");
            assert_ne!(
                app.references["remote-route"],
                app.references["local-route"]
            );
            assert_eq!(app.availability["remote-route"], "owner_unavailable");
            assert_eq!(app.profiles, ["coder"]);
            start_creation(&cx, &mut app, "coder".into());
            assert!(app.session_operation.is_none());
            assert!(app.status.contains("--cwd"));
            app.cwd = "C:\\owner-only".into();
            app.explicit_cwd = true;
            start_creation(&cx, &mut app, "coder".into());
            // A picker refresh during creation cannot change the captured launch target.
            app.profiles = vec!["another-agent".into()];
            app.cwd = "/another-owner".into();
            let result = operation_completion(&mut app.session_operation)
                .await
                .unwrap();
            app.session_operation = None;
            let created = apply_session_result(&mut app, result, "Creating session").unwrap();
            apply_created(&mut app, created);
            let created = app
                .sessions
                .iter()
                .find(|entry| entry.session_id == "created-route")
                .unwrap();
            assert_eq!(created.agent, "coder");
            assert_eq!(created.cwd, std::path::Path::new("C:\\owner-only"));
            assert_eq!(app.references["created-route"].owner.as_str(), REMOTE);
            assert!(!app.creation_uncertain);
            Ok(())
        }),
    )
    .await
    .unwrap()
    .unwrap();
}

#[test]
fn changed_gateway_aliases_rebind_drafts_receipts_and_history_without_merging_owners() {
    let reference = |owner: &str| hive_protocol::remote::SessionReference {
        owner: hive_core::identity::NodeId::try_from(owner.to_owned()).unwrap(),
        id: "same".into(),
    };
    let mut app = App {
        all: true,
        ..App::default()
    };
    app.references.insert("old-route".into(), reference(REMOTE));
    app.references
        .insert("local-route".into(), reference(LOCAL));
    app.active = Some("old-route".into());
    app.recent = vec!["old-route".into(), "local-route".into()];
    app.conversations.entry("old-route".into()).or_default().seq = 42;
    app.drafts.insert("old-route".into(), "unsent".into());
    app.pending.insert(
        "old-route".into(),
        (
            "original-key".into(),
            draft::PromptDraft {
                text: "uncertain".into(),
                images: Vec::new(),
            },
        ),
    );
    let row = |id: &str| serde_json::from_value(json!({"sessionId":id,"cwd":"/work"})).unwrap();
    apply_catalog(
        &mut app,
        Catalog {
            snapshot: SessionSnapshot {
                sessions: vec![row("new-route"), row("local-route")],
            },
            providers: Ok(json!({"agents":[],"profiles":[]})),
            owners: Vec::new(),
            references: BTreeMap::from([
                ("new-route".into(), reference(REMOTE)),
                ("local-route".into(), reference(LOCAL)),
            ]),
            availability: BTreeMap::new(),
            owner: None,
            partial: false,
        },
    );
    assert_eq!(app.active.as_deref(), Some("new-route"));
    assert_eq!(app.conversations["new-route"].seq, 42);
    assert_eq!(app.drafts["new-route"], "unsent");
    assert_eq!(app.pending["new-route"].0, "original-key");
    assert_eq!(app.recent, ["new-route", "local-route"]);
    assert!(!app.conversations.contains_key("local-route"));
}
