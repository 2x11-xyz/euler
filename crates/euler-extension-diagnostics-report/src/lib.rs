use euler_sdk::{
    ArgSpec, ArgValueKind, ArtifactRecord, ArtifactWrite, Capability, CommandContext,
    CommandDescriptor, CommandRegistrar, DiagnosticsQuery, Extension, ExtensionCommand,
    ExtensionError, ExtensionManifest, HostApi,
};
use serde_json::{json, Map, Value};
use std::collections::BTreeMap;

const EXTENSION_ID: &str = "diagnostics-report";
const DISPLAY_NAME: &str = "Diagnostics Report";
const COMMAND_NAME: &str = "report";
const VERSION: &str = env!("CARGO_PKG_VERSION");
const SCHEMA: &str = "euler.diagnostics.report.v1";
const MEDIA_TYPE: &str = "application/vnd.euler.diagnostics-report.v1+json";
const DEFAULT_TAIL_LINES: usize = 2048;
const REPORT_MAX_BYTES: usize = 1024 * 1024;

#[derive(Clone, Copy, Debug, Default)]
pub struct DiagnosticsReportExtension;

impl Extension for DiagnosticsReportExtension {
    fn manifest(&self) -> ExtensionManifest {
        ExtensionManifest {
            id: EXTENSION_ID.to_owned(),
            version: VERSION.to_owned(),
            display_name: DISPLAY_NAME.to_owned(),
            capabilities: vec![Capability::DiagnosticsRead, Capability::ArtifactWrite],
        }
    }

    fn register(&self, registrar: &mut dyn CommandRegistrar) -> Result<(), ExtensionError> {
        registrar.register_command(COMMAND_NAME, Box::new(ReportCommand));
        Ok(())
    }
}

#[derive(Clone, Copy, Debug)]
struct ReportCommand;

impl ExtensionCommand for ReportCommand {
    fn descriptor(&self) -> CommandDescriptor {
        CommandDescriptor {
            name: COMMAND_NAME.to_owned(),
            display_name: "Write diagnostics report".to_owned(),
            summary: "Aggregate the current session diagnostics log into a report artifact."
                .to_owned(),
            required_capabilities: vec![Capability::DiagnosticsRead, Capability::ArtifactWrite],
            args: vec![ArgSpec {
                flag: "tail-lines".to_owned(),
                input_key: "tail_lines".to_owned(),
                value_kind: ArgValueKind::PositiveInt { max: None },
                required: false,
                repeatable: false,
            }],
            accepts_session_id: false,
        }
    }

    fn execute(
        &self,
        context: CommandContext,
        host: &dyn HostApi,
    ) -> Result<Value, ExtensionError> {
        let input = ReportInput::parse(&context.input)?;
        let page = host.read_diagnostics(DiagnosticsQuery {
            tail_lines: input.tail_lines,
            max_bytes: REPORT_MAX_BYTES,
        })?;
        if page.lines.is_empty() {
            return Err(input_error("no diagnostics available for this session"));
        }
        let report = Report::from_lines(&page.lines, page.truncated).to_json();
        let bytes = serde_json::to_vec(&report)
            .map_err(|error| ExtensionError::ArtifactWriteFailed(error.to_string()))?;
        let record = host.write_artifact(ArtifactWrite {
            display_name: DISPLAY_NAME.to_owned(),
            media_type: MEDIA_TYPE.to_owned(),
            bytes,
            source_event_ids: Vec::new(),
            metadata: metadata(&report),
        })?;
        Ok(output(record, &report))
    }
}

#[derive(Debug, Eq, PartialEq)]
struct ReportInput {
    tail_lines: usize,
}

impl ReportInput {
    fn parse(value: &Value) -> Result<Self, ExtensionError> {
        if value.is_null() {
            return Ok(Self::default());
        }
        let object = value
            .as_object()
            .ok_or_else(|| input_error("diagnostics-report input must be a JSON object"))?;
        reject_unknown_fields(object)?;
        Ok(Self {
            tail_lines: parse_tail_lines(object)?,
        })
    }
}

impl Default for ReportInput {
    fn default() -> Self {
        Self {
            tail_lines: DEFAULT_TAIL_LINES,
        }
    }
}

#[derive(Debug, Default, Eq, PartialEq)]
struct Report {
    lines_scanned: usize,
    malformed_lines: usize,
    truncated: bool,
    event_counts: BTreeMap<String, usize>,
    duration_ms: BTreeMap<String, DurationStats>,
    ok_false_counts: BTreeMap<String, usize>,
    permission_allowed: usize,
    permission_denied: usize,
}

