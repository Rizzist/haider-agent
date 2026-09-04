//! Daemon-owned installation engine for Google's Antigravity ACP agent.
//!
//! Antigravity ships as a large proprietary ZIP from `dl.google.com`. The ACP
//! registry entry (`antigravity-acp`, publisher Google LLC, license
//! proprietary) publishes **no checksum and no size**: the registry schema
//! does define an optional per-target `sha256`, and other agents in that same
//! registry populate it, but Google's entry omits it and `dl.google.com`
//! returns no `x-goog-hash` header either. Haider therefore owns the integrity
//! pin itself — see [`ANTIGRAVITY_PINS`], whose digests and sizes are
//! release-measured and copied verbatim from
//! `docs/testing/v0.0.970/_antigravity-pins.md`.
//!
//! The install discipline is `haider-stt`'s `download.rs` — stream into a temp
//! file beside the final path, hash while streaming, verify before publishing,
//! and remove the temp file on any refusal so a failed install leaves nothing
//! behind. The catalog discipline is `typed_agent_installer.rs`: a closed
//! table, structured argv, and bounded typed errors; nothing caller-supplied,
//! model-supplied or config-supplied can become a download URL.
//!
//! Laws enforced here:
//!
//! - **The URL comes from the pin only.** [`AntigravityPin`] has private
//!   fields and no public constructor, so the only pins that exist outside
//!   this module are the five in [`ANTIGRAVITY_PINS`]. Every hop of the
//!   transfer is additionally checked against [`APPROVED_ARCHIVE_HOSTS`] by an
//!   explicit redirect policy — a redirect off the approved Google origin is
//!   refused, not followed.
//! - **Hash the bytes that land on disk.** `dl.google.com` gzips these
//!   archives in transit, and the pin describes the ZIP file itself, so the
//!   request asks for `identity` encoding and the digest is taken over the
//!   decoded bytes actually written. A comparison against a gzip-encoded body
//!   length would mismatch.
//! - **Every ZIP entry is hostile.** Entry count, entry names, entry types,
//!   per-entry and total uncompressed size, and compression ratio are all
//!   screened before extraction and re-checked incrementally during it. The
//!   destination basename is one of the two pinned constants, never a name
//!   taken from the archive.
//! - **Immutable versions, atomic pointer.** A version tree is published by a
//!   single `rename(2)` of a fully verified staging directory, and only then
//!   does the `active` pointer move, through
//!   [`haider_platform::replace_file`] plus a directory sync.
//! - **Leases keep a running child's executable alive.** A version with a live
//!   lease is never removed or replaced. Liveness is decided by the kernel
//!   (an advisory `File::try_lock`), not by a timeout — see
//!   [`AntigravityInstaller::acquire_lease`].
//! - **No auto-upgrade in 970.** Nothing in this module spawns a task, runs on
//!   a timer, or moves the `active` pointer on its own. Installing a version
//!   is an explicit [`AntigravityInstaller::ensure_installed`] call, and a
//!   call that finds a valid pinned install performs no transfer and does not
//!   touch the pointer.

use std::fmt::{Display, Formatter};
use std::fs::{DirBuilder, File, OpenOptions, TryLockError};
use std::io::{Read as _, Seek as _, SeekFrom, Write as _};
use std::path::{Path, PathBuf};
use std::time::Duration;

use async_trait::async_trait;
use sha2::Digest as _;
use subtle::ConstantTimeEq as _;

// ---------------------------------------------------------------------------
// Release-owned pin table
// ---------------------------------------------------------------------------

/// One release-owned integrity pin: exactly which archive Haider will fetch
/// for one host, and exactly what it must hash to.
///
/// Fields are private and there is no public constructor, so the only values
/// of this type reachable from outside this module are the entries of
/// [`ANTIGRAVITY_PINS`]. That is what makes "the download URL can only come
/// from the release pin" a type-level property rather than a convention.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct AntigravityPin {
    platform_key: &'static str,
    version: &'static str,
    archive_url: &'static str,
    archive_size_bytes: u64,
    archive_sha256: &'static str,
    executable_name: &'static str,
    helper_name: &'static str,
    extra_args: &'static [&'static str],
}

impl AntigravityPin {
    /// Registry platform key (`darwin-aarch64`, `linux-x86_64`, ...).
    #[must_use]
    pub const fn platform_key(&self) -> &'static str {
        self.platform_key
    }

    /// Agent version this pin installs.
    #[must_use]
    pub const fn version(&self) -> &'static str {
        self.version
    }

    /// Release-pinned archive URL. The only URL this installer will fetch.
    #[must_use]
    pub const fn archive_url(&self) -> &'static str {
        self.archive_url
    }

    /// Exact size of the ZIP file on disk, after any transfer decoding.
    #[must_use]
    pub const fn archive_size_bytes(&self) -> u64 {
        self.archive_size_bytes
    }

    /// Lowercase-hex SHA-256 of the ZIP file on disk.
    #[must_use]
    pub const fn archive_sha256(&self) -> &'static str {
        self.archive_sha256
    }

    /// Archive-root name of the ACP executable.
    #[must_use]
    pub const fn executable_name(&self) -> &'static str {
        self.executable_name
    }

    /// Archive-root name of the sibling helper the executable requires.
    #[must_use]
    pub const fn helper_name(&self) -> &'static str {
        self.helper_name
    }

    /// Extra argv the registry entry requires after the executable.
    #[must_use]
    pub const fn extra_args(&self) -> &'static [&'static str] {
        self.extra_args
    }

    /// Test-only pin. Production code can only reach [`ANTIGRAVITY_PINS`], so
    /// the platform key and URL are fixed here: a fixture varies only the
    /// version, the integrity values, and the launch shape.
    #[cfg(test)]
    pub(crate) const fn for_test(
        version: &'static str,
        archive_size_bytes: u64,
        archive_sha256: &'static str,
        executable_name: &'static str,
        helper_name: &'static str,
        extra_args: &'static [&'static str],
    ) -> Self {
        Self {
            platform_key: "fixture",
            version,
            archive_url: "https://dl.google.com/agy-extensions/releases/fixture.zip",
            archive_size_bytes,
            archive_sha256,
            executable_name,
            helper_name,
            extra_args,
        }
    }
}

/// Unix archive-root name of the ACP executable.
const UNIX_EXECUTABLE: &str = "agy_acp_server.par";
/// Unix archive-root name of the sibling helper.
const UNIX_HELPER: &str = "localharness_external";
/// Windows archive-root name of the ACP executable.
const WINDOWS_EXECUTABLE: &str = "agy_acp_server.exe";
/// Windows archive-root name of the sibling helper.
const WINDOWS_HELPER: &str = "localharness_external.exe";

/// Linux — and only Linux — takes this extra argv, with an empty value, from
/// the registry entry.
const LINUX_EXTRA_ARGS: &[&str] = &["--uid="];
/// macOS and Windows take no extra argv.
const NO_EXTRA_ARGS: &[&str] = &[];

/// The release-owned pin table for Antigravity ACP agent 1.1.1.
///
/// Every digest and size below was measured first-hand on 2026-09-04 by
/// downloading the archive from the URL shown and hashing the bytes on disk,
/// then cross-checked against an unrelated third party's independently
/// published table; all five matched exactly. They are transcribed verbatim
/// from `docs/testing/v0.0.970/_antigravity-pins.md` — never retype a digest.
///
/// There is deliberately **no `darwin-x86_64` entry**: Google publishes no
/// Intel macOS build, and that host must fail with
/// [`AntigravityInstallError::UnsupportedPlatform`] rather than fall back to
/// another architecture's archive.
pub static ANTIGRAVITY_PINS: &[AntigravityPin] = &[
    AntigravityPin {
        platform_key: "darwin-aarch64",
        version: ANTIGRAVITY_VERSION,
        archive_url: "https://dl.google.com/agy-extensions/releases/macos/agy-acp-server-agy_acp_server_1.1.1-darwin-arm64.zip",
        archive_size_bytes: 316_014_828,
        archive_sha256: "fdfa915652cdb7ba8085cc8fffed072cbe009251aa2c951aabdda07a8c28a189",
        executable_name: UNIX_EXECUTABLE,
        helper_name: UNIX_HELPER,
        extra_args: NO_EXTRA_ARGS,
    },
    AntigravityPin {
        platform_key: "linux-x86_64",
        version: ANTIGRAVITY_VERSION,
        archive_url: "https://dl.google.com/agy-extensions/releases/linux/agy-acp-server-agy_acp_server_1.1.1-linux-x86_64.zip",
        archive_size_bytes: 681_969_407,
        archive_sha256: "38f62d01b32deb0907b3d39a71ec301fd36369f6ffd1cf262d4af385177f79df",
        executable_name: UNIX_EXECUTABLE,
        helper_name: UNIX_HELPER,
        extra_args: LINUX_EXTRA_ARGS,
    },
    AntigravityPin {
        platform_key: "linux-aarch64",
        version: ANTIGRAVITY_VERSION,
        archive_url: "https://dl.google.com/agy-extensions/releases/linux/agy-acp-server-agy_acp_server_1.1.1-linux-arm64.zip",
        archive_size_bytes: 656_572_786,
        archive_sha256: "ed69e64b308fcb123ab54bf3277bf9cb0d651064f885ea5aab0ff520c7175398",
        executable_name: UNIX_EXECUTABLE,
        helper_name: UNIX_HELPER,
        extra_args: LINUX_EXTRA_ARGS,
    },
    AntigravityPin {
        platform_key: "windows-x86_64",
        version: ANTIGRAVITY_VERSION,
        archive_url: "https://dl.google.com/agy-extensions/releases/windows/agy-acp-server-agy_acp_server_1.1.1-windows-x86_64.zip",
        archive_size_bytes: 468_238_392,
        archive_sha256: "47cb50eef14f0a4655d78cfcfda869bcea7aaee5f9787e936bc2935ea612c3b8",
        executable_name: WINDOWS_EXECUTABLE,
        helper_name: WINDOWS_HELPER,
        extra_args: NO_EXTRA_ARGS,
    },
    AntigravityPin {
        platform_key: "windows-aarch64",
        version: ANTIGRAVITY_VERSION,
        archive_url: "https://dl.google.com/agy-extensions/releases/windows/agy-acp-server-agy_acp_server_1.1.1-windows-arm64.zip",
        archive_size_bytes: 468_521_191,
        archive_sha256: "35f4b1f47ba6a3fea7b0a3e30010df5ea73a64b4f0e7cf991cddc673ddfbcafc",
        executable_name: WINDOWS_EXECUTABLE,
        helper_name: WINDOWS_HELPER,
        extra_args: NO_EXTRA_ARGS,
    },
];

