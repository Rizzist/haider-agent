"""Pin the two-stage model picker against a production-shaped inventory."""

from __future__ import annotations

import json
import re

from gate import (
    DAEMON_STARTUP,
    DAEMON_STOP,
    FAIL,
    PASS,
    PROCESS_EXIT_GRACE,
    STATUS_REQUEST,
    BudgetSum,
    Evidence,
)
from gate.tui_probe import TUI_ACTION, TUI_BOOT, TUI_EXIT, TUI_REPAINT

id = "t0.tui.model_picker_cardinality"
tier = "t0"
area = "tui"
needs = ("binary", "daemon", "pty", "network:none")
script = [{"step": "finish", "reason": "end_turn"}]
turns_expected = 0

# Registry #94: the inventory currently yields fewer than 48 unique search
# targets. The declaration conservatively contains 49 complete PTY actions
# (one cardinality/escape probe + up to 48 target searches), plus startup and
# cleanup. Five repaint envelopes per PTY cover the top-level escape sequence
# and the longest search/select/provider-stage path. Total =
# 49*(25+12+5*4+2.5)+30+3*60+20+2+2 = 3149.5s.
budget = BudgetSum(
    (
        DAEMON_STARTUP,
        STATUS_REQUEST,
        STATUS_REQUEST,
        *((TUI_BOOT, TUI_ACTION, TUI_REPAINT, TUI_REPAINT, TUI_REPAINT,
           TUI_REPAINT, TUI_REPAINT, TUI_EXIT) * 49),
        STATUS_REQUEST,
        DAEMON_STOP,
        PROCESS_EXIT_GRACE,
        PROCESS_EXIT_GRACE,
    )
)
timed = False


def _seed_inventory(ctx) -> None:
    profiles = []
    accounts = []
    for index in range(1, 9):
        provider = f"qa-api-{index}"
        profiles.append(
            {
                "provider_id": provider,
                "display_name": provider,
                "api_family": "openai_chat_completions",
                "base_url": f"https://qa{index}.openai.azure.com/openai/v1",
                "enabled": True,
                "auth_requirement": "api_key",
                "configured_models": ["shared-alpha", "shared-beta", f"unique-{index}"],
                "default_model": "shared-alpha",
                "provenance": "custom",
                "trust": "full",
            }
        )
        accounts.append(
            {
                "alias": f"qa-api-account-{index}",
                "provider": provider,
                "auth_method": "api_key",
                "identity": "QA inventory fixture",
                "status": {"status": "ok"},
                "active": True,
            }
        )
    # OAuth is release-owned; use two release IDs, never fictitious custom
    # OAuth providers. The persisted production enum spelling is `o_auth`.
    for index, provider in enumerate(("anthropic-oauth", "openai-oauth"), start=1):
        profiles.append(
            {
                "provider_id": provider,
                "display_name": provider,
                "api_family": "openai_chat_completions",
                "base_url": f"https://oauth{index}.openai.azure.com/openai/v1",
                "enabled": True,
                "auth_requirement": "o_auth",
                "configured_models": ["oauth-shared"],
                "default_model": "oauth-shared",
                "provenance": "custom",
                "trust": "full",
            }
        )
        accounts.append(
            {
                "alias": f"qa-oauth-account-{index}",
                "provider": provider,
                "auth_method": "oauth",
                "identity": "QA OAuth inventory fixture",
                "status": {"status": "ok"},
                "active": True,
            }
        )
    (ctx.profile_dir / "providers.json").write_text(
        json.dumps({"providers": profiles}, separators=(",", ":")), encoding="utf-8"
    )
    (ctx.profile_dir / "accounts.json").write_text(
        json.dumps(accounts, separators=(",", ":")), encoding="utf-8"
    )


def _open_picker(ctx):
    from gate.tui_probe import TuiProcess

    tui = TuiProcess(ctx)
    tui.type("/model")
    tui.enter()
    tui.settle(0.45)
    return tui


