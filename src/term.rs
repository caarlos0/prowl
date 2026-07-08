//! Quiet the terminal input while the watch dashboard is on screen: typed keys
//! are neither echoed onto the dashboard nor left buffered for the shell. Signal
//! keys (Ctrl-C, Ctrl-Z, Ctrl-\) keep working because `ISIG` stays enabled — we
//! only drop local echo and line buffering, then discard whatever was typed.

/// The outcome of waiting for the next refresh while watching.
#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub enum Wait {
    /// The interval elapsed: do a scheduled refresh.
    Tick,
    /// The user pressed `r`/`R`: refresh now.
    Refresh,
    /// The user pressed `?`: toggle the help (help) legend.
    ToggleHelp,
    /// The user pressed Tab: switch to the other view.
    SwitchView,
    /// `k` / Up arrow: move the selection up one row.
    Up,
    /// `j` / Down arrow: move the selection down one row.
    Down,
    /// `g`: move the selection to the first row.
    Top,
    /// `G`: move the selection to the last row.
    Bottom,
    /// Ctrl-U: move the selection up half a page.
    HalfUp,
    /// Ctrl-D: move the selection down half a page.
    HalfDown,
    /// `Enter`: open the selected row in the browser.
    Open,
    /// `/`: open the search prompt to filter rows.
    Search,
    /// A lone `Esc`: clear an applied search filter.
    Cancel,
}

/// A keystroke while the search prompt is open (raw text input, unlike the
/// semantic `Wait` actions of normal mode).
#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub enum SearchKey {
    /// A printable character to append to the query.
    Char(char),
    /// Backspace: drop the last query character.
    Backspace,
    /// Enter: apply the filter and leave the prompt.
    Enter,
    /// Esc: clear the filter and leave the prompt.
    Esc,
    /// The interval elapsed with no input: do a scheduled refresh.
    Tick,
}

#[cfg(unix)]
mod imp {
    use std::cmp::Ordering;
    use std::io::IsTerminal;
    use std::os::fd::AsRawFd;
    use std::sync::{Mutex, OnceLock};
    use std::time::Instant;

    use super::{SearchKey, Wait};

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

    /// Poll stdin until it's readable or `deadline` elapses, then read once.
    /// `Some(n)` = n bytes in `buf`; `None` = timeout / EOF / stdin isn't a
    /// quieted terminal — in the can't-read cases it first sleeps out the
    /// interval so the caller (which treats `None` as a tick) doesn't busy-loop.
    /// Retries on `EINTR` (e.g. SIGCONT after Ctrl-Z).
    fn poll_bytes(deadline: Instant, buf: &mut [u8]) -> Option<usize> {
        let sleep_out = || std::thread::sleep(deadline.saturating_duration_since(Instant::now()));
        let Some((fd, _)) = *SAVED.lock().unwrap() else {
            sleep_out();
            return None;
        };
        loop {
            let remaining = deadline.saturating_duration_since(Instant::now());
            if remaining.is_zero() {
                return None;
            }
            let ms = remaining.as_millis().min(i32::MAX as u128) as i32;
            let mut pfd = libc::pollfd {
                fd,
                events: libc::POLLIN,
                revents: 0,
            };
            match unsafe { libc::poll(&mut pfd, 1, ms) }.cmp(&0) {
                Ordering::Less => {
                    if std::io::Error::last_os_error().raw_os_error() == Some(libc::EINTR) {
                        continue;
                    }
                    sleep_out();
                    return None;
                }
                Ordering::Equal => return None, // timed out
                Ordering::Greater => {}
            }
            let n = unsafe { libc::read(fd, buf.as_mut_ptr().cast(), buf.len()) };
            if n <= 0 {
                sleep_out(); // EOF/error on stdin
                return None;
            }
            return Some(n as usize);
        }
    }

