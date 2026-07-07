//! Env-gated render/resize metrics for TUI dogfood harnesses.
//!
//! When `EULER_TUI_METRICS` names a writable path, each recorded metric
//! appends one JSONL row: `{"ts_ms":…,"metric":"…","total":…}`. Harnesses
//! segment bursts by timestamp and assert event/replay/paint ratios that
//! terminal-side observation cannot attribute. Disabled (single atomic load)
//! when the variable is unset.

use std::fs::{File, OpenOptions};
use std::io::Write;
use std::sync::{Mutex, OnceLock};
use std::time::Instant;

pub const METRICS_ENV: &str = "EULER_TUI_METRICS";

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum Metric {
    ResizeEvent,
    ResizeAction,
    HistoryReplay,
    ScrollbackPurge,
    RenderFrame,
    TerminalFlush,
}

impl Metric {
    const ALL: usize = 6;

    fn index(self) -> usize {
        match self {
            Self::ResizeEvent => 0,
            Self::ResizeAction => 1,
            Self::HistoryReplay => 2,
            Self::ScrollbackPurge => 3,
            Self::RenderFrame => 4,
            Self::TerminalFlush => 5,
        }
    }

    fn name(self) -> &'static str {
        match self {
            Self::ResizeEvent => "resize_event",
            Self::ResizeAction => "resize_action",
            Self::HistoryReplay => "history_replay",
            Self::ScrollbackPurge => "scrollback_purge",
            Self::RenderFrame => "render_frame",
            Self::TerminalFlush => "terminal_flush",
        }
    }
}

struct Sink {
    file: File,
    started: Instant,
    totals: [u64; Metric::ALL],
}

fn sink() -> Option<&'static Mutex<Sink>> {
    static SINK: OnceLock<Option<Mutex<Sink>>> = OnceLock::new();
    SINK.get_or_init(|| {
        let path = std::env::var_os(METRICS_ENV)?;
        let file = OpenOptions::new()
            .create(true)
            .append(true)
            .open(path)
            .ok()?;
        Some(Mutex::new(Sink {
            file,
            started: Instant::now(),
            totals: [0; Metric::ALL],
        }))
    })
    .as_ref()
}

/// Record one occurrence of `metric`. No-op unless `EULER_TUI_METRICS` is set.
pub fn record(metric: Metric) {
    let Some(sink) = sink() else {
        return;
    };
    let Ok(mut sink) = sink.lock() else {
        return;
    };
    sink.totals[metric.index()] += 1;
    let row = format!(
        "{{\"ts_ms\":{:.3},\"metric\":\"{}\",\"total\":{}}}\n",
        sink.started.elapsed().as_secs_f64() * 1000.0,
        metric.name(),
        sink.totals[metric.index()],
    );
    let _ = sink.file.write_all(row.as_bytes());
}
