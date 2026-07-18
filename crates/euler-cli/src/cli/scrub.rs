use anyhow::{anyhow, Result};
use euler_core::{EulerHome, SessionStore};
use std::io::{self, IsTerminal, Read};

/// `euler scrub <session>` — post-close credential removal (issue #100).
/// Resolves a closed session by id or name and reads exact values from stdin,
/// keeping them out of shell history and the process command line.
#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) struct ScrubArgs {
    pub(crate) session: String,
}

impl ScrubArgs {
    pub(super) fn parse(args: &mut impl Iterator<Item = String>) -> Result<Self> {
        let Some(session) = args.next() else {
            return Err(anyhow!(
                "usage: euler scrub <session>  (secret values are read from stdin, one per line)"
            ));
        };
        // Values are NEVER accepted as arguments: argv lands in shell history
        // and the process command line (`ps`), which is exactly where a secret
        // must not go. Read them from stdin instead.
        if args.next().is_some() {
            return Err(anyhow!(
                "euler scrub reads secret values from stdin (one per line), not arguments — \
                 passing a secret on the command line would leak it into shell history and `ps`"
            ));
        }
        Ok(Self { session })
    }
}

/// Resolve a closed session by id or name and scrub the given values from every
/// persistent surface (issue #100). Prints a counts-only summary and the
/// un-recall caveat; the value never reaches stdout.
pub(super) fn run(args: ScrubArgs) -> Result<()> {
    let home = EulerHome::resolve()?;
    let store = SessionStore::new(home)?;
    let Some(record) = store.resolve_session_reference(&args.session)? else {
        return Err(anyhow!("no session found with id or name {}", args.session));
    };
    let values = read_values_from_stdin()?;
    let surfaces = euler_core::scrub::ScrubSurfaces {
        workspace_root: record.root(),
    };
    let report = euler_core::scrub::scrub_closed_session(
        record.session_dir(),
        record.id(),
        surfaces,
        &values,
    )?;
    println!("{}", report.summary_line());
    Ok(())
}

/// Read the secret values to scrub from stdin (one per line) — never argv, so
/// they cannot leak into shell history or the process command line.
fn read_values_from_stdin() -> Result<Vec<String>> {
    if io::stdin().is_terminal() {
        return Err(anyhow!(
            "euler scrub reads secret values from stdin; pipe them in, one per line \
             (e.g. `printf '%s\\n' \"$SECRET\" | euler scrub <session>`)"
        ));
    }
    let mut input = String::new();
    io::stdin().read_to_string(&mut input)?;
    parse_values(&input)
}

pub(crate) fn parse_values(input: &str) -> Result<Vec<String>> {
    let values: Vec<String> = input
        .lines()
        .filter(|line| !line.trim().is_empty())
        .map(str::to_owned)
        .collect();
    if values.is_empty() {
        return Err(anyhow!("no secret values provided on stdin"));
    }
    if let Some(short) = values
        .iter()
        .find(|value| value.len() < euler_core::scrub::MIN_SCRUB_VALUE_LEN)
    {
        return Err(anyhow!(
            "scrub value is too short ({} chars); the minimum is {} to avoid mangling \
             unrelated content",
            short.len(),
            euler_core::scrub::MIN_SCRUB_VALUE_LEN
        ));
    }
    Ok(values)
}
