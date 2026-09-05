#![allow(clippy::expect_used)]

use super::*;
use base64::Engine as _;
use std::io::Cursor;
use std::sync::{Arc, Mutex};

fn staged_upload_count(cas: &FileCas) -> usize {
    fs::read_dir(&cas.root)
        .expect("CAS staging directory")
        .map(|entry| entry.expect("CAS staging entry"))
        .filter(|entry| entry.file_name().to_string_lossy().starts_with(".tmp-"))
        .count()
}

#[test]
fn streamed_cas_put_checks_digest_and_declared_length_before_publication() {
    let root = tempfile::tempdir().expect("CAS root");
    let cas = FileCas::open(root.path()).expect("open CAS");
    let bytes = b"three complete transport chunks";
    let expected = artifact_for(bytes);
    let mut upload = cas.begin_put(bytes.len() as u64).expect("begin upload");
    for chunk in bytes.chunks(7) {
        upload.write_chunk(chunk).expect("upload chunk");
        assert!(!cas.path_for(&expected).expect("object path").exists());
    }
    assert_eq!(upload.received_len(), bytes.len() as u64);
    assert_eq!(upload.finish(&expected).expect("publish"), expected);
    assert_eq!(cas.get(&expected).expect("read complete object"), bytes);
    assert_eq!(staged_upload_count(&cas), 0);

    let wrong_digest = artifact_for(b"wrong digest");
    let mut upload = cas.begin_put(bytes.len() as u64).expect("begin mismatch");
    upload.write_chunk(bytes).expect("write mismatch bytes");
    assert!(upload.finish(&wrong_digest).is_err());
    assert!(!cas.path_for(&wrong_digest).expect("mismatch path").exists());
    assert_eq!(staged_upload_count(&cas), 0);

    let incomplete = artifact_for(b"partial");
    let mut upload = cas.begin_put(8).expect("begin incomplete");
    upload.write_chunk(b"partial").expect("write prefix");
    assert!(upload.finish(&incomplete).is_err());
    assert!(!cas.path_for(&incomplete).expect("partial path").exists());
    assert_eq!(staged_upload_count(&cas), 0);
}

#[test]
fn streamed_cas_partial_drop_aborts_and_reconnect_retry_deduplicates() {
    let root = tempfile::tempdir().expect("CAS root");
    let cas = FileCas::open(root.path()).expect("open CAS");
    let bytes = b"restart an immutable upload after disconnect";
    let expected = artifact_for(bytes);
    {
        let mut upload = cas.begin_put(bytes.len() as u64).expect("begin upload");
        upload
            .write_chunk(&bytes[..9])
            .expect("write partial frame prefix");
        assert_eq!(staged_upload_count(&cas), 1);
    }
    assert_eq!(staged_upload_count(&cas), 0);
    assert!(!cas.path_for(&expected).expect("object path").exists());
    for _ in 0..2 {
        let mut upload = cas.begin_put(bytes.len() as u64).expect("retry upload");
        upload.write_chunk(bytes).expect("write complete retry");
        assert_eq!(upload.finish(&expected).expect("publish retry"), expected);
        assert_eq!(staged_upload_count(&cas), 0);
    }
    assert_eq!(cas.get(&expected).expect("retry bytes"), bytes);
}

#[test]
fn streamed_cas_overlong_chunk_poisons_complete_prefix() {
    let root = tempfile::tempdir().expect("CAS root");
    let cas = FileCas::open(root.path()).expect("open CAS");
    let expected = artifact_for(b"ok");
    let mut upload = cas.begin_put(2).expect("begin upload");
    upload.write_chunk(b"ok").expect("exact prefix");
    assert!(upload.write_chunk(b"extra").is_err());
    assert!(upload.finish(&expected).is_err());
    assert!(!cas.path_for(&expected).expect("object path").exists());
    assert_eq!(staged_upload_count(&cas), 0);
}

