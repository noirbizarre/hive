//! Exercise the real key controller without requiring a user's terminal or raw mode.

use super::*;
use serde_json::{Value, json};

async fn connected(body: impl AsyncFnOnce(V2ConnectionTo<Agent>) -> Result<(), Error>) {
    let agent = Agent
        .v2()
        .on_receive_request(
            async |request: v2::InitializeRequest,
                   responder: agent_client_protocol::Responder<v2::InitializeResponse>,
                   _: V2ConnectionTo<Client>| {
                responder.respond(v2::InitializeResponse::new(
                    request.protocol_version,
                    v2::Implementation::new("keyboard-fixture", "1"),
                ))
            },
            agent_client_protocol::on_receive_request!(),
        )
        .on_receive_notification(
            async |_: v2::CancelSessionNotification, _: V2ConnectionTo<Client>| Ok(()),
            agent_client_protocol::on_receive_notification!(),
        )
        .on_receive_request(
            async |request: v2::SetSessionConfigOptionRequest,
                   responder: agent_client_protocol::Responder<
                v2::SetSessionConfigOptionResponse,
            >,
                   _: V2ConnectionTo<Client>| {
                assert_eq!(request.session_id.to_string(), "hive-test");
                assert!(
                    ["model", "collaboration_mode"]
                        .contains(&request.config_id.to_string().as_str())
                );
                assert_eq!(request.meta.unwrap()["hive"]["token"], "test");
                let v2::SessionConfigOptionValue::Id { value } = request.value else {
                    return Err(Error::invalid_params());
                };
                responder.respond(v2::SetSessionConfigOptionResponse::new(vec![
                    v2::SessionConfigOption::select(
                        request.config_id,
                        "Model",
                        value,
                        vec![v2::SessionConfigSelectOption::new(
                            v2::SessionConfigValueId::new("careful"),
                            "Careful",
                        )],
                    ),
                ]))
            },
            agent_client_protocol::on_receive_request!(),
        );
    tokio::time::timeout(
        Duration::from_secs(5),
        Client.v2().connect_with(agent, async move |cx| {
            cx.send_request(v2::InitializeRequest::new(
                ProtocolVersion::V2,
                v2::Implementation::new("keyboard-test", "1"),
            ))
            .block_task()
            .await?;
            body(cx).await
        }),
    )
    .await
    .unwrap()
    .unwrap();
}

async fn key(
    cx: &V2ConnectionTo<Agent>,
    app: &mut App,
    pending: &mut Option<Interaction>,
    code: KeyCode,
    modifiers: KeyModifiers,
) {
    let (acks, _) = mpsc::unbounded_channel();
    let (created, _) = mpsc::unbounded_channel();
    assert!(
        !handle_key(
            cx,
            "127.0.0.1:0",
            app,
            event::KeyEvent::new(code, modifiers),
            pending,
            acks,
            created
        )
        .await
    );
}

async fn press(
    cx: &V2ConnectionTo<Agent>,
    app: &mut App,
    pending: &mut Option<Interaction>,
    code: KeyCode,
) {
    key(cx, app, pending, code, KeyModifiers::NONE).await;
}

async fn text(
    cx: &V2ConnectionTo<Agent>,
    app: &mut App,
    pending: &mut Option<Interaction>,
    value: &str,
) {
    for ch in value.chars() {
        press(cx, app, pending, KeyCode::Char(ch)).await;
    }
}

async fn erase(cx: &V2ConnectionTo<Agent>, app: &mut App, pending: &mut Option<Interaction>) {
    while !app.input.is_empty() {
        press(cx, app, pending, KeyCode::Backspace).await;
    }
}

