use super::ExtensionSearchArgs;
use crate::bundled_extensions::BundledDescriptor;
use euler_core::{ExtensionMaterialization, LinkedExtension};
use euler_sdk::{LoadedExtensionPackage, StaticCommandDescriptor};
use serde::Serialize;
use std::path::Path;

#[derive(Serialize)]
pub(super) struct PackageValidationInfo<'a> {
    id: &'a str,
    display_name: &'a str,
    version: &'a str,
    source_path: String,
    manifest_sha256: &'a str,
    runtime_kind: &'a str,
    command_count: usize,
    status: &'a str,
}

#[derive(Serialize)]
pub(super) struct LinkedLinkInfo<'a> {
    id: &'a str,
    source_path: String,
    manifest_sha256: &'a str,
    updated_ts_ms: u64,
    runtime_kind: &'a str,
    status: &'static str,
    broken_reason: Option<&'a str>,
}

#[derive(Serialize)]
pub(super) struct LinkedInfo<'a> {
    id: &'a str,
    display_name: &'a str,
    version: &'a str,
    source_kind: &'static str,
    runtime_kind: &'a str,
    capabilities: &'a [String],
    commands: &'a [StaticCommandDescriptor],
    source_path: Option<String>,
    manifest_sha256: &'a str,
    updated_ts_ms: u64,
    status: &'static str,
    execution_granted: bool,
    requires_review: bool,
    requires_execution_grant: bool,
    broken_reason: Option<&'a str>,
}

#[derive(Serialize)]
pub(super) struct UnlinkInfo<'a> {
    pub(super) id: &'a str,
    pub(super) status: &'static str,
}

#[derive(Serialize)]
pub(super) struct UninstallInfo<'a> {
    pub(super) id: &'a str,
    pub(super) source_kind: &'static str,
    pub(super) status: &'static str,
}

#[derive(Serialize)]
pub(super) struct InstalledInfo<'a> {
    id: &'a str,
    display_name: &'a str,
    version: &'a str,
    source_kind: &'static str,
    manifest_sha256: &'a str,
    updated_ts_ms: u64,
    runtime_kind: &'a str,
    status: &'static str,
    execution_granted: bool,
    requires_review: bool,
    requires_execution_grant: bool,
}

pub(super) fn package_validation_info<'a>(
    package: &'a LoadedExtensionPackage,
    status: &'a str,
) -> PackageValidationInfo<'a> {
    PackageValidationInfo {
        id: &package.descriptor.id,
        display_name: &package.descriptor.display_name,
        version: &package.descriptor.version,
        source_path: display_path(&package.canonical_dir),
        manifest_sha256: &package.manifest_sha256,
        runtime_kind: &package.descriptor.runtime_kind,
        command_count: package.descriptor.commands.len(),
        status,
    }
}

pub(super) fn linked_link_info(linked: &LinkedExtension) -> LinkedLinkInfo<'_> {
    LinkedLinkInfo {
        id: &linked.id,
        source_path: display_path(&linked.source_path),
        manifest_sha256: &linked.manifest_sha256,
        updated_ts_ms: linked.updated_ts_ms,
        runtime_kind: &linked.descriptor.runtime_kind,
        status: linked.status.as_str(),
        broken_reason: linked.broken_reason.as_deref(),
    }
}

pub(super) fn installed_info_summary(linked: &LinkedExtension) -> InstalledInfo<'_> {
    InstalledInfo {
        id: &linked.id,
        display_name: &linked.descriptor.display_name,
        version: &linked.descriptor.version,
        source_kind: linked.materialization.as_str(),
        manifest_sha256: &linked.manifest_sha256,
        updated_ts_ms: linked.updated_ts_ms,
        runtime_kind: &linked.descriptor.runtime_kind,
        status: linked.status.as_str(),
        execution_granted: false,
        requires_review: false,
        requires_execution_grant: true,
    }
}

pub(super) fn linked_info(linked: &LinkedExtension) -> LinkedInfo<'_> {
    let is_linked = linked.materialization == ExtensionMaterialization::Linked;
    LinkedInfo {
        id: &linked.id,
        display_name: &linked.descriptor.display_name,
        version: &linked.descriptor.version,
        source_kind: linked.materialization.as_str(),
        runtime_kind: &linked.descriptor.runtime_kind,
        capabilities: &linked.descriptor.capabilities,
        commands: &linked.descriptor.commands,
        source_path: is_linked.then(|| display_path(&linked.source_path)),
        manifest_sha256: &linked.manifest_sha256,
        updated_ts_ms: linked.updated_ts_ms,
        status: linked.status.as_str(),
        execution_granted: false,
        requires_review: is_linked,
        requires_execution_grant: !is_linked,
        broken_reason: linked.broken_reason.as_deref(),
    }
}

fn display_path(path: &Path) -> String {
    path.to_string_lossy().into_owned()
}

#[derive(Serialize)]
pub(super) struct SearchOutput<'a> {
    query: &'a str,
    filters: SearchFilters<'a>,
    results: Vec<SearchResult>,
}

