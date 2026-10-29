//! The pane's pass-through (ARCHITECTURE §9): `claude attach` runs on a pseudo-terminal of its
//! own, and chief copies the bytes between it and the pane's terminal untouched. Claude draws on
//! the user's terminal as if it had it (its scrollback, its selection, its rendering), and chief
//! still sees every key on the way in, so Ctrl+Q quits chief whatever Claude is doing. There is
//! no emulator: chief reads the keys only as far as telling Ctrl+Q apart.

use std::io::{self, ErrorKind, Write};
use std::process::{Command, ExitStatus};
use std::time::{Duration, Instant};

use ratatui::crossterm::terminal;

use crate::embed::Pty;

/// How long one wait for bytes lasts; a signal (a switch, a hangup, a resize) cuts it short.
const TICK: Duration = Duration::from_millis(50);
/// How long the start of an escape sequence waits for the rest before it goes on as it is: a lone
/// Esc is a key of its own.
const HOLD: Duration = Duration::from_millis(10);
/// How long attach gets to be reaped once it closed its terminal.
const EXIT_WAIT: Duration = Duration::from_millis(500);
const ESC: u8 = 0x1b;
const CTRL_Q: u8 = 0x11;
/// The longest escape sequence the key reader holds back; anything longer isn't a key.
const MAX_SEQ: usize = 64;

/// How a relayed attach ended.
#[derive(Debug, PartialEq, Eq)]
pub enum Ended {
    /// By itself: with its status, or None when it closed its terminal and had to be ended.
    Exited(Option<ExitStatus>),
    /// chief ended it: the board wants another target.
    Switched,
    /// chief ended it: the user pressed Ctrl+Q, here or on the board.
    Quit,
    /// chief ended it: the pane's terminal hung up, or SIGHUP or SIGTERM arrived.
    HungUp,
}

/// What the relay asks the pane between waits. Each is looked at after every wait.
pub struct Signals<'a> {
    pub hung_up: &'a dyn Fn() -> bool,
    /// The board asked chief to quit (its Ctrl+Q).
    pub quit: &'a dyn Fn() -> bool,
    /// The board asked for another target, and it still applies.
    pub switch: &'a dyn Fn() -> bool,
    /// The terminal changed size (SIGWINCH) since the last look.
    pub resized: &'a dyn Fn() -> bool,
}

