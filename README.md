<div align="center">

<img src="assets/haider-agent-logo.png" alt="Haider Agent logo" width="320" />

<br/>

# بِسْمِ اللَّهِ الرَّحْمَٰنِ الرَّحِيمِ

*In the Name of God, the Most Beneficent, the Most Merciful*

<br/>

# ⚔️ Haider — حيدر

### **The Harness** — a first-party, provider-agnostic coding-agent runtime

*One Rust binary that is a TUI, a headless runtime, and a per-device daemon — where every piece of interior state is a typed, evented, queryable contract.*

![Version](https://img.shields.io/github/v/release/Rizzist/haider-agent?label=Haider&color=8b0000) ![Rust](https://img.shields.io/badge/Rust-100%25-orange?logo=rust) ![macOS](https://img.shields.io/badge/macOS-✓-black?logo=apple) ![Windows](https://img.shields.io/badge/Windows-✓-0078D4) ![Linux](https://img.shields.io/badge/Linux-✓-FCC624?logo=linux&logoColor=black) ![Android](https://img.shields.io/badge/Android-APK-3DDC84?logo=android&logoColor=white)

</div>

---

## ⬇️ Download

Open the [latest release](https://github.com/Rizzist/haider-agent/releases/latest) and choose the exact asset for your platform.

| Platform | Release asset |
|---|---|
| macOS arm64 (signed · notarized) | `haider-v<version>-aarch64-apple-darwin.tar.xz` |
| macOS x86_64 (signed · notarized) | `haider-v<version>-x86_64-apple-darwin.tar.xz` |
| Linux x86_64 | `haider-v<version>-x86_64-unknown-linux-gnu.tar.xz` |
| Linux arm64 | `haider-v<version>-aarch64-unknown-linux-gnu.tar.xz` |
| Windows x86_64 | `haider-v<version>-x86_64-pc-windows-msvc.zip` |
| Android (APK, sideloadable) | `haider-v<version>-android.apk` |

Every binary asset has a `.sha256` beside it. Download both files into the same directory, then verify before extracting or installing. The same command works for every `.tar.xz`, `.zip`, and `.apk` asset; replace `<asset>` with the full asset filename from the table above:

```console
$ shasum -a 256 -c <asset>.sha256
<asset>: OK
```

The [native release workflow](.github/workflows/release.yml) produces the five target archives, signs and notarizes both macOS builds, and verifies the Android checksum before admitting an APK to a release.

For Android, allow **Install unknown apps** for the browser or file manager that opens the download, then install the APK. A published release APK has been signed with the configured release key and passed v2/v3 signature verification. The app's updater reads GitHub Releases, downloads the matching checksum, verifies SHA-256 after download and again before handing the file to Android's package installer. See the [Android release workflow](.github/workflows/android-apk.yml), [update checker](android/app/src/main/java/ai/diffforge/haider/update/UpdateChecker.kt), and [installer boundary](android/app/src/main/java/ai/diffforge/haider/update/PackageInstallerLauncher.kt).

The APK is a separately gated companion build. If that job fails, the native release can still publish without Android; the release page is the authority for which assets are actually present.

Extract the native archive, put `haider` and `haiderd` on your `PATH`, and run:

```console
$ haider --version
$ haider
```

---

## ⚔️ What is Haider?

Haider is a local coding-agent harness built around one durable, per-device daemon. The daemon owns sessions, provider state, permissions, and the append-only event journal; the CLI, TUI, headless runner, Android app, and other clients attach to it through typed contracts. A surface may render daemon truth, replay it, or submit a command, but it does not become a second source of truth.

## 01 / Speed + memory

**Measured, not asserted.** These results were [measured on haider-agent v0.0.960 by the haidercode 21-case adapter conformance gate](https://haidercode.ai/benchmark):

| Measurement | Result |
|---|---|
| Conformance | 19 of 21 · zero failures · the two non-passes are Linux-only procfs platform skips |
| Peak RSS | haider 59.4 MiB · pi 254.5 MiB · 4.3× lighter, vs pi only |
| Suite wall time | haider 8.9 s · pi 20.7 s · 2.3× faster, vs pi only |
| Work-time per case | haider 187 ms · pi 666 ms · rick 43.5 ms |

rick records 54.6 MiB peak RSS on 9 of 21. The method uses the same deepseek-v4-flash model, the same machine, and pinned builds. It tests protocol behavior, telemetry, retries, isolation, and patch collection — not coding quality. An unknown stays “unknown,” never zero.

## 02 / Five targets + APK

The release matrix builds five native target triples: macOS arm64 and x86_64, Linux x86_64 and arm64, and Windows x86_64. Both macOS builds package Developer ID-signed, Apple-notarized binaries; every release archive has an adjacent SHA-256 file. Android is produced by a separate release-key-signed APK workflow and joins the release only after its checksum verifies.

The exact filenames and verification steps are in the Download table above. The source of truth is the [native release matrix](.github/workflows/release.yml) plus the [Android companion workflow](.github/workflows/android-apk.yml).

## 03 / Any provider, or none

**The model is a lane.** The current provider crate exports [13 built-in provider classes](crates/haider-provider/src/lib.rs): API-key and OAuth lanes for the supported hosted services, Haider Code, and OpenAI-compatible transport. The daemon also accepts custom OpenAI-compatible profiles (standard OpenAI Chat Completions or Anthropic Messages) with an API key or no authentication, including validated trusted-LAN endpoints for local servers.

Catalog availability is daemon-owned: unavailable stays unavailable, never an invented default. `session.select_model` is a receipted mutation; the next turn resolves through the newly selected provider/model pair while the durable session and transcript remain in place. See the [provider registry](crates/haider-daemon/src/provider_registry.rs), [pair-switch tests](crates/haider-daemon/src/pair_switch_runtime_tests.rs), and the [client contract](docs/client-contract-v1.md).

Add a local router with no key; Haider validates the trusted-LAN origin and
discovers its live `/v1/models` inventory before making it selectable:

```console
$ haider account add local-router --base-url http://127.0.0.1:8000 --no-auth --api-family openai
```

For a hosted endpoint, keep the key out of command arguments and source it
from an environment variable. The value is staged directly into the daemon
vault and is never printed in JSON or terminal output:

```console
$ export HAIDER_ROUTER_API_KEY='…'
$ haider account add hosted-router --base-url https://router.example.com --api-key-env HAIDER_ROUTER_API_KEY --api-family openai
```

Use the discovered ids as `local-router/<model>` or
`hosted-router/<model>`. Re-probe one account with
`haider account probe <alias>`, or force inventory refresh with
`haider models --refresh [<alias>]`. Custom catalogs are advisory for
inference: a caller-configured id omitted by `/v1/models` is refreshed once,
then reaches the compatible chat wire verbatim and is shown as unlisted rather
than fabricated into the available catalog.

## 04 / Tokenomics

**Byte-stable prefixes, explicit cache epochs.** Append-only Anthropic and OpenAI-compatible turns are tested to preserve the serialized provider-visible prefix. Content-addressed provider views reject an undeclared same-epoch mutation to system, tool-schema, or prior history bytes.

Compaction, provider/model changes, and other header changes are declared cache-epoch boundaries; Haider does not mislabel them as warm continuation. The session usage fold tracks logical input, cache reads, cache writes, output, and telemetry coverage. It publishes a hit rate only when every logical input token has an authoritative cache split; otherwise the UI renders `n/a`.

Evidence: [wire-prefix tests](crates/haider-provider/tests/prompt_cache_prefix_tests.rs), [provider-view continuity](crates/haider-provider/src/cachemaxxing/provider_view.rs), and [session cache accounting](crates/haider-tui/src/cache_usage.rs).

## 05 / Sessions as a tree

**Branch. Fork. Replay. Resume.** `branch.create` writes a durable named reference at exact fork and head coordinates. `session.fork` creates an independently durable child with source provenance; `session.metafork` binds reviewed prompt omissions to an accepted digest instead of deleting copied history.

`session.attach` replays the journal and then follows the live tail. `haider resume` opens the daemon-owned session roster, while `haider resume <session-id>` attaches directly. The durable topology lives in [branch contracts](crates/haider-protocol/src/branch.rs) and [fork provenance](crates/haider-protocol/src/session_fork.rs); the public doors are specified in the [client contract](docs/client-contract-v1.md).

## 06 / Loom + workflows

**Delegation with a contract.** Loom's `pipe/v1` DSL defines typed inputs and outputs, explicit dependencies, forks, joins, bounded back-edges, and evidence gates, then lowers the workflow onto the Convergence Graph. A dependent node becomes ready only when its declared dependencies are green.

Agent types declare job I/O plus CLI and API capabilities. At spawn, the daemon intersects the type grant with the default child grant and the durable parent ceiling; it validates the effective grant before work begins. Workflow state and evidence remain queryable through `graph.inspect`.

Evidence: [Loom DSL and type contracts](crates/haider-protocol/src/loom.rs), [workflow design](docs/design/loom-pipe-v1.md), [delegation grant enforcement](crates/haider-daemon/src/delegation.rs), and [client graph surface](docs/client-contract-v1.md).

## 07 / The Pipe + client SDK

**The Pipe is seekable.** Every durable `RawEnvelope` carries the sole replay cursor for its session. `session.attach` replays exactly `(after_seq, replay_through_seq]`, emits `AttachCaughtUp`, and then continues with later live envelopes.

Delivery is at least once, so clients advance their cursor only after fully applying a consecutive sequence. On a discontinuity they stop reduction and reattach from the last applied cursor; they never skip the hole. Native sidecar paths are daemon-published rather than guessed, and third-party clients can build on `haider-client` instead of scraping terminal output.

Start with the authoritative [client contract v1](docs/client-contract-v1.md) and the [gap-recovery client tests](crates/haider-client/tests/observe_tests.rs).

## 08 / Voice

**Talk to the running session.** The shipped TUI voice path is `/talk`: toggle dictation with local Whisper or Deepgram, inspect the live ghost transcript, then commit it into the current daemon session. Cancelling leaves the ghost text out of the durable conversation.

Local Whisper can run offline. The Deepgram key is stored through the daemon vault boundary, and downloaded Whisper models are SHA-256 verified before installation. The duplex `/voice` and Aura surfaces remain demo-only in live mode and are not claimed as shipped features here.

Evidence: [`/talk` state machine](crates/haider-tui/src/talk.rs), [STT engine](crates/haider-stt/src), [live TUI wiring](crates/haider-tui/src/app.rs), and [implementation notes](docs/briefs/T2-talk-ux-notes.md).

## 🏗️ How it's built

| Crate | Role |
|---|---|
| `haider-protocol` | Typed events and contracts for sessions, graph, Loom, providers, and permissions |
| `haider-store` | Append-only journal, schema migrations, receipts, and content-addressed artifacts |
| `haider-core` | Turn loop, prompt projection, context policy, and compaction |
| `haider-provider` | Provider adapters, exact wire builders, catalogs, caching, and usage normalization |
| `haider-tools` | Tool registry, effect broker, and process, web, and computer backends |
| `haider-daemon` / `haider-daemond` | Durable session owner, workers, permissions, accounts, graphs, and client transport |
| `haider-rpc` / `haider-client` | Versioned wire types, discovery, replay, and the client SDK |
| `haider-tui` / `haider-cli` / `haider-stt` | Terminal surfaces, headless entry points, and dictation |

The [client contract v1](docs/client-contract-v1.md) is the integration boundary: discovery, framing, feature negotiation, replay laws, absence semantics, and projection precedence are specified there.

## 🚀 Development

```bash
cargo fmt --all --check
cargo clippy --workspace --all-targets --locked -- -D warnings
cargo test --workspace
cargo run -p xtask -- test-count
```

Repository rules live in [CONVENTIONS.md](CONVENTIONS.md). CI enforces formatting, workspace-wide Clippy with warnings denied, and the checked-in workspace test-count floor.

## 📜 License

Licensed under the **[Kingdom of Abraham Permissive License (KOA-P-1.0)](LICENSE.md)** — an MIT-equivalent license for the AI Agents Era. Every copy must carry the license in full. See the [Kingdom Of Abraham Licenses](https://github.com/Rizzist/Kingdom-Of-Abraham-Licenses) collection.

---

<div align="center">

**⚔️ The harness holds. حيدر**

*Haider — typed events, honest state, warm caches.*

</div>
