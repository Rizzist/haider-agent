use super::*;
#[cfg(target_vendor = "apple")]
use rustix::mm::{MapFlags, ProtFlags, mmap, munmap};
#[cfg(target_vendor = "apple")]
use std::ffi::c_void;
#[cfg(target_vendor = "apple")]
use std::os::unix::fs::MetadataExt;
#[cfg(target_vendor = "apple")]
use std::ptr;

/// MUTATION CHECK: bypass `try_clone_file_at` by passing a clone source that
/// returns `None`. Expected failure: the metadata-guarded fallback returns the
/// torn `a…a/c…c` mix, so the coherent-basis assertion below fails. Verified by
/// executed revert in W4a1.3.
#[cfg(target_vendor = "apple")]
#[test]
fn apfs_clone_snapshot_never_false_passes_a_map_shared_torn_read() {
    const HALF_BYTES: usize = 64 * 1024;
    const FILE_BYTES: usize = HALF_BYTES * 2;

    let directory = tempfile::tempdir().expect("temporary directory");
    let target = directory.path().join("target.txt");
    let prefix_before = vec![b'a'; HALF_BYTES];
    let suffix_before = vec![b'b'; HALF_BYTES];
    let initial = [prefix_before.as_slice(), suffix_before.as_slice()].concat();
    let prefix_after = vec![b'd'; HALF_BYTES];
    let suffix_after = vec![b'c'; HALF_BYTES];
    let final_bytes = [prefix_after.as_slice(), suffix_after.as_slice()].concat();
    let torn_mix = [prefix_before.as_slice(), suffix_after.as_slice()].concat();
    fs::write(&target, &initial).expect("seed target");
    let parent = rustix::fs::open(
        directory.path(),
        OFlags::RDONLY | OFlags::DIRECTORY | OFlags::CLOEXEC,
        Mode::empty(),
    )
    .expect("open parent");
    let mut source = fs::File::open(&target).expect("open target");
    let writer = fs::OpenOptions::new()
        .read(true)
        .write(true)
        .open(&target)
        .expect("open mmap writer");
    // SAFETY: `mapping` is page-aligned storage returned for this file and
    // length. This test creates no Rust references into it, mutates it only on
    // this thread, and unmaps exactly the original range after the read.
    let mapping = unsafe {
        mmap(
            ptr::null_mut(),
            FILE_BYTES,
            ProtFlags::READ | ProtFlags::WRITE,
            MapFlags::SHARED,
            &writer,
            0,
        )
        .expect("map target shared")
    };
    let mut edited = false;
    let mut metadata_before_edit = None;
    let mut metadata_after_edit = None;

    let snapshot = file_snapshot_with_reader(
        &parent,
        &mut source,
        &target,
        try_clone_file_at,
        |read_file, buffer| {
            let prefix_read = read_file.read_at(&mut buffer[..HALF_BYTES], 0)?;
            if !edited {
                metadata_before_edit = Some(fs::metadata(&target).expect("metadata before mmap"));
                // SAFETY: the mapping covers FILE_BYTES writable bytes, and
                // the two non-overlapping writes stay within that allocation.
                unsafe {
                    mapping.cast::<u8>().write_bytes(b'd', HALF_BYTES);
                    mapping
                        .cast::<u8>()
                        .add(HALF_BYTES)
                        .write_bytes(b'c', HALF_BYTES);
                }
                metadata_after_edit = Some(fs::metadata(&target).expect("metadata after mmap"));
                edited = true;
            }
            let suffix_read = read_file.read_at(
                &mut buffer[prefix_read..],
                u64::try_from(prefix_read).expect("prefix offset"),
            )?;
            Ok(prefix_read + suffix_read)
        },
    )
    .expect("take clone-backed snapshot");

    // SAFETY: no reference into `mapping` exists, and this is the exact pointer
    // and length returned by `mmap` above.
    unsafe { munmap(mapping.cast::<c_void>(), FILE_BYTES).expect("unmap target") };
    drop(writer);

    let initial_metadata = metadata_before_edit.expect("captured metadata before mmap");
    let final_metadata = metadata_after_edit.expect("captured metadata after mmap");
    assert_eq!(
        initial_metadata.ino(),
        final_metadata.ino(),
        "reproduction must preserve the inode"
    );
    assert_eq!(
        initial_metadata.len(),
        final_metadata.len(),
        "reproduction must preserve the size"
    );
    assert_eq!(
        initial_metadata.mtime(),
        final_metadata.mtime(),
        "MAP_SHARED reproduction must evade whole-second mtime"
    );
    assert_eq!(
        initial_metadata.mtime_nsec(),
        final_metadata.mtime_nsec(),
        "MAP_SHARED reproduction must evade nanosecond mtime"
    );
    assert_eq!(
        initial_metadata.ctime(),
        final_metadata.ctime(),
        "MAP_SHARED reproduction must evade whole-second ctime"
    );
    assert_eq!(
        initial_metadata.ctime_nsec(),
        final_metadata.ctime_nsec(),
        "MAP_SHARED reproduction must evade nanosecond ctime"
    );
    assert_ne!(
        blake3::hash(&snapshot.bytes),
        blake3::hash(&torn_mix),
        "the clone-backed snapshot must never accept the torn read"
    );
    assert_eq!(snapshot.basis, SnapshotBasis::CowClone);
    assert!(
        snapshot.bytes == initial || snapshot.bytes == final_bytes,
        "the clone-backed read must equal one coherent file state"
    );
    assert_eq!(fs::read(&target).expect("read final target"), final_bytes);
}

#[test]
fn clone_unavailable_degrades_to_metadata_guarded_best_effort() {
    let directory = tempfile::tempdir().expect("temporary directory");
    let target = directory.path().join("target.txt");
    fs::write(&target, "before").expect("seed target");
    let parent = rustix::fs::open(
        directory.path(),
        OFlags::RDONLY | OFlags::DIRECTORY | OFlags::CLOEXEC,
        Mode::empty(),
    )
    .expect("open parent");
    let mut source = fs::File::open(&target).expect("open target");

    let snapshot = file_snapshot_with_reader(
        &parent,
        &mut source,
        &target,
        |_parent, _source| None,
        |read_file, buffer| read_file.read_at(buffer, 0),
    )
    .expect("fall back without crashing");

    assert_eq!(snapshot.basis, SnapshotBasis::MetadataGuardedFallback);
    assert_eq!(snapshot.bytes, b"before");
}
