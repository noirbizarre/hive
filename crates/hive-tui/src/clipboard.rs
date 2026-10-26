//! Keep clipboard ownership alive and potentially blocking desktop IPC off the UI thread.

use tokio::sync::mpsc;

pub enum Operation {
    Copy(String),
    Cut(String),
    Paste,
}

pub struct Request {
    pub operation: Operation,
    pub session: Option<String>,
    pub original: String,
    pub editor: crate::editor::PromptEditor,
    pub target: Target,
}

pub enum Target {
    Chat,
    Question(String),
    Picker(String),
}

pub struct Completion {
    pub request: Request,
    pub result: Result<Option<String>, ClipboardError>,
}

#[derive(Debug, thiserror::Error)]
pub enum ClipboardError {
    #[error(
        "Clipboard unavailable: {0}; check your desktop clipboard service or use terminal copy/paste"
    )]
    Backend(String),
    #[error(transparent)]
    Import(#[from] crate::editor::ImportTooLarge),
}

pub struct Clipboard {
    pub requests: std::sync::mpsc::Sender<Request>,
    pub completions: mpsc::UnboundedReceiver<Completion>,
}

impl Clipboard {
    pub fn new() -> Self {
        Self::worker(|| {
            let mut clipboard = None;
            move |operation: &Operation| {
                (|| {
                    if clipboard.is_none() {
                        clipboard = Some(arboard::Clipboard::new()?);
                    }
                    let clipboard = clipboard.as_mut().expect("clipboard initialized");
                    match operation {
                        Operation::Copy(text) | Operation::Cut(text) => {
                            clipboard.set_text(text.clone()).map(|()| None)
                        }
                        Operation::Paste => clipboard.get_text().map(Some),
                    }
                })()
                .map_err(|error: arboard::Error| ClipboardError::Backend(error.to_string()))
            }
        })
    }

    fn worker<F>(make: impl FnOnce() -> F + Send + 'static) -> Self
    where
        F: FnMut(&Operation) -> Result<Option<String>, ClipboardError>,
    {
        let (requests, receiver) = std::sync::mpsc::channel::<Request>();
        let (sender, completions) = mpsc::unbounded_channel();
        std::thread::spawn(move || {
            // Initialize inside the owner thread; native handles need not be transferable between threads.
            let mut apply = make();
            while let Ok(request) = receiver.recv() {
                let result = apply(&request.operation);
                if sender.send(Completion { request, result }).is_err() {
                    break;
                }
            }
        });
        Self {
            requests,
            completions,
        }
    }
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
    let result = completion.result.and_then(|value| {
        value
            .map(|text| crate::editor::import(&text))
            .transpose()
            .map_err(ClipboardError::Import)
    });
    match result {
        Err(error) => app.status = error.to_string(),
        Ok(value) => {
            let request = completion.request;
            if matches!(request.operation, Operation::Copy(_)) {
                app.status = "Selection copied".into();
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
            if let Some(value) = value {
                app.editor
                    .insert(&mut app.input, &crate::editor::normalize(&value));
            } else if matches!(request.operation, Operation::Cut(_)) {
                app.editor.insert(&mut app.input, "");
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
        }
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
        assert_eq!(pasted.result.unwrap().as_deref(), Some(text));
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
                Operation::Paste => Ok(stored.clone()),
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
