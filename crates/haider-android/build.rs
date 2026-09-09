use sha2::{Digest, Sha256};
use std::path::{Path, PathBuf};

fn source_files(directory: &Path, files: &mut Vec<PathBuf>) -> std::io::Result<()> {
    for entry in std::fs::read_dir(directory)? {
        let entry = entry?;
        let path = entry.path();
        let kind = entry.file_type()?;
        if kind.is_dir() && entry.file_name() != "target" {
            source_files(&path, files)?;
        } else if kind.is_file()
            && (path.extension().is_some_and(|extension| extension == "rs")
                || entry.file_name() == "Cargo.toml")
        {
            files.push(path);
        }
    }
    Ok(())
}

fn main() -> Result<(), Box<dyn std::error::Error>> {
    println!("cargo:rerun-if-changed=private-symbols.map");
    if std::env::var("CARGO_CFG_TARGET_OS").as_deref() == Ok("android") {
        let manifest = PathBuf::from(std::env::var("CARGO_MANIFEST_DIR")?);
        let root = manifest
            .parent()
            .and_then(Path::parent)
            .ok_or("workspace root unavailable")?;
        let mut files = vec![
            root.join("Cargo.toml"),
            root.join("Cargo.lock"),
            root.join(".cargo/config.toml"),
            manifest.join("private-symbols.map"),
        ];
        source_files(&root.join("crates"), &mut files)?;
        files.sort();
        let mut digest = Sha256::new();
        for file in files {
            println!("cargo:rerun-if-changed={}", file.display());
            digest.update(file.strip_prefix(root)?.as_os_str().as_encoded_bytes());
            digest.update([0]);
            digest.update(std::fs::read(file)?);
            digest.update([0]);
        }
        println!(
            "cargo:rustc-env=HAIDER_ANDROID_SOURCE_ID=sha256:{:x}",
            digest.finalize()
        );
        println!("cargo:rustc-cdylib-link-arg=-Wl,--build-id=sha1");
        if std::env::var("CARGO_CFG_TARGET_ARCH").as_deref() == Ok("aarch64") {
            // BLAKE3's NEON C implementation calls this Rust helper. Keep the
            // optimized implementation, but make its helper private to this SO.
            let manifest = std::env::var("CARGO_MANIFEST_DIR").unwrap_or_default();
            println!(
                "cargo:rustc-cdylib-link-arg=-Wl,--version-script={manifest}/private-symbols.map"
            );
        }
    }
    Ok(())
}
