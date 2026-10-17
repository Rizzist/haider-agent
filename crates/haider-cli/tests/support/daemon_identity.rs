//! Exit identity survives normal daemon runtime cleanup.

use std::path::Path;

/// The RPC-only fixtures never open the event store in the client. Its
/// store_open trace therefore selects the daemon's own PID header. The caller
/// decides whether publication must already be complete or may still flush.
pub fn published_exit_pid(directory: &Path) -> Option<u32> {
    let mut pids = Vec::new();
    for entry in std::fs::read_dir(directory).expect("read fixture exit traces") {
        let path = entry.expect("exit trace entry").path();
        let contents = std::fs::read_to_string(path).expect("read fixture exit trace");
        // create_new precedes the exit guard's buffered writes. Empty/partial
        // JSON is not a published identity; retrying callers can observe more.
        let records = contents
            .lines()
            .filter_map(|line| serde_json::from_str::<serde_json::Value>(line).ok())
            .collect::<Vec<_>>();
        if records.iter().any(|record| record["phase"] == "store_open") {
            assert_eq!(records[0]["schema"], 4, "daemon exit trace schema");
            let pid = records[0]["pid"]
                .as_u64()
                .and_then(|pid| u32::try_from(pid).ok())
                .filter(|pid| *pid != 0)
                .expect("daemon exit trace header PID");
            pids.push(pid);
        }
    }
    assert!(pids.len() <= 1, "one daemon must open the fixture store");
    pids.first().copied()
}
