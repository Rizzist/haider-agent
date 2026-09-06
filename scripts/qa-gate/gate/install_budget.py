"""Resource-accounted installer watchdogs shared by QA and native fixtures.

These are harness allowances, not claims of enforced product phase deadlines.
The helper verifies every member twice and makes seven full SHA-256 passes. A
serial sum remains safe when the implementation overlaps independent members.
"""
from __future__ import annotations

from pathlib import Path
import sys

from .contract import BudgetPart, BudgetSum, VERSION_QUERY

MEMBERS = ("haider", "haider-tui", "haiderd")
MAX_MEMBER_BYTES = 256 * 1024 * 1024  # update/staging.rs MAX_BINARY_BYTES
# Conservative harness service-rate floors. Production-linked debug SHA measured 13-15 MiB/s;
# reserve 8 MiB/s under contention. Disk/XZ work gets an independent 8-pass
# allowance at 16 MiB/s (archive read/hash/unpack + npm copy + stage/commit IO).
HASH_BYTES_PER_SECOND = 8 * 1024 * 1024
IO_BYTES_PER_SECOND = 16 * 1024 * 1024
HASH_PASSES = 7
IO_PASSES = 8
FETCH_ATTEMPTS = 2
FETCH_CONNECT_SECONDS = 30
FETCH_TRANSFER_BYTES_PER_SECOND = 1024 * 1024
FETCH_ARCHIVE_BYTES = 128 * 1024 * 1024
FETCH_CHECKSUM_BYTES = 16 * 1024


def fetch_attempt_seconds(capacity: int) -> int:
    return FETCH_CONNECT_SECONDS + (capacity + FETCH_TRANSFER_BYTES_PER_SECOND - 1) // FETCH_TRANSFER_BYTES_PER_SECOND


INSTALL_FETCH = BudgetPart(
    "parallel archive and sidecar fetch, two attempts each",
    FETCH_ATTEMPTS * max(fetch_attempt_seconds(FETCH_ARCHIVE_BYTES), fetch_attempt_seconds(FETCH_CHECKSUM_BYTES)),
    "install.sh/npm: 2 * (30s connection + ceil(128MiB archive / 1MiB/s)) = 316s; "
    "concurrent checksum is smaller; retries retain independent attempt deadlines",
)


def install_budget(member_sizes: tuple[int, ...], *, old_bytes: int = 0) -> BudgetSum:
    if not member_sizes or any(size <= 0 for size in member_sizes) or old_bytes < 0:
        raise ValueError("installer budget requires positive member byte sizes")
    total = sum(member_sizes)
    count = len(member_sizes)
    return BudgetSum((
        BudgetPart("seven complete member SHA-256 passes and old-prefix digest", (HASH_PASSES * total + old_bytes) / HASH_BYTES_PER_SECOND,
                   "update/staging.rs source/copy/freeze/immutable; transaction.rs immutable/installed/finalize plus old-prefix digest"),
        BudgetPart("archive, extraction, copies and durable publication IO", IO_PASSES * total / IO_BYTES_PER_SECOND,
                   "harness 16 MiB/s IO service floor over eight complete member-byte passes"),
        BudgetPart("staged and installed member version probes", 2 * count * VERSION_QUERY.seconds,
                   "two version probes per runtime member; shared VERSION_QUERY allowance"),
        BudgetPart("member quarantine/signature work", count * VERSION_QUERY.seconds,
                   "aggregate harness policy per member for xattr/sign/verify work, not an enforced per-command deadline"),
        BudgetPart("staged CLI offline self-test", VERSION_QUERY.seconds,
                   "one shared cold-query allowance for the staged CLI offline self-test"),
    ))


def native_member_sizes(directory: Path, extension: str = "") -> tuple[int, ...]:
    names = MEMBERS + (("haider-wayland-portal",) if sys.platform.startswith("linux") else ())
    return tuple((directory / (name + extension)).stat().st_size for name in names)


# The loader must declare a bound before a Context/native directory exists.
# Use actual Rust per-member input capacity and include Linux's optional fourth
# companion. Remote assets need not have the local debug build's byte sizes;
# native fixtures use their measured sizes instead of this capacity envelope.
MAX_INSTALL = install_budget((MAX_MEMBER_BYTES,) * (len(MEMBERS) + 1),
                             old_bytes=MAX_MEMBER_BYTES * (len(MEMBERS) + 1))
