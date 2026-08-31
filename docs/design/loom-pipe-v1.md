# Loom pipe/v1 — the workflow DSL

The authoring model never emits a workflow DAG as JSON. It emits **pipe
source**: one terse line per node. The harness parses the source **locally**
into an AST and compiles the AST into typed runtime nodes. Same information,
roughly a quarter of the tokens (measured −72% vs the equivalent JSON on the
make-video graph), and a malformed line is a local parse error — never a
silently wrong graph.

The pipe source is the workflow's **structure of record**: the compiled node
list is a derived artifact, regenerated from the source at load. A registered
workflow is immutable at its revision; authoring a change mints a new rev.

## Grammar

```
workflow   = header NL node+
header     = name ":" ws in-type ws "->" ws out-type
node       = name [ws "@" agent-type] [ws quoted-task] [ws ":" gate] [ws deps] [ws red]
name       = [A-Za-z0-9_-]+
agent-type = [A-Za-z0-9_-]+          ; must resolve in the Loom registry
quoted-task= '"' [^"]* '"'
gate       = "cmd" | "ship" | "human" | "all-of-" N
deps       = "<-" name ("," name)*   ; explicit incoming green dependencies
red        = "↻" | (("↺" | "^") name) ; conditional self-loop or back-edge
comment    = "#" ...                 ; whole-line, ignored
```

- A node **with** `@agent-type` is a **work node**: a capability-scoped
  specialist that transforms the artifact. Its typed I/O signature is
  **derived from the registry** (`AgentType.in → AgentType.out`), never
  authored in the source. At runtime the daemon joins the pinned workflow's
  current ready node to this immutable metadata, verifies the exact durable
  required-CLI install job, and applies the specialist role, tool grant, and
  CLI scope before provider dispatch. Model-authored spawn arguments are not
  the authority and cannot omit or substitute the node's declared type.
- A node **without** `@agent-type` is a **control node**: it only gates —
  identity on its complete input artifact, including a merged JOIN input.
- A node with no `<-...` clause preserves the original pipe/v1 shorthand: the
  first node has no dependency and every later node depends on the previous
  source line. An explicit clause overrides that shorthand. Reusing one node as
  the dependency of multiple later nodes is a **FORK**; listing multiple names
  on one node is a **JOIN**. Dependency names are unique and must target earlier
  source lines, so the source itself stays in deterministic topological order.
- Gate default is `cmd`. Lowering (onto the CG gate vocabulary):
  | gate | compiles to | green when |
  |---|---|---|
  | `cmd` | CommandGreen | the node's command/child exits 0 |
  | `ship` | CommandGreen | INTERIM: a reviewer child is still a child whose gate exits green; a dedicated reviewer gate can widen this lowering without touching sources |
  | `all-of-N` | AllOfN{n} · N ≤ 8 | every one of N fan-out dimensions is green (N is bounded by the per-attempt evidence budget — a wider N would be a never-green node) |
  | `human` | HumanConfirm | the human approves (the run parks on a menu) |
- `↻` declares a conditional self-loop. `↺target` (ASCII alias
  `^target`) declares a conditional back-edge to an earlier dependency
  ancestor; forward, unrelated, and unknown targets are rejected. On a red
  terminal condition the runtime opens a new bounded hop at that exact target,
  invalidates the target and only its dependency descendants, and preserves
  unrelated fork branches. Green opens every dependent whose full dependency
  set is green; human gates hold without traversing; the workflow is `done`
  when every terminal branch is satisfied. The immutable compiled graph stores
  the target so a later registry revision cannot change a pinned run.

## Example (the flagship)

```
make-video: SourceURL -> VideoFile
research @researcher "pull the source and transcribe it" :cmd
propose  @proposer   "shape a hook and a 6-beat arc"     :ship
capture  @capturer   "gather b-roll for every beat"      :all-of-6 ↺propose
edit     @editor     "cut to the arc, trim dead air"     :ship <-propose,capture ↺capture
render   @renderer   "encode 1080p H.264"                :cmd
publish              "you approve the cut"               :human
```

A minimal diamond uses the same clause for both directions:

```
diamond: Seed -> Result
start
left  @left  <-start
right @right <-start
join  @join  <-left,right
```

`left` and `right` become ready together after `start`; `join` becomes ready
only after both are green.

## Contract

- **Parse** (`pipe source → AST`) is pure and total: errors are collected on
  the AST (`errors: Vec<String>`), never thrown. An AST with errors is
  rejected at registration, not at run.
- **Compile** (`AST → nodes`) resolves gates, derives each work node's typed
  I/O from the registry, and wires dependency edges (`green: every dependent
  whose inputs are ready`, `red: retry ≤cap`, `back: target`, `human: hold`).
- **Type-check** (A4): edge `A → B` is valid iff `A.out` is accepted by
  `B.in`. Acceptance is exact string match, with ONE widening: an expected
  composite `X + Y` accepts a carried `X` or `Y` on a **single incoming edge**.
  A JOIN is stricter: all predecessor outputs are merged and their normalized,
  order-insensitive operand set must exactly equal the work node's declared
  input. Missing and extra branch artifacts both reject. A control JOIN carries
  that merged expression unchanged. Multiple terminal branches are merged by
  the same strict rule for the workflow header output. Checked at registration;
  header types must be identifiers (or `A + B` composites), never empty.
- **Bounds**: hop cap and per-node attempt caps are runtime constants, not
  source-controlled (a graph author cannot un-bound a run).
- **Autonomous continuation**: one external turn may spend as many workflow
  continuations as the declared stages require and its run budget permits.
  Each logical provider-request boundary rebinds the current typed node and
  its exact CAS inputs. The run deadline, `max-cost`, and
  `max_provider_requests_per_turn` remain the enclosing bounds; crossing the
  request ceiling returns the typed loop-limit error. A repeated
  `(run_id, workflow-state digest)` after a durable finalization deferral stays
  fail-closed as `workflow_unfinished`, because crash/replay ambiguity or no
  progress cannot safely authorize duplicate work. A changed digest is durable
  proof of stage progress and may spend the next continuation.
- Versioning: this document is `pipe/v1`. The version rides the registry
  record, not the source text.