    /// Discard any input still buffered (collapses key-repeat / an escape burst).
    fn drain() {
        if let Some((fd, _)) = *SAVED.lock().unwrap() {
            let mut buf = [0u8; 256];
            while unsafe { libc::read(fd, buf.as_mut_ptr().cast(), buf.len()) } > 0 {}
        }
    }

    /// Wait up to `deadline` for the next scheduled refresh, returning early on
    /// a recognized keypress: `r`/`R` refresh, Tab switch view, `?` help, `/`
    /// search, Enter open, and the movement keys (`j`/`k`/`g`/`G`, the arrows,
    /// and Ctrl-D/Ctrl-U for half a page). Every other keystroke is discarded.
    /// Falls back to a plain sleep when stdin isn't a quieted terminal.
    pub fn wait(deadline: Instant) -> Wait {
        let mut buf = [0u8; 256];
        loop {
            let Some(n) = poll_bytes(deadline, &mut buf) else {
                return Wait::Tick;
            };
            let Some(action) = classify(&buf[..n]) else {
                continue; // unrecognized keys keep us waiting
            };
            drain(); // collapse key-repeat into a single action
            return action;
        }
    }

    /// Read a burst of search-prompt input, or a tick if the interval elapsed.
    /// Unlike `wait`, keystrokes are not collapsed — every typed character is
    /// returned so live filtering keeps up with fast typing.
    pub fn read_search(deadline: Instant) -> Vec<SearchKey> {
        let mut buf = [0u8; 256];
        match poll_bytes(deadline, &mut buf) {
            Some(n) => parse_search(&buf[..n]),
            None => vec![SearchKey::Tick],
        }
    }

    /// Parse raw input bytes into search keystrokes: printable ASCII becomes
    /// `Char`, CR/LF `Enter`, DEL/BS `Backspace`, a lone ESC `Esc`. CSI/SS3
    /// escape sequences (arrows) and other control/non-ASCII bytes are ignored.
    fn parse_search(bytes: &[u8]) -> Vec<SearchKey> {
        let mut out = Vec::new();
        let mut i = 0;
        while i < bytes.len() {
            match bytes[i] {
                0x1b => {
                    // A 3-byte CSI/SS3 escape (`ESC [ x` / `ESC O x`, e.g. an
                    // arrow) is swallowed; a lone ESC cancels the search.
                    if matches!(bytes.get(i + 1), Some(b'[' | b'O')) {
                        i += 3;
                        continue;
                    }
                    out.push(SearchKey::Esc);
                }
                b'\r' | b'\n' => out.push(SearchKey::Enter),
                0x7f | 0x08 => out.push(SearchKey::Backspace),
                0x20..=0x7e => out.push(SearchKey::Char(bytes[i] as char)),
                _ => {}
            }
            i += 1;
        }
        out
    }

    /// Map a burst of input bytes to the highest-priority recognized action, if
    /// any. Action keys (open/refresh/switch/help) win over movement so a stray
    /// arrow in the same read can't shadow them. Enter arrives as CR or (after
    /// ICRNL) LF; arrows are the 3-byte escape sequences `ESC [ A` / `ESC [ B`.
    fn classify(bytes: &[u8]) -> Option<Wait> {
        let has = |b: u8| bytes.contains(&b);
        let seq = |s: &[u8]| bytes.windows(s.len()).any(|w| w == s);
        if has(b'\r') || has(b'\n') {
            Some(Wait::Open)
        } else if has(b'r') || has(b'R') {
            Some(Wait::Refresh)
        } else if has(b'\t') {
            Some(Wait::SwitchView)
        } else if has(b'?') {
            Some(Wait::ToggleHelp)
        } else if has(b'/') {
            Some(Wait::Search)
        } else if has(b'g') {
            Some(Wait::Top)
        } else if has(b'G') {
            Some(Wait::Bottom)
        } else if has(0x04) {
            Some(Wait::HalfDown) // Ctrl-D
        } else if has(0x15) {
            Some(Wait::HalfUp) // Ctrl-U
        } else if has(b'k') || seq(b"\x1b[A") {
            Some(Wait::Up)
        } else if has(b'j') || seq(b"\x1b[B") {
            Some(Wait::Down)
        } else if bytes.len() == 1 && bytes[0] == 0x1b {
            Some(Wait::Cancel) // a lone Esc (arrow escapes are longer)
        } else {
            None
        }
    }

