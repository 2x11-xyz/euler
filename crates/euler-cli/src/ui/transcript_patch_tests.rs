use super::{
    patch_diff::{self, PatchDisplay},
    test_backend::VT100Backend,
    text::display_width,
    theme::Theme,
    transcript::{project_events, transcript_widget, TranscriptItem},
};
use euler_event::{object, EventEnvelope, EventKind};
use ratatui::{layout::Rect, text::Line, Terminal};

#[test]
fn projects_patch_events_with_current_payload_fields() {
    let events = vec![event(
        EventKind::PATCH_PROPOSED,
        object([
            ("path", "src/lib.rs".into()),
            ("old", "fn old() {}\n".into()),
            ("new", "fn new() {}\n".into()),
        ]),
    )];

    assert_eq!(
        project_events(&events),
        vec![TranscriptItem::PatchProposed {
            path: "src/lib.rs".to_owned(),
            old: Some("fn old() {}\n".to_owned()),
            new: Some("fn new() {}\n".to_owned()),
        }]
    );
}

#[test]
fn projects_path_only_patch_event_as_unknown_without_fake_diff() {
    let events = vec![event(
        EventKind::PATCH_PROPOSED,
        object([("path", "src/lib.rs".into())]),
    )];

    assert_eq!(
        project_events(&events),
        vec![TranscriptItem::PatchProposed {
            path: "src/lib.rs".to_owned(),
            old: None,
            new: None,
        }]
    );
}

#[test]
fn vt100_renders_patch_diff_with_line_numbers_and_bounded_preview() {
    let old = "a\nb\nc\n".to_owned();
    let new = format!(
        "a\nbeta\nc\n{}",
        (1..=14)
            .map(|index| format!("extra {index}\n"))
            .collect::<String>()
    );
    let events = vec![event(
        EventKind::PATCH_APPLIED,
        object([
            ("path", "src/lib.rs".into()),
            ("old", old.into()),
            ("new", new.into()),
        ]),
    )];
    let theme = Theme::default();

    let contents = rendered_screen(&events, &theme, 80, 32);

    assert!(contents.contains("Edited src/lib.rs"));
    // §4.1: no git `@@ … @@` fences; this diff resolves no symbol, so the
    // header row is omitted and the body follows straight after the file row.
    assert!(!contents.contains("@@"), "no hunk fences");
    assert!(contents.contains("     2 - b"));
    assert!(contents.contains("     2 + beta"));
    assert!(!contents.contains("bounded patch"));
    assert!(contents.contains("ctrl+o expand"));
    assert!(!contents.contains("extra 14"));
    assert!(contents.contains(""));
}

#[test]
fn vt100_renders_patch_diff_width_bounded_when_narrow() {
    let events = vec![event(
        EventKind::PATCH_APPLIED,
        object([
            ("path", "src/lib.rs".into()),
            ("old", "fn old_name() {}\n".into()),
            ("new", "pub fn new_name() {}\n".into()),
        ]),
    )];
    let theme = Theme::default();

    let contents = rendered_screen(&events, &theme, 36, 10);

    assert!(contents.contains("     1 - fn old_"));
    assert!(contents.contains("     1 + pub fn"));
    for row in contents.lines() {
        assert!(
            display_width(row) <= 36,
            "row overflowed narrow patch artifact: {row:?}"
        );
    }
}

#[test]
fn patch_diff_contract_keeps_summary_row_before_detail_rows() {
    let theme = Theme::default();
    let rows = patch_diff::render_patch(
        PatchDisplay {
            label: "Patch proposed",
            path: "src/lib.rs",
            old: Some("a\n"),
            new: Some("b\n"),
        },
        &theme,
        80,
        5,
    );
    let texts = line_texts(&rows);

    assert!(
        texts
            .first()
            .is_some_and(|row| row.contains("Patch proposed") && row.contains("src/lib.rs")),
        "texts: {texts:?}"
    );
    assert!(
        texts.iter().skip(1).any(|row| row.contains("   1 - a")),
        "detail rows missing after summary: {texts:?}"
    );
}

fn rendered_screen(events: &[EventEnvelope], theme: &Theme, width: u16, height: u16) -> String {
    let backend = VT100Backend::new(width, height);
    let mut terminal = Terminal::new(backend).expect("terminal");

    terminal
        .draw(|frame| {
            frame.render_widget(
                transcript_widget(events, theme),
                Rect::new(0, 0, width, height),
            );
        })
        .expect("draw");

    terminal.backend().screen_contents()
}

fn line_texts(lines: &[Line<'_>]) -> Vec<String> {
    lines
        .iter()
        .map(|line| {
            line.spans
                .iter()
                .map(|span| span.content.as_ref())
                .collect()
        })
        .collect()
}

fn event(kind: &'static str, payload: euler_event::JsonObject) -> EventEnvelope {
    EventEnvelope::new("session", "agent", None, kind, payload)
}
