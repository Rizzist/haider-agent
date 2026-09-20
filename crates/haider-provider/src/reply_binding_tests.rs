#![allow(clippy::expect_used)]

use super::{
    PreparedReplyBinding, PreparedReplyBindings, ReplyTokenMatcher, reply_token_locations,
    write_json_value_with_replies,
};
use haider_protocol::reply::ReplyText;

fn binding(marker: &str, text: &str) -> PreparedReplyBinding {
    PreparedReplyBinding {
        marker: marker.into(),
        text: ReplyText::from(text),
    }
}

fn legacy_locations(
    encoded: &[u8],
    bindings: &[PreparedReplyBinding],
) -> Option<Vec<(usize, usize)>> {
    let mut locations = Vec::new();
    for binding in bindings {
        let token = serde_json::to_vec(&binding.marker).ok()?;
        let mut matches = encoded
            .windows(token.len())
            .enumerate()
            .filter_map(|(offset, window)| (window == token).then_some(offset));
        let Some(offset) = matches.next() else {
            continue;
        };
        if matches.next().is_some() {
            return None;
        }
        locations.push((offset, token.len()));
    }
    locations.sort_unstable();
    let mut end = 0;
    for (offset, len) in &locations {
        if *offset < end {
            return None;
        }
        end = offset.checked_add(*len)?;
    }
    Some(locations)
}

fn indexed_locations(
    encoded: &[u8],
    bindings: &PreparedReplyBindings,
) -> Option<Vec<(usize, usize)>> {
    reply_token_locations(encoded, bindings).map(|locations| {
        locations
            .into_iter()
            .map(|(offset, len, _)| (offset, len))
            .collect()
    })
}

#[test]
fn linear_index_matches_legacy_bytes_across_adversarial_templates() {
    let fixtures = [
        (
            vec![binding("first", "one"), binding("second", "two")],
            br#"{"a":"first","b":"second"}"#.as_slice(),
        ),
        (
            vec![binding("quote\"slash\\", "escaped")],
            br#"{"a":"quote\"slash\\","b":"quote\\\"slash\\\\"}"#.as_slice(),
        ),
        (
            vec![binding("π🙂", "unicode")],
            "{\"before\":\"é\",\"target\":\"π🙂\",\"after\":\"界\"}".as_bytes(),
        ),
        (
            vec![binding("missing", "unused"), binding("present", "used")],
            br#"{"value":"present"}"#.as_slice(),
        ),
        (
            vec![binding("duplicate", "refused")],
            br#"{"a":"duplicate","b":"duplicate"}"#.as_slice(),
        ),
        (
            vec![binding("same", "one"), binding("same", "two")],
            br#"{"a":"same"}"#.as_slice(),
        ),
        (
            vec![binding("present", "malformed")],
            b"\xff{not-json:\"present\",tail:\xf0\x9f".as_slice(),
        ),
    ];

    for (items, encoded) in fixtures {
        let expected = legacy_locations(encoded, &items);
        let bindings = PreparedReplyBindings::try_new(items).expect("binding index");
        assert_eq!(indexed_locations(encoded, &bindings), expected);
    }
}

#[test]
fn legitimate_duplicate_marker_content_is_still_refused() {
    let bindings = PreparedReplyBindings::try_new(vec![binding("arena-marker", "reply")])
        .expect("binding index");
    let value = serde_json::json!({
        "bound": "arena-marker",
        "legitimate_content": "arena-marker",
    });
    let encoded = serde_json::to_vec(&value).expect("template bytes");

    assert!(reply_token_locations(&encoded, &bindings).is_none());
    assert!(write_json_value_with_replies(&mut Vec::new(), &value, &bindings).is_err());
}

#[test]
fn marker_like_substrings_do_not_create_false_ambiguity() {
    let bindings = PreparedReplyBindings::try_new(vec![binding("arena-marker", "reply")])
        .expect("binding index");
    let value = serde_json::json!({
        "bound": "arena-marker",
        "legitimate_content": "prefix arena-marker suffix",
    });
    let expected = serde_json::to_vec(&serde_json::json!({
        "bound": "reply",
        "legitimate_content": "prefix arena-marker suffix",
    }))
    .expect("expected bytes");
    let mut actual = Vec::new();

    write_json_value_with_replies(&mut actual, &value, &bindings).expect("unambiguous wire");
    assert_eq!(actual, expected);
}

#[test]
fn matcher_carries_markers_across_every_chunk_and_utf8_boundary() {
    let tokens = vec!["π🙂".as_bytes().to_vec(), b"aba".to_vec(), b"bac".to_vec()];
    let matcher = ReplyTokenMatcher::new(&tokens).expect("matcher");
    let encoded = "éπ🙂界-abac".as_bytes();
    let byte_chunks = encoded.chunks(1).collect::<Vec<_>>();

    assert_eq!(
        matcher.unique_locations(byte_chunks),
        Some(vec![
            Some("é".len()),
            Some("éπ🙂界-".len()),
            Some("éπ🙂界-a".len()),
        ])
    );
}

#[test]
fn overlapping_and_duplicate_patterns_keep_refusal_semantics() {
    let duplicate = PreparedReplyBindings::try_new(vec![
        binding("same-marker", "one"),
        binding("same-marker", "two"),
    ])
    .expect("binding index");
    let encoded = br#"{"value":"same-marker"}"#;
    assert!(reply_token_locations(encoded, &duplicate).is_none());

    let matcher =
        ReplyTokenMatcher::new(&[b"aba".to_vec(), b"bac".to_vec()]).expect("overlap matcher");
    let offsets = matcher
        .unique_locations([b"ab".as_slice(), b"ac".as_slice()])
        .expect("each pattern occurs once");
    assert_eq!(offsets, vec![Some(0), Some(1)]);
    let mut spans = offsets
        .into_iter()
        .enumerate()
        .map(|(index, offset)| (offset.expect("offset"), matcher.token_lengths[index]))
        .collect::<Vec<_>>();
    spans.sort_unstable();
    assert!(spans[1].0 < spans[0].0 + spans[0].1);
}