/// Agent version every entry in [`ANTIGRAVITY_PINS`] installs.
pub const ANTIGRAVITY_VERSION: &str = "1.1.1";

/// Hosts a transfer for a pinned archive may touch, on any redirect hop.
pub const APPROVED_ARCHIVE_HOSTS: &[&str] = &["dl.google.com"];

/// Resolves `(os, arch)` — as spelled by [`std::env::consts`] — to its pin.
///
/// Intel macOS (`("macos", "x86_64")`) deliberately has no entry.
pub fn pin_for_platform(
    os: &str,
    arch: &str,
) -> Result<&'static AntigravityPin, AntigravityInstallError> {
    let platform_key = match (os, arch) {
        ("macos", "aarch64") => "darwin-aarch64",
        ("linux", "x86_64") => "linux-x86_64",
        ("linux", "aarch64") => "linux-aarch64",
        ("windows", "x86_64") => "windows-x86_64",
        ("windows", "aarch64") => "windows-aarch64",
        _ => {
            return Err(AntigravityInstallError::UnsupportedPlatform {
                os: os.to_owned(),
                arch: arch.to_owned(),
            });
        }
    };
    ANTIGRAVITY_PINS
        .iter()
        .find(|pin| pin.platform_key == platform_key)
        .ok_or_else(|| AntigravityInstallError::UnsupportedPlatform {
            os: os.to_owned(),
            arch: arch.to_owned(),
        })
}

/// Resolves the running host to its pin.
pub fn pin_for_host() -> Result<&'static AntigravityPin, AntigravityInstallError> {
    pin_for_platform(std::env::consts::OS, std::env::consts::ARCH)
}

/// True when `url` is an origin a pinned archive transfer may touch.
///
/// Checked for the pin's own URL before the request is sent and again for
/// every redirect hop, so a `Location` header cannot move the transfer off
/// Google's download origin.
#[must_use]
pub fn approved_archive_origin(url: &reqwest::Url) -> bool {
    url.scheme() == "https"
        && url
            .host_str()
            .is_some_and(|host| APPROVED_ARCHIVE_HOSTS.contains(&host))
}

// ---------------------------------------------------------------------------
// Bounds
//
// Every bound below carries the arithmetic it was derived from. The measured
// inputs are the release pin table and the first-hand darwin-arm64 archive
// listing (`agy_acp_server.par` 802,163,856 B + `localharness_external`
// 116,766,704 B = 918,930,560 B of content in a 316,014,828 B archive), plus
// the third-party linux figure of ~2.0e9 B extracted.
// ---------------------------------------------------------------------------

/// End-to-end transfer budget for one archive.
///
/// Largest pinned archive: 681,969,407 B (linux-x86_64) = 650.4 MiB.
/// Assumed floor bandwidth: 1 MiB/s (8 Mbit/s) — the slowest link on which
/// installing an 885 MiB - 2.0 GB agent is still worth attempting.
///   650.4 MiB / 1 MiB/s = 650 s of pure transfer.
/// Doubling covers TLS setup, server ramp-up and the final disk flush:
///   650 s x 2 = 1300 s, rounded up to 1800 s.
pub const DOWNLOAD_TIMEOUT: Duration = Duration::from_secs(1800);

/// TCP+TLS establishment budget. A healthy connection to a Google edge
/// completes in well under a second; 30 s is the point past which the network,
/// not the server, is the problem, and it fails fast instead of burning the
/// 1800 s transfer budget on a black hole.
pub const CONNECT_TIMEOUT: Duration = Duration::from_secs(30);

/// Per-read stall budget. At the 1 MiB/s floor bandwidth a 64 KiB chunk lands
/// in 64 KiB / 1 MiB/s = 0.0625 s, so 60 s is ~960x the expected inter-chunk
/// gap. Only a body that has genuinely stopped trips it, and it stops a server
/// that would otherwise dribble for the whole 1800 s budget.
pub const READ_STALL_TIMEOUT: Duration = Duration::from_secs(60);

/// Redirect hops a transfer may take, all of which must stay on an approved
/// host. `dl.google.com` serves these archives with a single `200`; 3 hops
/// absorb a future same-origin CDN indirection while keeping the chain bounded
/// (the custom policy replaces reqwest's own loop limit, which does not apply).
const MAX_REDIRECT_HOPS: usize = 3;

/// Central-directory records an archive may declare.
///
/// The pinned archives hold exactly 2 entries. 8 is 4x that headroom, so a
/// future Google build that adds a sibling is diagnosed as
/// [`AntigravityInstallError::UnexpectedEntry`] — which names the file — rather
/// than as a bare count refusal, while a million-entry directory bomb is
/// refused from the end-of-central-directory count before a single record is
/// parsed.
pub const MAX_ARCHIVE_ENTRIES: u64 = 8;

/// Uncompressed bytes one entry may produce.
///
/// Largest entry measured first-hand: `agy_acp_server.par` at 802,163,856 B
/// (darwin-arm64), i.e. 802,163,856 / 918,930,560 = 87.3 % of that archive's
/// content. Linux extracts to ~2.0e9 B, so its executable is about
/// 0.873 x 2.0e9 = 1.75e9 B. 3 GiB = 3,221,225,472 B is 1.84x that — room for
/// the linux binary to nearly double without admitting a bomb.
pub const MAX_ENTRY_UNCOMPRESSED_BYTES: u64 = 3 * 1024 * 1024 * 1024;

/// Uncompressed bytes a whole archive may produce.
///
/// Largest pinned extracted tree: linux at ~2.0e9 B (darwin measured at
/// 918,930,560 B). 4 GiB = 4,294,967,296 B is 2.1x the linux tree.
pub const MAX_TOTAL_UNCOMPRESSED_BYTES: u64 = 4 * 1024 * 1024 * 1024;

/// Uncompressed-to-compressed ratio one entry may reach.
///
/// Measured: darwin 918,930,560 / 316,014,828 = 2.91; linux
/// ~2.0e9 / 681,969,407 = 2.93. Both payloads are already-compact Mach-O/ELF
/// images, so ~2.9 is the real number. 20 is 6.9x it, while DEFLATE's
/// theoretical maximum is 1032:1 — a bomb is caught roughly 50x before it can
/// matter. Enforced incrementally, so the abort happens after writing at most
/// the allowance, never after materializing the declared size.
pub const MAX_COMPRESSION_RATIO: u64 = 20;

/// Bytes an entry may produce before the ratio guard means anything.
///
/// Below 64 KiB the ratio is dominated by per-entry framing rather than the
/// payload, so it is not evidence of a bomb. 64 KiB is
/// 65,536 / 3,221,225,472 = 0.002 % of the per-entry cap, so the floor cannot
/// mask one either.
pub const RATIO_FLOOR_BYTES: u64 = 64 * 1024;

/// Extraction read granularity. One 64 KiB read bounds how far past a cap the
/// extractor can write before it trips: at most 65,536 B, i.e. 0.002 % of the
/// per-entry cap.
const EXTRACT_CHUNK_BYTES: usize = 64 * 1024;

/// Tail the end-of-central-directory scan may look back over: the 22-byte EOCD
/// record, plus its maximum 65,535-byte comment, plus the 20-byte ZIP64
/// end-of-central-directory locator that may precede it.
///   22 + 65,535 + 20 = 65,577 B.
const EOCD_SEARCH_WINDOW: u64 = 65_577;

/// Size of a ZIP end-of-central-directory record without its comment.
const EOCD_FIXED_BYTES: usize = 22;
/// Size of a ZIP64 end-of-central-directory locator.
const ZIP64_LOCATOR_BYTES: usize = 20;
/// Bytes of a ZIP64 end-of-central-directory record this reader needs.
const ZIP64_EOCD_PREFIX_BYTES: usize = 40;

const EOCD_SIGNATURE: [u8; 4] = [0x50, 0x4b, 0x05, 0x06];
const ZIP64_LOCATOR_SIGNATURE: [u8; 4] = [0x50, 0x4b, 0x06, 0x07];
const ZIP64_EOCD_SIGNATURE: [u8; 4] = [0x50, 0x4b, 0x06, 0x06];

/// Bytes of a hostile entry name that may reach an error message.
///
/// The pinned names are 18 (`agy_acp_server.par`) and 21
/// (`localharness_external`) bytes; 128 gives 6x headroom while keeping a
/// crafted 64 KiB entry name out of the daemon's error surface.
const MAX_ENTRY_LABEL_BYTES: usize = 128;

/// Bytes a version string may occupy. The pinned version is `1.1.1` (5 bytes);
/// 64 leaves room for a long pre-release tag while bounding the one path
/// segment a version is ever allowed to become.
const MAX_VERSION_BYTES: usize = 64;

/// Bytes the `active` pointer file may hold: one 64-byte version segment plus
/// a trailing newline and any stray whitespace. Anything larger is not a
/// pointer this installer wrote.
const MAX_ACTIVE_POINTER_BYTES: u64 = 128;

/// Randomness in a lease or staging name.
///
/// 16 bytes = 128 bits. Even at 2^64 concurrent names the birthday collision
/// probability is still about 2^-1... per 2^64 draws, i.e. unreachable here;
/// unique names are what let the stale-lease sweeper delete only files it has
/// itself locked, with no chance of unlinking a successor's lease.
const RANDOM_NAME_BYTES: usize = 16;

