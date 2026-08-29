#!/usr/bin/env python3
"""Fail when per-crate Rust unsafe-block counts differ from the reviewed baseline."""

from __future__ import annotations

import argparse
import json
import subprocess
import sys
import tomllib
from dataclasses import dataclass
from itertools import product
from pathlib import Path


BASELINE = Path("ci/unsafe-counts.json")


@dataclass(frozen=True)
class Token:
    text: str
    line: int


def _consume_quoted(source: str, start: int, quote: str) -> int:
    index = start + 1
    while index < len(source):
        if source[index] == "\\":
            index += 2
        elif source[index] == quote:
            return index + 1
        else:
            index += 1
    return len(source)


def _raw_string_end(source: str, start: int) -> int | None:
    prefix_length = 0
    for prefix in ("br", "cr", "r"):
        if source.startswith(prefix, start):
            prefix_length = len(prefix)
            break
    if prefix_length == 0:
        return None
    cursor = start + prefix_length
    hashes = 0
    while cursor < len(source) and source[cursor] == "#":
        hashes += 1
        cursor += 1
    if cursor >= len(source) or source[cursor] != '"':
        return None
    terminator = '"' + ("#" * hashes)
    end = source.find(terminator, cursor + 1)
    return len(source) if end < 0 else end + len(terminator)


def lex_rust(source: str) -> list[Token]:
    tokens: list[Token] = []
    index = 0
    line = 1
    while index < len(source):
        character = source[index]
        if character.isspace():
            line += character == "\n"
            index += 1
            continue
        if source.startswith("//", index):
            end = source.find("\n", index + 2)
            index = len(source) if end < 0 else end
            continue
        if source.startswith("/*", index):
            start = index
            depth = 1
            index += 2
            while index < len(source) and depth:
                if source.startswith("/*", index):
                    depth += 1
                    index += 2
                elif source.startswith("*/", index):
                    depth -= 1
                    index += 2
                else:
                    index += 1
            line += source.count("\n", start, index)
            continue
        raw_end = _raw_string_end(source, index)
        if raw_end is not None:
            tokens.append(Token(source[index:raw_end], line))
            line += source.count("\n", index, raw_end)
            index = raw_end
            continue
        if source.startswith("r#", index) and index + 2 < len(source) and (
            source[index + 2].isalpha() or source[index + 2] == "_"
        ):
            end = index + 3
            while end < len(source) and (source[end].isalnum() or source[end] == "_"):
                end += 1
            tokens.append(Token(source[index:end], line))
            index = end
            continue
        if character == '"' or (
            character in "bc"
            and index + 1 < len(source)
            and source[index + 1] == '"'
        ):
            quote_start = index if character == '"' else index + 1
            end = _consume_quoted(source, quote_start, '"')
            tokens.append(Token(source[index:end], line))
            line += source.count("\n", index, end)
            index = end
            continue
        if character == "'":
            cursor = index + 1
            if cursor < len(source) and (source[cursor].isalpha() or source[cursor] == "_"):
                while cursor < len(source) and (
                    source[cursor].isalnum() or source[cursor] == "_"
                ):
                    cursor += 1
                if cursor >= len(source) or source[cursor] != "'":
                    tokens.append(Token("'", line))
                    index += 1
                    continue
            end = _consume_quoted(source, index, "'")
            tokens.append(Token(source[index:end], line))
            line += source.count("\n", index, end)
            index = end
            continue
        if character == "b" and index + 1 < len(source) and source[index + 1] == "'":
            end = _consume_quoted(source, index + 1, "'")
            tokens.append(Token(source[index:end], line))
            line += source.count("\n", index, end)
            index = end
            continue
        if character.isalpha() or character == "_":
            end = index + 1
            while end < len(source) and (source[end].isalnum() or source[end] == "_"):
                end += 1
            tokens.append(Token(source[index:end], line))
            index = end
            continue
        tokens.append(Token(character, line))
        index += 1
    return tokens


CfgExpression = tuple[str, object]


