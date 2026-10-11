use ratatui::style::{Color, Modifier, Style};

/// Terminal defaults and ANSI colors retain the user's terminal theme, including transparency.
#[derive(Clone, Copy, Default)]
pub struct Theme;

impl Theme {
    pub fn text(self) -> Style {
        Style::default().fg(Color::Reset).bg(Color::Reset)
    }

    pub fn muted(self) -> Style {
        self.text().add_modifier(Modifier::DIM)
    }

    pub fn accent(self) -> Style {
        self.text().fg(Color::Cyan)
    }

    pub fn warning(self) -> Style {
        self.text().fg(Color::Yellow)
    }

    pub fn selected(self) -> Style {
        // Reverse video uses the terminal's own foreground and background in either mode.
        self.text().add_modifier(Modifier::REVERSED)
    }
}