/// Attempts to find an unused staging or lease name before giving up.
/// With 128-bit names a single attempt suffices; 16 covers a broken RNG
/// without spinning.
const MAX_STAGING_ATTEMPTS: usize = 16;

/// Owner-only directory mode for every directory this installer creates.
pub const DIRECTORY_MODE: u32 = 0o700;
/// Owner-only mode for installed executables. They are launched by the daemon,
/// so the owner-execute bit is required and no group/other bit is allowed.
pub const EXECUTABLE_MODE: u32 = 0o700;
/// Owner-only mode for the `active` pointer and lease files.
pub const CONTROL_FILE_MODE: u32 = 0o600;

/// Directory holding immutable, activated version trees.
pub const VERSIONS_DIRECTORY: &str = "versions";
/// Directory holding per-version lease files.
pub const LEASES_DIRECTORY: &str = "leases";
/// Directory holding in-flight and verified archives.
pub const DOWNLOADS_DIRECTORY: &str = "downloads";
/// File naming the currently activated version.
pub const ACTIVE_POINTER: &str = "active";
/// Suffix of an in-flight download, following `haider-stt`'s convention.
const DOWNLOAD_TEMP_SUFFIX: &str = ".download";

// ---------------------------------------------------------------------------
// Errors
// ---------------------------------------------------------------------------

/// Why a ZIP entry's name is not usable as a destination.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum EntryPathFault {
    /// The name is empty.
    Empty,
    /// The name begins with `/` or `\`.
    Absolute,
    /// The name carries a drive letter or a UNC prefix.
    DriveOrUnc,
    /// The name has a `..` component.
    ParentComponent,
    /// The name has a `.` component or a path separator, so it is not the flat
    /// archive-root file the pin describes.
    NotFlat,
    /// The name has a NUL byte or another control character.
    ControlByte,
}

impl Display for EntryPathFault {
    fn fmt(&self, formatter: &mut Formatter<'_>) -> std::fmt::Result {
        formatter.write_str(match self {
            Self::Empty => "empty name",
            Self::Absolute => "absolute path",
            Self::DriveOrUnc => "drive-letter or UNC path",
            Self::ParentComponent => "parent-directory component",
            Self::NotFlat => "not a flat archive-root name",
            Self::ControlByte => "control byte in the name",
        })
    }
}

/// Why a ZIP entry is not a plain regular file.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum EntryKindFault {
    /// A directory entry.
    Directory,
    /// A symbolic link, by name convention or by mode bits.
    Symlink,
    /// A device, socket, FIFO, or any other non-regular mode.
    NotRegular,
}

impl Display for EntryKindFault {
    fn fmt(&self, formatter: &mut Formatter<'_>) -> std::fmt::Result {
        formatter.write_str(match self {
            Self::Directory => "a directory",
            Self::Symlink => "a symbolic link",
            Self::NotRegular => "not a regular file",
        })
    }
}

/// Typed, bounded failure surface of the Antigravity installer.
///
/// Nothing here carries archive-controlled text unbounded: entry names are
/// sanitized and truncated to [`MAX_ENTRY_LABEL_BYTES`] before they reach a
/// message.
#[derive(Debug)]
pub enum AntigravityInstallError {
    /// Google publishes no build for this host. Notably Intel macOS; there is
    /// never a fallback to another architecture's archive.
    UnsupportedPlatform { os: String, arch: String },
    /// The HTTP client could not be built, the request failed, the status was
    /// not a success, or the body was interrupted.
    Http { message: String },
    /// A URL — the pin's own or a redirect hop's — is not on the approved
    /// Google download origin. Only the host is reported; the URL is not.
    OriginRefused { host: String },
    /// The body exceeded the pinned archive size mid-stream and was abandoned.
    ArchiveOverran { pinned_bytes: u64 },
    /// The completed body did not have the pinned archive size.
    ArchiveSizeMismatch { expected: u64, actual: u64 },
    /// The completed body did not have the pinned archive digest.
    ArchiveDigestMismatch { expected: String, actual: String },
    /// The pin table itself carries a digest that is not 64 lowercase hex
    /// characters. Unreachable while the table is release-owned; checked so a
    /// bad edit fails closed rather than skipping verification.
    MalformedPinDigest { platform_key: &'static str },
    /// The archive is not a readable ZIP, or its end-of-central-directory
    /// record is missing or inconsistent.
    MalformedArchive { message: String },
    /// The `active` pointer file is not something this installer wrote.
    MalformedActivePointer { path: PathBuf },
    /// The archive declares more central-directory records than the cap.
    EntryCountExceeded { declared: u64, limit: u64 },
    /// The archive declares more records than it has distinct names, so at
    /// least one name is repeated.
    DuplicateEntry { declared: u64, distinct: u64 },
    /// An entry name cannot be used as a destination.
    UnsafeEntryPath {
        entry: String,
        fault: EntryPathFault,
    },
    /// An entry is not a plain regular file.
    NonRegularEntry {
        entry: String,
        fault: EntryKindFault,
    },
    /// An entry is not one of the two the pin expects. An extra entry is a
    /// refusal, never a warning.
    UnexpectedEntry { entry: String },
    /// A pinned entry is absent from the archive.
    MissingEntry { entry: String },
    /// One entry's uncompressed output exceeded the per-entry cap.
    EntryTooLarge {
        entry: String,
        written_bytes: u64,
        limit: u64,
    },
    /// The archive's total uncompressed output exceeded the total cap.
    TotalTooLarge { written_bytes: u64, limit: u64 },
    /// One entry's compression ratio exceeded the bomb threshold. Reported
    /// with the bytes actually written, which is bounded by the allowance plus
    /// one read chunk — never the declared uncompressed size.
    CompressionRatioExceeded {
        entry: String,
        written_bytes: u64,
        allowance_bytes: u64,
    },
    /// An entry produced a different number of bytes than it declared.
    EntrySizeMismatch {
        entry: String,
        declared: u64,
        written_bytes: u64,
    },
    /// A version string is not usable as a single path segment.
    InvalidVersion { version: String },
    /// An installed or staged path is not a regular file.
    NotRegularFile { path: PathBuf },
    /// An installed path that must be a directory is not one.
    NotADirectory { path: PathBuf },
    /// An installed or staged file is empty.
    EmptyFile { path: PathBuf },
    /// An installed or staged path is not owned by the current user.
    NotOwnedByCurrentUser { path: PathBuf },
    /// An installed or staged path grants a group or other permission bit, or
    /// an executable is not owner-executable. Activation fails closed: a
    /// suspicious tree is never silently replaced.
    InsecurePermissions { path: PathBuf, mode: u32 },
    /// A live lease holds this version, so it is neither removed nor replaced.
    VersionLeased { version: String },
    /// A named filesystem step failed at a named path.
    Io {
        operation: &'static str,
        path: PathBuf,
        source: std::io::Error,
    },
    /// A daemon-owned facility failed (randomness, blocking worker join).
    Internal { message: String },
}

impl Display for AntigravityInstallError {
    fn fmt(&self, formatter: &mut Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::UnsupportedPlatform { os, arch } => write!(
                formatter,
                "Google publishes no Antigravity ACP build for {os}-{arch}"
            ),
            Self::Http { message } => write!(formatter, "antigravity archive transfer: {message}"),
            Self::OriginRefused { host } => write!(
                formatter,
                "antigravity archive transfer refused a hop to host `{host}`"
            ),
            Self::ArchiveOverran { pinned_bytes } => write!(
                formatter,
                "antigravity archive body exceeded its pinned {pinned_bytes} bytes"
            ),
            Self::ArchiveSizeMismatch { expected, actual } => write!(
                formatter,
                "antigravity archive is {actual} bytes, pinned at {expected}"
            ),
            Self::ArchiveDigestMismatch { expected, actual } => write!(
                formatter,
                "antigravity archive sha256 is {actual}, pinned at {expected}"
            ),
            Self::MalformedPinDigest { platform_key } => write!(
                formatter,
                "antigravity pin for `{platform_key}` has a malformed sha256"
            ),
            Self::MalformedArchive { message } => {
                write!(formatter, "antigravity archive is malformed: {message}")
            }
            Self::MalformedActivePointer { path } => write!(
                formatter,
                "antigravity active pointer {} is malformed",
                path.display()
            ),
            Self::EntryCountExceeded { declared, limit } => write!(
                formatter,
                "antigravity archive declares {declared} entries, limit {limit}"
            ),
            Self::DuplicateEntry { declared, distinct } => write!(
                formatter,
                "antigravity archive declares {declared} entries but only {distinct} distinct names"
            ),
            Self::UnsafeEntryPath { entry, fault } => write!(
                formatter,
                "antigravity archive entry `{entry}` is refused: {fault}"
            ),
            Self::NonRegularEntry { entry, fault } => {
                write!(formatter, "antigravity archive entry `{entry}` is {fault}")
            }
            Self::UnexpectedEntry { entry } => write!(
                formatter,
                "antigravity archive entry `{entry}` is not one the pin expects"
            ),
            Self::MissingEntry { entry } => write!(
                formatter,
                "antigravity archive is missing pinned entry `{entry}`"
            ),
            Self::EntryTooLarge {
                entry,
                written_bytes,
                limit,
            } => write!(
                formatter,
                "antigravity archive entry `{entry}` exceeded {limit} bytes after {written_bytes}"
            ),
            Self::TotalTooLarge {
                written_bytes,
                limit,
            } => write!(
                formatter,
                "antigravity archive exceeded {limit} uncompressed bytes after {written_bytes}"
            ),
            Self::CompressionRatioExceeded {
                entry,
                written_bytes,
                allowance_bytes,
            } => write!(
                formatter,
                "antigravity archive entry `{entry}` passed its {allowance_bytes}-byte compression allowance after {written_bytes}"
            ),
            Self::EntrySizeMismatch {
                entry,
                declared,
                written_bytes,
            } => write!(
                formatter,
                "antigravity archive entry `{entry}` declared {declared} bytes but produced {written_bytes}"
            ),
            Self::InvalidVersion { version } => write!(
                formatter,
                "antigravity version `{version}` is not a usable path segment"
            ),
            Self::NotRegularFile { path } => write!(
                formatter,
                "antigravity install path {} is not a regular file",
                path.display()
            ),
            Self::NotADirectory { path } => write!(
                formatter,
                "antigravity install path {} is not a directory",
                path.display()
            ),
            Self::EmptyFile { path } => write!(
                formatter,
                "antigravity install file {} is empty",
                path.display()
            ),
            Self::NotOwnedByCurrentUser { path } => write!(
                formatter,
                "antigravity install path {} is not owned by this user",
                path.display()
            ),
            Self::InsecurePermissions { path, mode } => write!(
                formatter,
                "antigravity install path {} has mode {mode:o}",
                path.display()
            ),
            Self::VersionLeased { version } => write!(
                formatter,
                "antigravity version `{version}` is held by a live lease"
            ),
            Self::Io {
                operation,
                path,
                source,
            } => write!(
                formatter,
                "could not {operation} {}: {source}",
                path.display()
            ),
            Self::Internal { message } => {
                write!(formatter, "antigravity installer failed: {message}")
            }
        }
    }
}

