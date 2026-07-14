//! High-fidelity graph exports and human-readable downstream views.

use self::graph::ViewerDag;
use self::palette::Palette;
use crate::active_state::ActiveGraphState;
use crate::projection::Projection;
use crate::slot_summary::render_artifact_summary;
use crate::{
    event_ids, input_error, optional_non_empty_string, optional_string, optional_string_array,
    parse_limit, parse_optional_positive_usize, provenance_query_args, split_update_events,
    write_projection_artifact, DISPLAY_NAME, MEDIA_TYPE_JSON, SCHEMA_NAME,
};
use euler_sdk::{
    ArgSpec, ArgValueKind, ArtifactRecord, ArtifactWrite, Capability, CommandContext,
    CommandDescriptor, ExtensionCommand, ExtensionError, HostApi, Invocation, ProvenanceQuery,
};
use serde_json::{json, Map, Value};
use std::fs::{self, File, OpenOptions};
use std::io::{self, Write};
use std::path::{Component, Path};
use std::sync::atomic::{AtomicU64, Ordering};

pub(super) mod graph;
mod html;
mod palette;
mod svg;
mod text;

pub(super) const EXPORT_COMMAND_NAME: &str = "export";
const EXPORT_METADATA_SCHEMA: &str = "euler.causal_dag.export.v1";
const OUT_PATH_MAX_BYTES: usize = 4096;
static MATERIALIZE_SEQUENCE: AtomicU64 = AtomicU64::new(0);

#[derive(Clone, Copy, Debug)]
pub(super) struct CausalDagExportCommand;

impl ExtensionCommand for CausalDagExportCommand {
    fn descriptor(&self) -> CommandDescriptor {
        let mut args = provenance_query_args(true, true);
        args.extend([
            ArgSpec {
                flag: "format".to_owned(),
                input_key: "format".to_owned(),
                value_kind: ArgValueKind::BoundedString { max_bytes: 16 },
                required: false,
                repeatable: false,
            },
            ArgSpec {
                flag: "out".to_owned(),
                input_key: "out".to_owned(),
                value_kind: ArgValueKind::BoundedString {
                    max_bytes: OUT_PATH_MAX_BYTES,
                },
                required: false,
                repeatable: false,
            },
        ]);
        CommandDescriptor {
            invocation: Invocation::User,
            name: EXPORT_COMMAND_NAME.to_owned(),
            display_name: "Export causal DAG".to_owned(),
            summary: "Export the active Causal DAG as HTML, JSON, SVG, DOT, Markdown, or summary."
                .to_owned(),
            required_capabilities: vec![
                Capability::ProvenanceRead,
                Capability::ArtifactWrite,
                Capability::FsRead,
                Capability::FsWrite,
            ],
            args,
            accepts_session_id: true,
        }
    }

    fn execute(
        &self,
        context: CommandContext,
        host: &dyn HostApi,
    ) -> Result<Value, ExtensionError> {
        let input = ExportInput::parse(&context.input)?;
        let source = GraphSource::load(host, &input)?;
        let dag = ViewerDag::from_artifact(&source.artifact)?;
        let palette = Palette::load()?;
        let bytes = input.format.render(&source.artifact, &dag, &palette)?;
        let suggested_name = format!("{}.{}", dag.suggested_stem(), input.format.extension());
        let materialized_bytes = input.out.as_ref().map(|_| bytes.clone());
        let record = match (input.format, source.record.as_ref()) {
            (ExportFormat::Json, Some(record)) => record.clone(),
            _ => write_view_artifact(host, &source, &dag, input.format, &suggested_name, bytes)?,
        };
        let out_path = input
            .out
            .as_deref()
            .zip(materialized_bytes.as_deref())
            .map(|(path, bytes)| materialize_copy(path, bytes))
            .transpose()?;
        Ok(export_output(
            &dag,
            &source,
            &record,
            input.format,
            &suggested_name,
            out_path.as_deref(),
        ))
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum ExportFormat {
    Html,
    Json,
    Svg,
    Dot,
    Markdown,
    Summary,
}

impl ExportFormat {
    fn parse(value: Option<&str>) -> Result<Self, ExtensionError> {
        match value.unwrap_or("json") {
            "html" => Ok(Self::Html),
            "json" => Ok(Self::Json),
            "svg" => Ok(Self::Svg),
            "dot" => Ok(Self::Dot),
            "markdown" | "md" => Ok(Self::Markdown),
            "summary" | "txt" => Ok(Self::Summary),
            value => Err(input_error(format!(
                "causal-dag export format must be html, json, svg, dot, markdown, or summary; got `{value}`"
            ))),
        }
    }

    fn as_str(self) -> &'static str {
        match self {
            Self::Html => "html",
            Self::Json => "json",
            Self::Svg => "svg",
            Self::Dot => "dot",
            Self::Markdown => "markdown",
            Self::Summary => "summary",
        }
    }

    fn extension(self) -> &'static str {
        match self {
            Self::Html => "html",
            Self::Json => "json",
            Self::Svg => "svg",
            Self::Dot => "dot",
            Self::Markdown => "md",
            Self::Summary => "txt",
        }
    }

