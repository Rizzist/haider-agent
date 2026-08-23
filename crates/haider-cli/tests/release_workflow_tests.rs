//! Static release-workflow pins for the stripped-binary/symbol-companion law.

#![allow(clippy::expect_used)]

use std::path::Path;

#[test]
fn release_archives_daemon_symbols_without_publishing_them_as_distribution_files() {
    let workspace = Path::new(env!("CARGO_MANIFEST_DIR")).join("../..");
    let workflow_path = workspace.join(".github/workflows/release.yml");
    let workflow = std::fs::read_to_string(&workflow_path)
        .unwrap_or_else(|error| panic!("read {}: {error}", workflow_path.display()));

    for required in [
        "archive daemon symbols by native build ID",
        "haider-symbol-archive$EXE",
        "\"$ARCHIVER\" \"$DBIN\" \"$SYMBOLS\" \"symbols/${{ matrix.target }}\"",
        "macOS) SYMBOLS=\"$RELEASE_DIR/haiderd.dSYM\"",
        "Linux) SYMBOLS=\"$RELEASE_DIR/haiderd.dwp\"",
        "Windows) SYMBOLS=\"$RELEASE_DIR/haiderd.pdb\"",
        "name: symbols-${{ matrix.target }}",
        "retention-days: 30",
    ] {
        assert!(
            workflow.contains(required),
            "release workflow must retain the symbol-archive contract fragment `{required}`"
        );
    }
    assert_eq!(
        workflow.matches("pattern: distribution-*").count(),
        3,
        "release consumers must download distributions without mixing in private symbols"
    );

    let manifest_path = workspace.join("Cargo.toml");
    let manifest = std::fs::read_to_string(&manifest_path)
        .unwrap_or_else(|error| panic!("read {}: {error}", manifest_path.display()));
    assert!(
        manifest
            .lines()
            .any(|line| line.trim() == "strip = \"symbols\""),
        "the distributed daemon must remain stripped"
    );
}