impl Report {
    fn from_lines(lines: &[String], truncated: bool) -> Self {
        let mut report = Self {
            lines_scanned: lines.len(),
            truncated,
            ..Self::default()
        };
        let mut durations: BTreeMap<String, Vec<u64>> = BTreeMap::new();
        for line in lines {
            let Some(object) = parse_line(line) else {
                report.malformed_lines += 1;
                continue;
            };
            let Some(event) = object.get("event").and_then(Value::as_str) else {
                report.malformed_lines += 1;
                continue;
            };
            *report.event_counts.entry(event.to_owned()).or_default() += 1;
            if duration_event(event) {
                if let Some(duration) = object.get("duration_ms").and_then(Value::as_u64) {
                    durations
                        .entry(event.to_owned())
                        .or_default()
                        .push(duration);
                }
            }
            if object.get("ok").and_then(Value::as_bool) == Some(false) {
                *report.ok_false_counts.entry(event.to_owned()).or_default() += 1;
            }
            if event == "permission_decision" {
                match object.get("allowed").and_then(Value::as_bool) {
                    Some(true) => report.permission_allowed += 1,
                    Some(false) => report.permission_denied += 1,
                    None => {}
                }
            }
        }
        report.duration_ms = durations
            .into_iter()
            .map(|(event, values)| (event, DurationStats::from_values(values)))
            .collect();
        report
    }

    fn to_json(&self) -> Value {
        json!({
            "schema": SCHEMA,
            "lines_scanned": self.lines_scanned,
            "malformed_lines": self.malformed_lines,
            "truncated": self.truncated,
            "turn_count": self.event_counts.get("turn_start").copied().unwrap_or(0),
            "event_counts": self.event_counts,
            "duration_ms": self.duration_ms,
            "ok_false_counts": self.ok_false_counts,
            "permission_decisions": {
                "allowed": self.permission_allowed,
                "denied": self.permission_denied,
            },
        })
    }
}

#[derive(Debug, Eq, PartialEq, serde::Serialize)]
struct DurationStats {
    count: usize,
    max: u64,
    p50: u64,
}

impl DurationStats {
    fn from_values(mut values: Vec<u64>) -> Self {
        values.sort_unstable();
        let count = values.len();
        let p50_index = count.div_ceil(2).saturating_sub(1);
        Self {
            count,
            max: values[count - 1],
            p50: values[p50_index],
        }
    }
}

fn parse_line(line: &str) -> Option<Map<String, Value>> {
    serde_json::from_str::<Value>(line)
        .ok()?
        .as_object()
        .cloned()
}

fn duration_event(event: &str) -> bool {
    matches!(
        event,
        "model_call_end" | "tool_exec_end" | "extension_command_end" | "provenance_append_end"
    )
}

fn reject_unknown_fields(object: &Map<String, Value>) -> Result<(), ExtensionError> {
    for key in object.keys() {
        if key != "tail_lines" {
            return Err(input_error(format!("unknown input field `{key}`")));
        }
    }
    Ok(())
}

fn parse_tail_lines(object: &Map<String, Value>) -> Result<usize, ExtensionError> {
    let Some(value) = object.get("tail_lines") else {
        return Ok(DEFAULT_TAIL_LINES);
    };
    if value.is_null() {
        return Ok(DEFAULT_TAIL_LINES);
    }
    let Some(parsed) = value.as_u64() else {
        return Err(input_error("tail_lines must be a positive integer"));
    };
    let parsed = usize::try_from(parsed).map_err(|_| input_error("tail_lines is too large"))?;
    if parsed == 0 {
        return Err(input_error("tail_lines must be greater than zero"));
    }
    Ok(parsed)
}

fn metadata(report: &Value) -> Map<String, Value> {
    Map::from_iter([
        ("schema".to_owned(), Value::String(SCHEMA.to_owned())),
        ("lines_scanned".to_owned(), report["lines_scanned"].clone()),
        ("truncated".to_owned(), report["truncated"].clone()),
    ])
}

fn output(record: ArtifactRecord, report: &Value) -> Value {
    json!({
        "persisted_event_id": record.persisted_event_id,
        "relative_path": record.relative_path,
        "sha256": record.sha256,
        "byte_len": record.byte_len,
        "lines_scanned": report["lines_scanned"],
        "malformed_lines": report["malformed_lines"],
        "truncated": report["truncated"],
        "turn_count": report["turn_count"],
    })
}

fn input_error(message: impl Into<String>) -> ExtensionError {
    ExtensionError::Message(message.into())
}

#[cfg(test)]
mod tests {
    use super::*;
    use euler_sdk::{DiagnosticsPage, ProvenancePage, ProvenanceQuery};
    use std::cell::RefCell;

