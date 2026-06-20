//! Quiet the terminal input while the watch dashboard is on screen: typed keys
//! are neither echoed onto the dashboard nor left buffered for the shell. Signal
//! keys (Ctrl-C, Ctrl-Z, Ctrl-\) keep working because `ISIG` stays enabled — we
//! only drop local echo and line buffering, then discard whatever was typed.

/// The outcome of waiting for the next refresh while watching.
pub enum Wait {
    /// The interval elapsed: do a scheduled refresh.
    Tick,
    /// The user pressed `r`/`R`: refresh now.
    Refresh,
    /// The user pressed `?`: toggle the help (help) legend.
    ToggleHelp,
}

#[cfg(unix)]
mod imp {
    use std::cmp::Ordering;
    use std::io::IsTerminal;
    use std::os::fd::AsRawFd;
    use std::sync::{Mutex, OnceLock};
    use std::time::Instant;

    use super::Wait;

    use crate::render::{HIDE_CURSOR, SHOW_CURSOR};

    // The terminal state to restore, shared so the Ctrl-C handler — which exits
    // via `process::exit`, skipping destructors — can put it back too.
    static SAVED: Mutex<Option<(i32, libc::termios)>> = Mutex::new(None);

    // Terminal state captured for the Ctrl-Z (SIGTSTP) / resume (SIGCONT)
    // handlers. Set once from `quiet`, then only read, so the handlers can reach
    // it without locking (`OnceLock::get` is just an atomic load). `termios` is a
    // plain C struct, hence `Send + Sync`.
    struct Suspend {
        fd: i32,
        original: libc::termios,
        quiet: libc::termios,
    }
    static SUSPEND: OnceLock<Suspend> = OnceLock::new();

    /// Write a static escape sequence straight to stdout — `write(2)` is one of
    /// the few calls safe to make from a signal handler (`print!` is not).
    fn raw_write(s: &str) {
        unsafe {
            libc::write(libc::STDOUT_FILENO, s.as_ptr().cast(), s.len());
        }
    }

    fn set_handler(sig: libc::c_int, handler: extern "C" fn(libc::c_int)) {
        unsafe {
            let mut sa: libc::sigaction = std::mem::zeroed();
            sa.sa_sigaction = handler as *const () as libc::sighandler_t;
            libc::sigemptyset(&mut sa.sa_mask);
            sa.sa_flags = libc::SA_RESTART;
            libc::sigaction(sig, &sa, std::ptr::null_mut());
        }
    }

    /// (Re)install the SIGTSTP handler. Split out because `on_tstp` resets the
    /// disposition to the default before stopping, so `on_cont` must re-arm it.
    fn arm_tstp() {
        set_handler(libc::SIGTSTP, on_tstp);
    }

    /// Ctrl-Z: show the cursor and restore the shell's terminal mode, then let
    /// the default SIGTSTP actually stop us. `SIGTSTP` is masked for the duration
    /// of this handler, so the re-raised signal is delivered (with the default
    /// disposition) once we return — stopping the process cleanly.
    extern "C" fn on_tstp(_sig: libc::c_int) {
        if let Some(s) = SUSPEND.get() {
            unsafe {
                libc::tcsetattr(s.fd, libc::TCSANOW, &s.original);
                raw_write(SHOW_CURSOR);
                libc::signal(libc::SIGTSTP, libc::SIG_DFL);
                libc::raise(libc::SIGTSTP);
            }
        }
    }

    /// Resume (`fg`): re-arm the suspend handler, re-quiet stdin, and hide the
    /// cursor again so the dashboard picks up where it left off.
    extern "C" fn on_cont(_sig: libc::c_int) {
        if let Some(s) = SUSPEND.get() {
            arm_tstp();
            unsafe {
                libc::tcsetattr(s.fd, libc::TCSANOW, &s.quiet);
            }
            raw_write(HIDE_CURSOR);
        }
    }

    /// Install the Ctrl-Z / resume handlers that keep the cursor in sync. Called
    /// once, after `quiet` has captured the terminal modes.
    fn install_suspend(fd: i32, original: libc::termios, quiet: libc::termios) {
        if SUSPEND
            .set(Suspend {
                fd,
                original,
                quiet,
            })
            .is_err()
        {
            return;
        }
        arm_tstp();
        set_handler(libc::SIGCONT, on_cont);
    }

    /// Drop guard that restores the terminal mode on a normal or `?` return.
    pub struct QuietInput;

    impl Drop for QuietInput {
        fn drop(&mut self) {
            restore();
        }
    }

