use super::*;
use hive_core::workspace::PathSyntax;
use serde_json::json;

fn node() -> NodeId {
    NodeId::try_from("node-00000000-0000-4000-8000-000000000001".to_owned()).unwrap()
}

fn path(value: &str) -> NodePath {
    NodePath::new(node(), PathSyntax::Posix, value.into()).unwrap()
}

fn context() -> ScopeContext {
    ScopeContext {
        node: node(),
        project: Some(ProjectId::from_name("hive")),
        workspace: Some(
            WorkspaceId::try_from("workspace-00000000-0000-4000-8000-000000000002".to_owned())
                .unwrap(),
        ),
    }
}

fn definition() -> ServerDefinition {
    ServerDefinition::new(
        Transport::Stdio {
            command: "server".into(),
            args: vec!["--mode".into(), "read".into()],
            env: BTreeMap::from([("TOKEN".into(), "secret-token".into())]),
        },
        Scope::Project,
        Ownership::Managed,
        Lifecycle::OnDemand {
            idle_timeout_seconds: 60,
        },
    )
    .unwrap()
}

fn identity() -> InstanceIdentity {
    InstanceIdentity::shared(
        "tools/search".into(),
        definition(),
        context(),
        ResolvedTransport::Stdio {
            executable: path("/usr/bin/server"),
            args: vec!["--mode".into(), "read".into()],
            env: BTreeMap::from([
                ("TOKEN".into(), "secret-token".into()),
                ("INHERITED".into(), "original".into()),
            ]),
            cwd: path("/projects/hive"),
        },
        Initialization {
            protocol_version: "2025-11-25".into(),
            client_info: json!({"name": "hive", "version": "1"}),
            capabilities: json!({"roots": {"listChanged": true}}),
            options: json!({"unknown-option": "preserved"}),
            roots: vec![path("/projects/hive")],
        },
        SecurityContext {
            principal: "local-user".into(),
            partition: "trusted".into(),
            policy_generation: "1".into(),
            credentials: Credentials::Complete {
                external_generations: BTreeMap::from([("credential-helper".into(), "1".into())]),
            },
        },
    )
    .unwrap()
}

// Re-enter the public checked constructor after mutations, rather than only
// testing derived equality on states that callers could never construct.
fn checked(value: InstanceIdentity, context: ScopeContext) -> Result<InstanceIdentity, McpError> {
    InstanceIdentity::shared(
        value.definition_id,
        value.definition,
        context,
        value.transport,
        value.initialization,
        value.security,
    )
}

#[test]
fn supported_definitions_round_trip_through_json_and_toml() {
    for scope in [Scope::Machine, Scope::Project, Scope::Workspace] {
        for (transport, ownership, lifecycle) in [
            (
                definition().0.transport,
                Ownership::Managed,
                Lifecycle::OnDemand {
                    idle_timeout_seconds: 0,
                },
            ),
            (
                definition().0.transport,
                Ownership::Forwarded,
                Lifecycle::Delegated,
            ),
            (
                Transport::Http {
                    url: "http://localhost:9000/mcp".into(),
                    headers: BTreeMap::new(),
                },
                Ownership::Forwarded,
                Lifecycle::Delegated,
            ),
        ] {
            let original = ServerDefinition::new(transport, scope, ownership, lifecycle).unwrap();
            assert_eq!(
                original,
                serde_json::from_str::<ServerDefinition>(
                    &serde_json::to_string(&original).unwrap()
                )
                .unwrap()
            );
            assert_eq!(
                original,
                toml::from_str::<ServerDefinition>(&toml::to_string(&original).unwrap()).unwrap()
            );
        }
    }
}

#[test]
fn unknown_fields_and_unreleased_old_scope_names_are_rejected() {
    let original = serde_json::to_value(definition()).unwrap();
    for location in [vec![], vec!["transport"], vec!["lifecycle"]] {
        let mut value = original.clone();
        let mut object = &mut value;
        for key in location {
            object = &mut object[key];
        }
        object["unexpected"] = json!(true);
        assert!(serde_json::from_value::<ServerDefinition>(value).is_err());
    }
    for (field, invalid) in [
        ("scope", "worktree"),
        ("scope", "user"),
        ("ownership", "automatic"),
    ] {
        let mut value = original.clone();
        value[field] = json!(invalid);
        assert!(serde_json::from_value::<ServerDefinition>(value).is_err());
    }
    let mut value = original;
    value["transport"]["kind"] = json!("local_http");
    assert!(serde_json::from_value::<ServerDefinition>(value).is_err());
}