#[tokio::test]
async fn editing_and_session_switches_preserve_unicode_carets_without_submitting_pasted_lines() {
    connected(async |cx| {
        let mut app = App::default();
        app.select_session("one".into());
        text(&cx, &mut app, &mut None, "a👩‍💻z").await;
        press(&cx, &mut app, &mut None, KeyCode::Left).await;
        key(&cx, &mut app, &mut None, KeyCode::Left, KeyModifiers::SHIFT).await;
        assert_eq!(app.editor.selected().as_deref(), Some("👩‍💻"));
        paste(&mut app, &None, "界\r\nnext");
        assert_eq!(app.input, "a界\nnextz");
        let caret = app.editor.caret;
        key(&cx, &mut app, &mut None, KeyCode::Enter, KeyModifiers::ALT).await;
        assert_eq!(app.input, "a界\nnext\nz");
        assert!(app.pending.is_empty());
        press(&cx, &mut app, &mut None, KeyCode::Left).await;
        app.select_session("two".into());
        text(&cx, &mut app, &mut None, "other draft").await;
        app.select_session("one".into());
        assert_eq!(app.editor.caret, caret);
        assert_eq!(app.input, "a界\nnext\nz");
        key(
            &cx,
            &mut app,
            &mut None,
            KeyCode::Char('k'),
            KeyModifiers::CONTROL,
        )
        .await;
        key(
            &cx,
            &mut app,
            &mut None,
            KeyCode::Char('C'),
            KeyModifiers::CONTROL | KeyModifiers::SHIFT,
        )
        .await;
        assert_eq!(app.input, "a界\nnext\nz");
        Ok(())
    })
    .await;
}

#[tokio::test]
async fn submitted_prompts_wait_for_the_authoritative_echo_and_identical_prompts_stay_distinct() {
    connected(async |cx| {
        let mut app = App {
            connected: true,
            ..App::default()
        };
        app.select_session("hive-test".into());
        app.conversations
            .entry("hive-test".into())
            .or_default()
            .generation = 1;
        app.owner = Some(
            [(
                "hive".into(),
                json!({"role":"pilot","generation":1,"token":"test"}),
            )]
            .into_iter()
            .collect(),
        );
        for seq in 1..=2 {
            app.input = "same question".into();
            press(&cx, &mut app, &mut None, KeyCode::Enter).await;
            assert_eq!(app.conversation().unwrap().lines.len(), seq as usize - 1);
            let event = v2::UpdateSessionNotification::new(
                v2::SessionId::new("hive-test"),
                v2::SessionUpdate::UserMessage(
                    v2::UserMessage::new(v2::MessageId::new(format!("user-{seq}")))
                        .content(vec!["same question".into()]),
                ),
            )
            .meta(
                [("hive".into(), json!({"seq":seq}))]
                    .into_iter()
                    .collect::<v2::Meta>(),
            );
            let state = app.conversations.get_mut("hive-test").unwrap();
            assert!(state.apply(event.clone()));
            assert!(!state.apply(event));
            assert_eq!(state.lines.len(), seq as usize);
            app.pending.clear();
        }
        key(
            &cx,
            &mut app,
            &mut None,
            KeyCode::Char('p'),
            KeyModifiers::CONTROL,
        )
        .await;
        assert!(app.picker.is_some());
        app.owner = None;
        press(&cx, &mut app, &mut None, KeyCode::Enter).await;
        assert!(app.configuration.is_none());
        assert!(app.status.contains("Take control"));
        Ok(())
    })
    .await;
}

#[tokio::test]
async fn session_settings_have_their_own_picker_and_send_native_identifiers() {
    connected(async |cx| {
        let mut app = App { connected: true, ..App::default() };
        app.select_session("hive-test".into());
        let state = app.conversations.entry("hive-test".into()).or_default();
        state.generation = 1;
        state.config_options = serde_json::from_value(json!([
            {"configId":"model","name":"Model","category":"model","type":"select","currentValue":"fast","options":[{"value":"fast","name":"Fast"}]},
            {"configId":"collaboration_mode","name":"Collaboration mode","category":"collaboration_mode","type":"select","currentValue":"default","options":[{"value":"default","name":"Default"},{"value":"plan","name":"Plan"}]}
        ])).unwrap();
        app.owner = Some([("hive".into(), json!({"role":"pilot","generation":1,"token":"test"}))].into_iter().collect());
        app.input = "keep draft".into();
        key(&cx, &mut app, &mut None, KeyCode::Char('p'), KeyModifiers::CONTROL).await;
        assert_eq!(app.picker.as_ref().unwrap().choices.len(), 1);
        assert_eq!(app.config_choices()[0].0, "model");
        press(&cx, &mut app, &mut None, KeyCode::Esc).await;
        key(&cx, &mut app, &mut None, KeyCode::Char('g'), KeyModifiers::CONTROL).await;
        assert_eq!(app.picker.as_ref().unwrap().kind, picker::Kind::Settings);
        assert!(app.config_choices().iter().all(|choice| choice.0 == "collaboration_mode"));
        text(&cx, &mut app, &mut None, "Plan").await;
        press(&cx, &mut app, &mut None, KeyCode::Enter).await;
        let (id, result) = app.configuration.take().unwrap().await.unwrap();
        let response = result.unwrap();
        assert_eq!(serde_json::to_value(&response.config_options).unwrap()[0]["currentValue"], "plan");
        configuration_result(&mut app, id, Ok(response));
        assert!(app.picker.is_none());
        assert_eq!(app.input, "keep draft");
        Ok(())
    }).await;
}

