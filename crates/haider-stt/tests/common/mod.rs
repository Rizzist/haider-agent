#![allow(dead_code)]
//! Shared loopback fixtures for the haider-stt law suites.
//!
//! These laws use loopback TCP. In restricted sandboxes that prohibit
//! `bind(2)` they compile but stop at fixture setup; they execute normally
//! in the workspace/CI runtime where loopback fixtures are permitted.

use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};

/// One canned HTTP response.
#[derive(Clone)]
pub struct CannedResponse {
    pub status: u16,
    pub content_type: &'static str,
    pub body: Vec<u8>,
}

impl CannedResponse {
    pub fn ok_bytes(body: Vec<u8>) -> Self {
        Self {
            status: 200,
            content_type: "application/octet-stream",
            body,
        }
    }

    pub fn ok_json(body: &str) -> Self {
        Self {
            status: 200,
            content_type: "application/json",
            body: body.as_bytes().to_vec(),
        }
    }

    pub fn status_only(status: u16) -> Self {
        Self {
            status,
            content_type: "text/plain",
            body: Vec::new(),
        }
    }
}

/// A minimal loopback HTTP/1.1 fixture: answers every request from a
/// path → response table, records hit counts and request lines.
pub struct HttpFixture {
    pub origin: String,
    pub hits: Arc<AtomicUsize>,
    pub seen: Arc<std::sync::Mutex<Vec<String>>>,
}

pub async fn spawn_http_fixture(routes: Vec<(String, CannedResponse)>) -> HttpFixture {
    use tokio::io::{AsyncReadExt as _, AsyncWriteExt as _};
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
        .await
        .expect("bind loopback HTTP fixture");
    let addr = listener.local_addr().expect("fixture addr");
    let hits = Arc::new(AtomicUsize::new(0));
    let seen = Arc::new(std::sync::Mutex::new(Vec::new()));
    let hits_for_task = Arc::clone(&hits);
    let seen_for_task = Arc::clone(&seen);
    tokio::spawn(async move {
        loop {
            let Ok((mut stream, _)) = listener.accept().await else {
                return;
            };
            let routes = routes.clone();
            let hits = Arc::clone(&hits_for_task);
            let seen = Arc::clone(&seen_for_task);
            tokio::spawn(async move {
                let mut head = Vec::new();
                let mut buffer = [0u8; 1024];
                // Read until the end of the request head; fixture requests
                // are GETs with no body.
                while !head.windows(4).any(|window| window == b"\r\n\r\n") {
                    match stream.read(&mut buffer).await {
                        Ok(0) | Err(_) => return,
                        Ok(read) => head.extend_from_slice(&buffer[..read]),
                    }
                }
                let head_text = String::from_utf8_lossy(&head).to_string();
                let request_line = head_text.lines().next().unwrap_or_default().to_owned();
                let path = request_line
                    .split(' ')
                    .nth(1)
                    .unwrap_or_default()
                    .to_owned();
                hits.fetch_add(1, Ordering::SeqCst);
                if let Ok(mut seen) = seen.lock() {
                    seen.push(head_text.clone());
                }
                let response = routes
                    .iter()
                    .find(|(route, _)| route == &path)
                    .map(|(_, response)| response.clone())
                    .unwrap_or(CannedResponse {
                        status: 404,
                        content_type: "text/plain",
                        body: b"not found".to_vec(),
                    });
                let reason = match response.status {
                    200 => "OK",
                    401 => "Unauthorized",
                    403 => "Forbidden",
                    404 => "Not Found",
                    500 => "Internal Server Error",
                    _ => "Status",
                };
                let header = format!(
                    "HTTP/1.1 {} {}\r\ncontent-type: {}\r\ncontent-length: {}\r\nconnection: close\r\n\r\n",
                    response.status,
                    reason,
                    response.content_type,
                    response.body.len(),
                );
                let _ = stream.write_all(header.as_bytes()).await;
                let _ = stream.write_all(&response.body).await;
                let _ = stream.shutdown().await;
            });
        }
    });
    HttpFixture {
        origin: format!("http://{addr}"),
        hits,
        seen,
    }
}

/// CRC-32 (IEEE) for the stored-zip builder.
fn crc32(bytes: &[u8]) -> u32 {
    let mut crc = 0xFFFF_FFFFu32;
    for byte in bytes {
        crc ^= u32::from(*byte);
        for _ in 0..8 {
            let mask = 0u32.wrapping_sub(crc & 1);
            crc = (crc >> 1) ^ (0xEDB8_8320 & mask);
        }
    }
    !crc
}

