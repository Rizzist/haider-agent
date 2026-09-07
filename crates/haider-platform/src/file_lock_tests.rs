use super::*;
use std::fs::OpenOptions;

#[test]
fn file_lock_fences_another_opener_until_unlock_or_close() -> io::Result<()> {
    let directory = tempfile::tempdir()?;
    let path = directory.path().join("lock");
    let open = || {
        OpenOptions::new()
            .read(true)
            .write(true)
            .create(true)
            .truncate(false)
            .open(&path)
    };
    let first = open()?;
    let second = open()?;
    lock_file_exclusive(&first)?;
    assert!(matches!(
        try_lock_file_exclusive(&second),
        Err(TryLockError::WouldBlock)
    ));
    unlock_file(&first)?;
    try_lock_file_exclusive(&second).map_err(io::Error::from)?;
    assert!(matches!(
        try_lock_file_exclusive(&first),
        Err(TryLockError::WouldBlock)
    ));
    drop(second);
    try_lock_file_exclusive(&first).map_err(io::Error::from)?;
    unlock_file(&first)
}