    /// The terminal's height in rows (for the half-page jump), or `None` when
    /// stdout isn't a terminal or the size can't be read.
    pub fn height() -> Option<u16> {
        let stdout = std::io::stdout();
        if !stdout.is_terminal() {
            return None;
        }
        let mut ws: libc::winsize = unsafe { std::mem::zeroed() };
        let ok = unsafe { libc::ioctl(stdout.as_raw_fd(), libc::TIOCGWINSZ, &mut ws) } == 0;
        (ok && ws.ws_row > 0).then_some(ws.ws_row)
    }

    #[cfg(test)]
    mod tests {
        use super::super::SearchKey;
        use super::{Wait, classify, parse_search};

        #[test]
        fn classify_maps_keys_to_actions() {
            // Enter (CR, or LF after ICRNL) opens; `o` no longer does.
            assert_eq!(classify(b"\r"), Some(Wait::Open));
            assert_eq!(classify(b"\n"), Some(Wait::Open));
            assert_eq!(classify(b"o"), None);
            assert_eq!(classify(b"O"), None);

            assert_eq!(classify(b"r"), Some(Wait::Refresh));
            assert_eq!(classify(b"\t"), Some(Wait::SwitchView));
            assert_eq!(classify(b"?"), Some(Wait::ToggleHelp));
            assert_eq!(classify(b"/"), Some(Wait::Search));
            assert_eq!(classify(b"g"), Some(Wait::Top));
            assert_eq!(classify(b"G"), Some(Wait::Bottom));
            assert_eq!(classify(b"\x04"), Some(Wait::HalfDown)); // Ctrl-D
            assert_eq!(classify(b"\x15"), Some(Wait::HalfUp)); // Ctrl-U

            // Movement: letters and the arrow escape sequences.
            assert_eq!(classify(b"k"), Some(Wait::Up));
            assert_eq!(classify(b"j"), Some(Wait::Down));
            assert_eq!(classify(b"\x1b[A"), Some(Wait::Up));
            assert_eq!(classify(b"\x1b[B"), Some(Wait::Down));

            // A lone Esc cancels (clears a filter); an arrow escape does not.
            assert_eq!(classify(b"\x1b"), Some(Wait::Cancel));
            assert_eq!(classify(b"\x1b[C"), None);

            // Unrecognized keys are ignored.
            assert_eq!(classify(b"x"), None);
        }

        #[test]
        fn parse_search_reads_text_and_edits() {
            use SearchKey::{Backspace, Char, Enter, Esc};
            // A typed word yields one Char per byte.
            assert_eq!(parse_search(b"foo"), vec![Char('f'), Char('o'), Char('o')]);
            // Editing keys.
            assert_eq!(
                parse_search(b"a\x7fb\r"),
                vec![Char('a'), Backspace, Char('b'), Enter]
            );
            // A lone ESC cancels; an arrow escape sequence is swallowed.
            assert_eq!(parse_search(b"\x1b"), vec![Esc]);
            assert_eq!(parse_search(b"a\x1b[Bb"), vec![Char('a'), Char('b')]);
        }
    }
}

#[cfg(not(unix))]
mod imp {
    use super::{SearchKey, Wait};
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
    pub fn read_search(deadline: Instant) -> Vec<SearchKey> {
        std::thread::sleep(deadline.saturating_duration_since(Instant::now()));
        vec![SearchKey::Tick]
    }
    pub fn height() -> Option<u16> {
        None
    }
}

pub use imp::{QuietInput, height, quiet, read_search, restore, wait};
