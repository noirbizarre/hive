//! Route clipboard operations while keeping desktop IPC off the UI thread.

use base64::Engine;
use tokio::sync::mpsc;

// Terminals impose different limits; bound allocation and wire traffic without truncating selections.
const MAX_OSC52_BYTES: usize = 64 * 1024;

pub enum Operation {
    Copy(String),
    Cut(String),
    Paste,
    PasteText,
    CopyImage(std::sync::Arc<crate::images::EncodedImage>),
    ExportImage(std::sync::Arc<crate::images::EncodedImage>),
    PrepareImage(String, Vec<u8>),
}

#[derive(Clone)]
pub enum Value {
    Text(String),
    Image(std::sync::Arc<crate::draft::Attachment>),
}
impl From<&str> for Value {
    fn from(text: &str) -> Self {
        Self::Text(text.into())
    }
}
impl From<String> for Value {
    fn from(text: String) -> Self {
        Self::Text(text)
    }
}

pub struct Request {
    pub operation: Operation,
    pub session: Option<String>,
    pub original: String,
    pub editor: crate::editor::PromptEditor,
    pub target: Target,
    pub revision: u64,
}

pub enum Target {
    Chat,
    Question(String),
    Picker(String),
}

pub struct Completion {
    pub request: Request,
    pub result: Result<Option<Value>, ClipboardError>,
}