/// Runs `cmd` on a pseudo-terminal of the pane's terminal's size and relays until it ends. The
/// pane's terminal must be in raw mode: Claude sets its own modes on the pty, and chief passes
/// every byte on. A child chief ends gets SIGTERM, a grace period, then SIGKILL (`Pty::end`).
pub fn relay(cmd: Command, signals: &Signals) -> io::Result<Ended> {
    let (cols, rows) = terminal::size().unwrap_or((80, 24));
    let mut pty = Pty::open(cmd, cols, rows)?;
    let mut keys = Keys::default();
    // Keys attach hasn't taken yet: a write never blocks, or a long paste into a busy Claude
    // would wait on attach while attach waits on chief to read its output.
    let mut pending: Vec<u8> = Vec::new();
    let mut buf = vec![0u8; 1 << 16];
    let mut out = io::stdout();
    loop {
        if (signals.hung_up)() {
            pty.end();
            return Ok(Ended::HungUp);
        }
        if (signals.quit)() {
            pty.end();
            return Ok(Ended::Quit);
        }
        if (signals.switch)() {
            pty.end();
            return Ok(Ended::Switched);
        }
        if (signals.resized)() {
            let (cols, rows) = terminal::size().unwrap_or((cols, rows));
            pty.resize(cols, rows);
        }
        let mut fds = [
            libc::pollfd {
                fd: libc::STDIN_FILENO,
                events: libc::POLLIN,
                revents: 0,
            },
            libc::pollfd {
                fd: pty.fd(),
                events: libc::POLLIN | if pending.is_empty() { 0 } else { libc::POLLOUT },
                revents: 0,
            },
        ];
        let wait = if keys.holding() { HOLD } else { TICK };
        // SAFETY: polls two descriptors, passed by a pointer to a live array.
        let ready = unsafe { libc::poll(fds.as_mut_ptr(), 2, wait.as_millis() as libc::c_int) };
        if ready < 0 {
            let err = io::Error::last_os_error();
            if err.kind() == ErrorKind::Interrupted {
                continue;
            }
            pty.end();
            return Err(err);
        }
        if ready == 0 {
            pending.extend(keys.flush());
            // Exited with its terminal still held open by something it started.
            if pty.exited().is_some() {
                return Ok(Ended::Exited(finish(pty)));
            }
            continue;
        }

        // Output first: what Claude drew before the key came.
        let [input, child] = fds;
        if child.revents & (libc::POLLIN | libc::POLLHUP | libc::POLLERR) != 0 {
            match pty.read(&mut buf) {
                Ok(0) => return Ok(Ended::Exited(finish(pty))),
                Ok(n) => {
                    if out.write_all(&buf[..n]).and_then(|()| out.flush()).is_err() {
                        pty.end();
                        return Ok(Ended::HungUp);
                    }
                }
                Err(e) if e.kind() == ErrorKind::Interrupted => {}
                Err(e) if e.kind() == ErrorKind::WouldBlock => {
                    if child.revents & libc::POLLHUP != 0 {
                        return Ok(Ended::Exited(finish(pty)));
                    }
                }
                // EIO once the child side is closed.
                Err(_) => return Ok(Ended::Exited(finish(pty))),
            }
        }
        if child.revents & libc::POLLOUT != 0 {
            match pty.write_some(&pending) {
                Ok(n) => {
                    pending.drain(..n);
                }
                Err(e) if matches!(e.kind(), ErrorKind::Interrupted | ErrorKind::WouldBlock) => {}
                // The child side is going; its read of 0 or EIO ends the relay.
                Err(_) => pending.clear(),
            }
        }
        if input.revents & libc::POLLIN != 0 {
            let mut keys_in = [0u8; 4096];
            // SAFETY: reads at most `keys_in.len()` bytes into `keys_in`.
            let n = unsafe {
                libc::read(
                    libc::STDIN_FILENO,
                    keys_in.as_mut_ptr().cast(),
                    keys_in.len(),
                )
            };
            if n > 0 {
                let (pass, quit) = keys.feed(&keys_in[..n as usize]);
                if quit {
                    pty.end();
                    return Ok(Ended::Quit);
                }
                pending.extend(pass);
            } else {
                let kind = io::Error::last_os_error().kind();
                if n == 0 || !matches!(kind, ErrorKind::Interrupted | ErrorKind::WouldBlock) {
                    pty.end();
                    return Ok(Ended::HungUp);
                }
            }
        } else if input.revents & (libc::POLLHUP | libc::POLLERR | libc::POLLNVAL) != 0 {
            pty.end();
            return Ok(Ended::HungUp);
        }
    }
}

/// attach closed its terminal: its status once it is reaped, or None when it doesn't exit in
/// time and is ended.
fn finish(mut pty: Pty) -> Option<ExitStatus> {
    let deadline = Instant::now() + EXIT_WAIT;
    loop {
        if let Some(status) = pty.exited() {
            return Some(status);
        }
        if Instant::now() >= deadline {
            pty.end();
            return None;
        }
        std::thread::sleep(Duration::from_millis(10));
    }
}

/// Watches the keys on their way to attach for Ctrl+Q, in each encoding a terminal sends it in:
/// the plain byte; the kitty keyboard protocol's `CSI 113;5u` once Claude turns that on; xterm's
/// modifyOtherKeys `CSI 27;5;113~`. A paste is never a key. Everything else passes as it came.
#[derive(Default)]
pub struct Keys {
    /// An escape sequence read so far.
    seq: Vec<u8>,
    /// Inside a bracketed paste.
    paste: bool,
}