#[tokio::test]
async fn the_model_picker_preserves_provider_labels_and_sends_the_advertised_value_with_ownership()
{
    connected(async |cx| {
        let mut app = App { connected: true, ..App::default() };
        app.select_session("hive-test".into());
        let state = app.conversations.entry("hive-test".into()).or_default();
        state.generation = 1;
        state.config_options = serde_json::from_value(json!([{
            "configId":"model", "name":"Model", "category":"model", "type":"select", "currentValue":"fast",
            "options":[{"groupId":"opaque-provider", "name":"Provider", "options":[{"value":"fast","name":"Fast"},{"value":"careful","name":"Careful"}]}]
        }])).unwrap();
        app.owner = Some([("hive".into(), json!({"role":"pilot","generation":1,"token":"test"}))].into_iter().collect());
        key(&cx, &mut app, &mut None, KeyCode::Char('p'), KeyModifiers::CONTROL).await;
        assert!(app.config_choices()[1].2.contains("Provider / Careful"));
        app.input = "unfinished prompt".into();
        text(&cx, &mut app, &mut None, "prv crfl").await;
        assert_eq!(app.picker.as_ref().unwrap().choices.len(), 1);
        press(&cx, &mut app, &mut None, KeyCode::Enter).await;
        // A second Enter must keep the in-flight receiver rather than submit another change.
        press(&cx, &mut app, &mut None, KeyCode::Enter).await;
        assert_eq!(app.picker.as_ref().unwrap().message, "Updating session configuration…");
        let (id, response) = app.configuration.take().unwrap().await.unwrap();
        assert_eq!(id, "hive-test");
        let response = response.unwrap();
        assert_eq!(serde_json::to_value(&response.config_options).unwrap()[0]["currentValue"], "careful");
        // Selection confirmation must come from the response/update, never the requested value alone.
        assert!(app.config_choices()[0].2.ends_with(" *"));
        configuration_result(&mut app, id, Ok(response));
        assert!(app.picker.is_none());
        assert_eq!(app.input, "unfinished prompt");
        Ok(())
    }).await;
}

#[tokio::test]
async fn dismissing_search_preserves_the_draft_and_no_matches_cannot_submit() {
    connected(async |cx| {
        let mut app = App {
            connected: true,
            ..App::default()
        };
        app.select_session("hive-test".into());
        app.owner = Some(
            [(
                "hive".into(),
                json!({"role":"pilot","generation":1,"token":"test"}),
            )]
            .into_iter()
            .collect(),
        );
        let state = app.conversations.entry("hive-test".into()).or_default();
        state.generation = 1;
        state.config_options = serde_json::from_value(json!([{
            "configId":"model", "name":"Model", "category":"model", "type":"select", "currentValue":"fast",
            "options":[{"value":"fast","name":"Fast"}]
        }]))
        .unwrap();
        app.input = "keep this draft".into();
        key(
            &cx,
            &mut app,
            &mut None,
            KeyCode::Char('p'),
            KeyModifiers::CONTROL,
        )
        .await;
        text(&cx, &mut app, &mut None, "unmatched").await;
        assert!(app.picker.as_ref().unwrap().choices.is_empty());
        press(&cx, &mut app, &mut None, KeyCode::Enter).await;
        assert!(app.configuration.is_none());
        press(&cx, &mut app, &mut None, KeyCode::Esc).await;
        assert!(app.picker.is_none());
        assert_eq!(app.input, "keep this draft");
        assert!(app.page == Page::Chat);
        Ok(())
    })
    .await;
}

