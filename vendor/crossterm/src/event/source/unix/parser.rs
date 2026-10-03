use std::{
    collections::VecDeque,
    time::{Duration, Instant},
};

use crate::event::{sys::unix::parse::parse_event, Event, InternalEvent, KeyCode};

// A read boundary is not a key boundary. Only a lone Escape is ambiguous;
// partial CSI, UTF-8 and bracketed paste keep the upstream buffering rules.
const ESCAPE_TIMEOUT: Duration = Duration::from_millis(50);

#[derive(Debug)]
pub(super) struct Parser {
    buffer: Vec<u8>,
    internal_events: VecDeque<InternalEvent>,
    escape_deadline: Option<Instant>,
}

impl Default for Parser {
    fn default() -> Self {
        Self {
            buffer: Vec::with_capacity(256),
            internal_events: VecDeque::with_capacity(128),
            escape_deadline: None,
        }
    }
}

impl Parser {
    pub(super) fn advance(&mut self, bytes: &[u8], more: bool) {
        self.advance_at(bytes, more, Instant::now());
    }

    fn advance_at(&mut self, bytes: &[u8], more: bool, now: Instant) {
        // Both poll backends check readable data before expiring Escape.
        // Draining queued events may delay this call past the deadline even
        // though the continuation is already waiting in the descriptor.
        if !bytes.is_empty() {
            self.escape_deadline = None;
        }
        for (index, byte) in bytes.iter().enumerate() {
            self.buffer.push(*byte);
            let more = index + 1 < bytes.len() || more || self.buffer == b"\x1b";
            match parse_event(&self.buffer, more) {
                Ok(Some(event)) => {
                    self.internal_events.push_back(event);
                    self.buffer.clear();
                }
                Ok(None) => {}
                Err(_) => self.buffer.clear(),
            }
        }
        self.escape_deadline = if self.buffer == b"\x1b" {
            self.escape_deadline.or(Some(now + ESCAPE_TIMEOUT))
        } else {
            None
        };
    }

    pub(super) fn poll_timeout(&self, caller: Option<Duration>) -> Option<Duration> {
        self.poll_timeout_at(caller, Instant::now())
    }

    fn poll_timeout_at(&self, caller: Option<Duration>, now: Instant) -> Option<Duration> {
        match self.escape_deadline {
            Some(deadline) => {
                let remaining = deadline.saturating_duration_since(now);
                Some(caller.map_or(remaining, |duration| duration.min(remaining)))
            }
            None => caller,
        }
    }

    pub(super) fn finish_pending_escape(&mut self) -> Option<InternalEvent> {
        self.finish_pending_escape_at(Instant::now())
    }

    fn finish_pending_escape_at(&mut self, now: Instant) -> Option<InternalEvent> {
        if matches!(self.escape_deadline, Some(deadline) if now >= deadline) {
            self.escape_deadline = None;
            self.buffer.clear();
            Some(InternalEvent::Event(Event::Key(KeyCode::Esc.into())))
        } else {
            None
        }
    }
}

impl Iterator for Parser {
    type Item = InternalEvent;

