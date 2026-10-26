use ratatui::{Frame, style::Modifier, text::Span};

pub fn executing(status: &str) -> bool {
    matches!(status, "in_progress" | "running")
}

pub fn marker(active: bool) -> Span<'static> {
    if active {
        // Tag a reserved cell until the final draw pass so clipped/covered indicators don't tick.
        Span::styled(
            " ",
            crate::theme::Theme
                .accent()
                .add_modifier(Modifier::RAPID_BLINK),
        )
    } else {
        Span::raw(" ")
    }
}

pub fn finish(frame: &mut Frame, phase: usize, live: bool) -> bool {
    let mut visible = false;
    for cell in &mut frame.buffer_mut().content {
        if cell.modifier.contains(Modifier::RAPID_BLINK) {
            // The tag never reaches the terminal; animation is clocked by our dirty-frame scheduler.
            cell.modifier.remove(Modifier::RAPID_BLINK);
            cell.set_symbol(if live {
                visible = true;
                ["⠋", "⠙", "⠹", "⠸", "⠼", "⠴", "⠦", "⠧", "⠇", "⠏"][phase % 10]
            } else {
                " "
            });
        }
    }
    visible
}

#[cfg(test)]
mod tests {
    use super::*;
    use ratatui::{
        Terminal,
        backend::TestBackend,
        layout::Rect,
        text::Line,
        widgets::{Clear, Paragraph},
    };

    #[test]
    fn queued_terminal_and_unknown_statuses_do_not_claim_execution() {
        for status in [
            "pending",
            "queued",
            "requires_action",
            "completed",
            "failed",
            "cancelled",
            "",
            "future",
        ] {
            assert!(!executing(status));
        }
        assert!(executing("running"));
        assert!(executing("in_progress"));
    }

    #[test]
    fn covered_clipped_and_disconnected_indicators_do_not_request_animation_frames() {
        let mut terminal = Terminal::new(TestBackend::new(10, 2)).unwrap();
        for (covered, live, expected) in [
            (false, true, true),
            (true, true, false),
            (false, false, false),
        ] {
            terminal
                .draw(|frame| {
                    frame.render_widget(
                        Paragraph::new(Line::from(vec![marker(true)])),
                        Rect::new(0, 0, 1, 1),
                    );
                    if covered {
                        frame.render_widget(Clear, frame.area());
                    }
                    assert_eq!(finish(frame, 0, live), expected);
                    assert!(
                        frame
                            .buffer_mut()
                            .content
                            .iter()
                            .all(|cell| !cell.modifier.contains(Modifier::RAPID_BLINK))
                    );
                })
                .unwrap();
        }
        terminal
            .draw(|frame| {
                frame.render_widget(
                    Paragraph::new(Line::from(vec![marker(true)])).scroll((1, 0)),
                    frame.area(),
                );
                assert!(!finish(frame, 1, true));
            })
            .unwrap();
    }
}
