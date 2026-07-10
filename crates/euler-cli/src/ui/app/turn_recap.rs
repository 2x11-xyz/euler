//! Turn-end recap and exit-recap formatting (Warm Ledger §5.7 / §5.8).

use crate::ui::status::short_session_id;
use euler_event::{EventEnvelope, EventKind};
use std::collections::BTreeMap;

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct TurnRecap {
    pub file_count: usize,
    pub added: usize,
    pub removed: usize,
    pub paths: Vec<String>,
    pub test_status: Option<TestStatus>,
    pub ctx_percent: Option<u64>,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum TestStatus {
    Pass,
    Fail,
}

impl TestStatus {
    pub fn label(self) -> &'static str {
        match self {
            Self::Pass => "tests pass",
            Self::Fail => "tests failed",
        }
    }
}

impl TurnRecap {
    pub fn summary_line(&self) -> String {
        let mut parts = Vec::new();
        parts.push(match self.file_count {
            0 => "0 files".to_owned(),
            1 => "1 file".to_owned(),
            n => format!("{n} files"),
        });
        if self.file_count > 0 && (self.added > 0 || self.removed > 0) {
            parts.push(format!("+{} −{}", self.added, self.removed));
        }
        if let Some(status) = self.test_status {
            parts.push(status.label().to_owned());
        }
        match self.ctx_percent {
            Some(pct) => parts.push(format!("ctx {}%", pct.min(99))),
            None => parts.push("ctx ?%".to_owned()),
        }
        parts.join(" · ")
    }

    pub fn files_line(&self) -> Option<String> {
        if self.paths.is_empty() {
            None
        } else {
            Some(self.paths.join("  "))
        }
    }
}

pub fn turn_recap_from_events(
    events: &[EventEnvelope],
    start: usize,
    ctx_percent: Option<u64>,
) -> TurnRecap {
    let slice = events.get(start..).unwrap_or(&[]);
    let (paths, added, removed) = aggregate_turn_files(slice);
    let test_status = detect_test_status(slice);
    TurnRecap {
        file_count: paths.len(),
        added,
        removed,
        paths,
        test_status,
        ctx_percent,
    }
}

fn aggregate_turn_files(events: &[EventEnvelope]) -> (Vec<String>, usize, usize) {
    let mut latest: BTreeMap<String, (usize, usize)> = BTreeMap::new();
    for event in events {
        match event.kind.as_str() {
            EventKind::FILE_DIFF => {
                let path = payload_str(event, "path").unwrap_or("");
                if path.is_empty() {
                    continue;
                }
                let (added, removed) = event
                    .payload
                    .get("diff")
                    .and_then(|v| v.as_str())
                    .map(count_diff_lines)
                    .unwrap_or((0, 0));
                latest.insert(path.to_owned(), (added, removed));
            }
            EventKind::FILE_CHANGE => {
                let path = payload_str(event, "path").unwrap_or("");
                if path.is_empty() {
                    continue;
                }
                latest.entry(path.to_owned()).or_insert((0, 0));
            }
            _ => {}
        }
    }
    let mut added = 0usize;
    let mut removed = 0usize;
    let mut paths = Vec::with_capacity(latest.len());
    for (path, (a, r)) in latest {
        added += a;
        removed += r;
        paths.push(path);
    }
    (paths, added, removed)
}

fn count_diff_lines(diff: &str) -> (usize, usize) {
    let mut added = 0usize;
    let mut removed = 0usize;
    for line in diff.lines() {
        if line.starts_with('+') && !line.starts_with("+++") {
            added += 1;
        } else if line.starts_with('-') && !line.starts_with("---") {
            removed += 1;
        }
    }
    (added, removed)
}

pub fn detect_test_status(events: &[EventEnvelope]) -> Option<TestStatus> {
    let mut last: Option<TestStatus> = None;
    let mut call_commands = std::collections::HashMap::<String, String>::new();
    for event in events {
        match event.kind.as_str() {
            EventKind::TOOL_CALL => {
                if payload_str(event, "name") != Some("run_shell") {
                    continue;
                }
                let id = payload_str(event, "id").unwrap_or("").to_owned();
                if id.is_empty() {
                    continue;
                }
                if let Some(command) = event
                    .payload
                    .get("input")
                    .and_then(|v| v.get("command"))
                    .and_then(|v| v.as_str())
                {
                    call_commands.insert(id, command.to_owned());
                }
            }
            EventKind::TOOL_RESULT => {
                if payload_str(event, "name") != Some("run_shell") {
                    continue;
                }
                let id = payload_str(event, "id").unwrap_or("");
                let command = call_commands.get(id).map(String::as_str).unwrap_or("");
                let output = payload_str(event, "output").unwrap_or("");
                if !looks_test_like(command, output) {
                    continue;
                }
                if let Some(status) = parse_test_summary(output) {
                    last = Some(status);
                } else {
                    let ok = event
                        .payload
                        .get("ok")
                        .and_then(|v| v.as_bool())
                        .unwrap_or(false);
                    last = Some(if ok {
                        TestStatus::Pass
                    } else {
                        TestStatus::Fail
                    });
                }
            }
            _ => {}
        }
    }
    last
}

