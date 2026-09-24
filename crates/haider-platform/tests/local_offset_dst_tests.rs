//! 973: per-instant local UTC offsets across daylight-saving changes.
#![allow(clippy::unwrap_used)]

/// The per-instant offset honours a DST change between two instants. The
/// zone is pinned in a CHILD process (`TZ=America/New_York`): mutating the
/// environment of the multi-threaded test process would be unsound. It
/// lives in its own integration binary so the child spawn can never race
/// the unit suite's file-lock fences (a forked child briefly shares open
/// file descriptions).
/// New York 2026: EDT (-4h) until 2026-11-01T06:00Z, EST (-5h) from then.
#[cfg(unix)]
#[test]
fn local_offset_is_resolved_per_instant_across_a_dst_change() {
    const CHILD: &str = "HAIDER_LOCAL_DATE_TEST_TZ_CHILD";
    if std::env::var_os(CHILD).is_some() {
        let before = haider_platform::local_utc_offset_seconds(1_793_512_740).unwrap(); // 05:59Z
        let at = haider_platform::local_utc_offset_seconds(1_793_512_800).unwrap(); // 06:00Z
        let after = haider_platform::local_utc_offset_seconds(1_793_534_400).unwrap(); // 12:00Z
        let spring = haider_platform::local_utc_offset_seconds(1_772_953_140).unwrap(); // 2026-03-08 06:59Z
        let spring_after = haider_platform::local_utc_offset_seconds(1_772_953_200).unwrap(); // 07:00Z
        assert_eq!((before, at, after), (-4 * 3600, -5 * 3600, -5 * 3600));
        assert_eq!((spring, spring_after), (-5 * 3600, -4 * 3600));
        return;
    }
    let status = std::process::Command::new(std::env::current_exe().unwrap())
        .args([
            "--exact",
            "local_offset_is_resolved_per_instant_across_a_dst_change",
            "--test-threads=1",
        ])
        .env(CHILD, "1")
        .env("TZ", "America/New_York")
        .status()
        .unwrap();
    assert!(status.success(), "child DST check failed: {status}");
}