def _provider_stage(frames, model: str) -> bool:
    pattern = re.compile(rf"^\s*PROVIDERS\s+—\s+{re.escape(model)}\b", re.MULTILINE)
    return all(pattern.search(frame.text) is not None for frame in frames)


def _placeholder_selection_index(frame, provider: str) -> int | None:
    """Read the exact placeholder's current filtered-row index from the TUI."""

    lines = frame.text.splitlines()
    try:
        list_start = next(index for index, line in enumerate(lines) if " choices " in line) + 1
        list_end = next(
            index
            for index, line in enumerate(lines[list_start:], start=list_start)
            if "select OAuth / open API providers" in line
        )
    except StopIteration:
        return None
    rows = [line for line in lines[list_start:list_end] if line.strip()]
    pattern = re.compile(rf"^\s*—\s+{re.escape(provider)}\s+")
    return next((index for index, line in enumerate(rows) if pattern.search(line)), None)


def run(ctx) -> list[Evidence]:
    from gate.tui_probe import RpcClient, daemon_transport_diagnosis, start_daemon

    _seed_inventory(ctx)
    status = start_daemon(ctx)
    rpc = RpcClient(status["daemon"]["socket_path"])
    evidence: list[Evidence] = []
    try:
        providers = rpc.request({"method": "provider.list"}).get("providers", [])
        enabled = [provider for provider in providers if provider.get("enabled") is True]
        api_slugs: set[str] = set()
        oauth_pairs: list[tuple[str, str]] = []
        placeholders: list[str] = []
        api_providers_by_slug: dict[str, list[str]] = {}
        provider_summaries: dict[str, dict] = {}
        for provider in enabled:
            name = provider.get("provider")
            if isinstance(name, str):
                provider_summaries[name] = provider
            models = provider.get("models") if isinstance(provider.get("models"), list) else []
            auth_methods = provider.get("auth_methods", [])
            if not models:
                placeholders.append(name)
            elif "oauth" in auth_methods:
                oauth_pairs.extend((name, model) for model in models)
            else:
                for model in models:
                    api_slugs.add(model)
                    api_providers_by_slug.setdefault(model, []).append(name)
        expected = len(api_slugs) + len(oauth_pairs) + len(placeholders)

        tui = _open_picker(ctx)
        try:
            wide, narrow = tui.repaint_both()
            match_wide = re.search(r"(\d+) choices", wide.text)
            match_narrow = re.search(r"(\d+) choices", narrow.text)
            actual_wide = int(match_wide.group(1)) if match_wide else None
            actual_narrow = int(match_narrow.group(1)) if match_narrow else None
            current_visible = "current" in wide.text and "current" in narrow.text

            # The intentionally overlapping API slug must drill exactly once;
            # Esc returns to MODELS before a second Esc closes the picker.
            tui.type("shared-alpha")
            tui.enter()
            tui.settle(0.35)
            provider_wide, provider_narrow = tui.repaint_both()
            provider_stage = (
                "shared-alpha" in provider_wide.text
                and "providers" in provider_wide.text.lower()
                and "qa-api-1" in provider_wide.text
                and "shared-alpha" in provider_narrow.text
                and "providers" in provider_narrow.text.lower()
                and "qa-api-1" in provider_narrow.text
            )
            tui.esc()
            tui.settle(0.25)
            list_wide, list_narrow = tui.repaint_both()
            esc_to_list = (
                "MODELS" in list_wide.text
                and "shared-alpha" in list_wide.text
                and "MODELS" in list_narrow.text
                and "shared-alpha" in list_narrow.text
            )
            tui.esc()
            tui.settle(0.25)
            closed_wide, closed_narrow = tui.repaint_both()
            esc_closed = "MODELS —" not in closed_wide.text and "MODELS —" not in closed_narrow.text
            current_rows = {
                "118x36": [
                    line.strip() for line in wide.text.splitlines() if "current" in line.lower()
                ],
                "80x24": [
                    line.strip() for line in narrow.text.splitlines() if "current" in line.lower()
                ],
            }
            provider_headers = {
                "118x36": [
                    line.strip()
                    for line in provider_wide.text.splitlines()
                    if "PROVIDERS —" in line
                ],
                "80x24": [
                    line.strip()
                    for line in provider_narrow.text.splitlines()
                    if "PROVIDERS —" in line
                ],
            }
            list_headers = {
                "118x36": [
                    line.strip() for line in list_wide.text.splitlines() if "MODELS —" in line
                ],
                "80x24": [
                    line.strip() for line in list_narrow.text.splitlines() if "MODELS —" in line
                ],
            }
            closed_headers = {
                "118x36": [
                    line.strip()
                    for line in closed_wide.text.splitlines()
                    if "MODELS —" in line
                ],
                "80x24": [
                    line.strip()
                    for line in closed_narrow.text.splitlines()
                    if "MODELS —" in line
                ],
            }
            clean, audit = tui.close()
        finally:
            if not tui.closed:
                tui.close()

        seeded_shape = (
            sum(name.startswith("qa-api-") for name in [p.get("provider", "") for p in enabled]) == 8
            and len([pair for pair in oauth_pairs if pair[0] in {"anthropic-oauth", "openai-oauth"}]) == 2
            and len(api_providers_by_slug.get("shared-alpha", [])) == 8
        )
        cardinality_ok = actual_wide == expected == actual_narrow and seeded_shape and clean
        evidence.append(
            Evidence(
                "top_level_cardinality",
                PASS if cardinality_ok else FAIL,
                f"top_rows expected={expected} actual_118x36={actual_wide!r} actual_80x24={actual_narrow!r} "
                f"unique_api_slugs={len(api_slugs)} oauth_pairs={len(oauth_pairs)} placeholders={len(placeholders)} "
                f"seed=8_api_overlap+2_oauth_pairs actual={str(seeded_shape).lower()} {audit}",
            )
        )
        evidence.append(
            Evidence(
                "current_and_escape",
                PASS if current_visible and provider_stage and esc_to_list and esc_closed else FAIL,
                f"current_visible_top={str(current_visible).lower()} provider_stage_once={str(provider_stage).lower()} "
                f"esc_to_list={str(esc_to_list).lower()} second_esc_closes={str(esc_closed).lower()} "
                f"actual_current_rows={current_rows!r} actual_provider_headers={provider_headers!r} "
                f"actual_list_headers={list_headers!r} actual_closed_headers={closed_headers!r}",
            )
        )

        unreachable: list[str] = []
        reach_details: list[str] = []
        targets: list[tuple[str, str | None]] = [(slug, None) for slug in sorted(api_slugs)]
        targets.extend(sorted((model, provider) for provider, model in oauth_pairs))
        targets.extend(("—", provider) for provider in sorted(placeholders))
        # Placeholder rows cannot be searched by the em dash alone without
        # returning all placeholders; their provider name is the target key.
        for model, provider in targets:
            candidate_providers = (
                [provider]
                if provider is not None
                else api_providers_by_slug.get(model, [])
            )
            # Include one represented provider token for model rows. Search is
            # token/substr based, so this distinguishes `claude-fable-5` from
            # the earlier `anthropic.claude-fable-5` row without relying on
            # rendered order. Placeholder rows have no model token.
            search = (
                provider
                if model == "—"
                else " ".join((model, *candidate_providers[:1]))
            )
            tui = _open_picker(ctx)
            try:
                tui.type(search)
                tui.settle(0.25)
                found_wide, found_narrow = tui.repaint_both()
                visible = model in found_wide.text and model in found_narrow.text
                if provider is not None:
                    visible = visible and provider in found_wide.text and provider in found_narrow.text
                placeholder_index = None
                if model == "—":
                    # A provider-name search can also match another provider's
                    # model slug (notably `anthropic.*`). Read the exact row's
                    # position from the rendered filtered list rather than
                    # assuming the placeholder is first or last.
                    placeholder_index = _placeholder_selection_index(found_wide, provider)
                    if placeholder_index is not None:
                        tui.down(placeholder_index)
                tui.enter()
                placeholder_signal = True
                if model == "—":
                    placeholder_signal = tui.wait_for(
                        lambda raw: b"no discovered models" in raw or b"unavailable" in raw
                    )
                else:
                    tui.settle(0.3)
                selected_wide, selected_narrow = tui.repaint_both()
                provider_drill = _provider_stage(
                    (selected_wide, selected_narrow), model
                )
                stages = 1 if provider_drill else 0
                if provider_drill:
                    tui.enter()
                    selection_signal = tui.wait_for(
                        lambda raw: (
                            b"model" in raw
                            and (model.encode() in raw or (provider or "").encode() in raw)
                        )
                    )
                    selected_wide, selected_narrow = tui.repaint_both()
                    if _provider_stage((selected_wide, selected_narrow), model):
                        stages = 2
                picker_closed = (
                    not re.search(r"^\s*MODELS\s+—", selected_wide.text, re.MULTILINE)
                    and not re.search(r"^\s*MODELS\s+—", selected_narrow.text, re.MULTILINE)
                )
                # A selected placeholder may open a provider-specific surface;
                # that is still a reachable target. Any unknown-command flash
                # or a second provider stage is not.
                recognized = "unknown command" not in (
                    selected_wide.text + "\n" + selected_narrow.text
                ).lower()
                refusal_reasons = sorted(
                    {
                        reason.lower()
                        for candidate in candidate_providers
                        if isinstance(candidate, str)
                        if isinstance(provider_summaries.get(candidate), dict)
                        if isinstance(
                            reason := provider_summaries[candidate].get("availability_reason"),
                            str,
                        )
                        and reason
                    }
                )
                refusal_needles = sorted(
                    {
                        needle
                        for reason in refusal_reasons
                        for needle in (reason, " ".join(reason.split()[:3]))
                        if needle
                    }
                )
                target_refusal = all(
                    (model == "—" or model in frame.text)
                    and (provider is None or provider in frame.text)
                    and any(needle in frame.text.lower() for needle in refusal_needles)
                    for frame in (selected_wide, selected_narrow)
                )
                placeholder_refusal = model != "—" or (
                    target_refusal
                )
                resolved = picker_closed or target_refusal
                if (
                    stages > 1
                    or not recognized
                    or (not visible and not target_refusal)
                    or not placeholder_refusal
                    or (model == "—" and placeholder_index is None)
                    or not placeholder_signal
                    or (provider_drill and not selection_signal)
                    or (model != "—" and not resolved)
                ):
                    unreachable.append(
                        f"{provider + '/' if provider else ''}{model}:"
                        f"selected={picker_closed},refused={target_refusal},"
                        f"stages={stages},recognized={recognized}"
                    )
                reach_details.append(
                    f"{provider + '/' if provider else ''}{model}:selected={picker_closed},"
                    f"refused={target_refusal},visible={visible},"
                    f"placeholder_index={placeholder_index!r},reasons={refusal_reasons!r},"
                    f"needles={refusal_needles!r},stages={stages}"
                )
                clean, _audit = tui.close()
                if not clean:
                    unreachable.append(f"{provider + '/' if provider else ''}{model}:unclean")
            finally:
                if not tui.closed:
                    tui.close()
        evidence.append(
            Evidence(
                "search_reachability",
                PASS if not unreachable else FAIL,
                f"targets={len(targets)} activated_each={str(not unreachable).lower()} "
                "search_plus_provider_stages_max=1 "
                f"unreachable={unreachable!r} actual={reach_details!r} "
                f"{daemon_transport_diagnosis(ctx.profile_dir)}",
            )
        )
        return evidence
    finally:
        rpc.close()