impl<'a> SearchOutput<'a> {
    pub(super) fn new(search: &'a ExtensionSearchArgs, results: Vec<SearchResult>) -> Self {
        Self {
            query: search.query.as_deref().unwrap_or_default(),
            filters: SearchFilters {
                capabilities: &search.capabilities,
                runtime_kind: search.runtime_kind.as_deref().unwrap_or_default(),
            },
            results,
        }
    }
}

#[derive(Serialize)]
struct SearchFilters<'a> {
    capabilities: &'a [String],
    runtime_kind: &'a str,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize)]
pub(super) struct SearchResult {
    #[serde(skip)]
    order: usize,
    id: String,
    display_name: String,
    version: String,
    source_kind: String,
    runtime_kind: String,
    status: String,
    execution_granted: bool,
    requires_review: bool,
    requires_execution_grant: bool,
    capabilities: Vec<String>,
    commands: Vec<SearchCommand>,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize)]
struct SearchCommand {
    name: String,
    display_name: String,
    summary: String,
    required_capabilities: Vec<String>,
}

pub(super) fn search_result_for_bundled(
    descriptor: &BundledDescriptor,
    status: &str,
) -> SearchResult {
    SearchResult {
        order: descriptor.table_index,
        id: descriptor.id.to_owned(),
        display_name: descriptor.display_name.to_owned(),
        version: descriptor.version.to_owned(),
        source_kind: "bundled".to_owned(),
        runtime_kind: descriptor.runtime_kind.to_owned(),
        status: status.to_owned(),
        execution_granted: status == "enabled",
        requires_review: false,
        requires_execution_grant: false,
        capabilities: descriptor
            .capabilities
            .iter()
            .map(|capability| capability.as_str().to_owned())
            .collect(),
        commands: descriptor
            .commands
            .iter()
            .map(|command| SearchCommand {
                name: command.name.to_owned(),
                display_name: command.display_name.to_owned(),
                summary: command.summary.to_owned(),
                required_capabilities: command
                    .required_capabilities
                    .iter()
                    .map(|capability| capability.as_str().to_owned())
                    .collect(),
            })
            .collect(),
    }
}

pub(super) fn search_result_for_linked(linked: &LinkedExtension) -> SearchResult {
    let is_linked = linked.materialization == ExtensionMaterialization::Linked;
    SearchResult {
        order: usize::MAX,
        id: linked.id.clone(),
        display_name: linked.descriptor.display_name.clone(),
        version: linked.descriptor.version.clone(),
        source_kind: linked.materialization.as_str().to_owned(),
        runtime_kind: linked.descriptor.runtime_kind.clone(),
        status: linked.status.as_str().to_owned(),
        execution_granted: false,
        requires_review: is_linked,
        requires_execution_grant: !is_linked,
        capabilities: linked.descriptor.capabilities.clone(),
        commands: linked
            .descriptor
            .commands
            .iter()
            .map(|command| SearchCommand {
                name: command.name.clone(),
                display_name: command.display_name.clone(),
                summary: command.summary.clone(),
                required_capabilities: command.required_capabilities.clone(),
            })
            .collect(),
    }
}

pub(super) fn search_matches(search: &ExtensionSearchArgs, result: &SearchResult) -> bool {
    if let Some(runtime) = &search.runtime_kind {
        if &result.runtime_kind != runtime {
            return false;
        }
    }
    if search
        .capabilities
        .iter()
        .any(|capability| !result.capabilities.iter().any(|item| item == capability))
    {
        return false;
    }
    let Some(query) = &search.query else {
        return true;
    };
    result.matches_query(&query.to_ascii_lowercase())
}

pub(super) fn sort_search_results(results: &mut [SearchResult]) {
    results.sort_by(|left, right| {
        source_rank(&left.source_kind)
            .cmp(&source_rank(&right.source_kind))
            .then_with(|| left.order.cmp(&right.order))
            .then_with(|| left.id.cmp(&right.id))
            .then_with(|| left.version.cmp(&right.version))
    });
}

impl SearchResult {
    fn matches_query(&self, folded_query: &str) -> bool {
        contains_ascii_case_insensitive(&self.id, folded_query)
            || contains_ascii_case_insensitive(&self.display_name, folded_query)
            || self
                .capabilities
                .iter()
                .any(|capability| contains_ascii_case_insensitive(capability, folded_query))
            || self
                .commands
                .iter()
                .any(|command| command.matches_query(folded_query))
    }
}

impl SearchCommand {
    fn matches_query(&self, folded_query: &str) -> bool {
        contains_ascii_case_insensitive(&self.name, folded_query)
            || contains_ascii_case_insensitive(&self.display_name, folded_query)
            || contains_ascii_case_insensitive(&self.summary, folded_query)
            || self
                .required_capabilities
                .iter()
                .any(|capability| contains_ascii_case_insensitive(capability, folded_query))
    }
}

fn contains_ascii_case_insensitive(haystack: &str, folded_needle: &str) -> bool {
    haystack.to_ascii_lowercase().contains(folded_needle)
}

fn source_rank(source_kind: &str) -> u8 {
    match source_kind {
        "bundled" => 0,
        "installed" => 1,
        "linked" => 2,
        _ => 3,
    }
}
