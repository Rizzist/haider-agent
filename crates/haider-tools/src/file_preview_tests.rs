#![allow(clippy::expect_used)]
use super::*;
use async_trait::async_trait;
use haider_protocol::ids::ArtifactRef;

#[derive(Default)]
struct Capture(Vec<Vec<u8>>);
#[async_trait]
impl CasSink for Capture {
    async fn put(&mut self, bytes: &[u8]) -> ToolResult<ArtifactRef> {
        self.0.push(bytes.to_vec());
        Ok(ArtifactRef::new(blake3::hash(bytes).to_hex().to_string()))
    }
    async fn put_file(&mut self, path: &std::path::Path) -> ToolResult<ArtifactRef> {
        self.put(&std::fs::read(path).expect("read artifact")).await
    }
}

#[tokio::test]
async fn default_page_has_2000_numbered_lines_and_exact_continuation() {
    let text: String = (1..=2001).map(|i| format!("line {i}\n")).collect();
    let mut cas = Capture::default();
    let first = bounded_file_read(
        text.clone(),
        &FsRead::new("handoff.md"),
        ResultBounds::file_read(),
        &mut cas,
    )
    .await
    .expect("first");
    assert!(first.preview.starts_with("1: line 1\n"));
    assert!(first.preview.contains("2000: line 2000\n"));
    assert!(!first.preview.contains("2001: line 2001"));
    assert!(first.preview.contains("\"offset\":2001"));
    assert!(!first.preview.contains("haider_elision_v1"));
    assert_eq!(cas.0, [text.as_bytes()]);
    let last = bounded_file_read(
        text,
        &FsRead::new("handoff.md").with_line_range(Some(2001), Some(2000)),
        ResultBounds::file_read(),
        &mut cas,
    )
    .await
    .expect("last");
    assert_eq!(last.preview, "2001: line 2001\n");
    assert!(!last.truncated);
}

#[tokio::test]
async fn byte_pages_continue_a_long_utf8_line_without_losing_characters() {
    let text = "é".repeat(crate::ORCHESTRATION_PREVIEW_MAX_BYTES);
    let mut cas = Capture::default();
    let mut column = 1;
    let mut reconstructed = String::new();
    loop {
        let page = bounded_file_read(
            text.clone(),
            &FsRead::new("long.txt").with_column(Some(column)),
            ResultBounds::file_read(),
            &mut cas,
        )
        .await
        .expect("page");
        let line = page
            .preview
            .lines()
            .next()
            .expect("line")
            .strip_prefix("1: ")
            .expect("number");
        reconstructed.push_str(line);
        column += line.chars().count();
        if !page.truncated {
            break;
        }
        assert!(page.preview.contains(&format!("\"column\":{column}")));
        assert!(line.len() <= crate::ORCHESTRATION_PREVIEW_MAX_BYTES);
    }
    assert_eq!(reconstructed, text);
}

#[tokio::test]
async fn range_inside_pem_and_secret_inside_long_line_remain_redacted() {
    let mut cas = Capture::default();
    let input = "-----BEGIN\x20PRIVATE KEY-----\nAA==\n-----END PRIVATE KEY-----\n";
    let result = bounded_file_read(
        input.into(),
        &FsRead::new("handoff.md").with_line_range(Some(2), Some(1)),
        ResultBounds::file_read(),
        &mut cas,
    )
    .await
    .expect("PEM range");
    assert!(result.preview.starts_with("2: [REDACTED:private_key]\n"));
    assert!(!result.preview.contains("AA=="));
    let input = format!("{}sk-abcdefghijklmnopQRSTUV tail", " ".repeat(65_530));
    let first = bounded_file_read(
        input.clone(),
        &FsRead::new("long.txt"),
        ResultBounds::file_read(),
        &mut cas,
    )
    .await
    .expect("first");
    let next = bounded_file_read(
        input,
        &FsRead::new("long.txt").with_column(Some(65_534)),
        ResultBounds::file_read(),
        &mut cas,
    )
    .await
    .expect("next");
    assert!(!first.preview.contains("sk-"));
    assert!(!next.preview.contains("abcdefghijklmnop"));
}

#[tokio::test]
async fn byte_continuation_preserves_the_end_of_an_explicit_line_range() {
    let text = "first\nsecond\nthird\nfourth\n";
    let mut cas = Capture::default();
    let page = bounded_file_read(
        text.into(),
        &FsRead::new("range.txt").with_line_range(Some(1), Some(3)),
        ResultBounds {
            max_preview_bytes: 15,
        },
        &mut cas,
    )
    .await
    .expect("page");
    assert!(page.preview.starts_with("1: first\n2: sec"));
    assert!(page.preview.contains("\"limit\":2"));
    assert!(page.preview.contains("\"offset\":2"));
    assert!(page.preview.contains("\"column\":4"));
    let rest = bounded_file_read(
        text.into(),
        &FsRead::new("range.txt")
            .with_line_range(Some(2), Some(2))
            .with_column(Some(4)),
        ResultBounds::file_read(),
        &mut cas,
    )
    .await
    .expect("rest");
    assert_eq!(rest.preview, "2: ond\n3: third\n");
    assert!(!rest.truncated);
}

#[tokio::test]
async fn redaction_outside_the_requested_range_does_not_mark_it_incomplete() {
    let mut cas = Capture::default();
    let result = bounded_file_read(
        "plain\nsk-abcdefghijklmnopQRSTUV\n".into(),
        &FsRead::new("range.txt").with_line_range(Some(1), Some(1)),
        ResultBounds::file_read(),
        &mut cas,
    )
    .await
    .expect("complete range");
    assert_eq!(result.preview, "1: plain\n");
    assert!(!result.truncated);
    assert!(result.truncation.is_none());
    assert!(result.artifact.is_none());
}

#[tokio::test]
async fn password_redaction_precedes_file_line_and_column_paging() {
    let mut cas = Capture::default();
    let text = concat!(
        "https://owner:fixturepass@example.test/repo\n",
        "postgres://owner:p%40ssw0rd@db.test/app\n",
        "password=\"abc\\\"SYNTHETICTAIL987\"\n",
        "password='abc\\'SYNTHETICTAIL987'\n",
        "password=\"abc\\\\SYNTHETICTAIL987\"\n",
        "password='abc\\\\SYNTHETICTAIL987'\n",
        "thread-01a0e893-52bc-7def-89ab-0123456789cd\n",
    );
    let expected = concat!(
        "https://owner:[REDACTED:secret_value]@example.test/repo\n",
        "postgres://owner:[REDACTED:secret_value]@db.test/app\n",
        "password=[REDACTED:secret_value]\n",
        "password=[REDACTED:secret_value]\n",
        "password=[REDACTED:secret_value]\n",
        "password=[REDACTED:secret_value]\n",
        "thread-01a0e893-52bc-7def-89ab-0123456789cd\n",
    );
    for (line, expected_line) in expected.split_inclusive('\n').enumerate() {
        for column in [1, 14, 20] {
            let page = bounded_file_read(
                text.into(),
                &FsRead::new("mixed.txt")
                    .with_line_range(Some(line + 1), Some(1))
                    .with_column(Some(column)),
                ResultBounds::file_read(),
                &mut cas,
            )
            .await
            .expect("page");
            let expected_content = format!("{}: {}", line + 1, &expected_line[column - 1..]);
            assert!(
                page.preview.starts_with(&expected_content),
                "{}",
                page.preview
            );
            for secret in ["fixturepass", "p%40ssw0rd", "SYNTHETICTAIL987"] {
                assert!(!page.preview.contains(secret));
            }
        }
    }
}
