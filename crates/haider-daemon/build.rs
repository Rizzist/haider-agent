use std::hash::{BuildHasher as _, Hasher as _};
use std::time::{SystemTime, UNIX_EPOCH};

fn main() {
    println!("cargo::rerun-if-changed=build.rs");
    let mut first = std::collections::hash_map::RandomState::new().build_hasher();
    first.write_u128(
        SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap_or_default()
            .as_nanos(),
    );
    first.write_u32(std::process::id());
    let mut second = std::collections::hash_map::RandomState::new().build_hasher();
    second.write_u64(first.finish());
    second.write(env!("CARGO_PKG_VERSION").as_bytes());
    println!(
        "cargo::rustc-env=HAIDER_BUILD_UUID={:016x}{:016x}",
        first.finish(),
        second.finish()
    );
}
