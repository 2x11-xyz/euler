use std::fs;
use std::path::{Path, PathBuf};

const MAX_MAIN_LINES: usize = 50;

#[test]
fn euler_cli_main_stays_a_composition_root() {
    let main = crate_root().join("src/main.rs");
    let source = fs::read_to_string(&main).expect("read euler-cli main.rs");
    let line_count = source.lines().count();

    assert!(
        line_count <= MAX_MAIN_LINES,
        "euler-cli main.rs has {line_count} lines; keep it at or below {MAX_MAIN_LINES} and move implementation into src/cli modules"
    );
    assert_eq!(
        source.matches("fn main(").count(),
        1,
        "main.rs must contain exactly one binary entrypoint"
    );
    assert!(
        source.contains("cli::run()"),
        "main.rs must delegate command execution to cli::run"
    );
}

fn crate_root() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).to_path_buf()
}
