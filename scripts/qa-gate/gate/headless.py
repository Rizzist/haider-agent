"""Small machine-output assertions shared by headless QA checks."""

from __future__ import annotations

import json
from typing import Any

from .contract import ContractError
from .context import CommandResult, parse_single_json


def json_document(result: CommandResult, label: str) -> dict[str, Any]:
    return parse_single_json(result.stdout, label)


def jsonl_documents(result: CommandResult, label: str) -> list[dict[str, Any]]:
    documents: list[dict[str, Any]] = []
    for index, line in enumerate(result.stdout.splitlines(), start=1):
        if not line.strip():
            continue
        try:
            value = json.loads(line)
        except json.JSONDecodeError as error:
            raise ContractError(f"{label} line {index} invalid JSON: {error.msg}") from error
        if not isinstance(value, dict):
            raise ContractError(f"{label} line {index} must be an object")
        documents.append(value)
    if not documents:
        raise ContractError(f"{label} emitted no JSON documents")
    return documents


def event_payloads(document: dict[str, Any]) -> list[dict[str, Any]]:
    events = document.get("events")
    if not isinstance(events, list):
        return []
    return [
        event["payload"]
        for event in events
        if isinstance(event, dict) and isinstance(event.get("payload"), dict)
    ]


def terminal_payloads_from_json(document: dict[str, Any]) -> list[dict[str, Any]]:
    return [payload for payload in event_payloads(document) if "terminal_kind" in payload]


def terminal_payloads_from_jsonl(documents: list[dict[str, Any]]) -> list[dict[str, Any]]:
    return [
        document["payload"]
        for document in documents
        if isinstance(document.get("payload"), dict)
        and "terminal_kind" in document["payload"]
    ]


def provider_request_ordinals(document: dict[str, Any]) -> set[int]:
    """Count durable request-attempt facts once, not started/completed twice."""

    ordinals: set[int] = set()
    for payload in event_payloads(document):
        if payload.get("type") != "item" or payload.get("event") != "completed":
            continue
        item = payload.get("item")
        if not isinstance(item, dict) or item.get("item") != "extension":
            continue
        if item.get("kind") != "cache_request_attempt_v1":
            continue
        data = item.get("data")
        ordinal = data.get("ordinal") if isinstance(data, dict) else None
        if isinstance(ordinal, int) and not isinstance(ordinal, bool):
            ordinals.add(ordinal)
    return ordinals


def nested_call_ids(value: object) -> list[str]:
    found: list[str] = []
    if isinstance(value, dict):
        for key, child in value.items():
            if key == "call_id" and isinstance(child, str):
                found.append(child)
            found.extend(nested_call_ids(child))
    elif isinstance(value, list):
        for child in value:
            found.extend(nested_call_ids(child))
    return found
