# Warm turn wall measurement and phase diagnostics

`t1.turn.wall_budget` measures 5 warmups and 25 retained turns per shape with
`HAIDER_PHASE_TRACE_DIR` removed from the client and daemon launch environment.
Its accepted ceilings, load gate and 6,142,000 ms outer deadline are unchanged.
The opt-in recorder still retains at most 100,000 rows per process and counts
all further drops. A trace-capacity limit cannot affect this wall arm.

The standalone warm harness offers attribution through a **separate short run**
with a fresh profile and daemon: one warmup and one retained turn per shape.
Its wall, CPU and RSS samples never enter the measurement's summaries or budget
adjudication. Disable the extra diagnostic with `--no-phases`:

```sh
python3 scripts/qa-gate/turn_wall_harness.py --bin-dir /path/to/bins --output report.json
python3 scripts/qa-gate/turn_wall_harness.py --bin-dir /path/to/bins --no-phases
```

The warm report keeps schema `haider.turn-wall.v1`. Its measured rows have no
phase map; `parameters.phase_attribution` is false and `phase_summary` is null.
`attribution.status` is `not_requested`, `complete`, `truncated` or `unavailable`.
Requested attribution includes `mode: separate_short_run` and its own `report`,
including binary identities, samples, load, daemon identity and cleanup.
`complete` describes the record stream, not comprehensive instrumentation:
unobserved work remains in each map's residual.

A truncated or unavailable diagnostic has a typed `reason` and no phase
summary. Reasons include `record_capacity`, `incomplete_trace`,
`unsupported_schema`, `unsupported_clock`, `missing_process_traces`, `invalid_trace` and
`invalid_attribution`. The reader continues to reject incomplete traces rather
than computing attribution from a partial stream. Supported trace schemas
1–4 remain readable. A decoder's recursion limit is reported as `invalid_trace`;
readable additive header fields remain supported. Correctness failures from the
diagnostic still fail the overall run. Diagnostic sample-count/load rejection does not change the
measurement result; the measurement's own load rejection remains ENV_BLOCKED.

Cold one-shot mode retains its existing per-profile tracing behavior, since
each sample has its own daemon and its own bounded record stream.
