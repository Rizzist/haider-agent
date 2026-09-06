//! Offline payload identity. Reading an executable as bounded file data does
//! not execute or map any of its TUI, audio or graphics code into this process.
use std::io::{self, Read};
use std::path::Path;

/// Build the search bytes at runtime. Only the payload executable emits the
/// complete contiguous marker; embedding it here would identify the thin CLI
/// and daemon as payloads too, allowing an accidental sibling copy to exec-loop.
pub fn marker_for_version(version: &str) -> Vec<u8> {
    let mut marker = b"haider.payload.v1\0".to_vec();
    marker.extend_from_slice(std::hint::black_box(version).as_bytes());
    marker.push(0);
    marker
}

pub fn verify_payload(path: &Path) -> io::Result<()> {
    let file = std::fs::File::open(path)?;
    if !file.metadata()?.is_file() {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "payload is not a regular executable",
        ));
    }
    verify_marker(file, &marker_for_version(env!("CARGO_PKG_VERSION")))
}

fn verify_marker(mut reader: impl Read, marker: &[u8]) -> io::Result<()> {
    let mut buffer = vec![0; 64 * 1024 + marker.len()];
    let mut retained = 0;
    loop {
        let read = reader.read(&mut buffer[retained..])?;
        if read == 0 {
            break;
        }
        let available = retained + read;
        if buffer[..available]
            .windows(marker.len())
            .any(|window| window == marker)
        {
            return Ok(());
        }
        retained = (marker.len() - 1).min(available);
        buffer.copy_within(available - retained..available, 0);
    }
    Err(io::Error::new(
        io::ErrorKind::InvalidData,
        "payload build/version does not match this haider",
    ))
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn identity_crosses_read_boundaries_and_rejects_other_versions() {
        let marker = b"haider.payload.v1\0test\0";
        let mut bytes = vec![b'x'; 64 * 1024 + marker.len() - 3];
        bytes.extend_from_slice(marker);
        assert!(verify_marker(&bytes[..], marker).is_ok());
        assert!(verify_marker(&bytes[..], b"haider.payload.v1\0other\0").is_err());
        assert!(verify_marker(&b""[..], marker).is_err());
    }
}