    fn media_type(self) -> &'static str {
        match self {
            Self::Html => "text/html; charset=utf-8",
            Self::Json => MEDIA_TYPE_JSON,
            Self::Svg => "image/svg+xml",
            Self::Dot => "text/vnd.graphviz; charset=utf-8",
            Self::Markdown => "text/markdown; charset=utf-8",
            Self::Summary => "text/plain; charset=utf-8",
        }
    }

    fn render(
        self,
        artifact: &Value,
        dag: &ViewerDag,
        palette: &Palette,
    ) -> Result<Vec<u8>, ExtensionError> {
        match self {
            Self::Html => html::render_html(dag),
            Self::Json => canonical_json_bytes(artifact),
            Self::Svg => svg::render_svg(dag, palette),
            Self::Dot => text::render_dot(dag, palette),
            Self::Markdown => Ok(text::render_markdown(dag)),
            Self::Summary => {
                let mut summary = render_artifact_summary(artifact)?.into_bytes();
                summary.push(b'\n');
                Ok(summary)
            }
        }
    }
}

#[derive(Debug)]
struct ExportInput {
    format: ExportFormat,
    out: Option<String>,
    limit: usize,
    scan_limit: Option<usize>,
    after_event_id: Option<String>,
    kinds: Vec<String>,
    session_id: Option<String>,
}

impl ExportInput {
    fn parse(value: &Value) -> Result<Self, ExtensionError> {
        let empty = Map::new();
        let object = match value {
            Value::Null => &empty,
            Value::Object(object) => object,
            _ => return Err(input_error("causal-dag export input must be a JSON object")),
        };
        reject_unknown_fields(object)?;
        let out = optional_non_empty_string(object, "out")?;
        if let Some(path) = out.as_deref() {
            validate_out_path(path)?;
        }
        Ok(Self {
            format: ExportFormat::parse(optional_string(object, "format")?.as_deref())?,
            out,
            limit: parse_limit(object)?,
            scan_limit: parse_optional_positive_usize(object, "scan_limit")?,
            after_event_id: optional_string(object, "after_event_id")?,
            kinds: optional_string_array(object, "kinds")?,
            session_id: optional_non_empty_string(object, "session_id")?,
        })
    }

    fn query(&self) -> ProvenanceQuery {
        let mut query = ProvenanceQuery::new(self.limit);
        query.scan_limit = self.scan_limit.unwrap_or(query.scan_limit);
        query.after_event_id.clone_from(&self.after_event_id);
        query.kinds.clone_from(&self.kinds);
        query
    }
}

#[derive(Clone, Debug)]
struct GraphSource {
    artifact: Value,
    artifact_event_id: String,
    record: Option<ArtifactRecord>,
    active: bool,
}

impl GraphSource {
    fn load(host: &dyn HostApi, input: &ExportInput) -> Result<Self, ExtensionError> {
        if let Some(active) = ActiveGraphState::load(host)? {
            validate_input_session(&active, input.session_id.as_deref())?;
            let byte_len = canonical_json_bytes(active.artifact())?.len();
            let record = active
                .artifact_relative_path()
                .map(|relative_path| ArtifactRecord {
                    persisted_event_id: active.artifact_event_id().to_owned(),
                    relative_path: relative_path.to_owned(),
                    sha256: active.artifact_sha256().to_owned(),
                    byte_len,
                });
            return Ok(Self {
                artifact: active.artifact().clone(),
                artifact_event_id: active.artifact_event_id().to_owned(),
                record,
                active: true,
            });
        }
        Self::snapshot(host, input)
    }

