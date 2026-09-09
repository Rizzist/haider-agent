//! Fixed-field lifecycle logs only: no error text, arguments, account aliases or URLs.
use crate::contract::Observation;
use rustix::fs::{AtFlags, Mode, OFlags};
use std::io::Write;
use std::path::Path;

fn validate_file(stat: &rustix::fs::Stat) -> std::io::Result<()> {
    if rustix::fs::FileType::from_raw_mode(stat.st_mode) != rustix::fs::FileType::RegularFile
        || stat.st_uid != rustix::process::geteuid().as_raw()
        || stat.st_mode & 0o777 != 0o600
        || stat.st_nlink != 1
    {
        return Err(std::io::Error::other("invalid native log"));
    }
    Ok(())
}

pub fn record(directory: &Path, observation: &Observation) -> std::io::Result<()> {
    let root = rustix::fs::open(
        directory,
        OFlags::RDONLY | OFlags::DIRECTORY | OFlags::NOFOLLOW | OFlags::CLOEXEC,
        Mode::empty(),
    )?;
    let bytes = serde_json::to_vec(&serde_json::json!({
        "jni_version":observation.jni_version, "phase":observation.phase,
        "daemon_generation":observation.daemon_generation, "error_code":observation.error_code,
        "daemon_version":env!("CARGO_PKG_VERSION")
    }))?;
    const CURRENT: &str = "native.log";
    match rustix::fs::statat(&root, CURRENT, AtFlags::SYMLINK_NOFOLLOW) {
        Ok(stat) => {
            validate_file(&stat)?;
            if (stat.st_size as u64).saturating_add(bytes.len() as u64 + 1) > 1_048_576 {
                let _ = rustix::fs::unlinkat(&root, "native.log.4", AtFlags::empty());
                for index in (1..4).rev() {
                    match rustix::fs::renameat(
                        &root,
                        format!("native.log.{index}"),
                        &root,
                        format!("native.log.{}", index + 1),
                    ) {
                        Ok(()) | Err(rustix::io::Errno::NOENT) => {}
                        Err(error) => return Err(error.into()),
                    }
                }
                rustix::fs::renameat(&root, CURRENT, &root, "native.log.1")?;
            }
        }
        Err(rustix::io::Errno::NOENT) => (),
        Err(error) => return Err(error.into()),
    }
    let mut file = std::fs::File::from(rustix::fs::openat(
        &root,
        CURRENT,
        OFlags::WRONLY
            | OFlags::CREATE
            | OFlags::APPEND
            | OFlags::NOFOLLOW
            | OFlags::CLOEXEC
            | OFlags::NONBLOCK,
        Mode::from_raw_mode(0o600),
    )?);
    validate_file(&rustix::fs::fstat(&file)?)?;
    file.write_all(&bytes)?;
    file.write_all(b"\n")
}

#[cfg(test)]
#[path = "logging_tests.rs"]
mod tests;