impl std::error::Error for AntigravityInstallError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            Self::Io { source, .. } => Some(source),
            _ => None,
        }
    }
}

fn io_error(
    operation: &'static str,
    path: &Path,
    source: std::io::Error,
) -> AntigravityInstallError {
    AntigravityInstallError::Io {
        operation,
        path: path.to_path_buf(),
        source,
    }
}

/// Bounds and de-fangs an archive-controlled name before it reaches a message.
///
/// Control bytes become `?` so a crafted name cannot inject newlines or
/// terminal escapes into a daemon log, and the result is truncated on a UTF-8
/// boundary at [`MAX_ENTRY_LABEL_BYTES`].
fn entry_label(name: &str) -> String {
    let mut label = String::with_capacity(name.len().min(MAX_ENTRY_LABEL_BYTES));
    for character in name.chars() {
        let character = if character.is_control() {
            '?'
        } else {
            character
        };
        if label.len() + character.len_utf8() > MAX_ENTRY_LABEL_BYTES {
            label.push('~');
            break;
        }
        label.push(character);
    }
    label
}

// ---------------------------------------------------------------------------
// Fetch seam
// ---------------------------------------------------------------------------

/// Verify-while-streaming destination for one pinned archive body.
///
/// The installer owns this type; an [`ArchiveSource`] only pushes bytes into
/// it. Size enforcement, hashing and the temp-file write therefore stay in
/// production code no matter which source is injected, and there is no
/// constructor a test source could use to hand back a pre-blessed result.
pub struct ArchiveSink {
    file: File,
    hasher: sha2::Sha256,
    written_bytes: u64,
    pinned_bytes: u64,
}

impl ArchiveSink {
    /// Accepts one body chunk.
    ///
    /// Aborts as soon as the running count would pass the pinned size, so a
    /// hostile server can neither fill the disk nor stream forever; the
    /// installer buffers nothing beyond this chunk.
    pub fn write_chunk(&mut self, chunk: &[u8]) -> Result<(), AntigravityInstallError> {
        let next = self.written_bytes.saturating_add(chunk.len() as u64);
        if next > self.pinned_bytes {
            return Err(AntigravityInstallError::ArchiveOverran {
                pinned_bytes: self.pinned_bytes,
            });
        }
        self.file
            .write_all(chunk)
            .map_err(|error| AntigravityInstallError::Http {
                message: format!("could not write archive chunk: {error}"),
            })?;
        self.hasher.update(chunk);
        self.written_bytes = next;
        Ok(())
    }

    /// Bytes accepted so far.
    #[must_use]
    pub fn written_bytes(&self) -> u64 {
        self.written_bytes
    }

    /// Exact size the pin says this archive must have.
    #[must_use]
    pub fn pinned_bytes(&self) -> u64 {
        self.pinned_bytes
    }
}

/// The injectable half of the fetch: everything that touches the network.
///
/// The filesystem half — temp file, hashing, size enforcement, verification,
/// extraction and activation — is not injectable, so a test can drive the
/// whole installer without a socket while still exercising the real
/// verification code.
#[async_trait]
pub trait ArchiveSource: Send + Sync {
    /// Streams the archive named by `pin` into `sink`.
    ///
    /// Implementations must take the URL from `pin` and from nowhere else, and
    /// must refuse any redirect that leaves [`APPROVED_ARCHIVE_HOSTS`].
    async fn stream_to(
        &self,
        pin: &AntigravityPin,
        sink: &mut ArchiveSink,
    ) -> Result<(), AntigravityInstallError>;
}

/// The production source: a reqwest client pinned to the Google origin.
pub struct HttpArchiveSource {
    client: reqwest::Client,
}

impl HttpArchiveSource {
    /// Builds the client with the transfer budgets and the origin-pinned
    /// redirect policy.
    pub fn new() -> Result<Self, AntigravityInstallError> {
        let policy = reqwest::redirect::Policy::custom(|attempt| {
            // The custom policy replaces reqwest's own loop limit, so the hop
            // bound has to be enforced here as well as the origin check.
            if attempt.previous().len() >= MAX_REDIRECT_HOPS {
                return attempt.error("antigravity archive redirect chain is too long");
            }
            if approved_archive_origin(attempt.url()) {
                attempt.follow()
            } else {
                attempt.error("antigravity archive redirect left the approved Google origin")
            }
        });
        let client = reqwest::Client::builder()
            .timeout(DOWNLOAD_TIMEOUT)
            .connect_timeout(CONNECT_TIMEOUT)
            .read_timeout(READ_STALL_TIMEOUT)
            .redirect(policy)
            .build()
            .map_err(|error| AntigravityInstallError::Http {
                message: format!("could not build the archive HTTP client: {error}"),
            })?;
        Ok(Self { client })
    }
}

#[async_trait]
impl ArchiveSource for HttpArchiveSource {
    async fn stream_to(
        &self,
        pin: &AntigravityPin,
        sink: &mut ArchiveSink,
    ) -> Result<(), AntigravityInstallError> {
        let url = reqwest::Url::parse(pin.archive_url).map_err(|error| {
            AntigravityInstallError::Http {
                message: format!("pinned archive URL is unusable: {error}"),
            }
        })?;
        // Defence in depth: the pin table is release-owned, but the origin law
        // is enforced here too so it holds for the request as well as for the
        // redirect hops.
        if !approved_archive_origin(&url) {
            return Err(AntigravityInstallError::OriginRefused {
                host: url.host_str().unwrap_or("<none>").to_owned(),
            });
        }
        // `dl.google.com` gzips these archives when the client asks for it,
        // and the pin describes the ZIP file itself. Asking for `identity`
        // keeps the bytes that land on disk identical to the bytes that were
        // hashed to make the pin.
        let mut response = self
            .client
            .get(url)
            .header(reqwest::header::ACCEPT_ENCODING, "identity")
            .send()
            .await
            .map_err(|error| AntigravityInstallError::Http {
                message: format!("request failed: {error}"),
            })?;
        if !response.status().is_success() {
            return Err(AntigravityInstallError::Http {
                message: format!("archive request returned HTTP {}", response.status()),
            });
        }
        while let Some(chunk) =
            response
                .chunk()
                .await
                .map_err(|error| AntigravityInstallError::Http {
                    message: format!("interrupted archive body: {error}"),
                })?
        {
            sink.write_chunk(&chunk)?;
        }
        Ok(())
    }
}

// ---------------------------------------------------------------------------
// Installation view
// ---------------------------------------------------------------------------

/// A verified, activated Antigravity install.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AntigravityInstallation {
    version: String,
    directory: PathBuf,
    executable: PathBuf,
    helper: PathBuf,
    args: Vec<String>,
}

impl AntigravityInstallation {
    /// Version this install provides.
    #[must_use]
    pub fn version(&self) -> &str {
        &self.version
    }

    /// Immutable version directory.
    #[must_use]
    pub fn directory(&self) -> &Path {
        &self.directory
    }

    /// Absolute path of the ACP executable to launch.
    #[must_use]
    pub fn executable(&self) -> &Path {
        &self.executable
    }

    /// Absolute path of the sibling helper the executable requires.
    #[must_use]
    pub fn helper(&self) -> &Path {
        &self.helper
    }

    /// Argv to pass after the executable, from the pin.
    #[must_use]
    pub fn args(&self) -> &[String] {
        &self.args
    }
}

/// What one [`AntigravityInstaller::ensure_installed`] call did.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum InstallOutcome {
    /// A valid install for the pinned version was already present. No
    /// transfer happened and the active pointer did not move.
    AlreadyPresent(AntigravityInstallation),
    /// The pinned archive was fetched, verified, extracted and activated.
    Installed(AntigravityInstallation),
}

impl InstallOutcome {
    /// The install this call left in place.
    #[must_use]
    pub fn installation(&self) -> &AntigravityInstallation {
        match self {
            Self::AlreadyPresent(installation) | Self::Installed(installation) => installation,
        }
    }
}

// ---------------------------------------------------------------------------
// Leases
// ---------------------------------------------------------------------------

/// A live claim on one installed version.
///
/// While a lease exists the version tree is never removed or replaced, so an
/// install cannot pull the executable out from under a running child.
///
/// Liveness is decided by the kernel, not by a timeout: the lease is an
/// advisory exclusive `File::try_lock` on its own uniquely named file. If the
/// holder dies — cleanly, by panic, or by `SIGKILL` — the operating system
/// drops the lock and the next sweeper reclaims the file. There is therefore
/// no staleness interval to derive and no way for a crashed holder to pin a
/// version forever.
#[derive(Debug)]
pub struct AntigravityLease {
    file: File,
    path: PathBuf,
    version: String,
}