#[tokio::test]
async fn history_keys_and_mouse_scroll_without_editing_the_draft_or_leaking_through_a_modal() {
    connected(async |cx| {
        let mut app = App::default();
        app.select_session("one".into());
        app.input = "unfinished prompt".into();
        for number in 0..40 {
            app.conversations
                .entry("one".into())
                .or_default()
                .push_notice(format!("Message {number}"));
        }
        let mut terminal =
            ratatui::Terminal::new(ratatui::backend::TestBackend::new(80, 20)).unwrap();
        terminal.draw(|frame| app.show(frame)).unwrap();
        press(&cx, &mut app, &mut None, KeyCode::PageUp).await;
        let top = app.transcript().unwrap().top.unwrap();
        let area = app.transcript().unwrap().area;
        handle_mouse(
            &mut app,
            event::MouseEvent {
                kind: MouseEventKind::ScrollUp,
                column: area.x + 2,
                row: area.y + 2,
                modifiers: KeyModifiers::NONE,
            },
        );
        assert_eq!(app.transcript().unwrap().top, Some(top - 3));
        key(
            &cx,
            &mut app,
            &mut None,
            KeyCode::Char('p'),
            KeyModifiers::CONTROL,
        )
        .await;
        press(&cx, &mut app, &mut None, KeyCode::PageDown).await;
        handle_mouse(
            &mut app,
            event::MouseEvent {
                kind: MouseEventKind::ScrollDown,
                column: area.x + 2,
                row: area.y + 2,
                modifiers: KeyModifiers::NONE,
            },
        );
        assert_eq!(app.transcript().unwrap().top, Some(top - 3));
        press(&cx, &mut app, &mut None, KeyCode::Esc).await;
        app.select_session("two".into());
        assert!(app.transcript().unwrap().top.is_none());
        app.select_session("one".into());
        assert_eq!(app.transcript().unwrap().top, Some(top - 3));
        assert_eq!(app.input, "unfinished prompt");
        press(&cx, &mut app, &mut None, KeyCode::End).await;
        assert!(app.transcript().unwrap().top.is_none());
        press(&cx, &mut app, &mut None, KeyCode::Home).await;
        assert_eq!(app.transcript().unwrap().top, Some(0));
        Ok(())
    })
    .await;
}

#[test]
fn rejected_changes_keep_search_and_old_successes_do_not_close_a_reopened_picker() {
    let mut app = App::default();
    app.select_session("session".into());
    app.picker = Some(picker::Picker::new("session".into(), vec![]));
    let picker = app.picker.as_mut().unwrap();
    picker.pending = true;
    picker.query = "careful".into();
    configuration_result(
        &mut app,
        "session".into(),
        Err("Agent rejected model".into()),
    );
    let picker = app.picker.as_ref().unwrap();
    assert_eq!(picker.query, "careful");
    assert_eq!(picker.message, "Agent rejected model");
    assert!(!picker.pending);
    app.picker = Some(picker::Picker::new("session".into(), vec![]));
    configuration_result(
        &mut app,
        "session".into(),
        Ok(v2::SetSessionConfigOptionResponse::new(vec![])),
    );
    assert!(app.picker.is_some());
    let (interaction, _receiver) = permission();
    show_interaction(&mut app, &interaction);
    assert!(app.picker.is_none());
    assert!(app.dialog.is_some());
}

fn form(
    request: Value,
) -> (
    Interaction,
    oneshot::Receiver<v2::CreateElicitationResponse>,
) {
    let (tx, rx) = oneshot::channel();
    (
        Interaction::Elicitation(
            serde_json::from_value(request).unwrap(),
            tx,
            FormDraft::default(),
        ),
        rx,
    )
}