    #[test]
    fn aggregates_synthetic_diagnostics_lines() {
        let lines = vec![
            line(json!({"event":"turn_start"})),
            line(json!({"event":"model_call_end","duration_ms":30,"ok":true,"model":"fixture"})),
            line(
                json!({"event":"model_call_end","duration_ms":10,"ok":false,"provider":"fixture"}),
            ),
            line(json!({"event":"tool_exec_end","duration_ms":8,"ok":false,"tool":"read_file"})),
            line(json!({"event":"permission_decision","allowed":true,"capability":"fs-read"})),
            line(json!({"event":"permission_decision","allowed":false,"capability":"fs-write"})),
            "not json".to_owned(),
        ];

        let report = Report::from_lines(&lines, true).to_json();

        assert_eq!(report["schema"], SCHEMA);
        assert_eq!(report["lines_scanned"], json!(7));
        assert_eq!(report["malformed_lines"], json!(1));
        assert_eq!(report["turn_count"], json!(1));
        assert_eq!(report["event_counts"]["model_call_end"], json!(2));
        assert_eq!(report["duration_ms"]["model_call_end"]["count"], json!(2));
        assert_eq!(report["duration_ms"]["model_call_end"]["max"], json!(30));
        assert_eq!(report["duration_ms"]["model_call_end"]["p50"], json!(10));
        assert_eq!(report["ok_false_counts"]["model_call_end"], json!(1));
        assert_eq!(report["ok_false_counts"]["tool_exec_end"], json!(1));
        assert_eq!(report["permission_decisions"]["allowed"], json!(1));
        assert_eq!(report["permission_decisions"]["denied"], json!(1));
        assert!(!serde_json::to_string(&report)
            .expect("report json")
            .contains("read_file"));
    }

    #[test]
    fn zero_lines_error_writes_no_artifact() {
        let host = MockHost::new(Vec::new(), false);
        let error = ReportCommand
            .execute(CommandContext { input: json!(null) }, &host)
            .expect_err("empty diagnostics fail");

        assert_eq!(
            error,
            input_error("no diagnostics available for this session")
        );
        assert!(host.writes.borrow().is_empty());
    }

    #[test]
    fn unknown_input_field_is_rejected() {
        let error = ReportInput::parse(&json!({"tail_lines": 1, "path": "nope"}))
            .expect_err("unknown field");

        assert_eq!(error, input_error("unknown input field `path`"));
    }

    #[test]
    fn descriptor_declares_both_capabilities() {
        let extension = DiagnosticsReportExtension;
        assert_eq!(
            extension.manifest().capabilities,
            vec![Capability::DiagnosticsRead, Capability::ArtifactWrite]
        );
        let descriptor = ReportCommand.descriptor();
        assert_eq!(
            descriptor.required_capabilities,
            vec![Capability::DiagnosticsRead, Capability::ArtifactWrite]
        );
    }

    #[test]
    fn command_writes_artifact_record_output() {
        let host = MockHost::new(vec![line(json!({"event":"turn_start"}))], false);
        let output = ReportCommand
            .execute(CommandContext { input: json!({}) }, &host)
            .expect("report");
        let writes = host.writes.borrow();
        let artifact: Value = serde_json::from_slice(&writes[0].bytes).expect("artifact json");

        assert_eq!(output["persisted_event_id"], json!("event-artifact"));
        assert_eq!(
            output["relative_path"],
            json!("extensions/diagnostics-report/artifacts/hash")
        );
        assert_eq!(artifact["schema"], json!(SCHEMA));
        assert_eq!(writes[0].media_type, MEDIA_TYPE);
    }

    struct MockHost {
        lines: Vec<String>,
        truncated: bool,
        writes: RefCell<Vec<ArtifactWrite>>,
    }

    impl MockHost {
        fn new(lines: Vec<String>, truncated: bool) -> Self {
            Self {
                lines,
                truncated,
                writes: RefCell::new(Vec::new()),
            }
        }
    }

    impl HostApi for MockHost {
        fn query_provenance(
            &self,
            _query: ProvenanceQuery,
        ) -> Result<ProvenancePage, ExtensionError> {
            Err(input_error("unused"))
        }

        fn read_diagnostics(
            &self,
            _query: DiagnosticsQuery,
        ) -> Result<DiagnosticsPage, ExtensionError> {
            Ok(DiagnosticsPage {
                lines: self.lines.clone(),
                truncated: self.truncated,
            })
        }

        fn state_dir(&self) -> Result<std::path::PathBuf, ExtensionError> {
            Err(input_error("unused"))
        }

        fn write_artifact(
            &self,
            artifact: ArtifactWrite,
        ) -> Result<ArtifactRecord, ExtensionError> {
            self.writes.borrow_mut().push(artifact);
            Ok(ArtifactRecord {
                persisted_event_id: "event-artifact".to_owned(),
                relative_path: "extensions/diagnostics-report/artifacts/hash".to_owned(),
                sha256: "hash".to_owned(),
                byte_len: 123,
            })
        }

        fn load_event_feed_checkpoint(
            &self,
            _name: &str,
        ) -> Result<Option<euler_sdk::EventFeedCheckpoint>, ExtensionError> {
            Err(input_error("unused"))
        }

        fn store_event_feed_checkpoint(
            &self,
            _name: &str,
            _checkpoint: euler_sdk::EventFeedCheckpoint,
        ) -> Result<(), ExtensionError> {
            Err(input_error("unused"))
        }
    }

    fn line(value: Value) -> String {
        serde_json::to_string(&value).expect("line json")
    }
}
