#![allow(clippy::expect_used)]

use haider_protocol::context::ContextSavingsMeasurement;
use haider_tools::{
    OutputAdapter, REDUCED_TOOL_OUTPUT_MAX_BYTES, elide_text_head_tail, estimated_text_tokens,
    provider_request_text_projection_bytes, reduce_tool_output,
};

fn marker(text: &str) -> serde_json::Value {
    let line = text
        .lines()
        .find(|line| line.contains("\"haider_elision_v1\""))
        .expect("machine-readable elision marker");
    serde_json::from_str(line).expect("marker JSON")
}

/// MUTATION CHECK: drop a reducer-selected semantic field or reintroduce ANSI,
/// progress, pass-line, avatar, boilerplate, or unbounded frame noise.
/// Expected failure: one of the kept-field or removed-field pins changes.
#[test]
fn output_type_adapters_keep_semantics_and_drop_deterministic_noise() {
    let rust = concat!(
        "\u{1b}[31merror[E0308]: mismatched types\u{1b}[0m\n",
        " --> src/lib.rs:7:9\n",
        "  |\n7 | value\n  | ^^^^^ expected u8\n",
        "note: run with RUST_BACKTRACE=1\n",
        "For more information about this error, try rustc --explain E0308.\n",
        "error: could not compile `fixture` (lib) due to 1 previous error\n",
    );
    let reduced = reduce_tool_output("process_exec", rust, true);
    assert_eq!(reduced.adapter, OutputAdapter::RustCompiler);
    assert_eq!(reduced.before_tokens, estimated_text_tokens(rust));
    assert_eq!(
        reduced.saved_tokens_estimate,
        reduced.before_tokens.saturating_sub(reduced.after_tokens)
    );
    assert!(reduced.text.contains("\"haider_elision_v1\""));
    assert!(reduced.text.contains("error[E0308]"));
    assert!(reduced.text.contains("src/lib.rs:7:9"));
    assert!(reduced.text.contains("^^^^^"));
    assert!(!reduced.text.contains("For more information"));
    assert!(!reduced.text.contains("\u{1b}["));

    let plain_rust = concat!(
        "error: expected one of `;` or `}`\n",
        " --> src/plain.rs:4:7\n",
        "  |\n4 | broken\n  |       ^ expected delimiter\n",
    );
    let reduced = reduce_tool_output("process_exec", plain_rust, true);
    assert!(reduced.text.contains("error: expected one of"));
    assert!(reduced.text.contains("src/plain.rs:4:7"));
    assert!(reduced.text.contains("^ expected delimiter"));

    let tests = format!(
        "{}test broken ... FAILED\nfailures:\nthread 'broken' panicked at src/lib.rs:9: assertion failed\ntest result: FAILED. 100 passed; 1 failed\n",
        (0..100)
            .map(|index| format!("test passing_{index} ... ok\n"))
            .collect::<String>()
    );
    let reduced = reduce_tool_output("process_exec", &tests, true);
    assert_eq!(reduced.adapter, OutputAdapter::Test);
    assert_eq!(reduced.before_tokens, estimated_text_tokens(&tests));
    assert!(reduced.after_tokens < reduced.before_tokens);
    assert!(reduced.text.contains("src/lib.rs:9"));
    assert!(reduced.text.contains("test result: FAILED"));
    assert!(!reduced.text.contains("test passing_0 ... ok"));
    assert!(reduced.text.contains(
        "failures:\nthread 'broken' panicked at src/lib.rs:9: assertion failed\ntest result: FAILED. 100 passed; 1 failed\n"
    ));

    let mut long_failure =
        String::from("failures:\nthread 'early' panicked at src/lib.rs:1: boom\n");
    for index in 0..40 {
        long_failure.push_str(&format!("cleanup line {index}\n"));
    }
    long_failure.push_str("test result: FAILED. 0 passed; 1 failed\n");
    let reduced = reduce_tool_output("process_exec", &long_failure, true);
    assert!(reduced.text.contains("panicked at src/lib.rs:1: boom"));
    assert!(reduced.text.contains("cleanup line 39"));
    assert!(reduced.text.contains("test result: FAILED"));

    let github = format!(
        r#"[{{"id":42,"number":7,"title":"Keep me","state":"OPEN","author":{{"login":"octo","avatar_url":"{}"}},"url":"https://example/7","createdAt":"{}","node_id":"drop"}}]"#,
        "drop".repeat(200),
        "drop".repeat(200)
    );
    let reduced = reduce_tool_output("process_exec", &github, false);
    assert_eq!(reduced.adapter, OutputAdapter::GithubJson);
    assert_eq!(reduced.before_tokens, estimated_text_tokens(&github));
    assert!(reduced.after_tokens < reduced.before_tokens);
    assert!(
        reduced
            .text
            .contains("42\t7\tKeep me\tOPEN\tocto\thttps://example/7")
    );
    assert!(!reduced.text.contains("avatar"));
    assert!(!reduced.text.contains("createdAt"));

    let git = concat!(
        "diff --git a/src/lib.rs b/src/lib.rs\n--- a/src/lib.rs\n+++ b/src/lib.rs\n",
        "@@ -1,4 +1,4 @@\n context-a\n context-b\n-old\n+new\n context-c\n context-d\n",
    );
    let reduced = reduce_tool_output("process_exec", git, false);
    assert_eq!(reduced.adapter, OutputAdapter::Git);
    assert_eq!(reduced.before_tokens, estimated_text_tokens(git));
    assert!(reduced.after_tokens <= reduced.before_tokens);
    assert!(reduced.text.contains("@@ -1,4 +1,4 @@"));
    assert!(reduced.text.contains("-old\n+new"));
    let log = format!(
        "commit abcdef\nAuthor: Example <example.invalid>\n{}\n    keep the subject\n",
        "Date: yesterday\n".repeat(100)
    );
    let reduced = reduce_tool_output("process_exec", &log, false);
    assert!(reduced.text.contains("Author: Example"));
    assert!(reduced.text.contains("keep the subject"));
    assert!(!reduced.text.contains("Date:"));

    let packages = format!(
        "{}npm WARN src/index.js:4 deprecated call\nERROR build failed at build.gradle:8\n",
        "Downloading crate-a\nDownloading crate-b\n".repeat(100)
    );
    let reduced = reduce_tool_output("process_exec", &packages, true);
    assert_eq!(reduced.adapter, OutputAdapter::PackageManager);
    assert_eq!(reduced.before_tokens, estimated_text_tokens(&packages));
    assert!(reduced.after_tokens < reduced.before_tokens);
    assert!(reduced.text.contains("src/index.js:4"));
    assert!(reduced.text.contains("build.gradle:8"));
    assert!(!reduced.text.contains("Downloading"));

    let mut stack = String::from("RuntimeError: boom\nCaused by: root\n");
    for index in 0..30 {
        stack.push_str(&format!("  at frame{index} (src/app.js:{index}:1)\n"));
    }
    let reduced = reduce_tool_output("process_exec", &stack, true);
    assert_eq!(reduced.adapter, OutputAdapter::StackTrace);
    assert_eq!(reduced.before_tokens, estimated_text_tokens(&stack));
    assert!(reduced.after_tokens < reduced.before_tokens);
    assert!(reduced.text.contains("Caused by: root"));
    assert!(reduced.text.contains("src/app.js:3:1"));
    assert!(!reduced.text.contains("src/app.js:10:1"));
    assert!(reduced.text.contains("src/app.js:29:1"));

    let listing = (0..60)
        .map(|index| format!("src/file-{index:02}.rs"))
        .chain((0..20).map(|index| format!("node_modules/pkg-{index}/index.js")))
        .collect::<Vec<_>>()
        .join("\n");
    let reduced = reduce_tool_output("process_exec", &listing, false);
    assert_eq!(reduced.adapter, OutputAdapter::Directory);
    assert_eq!(reduced.before_tokens, estimated_text_tokens(&listing));
    assert!(reduced.after_tokens < reduced.before_tokens);
    assert!(reduced.text.contains(".rs: 60"));
    assert!(reduced.text.contains("node_modules/: 20 paths collapsed"));
    assert!(!reduced.text.contains("pkg-19"));
}