#[test]
fn streamed_cas_read_large_blob_is_seekable_and_detects_corruption_before_read() {
    let root = tempfile::tempdir().expect("CAS root");
    let cas = FileCas::open(root.path()).expect("open CAS");
    let chunk = [0x59_u8; 16 * 1024];
    let mut upload = cas.begin_put(8 * 1024 * 1024).expect("begin large upload");
    for _ in 0..512 {
        upload.write_chunk(&chunk).expect("write bounded chunk");
    }
    let artifact = upload.finish_computed().expect("publish large blob");
    let mut reader = cas
        .open_verified(&artifact)
        .expect("verified streamed reader");
    assert_eq!(reader.stream_position().expect("reader offset"), 0);
    assert_eq!(
        std::io::copy(&mut reader, &mut std::io::sink()).expect("stream blob"),
        8 * 1024 * 1024
    );
    reader.rewind().expect("seek verified object");
    let mut prefix = [0_u8; 32];
    reader.read_exact(&mut prefix).expect("read prefix");
    assert_eq!(prefix, [0x59_u8; 32]);
    drop(reader);

    let path = cas.path_for(&artifact).expect("object path");
    let mut corrupted = OpenOptions::new()
        .write(true)
        .open(path)
        .expect("open mutation target");
    corrupted.write_all(b"X").expect("mutate first byte");
    drop(corrupted);
    assert!(
        cas.open_verified(&artifact).is_err(),
        "corruption must fail before returning any reader"
    );
}

#[test]
fn streamed_cas_empty_upload_and_invalid_digest_leave_no_staging() {
    let root = tempfile::tempdir().expect("CAS root");
    let cas = FileCas::open(root.path()).expect("open CAS");
    let empty = artifact_for(b"");
    assert_eq!(
        cas.begin_put(0)
            .expect("empty upload")
            .finish(&empty)
            .expect("empty object"),
        empty
    );
    assert!(
        cas.begin_put(0)
            .expect("invalid upload")
            .finish(&ArtifactRef::new("blake3:../outside"))
            .is_err()
    );
    assert_eq!(staged_upload_count(&cas), 0);
}

/// MUTATION CHECK: omit the existing-object success branch's shard sync.
/// The losing uploader must close its own durability boundary while the
/// original publisher is paused between hard-link publication and sync.
#[test]
fn streamed_cas_dedup_syncs_shard_while_original_publisher_is_paused() {
    assert_duplicate_syncs_shard(|cas, bytes, expected| {
        let mut upload = cas.begin_put(bytes.len() as u64)?;
        upload.write_chunk(bytes)?;
        upload.finish(expected)
    });
}

#[test]
fn streamed_cas_publication_racing_legacy_put_and_file_put_still_syncs_shard() {
    assert_duplicate_syncs_shard(|cas, bytes, _expected| cas.put(bytes));
    assert_duplicate_syncs_shard(|cas, bytes, _expected| {
        cas.put_reader(Cursor::new(bytes), Path::new("legacy-reader"))
    });
}