fn permission() -> (
    Interaction,
    oneshot::Receiver<v2::RequestPermissionResponse>,
) {
    let request = v2::RequestPermissionRequest::new(
        v2::SessionId::new("session"),
        "Run the command?",
        vec![
            v2::PermissionOption::new("allow", "Allow", v2::PermissionOptionKind::AllowOnce),
            v2::PermissionOption::new("reject", "Reject", v2::PermissionOptionKind::RejectAlways),
        ],
    )
    .description("Review the operation before choosing")
    .subject(v2::RequestPermissionSubject::from(
        v2::CommandPermissionSubject::new("cargo test", std::env::current_dir().unwrap()),
    ));
    let (tx, rx) = oneshot::channel();
    (Interaction::Permission(request, tx), rx)
}

#[tokio::test]
async fn permission_keys_require_a_listed_choice_and_escape_never_approves() {
    connected(async |cx| {
        let mut app = App::default();
        let (request, mut answer) = permission();
        show_interaction(&mut app, &request);
        let dialog = app.dialog.as_deref().unwrap();
        assert!(dialog.contains("cargo test"));
        assert!(dialog.contains("Review the operation"));
        assert!(dialog.contains("allow_once"));
        assert!(dialog.contains("reject_always"));
        assert!(!request.expired());
        assert!(!request.request_scoped());
        assert!(request.url_id().is_none());
        let mut pending = Some(request);
        press(&cx, &mut app, &mut pending, KeyCode::PageDown).await;
        assert_eq!(app.dialog_scroll, 4);
        press(&cx, &mut app, &mut pending, KeyCode::PageUp).await;
        assert_eq!(app.dialog_scroll, 0);
        press(&cx, &mut app, &mut pending, KeyCode::Enter).await;
        assert!(matches!(
            answer.try_recv(),
            Err(oneshot::error::TryRecvError::Empty)
        ));
        assert!(app.status.contains("listed permission"));
        text(&cx, &mut app, &mut pending, "9x").await;
        assert_eq!(app.input, "9");
        press(&cx, &mut app, &mut pending, KeyCode::Enter).await;
        assert!(pending.is_some());
        press(&cx, &mut app, &mut pending, KeyCode::Backspace).await;
        press(&cx, &mut app, &mut pending, KeyCode::Char('2')).await;
        press(&cx, &mut app, &mut pending, KeyCode::Enter).await;
        assert_eq!(
            serde_json::to_value(answer.await.unwrap()).unwrap()["outcome"]["optionId"],
            "reject"
        );
        assert!(pending.is_none());
        assert!(app.dialog.is_none());
        assert!(app.input.is_empty());

        let (request, answer) = permission();
        show_interaction(&mut app, &request);
        pending = Some(request);
        let (acks, _) = mpsc::unbounded_channel();
        let (created, _) = mpsc::unbounded_channel();
        let released = event::KeyEvent {
            kind: KeyEventKind::Release,
            ..event::KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE)
        };
        assert!(
            !handle_key(
                &cx,
                "127.0.0.1:0",
                &mut app,
                released,
                &mut pending,
                acks,
                created
            )
            .await
        );
        assert!(pending.is_some());
        press(&cx, &mut app, &mut pending, KeyCode::Esc).await;
        assert!(matches!(
            answer.await.unwrap().outcome,
            v2::RequestPermissionOutcome::Cancelled
        ));
        Ok(())
    })
    .await;
}

