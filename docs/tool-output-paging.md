# Reading orchestration output

`fs_read(path)` returns a numbered prefix: up to 64 KiB of content or 2,000
lines. The continuation at the end gives an exact `fs_read` call for the next
page. `offset` is a one-based line number and `limit` selects a line count.
An explicit line range is a complete request when that range fits. `column`
is a one-based character position in the first displayed, secret-redacted
line; it allows a long UTF-8 line to continue without dropping its middle.
Freshness hashes still cover the complete original file. Directory and search
inventories retain their separate bounds.

Ordinary previews preserve UUIDs, ULIDs, 32/40/64-digit hexadecimal identifiers
and digests, paths, and base64 encoding of those validated carriers. An exact,
call-local path allow-list also preserves conventionally tagged thread, run,
and session identifiers in explicitly requested files. Standard shell output
also preserves `HEAD:<40-hex>`, `urn:uuid:<UUID>`, `thread-<UUID>`, UUID filenames
with conventional lowercase extensions, and CIDv0 identifiers whose base58btc
decoding is a SHA-256 multihash. Mixed-case alphanumeric run IDs (26–64
characters) require the exact `run_id=` field; their alphabet alone grants no
exemption. This grants no file access and does not exempt unknown random values.
Known credential prefixes, secret assignments, bearer credentials and PEM
material remain redacted. URL userinfo passwords, including percent escapes,
are removed while the username stays visible. Quoted secret values consume
escaped quotes and backslashes, including LF/CRLF, through the actual closing
quote. One consumer carries this state across file lines and per-stream journal
chunks. Physical LF boundaries stay visible, with each nonempty secret line
replaced, so file line/column paging cannot reveal a continuation alone.
The quote scan window is 2 MiB of bytes, counting both quotes, escapes and line
endings. A closing quote at the last byte is accepted. If the window ends first,
the rest of that input/stream is suppressed (including later apparent closing
quotes), preserving only line boundaries. EOF also suppresses an unfinished
value. The consumer keeps constant-size state and never buffers the quoted
value; existing per-line stream buffering remains capped at 2 MiB. Secret
context always takes precedence over these identifier shapes.
Redaction runs before range selection. Lockdown keeps its historical strict
classifier and read path.

Foreground process previews allow 64 KiB, with explicit head/tail byte
accounting when reduction is necessary. The 1 MiB default execution capture
can therefore be retrieved in 16 pages, using at most half the default
32-request soft tranche. Neither the 64-request turn ceiling nor the process
execution limit is increased.

A process result supplies a `capture:…` handle. Call
`task_output(task_id="capture:…", cursor=0)` and follow `next_cursor` until
`exhausted` is true. Cursors count bytes in the stable secret-redacted UTF-8
capture. Invalid UTF-8 boundaries are rejected. Handles are scoped to the
session, with durable lookup through the recorded process signal and tool
result after restart. A process terminated by an output or time bound can
have uncaptured output; the execution result discloses that limit separately.
`exhausted` means the retained capture has been read. Cursor pages from
`task_output` reach the model intact even when the source stream was truncated;
the cursor never advances past content removed by a second preview reducer.
New background tasks also redact complete lines per stream before retention,
so their cursor pages, completion tails, byte counts and hashes describe the
same safe stream. Historical task facts retain their existing replay format.
While a background task is running, reads include a redacted snapshot of its
unfinished lines so prompts without newlines remain visible. A running
snapshot can change as those lines grow; stable full paging is available after
completion.

Raw captures remain in the existing owner-authorized CAS. They are not used
as model pages. Complete lines are redacted before live command output enters
the journal, including credentials split across OS read chunks. Terminal control
sequences are removed at this boundary; ordinary non-secret binary bytes remain
byte-exact in the journal. A partial
line is delayed until its newline or process completion. Unreasonably long
unterminated lines are suppressed conservatively at the process capture cap.

The real terminal regression uses throwaway profiles and synthetic secrets:
`python3 scripts/qa-gate/redaction_tui_regression.py --bin-dir <built-binaries> --evidence-dir <fresh-directory>`.
It types a real shell command, requires the complete output in a fresh PTY
frame, checks decoded journal bytes, and records ANSI frames and cleanup.
