//! W9a discovery, staging, transaction, and recovery laws.
#![allow(clippy::expect_used)]
#![allow(dead_code)]

#[path = "../src/main.rs"]
mod cli_main;

use cli_main::update::UpdateError;
use cli_main::update::discovery::{
    CurlTransport, DiscoveryOutcome, ReleaseSelection, ReleaseSource, SemVersion, UpdateTransport,
    discover,
};
use cli_main::update::stage_then_acquire;
use cli_main::update::staging::{StageVerifier, sha256_file, stage_release};
use cli_main::update::transaction::{
    CommitBoundary, FaultInjector, InstallLayout, InstalledPairVerifier, NoFaults,
    PreparedTransaction, TransactionPhase, commit_pair, marker_path,
};
use std::collections::BTreeMap;
use std::fs;
use std::io::{Read, Write};
use std::net::TcpListener;
use std::os::unix::fs::{MetadataExt, PermissionsExt};
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::time::Duration;

#[derive(Default)]
struct FakeTransport {
    get: BTreeMap<String, Vec<u8>>,
    downloads: BTreeMap<String, Vec<u8>>,
    get_calls: usize,
    download_calls: usize,
}

impl UpdateTransport for FakeTransport {
    fn get_bytes(&mut self, url: &str, _limit: usize) -> Result<Vec<u8>, UpdateError> {
        self.get_calls += 1;
        self.get
            .get(url)
            .cloned()
            .ok_or_else(|| UpdateError::Network(format!("missing fake GET {url}")))
    }

    fn download(&mut self, url: &str, path: &Path, _limit: u64) -> Result<(), UpdateError> {
        self.download_calls += 1;
        let bytes = self
            .downloads
            .get(url)
            .ok_or_else(|| UpdateError::Network(format!("missing fake download {url}")))?;
        fs::write(path, bytes).map_err(|error| UpdateError::io("fake download", error))
    }
}

fn source() -> ReleaseSource {
    ReleaseSource {
        api_base: "http://fixture.invalid".into(),
        repository: "owner/repo".into(),
        allow_http: true,
    }
}

fn releases_url(page: usize) -> String {
    format!("http://fixture.invalid/repos/owner/repo/releases?per_page=100&page={page}")
}

fn release_json(tag: &str, targets: &[&str]) -> serde_json::Value {
    let mut assets = Vec::new();
    for target in targets {
        let archive = format!("haider-{tag}-{target}.tar.xz");
        assets.push(serde_json::json!({
            "name": archive,
            "browser_download_url": format!("http://fixture.invalid/{target}/archive")
        }));
        assets.push(serde_json::json!({
            "name": format!("{archive}.sha256"),
            "browser_download_url": format!("http://fixture.invalid/{target}/checksum")
        }));
    }
    serde_json::json!({
        "tag_name": tag,
        "draft": false,
        "prerelease": true,
        "assets": assets,
    })
}

fn discovery_transport(releases: Vec<serde_json::Value>) -> FakeTransport {
    let mut transport = FakeTransport::default();
    transport.get.insert(
        releases_url(1),
        serde_json::to_vec(&releases).expect("release JSON"),
    );
    transport
}

/// MUTATION CHECK: use `/releases/latest`, discard prereleases, trust API
/// order, or use physical host architecture. Expected RUNTIME failure: one
/// of both compiled-target selections is not the highest SemVer prerelease.
#[test]
fn discovery_includes_prereleases_orders_semver_and_selects_both_targets() {
    let targets = ["aarch64-apple-darwin", "x86_64-apple-darwin"];
    for target in targets {
        let mut transport = discovery_transport(vec![
            release_json("v0.0.9", &targets),
            release_json("v0.0.11-beta.2", &targets),
            release_json("v0.0.10", &targets),
        ]);
        let outcome =
            discover(&mut transport, &source(), "0.0.1", target).expect("discover target release");
        let DiscoveryOutcome::Update(selection) = outcome else {
            panic!("expected update")
        };
        assert_eq!(selection.version.to_string(), "0.0.11-beta.2");
        assert_eq!(
            selection.archive_name,
            format!("haider-v0.0.11-beta.2-{target}.tar.xz")
        );
        assert_eq!(transport.get_calls, 1);
        assert_eq!(transport.download_calls, 0);
    }
}

/// MUTATION CHECK: let an equal release reach download/staging. Expected
/// RUNTIME failure: the download call count becomes nonzero instead of a
/// list-only successful no-op.
#[test]
fn equal_version_is_list_only_and_build_metadata_has_semver_equality() {
    let target = "aarch64-apple-darwin";
    let mut transport = discovery_transport(vec![release_json("v1.2.3+publisher.7", &[target])]);
    let outcome =
        discover(&mut transport, &source(), "1.2.3+local.9", target).expect("equal SemVer no-op");
    assert!(matches!(outcome, DiscoveryOutcome::Current(_)));
    assert_eq!(transport.get_calls, 1);
    assert_eq!(transport.download_calls, 0);
}

/// MUTATION CHECK: admit downgrade, malformed tag, missing asset, or a
/// duplicate exact asset. Expected RUNTIME failure: a refused fixture below
/// becomes selectable and can proceed toward download.
#[test]
fn downgrade_malformed_and_asset_mismatch_refuse_before_download() {
    let target = "x86_64-apple-darwin";
    let mut downgrade = discovery_transport(vec![release_json("v1.0.0", &[target])]);
    assert!(discover(&mut downgrade, &source(), "2.0.0", target).is_err());

    let mut malformed = discovery_transport(vec![release_json("release-2", &[target])]);
    assert!(discover(&mut malformed, &source(), "1.0.0", target).is_err());

    let mut missing = release_json("v2.0.0", &[target]);
    missing["assets"].as_array_mut().expect("assets").pop();
    let mut missing = discovery_transport(vec![missing]);
    assert!(discover(&mut missing, &source(), "1.0.0", target).is_err());

    let mut duplicate = release_json("v2.0.0", &[target]);
    let first = duplicate["assets"][0].clone();
    duplicate["assets"]
        .as_array_mut()
        .expect("assets")
        .push(first);
    let mut duplicate = discovery_transport(vec![duplicate]);
    assert!(discover(&mut duplicate, &source(), "1.0.0", target).is_err());
    for transport in [downgrade, malformed, missing, duplicate] {
        assert_eq!(transport.download_calls, 0);
    }
}

