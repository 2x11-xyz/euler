/// Best-effort terminal column count for banner centering. Uses libc
/// TIOCGWINSZ on Unix and falls back to 80 when the width cannot be
/// determined (non-TTY, narrow pipe, non-Unix, ioctl failure). Bounded to a
/// 20-column floor so centering math stays sane on degenerate inputs.
pub(super) fn terminal_width() -> usize {
    std::cmp::max(terminal_columns().unwrap_or(80), 20)
}

#[cfg(unix)]
fn terminal_columns() -> Option<usize> {
    use std::os::fd::AsRawFd;
    // Safety: TIOCGWINSZ writes a winsize struct through the ioctl pointer;
    // the struct is stack-local and the fd is stdout. This specific request
    // is observational with respect to terminal state.
    unsafe {
        let mut ws: libc::winsize = std::mem::zeroed();
        let fd = std::io::stdout().as_raw_fd();
        if libc::ioctl(fd, libc::TIOCGWINSZ, &mut ws as *mut _) == 0 {
            let cols = ws.ws_col;
            if cols != 0 {
                return Some(cols as usize);
            }
        }
    }
    None
}

#[cfg(not(unix))]
fn terminal_columns() -> Option<usize> {
    None
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum InteractiveLaunch {
    Tui,
    LineOriented,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) struct TuiLaunchIntent {
    pub(crate) default_interactive: bool,
    pub(crate) no_tty_arg: bool,
    pub(crate) env_no_tty: bool,
    pub(crate) stdin_tty: bool,
    pub(crate) stdout_tty: bool,
}

pub(crate) fn decide_interactive_launch(intent: TuiLaunchIntent) -> InteractiveLaunch {
    if intent.default_interactive
        && !intent.no_tty_arg
        && !intent.env_no_tty
        && intent.stdin_tty
        && intent.stdout_tty
    {
        InteractiveLaunch::Tui
    } else {
        InteractiveLaunch::LineOriented
    }
}

pub(super) fn euler_no_tty_env() -> bool {
    std::env::var("EULER_NO_TTY").is_ok_and(|value| value == "1")
}
