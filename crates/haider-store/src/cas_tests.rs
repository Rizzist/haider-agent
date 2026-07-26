#![allow(clippy::expect_used)]

use super::*;
use std::io::Cursor;

struct MutatingReader {
    source: Cursor<Vec<u8>>,
    mutate_after_first_read: bool,
}

impl Read for MutatingReader {
    fn read(&mut self, buffer: &mut [u8]) -> std::io::Result<usize> {
        let read = self.source.read(buffer)?;
        if self.mutate_after_first_read && read != 0 {
            self.mutate_after_first_read = false;
            let position = usize::try_from(self.source.position()).unwrap_or(usize::MAX);
            self.source.get_mut()[position..].fill(b'b');
        }
        Ok(read)
    }
}

#[test]
fn put_reader_publishes_mutated_source_bytes_under_their_actual_digest() {
    let root = tempfile::tempdir().expect("CAS root");
    let cas = FileCas::open(root.path()).expect("open CAS");
    let source = MutatingReader {
        source: Cursor::new(vec![b'a'; 32 * 1024]),
        mutate_after_first_read: true,
    };

    let artifact = cas
        .put_reader(source, Path::new("mutating-reader"))
        .expect("publish copied bytes");
    let published = cas.get(&artifact).expect("read published bytes");

    assert_eq!(&published[..16 * 1024], vec![b'a'; 16 * 1024]);
    assert_eq!(&published[16 * 1024..], vec![b'b'; 16 * 1024]);
    assert_eq!(
        artifact.as_str(),
        format!("blake3:{}", blake3::hash(&published).to_hex())
    );
    assert!(cas.verify(&artifact));
}