/// MUTATION CHECK: accept leading zeros or misorder numeric/text prerelease
/// identifiers. Expected RUNTIME failure: these SemVer 2.0 precedence and
/// rejection assertions change.
#[test]
fn semver_parser_enforces_semver_two_precedence() {
    let ordered = [
        "1.0.0-alpha",
        "1.0.0-alpha.1",
        "1.0.0-alpha.beta",
        "1.0.0-beta",
        "1.0.0-beta.2",
        "1.0.0-beta.11",
        "1.0.0-rc.1",
        "1.0.0",
    ];
    let parsed = ordered
        .iter()
        .map(|version| SemVersion::parse(version).expect("valid SemVer"))
        .collect::<Vec<_>>();
    assert!(parsed.windows(2).all(|pair| pair[0] < pair[1]));
    for malformed in ["01.0.0", "1.00.0", "1.0", "1.0.0-01", "1.0.0-"] {
        assert!(SemVersion::parse(malformed).is_err(), "{malformed}");
    }
}

/// Network fixture law: this TLS-intercepted environment must not pin curl
/// error codes. Expected RUNTIME failure for ignoring a declared truncation:
/// discovery returns a release and could mutate later; we assert only local
/// side effects and request observation.
#[test]
fn truncated_local_http_response_has_no_local_mutation() {
    let listener = match TcpListener::bind("127.0.0.1:0") {
        Ok(listener) => listener,
        Err(error) if error.kind() == std::io::ErrorKind::PermissionDenied => {
            // Some hermetic runners deny AF_INET even for loopback. The same
            // fixture executes on ordinary macOS CI; do not replace it with
            // an external network assertion on the intercepted box.
            return;
        }
        Err(error) => panic!("bind fixture: {error}"),
    };
    let address = listener.local_addr().expect("fixture address");
    let requests = Arc::new(AtomicUsize::new(0));
    let observed = Arc::clone(&requests);
    let server = std::thread::spawn(move || {
        let (mut stream, _) = listener.accept().expect("fixture accept");
        stream
            .set_read_timeout(Some(Duration::from_secs(5)))
            .expect("read timeout");
        let mut request = [0_u8; 4096];
        let _ = stream.read(&mut request);
        observed.fetch_add(1, Ordering::SeqCst);
        stream
            .write_all(b"HTTP/1.1 200 OK\r\nContent-Length: 100\r\nConnection: close\r\n\r\n[]")
            .expect("write truncated response");
    });
    let install = tempfile::tempdir().expect("install fixture");
    write_executable(&install.path().join("haider"), b"old-cli");
    write_executable(&install.path().join("haiderd"), b"old-daemon");
    let before = pair_snapshot(install.path());
    let source = ReleaseSource {
        api_base: format!("http://{address}"),
        repository: "owner/repo".into(),
        allow_http: true,
    };
    let mut transport = CurlTransport::without_token();
    let _ = discover(&mut transport, &source, "1.0.0", "aarch64-apple-darwin");
    server.join().expect("fixture server");
    assert_eq!(requests.load(Ordering::SeqCst), 1);
    assert_eq!(pair_snapshot(install.path()), before);
    assert!(stage_entries(install.path()).is_empty());
}

/// MUTATION CHECK: restore curl `--location` on an authenticated command or
/// put the token in argv/environment. Expected RUNTIME failure: the local
/// redirect target receives a second request, or the first request lacks the
/// stdin-delivered Authorization header.
#[test]
fn authenticated_release_request_does_not_follow_or_forward_redirects() {
    let listener = match TcpListener::bind("127.0.0.1:0") {
        Ok(listener) => listener,
        Err(error) if error.kind() == std::io::ErrorKind::PermissionDenied => return,
        Err(error) => panic!("bind redirect fixture: {error}"),
    };
    let address = listener.local_addr().expect("redirect fixture address");
    let requests = Arc::new(AtomicUsize::new(0));
    let saw_auth = Arc::new(AtomicBool::new(false));
    let observed_requests = Arc::clone(&requests);
    let observed_auth = Arc::clone(&saw_auth);
    let server = std::thread::spawn(move || {
        let (mut stream, _) = listener.accept().expect("accept authenticated request");
        observed_requests.fetch_add(1, Ordering::SeqCst);
        let mut request = [0_u8; 8192];
        let read = stream
            .read(&mut request)
            .expect("read authenticated request");
        let request = String::from_utf8_lossy(&request[..read]);
        observed_auth.store(
            request.contains("Authorization: Bearer fixture-secret"),
            Ordering::SeqCst,
        );
        stream
            .write_all(
                format!(
                    "HTTP/1.1 302 Found\r\nLocation: http://{address}/leak\r\nContent-Length: 0\r\nConnection: close\r\n\r\n"
                )
                .as_bytes(),
            )
            .expect("write redirect response");
        drop(stream);
        listener
            .set_nonblocking(true)
            .expect("nonblocking redirect listener");
        for _ in 0..30 {
            match listener.accept() {
                Ok((_leaked, _)) => {
                    observed_requests.fetch_add(1, Ordering::SeqCst);
                    break;
                }
                Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => {
                    std::thread::sleep(Duration::from_millis(10));
                }
                Err(error) => panic!("poll redirect target: {error}"),
            }
        }
    });
    let transport = CurlTransport::with_token_for_test("fixture-secret");
    let _ = transport.authenticated_get_for_test(&format!("http://{address}/releases"), 4096);
    server.join().expect("redirect fixture server");
    assert!(saw_auth.load(Ordering::SeqCst));
    assert_eq!(requests.load(Ordering::SeqCst), 1);
}

