# tpsfix — a reliable tokens-per-second readout (lane-970-tpsfix)

Owner bug (screenshot, 2026-09-03): the streaming widget showed `0 tps · μ6`
while the model was *thinking*, flapped between `0` and `~5000 tps` while it
streamed, settled on `166 tps` at the end (a number that matched nothing), and
occupied ~40 columns for what is a glanceable ambient figure.

This note states the definition the estimator implements and why, then records
the three root causes it fixes.

## 1. Definition

### Output tokens

An **output token** is a token the model *generated* this turn: assistant text,
reasoning / thinking content, and tool-call argument fragments. Tool *results*
and command output are execution data, not generation, and are excluded.

This is deliberately the same set the providers meter:

| Provider | Field | Contents |
| --- | --- | --- |
| OpenAI (Responses) | `usage.output_tokens` | text + tool args + `output_tokens_details.reasoning_tokens` |
| OpenAI (Chat) | `usage.completion_tokens` | text + tool args + `completion_tokens_details.reasoning_tokens` |
| Anthropic | `usage.output_tokens` | text + tool args + thinking blocks |

`haider_protocol::provider::Usage::output` is populated from exactly those
fields (`crates/haider-provider/src/openai.rs:6069`, `:6107`), with `reasoning`
carried alongside as a *detail* sub-count, not an addend. So `Usage::output`
already is "all generated tokens" and the widget must not add `reasoning` to it.

### The clock: generation time, not turn time

`tps = output tokens ÷ generation time`, where **generation time starts at the
first output token, not at turn start**. Time-to-first-token (prompt
processing, queueing, provider backoff) is excluded.

This is the same split every local runtime reports:

* **llama.cpp** prints `prompt eval time` and `eval time` separately; its
  `eval time … (x ms per token, y tokens per second)` line divides `n_eval` by
  `t_eval`, which begins after the prompt is ingested.
* **Ollama** reports `eval_count / eval_duration` for generation and
  `prompt_eval_count / prompt_eval_duration` for the prefill; the "tokens/s"
  figure users quote is the former.

A single number that folded TTFT in would read as a property of the *prompt*,
not the *decoder*, and would swing wildly with cache hits. So: TTFT out.

Because the TUI samples on a frame clock rather than seeing individual tokens,
generation start is taken as the timestamp of the **last observation that still
had zero output** (the tightest upper bound on TTFT we can actually see), or
the turn's first observation when output was already present. Generation end is
the last observation at which the output count grew.

### Live rate

* Sliding window of **2 000 ms** over the output series.
* Suppressed unless the window spans **≥ 500 ms**. This is the guard that turns
  the owner's spike into a non-event: one 5 000-token delta 100 ms after the
  turn opened reads as 50 000 tps if you differentiate the pair directly, and as
  nothing at all until the window has aged half a second.
* Suppressed unless the window covers **≥ 8 tokens** — *unless* the window has
  fully aged (`span ≥ 2 000 ms`), in which case a genuinely slow or stopped
  stream is allowed through so the EMA decays instead of freezing at a stale
  high reading. That puts the measurable floor at 4 tok/s.
* Nothing at all is published before **generation has started** (see the
  warm-up rule below). Without this guard the aged-window escape above would
  itself publish `0 tps` two seconds into silent thinking — which is exactly the
  screenshot, and is how the test suite caught it.
* **EMA** smoothing, α = 0.4, recomputed at most every **250 ms** so the digits
  do not strobe at delta cadence.
* Floored at **1 tps** once a rate exists: a live turn never wears a bare
  `0 tps`. Zero is reserved for "not generating", which the warm-up form states
  in words rather than in a misleading number.

### Final rate

At turn end: `total output tokens ÷ (generation start → last growth)`, using the
provider's own final `Usage::output` for this turn when one was reported. When
the generation span is under 500 ms the last live value is kept rather than
dividing by a degenerate interval.

### μ (mean)

**Removed from the compact widget.** Once the turn settles, the widget's number
*is* the turn mean, so a `μ` beside it was a duplicate — and the widget's new
fixed budget (§3) has no room for it. `μ` and `p95` survive only on the verbose
`--plain` diagnostic row, where they are explicitly the mean / 95th percentile
of the **closed 5 s buckets** that also drive the sparkline — a distribution
figure, not a second headline rate.

## 2. Token counting: calibrated bytes, exact totals

Provider usage arrives as a **step function**: one `usage` envelope per physical
provider request, carrying a cumulative count. Differentiating that directly is
what produced the `0 … 5000 … 0` flap — three seconds of `Δ = 0` followed by one
tick of `Δ = 900`.

