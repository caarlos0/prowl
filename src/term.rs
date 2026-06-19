//! Quiet the terminal input while the watch dashboard is on screen: typed keys
//! are neither echoed onto the dashboard nor left buffered for the shell. Signal
//! keys (Ctrl-C, Ctrl-Z, Ctrl-\) keep working because `ISIG` stays enabled — we
//! only drop local echo and line buffering, then discard whatever was typed.

#[cfg(unix)]
mod imp {
    use std::cmp::Ordering;
    use std::io::IsTerminal;
    use std::os::fd::AsRawFd;
    use std::sync::Mutex;
    use std::time::{Duration, Instant};

    // The terminal state to restore, shared so the Ctrl-C handler — which exits
    // via `process::exit`, skipping destructors — can put it back too.
    static SAVED: Mutex<Option<(i32, libc::termios)>> = Mutex::new(None);

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

    /// Wait up to `dur` for the next refresh. Returns `true` early when the
    /// user presses `r`/`R` to refresh now; every other keystroke is discarded.
    /// Falls back to a plain sleep when stdin isn't a quieted terminal.
    pub fn wait_or_refresh(dur: Duration) -> bool {
        let Some((fd, _)) = *SAVED.lock().unwrap() else {
            std::thread::sleep(dur);
            return false;
        };
        let deadline = Instant::now() + dur;
        let mut buf = [0u8; 256];
        loop {
            let remaining = deadline.saturating_duration_since(Instant::now());
            if remaining.is_zero() {
                return false;
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
                    return false;
                }
                Ordering::Equal => return false, // timed out: scheduled refresh
                Ordering::Greater => {}
            }
            let n = unsafe { libc::read(fd, buf.as_mut_ptr().cast(), buf.len()) };
            if n <= 0 {
                // EOF/error on stdin: stop polling, just wait out the interval.
                std::thread::sleep(deadline.saturating_duration_since(Instant::now()));
                return false;
            }
            if buf[..n as usize].iter().any(|&b| b == b'r' || b == b'R') {
                // Collapse key-repeat into a single refresh.
                while unsafe { libc::read(fd, buf.as_mut_ptr().cast(), buf.len()) } > 0 {}
                return true;
            }
            // Non-'r' keys are discarded; keep waiting for the rest of `dur`.
        }
    }
}

#[cfg(not(unix))]
mod imp {
    pub struct QuietInput;

    pub fn quiet() -> Option<QuietInput> {
        None
    }
    pub fn restore() {}
    pub fn wait_or_refresh(dur: std::time::Duration) -> bool {
        std::thread::sleep(dur);
        false
    }
}

pub use imp::{QuietInput, quiet, restore, wait_or_refresh};
