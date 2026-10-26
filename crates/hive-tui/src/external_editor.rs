//! External editors own the terminal until their process exits; imports remain unsent drafts.

use std::io::Write;
use tokio::io::AsyncReadExt;

#[derive(Debug, thiserror::Error)]
pub enum EditorError {
    #[error(transparent)]
    Import(#[from] crate::editor::ImportTooLarge),
    #[error("Invalid editor command: {0}; set VISUAL or EDITOR to an executable and arguments")]
    Command(String),
    #[error("Could not {action}: {source}; check EDITOR and temporary-directory permissions")]
    Io {
        action: &'static str,
        #[source]
        source: std::io::Error,
    },
    #[error("Editor exited with {0}; draft preserved, fix the editor command and retry")]
    Exit(std::process::ExitStatus),
}

pub struct Prepared {
    file: tempfile::TempPath,
    command: Vec<String>,
    session: Option<String>,
    original: String,
}

pub struct Edited {
    pub session: Option<String>,
    pub original: String,
    pub text: String,
}

pub struct Task(pub tokio::task::JoinHandle<Result<Edited, EditorError>>);

impl Drop for Task {
    fn drop(&mut self) {
        // Dropping a JoinHandle detaches it; abort so kill_on_drop also handles UI error exits.
        self.0.abort();
    }
}

fn io(action: &'static str, source: std::io::Error) -> EditorError {
    EditorError::Io { action, source }
}

impl Prepared {
    pub fn new(session: Option<String>, original: String) -> Result<Self, EditorError> {
        let command = ["VISUAL", "EDITOR"]
            .into_iter()
            .filter_map(|key| std::env::var(key).ok())
            .find(|value| !value.trim().is_empty())
            .unwrap_or_else(|| "vi".into());
        let command = shell_words::split(&command)
            .map_err(|error| EditorError::Command(error.to_string()))?;
        if command.is_empty() {
            return Err(EditorError::Command("empty executable".into()));
        }
        let mut file = tempfile::Builder::new()
            .prefix("hive-prompt-")
            .suffix(".md")
            .tempfile()
            .map_err(|error| io("create prompt file", error))?;
        file.write_all(original.as_bytes())
            .map_err(|error| io("write prompt file", error))?;
        file.flush()
            .map_err(|error| io("flush prompt file", error))?;
        Ok(Self {
            // Close the writer before another process opens or atomically replaces the file.
            file: file.into_temp_path(),
            command,
            session,
            original,
        })
    }

    pub async fn run(self) -> Result<Edited, EditorError> {
        let status = tokio::process::Command::new(&self.command[0])
            .args(&self.command[1..])
            .arg(self.file.as_os_str())
            .stdin(std::process::Stdio::inherit())
            .stdout(std::process::Stdio::inherit())
            .stderr(std::process::Stdio::inherit())
            .kill_on_drop(true)
            .status()
            .await
            .map_err(|error| io("launch editor", error))?;
        if !status.success() {
            return Err(EditorError::Exit(status));
        }
        let file = tokio::fs::File::open(&self.file)
            .await
            .map_err(|error| io("open edited prompt", error))?;
        let mut text = String::new();
        file.take((crate::editor::MAX_IMPORT_BYTES + 1) as u64)
            .read_to_string(&mut text)
            .await
            .map_err(|error| io("read edited prompt as UTF-8", error))?;
        Ok(Edited {
            session: self.session,
            original: self.original,
            text: crate::editor::import(&text)?,
        })
    }
}

pub fn apply(app: &mut crate::ui::App, edited: Edited) {
    // An incoming question can be shown while reconnecting; import into the draft, never its answer.
    let draft = app.suspended_draft.as_mut().unwrap_or(&mut app.input);
    if app.active == edited.session && *draft == edited.original {
        *draft = edited.text;
        app.status = "Editor changes loaded; Enter sends the draft".into();
    } else if let Some(session) = edited.session {
        if app.drafts.get(&session) == Some(&edited.original) {
            app.drafts.insert(session, edited.text);
        } else {
            app.status = "Draft changed while editing; external edit could not be applied".into();
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[cfg(unix)]
    fn fixture(script: &str) -> Prepared {
        // The fixed test shell models an editor executable; production never constructs shell code.
        Prepared {
            file: tempfile::NamedTempFile::new().unwrap().into_temp_path(),
            command: vec!["sh".into(), "-c".into(), script.into(), "fixture".into()],
            session: Some("one".into()),
            original: "old".into(),
        }
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn successful_edits_are_loaded_without_sending_and_empty_edits_are_valid() {
        let edited = fixture("printf 'new\\ntext' > \"$1\"").run().await.unwrap();
        let mut app = crate::ui::App {
            active: Some("one".into()),
            input: "old".into(),
            ..Default::default()
        };
        apply(&mut app, edited);
        assert_eq!(app.input, "new\ntext");
        assert!(app.pending.is_empty());
        assert!(fixture(": > \"$1\"").run().await.unwrap().text.is_empty());
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn nonzero_exit_and_invalid_utf8_reject_editor_output() {
        assert!(matches!(
            fixture("exit 7").run().await,
            Err(EditorError::Exit(_))
        ));
        assert!(matches!(
            fixture("printf '\\377' > \"$1\"").run().await,
            Err(EditorError::Io { .. })
        ));
    }

    #[test]
    fn an_edit_returning_during_a_question_updates_only_the_suspended_draft() {
        let mut app = crate::ui::App {
            active: Some("one".into()),
            input: "private answer".into(),
            suspended_draft: Some("old".into()),
            ..Default::default()
        };
        apply(
            &mut app,
            Edited {
                session: Some("one".into()),
                original: "old".into(),
                text: "new".into(),
            },
        );
        assert_eq!(app.input, "private answer");
        assert_eq!(app.suspended_draft.as_deref(), Some("new"));
    }
}
