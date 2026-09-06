//! macOS signature identity must survive the shared install staging path.
#![cfg(target_os = "macos")]
#![allow(clippy::expect_used)]

use haider_cli::update::staging::{StageVerifier, SystemStageVerifier, sha256_file};
use std::process::Command;

#[test]
fn staging_preserves_valid_signed_bytes_and_refuses_to_resign_corruption() {
    let root = tempfile::tempdir().expect("signature fixture");
    let binary = root.path().join("haider");
    std::fs::copy(env!("CARGO_BIN_EXE_haider"), &binary).expect("copy native executable");
    let signed = Command::new("/usr/bin/codesign")
        .args([
            "--force",
            "--sign",
            "-",
            "--timestamp=none",
            "--identifier",
            "ai.haider.test.signature-kept",
        ])
        .arg(&binary)
        .output()
        .expect("sign fixture with a distinguishable identity");
    assert!(signed.status.success(), "{signed:?}");
    let before = sha256_file(&binary).expect("signed digest");
    SystemStageVerifier
        .sign(&binary)
        .expect("preserve valid signature");
    SystemStageVerifier
        .verify_signature(&binary)
        .expect("signature remains valid");
    assert_eq!(sha256_file(&binary).expect("staged digest"), before);

    let mut corrupt = std::fs::read(&binary).expect("native signed bytes");
    corrupt[4096] ^= 1;
    std::fs::write(&binary, corrupt).expect("corrupt a signed page");
    let corrupted = sha256_file(&binary).expect("corrupted digest");
    assert!(SystemStageVerifier.verify_signature(&binary).is_err());
    assert!(SystemStageVerifier.sign(&binary).is_err());
    assert_eq!(sha256_file(&binary).expect("refused digest"), corrupted);
}