    fn snapshot(host: &dyn HostApi, input: &ExportInput) -> Result<Self, ExtensionError> {
        let page = host.query_provenance(input.query())?;
        let split = split_update_events(&page.events)?;
        let projection = Projection::from_events(
            &split.source_events,
            input.session_id.as_deref(),
            !page.truncated,
        )?;
        let artifact = projection.artifact_value();
        let source_event_ids = event_ids(&split.source_events);
        let record = write_projection_artifact(host, &projection, &page, source_event_ids)?;
        Ok(Self {
            artifact,
            artifact_event_id: record.persisted_event_id.clone(),
            record: Some(record),
            active: false,
        })
    }
}

fn write_view_artifact(
    host: &dyn HostApi,
    source: &GraphSource,
    dag: &ViewerDag,
    format: ExportFormat,
    suggested_name: &str,
    bytes: Vec<u8>,
) -> Result<ArtifactRecord, ExtensionError> {
    host.write_artifact(ArtifactWrite {
        display_name: format!("{DISPLAY_NAME} {} export", format.as_str()),
        media_type: format.media_type().to_owned(),
        bytes,
        source_event_ids: vec![source.artifact_event_id.clone()],
        metadata: Map::from_iter([
            ("schema".to_owned(), json!(EXPORT_METADATA_SCHEMA)),
            ("source_schema".to_owned(), json!(SCHEMA_NAME)),
            ("format".to_owned(), json!(format.as_str())),
            (
                "source_artifact_event_id".to_owned(),
                json!(source.artifact_event_id),
            ),
            ("suggested_name".to_owned(), json!(suggested_name)),
            ("node_count".to_owned(), json!(dag.node_count())),
            ("edge_count".to_owned(), json!(dag.edge_count())),
            ("cross_arc_count".to_owned(), json!(dag.cross_arc_count())),
            (
                "self_contained".to_owned(),
                json!(format == ExportFormat::Html),
            ),
        ]),
    })
}

fn export_output(
    dag: &ViewerDag,
    source: &GraphSource,
    record: &ArtifactRecord,
    format: ExportFormat,
    suggested_name: &str,
    out_path: Option<&str>,
) -> Value {
    json!({
        "schema": EXPORT_METADATA_SCHEMA,
        "source_schema": SCHEMA_NAME,
        "format": format.as_str(),
        "suggested_name": suggested_name,
        "persisted_event_id": record.persisted_event_id,
        "relative_path": record.relative_path,
        "sha256": record.sha256,
        "byte_len": record.byte_len,
        "source_artifact_event_id": source.artifact_event_id,
        "active_graph": source.active,
        "node_count": dag.node_count(),
        "edge_count": dag.edge_count(),
        "cross_arc_count": dag.cross_arc_count(),
        "self_contained": format == ExportFormat::Html,
        "out_path": out_path,
    })
}

fn validate_input_session(
    active: &ActiveGraphState,
    expected: Option<&str>,
) -> Result<(), ExtensionError> {
    let actual = active
        .artifact()
        .pointer("/session/id")
        .and_then(Value::as_str)
        .ok_or_else(|| input_error("active causal-dag graph has no session id"))?;
    if expected.is_some_and(|expected| expected != actual) {
        return Err(input_error(
            "session_id does not match the active causal-dag graph",
        ));
    }
    Ok(())
}

fn reject_unknown_fields(object: &Map<String, Value>) -> Result<(), ExtensionError> {
    for key in object.keys() {
        if !matches!(
            key.as_str(),
            "format" | "out" | "limit" | "scan_limit" | "after_event_id" | "kinds" | "session_id"
        ) {
            return Err(input_error(format!("unknown input field `{key}`")));
        }
    }
    Ok(())
}

fn canonical_json_bytes(value: &Value) -> Result<Vec<u8>, ExtensionError> {
    let mut bytes = serde_json::to_vec(value)
        .map_err(|error| input_error(format!("causal-dag JSON encode failed: {error}")))?;
    bytes.push(b'\n');
    Ok(bytes)
}

fn validate_out_path(path: &str) -> Result<(), ExtensionError> {
    let path = Path::new(path);
    if path.as_os_str().len() > OUT_PATH_MAX_BYTES
        || path.is_absolute()
        || path
            .components()
            .any(|component| !matches!(component, Component::Normal(_) | Component::CurDir))
        || path.file_name().is_none()
    {
        return Err(input_error(
            "causal-dag export out must be a workspace-relative file path without `..`",
        ));
    }
    Ok(())
}