fn looks_test_like(command: &str, output: &str) -> bool {
    let cmd = command.to_ascii_lowercase();
    if cmd.contains("cargo test")
        || cmd.contains("cargo nextest")
        || cmd.contains("nextest run")
        || cmd.contains("pytest")
        || cmd.contains("python -m pytest")
    {
        return true;
    }
    output.lines().any(|line| {
        let lower = line.to_ascii_lowercase();
        lower.contains("test result:")
            || lower.contains("passed;")
            || (lower.contains("passed") && lower.contains("failed"))
            || (lower.contains("=====") && (lower.contains(" passed") || lower.contains(" failed")))
    })
}

pub fn parse_test_summary(output: &str) -> Option<TestStatus> {
    for line in output.lines().rev().take(40) {
        let trimmed = line.trim();
        if trimmed.is_empty() {
            continue;
        }
        let lower = trimmed.to_ascii_lowercase();
        if lower.contains("test result:") {
            if lower.contains("test result: ok") || lower.contains("test result:ok") {
                return Some(TestStatus::Pass);
            }
            if lower.contains("failed") {
                return Some(TestStatus::Fail);
            }
        }
        if lower.contains("passed") && lower.contains("failed") {
            if let Some(status) = classify_passed_failed_line(&lower) {
                return Some(status);
            }
        }
        if lower.contains("=====") || lower.contains("passed in") || lower.contains("failed in") {
            if lower.contains("failed") && !lower.contains("0 failed") {
                return Some(TestStatus::Fail);
            }
            if lower.contains("passed") && !lower.contains("failed") {
                return Some(TestStatus::Pass);
            }
            if lower.contains("failed") {
                return Some(TestStatus::Fail);
            }
        }
    }
    None
}

fn classify_passed_failed_line(lower: &str) -> Option<TestStatus> {
    let failed = extract_count_before(lower, "failed").unwrap_or(0);
    let passed = extract_count_before(lower, "passed");
    if passed.is_none() && failed == 0 {
        return None;
    }
    if failed > 0 {
        Some(TestStatus::Fail)
    } else {
        Some(TestStatus::Pass)
    }
}

fn extract_count_before(haystack: &str, word: &str) -> Option<u64> {
    let mut search = haystack;
    while let Some(idx) = search.find(word) {
        let before = &search[..idx];
        let token = before
            .trim_end()
            .rsplit(|c: char| !c.is_ascii_digit())
            .next()
            .unwrap_or("");
        if !token.is_empty() {
            if let Ok(n) = token.parse::<u64>() {
                return Some(n);
            }
        }
        search = &search[idx + word.len()..];
    }
    None
}

fn payload_str<'a>(event: &'a EventEnvelope, key: &str) -> Option<&'a str> {
    event.payload.get(key).and_then(|v| v.as_str())
}

pub fn ctx_percent(input_tokens: u64, window: Option<u64>) -> Option<u64> {
    let window = window.filter(|w| *w > 0)?;
    let numerator = u128::from(input_tokens)
        .saturating_mul(100)
        .saturating_add(u128::from(window / 2));
    let percent = numerator / u128::from(window);
    Some(u64::try_from(percent).unwrap_or(u64::MAX).min(99))
}

pub fn session_files_changed_count(events: &[EventEnvelope]) -> usize {
    let mut paths = std::collections::BTreeSet::new();
    for event in events {
        match event.kind.as_str() {
            EventKind::FILE_DIFF | EventKind::FILE_CHANGE => {
                if let Some(path) = payload_str(event, "path").filter(|p| !p.is_empty()) {
                    paths.insert(path.to_owned());
                }
            }
            _ => {}
        }
    }
    paths.len()
}