def _parse_cfg_expression(words: list[str], start: int) -> tuple[CfgExpression, int] | None:
    if start >= len(words):
        return None
    operator = words[start]
    if operator in {"all", "any", "not"} and start + 1 < len(words) and words[start + 1] == "(":
        cursor = start + 2
        operands: list[CfgExpression] = []
        while cursor < len(words) and words[cursor] != ")":
            parsed = _parse_cfg_expression(words, cursor)
            if parsed is None:
                return None
            expression, cursor = parsed
            operands.append(expression)
            if cursor < len(words) and words[cursor] == ",":
                cursor += 1
            elif cursor >= len(words) or words[cursor] != ")":
                return None
        if cursor >= len(words) or words[cursor] != ")":
            return None
        if operator == "not" and len(operands) != 1:
            return None
        return (operator, tuple(operands)), cursor + 1

    cursor = start + 1
    nested = 0
    while cursor < len(words):
        if words[cursor] == "(":
            nested += 1
        elif words[cursor] == ")":
            if nested == 0:
                break
            nested -= 1
        elif words[cursor] == "," and nested == 0:
            break
        cursor += 1
    atom = "".join(words[start:cursor])
    return (("constant", False) if atom == "test" else ("atom", atom)), cursor


def _cfg_atoms(expression: CfgExpression) -> set[str]:
    kind, value = expression
    if kind == "atom":
        return {str(value)}
    if kind == "constant":
        return set()
    atoms: set[str] = set()
    for operand in value:
        atoms.update(_cfg_atoms(operand))
    return atoms


def _evaluate_cfg(expression: CfgExpression, assignment: dict[str, bool]) -> bool:
    kind, value = expression
    if kind == "atom":
        return assignment[str(value)]
    if kind == "constant":
        return bool(value)
    operands = tuple(_evaluate_cfg(operand, assignment) for operand in value)
    if kind == "all":
        return all(operands)
    if kind == "any":
        return any(operands)
    return not operands[0]


def _cfg_can_be_true_with_test_disabled(expression: CfgExpression) -> bool:
    atoms = sorted(_cfg_atoms(expression))
    # Exact SAT is exponential in arbitrary Boolean cfg syntax. Real Rust cfg
    # attributes are tiny; fail closed instead of hanging or approximating if
    # a future source exceeds this reviewed scanner bound.
    if len(atoms) > 16:
        raise RuntimeError(
            f"unsafe scanner cfg expression has {len(atoms)} atoms; maximum is 16"
        )
    for values in product((False, True), repeat=len(atoms)):
        if _evaluate_cfg(expression, dict(zip(atoms, values, strict=True))):
            return True
    return False


def _attribute_is_test_only(tokens: list[Token]) -> bool:
    words = [token.text for token in tokens]
    if words == ["test"]:
        return True
    if len(words) < 4 or words[:2] != ["cfg", "("] or words[-1] != ")":
        return False
    parsed = _parse_cfg_expression(words, 2)
    return (
        parsed is not None
        and parsed[1] == len(words) - 1
        and not _cfg_can_be_true_with_test_disabled(parsed[0])
    )


def _rust_string_literal(token: str) -> str | None:
    if token.startswith('"'):
        try:
            value = json.loads(token)
        except json.JSONDecodeError:
            return None
        return value if isinstance(value, str) else None
    if token.startswith("r"):
        quote = token.find('"')
        if quote < 1:
            return None
        hashes = token[1:quote]
        if any(character != "#" for character in hashes):
            return None
        suffix = '"' + hashes
        if not token.endswith(suffix):
            return None
        return token[quote + 1 : -len(suffix)]
    return None


def _validate_path_attribute(
    tokens: list[Token], source_path: Path | None, package_root: Path | None
) -> None:
    words = [token.text for token in tokens]
    if not words or words[0] != "path":
        return
    if len(words) != 3 or words[1] != "=":
        raise RuntimeError("unsafe scanner refuses an unsupported #[path] attribute")
    target_text = _rust_string_literal(words[2])
    if target_text is None or not target_text.endswith(".rs"):
        raise RuntimeError("unsafe scanner refuses a non-.rs #[path] target")
    if source_path is None or package_root is None:
        return
    target = (source_path.parent / target_text).resolve()
    package = package_root.resolve()
    if not target.is_relative_to(package):
        raise RuntimeError(
            f"unsafe scanner refuses out-of-crate #[path] target {target_text!r}"
        )
    if not target.is_file():
        raise RuntimeError(
            f"unsafe scanner #[path] target does not exist: {target_text!r}"
        )