fn assert_duplicate_syncs_shard(
    put: impl FnOnce(&FileCas, &[u8], &ArtifactRef) -> StoreResult<ArtifactRef>,
) {
    let root = tempfile::tempdir().expect("CAS root");
    let cas = FileCas::open(root.path()).expect("open CAS");
    let bytes = b"concurrent publication requires its own durable acknowledgement";
    let expected = artifact_for(bytes);
    let shard = cas
        .path_for(&expected)
        .expect("object path")
        .parent()
        .expect("shard")
        .to_path_buf();
    let publisher = cas.clone();
    let publisher_digest = expected.clone();
    let publisher_shard = shard.clone();
    let (linked_sender, linked_receiver) = std::sync::mpsc::sync_channel(1);
    let (release_sender, release_receiver) = std::sync::mpsc::sync_channel(1);
    let original = std::thread::spawn(move || {
        with_cas_sync_test_hook(
            move |path, policy, target| {
                if path == publisher_shard
                    && policy == haider_platform::SyncPolicy::Full
                    && target == CasSyncTarget::Directory
                {
                    linked_sender.send(()).expect("signal unsynced publication");
                    release_receiver.recv().expect("release original publisher");
                }
            },
            || {
                let mut upload = publisher
                    .begin_put(bytes.len() as u64)
                    .expect("original upload");
                upload.write_chunk(bytes).expect("original bytes");
                upload.finish(&publisher_digest).expect("original publish")
            },
        )
    });
    linked_receiver
        .recv()
        .expect("original link exists but is not synced");
    let observed = Arc::new(Mutex::new(Vec::new()));
    let observations = Arc::clone(&observed);
    let duplicate = with_cas_sync_test_hook(
        move |path, policy, target| {
            observations.lock().expect("sync observations").push((
                path.to_path_buf(),
                policy,
                target,
            ));
        },
        || put(&cas, bytes, &expected),
    );
    // Release before assertions, including on a failed duplicate, so a
    // mutation cannot strand a test thread behind the publication barrier.
    release_sender.send(()).expect("release publisher");
    assert_eq!(original.join().expect("publisher thread"), expected);
    assert_eq!(duplicate.expect("duplicate acknowledgement"), expected);
    assert!(
        observed
            .lock()
            .expect("sync observations")
            .iter()
            .any(|(path, policy, target)| {
                path == &shard
                    && *policy == haider_platform::SyncPolicy::Full
                    && *target == CasSyncTarget::Directory
            }),
        "deduplicated acknowledgement must sync the target shard independently"
    );
}

fn png(width: u32, height: u32) -> Vec<u8> {
    let image = image::RgbaImage::from_pixel(width, height, image::Rgba([17, 42, 91, 255]));
    let mut encoded = Cursor::new(Vec::new());
    DynamicImage::ImageRgba8(image)
        .write_to(&mut encoded, ImageFormat::Png)
        .expect("encode PNG fixture");
    encoded.into_inner()
}

fn jpeg_fixture() -> Vec<u8> {
    base64::engine::general_purpose::STANDARD
        .decode("/9j/4AAQSkZJRgABAgAAAQABAAD//gAQTGF2YzYyLjI4LjEwMgD/2wBDAAgoKC8oLzc3Nzc3N0E8QUNDQ0FBQUFDQ0NISEhVVVVISEhDQ0hIUFBVVVxfXFdXVVdfX2RkZHh4c3OMjJGsrM//xABMAAEBAAAAAAAAAAAAAAAAAAAABwEBAQAAAAAAAAAAAAAAAAAABQcQAQAAAAAAAAAAAAAAAAAAAAARAQAAAAAAAAAAAAAAAAAAAAD/wAARCAAIABADASIAAhEAAxEA/9oADAMBAAIRAxEAPwCOAL+Kf//Z")
        .expect("valid JPEG fixture")
}

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

