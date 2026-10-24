mod parser;

#[cfg(feature = "use-dev-tty")]
pub(crate) mod tty;

#[cfg(not(feature = "use-dev-tty"))]
pub(crate) mod mio;

#[cfg(feature = "use-dev-tty")]
pub(crate) use self::tty::UnixInternalEventSource;

#[cfg(not(feature = "use-dev-tty"))]
pub(crate) use self::mio::UnixInternalEventSource;

#[cfg(test)]
mod read_boundary_tests {
    use super::UnixInternalEventSource;
    use crate::event::{source::EventSource, Event, InternalEvent, KeyCode};
    use crate::terminal::sys::file_descriptor::FileDesc;
    use std::{io::Write, os::unix::net::UnixStream, time::Duration};

    fn owned_input(input: UnixStream) -> FileDesc<'static> {
        #[cfg(not(feature = "libc"))]
        {
            FileDesc::Owned(input.into())
        }
        #[cfg(feature = "libc")]
        {
            use std::os::unix::io::IntoRawFd;
            FileDesc::new(input.into_raw_fd(), true)
        }
    }

    #[test]
    fn blocking_descriptor_flushes_bare_escape_without_waiting_for_another_byte() {
        let (input, mut output) = UnixStream::pair().unwrap();
        let (tx, rx) = std::sync::mpsc::channel();
        let worker = std::thread::spawn(move || {
            let mut source =
                UnixInternalEventSource::from_file_descriptor(owned_input(input)).unwrap();
            tx.send(source.try_read(None).unwrap()).unwrap();
        });
        output.write_all(b"\x1b").unwrap();
        let first = rx.recv_timeout(Duration::from_secs(1));
        // Release the unfixed blocking read before asserting, so a failed
        // regression does not leave a blocked test thread behind.
        if first.is_err() {
            output.write_all(b"x").unwrap();
        }
        worker.join().unwrap();
        assert_eq!(
            first,
            Ok(Some(InternalEvent::Event(Event::Key(KeyCode::Esc.into()))))
        );
    }

    fn blocking_split_poll(
        prefix: &[u8],
        suffix: &[u8],
        expected: InternalEvent,
        timeout: Duration,
    ) {
        let (input, mut output) = UnixStream::pair().unwrap();
        let (tx, rx) = std::sync::mpsc::channel();
        let (continue_tx, continue_rx) = std::sync::mpsc::channel();
        let worker = std::thread::spawn(move || {
            let mut source =
                UnixInternalEventSource::from_file_descriptor(owned_input(input)).unwrap();
            tx.send(source.try_read(Some(timeout)).unwrap()).unwrap();
            // The continuation is readable before the second poll, even
            // when the consumer is delayed by the scheduler.
            continue_rx.recv().unwrap();
            tx.send(source.try_read(Some(Duration::from_secs(1))).unwrap())
                .unwrap();
        });
        output.write_all(prefix).unwrap();
        let first = rx.recv_timeout(Duration::from_secs(1));
        // This also releases the old second read after a failed first poll.
        output.write_all(suffix).unwrap();
        continue_tx.send(()).unwrap();
        let second = rx.recv_timeout(Duration::from_secs(2));
        worker.join().unwrap();
        assert_eq!(first, Ok(None), "prefix={prefix:?}");
        assert_eq!(second, Ok(Some(expected)), "prefix={prefix:?}");
    }

    #[test]
    fn blocking_descriptor_respects_short_polls_and_preserves_partial_sequences() {
        for timeout in [Duration::ZERO, Duration::from_millis(10)] {
            for prefix in [b"\x1b".as_slice(), b"\x1b["] {
                blocking_split_poll(
                    prefix,
                    if prefix.len() == 1 { b"[B" } else { b"B" },
                    InternalEvent::Event(Event::Key(KeyCode::Down.into())),
                    timeout,
                );
            }
            blocking_split_poll(
                b"\xce",
                b"\xb2",
                InternalEvent::Event(Event::Key(KeyCode::Char('β').into())),
                timeout,
            );
        }
    }

    #[test]
    #[cfg(feature = "bracketed-paste")]
    fn blocking_descriptor_repolls_full_paste_buffers_and_retains_the_terminator() {
        for timeout in [Duration::ZERO, Duration::from_millis(10)] {
            for len in [1018, 2048, 3072] {
                let text = "a".repeat(len);
                let prefix = format!("\x1b[200~{text}");
                blocking_split_poll(
                    prefix.as_bytes(),
                    b"\x1b[201~",
                    InternalEvent::Event(Event::Paste(text)),
                    timeout,
                );
            }
        }
    }

    #[test]
    #[cfg(feature = "bracketed-paste")]
    fn blocking_descriptor_reads_complete_multibuffer_paste_and_queued_keys() {
        let (input, mut output) = UnixStream::pair().unwrap();
        let (tx, rx) = std::sync::mpsc::channel();
        let text = "b".repeat(3072);
        let bytes = format!("\x1b[200~{text}\x1b[201~x");
        let worker = std::thread::spawn(move || {
            let mut source =
                UnixInternalEventSource::from_file_descriptor(owned_input(input)).unwrap();
            tx.send([
                source.try_read(Some(Duration::from_secs(1))).unwrap(),
                source.try_read(Some(Duration::ZERO)).unwrap(),
            ])
            .unwrap();
        });
        output.write_all(bytes.as_bytes()).unwrap();
        let events = rx.recv_timeout(Duration::from_secs(2));
        worker.join().unwrap();
        assert_eq!(
            events,
            Ok([
                Some(InternalEvent::Event(Event::Paste(text))),
                Some(InternalEvent::Event(Event::Key(KeyCode::Char('x').into()))),
            ])
        );
    }

    #[test]
    fn readable_suffix_precedes_expiry_after_the_consumer_pauses() {
        let (input, mut output) = UnixStream::pair().unwrap();
        input.set_nonblocking(true).unwrap();
        let mut source = UnixInternalEventSource::from_file_descriptor(owned_input(input)).unwrap();
        output.write_all(b"x\x1b").unwrap();
        assert_eq!(
            source.try_read(Some(Duration::ZERO)).unwrap(),
            Some(InternalEvent::Event(Event::Key(KeyCode::Char('x').into())))
        );
        // The suffix is readable before the deadline. The caller is busy
        // processing the prefix, so the source must prefer it on resumption.
        output.write_all(b"[B").unwrap();
        std::thread::sleep(Duration::from_millis(80));
        assert_eq!(
            source.try_read(Some(Duration::ZERO)).unwrap(),
            Some(InternalEvent::Event(Event::Key(KeyCode::Down.into())))
        );
        assert_eq!(source.try_read(Some(Duration::ZERO)).unwrap(), None);
    }

    #[test]
    fn actual_poll_backend_retains_escape_after_zero_and_short_polls() {
        let (input, mut output) = UnixStream::pair().unwrap();
        input.set_nonblocking(true).unwrap();
        let mut source = UnixInternalEventSource::from_file_descriptor(owned_input(input)).unwrap();
        output.write_all(b"\x1b").unwrap();
        assert_eq!(source.try_read(Some(Duration::ZERO)).unwrap(), None);
        assert_eq!(
            source.try_read(Some(Duration::from_millis(1))).unwrap(),
            None
        );
        output.write_all(b"[B").unwrap();
        assert_eq!(
            source.try_read(Some(Duration::from_millis(100))).unwrap(),
            Some(InternalEvent::Event(Event::Key(KeyCode::Down.into())))
        );
        output.write_all(b"\x1b").unwrap();
        assert_eq!(
            source.try_read(Some(Duration::from_millis(100))).unwrap(),
            Some(InternalEvent::Event(Event::Key(KeyCode::Esc.into())))
        );
        assert_eq!(source.try_read(Some(Duration::ZERO)).unwrap(), None);
    }

    #[test]
    fn actual_poll_backend_flushes_pending_escape_when_the_peer_closes() {
        let (input, mut output) = UnixStream::pair().unwrap();
        input.set_nonblocking(true).unwrap();
        output.write_all(b"x\x1b").unwrap();
        drop(output);
        let (tx, rx) = std::sync::mpsc::channel();
        std::thread::spawn(move || {
            let mut source =
                UnixInternalEventSource::from_file_descriptor(owned_input(input)).unwrap();
            tx.send([
                source.try_read(Some(Duration::ZERO)).unwrap(),
                source.try_read(None).unwrap(),
                source.try_read(Some(Duration::ZERO)).unwrap(),
            ])
            .unwrap();
        });
        assert_eq!(
            rx.recv_timeout(Duration::from_secs(1)).unwrap(),
            [
                Some(InternalEvent::Event(Event::Key(KeyCode::Char('x').into()))),
                Some(InternalEvent::Event(Event::Key(KeyCode::Esc.into()))),
                None,
            ]
        );
    }

    #[test]
    fn actual_poll_backend_flushes_bare_escape_without_a_caller_deadline() {
        let (input, mut output) = UnixStream::pair().unwrap();
        input.set_nonblocking(true).unwrap();
        let (tx, rx) = std::sync::mpsc::channel();
        std::thread::spawn(move || {
            let mut source =
                UnixInternalEventSource::from_file_descriptor(owned_input(input)).unwrap();
            tx.send(source.try_read(None).unwrap()).unwrap();
        });
        output.write_all(b"\x1b").unwrap();
        assert_eq!(
            rx.recv_timeout(Duration::from_secs(1)).unwrap(),
            Some(InternalEvent::Event(Event::Key(KeyCode::Esc.into())))
        );
    }
}