def count_source(
    source: str,
    whole_file_is_test: bool,
    source_path: Path | None = None,
    package_root: Path | None = None,
) -> tuple[int, int]:
    tokens = lex_rust(source)
    macro_group_depths: list[tuple[str, int]] = []
    production = 0
    test = 0
    depth = 0
    paren_depth = 0
    bracket_depth = 0
    test_scope_depths: list[int] = []
    pending_test_item = False
    pending_origin = (0, 0, 0)
    pending_head_kind: str | None = None
    pending_angle_depth = 0
    index = 0
    while index < len(tokens):
        token = tokens[index]
        if (
            token.text == "include"
            and index + 1 < len(tokens)
            and tokens[index + 1].text == "!"
        ):
            raise RuntimeError(
                f"unsafe scanner refuses include! source expansion at line {token.line}"
            )
        if token.text in {"(", "[", "{"}:
            prior = tokens[index - 1].text if index > 0 else ""
            prior_prior = tokens[index - 2].text if index > 1 else ""
            starts_macro_rules = (
                index > 2
                and tokens[index - 3].text == "macro_rules"
                and prior_prior == "!"
            )
            if (prior == "!" and prior_prior != "#") or starts_macro_rules:
                macro_group_depths.append((token.text, 1))
            elif macro_group_depths:
                opener, group_depth = macro_group_depths[-1]
                if token.text == opener:
                    macro_group_depths[-1] = (opener, group_depth + 1)
        elif token.text in {")", "]", "}"} and macro_group_depths:
            opener, group_depth = macro_group_depths[-1]
            matching = {"(": ")", "[": "]", "{": "}"}[opener]
            if token.text == matching:
                if group_depth == 1:
                    macro_group_depths.pop()
                else:
                    macro_group_depths[-1] = (opener, group_depth - 1)
        if token.text == "#":
            cursor = index + 1
            inner_attribute = False
            if cursor < len(tokens) and tokens[cursor].text == "!":
                inner_attribute = True
                cursor += 1
            if cursor < len(tokens) and tokens[cursor].text == "[":
                attribute_start = cursor + 1
                attribute_depth = 1
                cursor += 1
                while cursor < len(tokens) and attribute_depth:
                    attribute_depth += tokens[cursor].text == "["
                    attribute_depth -= tokens[cursor].text == "]"
                    cursor += 1
                if attribute_depth == 0:
                    attribute_tokens = tokens[attribute_start : cursor - 1]
                    _validate_path_attribute(
                        attribute_tokens, source_path, package_root
                    )
                    attribute_is_test_only = _attribute_is_test_only(attribute_tokens)
                    if attribute_is_test_only and inner_attribute:
                        if not test_scope_depths or test_scope_depths[-1] != depth:
                            test_scope_depths.append(depth)
                    elif attribute_is_test_only:
                        pending_test_item = True
                        pending_origin = (depth, paren_depth, bracket_depth)
                        pending_head_kind = None
                        pending_angle_depth = 0
                index = cursor
                continue
        if token.text == "unsafe" and index + 1 < len(tokens):
            next_token = tokens[index + 1].text
            if next_token == "{":
                if whole_file_is_test or pending_test_item or test_scope_depths:
                    test += 1
                else:
                    production += 1
            elif macro_group_depths or next_token == "$":
                raise RuntimeError(
                    f"unsafe scanner refuses macro-generated unsafe syntax at line {token.line}"
                )
            elif next_token not in {"(", "extern", "fn", "impl", "trait"}:
                raise RuntimeError(
                    f"unsafe scanner found unsupported unsafe syntax at line {token.line}"
                )
        if token.text == "{":
            depth += 1
            if pending_test_item and (
                (depth - 1, paren_depth, bracket_depth) == pending_origin
                and pending_angle_depth == 0
            ):
                test_scope_depths.append(depth)
                pending_test_item = False
                pending_head_kind = None
        elif token.text == "}":
            if test_scope_depths and test_scope_depths[-1] == depth:
                test_scope_depths.pop()
            depth = max(0, depth - 1)
        elif pending_test_item:
            item_keywords = {
                "const",
                "enum",
                "fn",
                "impl",
                "macro_rules",
                "mod",
                "static",
                "struct",
                "trait",
                "type",
                "union",
                "use",
            }
            if (
                pending_head_kind is None
                and (depth, paren_depth, bracket_depth) == pending_origin
                and pending_angle_depth == 0
            ):
                if token.text in item_keywords and paren_depth == 0 and bracket_depth == 0:
                    pending_head_kind = "item"
                elif (
                    token.text[0].isalpha()
                    and token.text not in {"async", "auto", "default", "extern", "pub", "unsafe"}
                ):
                    pending_head_kind = "other"
            elif token.text == "<" and (
                depth, paren_depth, bracket_depth
            ) == pending_origin:
                pending_angle_depth += 1
            elif token.text == ">" and (
                depth, paren_depth, bracket_depth
            ) == pending_origin:
                pending_angle_depth = max(0, pending_angle_depth - 1)
            if (
                token.text == ";"
                and (depth, paren_depth, bracket_depth) == pending_origin
            ) or (
                token.text == ","
                and (depth, paren_depth, bracket_depth) == pending_origin
                and pending_head_kind != "item"
            ):
                pending_test_item = False
                pending_head_kind = None
                pending_angle_depth = 0
        if token.text == "(":
            paren_depth += 1
        elif token.text == ")":
            paren_depth = max(0, paren_depth - 1)
        elif token.text == "[":
            bracket_depth += 1
        elif token.text == "]":
            bracket_depth = max(0, bracket_depth - 1)
        index += 1
    return production, test


