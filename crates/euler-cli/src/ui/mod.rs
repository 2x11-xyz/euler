pub mod activity;
pub mod app;
#[cfg(test)]
pub(crate) mod app_layout;
pub mod banner;
pub mod bottom_surface;
pub mod commands;
pub mod composer;
pub(crate) mod consent_prompt;
pub mod dirty;
pub mod event_loop;
pub(crate) mod external_clipboard;
pub(crate) mod external_editor;
pub(crate) mod glyphs;
pub(crate) mod history_insert;
pub(crate) mod markdown;
pub(crate) mod markdown_stream;
pub(crate) mod metrics;
pub(crate) mod patch_approval;
pub(crate) mod patch_diff;
pub(crate) mod search;
pub mod status;
pub(crate) mod syntax;
pub mod terminal;
#[cfg(test)]
#[path = "test_backend_test.rs"]
pub mod test_backend;
pub(crate) mod text;
pub mod theme;
pub mod transcript;
#[cfg(test)]
mod transcript_patch_tests;
#[cfg(test)]
mod transcript_tests;
pub mod tui_decider;
pub mod visual_canvas;
pub(crate) mod workspace_files;
