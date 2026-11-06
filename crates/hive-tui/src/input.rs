//! One input owner demultiplexes terminal replies before they can become editor keys.

#[cfg(unix)]
use crossterm::event::Event;

#[derive(Debug)]
#[cfg_attr(not(unix), allow(dead_code))]
pub enum Reply {
    ProgramStatus(crate::program_status::Features, std::time::Instant),
    Clipboard(Vec<u8>),
    PasteMode(u8),
    Error(String),
}

#[cfg(unix)]
#[derive(Default)]
struct Decoder {
    bytes: Vec<u8>,
    control: bool,
    discard: bool,
    escaped: bool,
    paste: bool,
    csi_discard: bool,
    quarantine: bool,
    started: Option<std::time::Instant>,
}

#[cfg(unix)]
impl Decoder {
    fn feed(&mut self, byte: u8, event: &mut impl FnMut(Event), reply: &mut impl FnMut(Reply)) {
        if self.quarantine {
            // A delayed reply tail is indistinguishable from typing. Require a lone Escape
            // before accepting keys again instead of releasing that tail into the composer.
            self.escaped = byte == 27;
            return;
        }
        if self.csi_discard {
            if (0x40..=0x7e).contains(&byte) {
                *self = Self::default();
            }
            return;
        }
        if self.control {
            let done = byte == 7 || (self.escaped && byte == b'\\');
            self.escaped = byte == 27;
            if !self.discard {
                self.bytes.push(byte);
                if self.bytes.len() > 16 * 1024 {
                    self.bytes.clear();
                    self.discard = true;
                    reply(Reply::Error(
                        "Terminal reply exceeds 16 KiB; retry with a smaller clipboard payload"
                            .into(),
                    ));
                }
            }
            if done {
                if !self.discard && self.bytes.starts_with(b"\x1b]5522;") {
                    let end = self.bytes.len() - if byte == 7 { 1 } else { 2 };
                    reply(Reply::Clipboard(self.bytes[7..end].to_vec()));
                }
                if !self.discard && self.bytes.starts_with(b"\x1b]7501;") {
                    let end = self.bytes.len() - if byte == 7 { 1 } else { 2 };
                    if let Some(features) =
                        crate::program_status::Features::parse(&self.bytes[7..end])
                    {
                        reply(Reply::ProgramStatus(features, std::time::Instant::now()));
                    }
                }
                *self = Self::default();
            }
            return;
        }
        self.bytes.push(byte);
        if self.paste {
            let end = self.bytes.ends_with(b"\x1b[201~");
            if self.bytes.len() > crate::editor::MAX_IMPORT_BYTES + 6 {
                // Keep only a terminator-sized suffix while discarding an oversized paste.
                self.discard = true;
                self.bytes.drain(..self.bytes.len() - 6);
            }
            if end {
                if self.discard {
                    reply(Reply::Error("Paste exceeds 8 MiB; paste less text".into()));
                } else {
                    self.bytes.truncate(self.bytes.len() - 6);
                    match String::from_utf8(std::mem::take(&mut self.bytes)) {
                        Ok(text) => event(Event::Paste(text)),
                        Err(_) => reply(Reply::Error(
                            "Paste is not UTF-8; paste plain text instead".into(),
                        )),
                    }
                }
                *self = Self::default();
            }
            return;
        }
        if self.bytes.len() == 2
            && matches!(
                self.bytes.as_slice(),
                b"\x1b]" | b"\x1bP" | b"\x1b_" | b"\x1b^"
            )
        {
            self.control = true;
            self.started = Some(std::time::Instant::now());
            return;
        }
        if self.bytes == b"\x1b[" {
            self.started = Some(std::time::Instant::now());
        }
        if self.bytes == b"\x1b[200~" {
            self.bytes.clear();
            self.paste = true;
            self.started = None;
            return;
        }
        if self.bytes.starts_with(b"\x1b[?5522;") && byte == b'y' {
            if let Ok(report) = std::str::from_utf8(&self.bytes)
                && let Some(status) = report
                    .strip_prefix("\x1b[?5522;")
                    .and_then(|s| s.strip_suffix("$y"))
                    .and_then(|s| s.parse().ok())
            {
                reply(Reply::PasteMode(status));
            }
            *self = Self::default();
            return;
        }
        // These prefixes belong to the outer framer, not the ordinary input parser.
        if self.bytes == b"\x1b"
            || b"\x1b[200~".starts_with(&self.bytes)
            || self.bytes.starts_with(b"\x1b[?")
        {
            if self.bytes.len() > 256 {
                self.bytes.clear();
                self.csi_discard = !(0x40..=0x7e).contains(&byte);
                if !self.csi_discard {
                    self.started = None;
                }
                reply(Reply::Error(
                    "Terminal CSI reply exceeds 256 bytes; retry the clipboard action".into(),
                ));
            } else if self.bytes.len() > 3 && (0x40..=0x7e).contains(&byte) {
                *self = Self::default();
            }
            return;
        }
        match terminput::Event::parse_from(&self.bytes) {
            Ok(Some(value)) => {
                if let Ok(value) = terminput_crossterm::to_crossterm(value) {
                    event(value);
                }
                *self = Self::default();
            }
            Ok(None) if self.bytes.len() < 256 => {}
            Ok(None) if self.bytes.starts_with(b"\x1b[") => {
                self.bytes.clear();
                self.csi_discard = true;
                reply(Reply::Error(
                    "Terminal CSI reply exceeds 256 bytes; retry the clipboard action".into(),
                ));
            }
            _ => *self = Self::default(),
        }
    }