/// What a whole CSI sequence is to the relay.
#[derive(Debug, PartialEq)]
enum Csi {
    Pass,
    Quit,
    /// Ctrl+Q's repeat or release: chief's key, so attach never sees half of it.
    Drop,
}

impl Keys {
    /// The bytes of `input` to pass on, and whether Ctrl+Q came (nothing after it is passed).
    /// An escape sequence cut off at the end is held for the next read, or `flush`.
    pub fn feed(&mut self, input: &[u8]) -> (Vec<u8>, bool) {
        let mut out = Vec::with_capacity(input.len());
        for &b in input {
            if self.seq.is_empty() {
                match b {
                    ESC => self.seq.push(b),
                    CTRL_Q if !self.paste => return (out, true),
                    _ => out.push(b),
                }
                continue;
            }
            if self.seq.len() == 1 {
                self.seq.push(b);
                // Alt+key, SS3 and the like: not ours.
                if b != b'[' {
                    out.append(&mut self.seq);
                }
                continue;
            }
            match b {
                // Parameters and intermediates.
                0x20..=0x3f if self.seq.len() < MAX_SEQ => self.seq.push(b),
                0x40..=0x7e => {
                    self.seq.push(b);
                    match self.csi() {
                        Csi::Quit => {
                            self.seq.clear();
                            return (out, true);
                        }
                        Csi::Drop => self.seq.clear(),
                        Csi::Pass => out.append(&mut self.seq),
                    }
                }
                // A new sequence cuts this one off.
                ESC => {
                    out.append(&mut self.seq);
                    self.seq.push(b);
                }
                _ => {
                    out.append(&mut self.seq);
                    out.push(b);
                }
            }
        }
        (out, false)
    }

    /// Whether the start of an escape sequence is held back.
    pub fn holding(&self) -> bool {
        !self.seq.is_empty()
    }

    /// The held bytes, when the rest never came: a lone Esc, say.
    pub fn flush(&mut self) -> Vec<u8> {
        std::mem::take(&mut self.seq)
    }

    fn csi(&mut self) -> Csi {
        let (body, last) = (
            &self.seq[2..self.seq.len() - 1],
            self.seq[self.seq.len() - 1],
        );
        match (last, body) {
            (b'~', b"200") => {
                self.paste = true;
                Csi::Pass
            }
            (b'~', b"201") => {
                self.paste = false;
                Csi::Pass
            }
            _ if self.paste => Csi::Pass,
            (b'u', _) => kitty(body),
            (b'~', _) => other_keys(body),
            _ => Csi::Pass,
        }
    }
}

/// `113[:…];<mods>[:<event>][;…]`: the kitty keyboard protocol's Ctrl+Q, pressed (no event or
/// 1), repeated (2) or released (3).
fn kitty(body: &[u8]) -> Csi {
    let mut params = body.split(|&b| b == b';');
    let key = params.next().and_then(|p| number(first(p)));
    let mods = params.next().unwrap_or_default();
    let mut mods = mods.split(|&b| b == b':');
    let ctrl = mods.next().is_some_and(|m| only_ctrl(number(m)));
    if key != Some(113) || !ctrl {
        return Csi::Pass;
    }
    match mods.next().map(number) {
        None | Some(Some(1)) => Csi::Quit,
        Some(Some(2 | 3)) => Csi::Drop,
        _ => Csi::Pass,
    }
}

/// `27;<mods>;113`: xterm's modifyOtherKeys Ctrl+Q.
fn other_keys(body: &[u8]) -> Csi {
    let params: Vec<_> = body.split(|&b| b == b';').map(number).collect();
    match params[..] {
        [Some(27), mods, Some(113)] if only_ctrl(mods) => Csi::Quit,
        _ => Csi::Pass,
    }
}

fn first(param: &[u8]) -> &[u8] {
    param.split(|&b| b == b':').next().unwrap_or_default()
}

fn number(digits: &[u8]) -> Option<u32> {
    std::str::from_utf8(digits).ok()?.parse().ok()
}