def _is_test_file(relative: Path) -> bool:
    return (
        "tests" in relative.parts
        or "benches" in relative.parts
        or relative.stem.endswith("_tests")
        or relative.name == "tests.rs"
    )


def _workspace_files(root: Path) -> list[Path]:
    command = subprocess.run(
        [
            "git",
            "ls-files",
            "--cached",
            "--others",
            "--exclude-standard",
            "-z",
        ],
        cwd=root,
        check=False,
        stdout=subprocess.PIPE,
        stderr=subprocess.PIPE,
    )
    if command.returncode != 0:
        detail = command.stderr.decode("utf-8", errors="replace").strip()
        raise RuntimeError(f"unsafe scanner could not enumerate workspace files: {detail}")
    return [
        root / encoded.decode("utf-8")
        for encoded in command.stdout.split(b"\0")
        if encoded
    ]


def _count_unconventional_source(source: str) -> int:
    tokens = lex_rust(source)
    macro_group_depths: list[tuple[str, int]] = []
    count = 0
    for index, token in enumerate(tokens):
        if token.text in {"(", "[", "{"}:
            prior = tokens[index - 1].text if index > 0 else ""
            prior_prior = tokens[index - 2].text if index > 1 else ""
            starts_macro_rules = (
                index > 2
                and tokens[index - 3].text == "macro_rules"
                and prior_prior == "!"
            )
            if (prior == "!" and prior_prior != "#") or starts_macro_rules:
                macro_group_depths.append((token.text, 1))
            elif macro_group_depths:
                opener, group_depth = macro_group_depths[-1]
                if token.text == opener:
                    macro_group_depths[-1] = (opener, group_depth + 1)
        elif token.text in {")", "]", "}"} and macro_group_depths:
            opener, group_depth = macro_group_depths[-1]
            matching = {"(": ")", "[": "]", "{": "}"}[opener]
            if token.text == matching:
                if group_depth == 1:
                    macro_group_depths.pop()
                else:
                    macro_group_depths[-1] = (opener, group_depth - 1)

        next_token = tokens[index + 1].text if index + 1 < len(tokens) else ""
        if token.text == "include" and next_token == "!":
            raise RuntimeError(
                f"unsafe scanner refuses nested source expansion at line {token.line}"
            )
        if token.text != "unsafe":
            continue
        if next_token == "{":
            count += 1
        elif macro_group_depths or next_token == "$":
            raise RuntimeError(
                f"unsafe scanner refuses macro-generated unsafe syntax at line {token.line}"
            )
    return count