/// Builds a minimal STORED (no compression) zip archive in memory — enough
/// for bsdtar to read, with fully controlled entry names.
pub fn build_stored_zip(entries: &[(&str, &[u8])]) -> Vec<u8> {
    let mut archive = Vec::new();
    let mut central = Vec::new();
    for (name, data) in entries {
        let offset = archive.len() as u32;
        let crc = crc32(data);
        let name_bytes = name.as_bytes();
        // Local file header.
        archive.extend_from_slice(&[0x50, 0x4B, 0x03, 0x04]);
        archive.extend_from_slice(&20u16.to_le_bytes()); // version needed
        archive.extend_from_slice(&0u16.to_le_bytes()); // flags
        archive.extend_from_slice(&0u16.to_le_bytes()); // method: stored
        archive.extend_from_slice(&0u16.to_le_bytes()); // mod time
        archive.extend_from_slice(&0u16.to_le_bytes()); // mod date
        archive.extend_from_slice(&crc.to_le_bytes());
        archive.extend_from_slice(&(data.len() as u32).to_le_bytes());
        archive.extend_from_slice(&(data.len() as u32).to_le_bytes());
        archive.extend_from_slice(&(name_bytes.len() as u16).to_le_bytes());
        archive.extend_from_slice(&0u16.to_le_bytes()); // extra len
        archive.extend_from_slice(name_bytes);
        archive.extend_from_slice(data);
        // Central directory record.
        central.extend_from_slice(&[0x50, 0x4B, 0x01, 0x02]);
        central.extend_from_slice(&20u16.to_le_bytes()); // version made by
        central.extend_from_slice(&20u16.to_le_bytes()); // version needed
        central.extend_from_slice(&0u16.to_le_bytes()); // flags
        central.extend_from_slice(&0u16.to_le_bytes()); // method
        central.extend_from_slice(&0u16.to_le_bytes()); // time
        central.extend_from_slice(&0u16.to_le_bytes()); // date
        central.extend_from_slice(&crc.to_le_bytes());
        central.extend_from_slice(&(data.len() as u32).to_le_bytes());
        central.extend_from_slice(&(data.len() as u32).to_le_bytes());
        central.extend_from_slice(&(name_bytes.len() as u16).to_le_bytes());
        central.extend_from_slice(&0u16.to_le_bytes()); // extra len
        central.extend_from_slice(&0u16.to_le_bytes()); // comment len
        central.extend_from_slice(&0u16.to_le_bytes()); // disk start
        central.extend_from_slice(&0u16.to_le_bytes()); // internal attrs
        central.extend_from_slice(&0o100755u32.wrapping_shl(16).to_le_bytes()); // external attrs
        central.extend_from_slice(&offset.to_le_bytes());
        central.extend_from_slice(name_bytes);
    }
    let central_offset = archive.len() as u32;
    let central_size = central.len() as u32;
    archive.extend_from_slice(&central);
    // End of central directory.
    archive.extend_from_slice(&[0x50, 0x4B, 0x05, 0x06]);
    archive.extend_from_slice(&0u16.to_le_bytes()); // disk
    archive.extend_from_slice(&0u16.to_le_bytes()); // cd disk
    archive.extend_from_slice(&(entries.len() as u16).to_le_bytes());
    archive.extend_from_slice(&(entries.len() as u16).to_le_bytes());
    archive.extend_from_slice(&central_size.to_le_bytes());
    archive.extend_from_slice(&central_offset.to_le_bytes());
    archive.extend_from_slice(&0u16.to_le_bytes()); // comment len
    archive
}

/// Writes an executable stub script and returns its path (unix test hosts).
pub fn write_stub_script(dir: &std::path::Path, name: &str, body: &str) -> std::path::PathBuf {
    let path = dir.join(name);
    std::fs::write(&path, body).expect("write stub script");
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt as _;
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o755))
            .expect("make stub executable");
    }
    path
}

/// The sha256 of a byte slice as lowercase hex (for fixture specs).
pub fn sha256_hex(bytes: &[u8]) -> String {
    use sha2::Digest as _;
    hex::encode(sha2::Sha256::digest(bytes))
}
