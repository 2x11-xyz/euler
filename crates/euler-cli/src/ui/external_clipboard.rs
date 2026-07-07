use std::ffi::OsString;
use std::io::Write;
use std::process::{Command, Stdio};

/// Keep the in-band OSC 52 payload below common terminal and multiplexer limits.
const OSC52_MAX_ENCODED_BYTES: usize = 100 * 1024;

pub(crate) trait ClipboardSink {
    fn copy(&self, text: &str) -> Result<(), String>;
}

#[derive(Clone, Debug, Default)]
pub(crate) struct SystemClipboard;

impl ClipboardSink for SystemClipboard {
    fn copy(&self, text: &str) -> Result<(), String> {
        let mut failures = Vec::new();
        let candidates = clipboard_commands();
        if candidates.is_empty() {
            return Err("no desktop clipboard command available".to_owned());
        }
        for candidate in candidates {
            match try_clipboard_command(*candidate, text) {
                Ok(true) => return Ok(()),
                Ok(false) => {}
                Err(message) => failures.push(message),
            }
        }
        Err(clipboard_error(failures))
    }
}

#[derive(Clone, Copy)]
struct ClipboardCommand {
    program: &'static str,
    args: &'static [&'static str],
}

fn clipboard_commands() -> &'static [ClipboardCommand] {
    clipboard_commands_for(
        cfg!(target_os = "macos"),
        cfg!(any(
            target_os = "linux",
            target_os = "freebsd",
            target_os = "netbsd",
            target_os = "openbsd"
        )),
        std::env::var_os("WAYLAND_DISPLAY"),
        std::env::var_os("DISPLAY"),
    )
}

fn clipboard_commands_for(
    is_macos: bool,
    is_unix_desktop: bool,
    wayland_display: Option<OsString>,
    x11_display: Option<OsString>,
) -> &'static [ClipboardCommand] {
    const MACOS: &[ClipboardCommand] = &[ClipboardCommand {
        program: "pbcopy",
        args: &[],
    }];
    const WAYLAND_FIRST: &[ClipboardCommand] = &[
        ClipboardCommand {
            program: "wl-copy",
            args: &[],
        },
        ClipboardCommand {
            program: "xclip",
            args: &["-selection", "clipboard"],
        },
        ClipboardCommand {
            program: "xsel",
            args: &["--input", "--clipboard"],
        },
    ];
    const X11_FIRST: &[ClipboardCommand] = &[
        ClipboardCommand {
            program: "xclip",
            args: &["-selection", "clipboard"],
        },
        ClipboardCommand {
            program: "xsel",
            args: &["--input", "--clipboard"],
        },
        ClipboardCommand {
            program: "wl-copy",
            args: &[],
        },
    ];
    if is_macos {
        return MACOS;
    }
    if !is_unix_desktop {
        return &[];
    }
    if wayland_display.is_some() {
        return WAYLAND_FIRST;
    }
    if x11_display.is_some() {
        return X11_FIRST;
    }
    X11_FIRST
}

fn try_clipboard_command(command: ClipboardCommand, text: &str) -> Result<bool, String> {
    let mut child = match Command::new(command.program)
        .args(command.args)
        .stdin(Stdio::piped())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
    {
        Ok(child) => child,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(false),
        Err(error) => return Err(format!("could not launch {}: {error}", command.program)),
    };
    write_clipboard_stdin(&mut child, command.program, text)?;
    match child.wait() {
        Ok(status) if status.success() => Ok(true),
        Ok(_) => Err(format!("{} exited with a non-zero status", command.program)),
        Err(error) => Err(format!("clipboard command failed: {error}")),
    }
}

fn write_clipboard_stdin(
    child: &mut std::process::Child,
    program: &str,
    text: &str,
) -> Result<(), String> {
    let Some(mut stdin) = child.stdin.take() else {
        return Err(format!("{program} did not open stdin"));
    };
    stdin
        .write_all(text.as_bytes())
        .map_err(|error| format!("could not write to {program}: {error}"))
}

fn clipboard_error(failures: Vec<String>) -> String {
    if failures.is_empty() {
        return "no desktop clipboard command found".to_owned();
    }
    failures.join("; ")
}

pub(crate) fn terminal_clipboard_sequence(text: &str) -> Result<String, String> {
    let encoded = base64_encode(text.as_bytes());
    let encoded_len = encoded.len();
    if encoded_len > OSC52_MAX_ENCODED_BYTES {
        return Err(format!(
            "encoded payload is {encoded_len} bytes, above the {} byte OSC 52 limit",
            OSC52_MAX_ENCODED_BYTES
        ));
    }
    Ok(format!("\x1b]52;c;{encoded}\x07"))
}