impl Drop for AntigravityLease {
    fn drop(&mut self) {
        // Explicit unlock, not just handle close: `flock` locks belong to the
        // open file description, and a concurrently spawned child between
        // clone and exec keeps that description — and its lock — alive after
        // this process closes its own descriptor. The file itself is left for
        // the sweeper, which is exactly the crashed-holder path.
        let _ = self.file.unlock();
    }
}

impl AntigravityLease {
    /// Version this lease holds.
    #[must_use]
    pub fn version(&self) -> &str {
        &self.version
    }

    /// Lease file backing this claim.
    #[must_use]
    pub fn path(&self) -> &Path {
        &self.path
    }

    /// Releases the claim and removes the lease file.
    ///
    /// Safe to skip: a holder that dies without calling this leaves an
    /// unlocked file that the next sweep reclaims.
    pub fn release(self) -> Result<(), AntigravityInstallError> {
        self.file
            .unlock()
            .map_err(|error| io_error("release antigravity lease", &self.path, error))?;
        match std::fs::remove_file(&self.path) {
            Ok(()) => Ok(()),
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(()),
            Err(error) => Err(io_error("remove antigravity lease", &self.path, error)),
        }
    }
}

// ---------------------------------------------------------------------------
// Installer
// ---------------------------------------------------------------------------

/// Owner of one Antigravity install root.
///
/// Layout under the root, all `0700`:
///
/// ```text
/// versions/<version>/   immutable once activated
/// leases/<version>/     one file per live claim on that version
/// downloads/            in-flight and verified archives
/// active                the activated version, replaced atomically
/// ```
#[derive(Debug, Clone)]
pub struct AntigravityInstaller {
    root: PathBuf,
}

impl AntigravityInstaller {
    /// Binds an installer to a daemon-owned root. Nothing is created until a
    /// call needs it.
    #[must_use]
    pub fn new(root: impl Into<PathBuf>) -> Self {
        Self { root: root.into() }
    }

    /// Install root.
    #[must_use]
    pub fn root(&self) -> &Path {
        &self.root
    }

    fn versions_dir(&self) -> PathBuf {
        self.root.join(VERSIONS_DIRECTORY)
    }

    fn leases_dir(&self) -> PathBuf {
        self.root.join(LEASES_DIRECTORY)
    }

    fn downloads_dir(&self) -> PathBuf {
        self.root.join(DOWNLOADS_DIRECTORY)
    }

    fn active_pointer(&self) -> PathBuf {
        self.root.join(ACTIVE_POINTER)
    }

    /// Creates the root and its three subdirectories, all owner-only, and
    /// refuses a root that is not a directory this user owns.
    fn prepare_root(&self) -> Result<(), AntigravityInstallError> {
        create_directory_all(&self.root)?;
        verify_directory(&self.root)?;
        for directory in [self.versions_dir(), self.leases_dir(), self.downloads_dir()] {
            create_directory_all(&directory)?;
            verify_directory(&directory)?;
        }
        Ok(())
    }

    /// Reads the activated version, if the pointer names one.
    ///
    /// The pointer's bytes are bounded and validated as a single path segment
    /// before they are ever joined onto a path.
    pub fn active_version(&self) -> Result<Option<String>, AntigravityInstallError> {
        let path = self.active_pointer();
        let metadata = match std::fs::symlink_metadata(&path) {
            Ok(metadata) => metadata,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(None),
            Err(error) => return Err(io_error("inspect antigravity active pointer", &path, error)),
        };
        if !metadata.is_file() {
            return Err(AntigravityInstallError::NotRegularFile { path });
        }
        if metadata.len() > MAX_ACTIVE_POINTER_BYTES {
            return Err(AntigravityInstallError::MalformedActivePointer { path });
        }
        let bytes = std::fs::read(&path)
            .map_err(|error| io_error("read antigravity active pointer", &path, error))?;
        let Ok(text) = String::from_utf8(bytes) else {
            return Err(AntigravityInstallError::MalformedActivePointer { path });
        };
        let version = text.trim();
        validate_version(version)?;
        Ok(Some(version.to_owned()))
    }

    /// Reports the installation for `pin` if one is present AND verifies.
    ///
    /// Directory existence is never trusted: the version directory and both
    /// binaries are re-inspected on every call for type, size, ownership and
    /// mode.
    ///
    /// - `Ok(None)` — nothing is installed for this pinned version, or the
    ///   tree is incomplete. An install is needed.
    /// - `Err(..)` — a tree IS present but is not safe to run (wrong owner, a
    ///   group/other permission bit, a non-regular file). This fails closed on
    ///   purpose: a suspicious tree is reported to the operator, never
    ///   silently overwritten.
    pub fn resolve(
        &self,
        pin: &AntigravityPin,
    ) -> Result<Option<AntigravityInstallation>, AntigravityInstallError> {
        validate_version(pin.version)?;
        let Some(active) = self.active_version()? else {
            return Ok(None);
        };
        if active != pin.version {
            return Ok(None);
        }
        let directory = self.versions_dir().join(&active);
        let executable = directory.join(pin.executable_name);
        let helper = directory.join(pin.helper_name);
        match inspect_directory(&directory)? {
            Presence::Absent => return Ok(None),
            Presence::Present => {}
        }
        for path in [&executable, &helper] {
            match inspect_executable(path)? {
                Presence::Absent => return Ok(None),
                Presence::Present => {}
            }
        }
        Ok(Some(AntigravityInstallation {
            version: active,
            directory,
            executable,
            helper,
            args: pin.extra_args.iter().map(|arg| (*arg).to_owned()).collect(),
        }))
    }

    /// Ensures the pinned version is installed, and returns how to launch it.
    ///
    /// This is the ONLY way a version is installed or activated. It is an
    /// explicit call: there is no timer, no background task, and no
    /// self-update path in this module, so nothing can move the active pointer
    /// behind a running agent's back. A call that finds a valid install for
    /// the pinned version returns [`InstallOutcome::AlreadyPresent`] having
    /// performed no transfer and touched no pointer.
    pub async fn ensure_installed(
        &self,
        pin: &AntigravityPin,
        source: &dyn ArchiveSource,
    ) -> Result<InstallOutcome, AntigravityInstallError> {
        validate_version(pin.version)?;
        let expected_digest = decode_pinned_digest(pin)?;
        self.prepare_root()?;

        if let Some(installation) = self.resolve(pin)? {
            return Ok(InstallOutcome::AlreadyPresent(installation));
        }

        // Refuse before spending 316-682 MB of transfer: a live lease means a
        // running child is using this exact version tree.
        if self.is_version_leased(pin.version)? {
            return Err(AntigravityInstallError::VersionLeased {
                version: pin.version.to_owned(),
            });
        }

        let archive = self
            .fetch_verified_archive(pin, source, &expected_digest)
            .await?;
        // Screening, extracting and publishing move up to 2.0 GB of blocking
        // filesystem work, so they run on the blocking pool rather than
        // stalling the daemon's reactor. `AntigravityPin` is `Copy` and holds
        // only `'static` data, so the pin crosses the boundary by value.
        let installer = self.clone();
        let pin_copy = *pin;
        tokio::task::spawn_blocking(move || installer.publish_verified_archive(&pin_copy, archive))
            .await
            .map_err(|error| AntigravityInstallError::Internal {
                message: format!("antigravity install worker failed: {error}"),
            })??;
        match self.resolve(pin)? {
            Some(installation) => Ok(InstallOutcome::Installed(installation)),
            None => Err(AntigravityInstallError::Internal {
                message: "activated antigravity install did not verify".into(),
            }),
        }
    }

    /// Streams the pinned archive into `downloads/`, verifying size and digest
    /// before the file is given its final name.
    ///
    /// Mirrors `haider-stt`'s `download.rs`: the temp file lives in the same
    /// directory as the final path so the publish is a `rename(2)`, and any
    /// refusal removes the temp file so nothing partial survives.
    async fn fetch_verified_archive(
        &self,
        pin: &AntigravityPin,
        source: &dyn ArchiveSource,
        expected_digest: &[u8; 32],
    ) -> Result<PathBuf, AntigravityInstallError> {
        let downloads = self.downloads_dir();
        let final_path = downloads.join(format!("{}-{}.zip", pin.platform_key, pin.version));
        let temp_path = downloads.join(format!(
            "{}-{}.zip{DOWNLOAD_TEMP_SUFFIX}",
            pin.platform_key, pin.version
        ));
        // A leftover from an interrupted run is never reused: it was never
        // verified, so it is unlinked and the replacement is opened with
        // `create_new`, which refuses to follow a name that reappeared as a
        // symlink in the window between the two calls. The window itself is
        // only reachable by this user: `prepare_root` has already verified
        // that `downloads/` is a `0700` directory this user owns.
        match std::fs::remove_file(&temp_path) {
            Ok(()) => {}
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
            Err(error) => {
                return Err(io_error(
                    "clear the antigravity archive staging path",
                    &temp_path,
                    error,
                ));
            }
        }
        let mut options = OpenOptions::new();
        options.write(true).create_new(true);
        haider_platform::configure_file_mode(&mut options, CONTROL_FILE_MODE);
        let file = options
            .open(&temp_path)
            .map_err(|error| io_error("create antigravity archive", &temp_path, error))?;
        let mut sink = ArchiveSink {
            file,
            hasher: sha2::Sha256::new(),
            written_bytes: 0,
            pinned_bytes: pin.archive_size_bytes,
        };

        let streamed = source.stream_to(pin, &mut sink).await;
        let result = streamed.and_then(|()| {
            sink.file
                .flush()
                .map_err(|error| io_error("flush antigravity archive", &temp_path, error))?;
            haider_platform::sync_file(&sink.file, haider_platform::SyncPolicy::Full)
                .map_err(|error| io_error("sync antigravity archive", &temp_path, error))?;
            if sink.written_bytes != pin.archive_size_bytes {
                return Err(AntigravityInstallError::ArchiveSizeMismatch {
                    expected: pin.archive_size_bytes,
                    actual: sink.written_bytes,
                });
            }
            let actual = sink.hasher.clone().finalize();
            // Constant-time over the raw 32 bytes; a hex comparison would
            // leak the matching prefix length through timing.
            if !bool::from(actual.as_slice().ct_eq(expected_digest.as_slice())) {
                return Err(AntigravityInstallError::ArchiveDigestMismatch {
                    expected: pin.archive_sha256.to_owned(),
                    actual: hex::encode(actual),
                });
            }
            Ok(())
        });

        if let Err(error) = result {
            drop(sink);
            let _ = std::fs::remove_file(&temp_path);
            return Err(error);
        }
        drop(sink);
        haider_platform::replace_file(&temp_path, &final_path).map_err(|error| {
            let _ = std::fs::remove_file(&temp_path);
            io_error("publish antigravity archive", &final_path, error)
        })?;
        haider_platform::sync_directory(&downloads)
            .map_err(|error| io_error("sync antigravity downloads", &downloads, error))?;
        Ok(final_path)
    }