    fn expire(&mut self, now: std::time::Instant, reply: &mut impl FnMut(Reply)) {
        if self.started.is_some_and(|started| {
            now.saturating_duration_since(started) >= std::time::Duration::from_secs(5)
        }) {
            *self = Self {
                quarantine: true,
                ..Self::default()
            };
            reply(Reply::Error("Incomplete terminal reply timed out; press Escape once to restore keyboard input, then retry".into()));
        }
    }

    fn escape_timeout(&mut self, event: &mut impl FnMut(Event), reply: &mut impl FnMut(Reply)) {
        if self.quarantine && self.escaped {
            *self = Self::default();
            reply(Reply::Error(
                "Keyboard input restored; retry the clipboard action".into(),
            ));
            return;
        }
        if self.bytes == b"\x1b" && !self.paste && !self.control {
            event(Event::Key(crossterm::event::KeyCode::Esc.into()));
            self.bytes.clear();
        }
    }
}

#[cfg(unix)]
pub fn reader(
    keys: tokio::sync::mpsc::UnboundedSender<Event>,
    replies: tokio::sync::mpsc::Sender<Reply>,
    controls: std::sync::mpsc::Receiver<crate::ReaderControl>,
) {
    use rustix::event::{PollFd, PollFlags, poll};
    use std::{io::IsTerminal, os::fd::AsFd};
    use std::{
        sync::{
            Arc,
            atomic::{AtomicBool, Ordering},
        },
        time::{Duration, Instant},
    };
    let resized = Arc::new(AtomicBool::new(false));
    // Match raw-mode ownership when stdin was redirected but a controlling TTY still exists.
    let controlling = if std::io::stdin().is_terminal() {
        None
    } else {
        match std::fs::File::open("/dev/tty") {
            Ok(file) => Some(file),
            Err(error) => {
                let _ = replies.try_send(Reply::Error(format!(
                    "Could not open terminal input: {error}; start Hive in a terminal"
                )));
                return;
            }
        }
    };
    let terminal_fd = match &controlling {
        Some(file) => file.as_fd(),
        None => rustix::stdio::stdin(),
    };
    let signal = signal_hook::flag::register(signal_hook::consts::SIGWINCH, resized.clone());
    let mut decoder = Decoder::default();
    let mut paused = false;
    let mut last_input = Instant::now();
    let mut emit = |event| {
        let _ = keys.send(event);
    };
    let mut overflow = false;
    let mut reply = |event: Option<Reply>| {
        // Never block pause acknowledgement on a full UI queue. After overflow, abort the
        // transaction before accepting more packets so dropped DATA cannot look like valid text.
        if overflow {
            if replies
                .try_send(Reply::Error(
                    "Clipboard reply queue overflowed; retry with less data".into(),
                ))
                .is_err()
            {
                return;
            }
            overflow = false;
        }
        if let Some(event) = event
            && replies.try_send(event).is_err()
        {
            overflow = true;
        }
    };
    while !keys.is_closed() {
        let control = if paused {
            controls.recv_timeout(Duration::from_millis(20)).ok()
        } else {
            controls.try_recv().ok()
        };
        match control {
            Some(crate::ReaderControl::Pause(ack)) => {
                paused = true;
                decoder = Decoder::default();
                let _ = ack.send(());
            }
            Some(crate::ReaderControl::Resume) => {
                paused = false;
                decoder = Decoder::default();
            }
            None => {}
        }
        if paused {
            continue;
        }
        reply(None);
        decoder.expire(Instant::now(), &mut |event| reply(Some(event)));
        if resized.swap(false, Ordering::Relaxed)
            && let Ok((w, h)) = crossterm::terminal::size()
        {
            emit(Event::Resize(w, h));
        }
        let mut fds = [PollFd::new(&terminal_fd, PollFlags::IN)];
        let timeout = rustix::event::Timespec {
            tv_sec: 0,
            tv_nsec: 20_000_000,
        };
        match poll(&mut fds, Some(&timeout)) {
            Ok(0) => {
                if last_input.elapsed() >= Duration::from_millis(40) {
                    decoder.escape_timeout(&mut emit, &mut |event| reply(Some(event)));
                }
            }
            Ok(_) => {
                let mut buffer = [0; 4096];
                match rustix::io::read(terminal_fd, &mut buffer) {
                    Ok(0) => break,
                    Ok(n) => {
                        last_input = Instant::now();
                        for byte in &buffer[..n] {
                            decoder.feed(*byte, &mut emit, &mut |event| reply(Some(event)));
                        }
                    }
                    Err(rustix::io::Errno::INTR) => {}
                    Err(error) => {
                        reply(Some(Reply::Error(format!(
                            "Terminal input failed: {error}; restart Hive"
                        ))));
                        break;
                    }
                }
            }
            Err(rustix::io::Errno::INTR) => {}
            Err(error) => {
                reply(Some(Reply::Error(format!(
                    "Terminal input polling failed: {error}; restart Hive"
                ))));
                break;
            }
        }
    }
    if let Ok(signal) = signal {
        signal_hook::low_level::unregister(signal);
    }
}