fn materialize_copy(path: &str, bytes: &[u8]) -> Result<String, ExtensionError> {
    validate_out_path(path)?;
    let root = std::env::current_dir()
        .and_then(|path| path.canonicalize())
        .map_err(materialize_error)?;
    let relative = Path::new(path);
    let destination = root.join(relative);
    let parent = destination
        .parent()
        .ok_or_else(|| input_error("causal-dag export out has no parent directory"))?;
    let parent = parent.canonicalize().map_err(materialize_error)?;
    if !parent.starts_with(&root) {
        return Err(input_error(
            "causal-dag export out resolves outside the workspace",
        ));
    }
    let file_name = destination
        .file_name()
        .and_then(|name| name.to_str())
        .ok_or_else(|| input_error("causal-dag export out file name is not UTF-8"))?;
    let sequence = MATERIALIZE_SEQUENCE.fetch_add(1, Ordering::Relaxed);
    let temp = parent.join(format!(
        ".{file_name}.{}.{sequence}.tmp",
        std::process::id()
    ));
    let result = write_temp_and_link(&temp, &destination, bytes);
    if result.is_err() {
        let _ = fs::remove_file(&temp);
    }
    result.map_err(materialize_error)?;
    Ok(relative.to_string_lossy().into_owned())
}

fn write_temp_and_link(temp: &Path, destination: &Path, bytes: &[u8]) -> io::Result<()> {
    let mut options = OpenOptions::new();
    options.write(true).create_new(true);
    let mut file = options.open(temp)?;
    set_private_permissions(&file)?;
    file.write_all(bytes)?;
    file.sync_all()?;
    drop(file);
    fs::hard_link(temp, destination)?;
    fs::remove_file(temp)?;
    sync_dir(destination.parent().unwrap_or_else(|| Path::new(".")))
}

#[cfg(unix)]
fn set_private_permissions(file: &File) -> io::Result<()> {
    use std::os::unix::fs::PermissionsExt;
    file.set_permissions(fs::Permissions::from_mode(0o600))
}

#[cfg(not(unix))]
fn set_private_permissions(_file: &File) -> io::Result<()> {
    Ok(())
}

#[cfg(unix)]
fn sync_dir(path: &Path) -> io::Result<()> {
    File::open(path)?.sync_all()
}

#[cfg(not(unix))]
fn sync_dir(_path: &Path) -> io::Result<()> {
    Ok(())
}

fn materialize_error(error: impl std::fmt::Display) -> ExtensionError {
    input_error(format!("causal-dag export out failed: {error}"))
}

#[cfg(test)]
mod tests {
    use super::{validate_out_path, ExportFormat};
    use crate::export::graph::ViewerDag;
    use crate::export::palette::Palette;
    use serde_json::Value;

    fn fixture() -> (Value, ViewerDag, Palette) {
        let artifact: Value = serde_json::from_str(include_str!(
            "../tests/fixtures/causal_dag/knuth_style_search/expected.causal-dag.json"
        ))
        .expect("fixture artifact");
        let dag = ViewerDag::from_artifact(&artifact).expect("viewer DAG");
        let palette = Palette::load().expect("palette");
        (artifact, dag, palette)
    }

    #[test]
    fn format_aliases_are_bounded_and_explicit() {
        assert_eq!(
            ExportFormat::parse(Some("md")).unwrap(),
            ExportFormat::Markdown
        );
        assert_eq!(
            ExportFormat::parse(Some("txt")).unwrap(),
            ExportFormat::Summary
        );
        assert!(ExportFormat::parse(Some("pdf")).is_err());
    }

    #[test]
    fn out_paths_stay_relative_without_parent_traversal() {
        assert!(validate_out_path("reports/dag.html").is_ok());
        assert!(validate_out_path("../dag.html").is_err());
        assert!(validate_out_path("/tmp/dag.html").is_err());
    }

    #[test]
    fn every_format_renders_from_the_same_v2_artifact() {
        let (artifact, dag, palette) = fixture();
        let cases = [
            (ExportFormat::Html, "<!DOCTYPE html>"),
            (ExportFormat::Json, "\"schema\":\"euler.causal_dag.v3\""),
            (ExportFormat::Svg, "<svg "),
            (ExportFormat::Dot, "digraph causal_dag"),
            (ExportFormat::Markdown, "## Backbone"),
            (ExportFormat::Summary, "GRAPH:"),
        ];
        for (format, marker) in cases {
            let bytes = format
                .render(&artifact, &dag, &palette)
                .expect("render format");
            assert!(
                String::from_utf8_lossy(&bytes).contains(marker),
                "{} output should contain {marker}",
                format.as_str()
            );
        }
    }
}
