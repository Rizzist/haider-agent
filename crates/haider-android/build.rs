fn main() {
    println!("cargo:rerun-if-changed=private-symbols.map");
    if std::env::var("CARGO_CFG_TARGET_OS").as_deref() == Ok("android") {
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
}