#[cfg(all(test, unix))]
mod tests {
    use super::*;

    #[test]
    fn truncated_replies_time_out_without_releasing_their_tail_as_keys() {
        let mut decoder = Decoder::default();
        let (mut events, mut replies) = (Vec::new(), Vec::new());
        for byte in b"\x1b]5522;type=read" {
            decoder.feed(*byte, &mut |e| events.push(e), &mut |r| replies.push(r));
        }
        decoder.expire(
            std::time::Instant::now() + std::time::Duration::from_secs(6),
            &mut |r| replies.push(r),
        );
        for byte in b"late payload\x1b\\" {
            decoder.feed(*byte, &mut |e| events.push(e), &mut |r| replies.push(r));
        }
        assert!(events.is_empty());
        decoder.feed(27, &mut |e| events.push(e), &mut |r| replies.push(r));
        decoder.escape_timeout(&mut |e| events.push(e), &mut |r| replies.push(r));
        decoder.feed(b'x', &mut |e| events.push(e), &mut |r| replies.push(r));
        assert_eq!(
            events,
            vec![Event::Key(crossterm::event::KeyCode::Char('x').into())]
        );
        assert_eq!(replies.len(), 2);
    }

    #[test]
    fn oversized_csi_replies_are_discarded_through_the_final_byte() {
        let mut decoder = Decoder::default();
        let (mut events, mut replies) = (Vec::new(), Vec::new());
        for byte in b"\x1b[?5522;"
            .iter()
            .copied()
            .chain(std::iter::repeat_n(b'1', 300))
            .chain(b"$yx".iter().copied())
        {
            decoder.feed(byte, &mut |e| events.push(e), &mut |r| replies.push(r));
        }
        assert_eq!(
            events,
            vec![Event::Key(crossterm::event::KeyCode::Char('x').into())]
        );
        assert!(matches!(&replies[..], [Reply::Error(_)]));
    }