#[test]
fn invalid_transport_and_lifecycle_settings_have_controlled_errors() {
    let http = |url: &str, headers| Transport::Http {
        url: url.into(),
        headers,
    };
    for url in [
        "file:///tmp/mcp",
        "https://user:secret@example.com/mcp",
        "https://example.com/#fragment",
        "not a URL",
    ] {
        assert_eq!(
            ServerDefinition::new(
                http(url, BTreeMap::new()),
                Scope::Machine,
                Ownership::Forwarded,
                Lifecycle::Delegated
            ),
            Err(McpError::Http)
        );
    }
    for headers in [
        BTreeMap::from([("Authorization".into(), "secret\ninvalid".into())]),
        BTreeMap::from([
            ("Authorization".into(), "one".into()),
            ("authorization".into(), "two".into()),
        ]),
    ] {
        assert_eq!(
            ServerDefinition::new(
                http("https://example.com/mcp", headers),
                Scope::Machine,
                Ownership::Forwarded,
                Lifecycle::Delegated
            ),
            Err(McpError::Http)
        );
    }
    assert_eq!(
        ServerDefinition::new(
            http("http://localhost/mcp", BTreeMap::new()),
            Scope::Machine,
            Ownership::Managed,
            Lifecycle::OnDemand {
                idle_timeout_seconds: 1
            }
        ),
        Err(McpError::ManagedHttp)
    );
    assert_eq!(
        ServerDefinition::new(
            definition().0.transport,
            Scope::Machine,
            Ownership::Managed,
            Lifecycle::Delegated
        ),
        Err(McpError::Lifecycle)
    );
    for transport in [
        Transport::Stdio {
            command: " ".into(),
            args: vec![],
            env: BTreeMap::new(),
        },
        Transport::Stdio {
            command: "server".into(),
            args: vec!["bad\0arg".into()],
            env: BTreeMap::new(),
        },
        Transport::Stdio {
            command: "server".into(),
            args: vec![],
            env: BTreeMap::from([("BAD=NAME".into(), "secret".into())]),
        },
    ] {
        assert_eq!(
            ServerDefinition::new(
                transport,
                Scope::Machine,
                Ownership::Forwarded,
                Lifecycle::Delegated
            ),
            Err(McpError::Process)
        );
    }
}

#[test]
fn every_behavior_affecting_input_prevents_incompatible_reuse() {
    let original = identity();
    // Each independent change must survive checked construction and split identity.
    let changes: Vec<InstanceIdentity> = (0..15)
        .map(|case| {
            let mut value = original.clone();
            match case {
                0 => value.definition_id.push_str("-other"),
                1 => {
                    value.definition.0.lifecycle = Lifecycle::OnDemand {
                        idle_timeout_seconds: 10,
                    }
                }
                2 => {
                    if let ResolvedTransport::Stdio { executable, .. } = &mut value.transport {
                        *executable = path("/opt/server");
                    }
                }
                3 => {
                    if let Transport::Stdio { args, .. } = &mut value.definition.0.transport {
                        args.reverse();
                    }
                    if let ResolvedTransport::Stdio { args, .. } = &mut value.transport {
                        args.reverse();
                    }
                }
                4 => {
                    if let ResolvedTransport::Stdio { env, .. } = &mut value.transport {
                        env.insert("INHERITED".into(), "changed".into());
                    }
                }
                5 => {
                    if let ResolvedTransport::Stdio { cwd, .. } = &mut value.transport {
                        *cwd = path("/projects/hive/subdir");
                    }
                }
                6 => value.initialization.protocol_version = "another-version".into(),
                7 => value.initialization.capabilities = json!({}),
                8 => value.initialization.options = json!({"unknown-option": "changed"}),
                9 => value.initialization.roots.push(path("/another-root")),
                10 => value.security.principal.push_str("-other"),
                11 => value.security.partition.push_str("-other"),
                12 => value.security.policy_generation = "2".into(),
                13 => {
                    value.security.credentials = Credentials::Complete {
                        external_generations: BTreeMap::from([(
                            "credential-helper".into(),
                            "2".into(),
                        )]),
                    }
                }
                14 => value.initialization.client_info = json!({"name": "another-client"}),
                _ => unreachable!(),
            }
            checked(value, context()).unwrap()
        })
        .collect();
    for (case, changed) in changes.into_iter().enumerate() {
        assert_ne!(original, changed, "case {case}");
    }
    let mut rotated = original.clone();
    if let Transport::Stdio { env, .. } = &mut rotated.definition.0.transport {
        env.insert("TOKEN".into(), "rotated".into());
    }
    if let ResolvedTransport::Stdio { env, .. } = &mut rotated.transport {
        env.insert("TOKEN".into(), "rotated".into());
    }
    assert_ne!(original, checked(rotated, context()).unwrap());
}

