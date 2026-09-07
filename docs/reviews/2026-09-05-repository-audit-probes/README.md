# Euler audit evidence — 2026-09-05

Synthetic probes against baseline commit `9dfb881`. This is a standalone Cargo package with its own `[workspace]`, kept at `docs/reviews/2026-09-05-repository-audit-probes/`. Its crate dependencies use `../../../crates/`; keep this directory depth when copying it.

These are **audit probes, not regression tests expecting corrected behavior**. Tracking issue #225 maps each probe to the fix that retires it; delete a probe when its regression test lands, and delete this package when the last one goes. This package must never become a CI gate. The library assertions deliberately pass when the observed baseline defects occur. After fixing an issue, convert its probe into a regression test with the desired assertions instead of preserving a passing audit result. Binaries print observations; their exit status alone does not establish correctness.

Prerequisites: the repository's supported Rust toolchain, a Unix host, `sh`, `cat`, `uniq`, `rg`, and Python 3 with `os.fork`. Verified during the audit on macOS with Rust 1.98.0; initial exploratory probes used 1.97.1. Fixtures use fresh temporary directories and synthetic credentials. The provider probe uses one loopback HTTP server; it contacts no external provider. The process probe launches a short-lived synthetic descendant that exits after about one second.

From the repository root:

```sh
env -u EULER_HOME cargo test --manifest-path docs/reviews/2026-09-05-repository-audit-probes/Cargo.toml --locked --lib -- --nocapture
env -u EULER_HOME cargo run --manifest-path docs/reviews/2026-09-05-repository-audit-probes/Cargo.toml --locked --bin context
env -u EULER_HOME cargo run --manifest-path docs/reviews/2026-09-05-repository-audit-probes/Cargo.toml --locked --bin compaction_isolation
env -u EULER_HOME cargo run --manifest-path docs/reviews/2026-09-05-repository-audit-probes/Cargo.toml --locked --bin provider
env -u EULER_HOME cargo run --manifest-path docs/reviews/2026-09-05-repository-audit-probes/Cargo.toml --locked --bin extension_native
env -u EULER_HOME cargo run --manifest-path docs/reviews/2026-09-05-repository-audit-probes/Cargo.toml --locked --bin extension_add
env -u EULER_HOME cargo run --manifest-path docs/reviews/2026-09-05-repository-audit-probes/Cargo.toml --locked --bin extension_nonzero
env -u EULER_HOME cargo run --manifest-path docs/reviews/2026-09-05-repository-audit-probes/Cargo.toml --locked --release --bin scaling
```

`extension_native` deliberately triggers a caught descriptor panic; its panic-hook stderr is expected and the process continues. `extension_nonzero` embeds `assets/child_exit.py` at compile time and writes the script solely into its fresh temporary directory. Scaling results are machine/build-dependent observations, not production latency guarantees. The scaling probe creates 4,097 small files plus a 32 MiB file, then removes its temporary directory on normal exit.

Contents:

- `src/lib.rs`: ten assertions for permission bypasses, prepare/apply races, tool-ID replay, revoked grants, terminal lifecycle, torn UTF-8 tails and lost partial output.
- `src/bin/context.rs`: child transcript retention and sequential/parallel context admission.
- `src/bin/compaction_isolation.rs`: project-guidance classification through compaction.
- `src/bin/provider.rs`: synthetic redaction, checkpoints, scrub, auth, SSE and custom-provider configuration observations.
- `src/bin/extension_*.rs`: native registration, package Add flow, and subprocess-descendant lifetime.
- `src/bin/scaling.rs`: pagination, canvas swaps, workspace snapshots and bounded file-read measurements.
- `outputs/`: recorded run outputs, including release scaling from the original baseline harness.
- `REVIEW_NOTES.md`: concise interpretation and source references; consult the repository audit report for priorities and remediation plans.

The lockfile began with the repository baseline dependency graph and was updated only to include this standalone probe package and its required graph. No compiled binaries, build caches, real credentials or real session contents are included.

The final bundle also preserves `outputs/workspace-tests.log`, the initial failing run in `outputs/workspace-tests-initial.log`, and `outputs/workspace-clippy.log`. The root audit report explains the macOS fixture failures and the successful run's one explicit skip. `outputs/portable-tests.log` verifies the copied package with its relative crate paths using `cargo test --locked --all-targets`; the ten assertions pass on the audited baseline.