/// MUTATION CHECK: auto-follow an authenticated asset-API redirect or omit
/// redirect-origin validation before the unauthenticated second process.
/// Expected RUNTIME failure: the loopback target receives a second request.
#[test]
fn authenticated_asset_redirect_is_validated_before_tokenless_fetch() {
    let listener = match TcpListener::bind("127.0.0.1:0") {
        Ok(listener) => listener,
        Err(error) if error.kind() == std::io::ErrorKind::PermissionDenied => return,
        Err(error) => panic!("bind asset redirect fixture: {error}"),
    };
    let address = listener.local_addr().expect("asset redirect address");
    let requests = Arc::new(AtomicUsize::new(0));
    let observed = Arc::clone(&requests);
    let server = std::thread::spawn(move || {
        let (mut stream, _) = listener.accept().expect("accept asset API request");
        observed.fetch_add(1, Ordering::SeqCst);
        let mut request = [0_u8; 8192];
        let read = stream.read(&mut request).expect("read asset API request");
        assert!(
            String::from_utf8_lossy(&request[..read])
                .contains("Authorization: Bearer fixture-secret")
        );
        stream
            .write_all(
                format!(
                    "HTTP/1.1 302 Found\r\nLocation: http://{address}/must-not-fetch\r\nContent-Length: 0\r\nConnection: close\r\n\r\n"
                )
                .as_bytes(),
            )
            .expect("write asset redirect");
        drop(stream);
        listener
            .set_nonblocking(true)
            .expect("nonblocking asset listener");
        for _ in 0..30 {
            match listener.accept() {
                Ok((_leaked, _)) => {
                    observed.fetch_add(1, Ordering::SeqCst);
                    break;
                }
                Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => {
                    std::thread::sleep(Duration::from_millis(10));
                }
                Err(error) => panic!("poll asset redirect target: {error}"),
            }
        }
    });
    let destination = tempfile::NamedTempFile::new().expect("asset destination");
    let transport = CurlTransport::with_token_for_test("fixture-secret");
    assert!(
        transport
            .authenticated_asset_for_test(
                &format!("http://{address}/asset-api"),
                destination.path(),
                4096,
            )
            .is_err()
    );
    server.join().expect("asset redirect server");
    assert_eq!(requests.load(Ordering::SeqCst), 1);
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum VerifyFailure {
    None,
    Xattr,
    Sign,
    Signature,
    SmokeCli,
    SmokeDaemon,
}

struct FakeVerifier {
    failure: VerifyFailure,
    calls: Arc<AtomicUsize>,
}

impl StageVerifier for FakeVerifier {
    fn remove_quarantine(&self, _path: &Path) -> Result<(), UpdateError> {
        self.calls.fetch_add(1, Ordering::SeqCst);
        fail_if(self.failure == VerifyFailure::Xattr, "xattr")
    }

    fn sign(&self, _path: &Path) -> Result<(), UpdateError> {
        self.calls.fetch_add(1, Ordering::SeqCst);
        fail_if(self.failure == VerifyFailure::Sign, "sign")
    }

    fn verify_signature(&self, _path: &Path) -> Result<(), UpdateError> {
        self.calls.fetch_add(1, Ordering::SeqCst);
        fail_if(self.failure == VerifyFailure::Signature, "signature")
    }

    fn smoke_haider(&self, _path: &Path, _target: &str) -> Result<(), UpdateError> {
        self.calls.fetch_add(1, Ordering::SeqCst);
        fail_if(self.failure == VerifyFailure::SmokeCli, "CLI smoke")
    }

    fn smoke_haiderd(&self, _path: &Path, _target: &str) -> Result<(), UpdateError> {
        self.calls.fetch_add(1, Ordering::SeqCst);
        fail_if(self.failure == VerifyFailure::SmokeDaemon, "daemon smoke")
    }
}

fn fail_if(fail: bool, name: &str) -> Result<(), UpdateError> {
    if fail {
        Err(UpdateError::Refused(format!("injected {name} failure")))
    } else {
        Ok(())
    }
}

#[derive(Clone)]
struct Member {
    name: String,
    kind: u8,
    mode: u64,
    data: Vec<u8>,
    declared_size: Option<u64>,
}

fn expected_members(version: &str, target: &str) -> Vec<Member> {
    let top = format!("haider-v{version}-{target}");
    vec![
        Member {
            name: format!("{top}/"),
            kind: b'5',
            mode: 0o755,
            data: Vec::new(),
            declared_size: None,
        },
        Member {
            name: format!("{top}/haider"),
            kind: b'0',
            mode: 0o755,
            data: b"new-cli".to_vec(),
            declared_size: None,
        },
        Member {
            name: format!("{top}/haiderd"),
            kind: b'0',
            mode: 0o755,
            data: b"new-daemon".to_vec(),
            declared_size: None,
        },
    ]
}

fn archive_bytes(root: &Path, members: &[Member]) -> Vec<u8> {
    let mut raw = Vec::new();
    for member in members {
        append_ustar_member(&mut raw, member);
    }
    raw.extend_from_slice(&[0_u8; 1024]);
    let raw_path = root.join("fixture.tar");
    fs::write(&raw_path, raw).expect("write raw tar");
    let mut output = None;
    for binary in ["/opt/homebrew/bin/xz", "/usr/local/bin/xz", "xz"] {
        match Command::new(binary)
            .args(["--compress", "--stdout"])
            .arg(&raw_path)
            .output()
        {
            Ok(candidate) => {
                output = Some(candidate);
                break;
            }
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
            Err(error) => panic!("start xz fixture compressor: {error}"),
        }
    }
    let output = output.expect("xz fixture compressor is installed");
    assert!(output.status.success(), "compress test archive");
    output.stdout
}

fn append_ustar_member(raw: &mut Vec<u8>, member: &Member) {
    let mut header = [0_u8; 512];
    assert!(member.name.len() <= 100, "test member path fits USTAR");
    header[..member.name.len()].copy_from_slice(member.name.as_bytes());
    write_octal(&mut header[100..108], member.mode, 7);
    write_octal(&mut header[108..116], 0, 7);
    write_octal(&mut header[116..124], 0, 7);
    let size = member.declared_size.unwrap_or(member.data.len() as u64);
    write_octal(&mut header[124..136], size, 11);
    write_octal(&mut header[136..148], 0, 11);
    header[148..156].fill(b' ');
    header[156] = member.kind;
    header[257..263].copy_from_slice(b"ustar\0");
    header[263..265].copy_from_slice(b"00");
    let checksum = header.iter().map(|byte| u64::from(*byte)).sum::<u64>();
    let encoded = format!("{checksum:06o}\0 ");
    header[148..156].copy_from_slice(encoded.as_bytes());
    raw.extend_from_slice(&header);
    raw.extend_from_slice(&member.data);
    let padding = (512 - (member.data.len() % 512)) % 512;
    raw.resize(raw.len() + padding, 0);
}

fn write_octal(field: &mut [u8], value: u64, digits: usize) {
    let encoded = format!("{value:0digits$o}\0");
    assert_eq!(encoded.len(), field.len());
    field.copy_from_slice(encoded.as_bytes());
}

fn selection_and_transport(
    root: &Path,
    members: &[Member],
    checksum_override: Option<String>,
) -> (ReleaseSelection, FakeTransport) {
    let version = "9.0.0";
    let target = cli_main::update::discovery::compiled_target().expect("macOS test target");
    let name = format!("haider-v{version}-{target}.tar.xz");
    let archive = archive_bytes(root, members);
    let archive_file = root.join("digest-input.tar.xz");
    fs::write(&archive_file, &archive).expect("digest archive");
    let digest = sha256_file(&archive_file).expect("archive digest");
    let checksum = checksum_override.unwrap_or_else(|| format!("{digest}  dist/{name}\n"));
    let selection = ReleaseSelection {
        version: SemVersion::parse(version).expect("selection version"),
        archive_name: name.clone(),
        archive_url: "archive".into(),
        checksum_name: format!("{name}.sha256"),
        checksum_url: "checksum".into(),
    };
    let mut transport = FakeTransport::default();
    transport.downloads.insert("archive".into(), archive);
    transport
        .downloads
        .insert("checksum".into(), checksum.into_bytes());
    (selection, transport)
}

/// MUTATION CHECK: reject the workflow's `dist/NAME` spelling. Expected
/// RUNTIME failure: a valid published checksum can no longer stage.
#[test]
fn staging_accepts_exact_workflow_checksum_basename() {
    let install = install_fixture();
    let target = cli_main::update::discovery::compiled_target().expect("target");
    let members = expected_members("9.0.0", target);
    let (selection, mut transport) = selection_and_transport(install.path(), &members, None);
    let verifier = FakeVerifier {
        failure: VerifyFailure::None,
        calls: Arc::new(AtomicUsize::new(0)),
    };
    let staged =
        stage_release(&mut transport, &verifier, install.path(), &selection).expect("valid stage");
    assert_eq!(
        fs::read(staged.haider_path()).expect("staged CLI"),
        b"new-cli"
    );
    assert_eq!(
        fs::read(staged.haiderd_path()).expect("staged daemon"),
        b"new-daemon"
    );
}

/// MUTATION CHECK: admit wrong, ambiguous, or basename-mismatched checksum.
/// Expected RUNTIME failure: verifier calls become nonzero and the installed
/// inode/bytes/mode snapshot can change.
#[test]
fn bad_checksums_stop_before_extraction_and_verification() {
    let target = cli_main::update::discovery::compiled_target().expect("target");
    let members = expected_members("9.0.0", target);
    for checksum in [
        format!("{}  wrong.tar.xz\n", "0".repeat(64)),
        format!("{}  first\n{}  second\n", "0".repeat(64), "1".repeat(64)),
        format!("{}  dist/wrong-name.tar.xz\n", "0".repeat(64)),
    ] {
        let install = install_fixture();
        let before = pair_snapshot(install.path());
        let (selection, mut transport) =
            selection_and_transport(install.path(), &members, Some(checksum));
        let calls = Arc::new(AtomicUsize::new(0));
        let verifier = FakeVerifier {
            failure: VerifyFailure::None,
            calls: Arc::clone(&calls),
        };
        assert!(stage_release(&mut transport, &verifier, install.path(), &selection).is_err());
        assert_eq!(calls.load(Ordering::SeqCst), 0);
        assert_eq!(pair_snapshot(install.path()), before);
        assert!(stage_entries(install.path()).is_empty());
    }
}

/// MUTATION CHECK: skip the archive-content digest comparison (a
/// WELL-FORMED checksum naming the right asset whose hash simply does not
/// match the downloaded bytes — the tampered/corrupted-transport case the
/// parsing fixtures above cannot reach). Expected RUNTIME failure: staging
/// succeeds, the verifier is called, or staging entries appear.
#[test]
fn wrong_content_digest_with_valid_checksum_refuses_before_extraction() {
    let target = cli_main::update::discovery::compiled_target().expect("target");
    let members = expected_members("9.0.0", target);
    let install = install_fixture();
    let before = pair_snapshot(install.path());
    let wrong_digest_checksum = format!(
        "{}  dist/haider-v9.0.0-{target}.tar.xz
",
        "0".repeat(64)
    );
    let (selection, mut transport) =
        selection_and_transport(install.path(), &members, Some(wrong_digest_checksum));
    let calls = Arc::new(AtomicUsize::new(0));
    let verifier = FakeVerifier {
        failure: VerifyFailure::None,
        calls: Arc::clone(&calls),
    };
    assert!(stage_release(&mut transport, &verifier, install.path(), &selection).is_err());
    assert_eq!(calls.load(Ordering::SeqCst), 0);
    assert_eq!(pair_snapshot(install.path()), before);
    assert!(stage_entries(install.path()).is_empty());
}

/// MUTATION CHECK: allow traversal/absolute/link/device/duplicate/extra,
/// missing, oversized, or non-executable archive members. Expected RUNTIME
/// failure: one malformed table row yields a verified capability.
#[test]
fn strict_archive_rejects_every_forbidden_member_shape() {
    let target = cli_main::update::discovery::compiled_target().expect("target");
    let top = format!("haider-v9.0.0-{target}");
    let valid = expected_members("9.0.0", target);
    let mut cases: Vec<(&str, Vec<Member>)> = Vec::new();
    let mut traversal = valid.clone();
    traversal[1].name = "../haider".into();
    cases.push(("traversal", traversal));
    let mut absolute = valid.clone();
    absolute[1].name = "/tmp/haider".into();
    cases.push(("absolute", absolute));
    for (name, kind) in [
        ("symlink", b'2'),
        ("hardlink", b'1'),
        ("device", b'3'),
        ("fifo", b'6'),
    ] {
        let mut members = valid.clone();
        members[1].kind = kind;
        members[1].data.clear();
        cases.push((name, members));
    }
    let mut duplicate = valid.clone();
    duplicate.push(valid[1].clone());
    cases.push(("duplicate", duplicate));
    let mut extra = valid.clone();
    extra.push(Member {
        name: format!("{top}/README"),
        kind: b'0',
        mode: 0o644,
        data: b"extra".to_vec(),
        declared_size: None,
    });
    cases.push(("extra", extra));
    cases.push(("missing-cli", vec![valid[0].clone(), valid[2].clone()]));
    cases.push(("missing-daemon", vec![valid[0].clone(), valid[1].clone()]));
    let mut oversized = valid.clone();
    oversized[1].declared_size = Some(256 * 1024 * 1024 + 1);
    oversized[1].data.clear();
    cases.push(("oversized", oversized));
    let mut non_executable = valid.clone();
    non_executable[1].mode = 0o600;
    cases.push(("non-executable", non_executable));
    let mut wrong_top = valid.clone();
    wrong_top[0].name = "different/".into();
    cases.push(("wrong-target-top", wrong_top));

    for (name, members) in cases {
        let install = install_fixture();
        let before = pair_snapshot(install.path());
        let (selection, mut transport) = selection_and_transport(install.path(), &members, None);
        let verifier = FakeVerifier {
            failure: VerifyFailure::None,
            calls: Arc::new(AtomicUsize::new(0)),
        };
        assert!(
            stage_release(&mut transport, &verifier, install.path(), &selection).is_err(),
            "case {name} must refuse"
        );
        assert_eq!(pair_snapshot(install.path()), before, "case {name}");
        assert!(stage_entries(install.path()).is_empty(), "case {name}");
    }
}

/// MUTATION CHECK: ignore xattr/sign/signature/smoke failures. Expected
/// RUNTIME failure: a failure category returns a capability or changes the
/// byte/inode/mode snapshot of either canonical path.
#[test]
fn every_staged_verification_failure_leaves_installed_pair_exact() {
    let target = cli_main::update::discovery::compiled_target().expect("target");
    let members = expected_members("9.0.0", target);
    for failure in [
        VerifyFailure::Xattr,
        VerifyFailure::Sign,
        VerifyFailure::Signature,
        VerifyFailure::SmokeCli,
        VerifyFailure::SmokeDaemon,
    ] {
        let install = install_fixture();
        let before = pair_snapshot(install.path());
        let (selection, mut transport) = selection_and_transport(install.path(), &members, None);
        let verifier = FakeVerifier {
            failure,
            calls: Arc::new(AtomicUsize::new(0)),
        };
        assert!(stage_release(&mut transport, &verifier, install.path(), &selection).is_err());
        assert_eq!(pair_snapshot(install.path()), before, "failure {failure:?}");
        assert!(stage_entries(install.path()).is_empty());
    }
}

struct PartialDownloadTransport;

impl UpdateTransport for PartialDownloadTransport {
    fn get_bytes(&mut self, _url: &str, _limit: usize) -> Result<Vec<u8>, UpdateError> {
        unreachable!("partial fixture does not discover")
    }

    fn download(&mut self, _url: &str, path: &Path, _limit: u64) -> Result<(), UpdateError> {
        fs::write(path, b"partial").expect("write partial fixture");
        Err(UpdateError::Network("injected disconnect".into()))
    }
}

/// MUTATION CHECK: let a disconnected `.part` reach checksum/extract/commit.
/// Expected RUNTIME failure: verifier calls or canonical inode/bytes/mode
/// change, or the owner-only stage survives the failed transfer.
#[test]
fn partial_download_cannot_escape_immutable_staging() {
    let install = install_fixture();
    let before = pair_snapshot(install.path());
    let target = cli_main::update::discovery::compiled_target().expect("target");
    let selection = ReleaseSelection {
        version: SemVersion::parse("9.0.0").expect("version"),
        archive_name: format!("haider-v9.0.0-{target}.tar.xz"),
        archive_url: "partial-archive".into(),
        checksum_name: format!("haider-v9.0.0-{target}.tar.xz.sha256"),
        checksum_url: "never-reached".into(),
    };
    let calls = Arc::new(AtomicUsize::new(0));
    let verifier = FakeVerifier {
        failure: VerifyFailure::None,
        calls: Arc::clone(&calls),
    };
    assert!(
        stage_release(
            &mut PartialDownloadTransport,
            &verifier,
            install.path(),
            &selection,
        )
        .is_err()
    );
    assert_eq!(calls.load(Ordering::SeqCst), 0);
    assert_eq!(pair_snapshot(install.path()), before);
    assert!(stage_entries(install.path()).is_empty());
}

/// MUTATION CHECK: acquire/recover before immutable staging. Expected
/// RUNTIME failure: the planted new/new pending transaction is rolled back
/// even though the partial transfer cannot produce a staged capability.
#[test]
fn failed_staging_cannot_enter_transaction_or_recover_a_pending_marker() {
    let install = install_fixture();
    let layout = InstallLayout::for_test(install.path().to_path_buf());
    let prepared = PreparedTransaction::acquire(layout.clone()).expect("prepare planted update");
    let pair = verified_pair(install.path());
    let committed = commit_pair(prepared, pair, &NoFaults, &BytePairVerifier, "1.0.0")
        .expect("plant pending new pair");
    drop(committed);
    let pending_pair = pair_snapshot(install.path());
    assert!(marker_path(&layout).exists());

    let target = cli_main::update::discovery::compiled_target().expect("target");
    let selection = ReleaseSelection {
        version: SemVersion::parse("10.0.0").expect("version"),
        archive_name: format!("haider-v10.0.0-{target}.tar.xz"),
        archive_url: "partial-archive".into(),
        checksum_name: format!("haider-v10.0.0-{target}.tar.xz.sha256"),
        checksum_url: "never-reached".into(),
    };
    let verifier = FakeVerifier {
        failure: VerifyFailure::None,
        calls: Arc::new(AtomicUsize::new(0)),
    };
    assert!(
        stage_then_acquire(
            &mut PartialDownloadTransport,
            &verifier,
            layout.clone(),
            &selection,
        )
        .is_err()
    );
    assert_eq!(pair_snapshot(install.path()), pending_pair);
    assert!(marker_path(&layout).exists());
    assert!(stage_entries(install.path()).is_empty());
}

struct BoundaryFault(CommitBoundary);

impl FaultInjector for BoundaryFault {
    fn after(&self, boundary: CommitBoundary) -> Result<(), UpdateError> {
        if boundary == self.0 {
            Err(UpdateError::Internal(format!(
                "injected boundary {boundary:?}"
            )))
        } else {
            Ok(())
        }
    }
}

struct BytePairVerifier;

impl InstalledPairVerifier for BytePairVerifier {
    fn verify(
        &self,
        layout: &InstallLayout,
        _pair: &cli_main::update::staging::VerifiedStagedPair,
    ) -> Result<(), UpdateError> {
        if fs::read(&layout.haider).expect("live CLI") == b"new-cli"
            && fs::read(&layout.haiderd).expect("live daemon") == b"new-daemon"
        {
            Ok(())
        } else {
            Err(UpdateError::Internal("not the new pair".into()))
        }
    }
}

struct FailingPairVerifier;

impl InstalledPairVerifier for FailingPairVerifier {
    fn verify(
        &self,
        _layout: &InstallLayout,
        _pair: &cli_main::update::staging::VerifiedStagedPair,
    ) -> Result<(), UpdateError> {
        Err(UpdateError::Refused(
            "injected installed-pair verification failure".into(),
        ))
    }
}

fn verified_pair(install: &Path) -> cli_main::update::staging::VerifiedStagedPair {
    let target = cli_main::update::discovery::compiled_target().expect("target");
    let members = expected_members("9.0.0", target);
    let (selection, mut transport) = selection_and_transport(install, &members, None);
    stage_release(
        &mut transport,
        &FakeVerifier {
            failure: VerifyFailure::None,
            calls: Arc::new(AtomicUsize::new(0)),
        },
        install,
        &selection,
    )
    .expect("verified pair")
}

/// MUTATION CHECK: omit rollback or move a fault hook before its named
/// boundary. Expected RUNTIME failure: one of all eight rows does not restore
/// the exact old inode/bytes/mode pair, or a restart entry becomes possible.
#[test]
fn every_commit_boundary_fault_restores_exact_old_pair() {
    for boundary in [
        CommitBoundary::BackupDaemon,
        CommitBoundary::BackupCli,
        CommitBoundary::RenameDaemon,
        CommitBoundary::RenameCli,
        CommitBoundary::ChmodDaemon,
        CommitBoundary::ChmodCli,
        CommitBoundary::InstallDirFsync,
        CommitBoundary::InstalledPairVerify,
    ] {
        let install = install_fixture();
        let before = pair_snapshot(install.path());
        let pair = verified_pair(install.path());
        let layout = InstallLayout::for_test(install.path().to_path_buf());
        let prepared = PreparedTransaction::acquire(layout).expect("prepare transaction");
        let result = commit_pair(
            prepared,
            pair,
            &BoundaryFault(boundary),
            &BytePairVerifier,
            "1.0.0",
        );
        assert!(result.is_err(), "boundary {boundary:?}");
        assert_eq!(
            pair_snapshot(install.path()),
            before,
            "boundary {boundary:?}"
        );
        assert!(!marker_path(&InstallLayout::for_test(install.path().to_path_buf())).exists());
    }
}

/// MUTATION CHECK: skip the pre-commit capability recheck. Expected RUNTIME
/// failure: altered staged bytes create a marker/backup or transiently replace
/// a canonical path instead of being refused before the transaction starts.
#[test]
fn altered_staged_capability_is_refused_before_marker_or_backup() {
    let install = install_fixture();
    let before = pair_snapshot(install.path());
    let pair = verified_pair(install.path());
    fs::set_permissions(pair.haider_path(), fs::Permissions::from_mode(0o700))
        .expect("make staged fixture writable");
    fs::write(pair.haider_path(), b"tampered-staged-cli").expect("alter staged fixture");
    let prepared =
        PreparedTransaction::acquire(InstallLayout::for_test(install.path().to_path_buf()))
            .expect("prepare transaction");
    assert!(commit_pair(prepared, pair, &NoFaults, &BytePairVerifier, "1.0.0").is_err());
    assert_eq!(pair_snapshot(install.path()), before);
    let layout = InstallLayout::for_test(install.path().to_path_buf());
    assert!(!marker_path(&layout).exists());
    assert!(!has_entry(install.path(), ".haider-old-"));
    assert!(!has_entry(install.path(), ".haiderd-old-"));
}

/// MUTATION CHECK: let a post-swap verifier failure produce a restart
/// capability. Expected RUNTIME failure: commit returns `Ok`, or rollback
/// does not restore the exact old pair before any restart call is possible.
#[test]
fn installed_pair_verifier_failure_rolls_back_without_restart_capability() {
    let install = install_fixture();
    let before = pair_snapshot(install.path());
    let pair = verified_pair(install.path());
    let layout = InstallLayout::for_test(install.path().to_path_buf());
    let prepared = PreparedTransaction::acquire(layout.clone()).expect("prepare transaction");
    assert!(commit_pair(prepared, pair, &NoFaults, &FailingPairVerifier, "1.0.0").is_err());
    assert_eq!(pair_snapshot(install.path()), before);
    assert!(!marker_path(&layout).exists());
}

/// MUTATION CHECK: accept a mixed canonical pair during recovery or make
/// recovery depend on marker phase. Expected RUNTIME failure: one of every
/// durable phase fixtures remains new/old after the next lock acquisition.
#[test]
fn every_marker_phase_recovers_to_a_never_mixed_old_pair() {
    for phase in [
        TransactionPhase::Prepared,
        TransactionPhase::BackupDaemon,
        TransactionPhase::BackupsReady,
        TransactionPhase::DaemonInstalled,
        TransactionPhase::PairInstalled,
        TransactionPhase::DaemonWritable,
        TransactionPhase::PairWritable,
        TransactionPhase::PairSynced,
        TransactionPhase::PairVerified,
        TransactionPhase::RestartPending,
        TransactionPhase::DrainSignaled,
        TransactionPhase::LockReleased,
        TransactionPhase::ChildSpawned,
        TransactionPhase::Finalizing,
        TransactionPhase::RollingBack,
    ] {
        let install = install_fixture();
        let old = pair_snapshot(install.path());
        let pair = verified_pair(install.path());
        let layout = InstallLayout::for_test(install.path().to_path_buf());
        let prepared = PreparedTransaction::acquire(layout.clone()).expect("prepare");
        let mut committed = commit_pair(prepared, pair, &NoFaults, &BytePairVerifier, "1.0.0")
            .expect("commit fixture");
        let target = pair_snapshot(install.path());
        shape_crash_fixture(install.path(), &layout, phase);
        committed.set_phase(phase).expect("plant crash phase");
        drop(committed);
        let recovered = PreparedTransaction::acquire(layout)
            .unwrap_or_else(|error| panic!("recover phase {phase:?}: {error}"));
        if phase == TransactionPhase::Finalizing {
            assert_eq!(pair_snapshot(install.path()), target, "phase {phase:?}");
            assert!(!marker_path(&InstallLayout::for_test(install.path().to_path_buf())).exists());
            assert!(!has_entry(install.path(), ".haider-old-"));
            assert!(!has_entry(install.path(), ".haiderd-old-"));
        } else {
            assert_eq!(pair_snapshot(install.path()), old, "phase {phase:?}");
        }
        drop(recovered);
    }
}

fn shape_crash_fixture(dir: &Path, layout: &InstallLayout, phase: TransactionPhase) {
    let cli_backup = find_entry(dir, ".haider-old-");
    let daemon_backup = find_entry(dir, ".haiderd-old-");
    match phase {
        TransactionPhase::Prepared => {
            fs::rename(daemon_backup, &layout.haiderd).expect("restore prepared daemon");
            fs::rename(cli_backup, &layout.haider).expect("restore prepared CLI");
        }
        TransactionPhase::BackupDaemon => {
            replace_from_backup(&daemon_backup, &layout.haiderd, "daemon");
            fs::rename(cli_backup, &layout.haider).expect("restore one-backup CLI");
        }
        TransactionPhase::BackupsReady => {
            replace_from_backup(&daemon_backup, &layout.haiderd, "daemon");
            replace_from_backup(&cli_backup, &layout.haider, "cli");
        }
        TransactionPhase::DaemonInstalled => {
            replace_from_backup(&cli_backup, &layout.haider, "cli");
            assert_eq!(
                fs::read(&layout.haiderd).expect("mixed daemon"),
                b"new-daemon"
            );
            assert_eq!(fs::read(&layout.haider).expect("mixed CLI"), b"old-cli");
        }
        TransactionPhase::RollingBack => {
            fs::rename(daemon_backup, &layout.haiderd).expect("first rollback rename");
            assert_eq!(
                fs::read(&layout.haiderd).expect("old daemon"),
                b"old-daemon"
            );
            assert_eq!(fs::read(&layout.haider).expect("new CLI"), b"new-cli");
        }
        TransactionPhase::PairInstalled => {
            set_mode(&layout.haiderd, 0o500);
            set_mode(&layout.haider, 0o500);
            assert_eq!(
                fs::metadata(&layout.haiderd).expect("daemon mode").mode() & 0o777,
                0o500
            );
            assert_eq!(
                fs::metadata(&layout.haider).expect("CLI mode").mode() & 0o777,
                0o500
            );
        }
        TransactionPhase::DaemonWritable => {
            set_mode(&layout.haider, 0o500);
            assert_eq!(
                fs::metadata(&layout.haiderd).expect("daemon mode").mode() & 0o777,
                0o700
            );
            assert_eq!(
                fs::metadata(&layout.haider).expect("CLI mode").mode() & 0o777,
                0o500
            );
        }
        TransactionPhase::PairWritable
        | TransactionPhase::PairSynced
        | TransactionPhase::PairVerified
        | TransactionPhase::RestartPending
        | TransactionPhase::DrainSignaled
        | TransactionPhase::LockReleased
        | TransactionPhase::ChildSpawned
        | TransactionPhase::Finalizing => {
            assert_eq!(
                fs::read(&layout.haiderd).expect("new daemon"),
                b"new-daemon"
            );
            assert_eq!(fs::read(&layout.haider).expect("new CLI"), b"new-cli");
        }
    }
}

fn set_mode(path: &Path, mode: u32) {
    fs::set_permissions(path, fs::Permissions::from_mode(mode)).expect("shape crash mode");
}

fn replace_from_backup(backup: &Path, canonical: &Path, label: &str) {
    let temporary = canonical.with_file_name(format!(".restore-{label}-fixture"));
    fs::hard_link(backup, &temporary).expect("link phase-accurate restore fixture");
    fs::rename(temporary, canonical).expect("publish phase-accurate restore fixture");
}

/// MUTATION CHECK: require both backups to exist even when one canonical path
/// was already restored before a crash. Expected RUNTIME failure: recovery
/// refuses the valid old/new fixture or leaves it mixed.
#[test]
fn recovery_completes_a_partially_restored_pair() {
    let install = install_fixture();
    let old = pair_snapshot(install.path());
    let pair = verified_pair(install.path());
    let layout = InstallLayout::for_test(install.path().to_path_buf());
    let prepared = PreparedTransaction::acquire(layout.clone()).expect("prepare");
    let committed =
        commit_pair(prepared, pair, &NoFaults, &BytePairVerifier, "1.0.0").expect("commit fixture");
    let daemon_backup = find_entry(install.path(), ".haiderd-old-");
    fs::rename(daemon_backup, &layout.haiderd).expect("simulate first rollback rename");
    drop(committed);
    let recovered = PreparedTransaction::acquire(layout).expect("complete recovery");
    assert_eq!(pair_snapshot(install.path()), old);
    drop(recovered);
}

/// MUTATION CHECK: consume a corrupt recovery backup or delete the marker on
/// refusal. Expected RUNTIME failure: recovery succeeds or the durable marker
/// disappears instead of retaining operator-recoverable evidence.
#[test]
fn corrupt_recovery_backup_refuses_and_retains_marker() {
    let install = install_fixture();
    let pair = verified_pair(install.path());
    let layout = InstallLayout::for_test(install.path().to_path_buf());
    let prepared = PreparedTransaction::acquire(layout.clone()).expect("prepare");
    let committed =
        commit_pair(prepared, pair, &NoFaults, &BytePairVerifier, "1.0.0").expect("commit fixture");
    let backup = find_entry(install.path(), ".haider-old-");
    fs::write(backup, b"corrupt-old-cli").expect("corrupt backup fixture");
    drop(committed);
    assert!(PreparedTransaction::acquire(layout.clone()).is_err());
    assert!(marker_path(&layout).exists());
}

/// MUTATION CHECK: ignore update-lock exclusion, symlink layout, or owner
/// writability. Expected RUNTIME failure: the second lock or an ambiguous
/// managed binary layout is admitted.
#[test]
fn transaction_refuses_concurrency_symlinks_and_read_only_layouts() {
    let install = install_fixture();
    let layout = InstallLayout::for_test(install.path().to_path_buf());
    let first = PreparedTransaction::acquire(layout.clone()).expect("first lock");
    assert!(PreparedTransaction::acquire(layout).is_err());
    drop(first);

    let symlinked = tempfile::tempdir().expect("symlink fixture");
    write_executable(&symlinked.path().join("real-haider"), b"old-cli");
    write_executable(&symlinked.path().join("haiderd"), b"old-daemon");
    std::os::unix::fs::symlink(
        symlinked.path().join("real-haider"),
        symlinked.path().join("haider"),
    )
    .expect("symlink CLI");
    assert!(
        PreparedTransaction::acquire(InstallLayout::for_test(symlinked.path().to_path_buf()))
            .is_err()
    );

    let read_only = install_fixture();
    fs::set_permissions(
        read_only.path().join("haiderd"),
        fs::Permissions::from_mode(0o500),
    )
    .expect("make managed fixture read-only");
    assert!(
        PreparedTransaction::acquire(InstallLayout::for_test(read_only.path().to_path_buf()))
            .is_err()
    );
}

fn install_fixture() -> tempfile::TempDir {
    let install = tempfile::tempdir().expect("install fixture");
    write_executable(&install.path().join("haider"), b"old-cli");
    write_executable(&install.path().join("haiderd"), b"old-daemon");
    install
}

fn write_executable(path: &Path, bytes: &[u8]) {
    fs::write(path, bytes).expect("write fixture binary");
    fs::set_permissions(path, fs::Permissions::from_mode(0o700)).expect("chmod fixture binary");
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct FileSnapshot {
    bytes: Vec<u8>,
    mode: u32,
    inode: u64,
}

fn pair_snapshot(dir: &Path) -> (FileSnapshot, FileSnapshot) {
    (
        snapshot(&dir.join("haider")),
        snapshot(&dir.join("haiderd")),
    )
}

fn snapshot(path: &Path) -> FileSnapshot {
    let metadata = fs::metadata(path).expect("fixture metadata");
    FileSnapshot {
        bytes: fs::read(path).expect("fixture bytes"),
        mode: metadata.mode(),
        inode: metadata.ino(),
    }
}

fn stage_entries(dir: &Path) -> Vec<PathBuf> {
    fs::read_dir(dir)
        .expect("read install")
        .filter_map(Result::ok)
        .map(|entry| entry.path())
        .filter(|path| {
            path.file_name()
                .and_then(|name| name.to_str())
                .is_some_and(|name| name.starts_with(".haider-update-stage-"))
        })
        .collect()
}

fn find_entry(dir: &Path, prefix: &str) -> PathBuf {
    fs::read_dir(dir)
        .expect("read fixture directory")
        .filter_map(Result::ok)
        .map(|entry| entry.path())
        .find(|path| {
            path.file_name()
                .and_then(|name| name.to_str())
                .is_some_and(|name| name.starts_with(prefix))
        })
        .unwrap_or_else(|| panic!("fixture entry with prefix {prefix}"))
}

fn has_entry(dir: &Path, prefix: &str) -> bool {
    fs::read_dir(dir)
        .expect("read fixture directory")
        .filter_map(Result::ok)
        .any(|entry| {
            entry
                .file_name()
                .to_str()
                .is_some_and(|name| name.starts_with(prefix))
        })
}