#[tokio::test]
async fn form_keys_retain_invalid_drafts_and_submit_typed_values_only_after_validation() {
    connected(async |cx| {
        let (request, mut answer) = form(json!({"mode":"form","sessionId":"session","message":"Configure the operation","requestedSchema":{
            "properties":{
                "a_name":{"type":"string","title":"Name","description":"A descriptive name","default":"Seed","minLength":2},
                "b_mode":{"type":"string","oneOf":[{"const":"safe","title":"Safe mode"},{"const":"fast","title":"Fast mode"}]},
                "c_approved":{"type":"boolean"},
                "d_tags":{"type":"array","items":{"anyOf":[{"const":"a|b","title":"First"},{"const":"c","title":"Second"}]},"minItems":1,"maxItems":2},
                "e_note":{"type":"string"},
                "f_free":{"type":"array","items":{"type":"string","enum":["other"]}},
                "g_count":{"type":"integer","minimum":0,"maximum":2},
                "h_ratio":{"type":"number","default":0.5}
            },"required":["a_name","b_mode","c_approved","d_tags","g_count"]}}));
        let mut app = App::default();
        show_interaction(&mut app, &request);
        assert_eq!(app.input, "Seed");
        assert!(app.dialog.as_deref().unwrap().contains("required"));
        assert!(app.dialog.as_deref().unwrap().contains("minLength"));
        let mut pending = Some(request);
        press(&cx, &mut app, &mut pending, KeyCode::Enter).await;
        assert!(matches!(answer.try_recv(), Err(oneshot::error::TryRecvError::Empty)));
        assert!(app.status.contains("required"));
        press(&cx, &mut app, &mut pending, KeyCode::Backspace).await;
        press(&cx, &mut app, &mut pending, KeyCode::Char('d')).await;
        press(&cx, &mut app, &mut pending, KeyCode::Tab).await;
        press(&cx, &mut app, &mut pending, KeyCode::Down).await;
        assert_eq!(app.input, "safe");
        press(&cx, &mut app, &mut pending, KeyCode::Down).await;
        assert_eq!(app.input, "fast");
        press(&cx, &mut app, &mut pending, KeyCode::Up).await;
        assert_eq!(app.input, "safe");
        press(&cx, &mut app, &mut pending, KeyCode::BackTab).await;
        assert_eq!(app.input, "Seed");
        press(&cx, &mut app, &mut pending, KeyCode::Tab).await;
        assert_eq!(app.input, "safe");
        press(&cx, &mut app, &mut pending, KeyCode::Tab).await;
        press(&cx, &mut app, &mut pending, KeyCode::Down).await;
        press(&cx, &mut app, &mut pending, KeyCode::Down).await;
        assert_eq!(app.input, "false");
        press(&cx, &mut app, &mut pending, KeyCode::Tab).await;
        press(&cx, &mut app, &mut pending, KeyCode::Char(' ')).await;
        press(&cx, &mut app, &mut pending, KeyCode::Down).await;
        press(&cx, &mut app, &mut pending, KeyCode::Char(' ')).await;
        assert_eq!(serde_json::from_str::<Value>(&app.input).unwrap(), json!(["a|b","c"]));
        press(&cx, &mut app, &mut pending, KeyCode::Up).await;
        press(&cx, &mut app, &mut pending, KeyCode::Char(' ')).await;
        assert_eq!(serde_json::from_str::<Value>(&app.input).unwrap(), json!(["c"]));
        erase(&cx, &mut app, &mut pending).await;
        text(&cx, &mut app, &mut pending, "[broken").await;
        press(&cx, &mut app, &mut pending, KeyCode::Char(' ')).await;
        assert!(app.status.contains("JSON string array"));
        erase(&cx, &mut app, &mut pending).await;
        text(&cx, &mut app, &mut pending, "[\"a|b\"]").await;
        press(&cx, &mut app, &mut pending, KeyCode::Tab).await;
        key(&cx, &mut app, &mut pending, KeyCode::Char(' '), KeyModifiers::CONTROL).await;
        press(&cx, &mut app, &mut pending, KeyCode::Tab).await;
        key(&cx, &mut app, &mut pending, KeyCode::Char(' '), KeyModifiers::CONTROL).await;
        press(&cx, &mut app, &mut pending, KeyCode::Down).await;
        assert!(app.input.is_empty());
        press(&cx, &mut app, &mut pending, KeyCode::Tab).await;
        text(&cx, &mut app, &mut pending, "3").await;
        press(&cx, &mut app, &mut pending, KeyCode::Enter).await;
        assert!(app.status.contains("bounds"));
        assert!(matches!(answer.try_recv(), Err(oneshot::error::TryRecvError::Empty)));
        assert_eq!(app.input, "3");
        press(&cx, &mut app, &mut pending, KeyCode::Tab).await;
        assert_eq!(app.input, "0.5");
        key(&cx, &mut app, &mut pending, KeyCode::Char(' '), KeyModifiers::CONTROL).await;
        assert_eq!(app.input, "0.5");
        press(&cx, &mut app, &mut pending, KeyCode::BackTab).await;
        erase(&cx, &mut app, &mut pending).await;
        text(&cx, &mut app, &mut pending, "2").await;
        press(&cx, &mut app, &mut pending, KeyCode::Enter).await;
        let response = serde_json::to_value(answer.await.unwrap()).unwrap();
        assert_eq!(response, json!({"action":"accept","content":{"a_name":"Seed","b_mode":"safe","c_approved":false,"d_tags":["a|b"],"e_note":"","f_free":[],"g_count":2,"h_ratio":0.5}}));
        assert!(pending.is_none());
        assert!(app.input.is_empty());
        Ok(())
    }).await;
}

