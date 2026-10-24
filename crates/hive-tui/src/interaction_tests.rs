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
