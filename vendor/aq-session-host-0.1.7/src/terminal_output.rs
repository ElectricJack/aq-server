//! Bounded tmux control-protocol bytes. A decoded tail is not renderer state.
use super::*;
pub const MAX_CONTROL_LINE: usize = 256 * 1024;
pub const MAX_CONTROL_READ: usize = 64 * 1024;
pub const MAX_CONTROL_EVENTS: usize = 64;
#[derive(PartialEq, Eq)]
pub enum ControlEvent {
    Bytes(Vec<u8>),
    Paused,
    Continued,
    LayoutChanged,
    Exited,
    CommandError,
    Attached,
}
impl std::fmt::Debug for ControlEvent {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Bytes(v) => f.debug_tuple("Bytes").field(&v.len()).finish(),
            Self::Paused => f.write_str("Paused"),
            Self::Continued => f.write_str("Continued"),
            Self::LayoutChanged => f.write_str("LayoutChanged"),
            Self::Exited => f.write_str("Exited"),
            Self::CommandError => f.write_str("CommandError"),
            Self::Attached => f.write_str("Attached"),
        }
    }
}
pub struct ControlDecoder {
    pane: Vec<u8>,
    pending: Vec<u8>,
    block: Option<Vec<u8>>,
    failed: bool,
}
impl ControlDecoder {
    pub fn new(pane: &str) -> Result<Self> {
        ensure!(
            pane.len() <= 32
                && pane
                    .strip_prefix('%')
                    .is_some_and(|v| !v.is_empty() && v.bytes().all(|b| b.is_ascii_digit())),
            "control pane identity"
        );
        Ok(Self {
            pane: pane.as_bytes().to_vec(),
            pending: vec![],
            block: None,
            failed: false,
        })
    }
    /// Strict single-batch decoding; a dense packet exceeding the event cap is
    /// still refused. Streaming callers should preserve unconsumed wire below.
    pub fn feed(&mut self, bytes: &[u8]) -> Result<Vec<ControlEvent>> {
        let (consumed, events) = self.feed_bounded(bytes)?;
        if consumed != bytes.len() {
            self.failed = true;
            self.pending.clear();
            bail!("control event count bound");
        }
        Ok(events)
    }
    /// Consume a prefix ending at a framing boundary, returning at most64
    /// events. The caller must retain/retry the exact unconsumed suffix before
    /// providing later bytes; no wire data is discarded or silently replayed.
    pub fn feed_bounded(&mut self, bytes: &[u8]) -> Result<(usize, Vec<ControlEvent>)> {
        ensure!(!self.failed, "control decoder is discontinuous");
        let result = self.consume(bytes);
        if result.is_err() {
            self.failed = true;
            self.pending.clear();
        }
        result
    }
    pub fn finish(&mut self) -> Result<()> {
        if self.failed || !self.pending.is_empty() || self.block.is_some() {
            self.failed = true;
            self.pending.clear();
            bail!("truncated control stream");
        }
        Ok(())
    }
    fn consume(&mut self, bytes: &[u8]) -> Result<(usize, Vec<ControlEvent>)> {
        ensure!(bytes.len() <= MAX_CONTROL_READ, "control read byte bound");
        let mut events = Vec::new();
        let mut consumed = 0;
        for piece in bytes.split_inclusive(|b| *b == b'\n') {
            if events.len() == MAX_CONTROL_EVENTS {
                break;
            }
            ensure!(
                self.pending.len() + piece.len() <= MAX_CONTROL_LINE,
                "control line byte bound"
            );
            self.pending.extend_from_slice(piece);
            consumed += piece.len();
            if piece.last() != Some(&b'\n') {
                continue;
            }
            let mut line = std::mem::take(&mut self.pending);
            line.pop();
            if let Some(event) = self.line(&line)? {
                events.push(event);
            }
        }
        Ok((consumed, events))
    }
    fn line(&mut self, line: &[u8]) -> Result<Option<ControlEvent>> {
        if let Some(header) = line.strip_prefix(b"%begin ") {
            ensure!(self.block.is_none(), "nested control block");
            validate_header(header)?;
            self.block = Some(header.to_vec());
            return Ok(None);
        }
        for prefix in [b"%end ".as_slice(), b"%error ".as_slice()] {
            if let Some(header) = line.strip_prefix(prefix) {
                validate_header(header)?;
                ensure!(
                    self.block.as_deref() == Some(header),
                    "control block boundary mismatch"
                );
                self.block = None;
                return Ok(Some(if prefix == b"%error " {
                    ControlEvent::CommandError
                } else {
                    ControlEvent::Attached
                }));
            }
        }
        if self.block.is_some() {
            return Ok(None);
        }
        if let Some(rest) = line.strip_prefix(b"%output ") {
            let (pane, value) = field(rest)?;
            self.check_pane(pane)?;
            return Ok(Some(ControlEvent::Bytes(unescape(value)?)));
        }
        if let Some(rest) = line.strip_prefix(b"%extended-output ") {
            let (pane, rest) = field(rest)?;
            self.check_pane(pane)?;
            let (age, rest) = field(rest)?;
            number(age)?;
            let value = if let Some(value) = rest.strip_prefix(b": ") {
                value
            } else {
                let at = rest
                    .windows(3)
                    .position(|v| v == b" : ")
                    .context("extended output separator")?;
                ensure!(at <= 128, "extended output header bound");
                &rest[at + 3..]
            };
            return Ok(Some(ControlEvent::Bytes(unescape(value)?)));
        }
        for (prefix, event) in [
            (b"%pause ".as_slice(), ControlEvent::Paused),
            (b"%continue ".as_slice(), ControlEvent::Continued),
        ] {
            if let Some(pane) = line.strip_prefix(prefix) {
                self.check_pane(pane)?;
                return Ok(Some(event));
            }
        }
        if line == b"%exit" || line.starts_with(b"%exit ") {
            return Ok(Some(ControlEvent::Exited));
        }
        if line.starts_with(b"%layout-change ") {
            return Ok(Some(ControlEvent::LayoutChanged));
        }
        ensure!(
            line.is_empty()
                || (line.first() == Some(&b'%') && !line.iter().any(|b| *b < 0x20 || *b == 0x7f)),
            "unexpected control framing"
        );
        Ok(None)
    }
    fn check_pane(&self, pane: &[u8]) -> Result<()> {
        ensure!(pane == self.pane, "foreign control pane");
        Ok(())
    }
}
fn field(bytes: &[u8]) -> Result<(&[u8], &[u8])> {
    let at = bytes
        .iter()
        .position(|b| *b == b' ')
        .context("control field missing")?;
    ensure!(at > 0, "empty control field");
    Ok((&bytes[..at], &bytes[at + 1..]))
}
fn number(bytes: &[u8]) -> Result<u64> {
    ensure!(
        !bytes.is_empty() && bytes.len() <= 20 && bytes.iter().all(u8::is_ascii_digit),
        "control numeric field"
    );
    std::str::from_utf8(bytes)?
        .parse()
        .context("control number overflow")
}
fn validate_header(bytes: &[u8]) -> Result<()> {
    let values: Vec<_> = bytes.split(|b| *b == b' ').collect();
    ensure!(values.len() == 3, "control block fields");
    for value in values {
        number(value)?;
    }
    Ok(())
}
fn unescape(bytes: &[u8]) -> Result<Vec<u8>> {
    let mut out = Vec::new();
    let mut at = 0;
    while at < bytes.len() {
        let byte = bytes[at];
        if byte == b'\\' {
            let digits = bytes
                .get(at + 1..at + 4)
                .context("truncated control escape")?;
            ensure!(
                digits.iter().all(|b| (b'0'..=b'7').contains(b)),
                "invalid control escape"
            );
            let value = (u16::from(digits[0] - b'0') << 6)
                | (u16::from(digits[1] - b'0') << 3)
                | u16::from(digits[2] - b'0');
            ensure!(value <= 255, "control escape overflow");
            out.push(value as u8);
            at += 4;
        } else {
            ensure!(byte >= 0x20 && byte != 0x7f, "unescaped control byte");
            out.push(byte);
            at += 1;
        }
        ensure!(
            out.len() <= MAX_CONTROL_READ,
            "decoded control output bound"
        );
    }
    Ok(out)
}