#[test]
fn scope_uses_registered_resources_instead_of_session_directories() {
    for scope in [Scope::Machine, Scope::Project, Scope::Workspace] {
        let mut value = identity();
        value.definition.0.scope = scope;
        let original = checked(value.clone(), context()).unwrap();
        let mut other_workspace = context();
        other_workspace.workspace = Some(WorkspaceId::new());
        let changed = checked(value.clone(), other_workspace).unwrap();
        assert_eq!(original == changed, scope != Scope::Workspace);
        let mut other_project = context();
        other_project.project = Some(ProjectId::from_name("other"));
        assert_eq!(
            original == checked(value.clone(), other_project).unwrap(),
            scope == Scope::Machine
        );
        let missing = ScopeContext {
            node: node(),
            project: None,
            workspace: None,
        };
        assert_eq!(checked(value, missing).is_ok(), scope == Scope::Machine);
    }
    let mut value = identity();
    if let ResolvedTransport::Stdio { cwd, .. } = &mut value.transport {
        *cwd = path("/projects/hive/subdir");
    }
    let changed = checked(value, context()).unwrap();
    assert_eq!(identity().scope, changed.scope);
    assert_ne!(identity(), changed);
}

#[test]
fn foreign_node_paths_and_incomplete_resolution_are_rejected() {
    let mut value = identity();
    if let ResolvedTransport::Stdio { cwd, .. } = &mut value.transport {
        *cwd = NodePath::new(NodeId::new(), PathSyntax::Posix, "/foreign".into()).unwrap();
    }
    assert_eq!(checked(value, context()), Err(McpError::Resolution));
    let mut value = identity();
    if let ResolvedTransport::Stdio { env, .. } = &mut value.transport {
        env.remove(&OsString::from("TOKEN"));
    }
    assert_eq!(checked(value, context()), Err(McpError::Resolution));
    let mut value = identity();
    value
        .initialization
        .roots
        .push(NodePath::new(NodeId::new(), PathSyntax::Posix, "/foreign".into()).unwrap());
    assert_eq!(checked(value, context()), Err(McpError::Resolution));
}

#[test]
fn unknown_credentials_require_independently_private_identities() {
    let mut value = identity();
    value.security.credentials = Credentials::Unknown;
    assert_eq!(checked(value.clone(), context()), Err(McpError::Security));
    let private = || {
        InstanceIdentity::private(
            value.definition_id.clone(),
            value.definition.clone(),
            context(),
            value.transport.clone(),
            value.initialization.clone(),
            value.security.clone(),
        )
        .unwrap()
    };
    assert_ne!(private(), private());
    let private = private();
    assert_eq!(private, private.clone());
    value.security.credentials = Credentials::Complete {
        external_generations: BTreeMap::from([("helper".into(), "".into())]),
    };
    assert_eq!(checked(value, context()), Err(McpError::Security));
}

fn http_identity(url: &str, token: &str) -> InstanceIdentity {
    let mut value = identity();
    let headers = BTreeMap::from([("authorization".into(), token.into())]);
    value.definition = ServerDefinition::new(
        Transport::Http {
            url: url.into(),
            headers: headers.clone(),
        },
        Scope::Project,
        Ownership::Forwarded,
        Lifecycle::Delegated,
    )
    .unwrap();
    value.transport = ResolvedTransport::Http {
        url: url.into(),
        headers,
        network_context: BTreeMap::new(),
    };
    checked(value, context()).unwrap()
}