    /// Screens, extracts, verifies and activates a verified archive.
    ///
    /// Every step here is blocking filesystem work over up to 2.0 GB, so it is
    /// deliberately synchronous and is called from a blocking context.
    fn publish_verified_archive(
        &self,
        pin: &AntigravityPin,
        archive_path: PathBuf,
    ) -> Result<(), AntigravityInstallError> {
        let versions = self.versions_dir();
        let staging = self.stage_directory(&versions)?;
        let result = self.extract_and_activate(pin, &archive_path, &staging);
        if result.is_err() {
            // Nothing partial survives a refusal, so the active pointer can
            // never come to name a half-extracted tree.
            let _ = std::fs::remove_dir_all(&staging);
        }
        // The verified archive is 316-682 MB and the extracted tree is its
        // only consumer; keeping both would more than double an already large
        // install. A later call re-verifies the tree, not the archive.
        let _ = std::fs::remove_file(&archive_path);
        result
    }

    fn extract_and_activate(
        &self,
        pin: &AntigravityPin,
        archive_path: &Path,
        staging: &Path,
    ) -> Result<(), AntigravityInstallError> {
        extract_pinned_archive(archive_path, staging, pin)?;
        // Verify what actually landed, not what the archive claimed.
        for name in [pin.executable_name, pin.helper_name] {
            let path = staging.join(name);
            match inspect_executable(&path)? {
                Presence::Present => {}
                Presence::Absent => {
                    return Err(AntigravityInstallError::MissingEntry {
                        entry: name.to_owned(),
                    });
                }
            }
        }
        haider_platform::sync_directory(staging)
            .map_err(|error| io_error("sync antigravity staging", staging, error))?;

        // Re-check immediately before the publish: a lease could have been
        // taken while the archive was in flight.
        if self.is_version_leased(pin.version)? {
            return Err(AntigravityInstallError::VersionLeased {
                version: pin.version.to_owned(),
            });
        }

        let versions = self.versions_dir();
        let target = versions.join(pin.version);
        // `rename(2)` onto a non-empty directory fails, so an unleased broken
        // tree is first moved aside under a name nothing points at. The window
        // between the two renames is the only moment `versions/<version>` does
        // not exist; the active pointer still names the old version through
        // it, and every reader verifies the tree before use, so the worst case
        // a reader observes is "install needed".
        let retired = if std::fs::symlink_metadata(&target).is_ok() {
            let retired = self.stage_directory_name(&versions, ".retired-")?;
            std::fs::rename(&target, &retired)
                .map_err(|error| io_error("retire antigravity version", &target, error))?;
            Some(retired)
        } else {
            None
        };
        let published = std::fs::rename(staging, &target)
            .map_err(|error| io_error("publish antigravity version", &target, error));
        if let Err(error) = published {
            if let Some(retired) = retired {
                // Put the previous tree back before surfacing the failure.
                let _ = std::fs::rename(&retired, &target);
            }
            return Err(error);
        }
        haider_platform::sync_directory(&versions)
            .map_err(|error| io_error("sync antigravity versions", &versions, error))?;
        self.write_active_pointer(pin.version)?;
        if let Some(retired) = retired {
            let _ = std::fs::remove_dir_all(&retired);
        }
        Ok(())
    }

    /// Replaces the active pointer atomically: staged in the same directory,
    /// published with the platform replacement primitive, then the directory
    /// entry is synced. A reader observes either the old version or the new
    /// one, never a torn write.
    fn write_active_pointer(&self, version: &str) -> Result<(), AntigravityInstallError> {
        validate_version(version)?;
        let target = self.active_pointer();
        let staged = self
            .root
            .join(format!(".{ACTIVE_POINTER}-{}.tmp", random_name()?));
        let mut options = OpenOptions::new();
        options.write(true).create_new(true);
        haider_platform::configure_file_mode(&mut options, CONTROL_FILE_MODE);
        let mut file = options
            .open(&staged)
            .map_err(|error| io_error("create antigravity active pointer", &staged, error))?;
        let write = file
            .write_all(version.as_bytes())
            .and_then(|()| file.write_all(b"\n"))
            .and_then(|()| file.flush());
        drop(file);
        if let Err(error) = write {
            let _ = std::fs::remove_file(&staged);
            return Err(io_error("write antigravity active pointer", &staged, error));
        }
        set_mode_exact(&staged, CONTROL_FILE_MODE)?;
        if let Err(error) = haider_platform::replace_file(&staged, &target) {
            let _ = std::fs::remove_file(&staged);
            return Err(io_error(
                "publish antigravity active pointer",
                &target,
                error,
            ));
        }
        haider_platform::sync_directory(&self.root)
            .map_err(|error| io_error("sync antigravity root", &self.root, error))
    }

    fn stage_directory(&self, versions: &Path) -> Result<PathBuf, AntigravityInstallError> {
        self.stage_directory_name(versions, ".staging-")
    }

    /// Creates a uniquely named working directory inside `versions/`.
    ///
    /// It has to live there, not in a system temp directory, so that the
    /// publish is a same-filesystem `rename(2)`. The dot prefix keeps it out
    /// of any listing of installed versions.
    fn stage_directory_name(
        &self,
        versions: &Path,
        prefix: &str,
    ) -> Result<PathBuf, AntigravityInstallError> {
        for _ in 0..MAX_STAGING_ATTEMPTS {
            let candidate = versions.join(format!("{prefix}{}", random_name()?));
            let mut builder = DirBuilder::new();
            builder.recursive(false);
            haider_platform::configure_directory_mode(&mut builder, DIRECTORY_MODE);
            match builder.create(&candidate) {
                Ok(()) => {
                    set_mode_exact(&candidate, DIRECTORY_MODE)?;
                    return Ok(candidate);
                }
                Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => continue,
                Err(error) => {
                    return Err(io_error(
                        "create antigravity staging directory",
                        &candidate,
                        error,
                    ));
                }
            }
        }
        Err(AntigravityInstallError::Internal {
            message: "antigravity staging names are exhausted".into(),
        })
    }

    /// Takes a live claim on `version`.
    ///
    /// The lease file is created under a unique temporary name, locked, and
    /// only then renamed into place. Publishing an already-locked inode is
    /// what makes the sweeper safe: a lease file that appears under its final
    /// name is never observable in an unlocked state, so "the lock is free"
    /// means exactly "the holder is gone".
    pub fn acquire_lease(
        &self,
        version: &str,
    ) -> Result<AntigravityLease, AntigravityInstallError> {
        validate_version(version)?;
        let directory = self.leases_dir().join(version);
        create_directory_all(&directory)?;
        verify_directory(&directory)?;
        for _ in 0..MAX_STAGING_ATTEMPTS {
            let name = random_name()?;
            let staged = directory.join(format!(".{name}.tmp"));
            let target = directory.join(format!("{name}.lease"));
            let mut options = OpenOptions::new();
            options.read(true).write(true).create_new(true);
            haider_platform::configure_file_mode(&mut options, CONTROL_FILE_MODE);
            let mut file = match options.open(&staged) {
                Ok(file) => file,
                Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => continue,
                Err(error) => {
                    return Err(io_error("create antigravity lease", &staged, error));
                }
            };
            let prepared = (|| -> std::io::Result<()> {
                // Diagnostic only. The lock, not these bytes, is authoritative.
                write!(file, "pid={}\nversion={version}\n", std::process::id())?;
                file.flush()
            })();
            if let Err(error) = prepared {
                drop(file);
                let _ = std::fs::remove_file(&staged);
                return Err(io_error("write antigravity lease", &staged, error));
            }
            match file.try_lock() {
                Ok(()) => {}
                Err(TryLockError::WouldBlock) => {
                    // Unreachable with a 128-bit fresh name; treated as a
                    // collision rather than trusted.
                    drop(file);
                    let _ = std::fs::remove_file(&staged);
                    continue;
                }
                Err(TryLockError::Error(error)) => {
                    drop(file);
                    let _ = std::fs::remove_file(&staged);
                    return Err(io_error("lock antigravity lease", &staged, error));
                }
            }
            if let Err(error) = std::fs::rename(&staged, &target) {
                let _ = file.unlock();
                drop(file);
                let _ = std::fs::remove_file(&staged);
                return Err(io_error("publish antigravity lease", &target, error));
            }
            return Ok(AntigravityLease {
                file,
                path: target,
                version: version.to_owned(),
            });
        }
        Err(AntigravityInstallError::Internal {
            message: "antigravity lease names are exhausted".into(),
        })
    }