    /// Drop echo and line buffering on stdin so typed keys are swallowed, while
    /// keeping signal generation (Ctrl-C/Ctrl-Z/…) intact. Returns `None` (a
    /// no-op) when stdin is not a terminal.
    pub fn quiet() -> Option<QuietInput> {
        let stdin = std::io::stdin();
        if !stdin.is_terminal() {
            return None;
        }
        let fd = stdin.as_raw_fd();
        let mut term = unsafe { std::mem::zeroed::<libc::termios>() };
        if unsafe { libc::tcgetattr(fd, &mut term) } != 0 {
            return None;
        }
        *SAVED.lock().unwrap() = Some((fd, term));

        let mut quiet = term;
        // Keep ISIG (signal keys); just stop echoing and line-buffering input.
        quiet.c_lflag &= !(libc::ECHO | libc::ICANON);
        // Make reads return immediately so `drain` never blocks.
        quiet.c_cc[libc::VMIN] = 0;
        quiet.c_cc[libc::VTIME] = 0;
        if unsafe { libc::tcsetattr(fd, libc::TCSANOW, &quiet) } != 0 {
            SAVED.lock().unwrap().take();
            return None;
        }
        // Keep the cursor visible in the shell if the user suspends with Ctrl-Z.
        install_suspend(fd, term, quiet);
        Some(QuietInput)
    }

    /// Restore the terminal to its previous mode (best-effort, idempotent). The
    /// `TCSAFLUSH` discards any keystrokes still buffered when we leave.
    pub fn restore() {
        if let Some((fd, term)) = SAVED.lock().unwrap().take() {
            unsafe {
                libc::tcsetattr(fd, libc::TCSAFLUSH, &term);
            }
        }
    }

    /// Wait up to `deadline` for the next scheduled refresh, returning early on
    /// a recognized keypress: `r`/`R` to refresh now, `?` to toggle the help
    /// legend. Every other keystroke is discarded. Falls back to a plain sleep
    /// when stdin isn't a quieted terminal.
    pub fn wait(deadline: Instant) -> Wait {
        let Some((fd, _)) = *SAVED.lock().unwrap() else {
            std::thread::sleep(deadline.saturating_duration_since(Instant::now()));
            return Wait::Tick;
        };
        let mut buf = [0u8; 256];
        loop {
            let remaining = deadline.saturating_duration_since(Instant::now());
            if remaining.is_zero() {
                return Wait::Tick;
            }
            let ms = remaining.as_millis().min(i32::MAX as u128) as i32;
            let mut pfd = libc::pollfd {
                fd,
                events: libc::POLLIN,
                revents: 0,
            };
            match unsafe { libc::poll(&mut pfd, 1, ms) }.cmp(&0) {
                // Interrupted (e.g. SIGCONT after Ctrl-Z) — retry; any other
                // error: wait the interval out rather than spin.
                Ordering::Less => {
                    if std::io::Error::last_os_error().raw_os_error() == Some(libc::EINTR) {
                        continue;
                    }
                    std::thread::sleep(deadline.saturating_duration_since(Instant::now()));
                    return Wait::Tick;
                }
                Ordering::Equal => return Wait::Tick, // timed out: scheduled refresh
                Ordering::Greater => {}
            }
            let n = unsafe { libc::read(fd, buf.as_mut_ptr().cast(), buf.len()) };
            if n <= 0 {
                // EOF/error on stdin: stop polling, just wait out the interval.
                std::thread::sleep(deadline.saturating_duration_since(Instant::now()));
                return Wait::Tick;
            }
            let bytes = &buf[..n as usize];
            // `r` (refresh) takes precedence over `?` (help toggle) if both were
            // typed in the same burst; other keys keep us waiting.
            let action = if bytes.iter().any(|&b| b == b'r' || b == b'R') {
                Wait::Refresh
            } else if bytes.contains(&b'?') {
                Wait::ToggleHelp
            } else {
                continue;
            };
            // Collapse key-repeat into a single action.
            while unsafe { libc::read(fd, buf.as_mut_ptr().cast(), buf.len()) } > 0 {}
            return action;
        }
    }
}

#[cfg(not(unix))]
mod imp {
    use super::Wait;
    use std::time::Instant;

    pub struct QuietInput;

    pub fn quiet() -> Option<QuietInput> {
        None
    }
    pub fn restore() {}
    pub fn wait(deadline: Instant) -> Wait {
        std::thread::sleep(deadline.saturating_duration_since(Instant::now()));
        Wait::Tick
    }
}

pub use imp::{QuietInput, quiet, restore, wait};