/// A guardian-owned read-only client, not renderer state or stop proof by itself.
/// The guardian holds stdin without sending commands. Only a drain to EAGAIN can
/// renew its independent short lease; loss is one-shot and never reattaches.
pub struct OutputObserver {
    terminal: terminal_io::TerminalHandle,
    lease: Option<reader_lease::Client>,
    stdout: File,
    wire: Vec<u8>,
    wire_offset: usize,
    decoder: ControlDecoder,
    process: ProcessEvidence,
    failed: bool,
    attached: bool,
    drained_boot_ms: Option<u64>,
}
impl OutputObserver {
    pub fn open(terminal: &terminal_io::TerminalHandle) -> Result<Self> {
        let decoder = ControlDecoder::new(&terminal.binding().pane.pane_id)?;
        terminal.output_settings()?;
        let claim = terminal.claim_output()?;
        let (lease, stdout, process) = reader_lease::Client::open(terminal, claim)?;
        Ok(Self {
            terminal: terminal.clone(),
            lease: Some(lease),
            stdout,
            wire: Vec::with_capacity(MAX_CONTROL_READ),
            wire_offset: 0,
            decoder,
            process,
            failed: false,
            attached: false,
            drained_boot_ms: None,
        })
    }
    pub(crate) fn drained_after(&self, after: u64) -> bool {
        self.is_attached()
            && self.drained_boot_ms.is_some_and(|t| t >= after)
            && self.buffered_wire_bytes() == 0
    }
    pub fn process(&self) -> &ProcessEvidence {
        &self.process
    }
    pub fn is_attached(&self) -> bool {
        self.attached && !self.failed
    }
    pub fn terminal(&self) -> &aq_session_protocol::TerminalIdentity {
        &self.terminal.binding().terminal
    }
    pub fn policy(&self) -> Result<aq_session_protocol::TerminalPolicy> {
        Ok(self.terminal.output_settings()?.2)
    }
    /// At most one64KiB nonblocking read. A caller must keep draining while
    /// network I/O is stalled; bounded queues may discard sequenced tail data.
    pub fn poll(&mut self) -> Result<Vec<ControlEvent>> {
        ensure!(!self.failed, "output observer is discontinuous");
        let result = self.read_once();
        if result.is_err() {
            self.failed = true;
            self.close();
        }
        result
    }
    fn read_once(&mut self) -> Result<Vec<ControlEvent>> {
        self.terminal.output_settings()?;
        self.lease
            .as_ref()
            .context("reader lease closed")?
            .check()?;
        let events = self.read_bounded()?;
        if events.iter().any(|e| matches!(e, ControlEvent::Attached)) {
            self.attached = true;
        }
        self.terminal.output_settings()?;
        if events.iter().any(|e| {
            matches!(
                e,
                ControlEvent::Paused | ControlEvent::Exited | ControlEvent::CommandError
            )
        }) {
            self.failed = true;
            self.close();
        }
        Ok(events)
    }
    /// Unconsumed wire from a single bounded read, never a second authority.
    pub fn buffered_wire_bytes(&self) -> usize {
        self.wire.len() - self.wire_offset
    }
    fn read_bounded(&mut self) -> Result<Vec<ControlEvent>> {
        if self.wire_offset == self.wire.len() {
            self.wire.clear();
            self.wire_offset = 0;
            let mut bytes = [0u8; MAX_CONTROL_READ];
            match self.stdout.read(&mut bytes) {
                Ok(0) => {
                    self.decoder.finish()?;
                    return Ok(vec![ControlEvent::Exited]);
                }
                Ok(n) => {
                    self.drained_boot_ms = None;
                    self.wire.extend_from_slice(&bytes[..n]);
                }
                Err(e) if e.kind() == std::io::ErrorKind::WouldBlock => {
                    self.drained_boot_ms = Some(ownership::raw_boot_ms());
                    // A retained dense-packet suffix must never renew this lease.
                    self.lease
                        .as_mut()
                        .context("reader lease closed")?
                        .drained()?;
                    return Ok(vec![]);
                }
                Err(e) if e.kind() == std::io::ErrorKind::Interrupted => return Ok(vec![]),
                Err(_) => bail!("output observer read failed"),
            }
        }
        let (consumed, events) = self.decoder.feed_bounded(&self.wire[self.wire_offset..])?;
        ensure!(
            consumed > 0 && consumed <= self.buffered_wire_bytes(),
            "control continuation progress"
        );
        self.wire_offset += consumed;
        Ok(events)
    }
    fn close(&mut self) {
        self.lease.take();
    }
}
impl Drop for OutputObserver {
    fn drop(&mut self) {
        self.close();
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn chunks_preserve_unicode_ansi_octal_and_literal_backslash_without_interpretation() {
        let wire = b"%begin 1 2 0\n%output command-body\n%end 1 2 0\n%output %0 \\033[?1049h\\351\\233\\252\\360\\237\\231\\202\\134\\015\\012\n";
        for size in [1, 2, 7, wire.len()] {
            let mut decoder = ControlDecoder::new("%0").unwrap();
            let mut bytes = vec![];
            for chunk in wire.chunks(size) {
                for event in decoder.feed(chunk).unwrap() {
                    if let ControlEvent::Bytes(v) = event {
                        bytes.extend(v);
                    }
                }
            }
            decoder.finish().unwrap();
            assert_eq!(bytes, "\x1b[?1049h雪🙂\\\r\n".as_bytes());
        }
    }
    #[test]
    fn malformed_foreign_and_oversized_streams_fail_closed() {
        for wire in [
            b"%output %1 private\n".as_slice(),
            b"%output %0 \\999\n",
            b"%output %0 \\400\n",
            b"%end 1 2 0\n",
        ] {
            let mut decoder = ControlDecoder::new("%0").unwrap();
            assert!(decoder.feed(wire).is_err());
            assert!(decoder.feed(b"%output %0 later\n").is_err());
        }
        let mut decoder = ControlDecoder::new("%0").unwrap();
        for _ in 0..4 {
            decoder.feed(&vec![b'x'; MAX_CONTROL_READ]).unwrap();
        }
        assert!(decoder.feed(b"x").is_err());
    }
    #[test]
    fn extended_output_and_pause_are_explicit_not_silent_recovery() {
        let mut decoder = ControlDecoder::new("%0").unwrap();
        let events = decoder
            .feed(b"%extended-output %0 12 : hi\\012\n%pause %0\n%continue %0\n%exit lost\n")
            .unwrap();
        assert!(matches!(&events[0], ControlEvent::Bytes(v) if v == b"hi\n"));
        assert!(matches!(events[1], ControlEvent::Paused));
        assert!(matches!(events[2], ControlEvent::Continued));
        assert!(matches!(events[3], ControlEvent::Exited));
    }
    #[test]
    fn dense_packet_continuation_preserves_every_event_and_escape_boundary() {
        let line =
            b"%output %0 \\033[?1049h\\351\\233\\252\\360\\237\\231\\202\\134\\033[?1049l\\012\n";
        let wire = line.repeat(1000);
        let expected = "\x1b[?1049h雪🙂\\\x1b[?1049l\n".as_bytes().repeat(1000);
        for size in (1..=line.len()).chain([127, 4096, MAX_CONTROL_READ]) {
            let mut decoder = ControlDecoder::new("%0").unwrap();
            let mut decoded = vec![];
            let mut count = 0;
            for chunk in wire.chunks(size) {
                let mut suffix = chunk;
                while !suffix.is_empty() {
                    let (consumed, events) = decoder.feed_bounded(suffix).unwrap();
                    assert!(consumed > 0 && consumed <= suffix.len());
                    assert!(events.len() <= MAX_CONTROL_EVENTS);
                    count += events.len();
                    for event in events {
                        let ControlEvent::Bytes(bytes) = event else {
                            panic!("wrong event");
                        };
                        decoded.extend(bytes);
                    }
                    suffix = &suffix[consumed..];
                }
            }
            decoder.finish().unwrap();
            assert_eq!(count, 1000);
            assert_eq!(decoded, expected);
        }
        // The original strict API keeps its exact fail-closed event limit.
        let mut strict = ControlDecoder::new("%0").unwrap();
        assert!(strict.feed(&line.repeat(MAX_CONTROL_EVENTS + 1)).is_err());
        assert!(strict.feed(line).is_err());
    }
    #[test]
    fn bounded_continuation_does_not_widen_line_decoded_byte_or_eof_bounds() {
        let mut line = ControlDecoder::new("%0").unwrap();
        for _ in 0..4 {
            line.feed_bounded(&vec![b'x'; MAX_CONTROL_READ]).unwrap();
        }
        assert!(line.feed_bounded(b"x").is_err());
        assert!(line.feed_bounded(b"\n").is_err());
        let mut oversized = b"%output %0 ".to_vec();
        oversized.extend(vec![b'x'; MAX_CONTROL_READ + 1]);
        oversized.push(b'\n');
        let mut bytes = ControlDecoder::new("%0").unwrap();
        assert!(
            oversized
                .chunks(MAX_CONTROL_READ)
                .any(|p| bytes.feed_bounded(p).is_err())
        );
        let mut eof = ControlDecoder::new("%0").unwrap();
        eof.feed_bounded(b"%output %0 \\03").unwrap();
        assert!(eof.finish().is_err());
        assert!(eof.feed_bounded(b"3\n").is_err());
    }
}