def workspace_counts(root: Path) -> dict[str, dict[str, int]]:
    with (root / "Cargo.toml").open("rb") as handle:
        workspace = tomllib.load(handle)
    counts: dict[str, dict[str, int]] = {}
    package_names_by_root: dict[Path, str] = {}
    for member in workspace["workspace"]["members"]:
        package_root = (root / member).resolve()
        with (package_root / "Cargo.toml").open("rb") as handle:
            manifest = tomllib.load(handle)
        package_name = manifest["package"]["name"]
        package_names_by_root[package_root] = package_name
        package_counts = {"production": 0, "test": 0}
        for source_path in sorted(package_root.rglob("*.rs")):
            relative = source_path.relative_to(package_root)
            production, test = count_source(
                source_path.read_text(encoding="utf-8"),
                _is_test_file(relative),
                source_path,
                package_root,
            )
            package_counts["production"] += production
            package_counts["test"] += test
        counts[package_name] = package_counts

    # Rust accepts source from any filename through macro-generated source and
    # module-path expansion. Count unsafe blocks in every other pending workspace
    # file conservatively as production so changing a file extension cannot evade
    # the reviewed per-crate baseline.
    for candidate in _workspace_files(root):
        if not candidate.is_file():
            continue
        resolved = candidate.resolve()
        owner = next(
            (
                (package_root, package_name)
                for package_root, package_name in package_names_by_root.items()
                if resolved.is_relative_to(package_root)
            ),
            None,
        )
        if candidate.suffix == ".rs" and owner is not None:
            continue
        encoded = candidate.read_bytes()
        if b"unsafe" not in encoded:
            continue
        try:
            unconventional_source = encoded.decode("utf-8")
        except UnicodeDecodeError as error:
            raise RuntimeError(
                f"unsafe scanner cannot inspect non-UTF-8 candidate {candidate}: {error}"
            ) from error
        unsafe_count = _count_unconventional_source(unconventional_source)
        if unsafe_count == 0:
            continue
        if owner is None:
            raise RuntimeError(
                f"unsafe scanner cannot attribute unsafe block(s) in {candidate}"
            )
        counts[owner[1]]["production"] += unsafe_count
    return dict(sorted(counts.items()))


def run_self_tests() -> None:
    fixtures = [
        ("unsafe { ffi(); }", False, (1, 0)),
        ("unsafe\n{ ffi(); }", False, (1, 0)),
        ('"unsafe {"; r#"unsafe {"#; // unsafe {\n', False, (0, 0)),
        ("/* outer /* unsafe { */ still */ unsafe { ok(); }", False, (1, 0)),
        ("#[cfg(test)] mod tests { fn f() { unsafe { ffi(); } } }", False, (0, 1)),
        ("#[cfg(all(test, windows))]\nfn f() { unsafe { ffi(); } }", False, (0, 1)),
        ("#[cfg(any(test, unix))]\nfn f() { unsafe { ffi(); } }", False, (1, 0)),
        (
            "#[cfg(not(any(test, windows)))]\nfn f() { unsafe { ffi(); } }",
            False,
            (1, 0),
        ),
        ("#[cfg(not(test))]\nfn f() { unsafe { ffi(); } }", False, (1, 0)),
        (
            "enum E { #[cfg(test)] A, B } fn f() { unsafe { ffi(); } }",
            False,
            (1, 0),
        ),
        (
            "#[cfg(test)] const B: bool = 1 < 2; fn f() { unsafe { ffi(); } }",
            False,
            (1, 0),
        ),
        (
            "enum E { #[cfg(test)] A = (1 < 2) as isize, B } "
            "fn f() { unsafe { ffi(); } }",
            False,
            (1, 0),
        ),
        (
            "struct S { #[cfg(test)] cb: fn(i32, i32), live: i32 } "
            "fn f() { unsafe { ffi(); } }",
            False,
            (1, 0),
        ),
        (
            "struct S(#[cfg(test)] fn(i32), i32); fn f() { unsafe { ffi(); } }",
            False,
            (1, 0),
        ),
        (
            "#[cfg(test)] fn f(a: i32, b: i32) { unsafe { ffi(); } }",
            False,
            (0, 1),
        ),
        (
            "#[cfg(test)] fn f<T, U>() { unsafe { ffi(); } }",
            False,
            (0, 1),
        ),
        (
            "#[cfg(test)] fn f<const B: bool = { 1 < 2 }>() { unsafe { ffi(); } } "
            "fn p() { unsafe { ffi(); } }",
            False,
            (1, 1),
        ),
        (
            "#[cfg(all(any(test, unix), not(unix)))] fn f() { unsafe { ffi(); } }",
            False,
            (0, 1),
        ),
        (
            "mod tests { #![cfg(test)] fn f() { unsafe { ffi(); } } } "
            "fn production() { unsafe { ffi(); } }",
            False,
            (1, 1),
        ),
        ("#![cfg(test)] fn f() { unsafe { ffi(); } }", False, (0, 1)),
        ("fn f() { r#unsafe { not_a_keyword(); } }", False, (0, 0)),
        ("fn f() { unsafe fn declaration() {} }", True, (0, 0)),
        ("fn f() { unsafe { ffi(); } }", True, (0, 1)),
    ]
    for source, whole_file_is_test, expected in fixtures:
        actual = count_source(source, whole_file_is_test)
        if actual != expected:
            raise RuntimeError(
                f"unsafe scanner self-test failed: expected={expected} actual={actual}"
            )
    unsupported_macros = [
        "macro_rules! x { ($body:block) => { unsafe $body } }",
        "macro_rules! x { ($kw:ident, $body:block) => { $kw $body } } "
        "x!(unsafe, { ffi(); });",
        "macro_rules! x { ($u:ident $d:ident, $body:block) => { $u $body } } "
        "x!(unsafe fn, { ffi(); });",
    ]
    for source in unsupported_macros:
        try:
            count_source(source, False)
        except RuntimeError:
            continue
        raise RuntimeError("unsafe scanner did not reject macro-generated unsafe syntax")
    unsupported_includes = [
        "include!(\"unsafe.inc\");",
        '#[path = "unsafe.inc"] mod generated;',
        '#[path = "../../outside.rs"] mod generated;',
    ]
    for source in unsupported_includes:
        try:
            count_source(source, False, Path("/crate/src/lib.rs"), Path("/crate"))
        except RuntimeError:
            continue
        raise RuntimeError("unsafe scanner did not reject external Rust source")
    if _count_unconventional_source("text unsafe { ffi(); }") != 1:
        raise RuntimeError("unsafe scanner did not find unconventional-source unsafe")
    try:
        _count_unconventional_source(
            "macro_rules! y { ($body:block) => { unsafe $body } }"
        )
    except RuntimeError:
        pass
    else:
        raise RuntimeError("unsafe scanner accepted unconventional macro-generated unsafe")
    if not _is_test_file(Path("src/nested_tests.rs")):
        raise RuntimeError("unsafe scanner did not classify *_tests.rs as test code")