fn base64_encode(input: &[u8]) -> String {
    const TABLE: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
    let mut output = String::with_capacity(input.len().div_ceil(3) * 4);
    for chunk in input.chunks(3) {
        let first = chunk[0];
        let second = chunk.get(1).copied().unwrap_or(0);
        let third = chunk.get(2).copied().unwrap_or(0);
        output.push(TABLE[usize::from(first >> 2)] as char);
        output.push(TABLE[usize::from(((first & 0b0000_0011) << 4) | (second >> 4))] as char);
        if chunk.len() > 1 {
            output.push(TABLE[usize::from(((second & 0b0000_1111) << 2) | (third >> 6))] as char);
        } else {
            output.push('=');
        }
        if chunk.len() > 2 {
            output.push(TABLE[usize::from(third & 0b0011_1111)] as char);
        } else {
            output.push('=');
        }
    }
    output
}

#[cfg(test)]
mod tests {
    use super::{
        base64_encode, clipboard_commands_for, terminal_clipboard_sequence, OSC52_MAX_ENCODED_BYTES,
    };
    use std::ffi::OsString;

    #[test]
    fn base64_encoder_matches_rfc_examples() {
        assert_eq!(base64_encode(b""), "");
        assert_eq!(base64_encode(b"f"), "Zg==");
        assert_eq!(base64_encode(b"fo"), "Zm8=");
        assert_eq!(base64_encode(b"foo"), "Zm9v");
        assert_eq!(base64_encode(b"hello"), "aGVsbG8=");
        assert_eq!(base64_encode("λ".as_bytes()), "zrs=");
        assert_eq!(base64_encode("🧪".as_bytes()), "8J+nqg==");
        assert_eq!(base64_encode("e\u{301}".as_bytes()), "ZcyB");
    }

    #[test]
    fn clipboard_command_selection_is_environment_scoped() {
        assert_eq!(
            clipboard_commands_for(true, false, None, None)[0].program,
            "pbcopy"
        );
        assert_eq!(
            clipboard_commands_for(false, true, Some(OsString::from("wayland-1")), None)[0].program,
            "wl-copy"
        );
        let mixed = clipboard_commands_for(
            false,
            true,
            Some(OsString::from("wayland-1")),
            Some(OsString::from(":0")),
        );
        assert_eq!(
            mixed
                .iter()
                .map(|command| command.program)
                .collect::<Vec<_>>(),
            vec!["wl-copy", "xclip", "xsel"]
        );
        let x11 = clipboard_commands_for(false, true, None, Some(OsString::from(":0")));
        assert_eq!(
            x11.iter()
                .map(|command| command.program)
                .collect::<Vec<_>>(),
            vec!["xclip", "xsel", "wl-copy"]
        );
        let unknown_linux = clipboard_commands_for(false, true, None, None);
        assert_eq!(
            unknown_linux
                .iter()
                .map(|command| command.program)
                .collect::<Vec<_>>(),
            vec!["xclip", "xsel", "wl-copy"]
        );
        assert!(clipboard_commands_for(false, false, None, None).is_empty());
    }

    #[test]
    fn osc52_sequence_wraps_base64_payload() {
        let output = terminal_clipboard_sequence("copy me").expect("osc52");

        assert_eq!(output.as_bytes(), b"\x1b]52;c;Y29weSBtZQ==\x07");
    }

    #[test]
    fn osc52_sequence_does_not_wrap_long_payloads() {
        let payload = "x".repeat(1024);
        let sequence = terminal_clipboard_sequence(&payload).expect("osc52");

        assert!(!sequence.contains('\n'));
        assert!(!sequence.contains('\r'));
        assert!(sequence.starts_with("\x1b]52;c;"));
        assert!(sequence.ends_with('\x07'));
    }

    #[test]
    fn osc52_sequence_accepts_payload_at_encoded_limit() {
        let payload = "x".repeat((OSC52_MAX_ENCODED_BYTES / 4) * 3);
        let sequence = terminal_clipboard_sequence(&payload).expect("at limit");
        let encoded = sequence
            .strip_prefix("\x1b]52;c;")
            .and_then(|value| value.strip_suffix('\x07'))
            .expect("osc52 wrapper");

        assert_eq!(encoded.len(), OSC52_MAX_ENCODED_BYTES);
    }

    #[test]
    fn osc52_sequence_rejects_oversized_payloads() {
        let payload = "x".repeat((OSC52_MAX_ENCODED_BYTES / 4) * 3 + 1);

        let error = terminal_clipboard_sequence(&payload).expect_err("oversized payload");

        assert!(error.contains("above the 102400 byte OSC 52 limit"));
    }
}
