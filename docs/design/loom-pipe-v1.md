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
node       = name [ws "@" agent-type] [ws quoted-task] [ws ":" gate] [ws back]
name       = [A-Za-z0-9_-]+
agent-type = [A-Za-z0-9_-]+          ; must resolve in the Loom registry
quoted-task= '"' [^"]* '"'
gate       = "cmd" | "ship" | "human" | "all-of-" N
back       = ("↺" | "^") name        ; conditional red back-edge
comment    = "#" ...                 ; whole-line, ignored
```

- A node **with** `@agent-type` is a **work node**: a capability-scoped
  specialist that transforms the artifact. Its typed I/O signature is
  **derived from the registry** (`AgentType.in → AgentType.out`), never
  authored in the source.
- A node **without** `@agent-type` is a **control node**: it only gates —
  identity on the artifact.
- Gate default is `cmd`. Meanings:
  | gate | resolved | green when |
  |---|---|---|
  | `cmd` | command-green | the node's command/child exits 0 |
  | `ship` | reviewer-SHIP | a reviewer says SHIP |
  | `all-of-N` | all-of-N | every one of N fan-out dimensions is green |
  | `human` | human-confirm | the human approves (the run parks on a menu) |
- `↺target` (ASCII alias `^target`) declares the red edge: when the gate
  exhausts its in-node retries, the runtime reopens `target` — which **must
  be an earlier node** — as a new rev-bound attempt. Forward or unknown
  targets are parse errors. Green always flows to the next line; the last
  node's green is `done`.

## Example (the flagship)

```
make-video: SourceURL -> VideoFile
research @researcher "pull the source and transcribe it" :cmd
propose  @proposer   "shape a hook and a 6-beat arc"     :ship
capture  @capturer   "gather b-roll for every beat"      :all-of-6 ↺propose
edit     @editor     "cut to the arc, trim dead air"     :ship ↺capture
render   @renderer   "encode 1080p H.264"                :cmd
publish              "you approve the cut"               :human
```

## Contract

- **Parse** (`pipe source → AST`) is pure and total: errors are collected on
  the AST (`errors: Vec<String>`), never thrown. An AST with errors is
  rejected at registration, not at run.
- **Compile** (`AST → nodes`) resolves gates, derives each work node's typed
  I/O from the registry, and wires edges (`green: next`, `red: retry ≤cap`,
  `back: target`, `human: hold`).
- **Type-check** (A4): edge `A → B` is valid iff `A.out` is accepted by
  `B.in` (subtype/coercion later; exact match first). Checked at
  registration.
- **Bounds**: hop cap and per-node attempt caps are runtime constants, not
  source-controlled (a graph author cannot un-bound a run).
- Versioning: this document is `pipe/v1`. The version rides the registry
  record, not the source text.