def main() -> int:
    parser = argparse.ArgumentParser()
    parser.add_argument("--update", action="store_true")
    parser.add_argument("--root", type=Path, default=Path(__file__).resolve().parents[1])
    arguments = parser.parse_args()
    root = arguments.root.resolve()
    run_self_tests()
    actual = workspace_counts(root)
    baseline_path = root / BASELINE
    if arguments.update:
        baseline_path.parent.mkdir(parents=True, exist_ok=True)
        baseline_path.write_text(
            json.dumps({"schema": 1, "counts": actual}, indent=2) + "\n",
            encoding="utf-8",
        )
        print(f"unsafe-count baseline explicitly updated: {baseline_path}")
        return 0
    try:
        baseline_document = json.loads(baseline_path.read_text(encoding="utf-8"))
    except (OSError, json.JSONDecodeError) as error:
        print(f"unsafe-count gate: invalid or missing baseline: {error}", file=sys.stderr)
        return 1
    if baseline_document.get("schema") != 1:
        print("unsafe-count gate: unsupported baseline schema", file=sys.stderr)
        return 1
    baseline = baseline_document.get("counts")
    if not isinstance(baseline, dict):
        print("unsafe-count gate: baseline counts must be an object", file=sys.stderr)
        return 1
    failures: list[str] = []
    baseline_crates = set(baseline)
    actual_crates = set(actual)
    for crate_name in sorted(actual_crates - baseline_crates):
        failures.append(f"crate={crate_name} is missing from the reviewed baseline")
    for crate_name in sorted(baseline_crates - actual_crates):
        failures.append(f"baseline contains removed or renamed crate={crate_name}")
    for crate_name in sorted(actual_crates & baseline_crates):
        for category in ("production", "test"):
            expected = baseline[crate_name].get(category)
            measured = actual[crate_name][category]
            if not isinstance(expected, int) or expected < 0:
                failures.append(
                    f"crate={crate_name} category={category} has an invalid baseline value"
                )
                continue
            if measured != expected:
                delta = measured - expected
                direction = "increased" if delta > 0 else "decreased"
                failures.append(
                    f"crate={crate_name} category={category} {direction}: "
                    f"baseline={expected} actual={measured} delta={delta:+d}"
                )
    if failures:
        print("unsafe-count gate: FAIL", file=sys.stderr)
        for failure in failures:
            print(f"  {failure}", file=sys.stderr)
        print(
            "Review the unsafe change, then run "
            "`python scripts/check-unsafe-counts.py --update` and commit the baseline diff.",
            file=sys.stderr,
        )
        return 1
    production = sum(count["production"] for count in actual.values())
    test = sum(count["test"] for count in actual.values())
    print(f"unsafe-count gate: PASS production={production} test={test}")
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
