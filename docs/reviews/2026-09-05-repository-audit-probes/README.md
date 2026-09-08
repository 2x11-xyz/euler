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

- `src/lib.rs`: five remaining assertions for tool-ID replay, revoked grants, terminal lifecycle, partial MaxTokens completion and torn UTF-8 tails.
- `src/bin/context.rs`: child transcript retention and sequential/parallel context admission.
- `src/bin/compaction_isolation.rs`: project-guidance classification through compaction.
- `src/bin/provider.rs`: synthetic redaction, checkpoints, scrub, auth, SSE and custom-provider configuration observations.
- `src/bin/extension_*.rs`: native registration, package Add flow, and subprocess-descendant lifetime.
- `src/bin/scaling.rs`: pagination, canvas swaps, workspace snapshots and bounded file-read measurements.
- `outputs/`: recorded run outputs, including release scaling from the original baseline harness.
- `REVIEW_NOTES.md`: concise interpretation and source references; consult the repository audit report for priorities and remediation plans.

Retired probes (regression tests with the desired assertions now live in the owning crate; see #225):

| Probe | Finding | Retired by | Regression test on `main` |
| --- | --- | --- | --- |
| `reproduces_uniq_write_without_approval` | F01 | #226 (two-parser approval grammar) | `command_safety::tests::options_are_an_allowlist_not_a_denylist` |
| `reproduces_glob_and_cd_read_scope_bypass` | F02 | #226 | `command_safety::tests::audit_f02_glob_cd_and_follow_fixture_now_requires_approval`, `every_operand_is_confined_with_no_exempt_position`, `recursive_default_readers_cannot_prove_safe` |
| `reproduces_prepared_create_overwriting_intervening_file` | F04 | #227 (structured-write hardening) | `tools_test::a_create_publishes_atomically_and_still_refuses_a_racing_name` |
| `reproduces_prepared_edit_losing_intervening_change` | F04 / F36 | #227 | `tools_test::structured_write_rejects_stale_preimage_after_prepare`, `session_test::checkpoint_is_recorded_before_the_write_and_stays_prepared_when_the_write_fails` |
| `reproduces_partial_stream_content_missing_from_durable_record` | F10 | #216 (partial response durability) | `assistant.response.chunk` tests in `session_test.rs`, `tests/session_loop.rs`, `tests/resume.rs` |

Each was confirmed to fail against `main` at `47600e6` before deletion. `reproduces_partial_max_tokens_as_normal_completion` (F11) was re-pinned to the `run.terminal` event added in #218; its defect is unchanged.

The lockfile began with the repository baseline dependency graph and was updated only to include this standalone probe package and its required graph; it was regenerated once more when #226 added `tree-sitter` to the workspace graph. No compiled binaries, build caches, real credentials or real session contents are included.

Workspace test, Clippy, and portable-package logs are not retained (the audit report summarizes them); `outputs/` holds only the probe observations named above.
