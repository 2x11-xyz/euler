//! Pre-session bordered consent cards (ADR 0017 phase 3).
//!
//! The acknowledgment card and the relocation-consent card are presented
//! before the session is constructed, because the decision determines the
//! immutable bootstrap the session records at `session.start`. They cannot be
//! ordinary in-transcript modals for that reason (those run inside a live
//! session), so this module renders the same bordered treatment on a
//! self-contained inline viewport, reads a single keypress, and returns the
//! decision. The border and row rendering are reused from the transcript's
//! permission panel; only the surrounding one-shot render loop is local.

use crate::ui::terminal::TerminalSession;
use crate::ui::theme::{ColorLevel, Theme, ThemeChoice};
use anyhow::Result;
use crossterm::event::{self, Event, KeyCode, KeyEventKind, KeyModifiers};
use ratatui::backend::CrosstermBackend;
use ratatui::text::{Line, Text};
use ratatui::widgets::Paragraph;
use ratatui::{Terminal, TerminalOptions, Viewport};
use std::io;

/// The user's answer to a bordered consent card.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum ConsentChoice {
    Accept,
    Decline,
}

/// Render a bordered card on an inline viewport and block for one decision.
///
/// `build` produces the card lines for the current selection (the accept
/// option highlighted or not) at the given width. `accept_key` is the
/// single affirmative hotkey (`y` for acknowledgment, `r` for relocation).
/// Decline is `n`, `Esc`, or `Ctrl-C`; `Up`/`Down` move the highlight and
/// `Enter` commits it. The default highlight is Decline (the safe bias), which
/// the caller reflects in its first `build` call.
fn prompt<F>(build: F, accept_key: char, theme_choice: ThemeChoice) -> Result<ConsentChoice>
where
    F: Fn(bool, u16, &Theme) -> Vec<Line<'static>>,
{
    let theme = Theme::for_choice_with_color_level(theme_choice, ColorLevel::detect());
    let _session = TerminalSession::enter()?;
    let width = crossterm::terminal::size()
        .map(|(cols, _)| cols)
        .unwrap_or(80);
    let mut accept_selected = false;
    let height =
        u16::try_from(build(accept_selected, width, &theme).len().max(1)).unwrap_or(u16::MAX);
    let mut terminal = Terminal::with_options(
        CrosstermBackend::new(io::stdout()),
        TerminalOptions {
            viewport: Viewport::Inline(height),
        },
    )?;
    let choice = loop {
        let lines = build(accept_selected, width, &theme);
        terminal.draw(|frame| {
            frame.render_widget(Paragraph::new(Text::from(lines.clone())), frame.area());
        })?;
        let Event::Key(key) = event::read()? else {
            continue;
        };
        if key.kind == KeyEventKind::Release {
            continue;
        }
        match key.code {
            KeyCode::Char('c') if key.modifiers.contains(KeyModifiers::CONTROL) => {
                break ConsentChoice::Decline;
            }
            KeyCode::Char(c) if c.eq_ignore_ascii_case(&accept_key) => break ConsentChoice::Accept,
            KeyCode::Char('n') | KeyCode::Char('N') | KeyCode::Esc => break ConsentChoice::Decline,
            KeyCode::Up | KeyCode::Down | KeyCode::Tab => accept_selected = !accept_selected,
            KeyCode::Enter => {
                break if accept_selected {
                    ConsentChoice::Accept
                } else {
                    ConsentChoice::Decline
                }
            }
            _ => {}
        }
    };
    // Leave a clean region for the app that follows.
    terminal.clear()?;
    Ok(choice)
}

/// Present the acknowledgment card. `content_changed` selects the changed
/// headline; `sources` and `skipped_count` populate the file list.
pub(crate) fn prompt_acknowledgment(
    folder_label: &str,
    content_changed: bool,
    sources: &[String],
    skipped_count: usize,
    theme_choice: ThemeChoice,
) -> Result<ConsentChoice> {
    prompt(
        |load_selected, width, theme| {
            crate::ui::transcript::render_acknowledgment_card(
                &crate::ui::transcript::AcknowledgmentCardView {
                    folder_label,
                    content_changed,
                    sources,
                    skipped_count,
                    load_selected,
                },
                theme,
                width,
            )
        },
        'y',
        theme_choice,
    )
}

/// Present the relocation-consent card.
pub(crate) fn prompt_relocation(
    recorded_folder: &str,
    current_folder: &str,
    last_active: &str,
    theme_choice: ThemeChoice,
) -> Result<ConsentChoice> {
    prompt(
        |resume_selected, width, theme| {
            crate::ui::transcript::render_relocation_card(
                &crate::ui::transcript::RelocationCardView {
                    recorded_folder,
                    current_folder,
                    last_active,
                    resume_selected,
                },
                theme,
                width,
            )
        },
        'r',
        theme_choice,
    )
}
