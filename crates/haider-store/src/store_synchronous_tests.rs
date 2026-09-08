#![allow(clippy::unwrap_used)]
use super::*;

#[test]
fn embedded_store_synchronous_is_typed_on_every_open() {
    let root = tempfile::tempdir().unwrap();
    for (mode, expected) in [(StoreSynchronous::Normal, 1), (StoreSynchronous::Full, 2)] {
        let lease = Store::acquire_profile(root.path()).unwrap();
        let store = Store::open_locked_with_synchronous(lease, mode).unwrap();
        let connection = store.connection().unwrap();
        let synchronous: u32 = connection
            .pragma_query_value(None, "synchronous", |r| r.get(0))
            .unwrap();
        let journal: String = connection
            .pragma_query_value(None, "journal_mode", |r| r.get(0))
            .unwrap();
        let integrity: String = connection
            .pragma_query_value(None, "integrity_check", |r| r.get(0))
            .unwrap();
        assert_eq!(synchronous, expected);
        assert_eq!(journal, "wal");
        assert_eq!(integrity, "ok");
    }
}