#[test]
fn http_paths_queries_credentials_and_network_context_remain_significant() {
    let original = http_identity("https://example.com/mcp?a=1&b=2", "secret-token");
    for url in [
        "https://example.com/mcp/?a=1&b=2",
        "https://example.com/other?a=1&b=2",
        "https://example.com/mcp?b=2&a=1",
    ] {
        assert_ne!(original, http_identity(url, "secret-token"));
    }
    assert_ne!(
        original,
        http_identity("https://example.com/mcp?a=1&b=2", "rotated")
    );
    let mut changed = original.clone();
    if let ResolvedTransport::Http {
        network_context, ..
    } = &mut changed.transport
    {
        network_context.insert("client-certificate-generation".into(), "2".into());
    }
    assert_ne!(original, checked(changed, context()).unwrap());
}

#[test]
fn normalized_http_header_names_preserve_explicit_overrides_but_changed_values_fail() {
    let mut value = http_identity("https://example.com/mcp", "secret-token");
    if let Transport::Http { headers, .. } = &mut value.definition.0.transport {
        let token = headers.remove("authorization").unwrap();
        headers.insert("Authorization".into(), token);
    }
    let mut value = checked(value, context()).unwrap();
    if let ResolvedTransport::Http { headers, .. } = &mut value.transport {
        headers.insert("authorization".into(), "different-token".into());
    }
    assert_eq!(checked(value, context()), Err(McpError::Resolution));
}

#[test]
fn map_insertion_order_does_not_change_compatibility() {
    let original = identity();
    let mut changed = original.clone();
    if let ResolvedTransport::Stdio { env, .. } = &mut changed.transport {
        let entries = env.clone();
        env.clear();
        env.extend(entries.into_iter().rev());
    }
    changed.initialization.options = serde_json::from_str("{\"b\":2,\"a\":1}").unwrap();
    let mut other = original;
    other.initialization.options = serde_json::from_str("{\"a\":1,\"b\":2}").unwrap();
    assert_eq!(
        checked(changed, context()).unwrap(),
        checked(other, context()).unwrap()
    );
}

#[test]
fn identical_paths_on_different_execution_nodes_never_share_instances() {
    let original = identity();
    let mut changed = original.clone();
    let mut remote = context();
    remote.node = NodeId::new();
    let relocate = |path: &NodePath| {
        NodePath::new(remote.node.clone(), path.syntax(), path.as_str().into()).unwrap()
    };
    if let ResolvedTransport::Stdio {
        executable, cwd, ..
    } = &mut changed.transport
    {
        *executable = relocate(executable);
        *cwd = relocate(cwd);
    }
    changed.initialization.roots = changed.initialization.roots.iter().map(relocate).collect();
    assert_ne!(original, checked(changed, remote).unwrap());
}

#[test]
fn definition_command_ownership_and_scope_changes_split_instance_identity() {
    let original = identity();
    let mut changed = original.clone();
    if let Transport::Stdio { command, .. } = &mut changed.definition.0.transport {
        *command = "another-command-resolving-to-the-same-executable".into();
    }
    assert_ne!(original, checked(changed, context()).unwrap());
    let mut changed = original.clone();
    changed.definition.0.ownership = Ownership::Forwarded;
    changed.definition.0.lifecycle = Lifecycle::Delegated;
    assert_ne!(original, checked(changed, context()).unwrap());
    let mut changed = original.clone();
    changed.definition.0.scope = Scope::Machine;
    assert_ne!(original, checked(changed, context()).unwrap());
}

#[test]
fn debug_and_controlled_errors_never_expose_secret_inputs() {
    let value = http_identity("https://example.com/mcp?secret-query", "secret-token");
    let output = format!(
        "{value:?} {:?} {:?} {:?} {:?}",
        value.definition, value.transport, value.initialization, value.security
    );
    for secret in [
        "secret-query",
        "secret-token",
        "credential-helper",
        "local-user",
    ] {
        assert!(!output.contains(secret));
    }
    let error = ServerDefinition::new(
        Transport::Http {
            url: "https://user:secret-password@example.com".into(),
            headers: BTreeMap::new(),
        },
        Scope::Machine,
        Ownership::Forwarded,
        Lifecycle::Delegated,
    )
    .unwrap_err();
    assert!(!format!("{error} {error:?}").contains("secret-password"));
    assert_eq!(error.code().unwrap().to_string(), "hive::mcp::http");
}