#[derive(Debug, thiserror::Error)]
pub enum ClipboardError {
    #[error(transparent)]
    Protocol(#[from] crate::osc5522::ProtocolError),
    #[error(transparent)]
    Draft(#[from] crate::draft::DraftError),
    #[error(
        "Clipboard unavailable: {0}; check your desktop clipboard service or use terminal copy/paste"
    )]
    Backend(String),
    #[error(
        "Selection is {bytes} bytes; terminal clipboard copies are limited to {MAX_OSC52_BYTES} bytes; select less text or disable OSC 52 writes to use the native clipboard"
    )]
    TooLarge { bytes: usize },
    #[error("Could not send data to the terminal clipboard: {0}; check terminal output and retry")]
    Terminal(#[source] std::io::Error),
    #[error("Clipboard worker stopped; restart Hive or use terminal copy/paste")]
    WorkerStopped,
    #[error(transparent)]
    Import(#[from] crate::editor::ImportTooLarge),
}

pub struct Clipboard {
    requests: std::sync::mpsc::SyncSender<Request>,
    pub completions: mpsc::Receiver<Completion>,
    pub protocol: std::cell::RefCell<crate::osc5522::Protocol>,
}

impl Clipboard {
    pub fn dispatch(
        &self,
        request: Request,
        capabilities: &crate::capabilities::TerminalCapabilities,
        output: &mut impl std::io::Write,
    ) -> Option<Completion> {
        if cfg!(unix)
            && capabilities.osc5522_read.is_usable()
            && matches!(request.operation, Operation::Paste | Operation::PasteText)
        {
            return self.protocol.borrow_mut().read(request, output);
        }
        let mut request = request;
        if cfg!(unix)
            && capabilities.osc5522_write.is_usable()
            && let Operation::CopyImage(image) = &request.operation
        {
            request.operation = Operation::ExportImage(image.clone());
        }
        // Prefer the user's terminal over a remote desktop clipboard when running through SSH.
        if capabilities.osc52_write.is_usable()
            && let Operation::Copy(text) | Operation::Cut(text) = &request.operation
        {
            let result = write_osc52(output, text).map(|()| None);
            return Some(Completion { request, result });
        }
        self.enqueue(request)
    }

    pub fn new() -> Self {
        Self::worker(|| {
            let mut clipboard = None;
            move |operation: &Operation| {
                (|| -> Result<Option<Value>, ClipboardError> {
                    match operation {
                        Operation::PrepareImage(mime, bytes) => {
                            return Ok(Some(Value::Image(crate::draft::Attachment::new(
                                mime.clone(),
                                bytes.clone(),
                            )?)));
                        }
                        Operation::ExportImage(image) => {
                            return Ok(Some(Value::Image(image.attachment()?)));
                        }
                        _ => {}
                    }
                    if clipboard.is_none() {
                        clipboard = Some(
                            arboard::Clipboard::new()
                                .map_err(|e| ClipboardError::Backend(e.to_string()))?,
                        );
                    }
                    let clipboard = clipboard.as_mut().expect("clipboard initialized");
                    match operation {
                        Operation::Copy(text) | Operation::Cut(text) => clipboard
                            .set_text(text.clone())
                            .map(|()| None)
                            .map_err(|e| ClipboardError::Backend(e.to_string())),
                        Operation::Paste => match clipboard.get_image() {
                            Ok(image) => Ok(Some(Value::Image(
                                crate::draft::Attachment::from_native(image)?,
                            ))),
                            Err(
                                arboard::Error::ContentNotAvailable
                                | arboard::Error::ClipboardNotSupported,
                            ) => clipboard
                                .get_text()
                                .map(|text| Some(text.into()))
                                .map_err(|e| ClipboardError::Backend(e.to_string())),
                            Err(error) => Err(ClipboardError::Backend(error.to_string())),
                        },
                        Operation::PasteText => clipboard
                            .get_text()
                            .map(|text| Some(text.into()))
                            .map_err(|error| ClipboardError::Backend(error.to_string())),
                        Operation::CopyImage(image) => clipboard
                            .set_image(image.attachment()?.native()?)
                            .map(|()| None)
                            .map_err(|e| ClipboardError::Backend(e.to_string())),
                        Operation::ExportImage(_) | Operation::PrepareImage(_, _) => {
                            unreachable!("handled before native clipboard access")
                        }
                    }
                })()
            }
        })
    }

    pub(crate) fn worker<F>(make: impl FnOnce() -> F + Send + 'static) -> Self
    where
        F: FnMut(&Operation) -> Result<Option<Value>, ClipboardError>,
    {
        // Bound queued image payloads and prepared completions while the UI is suspended.
        let (requests, receiver) = std::sync::mpsc::sync_channel::<Request>(4);
        let (sender, completions) = mpsc::channel(4);
        std::thread::spawn(move || {
            // Initialize inside the owner thread; native handles need not be transferable between threads.
            let mut apply = make();
            while let Ok(request) = receiver.recv() {
                let result = apply(&request.operation);
                if sender
                    .blocking_send(Completion { request, result })
                    .is_err()
                {
                    break;
                }
            }
        });
        Self {
            requests,
            completions,
            protocol: Default::default(),
        }
    }

    pub fn prepare_image(
        &self,
        mut request: Request,
        mime: String,
        bytes: Vec<u8>,
    ) -> Option<Completion> {
        request.operation = Operation::PrepareImage(mime, bytes);
        self.enqueue(request)
    }

    fn enqueue(&self, request: Request) -> Option<Completion> {
        use std::sync::mpsc::TrySendError;
        self.requests
            .try_send(request)
            .err()
            .map(|error| match error {
                TrySendError::Full(request) => Completion {
                    request,
                    result: Err(crate::osc5522::ProtocolError("worker is busy".into()).into()),
                },
                TrySendError::Disconnected(request) => Completion {
                    request,
                    result: Err(ClipboardError::WorkerStopped),
                },
            })
    }

    pub fn finish(
        &self,
        completion: Completion,
        output: &mut impl std::io::Write,
    ) -> Option<Completion> {
        if matches!(completion.request.operation, Operation::ExportImage(_)) {
            match completion.result {
                Ok(Some(Value::Image(image))) => {
                    self.protocol
                        .borrow_mut()
                        .write(completion.request, image, output)
                }
                result => Some(Completion {
                    request: completion.request,
                    result,
                }),
            }
        } else {
            Some(completion)
        }
    }
}

fn write_osc52(output: &mut impl std::io::Write, text: &str) -> Result<(), ClipboardError> {
    if text.len() > MAX_OSC52_BYTES {
        return Err(ClipboardError::TooLarge { bytes: text.len() });
    }
    // Only Base64 reaches the payload, so selected escape characters cannot inject terminal commands.
    let encoded = base64::engine::general_purpose::STANDARD.encode(text);
    let sequence = format!("\x1b]52;c;{encoded}\x1b\\");
    output
        .write_all(sequence.as_bytes())
        .and_then(|()| output.flush())
        .map_err(ClipboardError::Terminal)
}

pub fn command(key: crossterm::event::KeyEvent) -> Option<char> {
    use crossterm::event::{KeyCode, KeyModifiers};
    let KeyCode::Char(ch) = key.code else {
        return None;
    };
    let clipboard = key.modifiers == KeyModifiers::ALT
        || key.modifiers == (KeyModifiers::CONTROL | KeyModifiers::SHIFT);
    (clipboard && matches!(ch.to_ascii_lowercase(), 'c' | 'v' | 'x'))
        .then_some(ch.to_ascii_lowercase())
}

pub fn complete(app: &mut crate::ui::App, completion: Completion) {
    let mut image = None;
    let result = completion.result.and_then(|value| match value {
        Some(Value::Text(text)) => crate::editor::import(&text)
            .map(Some)
            .map_err(ClipboardError::Import),
        Some(Value::Image(value)) => {
            image = Some(value);
            Ok(None)
        }
        None => Ok(None),
    });
    match result {
        Err(error) => {
            app.status = error.to_string();
            if matches!(error, ClipboardError::Backend(_))
                && matches!(
                    completion.request.operation,
                    Operation::Paste | Operation::PasteText
                )
                && app.terminal_capabilities.osc52_read.is_usable()
            {
                app.status.push_str(
                    "; OSC 52 reads are not implemented; enable OSC 5522 reads or use terminal-native paste",
                );
            }
        }
        Ok(value) => {
            let request = completion.request;
            if matches!(
                request.operation,
                Operation::Copy(_) | Operation::CopyImage(_) | Operation::ExportImage(_)
            ) {
                app.status = "Selection copied".into();
                return;
            }
            if app.draft_revision != request.revision {
                app.status =
                    "Clipboard destination changed; retry paste in the intended field".into();
                return;
            }
            if image.is_some() && !matches!(request.target, Target::Chat) {
                app.status =
                    "Images belong in a chat draft; paste plain text into this field".into();
                return;
            }
            match request.target {
                Target::Question(dialog)
                    if app.active == request.session
                        && app.dialog.as_ref() == Some(&dialog)
                        && app.input == request.original =>
                {
                    if let Some(value) = value {
                        app.input
                            .push_str(&crate::editor::normalize(&value).replace('\n', " "));
                        app.draft_revision = app.draft_revision.wrapping_add(1);
                    }
                    return;
                }
                Target::Picker(query)
                    if app.active == request.session
                        && app
                            .picker
                            .as_ref()
                            .is_some_and(|picker| picker.query == query) =>
                {
                    if let Some(value) = value
                        && let Some(picker) = app.picker.as_mut()
                    {
                        picker
                            .query
                            .push_str(&crate::editor::normalize(&value).replace('\n', " "));
                    }
                    let choices = app.config_choices();
                    if let Some(picker) = app.picker.as_mut() {
                        picker.refresh(choices);
                    }
                    app.draft_revision = app.draft_revision.wrapping_add(1);
                    return;
                }
                Target::Chat => {}
                _ => {
                    app.status =
                        "Clipboard destination changed; paste again in the intended field".into();
                    return;
                }
            }
            // A late desktop response must not overwrite a new session, modal, caret, or draft.
            if app.active != request.session
                || app.draft_revision != request.revision
                || app.input != request.original
                || app.dialog.is_some()
                || app.picker.is_some()
                || app.editor.caret != request.editor.caret
                || app.editor.selection() != request.editor.selection()
            {
                app.status =
                    "Draft changed while clipboard was busy; retry the clipboard action".into();
                return;
            }
            if let Some(image) = image {
                let bytes = app
                    .attachments
                    .iter()
                    .map(|image| image.bytes.len())
                    .sum::<usize>();
                if app.attachments.len() >= crate::draft::MAX_IMAGES
                    || image.bytes.len() > crate::draft::MAX_IMAGE_BYTES.saturating_sub(bytes)
                {
                    app.status = crate::draft::DraftError::Limit.to_string();
                    return;
                }
                app.attachments.push(image);
                app.status = "Image attached · Alt-Backspace removes the last image".into();
                app.draft_revision = app.draft_revision.wrapping_add(1);
            } else if let Some(value) = value {
                app.editor
                    .insert(&mut app.input, &crate::editor::normalize(&value));
                app.draft_revision = app.draft_revision.wrapping_add(1);
            } else if matches!(request.operation, Operation::Cut(_)) {
                app.editor.insert(&mut app.input, "");
                app.draft_revision = app.draft_revision.wrapping_add(1);
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};

    fn request(app: &crate::ui::App, operation: Operation) -> Request {
        Request {
            operation,
            session: app.active.clone(),
            original: app.input.clone(),
            editor: app.editor.clone(),
            target: Target::Chat,
            revision: app.draft_revision,
        }
    }

    fn capabilities(write: bool, read: bool) -> crate::capabilities::TerminalCapabilities {
        use crate::capabilities::{Capability, Support, TerminalCapabilities};
        let permitted = Capability {
            effective: Support::Supported,
            ..Default::default()
        };
        TerminalCapabilities {
            osc52_write: if write {
                permitted
            } else {
                Capability::default()
            },
            osc52_read: if read {
                permitted
            } else {
                Capability::default()
            },
            ..Default::default()
        }
    }

    #[test]
    fn image_completions_attach_only_to_the_unchanged_chat_destination() {
        let image = crate::draft::Attachment::from_native(arboard::ImageData {
            width: 1,
            height: 1,
            bytes: vec![0; 4].into(),
        })
        .unwrap();
        let mut app = crate::ui::App {
            active: Some("one".into()),
            ..Default::default()
        };
        let original = request(&app, Operation::Paste);
        app.draft_revision += 1;
        complete(
            &mut app,
            Completion {
                request: original,
                result: Ok(Some(Value::Image(image.clone()))),
            },
        );
        assert!(app.attachments.is_empty());
        let mut modal = request(&app, Operation::Paste);
        modal.target = Target::Picker(String::new());
        complete(
            &mut app,
            Completion {
                request: modal,
                result: Ok(Some(Value::Image(image.clone()))),
            },
        );
        assert!(app.attachments.is_empty());
        let original = request(&app, Operation::Paste);
        complete(
            &mut app,
            Completion {
                request: original,
                result: Ok(Some(Value::Image(image))),
            },
        );
        assert_eq!(app.attachments.len(), 1);
        assert!(app.input.is_empty());
        assert!(app.pending.is_empty());
    }

    #[test]
    fn a_fifth_image_is_rejected_without_losing_existing_attachments() {
        let image = crate::draft::Attachment::from_native(arboard::ImageData {
            width: 1,
            height: 1,
            bytes: vec![0; 4].into(),
        })
        .unwrap();
        let mut app = crate::ui::App::default();
        for _ in 0..5 {
            let request = request(&app, Operation::Paste);
            complete(
                &mut app,
                Completion {
                    request,
                    result: Ok(Some(Value::Image(image.clone()))),
                },
            );
        }
        assert_eq!(app.attachments.len(), 4);
        assert!(app.status.contains("four images"));
        assert!(app.pending.is_empty());
    }

    #[test]
    fn terminal_copies_encode_unicode_and_control_characters_as_data() {
        for (text, expected) in [
            ("", ""),
            ("hello", "aGVsbG8="),
            ("界\n\x1b\x07", "55WMChsH"),
        ] {
            let mut output = Vec::new();
            write_osc52(&mut output, text).unwrap();
            assert_eq!(output, format!("\x1b]52;c;{expected}\x1b\\").as_bytes());
        }
    }

    #[test]
    fn terminal_copy_limits_count_utf8_bytes_before_emitting_any_output() {
        let mut text = "界".repeat(MAX_OSC52_BYTES / 3);
        text.push('a');
        assert_eq!(text.len(), MAX_OSC52_BYTES);
        let mut output = Vec::new();
        write_osc52(&mut output, &text).unwrap();
        assert_eq!(output.len(), 87_384 + 9);
        output.clear();
        text.push('a');
        assert!(matches!(
            write_osc52(&mut output, &text),
            Err(ClipboardError::TooLarge { bytes }) if bytes == MAX_OSC52_BYTES + 1
        ));
        assert!(output.is_empty());
    }

    #[test]
    fn terminal_copy_and_cut_do_not_touch_the_native_clipboard() {
        let clipboard = Clipboard::worker(|| {
            |_: &Operation| panic!("terminal copies must not use the native clipboard")
        });
        let mut app = crate::ui::App {
            input: "selected 界".into(),
            ..Default::default()
        };
        app.editor.key(
            &mut app.input,
            KeyEvent::new(KeyCode::Char('a'), KeyModifiers::CONTROL),
        );
        for operation in [
            Operation::Copy(app.input.clone()),
            Operation::Cut(app.input.clone()),
        ] {
            let mut output = Vec::new();
            let completion = clipboard
                .dispatch(
                    request(&app, operation),
                    &capabilities(true, false),
                    &mut output,
                )
                .unwrap();
            assert!(completion.result.is_ok());
            assert!(output.starts_with(b"\x1b]52;c;"));
            complete(&mut app, completion);
        }
        assert!(app.input.is_empty());
    }

    #[tokio::test]
    async fn unknown_disabled_and_unavailable_terminal_writes_use_native_copy() {
        use crate::capabilities::{CapabilitySource, Support};
        let clipboard = &mut Clipboard::worker(|| {
            |operation: &Operation| {
                assert!(matches!(operation, Operation::Copy(text) if text == "native"));
                Ok(None)
            }
        });
        for (support, source) in [
            (Support::Unknown, CapabilitySource::Automatic),
            (Support::Unsupported, CapabilitySource::Override),
            (Support::Unsupported, CapabilitySource::UnavailableTransport),
        ] {
            let mut policy = capabilities(false, false);
            policy.osc52_write.effective = support;
            policy.osc52_write.source = source;
            let mut output = Vec::new();
            assert!(
                clipboard
                    .dispatch(
                        request(&crate::ui::App::default(), Operation::Copy("native".into())),
                        &policy,
                        &mut output,
                    )
                    .is_none()
            );
            assert!(clipboard.completions.recv().await.unwrap().result.is_ok());
            assert!(output.is_empty());
        }
    }

    #[tokio::test]
    async fn osc52_read_opt_in_never_emits_queries_and_native_paste_failures_explain_the_limitation()
     {
        let mut clipboard = Clipboard::worker(|| {
            let mut first = true;
            move |operation: &Operation| {
                assert!(matches!(operation, Operation::Paste));
                if std::mem::take(&mut first) {
                    Ok(Some("native paste".into()))
                } else {
                    Err(ClipboardError::Backend("no display".into()))
                }
            }
        });
        let mut app = crate::ui::App {
            terminal_capabilities: capabilities(true, true),
            ..Default::default()
        };
        let mut output = Vec::new();
        for _ in 0..2 {
            assert!(
                clipboard
                    .dispatch(
                        request(&app, Operation::Paste),
                        &app.terminal_capabilities,
                        &mut output,
                    )
                    .is_none()
            );
            complete(&mut app, clipboard.completions.recv().await.unwrap());
        }
        assert_eq!(app.input, "native paste");
        assert!(app.status.contains("OSC 52 reads are not implemented"));
        assert!(output.is_empty());
    }

    #[test]
    fn terminal_write_and_flush_failures_preserve_cut_selections() {
        struct BrokenOutput {
            fail_flush: bool,
        }
        impl std::io::Write for BrokenOutput {
            fn write(&mut self, bytes: &[u8]) -> std::io::Result<usize> {
                if self.fail_flush {
                    Ok(bytes.len())
                } else {
                    Err(std::io::ErrorKind::BrokenPipe.into())
                }
            }
            fn flush(&mut self) -> std::io::Result<()> {
                Err(std::io::ErrorKind::BrokenPipe.into())
            }
        }
        let clipboard = Clipboard::worker(|| {
            |_: &Operation| panic!("failed terminal output must not switch clipboard destinations")
        });
        for fail_flush in [false, true] {
            let mut app = crate::ui::App {
                input: "keep me".into(),
                ..Default::default()
            };
            app.editor.key(
                &mut app.input,
                KeyEvent::new(KeyCode::Char('a'), KeyModifiers::CONTROL),
            );
            let completion = clipboard
                .dispatch(
                    request(&app, Operation::Cut(app.input.clone())),
                    &capabilities(true, false),
                    &mut BrokenOutput { fail_flush },
                )
                .unwrap();
            assert!(matches!(
                completion.result,
                Err(ClipboardError::Terminal(_))
            ));
            complete(&mut app, completion);
            assert_eq!(app.input, "keep me");
            assert_eq!(app.editor.selected().as_deref(), Some("keep me"));
            assert!(app.status.contains("check terminal output"));
        }
    }

    #[test]
    fn a_stopped_worker_reports_the_lost_clipboard_request() {
        let (requests, receiver) = std::sync::mpsc::sync_channel(4);
        drop(receiver);
        let (_sender, completions) = mpsc::channel(4);
        let clipboard = Clipboard {
            requests,
            completions,
            protocol: Default::default(),
        };
        let mut output = Vec::new();
        let completion = clipboard
            .dispatch(
                request(&crate::ui::App::default(), Operation::Paste),
                &capabilities(true, true),
                &mut output,
            )
            .unwrap();
        assert!(matches!(
            completion.result,
            Err(ClipboardError::WorkerStopped)
        ));
        assert!(output.is_empty());
    }

    #[tokio::test]
    #[ignore = "writes the native clipboard; run only on an isolated test display"]
    async fn a_native_clipboard_worker_retains_ownership_between_copy_and_paste() {
        let mut clipboard = Clipboard::new();
        let app = crate::ui::App::default();
        let text = "clipboard 界 👩‍💻\nsecond line";
        clipboard
            .requests
            .send(request(&app, Operation::Copy(text.into())))
            .unwrap();
        let copied = tokio::time::timeout(
            std::time::Duration::from_secs(5),
            clipboard.completions.recv(),
        )
        .await
        .unwrap()
        .unwrap();
        assert!(copied.result.unwrap().is_none());
        clipboard
            .requests
            .send(request(&app, Operation::Paste))
            .unwrap();
        let pasted = tokio::time::timeout(
            std::time::Duration::from_secs(5),
            clipboard.completions.recv(),
        )
        .await
        .unwrap()
        .unwrap();
        assert!(matches!(pasted.result.unwrap(), Some(Value::Text(value)) if value == text));
    }

    #[test]
    fn plain_control_c_is_never_a_clipboard_command() {
        assert_eq!(
            command(KeyEvent::new(KeyCode::Char('c'), KeyModifiers::CONTROL)),
            None
        );
        assert_eq!(
            command(KeyEvent::new(
                KeyCode::Char('C'),
                KeyModifiers::CONTROL | KeyModifiers::SHIFT
            )),
            Some('c')
        );
        assert_eq!(
            command(KeyEvent::new(KeyCode::Char('v'), KeyModifiers::ALT)),
            Some('v')
        );
    }

    #[tokio::test]
    async fn successful_worker_completions_copy_cut_and_replace_the_captured_selection() {
        let mut clipboard = Clipboard::worker(|| {
            let mut stored = None;
            move |operation: &Operation| match operation {
                Operation::Copy(text) | Operation::Cut(text) => {
                    stored = Some(text.clone());
                    Ok(None)
                }
                Operation::Paste | Operation::PasteText => Ok(stored.clone().map(Value::Text)),
                _ => panic!("text clipboard test received an image operation"),
            }
        });
        let mut app = crate::ui::App {
            input: "keep this".into(),
            ..Default::default()
        };
        app.editor.key(
            &mut app.input,
            KeyEvent::new(KeyCode::Char('a'), KeyModifiers::CONTROL),
        );
        clipboard
            .requests
            .send(request(&app, Operation::Copy(app.input.clone())))
            .unwrap();
        complete(&mut app, clipboard.completions.recv().await.unwrap());
        assert_eq!(app.input, "keep this");
        assert_eq!(app.status, "Selection copied");
        clipboard
            .requests
            .send(request(&app, Operation::Cut(app.input.clone())))
            .unwrap();
        complete(&mut app, clipboard.completions.recv().await.unwrap());
        assert!(app.input.is_empty());
        clipboard
            .requests
            .send(request(&app, Operation::Paste))
            .unwrap();
        complete(&mut app, clipboard.completions.recv().await.unwrap());
        assert_eq!(app.input, "keep this");
    }

    #[test]
    fn pasted_form_and_search_text_stays_in_the_captured_modal_without_submitting() {
        let mut app = crate::ui::App {
            input: "answer ".into(),
            dialog: Some("field one".into()),
            ..Default::default()
        };
        let mut pending = request(&app, Operation::Paste);
        pending.target = Target::Question("field one".into());
        complete(
            &mut app,
            Completion {
                request: pending,
                result: Ok(Some("one\r\ntwo".into())),
            },
        );
        assert_eq!(app.input, "answer one two");
        assert!(app.dialog.is_some());
        assert!(app.pending.is_empty());
        let mut pending = request(&app, Operation::Paste);
        pending.target = Target::Question("field one".into());
        app.dialog = Some("field two".into());
        complete(
            &mut app,
            Completion {
                request: pending,
                result: Ok(Some("stale".into())),
            },
        );
        assert_eq!(app.input, "answer one two");
        app.dialog = None;
        app.picker = Some(crate::picker::Picker::new("session".into(), vec![]));
        let mut pending = request(&app, Operation::Paste);
        pending.target = Target::Picker(String::new());
        complete(
            &mut app,
            Completion {
                request: pending,
                result: Ok(Some("model\nname".into())),
            },
        );
        assert_eq!(app.picker.as_ref().unwrap().query, "model name");
        assert!(app.pending.is_empty());
    }

    #[test]
    fn failed_cut_preserves_the_draft_and_selection() {
        let mut app = crate::ui::App {
            input: "keep me".into(),
            ..Default::default()
        };
        app.editor.key(
            &mut app.input,
            KeyEvent::new(KeyCode::Char('a'), KeyModifiers::CONTROL),
        );
        let request = request(&app, Operation::Cut("keep me".into()));
        complete(
            &mut app,
            Completion {
                request,
                result: Err(ClipboardError::Backend("no display".into())),
            },
        );
        assert_eq!(app.input, "keep me");
        assert_eq!(app.editor.selected().as_deref(), Some("keep me"));
    }

    #[test]
    fn delayed_paste_cannot_overwrite_a_different_caret_or_session() {
        let mut app = crate::ui::App {
            input: "draft".into(),
            active: Some("one".into()),
            ..Default::default()
        };
        app.editor.sync(&app.input);
        let original = request(&app, Operation::Paste);
        app.editor.key(
            &mut app.input,
            KeyEvent::new(KeyCode::Left, KeyModifiers::NONE),
        );
        complete(
            &mut app,
            Completion {
                request: original,
                result: Ok(Some("late".into())),
            },
        );
        assert_eq!(app.input, "draft");
        let original = request(&app, Operation::Paste);
        app.select_session("two".into());
        complete(
            &mut app,
            Completion {
                request: original,
                result: Ok(Some("late".into())),
            },
        );
        assert!(app.input.is_empty());
    }
}
