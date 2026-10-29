//! Recovery instructions must remain correct with reconnecting old clients.
#![allow(clippy::expect_used)]
#[test]
fn helper_stop_recovery_closes_old_clients_before_restart_for_both_spawn_modes() {
    use haider_rpc::{HELPER_STOP_UNSUPPORTED_MESSAGE, HELPER_STOP_UNSUPPORTED_NO_SPAWN_MESSAGE};
    for message in [
        HELPER_STOP_UNSUPPORTED_MESSAGE,
        HELPER_STOP_UNSUPPORTED_NO_SPAWN_MESSAGE,
    ] {
        let close = message
            .find("close older clients attached to this profile")
            .expect("close reconnecting clients");
        let stop = message
            .find("`haider daemon stop`")
            .expect("literal stop remedy");
        assert!(close < stop);
        assert!(message.contains("session --watch"));
        assert!(message.contains("TUI"));
        assert!(message.contains("restart the old daemon"));
    }
    assert!(HELPER_STOP_UNSUPPORTED_NO_SPAWN_MESSAGE.contains("without --no-spawn"));
    assert!(HELPER_STOP_UNSUPPORTED_MESSAGE.contains("current daemon starts automatically"));
}