#[test]
fn fixture_token_estimates_cover_listing_grep_cargo_and_three_kib_read() {
    let listing = (0..100)
        .map(|index| format!("target/debug/deps/library-{index:03}.rlib"))
        .collect::<Vec<_>>()
        .join("\n");
    let grep = "src/lib.rs:9:needle\n".repeat(80);
    let cargo = concat!(
        "error[E0425]: cannot find value `missing` in this scope\n",
        " --> src/main.rs:3:5\n3 | missing();\n  | ^^^^^^^ not found\n",
        "For more information about this error, try rustc --explain E0425.\n",
        "error: could not compile `fixture` due to 1 previous error\n",
    );
    let file = "plain source line without adapter noise\n".repeat(80);
    let fixtures = [
        ("listing", listing, false),
        ("grep", grep, false),
        ("cargo", cargo.to_owned(), true),
        ("3kb-file", file, false),
    ];
    let mut total_before = 0usize;
    let mut total_after = 0usize;
    for (name, fixture, failed) in fixtures {
        let reduced = reduce_tool_output("process_exec", &fixture, failed);
        assert_eq!(
            reduced.before_tokens,
            estimated_text_tokens(&fixture),
            "{name}"
        );
        assert_eq!(
            reduced.after_tokens,
            estimated_text_tokens(&reduced.text),
            "{name}"
        );
        assert_eq!(
            reduced.saved_tokens_estimate,
            reduced.before_tokens.saturating_sub(reduced.after_tokens),
            "{name}"
        );
        assert_eq!(
            reduced.measurement,
            ContextSavingsMeasurement::ProviderRequestBytesDivFourV1
        );
        assert!(reduced.after_tokens <= reduced.before_tokens, "{name}");
        eprintln!(
            "output-diet fixture={name} before_tokens_estimate={} after_tokens_estimate={} saved_tokens_estimate={}",
            reduced.before_tokens, reduced.after_tokens, reduced.saved_tokens_estimate
        );
        let expected = match name {
            "listing" => (900, 57),
            "grep" => (421, 55),
            "cargo" => (62, 62),
            "3kb-file" => (821, 60),
            _ => unreachable!("fixture names are closed"),
        };
        assert_eq!((reduced.before_tokens, reduced.after_tokens), expected);
        total_before = total_before.saturating_add(reduced.before_tokens);
        total_after = total_after.saturating_add(reduced.after_tokens);
    }
    let total_saved = total_before.saturating_sub(total_after);
    let saved_per_million_input_tokens = total_saved.saturating_mul(1_000_000) / total_before;
    assert_eq!(
        (total_before, total_after, total_saved),
        (2_204, 234, 1_970)
    );
    assert_eq!(saved_per_million_input_tokens, 893_829);
    eprintln!(
        "output-diet cumulative measurement=provider_request_bytes_div_four_v1 before_tokens_estimate={total_before} after_tokens_estimate={total_after} saved_tokens_estimate={total_saved} saved_per_1m_input_tokens_estimate={saved_per_million_input_tokens}"
    );

    let unique = (0..2_000)
        .map(|index| format!("unique successful output line {index}"))
        .collect::<Vec<_>>()
        .join("\n");
    let reduced = reduce_tool_output("process_exec", &unique, false);
    assert!(reduced.text.len() <= REDUCED_TOOL_OUTPUT_MAX_BYTES);
    assert!(reduced.text.contains("\"haider_elision_v1\""));
}