/// A modifier parameter (1 + a bit mask) that holds Ctrl and no other modifier: Caps Lock and
/// Num Lock (64, 128) don't count, Shift, Alt, Super, Hyper and Meta do.
fn only_ctrl(mods: Option<u32>) -> bool {
    mods.and_then(|m| m.checked_sub(1))
        .is_some_and(|m| m & 0b11_1111 == 0b100)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn feed(input: &[u8]) -> (Vec<u8>, bool) {
        Keys::default().feed(input)
    }

    #[test]
    fn ctrl_q_in_each_encoding_quits_and_nothing_after_it_passes() {
        assert_eq!(feed(b"ab\x11cd"), (b"ab".to_vec(), true));
        for seq in [
            &b"\x1b[113;5u"[..],
            b"\x1b[113;5:1u",
            b"\x1b[113:81;5u",
            b"\x1b[113;5;113u",
            // Caps Lock and Num Lock on.
            b"\x1b[113;69u",
            b"\x1b[113;133u",
            b"\x1b[27;5;113~",
        ] {
            let mut input = b"a".to_vec();
            input.extend(seq);
            input.extend(b"b");
            assert_eq!(
                feed(&input),
                (b"a".to_vec(), true),
                "{}",
                String::from_utf8_lossy(seq)
            );
        }
    }

    #[test]
    fn other_keys_pass_as_they_came() {
        for input in [
            &b"hello\r"[..],
            // Ctrl+C, Ctrl+Shift+Q, Alt+Ctrl+Q, q: kitty, then modifyOtherKeys.
            b"\x1b[99;5u",
            b"\x1b[113;6u",
            b"\x1b[113;7u",
            b"\x1b[113u",
            b"\x1b[27;6;113~",
            // Arrows, SS3, Alt+q, a function key, an SGR mouse report.
            b"\x1b[A\x1bOA\x1bq\x1b[15~",
            b"\x1b[<0;10;5M",
            // Shift+Enter, released.
            b"\x1b[13;2:3u",
        ] {
            assert_eq!(
                feed(input),
                (input.to_vec(), false),
                "{}",
                String::from_utf8_lossy(input)
            );
        }
    }

    #[test]
    fn ctrl_qs_repeat_and_release_never_reach_attach() {
        assert_eq!(feed(b"\x1b[113;5:2u\x1b[113;5:3ux"), (b"x".to_vec(), false));
    }

    #[test]
    fn a_paste_is_never_a_key() {
        let mut keys = Keys::default();
        let paste = b"\x1b[200~a\x11b\x1b[113;5u\x1b[201~";
        assert_eq!(keys.feed(paste), (paste.to_vec(), false));
        assert_eq!(keys.feed(b"\x11"), (Vec::new(), true));
    }

    #[test]
    fn a_sequence_cut_between_reads_is_held_until_its_end_or_a_flush() {
        let mut keys = Keys::default();
        assert_eq!(keys.feed(b"a\x1b[113"), (b"a".to_vec(), false));
        assert!(keys.holding());
        assert_eq!(keys.feed(b";5u"), (Vec::new(), true));

        let mut keys = Keys::default();
        assert_eq!(keys.feed(b"\x1b"), (Vec::new(), false));
        assert_eq!(keys.flush(), b"\x1b");
        assert!(!keys.holding());
        assert_eq!(keys.feed(b"\x1b["), (Vec::new(), false));
        assert_eq!(keys.feed(b"B"), (b"\x1b[B".to_vec(), false));
    }

    #[test]
    fn what_isnt_a_sequence_after_all_passes() {
        // A control byte, a new Esc, and an overlong run.
        assert_eq!(feed(b"\x1b[1\rx"), (b"\x1b[1\rx".to_vec(), false));
        let mut keys = Keys::default();
        assert_eq!(
            keys.feed(b"\x1b[1\x1b[A"),
            (b"\x1b[1\x1b[A".to_vec(), false)
        );
        let long = [b"\x1b[".to_vec(), vec![b'1'; MAX_SEQ]].concat();
        assert_eq!(feed(&long), (long.clone(), false));
    }
}
