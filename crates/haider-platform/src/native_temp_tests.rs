#![allow(clippy::unwrap_used)]
use super::*;

#[test]
fn concurrent_first_temp_initialization_is_idempotent_but_path_is_immutable() {
    let root = tempfile::tempdir().unwrap();
    let path = root.path().canonicalize().unwrap();
    let directory = ImmutableTempDirectory::default();
    let before_publish = std::sync::Barrier::new(2);
    std::thread::scope(|scope| {
        let first = scope.spawn(|| {
            directory.initialize_with(&path, || {
                before_publish.wait();
            })
        });
        let second = scope.spawn(|| {
            directory.initialize_with(&path, || {
                before_publish.wait();
            })
        });
        first.join().unwrap().unwrap();
        second.join().unwrap().unwrap();
    });
    directory.initialize(&path).unwrap();
    let different = tempfile::tempdir().unwrap();
    assert!(
        directory
            .initialize(&different.path().canonicalize().unwrap())
            .is_err()
    );
    assert_eq!(directory.0.get(), Some(&path));
}
