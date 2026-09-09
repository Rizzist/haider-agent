#![allow(clippy::expect_used)]
use haider_tools::{parse_session_transcript, session_transcript_manifest};
use serde_json::json;

#[test]
fn session_transcript_arguments_are_strict_and_bounded() {
    let default = parse_session_transcript(json!({"session_id":"previous"})).expect("defaults");
    assert_eq!(default.after_seq, 0);
    assert_eq!(default.limit, 100);
    assert!(session_transcript_manifest().effects.is_empty());
    for args in [
        json!({}),
        json!({"session_id":""}),
        json!({"session_id":"previous","profile":"other"}),
        json!({"session_id":"previous","limit":0}),
        json!({"session_id":"previous","limit":1025}),
        json!({"session_id":"previous","after_seq":-1}),
        json!({"session_id":"previous","after_seq":u64::MAX}),
        json!({"session_id":"previous","limit":1.5}),
        json!({"session_id":"x\n"}),
    ] {
        assert!(parse_session_transcript(args.clone()).is_err(), "{args}");
    }
    assert!(
        parse_session_transcript(json!({"session_id":"previous","after_seq":u64::MAX-1,"limit":1}))
            .is_ok()
    );
}