pub fn exit_recap_lines(
    session_id: &str,
    event_count: usize,
    files_changed: usize,
) -> Vec<ExitRecapLine> {
    let full_id = if session_id.is_empty() {
        "e????"
    } else {
        session_id
    };
    let short_id = short_session_id(full_id);
    vec![
        ExitRecapLine::Normal(format!(
            "session {short_id} saved · {event_count} events · {files_changed} files changed"
        )),
        // The resume command must keep the full ULID so it actually works
        // when copy-pasted; only the headline above uses the short form.
        ExitRecapLine::Normal(format!("resume  euler --resume {full_id}")),
        ExitRecapLine::Faint("export  euler extension run session-export …".to_owned()),
    ]
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum ExitRecapLine {
    Normal(String),
    Faint(String),
}

impl ExitRecapLine {
    pub fn text(&self) -> &str {
        match self {
            Self::Normal(s) | Self::Faint(s) => s,
        }
    }

    pub fn is_faint(&self) -> bool {
        matches!(self, Self::Faint(_))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use euler_event::object;
    use serde_json::json;

    fn event(kind: &'static str, payload: euler_event::JsonObject) -> EventEnvelope {
        EventEnvelope::new("session", "agent", None, kind, payload)
    }

    #[test]
    fn recap_formats_files_diffstat_tests_and_ctx() {
        let events = vec![
            event(
                EventKind::FILE_DIFF,
                object([
                    ("path", "src/a.rs".into()),
                    ("diff", "--- a\n+++ b\n@@\n-line\n+line2\n+line3\n".into()),
                ]),
            ),
            event(
                EventKind::TOOL_CALL,
                object([
                    ("id", "c1".into()),
                    ("name", "run_shell".into()),
                    ("input", json!({"command": "cargo test -q"})),
                ]),
            ),
            event(
                EventKind::TOOL_RESULT,
                object([
                    ("id", "c1".into()),
                    ("name", "run_shell".into()),
                    ("ok", true.into()),
                    (
                        "output",
                        "test result: ok. 3 passed; 0 failed; 0 ignored".into(),
                    ),
                ]),
            ),
        ];
        let recap = turn_recap_from_events(&events, 0, Some(12));
        assert_eq!(
            recap.summary_line(),
            "1 file · +2 −1 · tests pass · ctx 12%"
        );
        assert_eq!(recap.files_line().as_deref(), Some("src/a.rs"));
    }

    #[test]
    fn parse_cargo_nextest_pytest_summaries() {
        assert_eq!(
            parse_test_summary("test result: ok. 12 passed; 0 failed"),
            Some(TestStatus::Pass)
        );
        assert_eq!(
            parse_test_summary("test result: FAILED. 10 passed; 2 failed"),
            Some(TestStatus::Fail)
        );
        assert_eq!(
            parse_test_summary("Summary [ 1.2s ] 12 tests run: 10 passed, 2 failed"),
            Some(TestStatus::Fail)
        );
        assert_eq!(
            parse_test_summary("===== 12 passed in 0.12s ====="),
            Some(TestStatus::Pass)
        );
        assert_eq!(parse_test_summary("hello world"), None);
    }

    #[test]
    fn exit_recap_is_at_most_five_lines_and_copy_ready() {
        let lines = exit_recap_lines("e0147", 42, 3);
        assert!(lines.len() <= 5);
        assert!(lines[1].text().contains("euler --resume e0147"));
        assert!(lines[2].is_faint());
    }

    #[test]
    fn exit_recap_shortens_saved_headline_but_keeps_full_id_in_resume_command() {
        let lines = exit_recap_lines("01KX488KQ6DXYPYGB0FK7GFD4T", 42, 3);
        assert!(
            lines[0].text().contains("session efd4t saved"),
            "headline should use the short display id: {:?}",
            lines[0]
        );
        assert!(
            lines[1]
                .text()
                .contains("euler --resume 01KX488KQ6DXYPYGB0FK7GFD4T"),
            "resume command must stay copy-ready with the full ULID: {:?}",
            lines[1]
        );
    }

    #[test]
    fn session_files_changed_dedupes_paths() {
        let events = vec![
            event(EventKind::FILE_CHANGE, object([("path", "a.rs".into())])),
            event(
                EventKind::FILE_DIFF,
                object([("path", "a.rs".into()), ("diff", "+x\n".into())]),
            ),
            event(EventKind::FILE_CHANGE, object([("path", "b.rs".into())])),
        ];
        assert_eq!(session_files_changed_count(&events), 2);
    }
}
