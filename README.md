<div align="center">

<img src="assets/haider-agent-logo.png" alt="Haider Agent logo" width="320" />

<br/>

# بِسْمِ اللَّهِ الرَّحْمَٰنِ الرَّحِيمِ

*In the Name of God, the Most Beneficent, the Most Merciful*

<br/>

# ⚔️ Haider — حيدر

### **The Harness** — a first-party, provider-agnostic coding-agent runtime

*One Rust binary that is a TUI, a headless runtime, and a per-device daemon — where every piece of interior state is a typed, evented, queryable contract.*

![Version](https://img.shields.io/github/v/release/Rizzist/haider-agent?label=Haider&color=8b0000) ![Rust](https://img.shields.io/badge/Rust-100%25-orange?logo=rust) ![macOS](https://img.shields.io/badge/macOS-✓-black?logo=apple) ![Windows](https://img.shields.io/badge/Windows-✓-0078D4) ![Linux](https://img.shields.io/badge/Linux-✓-FCC624?logo=linux&logoColor=black)

</div>

---

## ⬇️ Download

Grab the [**latest release**](https://github.com/Rizzist/haider-agent/releases/latest) and pick the archive for your platform:

| Platform | Asset |
|---|---|
| 🍎 macOS (Apple Silicon) | `haider-v<version>-aarch64-apple-darwin.tar.xz` |
| 🍎 macOS (Intel) | `haider-v<version>-x86_64-apple-darwin.tar.xz` |
| 🐧 Linux (x86_64 / arm64) | `haider-v<version>-<arch>-unknown-linux-gnu.tar.xz` |
| 🪟 Windows | `haider-v<version>-x86_64-pc-windows-msvc.zip` |

Every asset ships with a `.sha256`. Extract, put `haider` and `haiderd` on your `PATH`, and run:

```
haider --version
haider
```

> **First launch:** builds are not yet OS-signed. On macOS, clear quarantine (`xattr -c haider haiderd`) or right-click → Open. `/update` inside the TUI checks for new releases from then on.

---

## ⚔️ What is Haider?

Haider is a coding-agent harness: the layer that turns a language model into a working software agent. You talk to it in a terminal; underneath, a per-device daemon owns every session, drives the model through a registry of typed tools, enforces permissions per effect, and journals everything that happens as durable, replayable events.

Most harnesses are a chat loop with tools bolted on. Haider is built the other way around: the **contract comes first**. Every fact an agent produces — a tool call, a subagent spawn, a permission grant, a compaction, an image it generated — is a typed event in an append-only journal, and every surface (the TUI, headless runs, a future ADE) is a pure projection of that log. The daemon is the single source of truth; clients render it, they never invent it.

It runs your model however you pay for it: twelve builtin providers — Anthropic and Claude subscription OAuth, OpenAI and codex subscription OAuth, Gemini, DeepSeek, xAI Grok API and SuperGrok subscription OAuth, Kimi coding-plan OAuth, Bedrock, Vertex, and any OpenAI-compatible endpoint you point it at — with live model catalogs, subscription quota meters, and credential imports from the official CLIs you already logged into.

> 🏛️ Sessions, subagents, typed workflows, permissions, voice, computer use, and usage metering are all wired into one evented core — not added on as plugins.

## ⚡ At a glance

**Codex, Claude Code, and opencode are agents in a terminal. Haider is the harness underneath** — the daemon owns sessions, accounts, and state; every surface is a view of it. Nothing is inferred, everything is declared.

| | |
|---|---|
| 🌳 **Sessions** | History is a tree — branch, fork, replay; attach from anywhere; kill anything, resume everything |
| 📡 **The Pipe** | Every session is a live, seekable event stream — the ADE folds it, `grep` reads it |
| 🔥 **Cachemaxxing** | Byte-stable prefixes + cache-riding compaction, with the live hit-rate on screen |
| 🧵 **Loom** | Agents with type signatures — capabilities declared, least-privilege by construction |
| 🕸️ **Convergence Graph** | Long work runs on a typed DAG with evidence-graded gates, not hope |
| 🤖 **Fleet** | Subagents are scheduled, leased citizens of the daemon — not orphaned child processes |
| 🔑 **Every model, your accounts** | Twelve provider lanes — subscription OAuth + API keys, harness-owned; switch without losing the session |
| 🪝 **Transparent Hooks** | Any editor, ADE, or script builds on the typed surface — no PTY scraping, ever |
| 🛰️ **LionWire** | One typed protocol under every encoding — TUI, JSON, RPC, msgpack wire; built for slow links |
| 🎙️ **Talk** | Push-to-transcribe on every surface |
| 🏠 **Local-first** | The daemon runs on your machine; code, transcripts, and credentials need nobody's cloud |
| 🔜 **Aura · Peers** | Voice orchestration; place agents on any enrolled device — leased, recovered, migrated |

## 🧭 Philosophy

- **Typed events over vibes.** Interior state is never a string soup. Protocol contracts are golden-fixture tested, schema changes are versioned migrations, and old daemons and new clients degrade honestly instead of guessing.
- **The daemon is the truth.** Labels, grants, catalogs, and graph state come from the daemon, spoof-stripped and validated. A client renders what is provable; a stale or foreign fact renders as *unavailable*, never as a lie.
- **Prompt-cache discipline is architecture, not optimization.** The append-only log plus a pure projection means every request is the previous request plus a suffix — the whole prefix rides the provider cache. Even compaction replays the conversation byte-for-byte plus one instruction so the summary itself rides the warm cache. CI asserts byte-prefix stability so a regression is a red build, not a bigger bill.
- **Least privilege by construction.** Tools declare effects; permissions gate effects; typed subagents get grants scoped to their declared capabilities — exec fenced to exact declared programs, network fenced per host on every redirect hop.
- **Humans gate the irreversible.** Registrations, risky effects, and full plans flow through durable review menus. An accepted plan *is* the authorization — auditable, replayable, honest.
- **Ship-verdict development.** Every wave runs a clean-code plan → implement → adversarial multi-round review → optimize → ship loop, with mutation-checked test pins and a CI that fails any patch reducing the workspace test count.

## 🧰 The toolset

**🖥️ The TUI.** A fast terminal client on the daemon's live event stream: session chips with a subagent tree, steering mid-turn, branching and fork/backtrack, a slash palette with custom commands, `!` shell escapes, themes, and a fleet view across sessions. Attachments ride a bounded ladder — paste text or images, attach files and PDFs — and when an agent *produces* an image, the transcript shows a durable 🖼 event you can click to reveal the file in your OS file manager.

**🤖 Providers and accounts.** Twelve builtin lanes plus custom endpoints. Subscription OAuth for Claude, codex, Kimi, and SuperGrok/X Premium — with device-code logins, credential import from the official CLIs (`~/.codex`, `~/.grok`, Kimi), proactive token refresh, and per-lane quota meters in `/usage` (weekly windows, reset countdowns, plan tiers). Model libraries come from each provider's live catalog, never from hardcoded lists. Credentials live in an encrypted file vault; account rotation survives quota errors mid-turn.

**🕸️ Convergence Graph.** Long work runs on a typed DAG, not hope: nodes with command/ship/human/all-of-N gates, evidence tallies with daemon-verified provenance, durable attempts, and a full-screen `/graph` view of exactly where a run stands. Todos dispatch into it; subagents report into it.

**🧵 Loom.** Typed workflows and capability-scoped agent types. A `pipe/v1` source like `clip: SourceURL -> VideoFile` plus node lines compiles onto the graph runtime — registered workflows run by name with zero new machinery. Agent types declare a job, typed I/O, and capability grants (CLIs, API hosts, skills); typed subagents spawn with least-privilege grants and render as accent-colored chips. Agents can author new types and workflows mid-session, but registration only lands through a human-accepted `plan` — the generic full-screen review-before-commit tool. Browse it all in `/loom`.

**🛠️ The tool registry.** Nineteen typed tools — bounded filesystem read/write/edit/search/glob/path, one-shot and background process execution with task supervision, fenced web fetch and search, subagent spawn/message, todos, graph evidence, request-input, plan, and native computer use. Tool schemas ride the wire as minimal stubs with one compact manual in the cached system prompt (the instruct-pipe), cutting the advertised tool prefix by over a third.

**🖱️ Computer use.** Native screenshot-and-actuate backends for macOS, Linux (X11/Wayland), and Windows, adapted to both Anthropic and OpenAI computer-use models — with default-deny grants, an OS-permission repair flow (Screen Recording / Accessibility), configurable screenshot redaction, and daemon-verified observation evidence.

**🎙️ Voice.** `/talk` push-to-talk dictation with local Whisper (offline) or Deepgram, bounded capture, and a model-download manager.

**🔐 Permissions.** Every tool effect is classed (fs read/write, exec, network-per-host, credentials, screen observe/control) and brokered: allow, ask, or deny, with durable one-shot grants through real menus, an opt-in auto-allow mode for unattended runs, and children that are pre-allowed only within their grant ceiling.

**📊 Usage and cost.** A local, exact token ledger per session and account — logical input, cache reads and writes, output — priced per provider where metered and quota-tracked where subscribed, with prefix-digest attribution that can tell you *why* a request missed the cache.

## 🏗️ How it's built

```text
┌──────────────────────────────────────────────────────────────────┐
│  ⚔️  haider (one binary)                                         │
│                                                                  │
│   TUI / headless CLI ───── typed RPC ───── haiderd (daemon)      │
│   (pure projections         (versioned      sessions · tools     │
│    of the event log,         frames,        permissions · graph  │
│    zero invented state)      features)      loom · accounts      │
│                                             journal + CAS        │
└──────────────────────────────┬───────────────────────────────────┘
                               │  provider adapters
                ┌──────────────▼───────────────┐
                │  Anthropic · OpenAI · Gemini │
                │  DeepSeek · xAI · Kimi       │
                │  Bedrock · Vertex · custom   │
                │  (API keys + subscription    │
                │   OAuth, live catalogs)      │
                └──────────────────────────────┘
```

| Crate | Role |
|---|---|
| `haider-protocol` | Typed contracts + golden fixtures — events, items, graph, loom, permissions |
| `haider-store` | Append-only journal, schema migrations, content-addressed artifact store |
| `haider-core` | The harness runtime: turn loop, prompt projection, compaction, context policy |
| `haider-provider` | Model adapters, wire builders, cache breakpoints, usage meters, catalogs |
| `haider-tools` | The tool registry, effect broker, process/web/computer backends |
| `haider-daemon` / `haider-daemond` | Session hub, permissions, subagents, OAuth, discovery — the truth |
| `haider-rpc` / `haider-client` | Versioned client API + connection machinery |
| `haider-tui` / `haider-cli` | The terminal client and the `haider` binary |
| `haider-accounts` · `haider-stt` · `haider-pdf` · `haider-platform` · `haider-verify` | Vault, dictation, PDF ladder, OS glue, verification gate |

Third-party clients should start with the authoritative
[client contract v1](docs/client-contract-v1.md): discovery and framing,
feature gates, snapshot/watch/replay laws, field-level precedence, absence
semantics, native-pipe following, and compatibility fixtures are specified
there. Do not infer the interface from terminal output.

## 🚀 Development

```bash
cargo build --release -p haider-cli -p haider-daemond
cargo test --workspace          # ~2,600 tests
cargo run -p xtask -- test-count   # the ledger CI enforces
```

Rules live in `CONVENTIONS.md`. Highlights: tests are mutation-checked pins (each documents the exact code deletion that must fail it); CI fails any patch that reduces the workspace test count; clippy runs `-D warnings` across the workspace; schema-affecting patches close all lanes until merged. Built by AI agents under the BUILDGUIDE discipline — every wave ends in an adversarial multi-round SHIP-verdict review, and the previous release dogfoods the next (N−1).

Haider is a sibling of [**Diff Forge AI**](https://github.com/Rizzist/diffforge-client), the Agentic Development Environment — Haider is the harness layer an ADE like Diff Forge can drive, with the durable event contract designed for richer surfaces to render.

## 📜 License

Licensed under the **[Kingdom of Abraham Permissive License (KOA-P-1.0)](LICENSE.md)** — an MIT-equivalent license for the AI Agents Era. Every copy must carry the license in full. See the [Kingdom Of Abraham Licenses](https://github.com/Rizzist/Kingdom-Of-Abraham-Licenses) collection.

---

<div align="center">

**⚔️ The harness holds. حيدر**

*Haider — typed events, honest state, warm caches.*

</div>