    /// True when at least one live lease holds `version`.
    ///
    /// Sweeps as it goes: a lease file this call can lock had its holder die,
    /// so the file is removed while the lock is held — which is why unique
    /// lease names matter, since a sweeper can then never unlink a
    /// successor's claim.
    pub fn is_version_leased(&self, version: &str) -> Result<bool, AntigravityInstallError> {
        validate_version(version)?;
        let directory = self.leases_dir().join(version);
        let entries = match std::fs::read_dir(&directory) {
            Ok(entries) => entries,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(false),
            Err(error) => return Err(io_error("list antigravity leases", &directory, error)),
        };
        for entry in entries {
            let entry =
                entry.map_err(|error| io_error("list antigravity leases", &directory, error))?;
            let path = entry.path();
            let metadata = match std::fs::symlink_metadata(&path) {
                Ok(metadata) => metadata,
                Err(error) if error.kind() == std::io::ErrorKind::NotFound => continue,
                Err(error) => return Err(io_error("inspect antigravity lease", &path, error)),
            };
            if !metadata.is_file() {
                continue;
            }
            let file = match OpenOptions::new().read(true).write(true).open(&path) {
                Ok(file) => file,
                Err(error) if error.kind() == std::io::ErrorKind::NotFound => continue,
                // A lease this process cannot even open is treated as live.
                // Refusing an install is always safer than replacing a tree a
                // running child may be executing from.
                Err(_) => return Ok(true),
            };
            match file.try_lock() {
                Ok(()) => {
                    let _ = std::fs::remove_file(&path);
                    let _ = file.unlock();
                }
                Err(TryLockError::WouldBlock) => return Ok(true),
                // Same fail-closed reasoning as an unopenable lease.
                Err(TryLockError::Error(_)) => return Ok(true),
            }
        }
        Ok(false)
    }
}

// ---------------------------------------------------------------------------
// Filesystem verification
// ---------------------------------------------------------------------------

/// Whether a path a caller may legitimately be missing was there.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Presence {
    Absent,
    Present,
}

fn create_directory_all(path: &Path) -> Result<(), AntigravityInstallError> {
    let mut builder = DirBuilder::new();
    builder.recursive(true);
    haider_platform::configure_directory_mode(&mut builder, DIRECTORY_MODE);
    builder
        .create(path)
        .map_err(|error| io_error("create antigravity directory", path, error))?;
    set_mode_exact(path, DIRECTORY_MODE)
}

/// Forces an exact mode. `mkdir`/`open` modes are masked by the process umask,
/// so the explicit set is what makes `0700` a guarantee rather than a request.
fn set_mode_exact(path: &Path, mode: u32) -> Result<(), AntigravityInstallError> {
    haider_platform::set_mode(path, mode)
        .map_err(|error| io_error("set antigravity path mode", path, error))
}

fn verify_directory(path: &Path) -> Result<(), AntigravityInstallError> {
    match inspect_directory(path)? {
        Presence::Present => Ok(()),
        Presence::Absent => Err(io_error(
            "open antigravity directory",
            path,
            std::io::Error::from(std::io::ErrorKind::NotFound),
        )),
    }
}

fn inspect_directory(path: &Path) -> Result<Presence, AntigravityInstallError> {
    let metadata = match std::fs::symlink_metadata(path) {
        Ok(metadata) => metadata,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(Presence::Absent),
        Err(error) => return Err(io_error("inspect antigravity directory", path, error)),
    };
    if !metadata.is_dir() {
        return Err(AntigravityInstallError::NotADirectory {
            path: path.to_path_buf(),
        });
    }
    if !haider_platform::metadata_is_current_user(&metadata) {
        return Err(AntigravityInstallError::NotOwnedByCurrentUser {
            path: path.to_path_buf(),
        });
    }
    let mode = haider_platform::metadata_mode(&metadata);
    if mode & 0o077 != 0 {
        return Err(AntigravityInstallError::InsecurePermissions {
            path: path.to_path_buf(),
            mode: mode & 0o7777,
        });
    }
    Ok(Presence::Present)
}

/// Verifies one installed or staged binary.
///
/// `Absent` means "not installed yet"; every other fault is an error, because
/// a present-but-wrong binary is a security signal, not a reason to reinstall.
fn inspect_executable(path: &Path) -> Result<Presence, AntigravityInstallError> {
    let metadata = match std::fs::symlink_metadata(path) {
        Ok(metadata) => metadata,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(Presence::Absent),
        Err(error) => return Err(io_error("inspect antigravity binary", path, error)),
    };
    if !metadata.is_file() {
        return Err(AntigravityInstallError::NotRegularFile {
            path: path.to_path_buf(),
        });
    }
    if metadata.len() == 0 {
        return Err(AntigravityInstallError::EmptyFile {
            path: path.to_path_buf(),
        });
    }
    if !haider_platform::metadata_is_current_user(&metadata) {
        return Err(AntigravityInstallError::NotOwnedByCurrentUser {
            path: path.to_path_buf(),
        });
    }
    let mode = haider_platform::metadata_mode(&metadata);
    // No group or other bit at all, and owner-executable: the daemon launches
    // this file, so anything another account can write is a code-execution
    // hole and anything the owner cannot execute is not an install.
    if mode & 0o077 != 0 || mode & 0o100 == 0 {
        return Err(AntigravityInstallError::InsecurePermissions {
            path: path.to_path_buf(),
            mode: mode & 0o7777,
        });
    }
    Ok(Presence::Present)
}

fn validate_version(version: &str) -> Result<(), AntigravityInstallError> {
    let invalid = version.is_empty()
        || version.len() > MAX_VERSION_BYTES
        || version == "."
        || version == ".."
        || version.starts_with('.')
        || !version
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'.' | b'-' | b'_' | b'+'));
    if invalid {
        return Err(AntigravityInstallError::InvalidVersion {
            version: entry_label(version),
        });
    }
    Ok(())
}

fn decode_pinned_digest(pin: &AntigravityPin) -> Result<[u8; 32], AntigravityInstallError> {
    let malformed = || AntigravityInstallError::MalformedPinDigest {
        platform_key: pin.platform_key,
    };
    if pin.archive_sha256.len() != 64
        || !pin
            .archive_sha256
            .bytes()
            .all(|byte| byte.is_ascii_digit() || matches!(byte, b'a'..=b'f'))
    {
        return Err(malformed());
    }
    let mut digest = [0_u8; 32];
    hex::decode_to_slice(pin.archive_sha256, &mut digest).map_err(|_| malformed())?;
    Ok(digest)
}

fn random_name() -> Result<String, AntigravityInstallError> {
    let mut bytes = [0_u8; RANDOM_NAME_BYTES];
    getrandom::fill(&mut bytes).map_err(|error| AntigravityInstallError::Internal {
        message: format!("could not draw a unique name: {error}"),
    })?;
    Ok(hex::encode(bytes))
}

// ---------------------------------------------------------------------------
// Archive screening and extraction
// ---------------------------------------------------------------------------

/// Screens then extracts a verified archive into `staging`.
///
/// Every entry is treated as hostile. The screen pass reads only the central
/// directory — no entry body is decompressed until the whole archive has been
/// accepted — and the extract pass re-checks every size bound incrementally,
/// because the central directory's declared sizes are attacker-controlled.
fn extract_pinned_archive(
    archive_path: &Path,
    staging: &Path,
    pin: &AntigravityPin,
) -> Result<(), AntigravityInstallError> {
    let mut file = File::open(archive_path)
        .map_err(|error| io_error("open antigravity archive", archive_path, error))?;
    let file_len = file
        .metadata()
        .map_err(|error| io_error("inspect antigravity archive", archive_path, error))?
        .len();

    // The declared record count is read from the end-of-central-directory
    // record BEFORE the ZIP parser runs, so a million-entry archive is refused
    // without parsing a million records. It is also the only way to see a
    // duplicated entry name: the ZIP reader keys entries by name and silently
    // keeps the last of a repeated pair.
    let declared = declared_entry_count(&mut file, file_len)?;
    if declared > MAX_ARCHIVE_ENTRIES {
        return Err(AntigravityInstallError::EntryCountExceeded {
            declared,
            limit: MAX_ARCHIVE_ENTRIES,
        });
    }

    let mut archive =
        zip::ZipArchive::new(file).map_err(|error| AntigravityInstallError::MalformedArchive {
            message: format!("could not read the archive: {error}"),
        })?;
    let distinct = archive.len() as u64;
    if distinct != declared {
        return Err(AntigravityInstallError::DuplicateEntry { declared, distinct });
    }

    let expected = [pin.executable_name, pin.helper_name];
    let mut seen = [false; 2];
    let mut declared_total = 0_u64;
    for index in 0..archive.len() {
        let entry =
            archive
                .by_index(index)
                .map_err(|error| AntigravityInstallError::MalformedArchive {
                    message: format!("could not read archive entry {index}: {error}"),
                })?;
        let name = entry.name().to_owned();
        // Type before name: a ZIP marks a directory by a trailing separator,
        // so screening the name first would report a directory entry as a
        // malformed name rather than as the wrong kind of entry.
        if let Some(fault) = entry_kind_fault(&entry) {
            return Err(AntigravityInstallError::NonRegularEntry {
                entry: entry_label(&name),
                fault,
            });
        }
        if let Some(fault) = entry_path_fault(&name) {
            return Err(AntigravityInstallError::UnsafeEntryPath {
                entry: entry_label(&name),
                fault,
            });
        }
        let Some(slot) = expected.iter().position(|candidate| *candidate == name) else {
            return Err(AntigravityInstallError::UnexpectedEntry {
                entry: entry_label(&name),
            });
        };
        // Unreachable while the reader dedupes by name and the count check
        // above passed; kept so the invariant does not depend on that.
        if seen[slot] {
            return Err(AntigravityInstallError::DuplicateEntry {
                declared,
                distinct: distinct.saturating_sub(1),
            });
        }
        seen[slot] = true;
        if entry.size() > MAX_ENTRY_UNCOMPRESSED_BYTES {
            return Err(AntigravityInstallError::EntryTooLarge {
                entry: entry_label(&name),
                written_bytes: 0,
                limit: MAX_ENTRY_UNCOMPRESSED_BYTES,
            });
        }
        declared_total = declared_total.saturating_add(entry.size());
        if declared_total > MAX_TOTAL_UNCOMPRESSED_BYTES {
            return Err(AntigravityInstallError::TotalTooLarge {
                written_bytes: 0,
                limit: MAX_TOTAL_UNCOMPRESSED_BYTES,
            });
        }
    }
    for (slot, present) in seen.iter().enumerate() {
        if !present {
            return Err(AntigravityInstallError::MissingEntry {
                entry: expected.get(slot).copied().unwrap_or_default().to_owned(),
            });
        }
    }

    extract_screened_archive(&mut archive, staging, &expected)
}

