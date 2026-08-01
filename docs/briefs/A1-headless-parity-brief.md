# A1 — `haider run` active-account parity

Problem: the run parser allowlists `--provider anthropic|fake`; the
daemon's ACTIVE account (openai-oauth on this box) is unreachable
headless. The TUI resolves identity from the daemon (account.list active
+ provider default model); the runner must do the same.

Design (no protocol change):
1. Runner bootstrap (haider-client headless): when the request carries no
   explicit provider, issue `account.list` on the ensured connection,
   pick the ACTIVE account's provider; resolve the model from the
   provider defaults (`provider.list` summary default_model, fallback
   first model); typed error `no_active_account` (exit 65 + remedy line)
   when nothing is active.
2. CLI: `--provider <name>` accepts ANY name (daemon validates at
   create — typed refusal, exit 65/76); `--model` optional when the
   provider has a default. `fake` keeps working (tests).
3. Laws: flagless run creates on the active account's provider+model
   (peer-fixture pins the create body); unknown provider → typed create
   refusal surfaces; anthropic path byte-unchanged; json/print outputs
   carry the resolved provider/model in `haider.run.v1` (additive
   fields: provider, model).

Mutations: drop the active-account read → flagless law fails; hardcode
anthropic → same; skip default-model resolution → create body law fails.

Lane: codex (CLI/client, non-UI) after v0.0.40 closes.
