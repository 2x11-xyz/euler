use anyhow::{anyhow, Result};
use chrono::{SecondsFormat, Utc};
use serde_json::{Map, Number, Value};
use std::fmt;
use std::fs::{File, OpenOptions};
use std::io::{BufWriter, Write};
use std::path::Path;
use std::sync::{Arc, Mutex, MutexGuard, OnceLock};
use tracing::field::{Field, Visit};
use tracing::Subscriber;
use tracing_subscriber::layer::{Context, SubscriberExt};
use tracing_subscriber::{Layer, Registry};

const CORE_DIAGNOSTICS_TARGET: &str = "euler_core::diagnostics";
const DIAGNOSTICS_FILE: &str = "diagnostics.jsonl";

#[derive(Clone)]
struct DiagnosticsSink {
    inner: Arc<Mutex<SinkState>>,
}

struct SinkState {
    writer: Option<BufWriter<File>>,
    warned_write_failure: bool,
    subscriber_unavailable: bool,
}

impl DiagnosticsSink {
    fn new() -> Self {
        Self {
            inner: Arc::new(Mutex::new(SinkState {
                writer: None,
                warned_write_failure: false,
                subscriber_unavailable: false,
            })),
        }
    }

    fn mark_subscriber_unavailable(&self) {
        recover_mutex(&self.inner).subscriber_unavailable = true;
    }

    fn bind(&self, file: File) -> Result<()> {
        let mut state = recover_mutex(&self.inner);
        if state.subscriber_unavailable {
            return Err(anyhow!(
                "diagnostics subscriber unavailable (another global subscriber is installed); diagnostics disabled for this process"
            ));
        }
        if state.writer.is_some() {
            return Err(anyhow!(
                "diagnostics already bound for this process; keeping the first session sink"
            ));
        }
        state.writer = Some(BufWriter::new(file));
        Ok(())
    }
}

struct DiagnosticsLayer {
    sink: DiagnosticsSink,
}

impl<S> Layer<S> for DiagnosticsLayer
where
    S: Subscriber,
{
    fn on_event(&self, event: &tracing::Event<'_>, _ctx: Context<'_, S>) {
        if event.metadata().target() != CORE_DIAGNOSTICS_TARGET {
            return;
        }
        let mut fields = EventFields::default();
        event.record(&mut fields);
        let Some(event_name) = fields.remove_string("event") else {
            return;
        };
        let Some(session_id) = fields.remove_string("session_id") else {
            return;
        };

        let mut object = Map::new();
        object.insert(
            "ts".to_owned(),
            Utc::now()
                .to_rfc3339_opts(SecondsFormat::Millis, true)
                .into(),
        );
        object.insert("level".to_owned(), event.metadata().level().as_str().into());
        object.insert("target".to_owned(), event.metadata().target().into());
        object.insert("session_id".to_owned(), session_id.into());
        object.insert("event".to_owned(), event_name.into());
        object.extend(fields.values);
        self.write_line(Value::Object(object));
    }
}

impl DiagnosticsLayer {
    fn write_line(&self, value: Value) {
        let Ok(mut line) = serde_json::to_string(&value) else {
            return;
        };
        line.push('\n');
        let mut state = recover_mutex(&self.sink.inner);
        let Some(writer) = state.writer.as_mut() else {
            return;
        };
        if writer.write_all(line.as_bytes()).is_err() || writer.flush().is_err() {
            state.writer = None;
            if !state.warned_write_failure {
                state.warned_write_failure = true;
                eprintln!("warning: diagnostics logging disabled after write failure");
            }
        }
    }
}

#[derive(Default)]
struct EventFields {
    values: Map<String, Value>,
}

impl EventFields {
    fn remove_string(&mut self, key: &str) -> Option<String> {
        self.values.remove(key)?.as_str().map(str::to_owned)
    }
}

impl Visit for EventFields {
    fn record_bool(&mut self, field: &Field, value: bool) {
        self.values.insert(field.name().to_owned(), value.into());
    }

    fn record_i64(&mut self, field: &Field, value: i64) {
        self.values.insert(field.name().to_owned(), value.into());
    }

    fn record_u64(&mut self, field: &Field, value: u64) {
        self.values.insert(field.name().to_owned(), value.into());
    }

    fn record_str(&mut self, field: &Field, value: &str) {
        self.values
            .insert(field.name().to_owned(), value.to_owned().into());
    }

    fn record_f64(&mut self, field: &Field, value: f64) {
        if let Some(number) = Number::from_f64(value) {
            self.values
                .insert(field.name().to_owned(), Value::Number(number));
        }
    }

    fn record_debug(&mut self, _field: &Field, _value: &dyn fmt::Debug) {}
}

static SINK: OnceLock<DiagnosticsSink> = OnceLock::new();

pub(crate) fn bind_session_dir(session_dir: &Path) {
    let sink = SINK.get_or_init(install_subscriber).clone();
    let path = session_dir.join(DIAGNOSTICS_FILE);
    let file = match OpenOptions::new().create(true).append(true).open(&path) {
        Ok(file) => file,
        Err(error) => {
            eprintln!(
                "warning: diagnostics logging disabled for {}: {error}",
                path.display()
            );
            return;
        }
    };
    if let Err(error) = sink.bind(file) {
        eprintln!("warning: {error}");
    }
}

fn install_subscriber() -> DiagnosticsSink {
    let sink = DiagnosticsSink::new();
    let layer = DiagnosticsLayer { sink: sink.clone() };
    // Hard constraint: exactly one session diagnostics sink may be
    // bound per process. The layer stays installed, while the writer is swapped
    // from None to the first successfully opened session file.
    if tracing::subscriber::set_global_default(Registry::default().with(layer)).is_err() {
        // Without our layer attached, a bound writer would never receive
        // events; mark the sink so bind() reports the truth instead of
        // silently pretending diagnostics are active.
        sink.mark_subscriber_unavailable();
        eprintln!("warning: diagnostics subscriber already installed; diagnostics disabled");
    }
    sink
}

fn recover_mutex<T>(mutex: &Mutex<T>) -> MutexGuard<'_, T> {
    mutex
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn visitor_keeps_scalar_fields() {
        let mut fields = EventFields::default();
        fields.values.insert("event".to_owned(), "turn_end".into());
        fields.values.insert("rounds".to_owned(), 2_u64.into());

        assert_eq!(fields.remove_string("event").as_deref(), Some("turn_end"));
        assert_eq!(fields.values.get("rounds"), Some(&Value::from(2_u64)));
    }

    #[test]
    fn second_bind_is_rejected_and_first_sink_stays_bound() {
        let temp = tempfile::tempdir().expect("temp dir");
        let first = OpenOptions::new()
            .create(true)
            .append(true)
            .open(temp.path().join("first.jsonl"))
            .expect("first file");
        let second = OpenOptions::new()
            .create(true)
            .append(true)
            .open(temp.path().join("second.jsonl"))
            .expect("second file");
        let sink = DiagnosticsSink::new();
        sink.bind(first).expect("first bind");
        assert!(sink.bind(second).is_err());

        let layer = DiagnosticsLayer { sink };
        layer.write_line(serde_json::json!({"event": "turn_end"}));

        let first_content = std::fs::read_to_string(temp.path().join("first.jsonl"))
            .expect("read first diagnostics");
        let second_content = std::fs::read_to_string(temp.path().join("second.jsonl"))
            .expect("read second diagnostics");
        assert!(first_content.contains("turn_end"));
        assert!(second_content.is_empty());
    }
}
