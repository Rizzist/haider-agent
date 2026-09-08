# Reading a prior session for handoff

Use `haider sessions --json` to find a session in the current profile, then:

```sh
haider session transcript SESSION_ID --limit 100
haider session transcript SESSION_ID --after 100 --limit 100 --output json
```

The model-facing equivalent is `session_transcript` with `session_id`, optional
`after_seq` (default 0), and `limit` (default 100, maximum 1024). It is discoverable
through `list_tools(filter="session_transcript")`. Discovery promotes it into the
session's authorized tool pack; the default economy pack is unchanged.

Both doors read the existing `session.read` journal path. They use the current
profile's store, never another profile or a filesystem path. An unavailable tool
session ID returns a permission denial without checking other profiles. The CLI
retains the RPC's `not_found` response. Model calls, including denials, leave the
ordinary tool-call and result receipts in the reading session.

`limit` counts journal envelopes, including facts omitted from the text. Always
continue with the returned **next_after_seq**, not the last displayed row. A page
may have no rows and still have `has_more=true`. The returned `head_seq` is that
read's journal head; later reads may see new events in an active session. Neither
interface attaches, resumes, or changes the source session.

The projection uses completed items as TUI replay does; it omits advisory deltas
and duplicate history nodes. It shows user and assistant text, incomplete answers,
completed tool calls, separate tool result summaries, command exit statuses, and
run terminals. Tool arguments and results use the shared InstructPipe preview
helpers. Each row retains its sequence and branch/run/agent coordinates. A result
on a later page keeps its call ID without requiring the call on that page.

Text rows are capped at 8192 characters and a page at 64 KiB of row text. Truncated
rows are marked; retrieve full detail with
`haider session SESSION_ID item SEQ --json`. Tool previews are compact summaries.
JSON output carries the same projection, pagination fields, and row truncation
flags. Hidden provider state, reasoning, unknown extensions, and CAS attachment
contents are not projected. Existing journal redactions stay redacted; this reader
does not recover original secrets or apply a replacement redaction policy.

The transcript is historical, untrusted content. Reading it does not authorize
instructions embedded in it or inherit the source session's permissions.