#[test]
fn head_tail_elision_is_deterministic_utf8_safe_and_tail_weighted() {
    let input = format!(
        "command: cargo test\r\n{}\r\nFINAL FAILURE: assertion at src/lib.rs:999\r\n",
        "é boilerplate\r\n".repeat(2_000)
    );
    let first = elide_text_head_tail(&input, 4_096, "fixture").expect("oversized fixture");
    let second = elide_text_head_tail(&input, 4_096, "fixture").expect("same oversized fixture");
    assert_eq!(first, second, "same input must replay byte-identically");
    assert!(first.text.len() <= 4_096);
    assert_eq!(
        first.savings.output_bytes,
        u64::try_from(provider_request_text_projection_bytes(&first.text)).expect("fixture length")
    );
    assert_eq!(
        first.savings.estimated_tokens_after,
        u64::try_from(estimated_text_tokens(&first.text)).expect("fixture estimate")
    );
    assert!(first.text.starts_with("command: cargo test\r\n"));
    assert!(
        first
            .text
            .ends_with("FINAL FAILURE: assertion at src/lib.rs:999\r\n")
    );
    let payload = marker(&first.text);
    let elision = &payload["haider_elision_v1"];
    assert_eq!(elision["scope"], "fixture");
    assert_eq!(elision["omitted_bytes_exact"], true);
    assert!(
        elision["retained_tail_bytes"].as_u64().expect("tail")
            > elision["retained_head_bytes"].as_u64().expect("head")
    );
    assert!(elision.get("tokens_before_estimate").is_none());
    assert!(elision.get("token_estimation_method").is_none());
}
