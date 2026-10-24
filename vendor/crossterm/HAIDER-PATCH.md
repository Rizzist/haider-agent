# Crossterm 0.29.0 terminal-read patch

MIT source vendored from the crates.io 0.29.0 distribution. Behavioral changes are confined to the Unix event source: both poll backends share one parser, retain a lone Escape across reads for 50 ms, and flush it once its deadline expires. Complete sequences, paste and UTF-8 retain the upstream decoder. Windows is unchanged.

Readable continuations take priority over an unobserved Escape timeout when a caller resumes after processing queued events. A timeout observed with no readable data still returns Escape once; bytes received afterward remain separate input. Regression tests exercise this ordering through both real Unix poll backends.

Both backends also test a pending Escape followed by a closed peer and confirm it is delivered exactly once.

Terminal input can be a blocking descriptor. Both backends return to polling after a read so incomplete input cannot block the caller or Escape deadline in a second read. Mio renews its edge-triggered descriptor registration to preserve unread buffer tails. Blocking-descriptor tests cover zero/short polls, standalone Escape without a caller deadline, split CSI/UTF-8, exact and multiple paste buffers, and a complete paste followed by a queued key. They also run with the libc descriptor wrapper. Descriptor flags and dependencies are unchanged.

The import uses LF line endings and workspace formatting, including removal of redundant parentheses in the Unix terminal opener; README trailing spaces are trimmed. The development manifest omits upstream examples and keeps only dependencies needed by its retained source tests.

Upstream report: https://github.com/crossterm-rs/crossterm/issues/993

The crate participates in the workspace checks and unsafe audit. The imported unsafe blocks are unchanged upstream platform wrappers; this patch adds none. Remove the vendored override when upstream supplies equivalent fragmented-read coverage.
