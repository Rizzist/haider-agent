#!/usr/bin/env python3
"""Reconstruct the pinned upstream default URL using synthetic state/PKCE.

Adapted from the independent lane-971 verifier's reference-check.py. No Codex
login, credentials, Haider builder or Haider fixture participates. With
--source-dir, read server.rs and default_client.rs offline; otherwise download
only those two public files from the immutable upstream commit.
"""

import argparse
import hashlib
from pathlib import Path
import re
import sys
import urllib.parse
import urllib.request


UPSTREAM_COMMIT = "2cbbf0c9b542a36a1c3284b5e804917635b6f666"
SOURCES = {
    "server.rs": (
        "codex-rs/login/src/server.rs",
        "59ea616627e40ba9682eab03b4c3d57ecbdc66557f188d354fbf6dee0ec1d6ea",
    ),
    "default_client.rs": (
        "codex-rs/login/src/auth/default_client.rs",
        "e50fc9a61d4b91127e2c248e15d45e735831e4070c396fa8efbc693717cfb23b",
    ),
}


def reconstruct(source_dir):
    sources = {}
    for name, (path, expected_hash) in SOURCES.items():
        if source_dir is not None:
            data = (source_dir / name).read_bytes()
        else:
            url = f"https://raw.githubusercontent.com/openai/codex/{UPSTREAM_COMMIT}/{path}"
            with urllib.request.urlopen(url, timeout=30) as response:
                data = response.read()
        if hashlib.sha256(data).hexdigest() != expected_hash:
            raise ValueError(f"{name} does not match upstream {UPSTREAM_COMMIT}")
        sources[name] = data.decode("utf-8")

    server = sources["server.rs"]
    builder = server[server.index("fn build_authorize_url("):server.index("fn generate_state()")]
    # Default query only: no forced workspace IDs or originator override.
    query = builder[:builder.index("if let Some(workspace_ids)")]
    keys = re.findall(r'\(\s*"([a-z_]+)"\.to_string\(\),', query)
    assert keys == [
        "response_type", "client_id", "redirect_uri", "scope", "code_challenge",
        "code_challenge_method", "id_token_add_organizations",
        "codex_cli_simplified_flow", "state", "originator",
    ], keys
    scope = re.search(r'"(openid profile email offline_access[^"\n]+)"', builder)[1]
    originator = re.search(r'DEFAULT_ORIGINATOR: &str = "([^"]+)"', sources["default_client.rs"])[1]
    issuer = re.search(r'DEFAULT_ISSUER: &str = "([^"]+)"', server)[1]
    port = re.search(r'DEFAULT_PORT: u16 = (\d+);', server)[1]
    assert 'format!("{issuer}/oauth/authorize?{qs}")' in builder
    assert 'urlencoding::encode(&v)' in builder
    values = [
        "code", "app_EMoamEEZ73f0CkXaXp7hrann",
        f"http://localhost:{port}/auth/callback", scope,
        "E9Melhoa2OwvFrEMTJguCHaoeK1t8URWbuGJSstw-cM", "S256", "true", "true",
        "fixture-state_971", originator,
    ]
    assert len(keys) == len(values)
    # Rust urlencoding::encode uses %20, not the form serializer's '+'.
    return issuer + "/oauth/authorize?" + "&".join(
        key + "=" + urllib.parse.quote(value, safe="-_.~")
        for key, value in zip(keys, values)
    )


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--source-dir", type=Path)
    parser.add_argument("--check", action="store_true", help="compare with the checked-in fixture")
    args = parser.parse_args()
    result = (reconstruct(args.source_dir) + "\n").encode("utf-8")
    if args.check:
        fixture = Path(__file__).with_name("codex-authorize-url.txt")
        if result != fixture.read_bytes():
            raise SystemExit("Codex authorize fixture differs from the pinned reconstruction")
        print(f"PASS: raw Codex authorize fixture matches {UPSTREAM_COMMIT}")
    else:
        sys.stdout.buffer.write(result)


if __name__ == "__main__":
    main()