The estimator therefore separates *shape* from *scale*:

* **Shape** comes from the streamed-content character counter, which advances on
  every text / reasoning / tool-args delta and is therefore smooth.
* **Scale** is one number, `chars_per_token`, defaulting to **4.0** (the usual
  English-text ratio for BPE vocabularies) and **re-derived from every exact
  usage frame**, clamped to `[1.5, 12.0]`.

The re-derivation uses the **delta** since the last calibration anchor —
`Δchars ÷ Δusage.output` — not the absolute totals, and only once the delta
carries at least 64 characters. The absolute form was written first and the test
suite rejected it: a turn whose first usage frame also bills 900 reasoning
tokens the provider never streamed collapses `chars ÷ total` onto the clamp, and
every later frame inherits the distortion (the streaming phase then read 133
tok/s instead of 50). The delta isolates the phase actually being metered, so
the ratio converges on the model's real tokenizer within a frame or two.

Three consequences:

1. At the instant a usage frame lands, the scale matches the provider's own
   accounting — the real number replaces the estimate without a step, because
   the correction is applied to the *ratio*, not to the series.
2. The estimate self-corrects for the model's actual tokenizer, for CJK, for
   code, and for OpenAI-style reasoning *summaries* (few streamed characters,
   many billed tokens → `chars_per_token` falls).
3. A thin frame — one not backed by enough streamed characters — is recorded
   (the `~` drops, the total is kept) but never moves the ratio.

A readout that has never seen an exact frame wears the `~` marker, unchanged.

When a provider bills output tokens it never streamed (reasoning with no summary
at all), `chars` does not move: the window carries fewer than 8 tokens, the live
rate is suppressed, and the widget shows the thinking state with elapsed time
instead of `0 tps`. The tokens still land in the turn total, so the final figure
is right.

## 3. The widget

Fixed budget, left-anchored, **does not grow with the terminal** (owner,
2026-09-03: "about a quarter of its current width, roughly 100 px"):

```
 ▁▂▃▄▅▆  126 tps      1 margin + 6 spark + 1 gap + 4 rate + 4 unit = 16 columns
 ▁▂▃▄▅▆ ~126 tps      approximate — the `~` rides inside the 4-cell rate field
          ⋯ 3.2s      thinking / warm-up: elapsed, never `0 tps`
```

* sparkline: **6** columns (was 24) — at one 5 s bucket per column that is 30 s
  of history, which is what an ambient strip can usefully carry.
* rate field: **4** columns, right-aligned, saturating at `9999`, so the `tps`
  unit never shifts as digits come and go.
* `· μN` dropped (§1).

Nothing is rendered to the right of the widget on that row, so alignment
elsewhere is unaffected; the fixed budget means it is also stable across
terminal resizes.

## 4. Root causes fixed

1. **Stale usage read as this turn's total.** `Usage::output` is cumulative
   *within a run*. The projection kept the previous turn's frame, so a new turn
   opened by reading the old cumulative number, saw no regression, reported
   `0 tps`, and then jumped by the whole of the next usage frame in a single
   tick. Fixed by a per-turn epoch (`SessionProjection::turn_epoch`) that gates
   the usage frame: a frame from an earlier turn is *not* this turn's total.
2. **Differentiating a step function.** Fixed by §2 (shape from chars, scale
   from usage).
3. **`0 tps` during thinking.** The old tracker was fed a cumulative count of
   zero and dutifully divided it. Fixed by the warm-up phase: no output tokens
   yet ⇒ no rate, show elapsed time.

A fourth, smaller one: the old "final" number was simply the last 1 s windowed
sample left in the buffer — the `166 tps` in the screenshot was the rate of the
last second of the turn, not of the turn. It is now the turn's own mean.

## 5. Known limits

* The turn's generation span is wall-clock from first to last output token, so
  **tool-execution time inside a turn is inside the span** and drags the settled
  figure down. That is the definition the task specified, and it matches how
  llama.cpp / ollama report a single generation; a version that excluded
  non-generating gaps would report a decoder figure with no relationship to the
  turn the user watched.
* The frame clock samples at delta cadence while streaming and every 600 ms
  otherwise, so generation start is resolved to the last observation that still
  had zero output. On a turn that thinks silently for a long time and then
  streams, that is accurate to one 600 ms tick.
* The verbose `--plain` row still carries `μ` / `p95` over closed 5 s buckets.
  With the sparkline down to 6 columns those aggregates now appear after 20 s of
  generation rather than after 20 s of a 24-column ring.