    #[test]
    fn program_status_replies_are_demultiplexed_without_consuming_neighboring_input() {
        for end in ["\x07", "\x1b\\"] {
            let bytes = format!("a\x1b]7501;?:states=idle,working:kinds=permission{end}\x1b]5522;type=read:status=OK\x1b\\b").into_bytes();
            for split in 0..bytes.len() {
                let mut decoder = Decoder::default();
                let (mut events, mut replies) = (Vec::new(), Vec::new());
                for chunk in [&bytes[..split], &bytes[split..]] {
                    for byte in chunk {
                        decoder.feed(*byte, &mut |e| events.push(e), &mut |r| replies.push(r));
                    }
                }
                assert_eq!(
                    events,
                    vec![
                        Event::Key(crossterm::event::KeyCode::Char('a').into()),
                        Event::Key(crossterm::event::KeyCode::Char('b').into())
                    ]
                );
                assert!(matches!(
                    &replies[..],
                    [Reply::ProgramStatus(_, _), Reply::Clipboard(_)]
                ));
            }
        }
        let mut decoder = Decoder::default();
        let (mut events, mut replies) = (Vec::new(), Vec::new());
        for byte in b"\x1b]7501;state=working\x07\x1b[200~\x1b]7501;?\x07\x1b[201~" {
            decoder.feed(*byte, &mut |e| events.push(e), &mut |r| replies.push(r));
        }
        assert!(replies.is_empty());
        assert_eq!(events, vec![Event::Paste("\x1b]7501;?\x07".into())]);
    }

    #[test]
    fn clipboard_replies_never_become_keys_at_any_fragment_boundary() {
        let bytes = b"a\x1b]5522;type=read:status=OK\x1b\\b";
        for split in 0..bytes.len() {
            let mut decoder = Decoder::default();
            let (mut events, mut replies) = (Vec::new(), Vec::new());
            for chunk in [&bytes[..split], &bytes[split..]] {
                for byte in chunk {
                    decoder.feed(*byte, &mut |e| events.push(e), &mut |r| replies.push(r));
                }
            }
            assert_eq!(events.len(), 2);
            assert!(
                matches!(&replies[..], [Reply::Clipboard(packet)] if packet == b"type=read:status=OK")
            );
        }
    }

    #[test]
    fn bracketed_paste_keeps_escape_looking_content_as_text() {
        let mut decoder = Decoder::default();
        let (mut events, mut replies) = (Vec::new(), Vec::new());
        for byte in b"\x1b[200~\x1b]5522;hello\x07\x1b[201~" {
            decoder.feed(*byte, &mut |e| events.push(e), &mut |r| replies.push(r));
        }
        assert_eq!(events, vec![Event::Paste("\x1b]5522;hello\x07".into())]);
        assert!(replies.is_empty());
    }

    #[test]
    fn oversized_control_strings_are_discarded_through_the_terminator() {
        let mut decoder = Decoder::default();
        let (mut events, mut replies) = (Vec::new(), Vec::new());
        for byte in b"\x1b]5522;"
            .iter()
            .copied()
            .chain(std::iter::repeat_n(b'a', 20000))
            .chain(b"\x1b\\x".iter().copied())
        {
            decoder.feed(byte, &mut |e| events.push(e), &mut |r| replies.push(r));
        }
        assert_eq!(events.len(), 1);
        assert!(matches!(&replies[..], [Reply::Error(_)]));
    }
}