    fn next(&mut self) -> Option<Self::Item> {
        self.internal_events.pop_front()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn readable_suffix_precedes_expiry_while_prefix_events_are_queued() {
        let now = Instant::now();
        let mut whole = Parser::default();
        whole.advance_at(b"/theme\r\x1b[B", false, now);
        let expected: Vec<_> = whole.collect();
        let mut split = Parser::default();
        split.advance_at(b"/theme\r\x1b", false, now);
        split.advance_at(b"[B", false, now + Duration::from_secs(1));
        let actual: Vec<_> = split.by_ref().collect();
        assert_eq!(actual, expected);
        assert_eq!(
            split.finish_pending_escape_at(now + Duration::from_secs(1)),
            None
        );
    }

    #[test]
    fn all_two_and_three_read_byte_cuts_match_whole_sequence() {
        let variants: &[&[u8]] = &[
            b"/theme\r\x1b[B\x1b[B",
            b"\x1b[27uhi",
            b"@ab\x1b[200~cd\x1b[201~",
            b"@ab\x1b[<35;2;2Mcd",
            b"@ab\x1b[<65;2;2Mcd",
            b"\x1b[B\x1b[B\x1b[B",
            b"\x1bOB\x1bOA\x1bOP",
            b"\x1b[1;5A\x1b[97;5u\x1b[Z",
            "@aβ漢".as_bytes(),
            "\x1bβx".as_bytes(),
            b"\x1bx\x03\t\r\x7f",
            b"\x1b[200~a\x1b[B\x1b[<65;2;2M\x1b[201~",
        ];
        let now = Instant::now();
        for bytes in variants {
            let mut whole = Parser::default();
            whole.advance_at(bytes, false, now);
            let expected: Vec<_> = whole.collect();
            assert!(!expected.is_empty());
            for first in 1..bytes.len() {
                for second in first..bytes.len() {
                    let mut parser = Parser::default();
                    parser.advance_at(&bytes[..first], false, now);
                    let mut actual: Vec<_> = parser.by_ref().collect();
                    parser.advance_at(
                        &bytes[first..second],
                        false,
                        now + Duration::from_millis(10),
                    );
                    actual.extend(parser.by_ref());
                    parser.advance_at(&bytes[second..], false, now + Duration::from_millis(20));
                    actual.extend(parser);
                    assert_eq!(actual, expected, "bytes={bytes:?} cuts={first}/{second}");
                }
            }
        }
    }

    #[test]
    fn lone_escape_deadline_survives_short_and_zero_polls_and_flushes_once() {
        let now = Instant::now();
        let mut parser = Parser::default();
        parser.advance_at(b"x\x1b", false, now);
        assert_eq!(
            parser.next(),
            Some(InternalEvent::Event(Event::Key(KeyCode::Char('x').into())))
        );
        assert_eq!(parser.next(), None);
        assert_eq!(parser.poll_timeout_at(None, now), Some(ESCAPE_TIMEOUT));
        assert_eq!(
            parser.poll_timeout_at(Some(Duration::ZERO), now),
            Some(Duration::ZERO)
        );
        assert_eq!(
            parser.poll_timeout_at(Some(Duration::from_millis(5)), now),
            Some(Duration::from_millis(5))
        );
        assert_eq!(
            parser.finish_pending_escape_at(now + ESCAPE_TIMEOUT - Duration::from_nanos(1)),
            None
        );
        assert_eq!(
            parser.finish_pending_escape_at(now + ESCAPE_TIMEOUT),
            Some(InternalEvent::Event(Event::Key(KeyCode::Esc.into())))
        );
        assert_eq!(parser.finish_pending_escape_at(now + ESCAPE_TIMEOUT), None);
        assert_eq!(parser.poll_timeout_at(None, now + ESCAPE_TIMEOUT), None);
        parser.advance_at(b"[Bx", false, now + ESCAPE_TIMEOUT);
        assert_eq!(
            parser.count(),
            3,
            "late suffix is ordinary text, never an arrow"
        );
    }

    #[test]
    fn continuation_cancels_deadline_but_partial_sequences_and_paste_do_not_expire() {
        let now = Instant::now();
        for prefix in [b"\x1b[".as_slice(), b"\xce", b"\x1b[200~a\x1b"] {
            let mut parser = Parser::default();
            parser.advance_at(prefix, false, now);
            assert_eq!(parser.poll_timeout_at(None, now), None);
            assert_eq!(
                parser.finish_pending_escape_at(now + Duration::from_secs(1)),
                None
            );
            assert_eq!(parser.buffer, prefix);
        }
        let mut parser = Parser::default();
        parser.advance_at(b"\x1b", false, now);
        parser.advance_at(b"[", false, now + Duration::from_millis(49));
        assert_eq!(parser.poll_timeout_at(None, now), None);
        parser.advance_at(b"B", false, now + Duration::from_secs(1));
        assert_eq!(
            parser.next(),
            Some(InternalEvent::Event(Event::Key(KeyCode::Down.into())))
        );
        assert_eq!(
            parser.finish_pending_escape_at(now + Duration::from_secs(1)),
            None
        );

        let mut parser = Parser::default();
        parser.advance_at(b"\x1b", false, now);
        parser.advance_at(b"[B\x1b", false, now + Duration::from_millis(49));
        assert_eq!(
            parser.poll_timeout_at(None, now + Duration::from_millis(49)),
            Some(ESCAPE_TIMEOUT)
        );
        assert_eq!(
            parser.finish_pending_escape_at(now + ESCAPE_TIMEOUT),
            None,
            "a second Escape gets its own deadline"
        );
        assert_eq!(
            parser.finish_pending_escape_at(now + Duration::from_millis(49) + ESCAPE_TIMEOUT),
            Some(InternalEvent::Event(Event::Key(KeyCode::Esc.into())))
        );
    }

    #[test]
    fn readable_alt_key_precedes_unobserved_escape_expiry() {
        let now = Instant::now();
        let mut parser = Parser::default();
        parser.advance_at(b"\x1b", false, now);
        parser.advance_at(b"x", false, now + ESCAPE_TIMEOUT);
        assert_eq!(
            parser.next(),
            Some(InternalEvent::Event(Event::Key(
                crate::event::KeyEvent::new(KeyCode::Char('x'), crate::event::KeyModifiers::ALT,)
            )))
        );
        assert_eq!(parser.next(), None);
        assert_eq!(parser.finish_pending_escape_at(now + ESCAPE_TIMEOUT), None);
    }

    #[test]
    fn large_paste_is_opaque_across_tty_buffer_and_utf8_boundaries() {
        let text = format!("{}β漢\x1b[B\x03\r\n{}", "a".repeat(1017), "z".repeat(1024));
        let bytes = format!("\x1b[200~{text}\x1b[201~").into_bytes();
        let now = Instant::now();
        for (first, second) in [
            (1024, 1025),
            (1024, 2048),
            (bytes.len() - 6, bytes.len() - 5),
        ] {
            let mut parser = Parser::default();
            parser.advance_at(&bytes[..first], true, now);
            assert_eq!(parser.next(), None);
            parser.advance_at(
                &bytes[first..second],
                false,
                now + Duration::from_millis(10),
            );
            assert_eq!(parser.next(), None);
            parser.advance_at(&bytes[second..], false, now + Duration::from_millis(20));
            assert_eq!(
                parser.next(),
                Some(InternalEvent::Event(Event::Paste(text.clone())))
            );
            assert_eq!(parser.next(), None);
        }
    }
}