fn extract_screened_archive(
    archive: &mut zip::ZipArchive<File>,
    staging: &Path,
    expected: &[&'static str; 2],
) -> Result<(), AntigravityInstallError> {
    let mut written_total = 0_u64;
    for index in 0..archive.len() {
        let mut entry =
            archive
                .by_index(index)
                .map_err(|error| AntigravityInstallError::MalformedArchive {
                    message: format!("could not open archive entry {index}: {error}"),
                })?;
        let name = entry.name().to_owned();
        let declared_size = entry.size();
        let compressed_size = entry.compressed_size();
        // The destination basename is the PINNED constant, matched by the
        // screen pass, never a string taken from the archive. No archive-
        // controlled byte reaches the filesystem.
        let Some(pinned_name) = expected
            .iter()
            .find(|candidate| **candidate == name)
            .copied()
        else {
            return Err(AntigravityInstallError::UnexpectedEntry {
                entry: entry_label(&name),
            });
        };
        let destination = staging.join(pinned_name);
        let mut options = OpenOptions::new();
        options.write(true).create_new(true);
        haider_platform::configure_file_mode(&mut options, EXECUTABLE_MODE);
        let mut output = options
            .open(&destination)
            .map_err(|error| io_error("create antigravity binary", &destination, error))?;

        // Ratio allowance for this entry, checked after every chunk so a bomb
        // aborts having written the allowance, not the declared size.
        let allowance = MAX_COMPRESSION_RATIO
            .saturating_mul(compressed_size)
            .saturating_add(RATIO_FLOOR_BYTES);
        let mut buffer = vec![0_u8; EXTRACT_CHUNK_BYTES];
        let mut written = 0_u64;
        loop {
            let read = entry.read(&mut buffer).map_err(|error| {
                AntigravityInstallError::MalformedArchive {
                    message: format!("could not decompress `{}`: {error}", entry_label(&name)),
                }
            })?;
            if read == 0 {
                break;
            }
            let chunk = buffer.get(..read).unwrap_or_default();
            output
                .write_all(chunk)
                .map_err(|error| io_error("write antigravity binary", &destination, error))?;
            written = written.saturating_add(read as u64);
            if written > MAX_ENTRY_UNCOMPRESSED_BYTES {
                return Err(AntigravityInstallError::EntryTooLarge {
                    entry: entry_label(&name),
                    written_bytes: written,
                    limit: MAX_ENTRY_UNCOMPRESSED_BYTES,
                });
            }
            if written > allowance {
                return Err(AntigravityInstallError::CompressionRatioExceeded {
                    entry: entry_label(&name),
                    written_bytes: written,
                    allowance_bytes: allowance,
                });
            }
            let total = written_total.saturating_add(written);
            if total > MAX_TOTAL_UNCOMPRESSED_BYTES {
                return Err(AntigravityInstallError::TotalTooLarge {
                    written_bytes: total,
                    limit: MAX_TOTAL_UNCOMPRESSED_BYTES,
                });
            }
        }
        if written != declared_size {
            return Err(AntigravityInstallError::EntrySizeMismatch {
                entry: entry_label(&name),
                declared: declared_size,
                written_bytes: written,
            });
        }
        written_total = written_total.saturating_add(written);
        output
            .flush()
            .map_err(|error| io_error("flush antigravity binary", &destination, error))?;
        haider_platform::sync_file(&output, haider_platform::SyncPolicy::Full)
            .map_err(|error| io_error("sync antigravity binary", &destination, error))?;
        drop(output);
        // The archive's own mode bits are discarded: the pin says these are
        // executables, and `0700` is the only mode this installer publishes.
        set_mode_exact(&destination, EXECUTABLE_MODE)?;
    }
    Ok(())
}

/// Rejects any entry name that is not a flat, relative, archive-root file.
fn entry_path_fault(name: &str) -> Option<EntryPathFault> {
    if name.is_empty() {
        return Some(EntryPathFault::Empty);
    }
    if name.chars().any(char::is_control) {
        return Some(EntryPathFault::ControlByte);
    }
    if name.starts_with('/') || name.starts_with('\\') {
        return Some(EntryPathFault::Absolute);
    }
    // A drive letter (`C:\x`) or a UNC prefix (`\\host\share`); the leading
    // backslash form is already caught above.
    if name.contains(':') {
        return Some(EntryPathFault::DriveOrUnc);
    }
    let mut components = name.split(['/', '\\']);
    if components.clone().any(|component| component == "..") {
        return Some(EntryPathFault::ParentComponent);
    }
    // The pin describes exactly two flat files at the archive root, so any
    // separator, `.` component or empty component is already outside the
    // expectation set and is refused as a shape fault rather than a name one.
    if components.clone().count() != 1
        || components.any(|component| component.is_empty() || component == ".")
    {
        return Some(EntryPathFault::NotFlat);
    }
    None
}

fn entry_kind_fault(entry: &zip::read::ZipFile<'_>) -> Option<EntryKindFault> {
    if entry.is_dir() {
        return Some(EntryKindFault::Directory);
    }
    if entry.is_symlink() {
        return Some(EntryKindFault::Symlink);
    }
    // A ZIP written on Windows carries no unix mode; when one IS present, its
    // file-type bits must say "regular file" and nothing else.
    match entry.unix_mode() {
        None => None,
        Some(mode) if mode & 0o170_000 == 0 || mode & 0o170_000 == 0o100_000 => None,
        Some(_) => Some(EntryKindFault::NotRegular),
    }
}

/// Reads the number of central-directory records the archive declares.
///
/// Located the standard way: scan backwards through the tail for the last
/// end-of-central-directory signature whose comment length accounts for
/// exactly the remaining bytes. A `0xFFFF` count means the real one lives in
/// the ZIP64 end-of-central-directory record, reached through the locator that
/// immediately precedes the EOCD.
fn declared_entry_count(file: &mut File, file_len: u64) -> Result<u64, AntigravityInstallError> {
    let malformed = |message: &str| AntigravityInstallError::MalformedArchive {
        message: message.to_owned(),
    };
    if file_len < EOCD_FIXED_BYTES as u64 {
        return Err(malformed(
            "file is shorter than a ZIP end-of-directory record",
        ));
    }
    let window = EOCD_SEARCH_WINDOW.min(file_len);
    let window_start = file_len - window;
    file.seek(SeekFrom::Start(window_start))
        .map_err(|error| malformed(&format!("could not seek the archive tail: {error}")))?;
    let mut tail = vec![0_u8; window as usize];
    file.read_exact(&mut tail)
        .map_err(|error| malformed(&format!("could not read the archive tail: {error}")))?;

    let mut eocd = None;
    let mut index = tail.len() - EOCD_FIXED_BYTES;
    loop {
        let record = tail.get(index..).unwrap_or_default();
        if record.get(..4) == Some(&EOCD_SIGNATURE[..]) {
            let comment_len = record
                .get(20..22)
                .and_then(|bytes| <[u8; 2]>::try_from(bytes).ok())
                .map_or(usize::MAX, |bytes| u16::from_le_bytes(bytes) as usize);
            if comment_len != usize::MAX && EOCD_FIXED_BYTES + comment_len == record.len() {
                eocd = Some(index);
                break;
            }
        }
        if index == 0 {
            break;
        }
        index -= 1;
    }
    let Some(eocd) = eocd else {
        return Err(malformed("no ZIP end-of-central-directory record"));
    };
    let record = tail.get(eocd..).unwrap_or_default();
    let total = record
        .get(10..12)
        .and_then(|bytes| <[u8; 2]>::try_from(bytes).ok())
        .map(u16::from_le_bytes)
        .ok_or_else(|| malformed("truncated ZIP end-of-central-directory record"))?;
    if total != u16::MAX {
        return Ok(u64::from(total));
    }

    // ZIP64: the 20-byte locator sits immediately before the EOCD.
    let locator_start = eocd
        .checked_sub(ZIP64_LOCATOR_BYTES)
        .ok_or_else(|| malformed("ZIP64 end-of-directory locator is missing"))?;
    let locator = tail
        .get(locator_start..eocd)
        .ok_or_else(|| malformed("ZIP64 end-of-directory locator is truncated"))?;
    if locator.get(..4) != Some(&ZIP64_LOCATOR_SIGNATURE[..]) {
        return Err(malformed("ZIP64 end-of-directory locator is missing"));
    }
    let zip64_offset = locator
        .get(8..16)
        .and_then(|bytes| <[u8; 8]>::try_from(bytes).ok())
        .map(u64::from_le_bytes)
        .ok_or_else(|| malformed("ZIP64 end-of-directory locator is truncated"))?;
    if zip64_offset.saturating_add(ZIP64_EOCD_PREFIX_BYTES as u64) > file_len {
        return Err(malformed("ZIP64 end-of-directory record is out of range"));
    }
    file.seek(SeekFrom::Start(zip64_offset))
        .map_err(|error| malformed(&format!("could not seek the ZIP64 directory: {error}")))?;
    let mut zip64 = [0_u8; ZIP64_EOCD_PREFIX_BYTES];
    file.read_exact(&mut zip64)
        .map_err(|error| malformed(&format!("could not read the ZIP64 directory: {error}")))?;
    if zip64.get(..4) != Some(&ZIP64_EOCD_SIGNATURE[..]) {
        return Err(malformed("ZIP64 end-of-directory record is missing"));
    }
    zip64
        .get(32..40)
        .and_then(|bytes| <[u8; 8]>::try_from(bytes).ok())
        .map(u64::from_le_bytes)
        .ok_or_else(|| malformed("ZIP64 end-of-directory record is truncated"))
}