/// MUTATION CHECK: route generic `Cas::put_batch` through the provider-view
/// ordering fence. Expected runtime failure: Full disappears or Barrier is
/// observed, weakening checkpoint preimage persistence at return.
#[test]
fn generic_put_batch_retains_one_trailing_full_fence() {
    let root = tempfile::tempdir().expect("CAS root");
    let cas = FileCas::open(root.path()).expect("open CAS");
    let observations = Arc::new(Mutex::new(Vec::new()));
    let observed = Arc::clone(&observations);

    let artifacts = with_cas_sync_test_hook(
        move |_path, policy, target| {
            observed
                .lock()
                .expect("CAS sync observation lock")
                .push((policy, target));
        },
        || Cas::put_batch(&cas, &[b"checkpoint-a".to_vec(), b"checkpoint-b".to_vec()]),
    )
    .expect("publish generic CAS batch");

    assert_eq!(artifacts.len(), 2);
    let observations = observations.lock().expect("CAS sync observations");
    assert_eq!(
        observations
            .iter()
            .filter(|(policy, _)| *policy == haider_platform::SyncPolicy::Full)
            .count(),
        1,
        "generic CAS batches retain one full device-cache flush"
    );
    assert_eq!(
        observations
            .iter()
            .filter(|(policy, _)| *policy == haider_platform::SyncPolicy::Barrier)
            .count(),
        0,
        "provider-view ordering policy must not leak into generic CAS batches"
    );
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

#[test]
fn put_image_downscales_before_publishing_and_metadata_matches_cas() {
    let root = tempfile::tempdir().expect("CAS root");
    let cas = FileCas::open(root.path()).expect("open CAS");
    let source = png(4_096, 1_024);

    let image = cas
        .put_image(source.clone(), "image/png")
        .expect("bound and publish image");
    let published = cas.get(&image.artifact).expect("read bounded image");
    let dimensions = ImageReader::with_format(Cursor::new(&published), ImageFormat::Png)
        .into_dimensions()
        .expect("bounded PNG dimensions");

    assert_eq!((image.width, image.height), dimensions);
    assert_eq!((image.width, image.height), (2_048, 512));
    assert_eq!(image.byte_len, published.len() as u64);
    assert!(image.byte_len <= TOOL_RESULT_IMAGE_MAX_BYTES);
    assert_ne!(
        published, source,
        "oversized source must not enter CAS unchanged"
    );
    assert!(cas.verify(&image.artifact));
}

#[test]
fn already_bounded_image_reuses_the_owned_source_buffer() {
    let root = tempfile::tempdir().expect("CAS root");
    let cas = FileCas::open(root.path()).expect("open CAS");
    let source = png(16, 8);
    let source_pointer = source.as_ptr();
    let expected = source.clone();

    let (bounded, width, height) = cas.bound_image(source, "image/png").expect("bounded image");

    assert_eq!(bounded.as_ptr(), source_pointer);
    assert_eq!(bounded, expected);
    assert_eq!((width, height), (16, 8));
}

#[test]
fn put_image_rejects_oversized_source_without_publishing_it() {
    let root = tempfile::tempdir().expect("CAS root");
    let cas = FileCas::open(root.path()).expect("open CAS");
    let oversized = vec![0_u8; TOOL_RESULT_IMAGE_MAX_SOURCE_BYTES + 1];

    let error = cas
        .put_image(oversized.clone(), "image/png")
        .expect_err("oversized source must be rejected before decode");

    assert_eq!(error.code, ErrorCode::InvalidArgument);
    assert!(error.message.contains("source limit"));
    let source_ref = artifact_for(&oversized);
    assert!(!cas.verify(&source_ref));
}

#[test]
fn put_image_rejects_truncated_or_mismatched_encodings_before_publication() {
    let root = tempfile::tempdir().expect("CAS root");
    let cas = FileCas::open(root.path()).expect("open CAS");
    let complete_png = png(16, 8);
    let truncated_png = &complete_png[..complete_png.len() / 2];
    let sof_only_jpeg = [
        0xff, 0xd8, 0xff, 0xc0, 0x00, 0x07, 0x08, 0x00, 0x01, 0x00, 0x01,
    ];
    let framed_fake_jpeg = [
        0xff, 0xd8, 0xff, 0xc0, 0x00, 0x07, 0x08, 0x00, 0x01, 0x00, 0x01, 0xff, 0xda, 0x00, 0x02,
        0xff, 0xd9,
    ];
    let mut invalid_table_jpeg = vec![0xff, 0xd8, 0xff, 0xdb, 0x00, 0x43, 0x00];
    invalid_table_jpeg.extend(std::iter::repeat_n(0_u8, 64));
    invalid_table_jpeg.extend_from_slice(&[0xff, 0xc4, 0x00, 0x13, 0x00]);
    invalid_table_jpeg.extend(std::iter::repeat_n(0_u8, 16));
    invalid_table_jpeg.extend_from_slice(&[
        0xff, 0xc0, 0x00, 0x0b, 0x08, 0x00, 0x01, 0x00, 0x01, 0x01, 0x01, 0x11, 0x00, 0xff, 0xda,
        0x00, 0x08, 0x01, 0x01, 0x00, 0x00, 0x3f, 0x00, 0x00, 0xff, 0xd9,
    ]);
    let valid_jpeg = jpeg_fixture();
    let sof = valid_jpeg
        .windows(2)
        .position(|window| window == [0xff, 0xc0])
        .expect("fixture SOF0");
    let sos = valid_jpeg
        .windows(2)
        .position(|window| window == [0xff, 0xda])
        .expect("fixture SOS");
    let sos_length = usize::from(u16::from_be_bytes([
        valid_jpeg[sos + 2],
        valid_jpeg[sos + 3],
    ]));
    let mut invalid_progressive_jpeg = valid_jpeg.clone();
    invalid_progressive_jpeg[sof + 1] = 0xc2;
    invalid_progressive_jpeg[sos + sos_length + 1] = 0xff;
    let dqt = valid_jpeg
        .windows(2)
        .position(|window| window == [0xff, 0xdb])
        .expect("fixture DQT");
    let dqt_length = usize::from(u16::from_be_bytes([
        valid_jpeg[dqt + 2],
        valid_jpeg[dqt + 3],
    ]));
    let quantizers = &valid_jpeg[dqt + 5..dqt + 2 + dqt_length];
    assert_eq!(quantizers.len(), 64);
    let mut wide_dqt = vec![0xff, 0xdb, 0x00, 0x83, 0x10];
    for quantizer in quantizers {
        wide_dqt.extend_from_slice(&[0x00, *quantizer]);
    }
    let mut invalid_wide_dqt_jpeg = valid_jpeg.clone();
    invalid_wide_dqt_jpeg.splice(dqt..dqt + 2 + dqt_length, wide_dqt);
    let dht = valid_jpeg
        .windows(2)
        .position(|window| window == [0xff, 0xc4])
        .expect("fixture DHT");
    let mut invalid_huffman_id_jpeg = valid_jpeg.clone();
    invalid_huffman_id_jpeg[dht + 4] = (invalid_huffman_id_jpeg[dht + 4] & 0xf0) | 0x02;
    let mut invalid_sampling_jpeg = valid_jpeg.clone();
    invalid_sampling_jpeg[sof + 11] = 0x51;
    let mut restart_without_dri_jpeg = valid_jpeg.clone();
    restart_without_dri_jpeg.splice(sos + 2 + sos_length..sos + 2 + sos_length, [0xff, 0xd0]);
    let mut repeated_scan_jpeg = valid_jpeg.clone();
    let scan = repeated_scan_jpeg[sos..repeated_scan_jpeg.len() - 2].to_vec();
    repeated_scan_jpeg.splice(
        repeated_scan_jpeg.len() - 2..repeated_scan_jpeg.len() - 2,
        scan,
    );
    let png_without_iend_crc = &complete_png[..complete_png.len() - 4];
    let mut png_with_trailing_payload = complete_png.clone();
    png_with_trailing_payload.extend_from_slice(b"junk");

    for (bytes, media_type) in [
        (truncated_png, "image/png"),
        (png_without_iend_crc, "image/png"),
        (png_with_trailing_payload.as_slice(), "image/png"),
        (sof_only_jpeg.as_slice(), "image/jpeg"),
        (framed_fake_jpeg.as_slice(), "image/jpeg"),
        (invalid_table_jpeg.as_slice(), "image/jpeg"),
        (invalid_progressive_jpeg.as_slice(), "image/jpeg"),
        (invalid_wide_dqt_jpeg.as_slice(), "image/jpeg"),
        (invalid_huffman_id_jpeg.as_slice(), "image/jpeg"),
        (invalid_sampling_jpeg.as_slice(), "image/jpeg"),
        (restart_without_dri_jpeg.as_slice(), "image/jpeg"),
        (repeated_scan_jpeg.as_slice(), "image/jpeg"),
        (complete_png.as_slice(), "image/jpeg"),
    ] {
        let error = cas
            .put_image(bytes.to_vec(), media_type)
            .expect_err("invalid encoded image must be rejected");
        assert_eq!(error.code, ErrorCode::InvalidArgument);
        assert!(!cas.verify(&artifact_for(bytes)));
    }
}

#[test]
fn put_image_accepts_complete_bounded_jpeg_and_rejects_its_truncation() {
    let root = tempfile::tempdir().expect("CAS root");
    let cas = FileCas::open(root.path()).expect("open CAS");
    let jpeg = jpeg_fixture();

    let image = cas
        .put_image(jpeg.clone(), "image/jpeg")
        .expect("complete bounded JPEG");
    assert_eq!((image.width, image.height), (16, 8));
    assert_eq!(cas.get(&image.artifact).expect("stored JPEG"), jpeg);

    let truncated = &jpeg[..jpeg.len() - 2];
    assert!(cas.put_image(truncated.to_vec(), "image/jpeg").is_err());
    assert!(!cas.verify(&artifact_for(truncated)));
}

#[test]
fn put_image_rejects_oversized_jpeg_and_source_pixel_bomb_without_publication() {
    let root = tempfile::tempdir().expect("CAS root");
    let cas = FileCas::open(root.path()).expect("open CAS");

    let jpeg = jpeg_fixture();
    let mut oversized_jpeg = jpeg[..jpeg.len() - 2].to_vec();
    while oversized_jpeg.len() <= TOOL_RESULT_IMAGE_MAX_BYTES as usize {
        oversized_jpeg.extend_from_slice(&[0xff, 0xfe, 0xff, 0xff]);
        oversized_jpeg.extend(std::iter::repeat_n(0_u8, usize::from(u16::MAX) - 2));
    }
    oversized_jpeg.extend_from_slice(&[0xff, 0xd9]);
    let error = cas
        .put_image(oversized_jpeg.clone(), "image/jpeg")
        .expect_err("oversized JPEG must fail closed without a decoder");
    assert_eq!(error.code, ErrorCode::InvalidArgument);
    assert!(error.message.contains("oversized JPEG"));
    assert!(!cas.verify(&artifact_for(&oversized_jpeg)));

    let mut pixel_bomb = png(1, 1);
    pixel_bomb[16..20].copy_from_slice(&40_000_001_u32.to_be_bytes());
    let mut hasher = crc32fast::Hasher::new();
    hasher.update(&pixel_bomb[12..29]);
    pixel_bomb[29..33].copy_from_slice(&hasher.finalize().to_be_bytes());
    let error = cas
        .put_image(pixel_bomb.clone(), "image/png")
        .expect_err("source pixel ceiling must precede full decode");
    assert_eq!(error.code, ErrorCode::InvalidArgument);
    assert!(error.message.contains("safe decode limit"));
    assert!(!cas.verify(&artifact_for(&pixel_bomb)));
}

#[test]
fn bounded_ref_validation_rejects_a_generic_artifact_and_dishonest_dimensions() {
    let root = tempfile::tempdir().expect("CAS root");
    let cas = FileCas::open(root.path()).expect("open CAS");
    let generic = b"this is not an image";
    let generic_ref = cas.put(generic).expect("generic CAS object");
    let forged = ImageBlockRef {
        artifact: generic_ref,
        media_type: "image/png".into(),
        width: 1,
        height: 1,
        byte_len: generic.len() as u64,
    };
    assert!(validate_image_block(generic, &forged).is_err());

    let encoded = png(16, 8);
    let wrong_dimensions = ImageBlockRef {
        artifact: artifact_for(&encoded),
        media_type: "image/png".into(),
        width: 8,
        height: 16,
        byte_len: encoded.len() as u64,
    };
    assert!(validate_image_block(&encoded, &wrong_dimensions).is_err());

    let wrong_address = ImageBlockRef {
        artifact: ArtifactRef::new(format!("blake3:{}", "0".repeat(64))),
        media_type: "image/png".into(),
        width: 16,
        height: 8,
        byte_len: encoded.len() as u64,
    };
    assert!(validate_image_block(&encoded, &wrong_address).is_err());
}