#[tokio::test]
async fn url_consent_and_unsupported_forms_never_accept_without_an_explicit_supported_action() {
    connected(async |cx| {
        let mut app = App::default();
        let url = json!({"mode":"url","sessionId":"session","message":"Authenticate","elicitationId":"auth","url":"https://example.com/auth"});
        let (request, mut answer) = form(url.clone());
        assert_eq!(request.url_id().as_deref(), Some("auth"));
        show_interaction(&mut app, &request);
        assert!(app.dialog.as_deref().unwrap().contains("https://example.com/auth"));
        assert!(matches!(answer.try_recv(), Err(oneshot::error::TryRecvError::Empty)));
        let mut pending = Some(request);
        press(&cx, &mut app, &mut pending, KeyCode::Esc).await;
        assert!(matches!(answer.await.unwrap().action, v2::ElicitationAction::Decline));
        let (request, answer) = form(url);
        show_interaction(&mut app, &request);
        pending = Some(request);
        press(&cx, &mut app, &mut pending, KeyCode::Enter).await;
        assert!(matches!(answer.await.unwrap().action, v2::ElicitationAction::Accept(_)));

        let (request, mut answer) = form(json!({"mode":"future","sessionId":"session","message":"Unsupported input"}));
        show_interaction(&mut app, &request);
        assert!(app.dialog.as_deref().unwrap().contains("Unsupported"));
        pending = Some(request);
        press(&cx, &mut app, &mut pending, KeyCode::Enter).await;
        assert!(matches!(answer.try_recv(), Err(oneshot::error::TryRecvError::Empty)));
        assert!(app.status.contains("unsupported"));
        press(&cx, &mut app, &mut pending, KeyCode::Esc).await;
        assert!(matches!(answer.await.unwrap().action, v2::ElicitationAction::Decline));

        let (request, answer) = form(json!({"mode":"form","sessionId":"session","message":"No fields","requestedSchema":{"properties":{}}}));
        show_interaction(&mut app, &request);
        pending = Some(request);
        press(&cx, &mut app, &mut pending, KeyCode::Tab).await;
        press(&cx, &mut app, &mut pending, KeyCode::Up).await;
        press(&cx, &mut app, &mut pending, KeyCode::Enter).await;
        assert!(matches!(answer.await.unwrap().action, v2::ElicitationAction::Accept(_)));
        Ok(())
    }).await;
}

#[tokio::test]
async fn cancelling_a_session_dismisses_its_dialog_and_does_not_reuse_form_input_as_a_prompt() {
    connected(async |cx| {
        let mut app = App {
            active: Some("session".into()),
            page: Page::Chat,
            owner: Some(v2::Meta::from_iter([(
                "hive".into(),
                json!({"role":"pilot","generation":1,"token":"owner"}),
            )])),
            ..App::default()
        };
        app.conversations
            .entry("session".into())
            .or_default()
            .generation = 1;
        let (request, answer) = form(json!({"mode":"form","sessionId":"session","message":"Enter a private value","requestedSchema":{"properties":{"secret":{"type":"string"}},"required":["secret"]}}));
        show_interaction(&mut app, &request);
        let mut pending = Some(request);
        text(&cx, &mut app, &mut pending, "private draft").await;
        key(
            &cx,
            &mut app,
            &mut pending,
            KeyCode::Char('c'),
            KeyModifiers::CONTROL,
        )
        .await;
        assert!(pending.is_none());
        assert!(app.dialog.is_none());
        assert!(app.input.is_empty());
        assert!(answer.await.is_err());
        assert_eq!(app.active.as_deref(), Some("session"));
        Ok(())
    })
    .await;
}
