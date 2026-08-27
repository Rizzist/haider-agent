#![allow(clippy::expect_used)]

use haider_tools::{
    OutputAdapter, REDUCED_TOOL_OUTPUT_MAX_BYTES, estimated_tokens, reduce_tool_output,
};

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
    assert_eq!(reduced.before_tokens, estimated_tokens(rust.len()));
    assert!(reduced.after_tokens < reduced.before_tokens);
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

    let tests = concat!(
        "test one ... ok\ntest two ... ok\ntest broken ... FAILED\n",
        "failures:\nthread 'broken' panicked at src/lib.rs:9: assertion failed\n",
        "test result: FAILED. 2 passed; 1 failed\n",
    );
    let reduced = reduce_tool_output("process_exec", tests, true);
    assert_eq!(reduced.adapter, OutputAdapter::Test);
    assert_eq!(reduced.before_tokens, estimated_tokens(tests.len()));
    assert!(reduced.after_tokens < reduced.before_tokens);
    assert!(reduced.text.contains("src/lib.rs:9"));
    assert!(reduced.text.contains("test result: FAILED"));
    assert!(!reduced.text.contains("test one ... ok"));
    assert!(reduced.text.ends_with(
        "failures:\nthread 'broken' panicked at src/lib.rs:9: assertion failed\ntest result: FAILED. 2 passed; 1 failed\n"
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

    let github = r#"[{"id":42,"number":7,"title":"Keep me","state":"OPEN","author":{"login":"octo","avatar_url":"drop"},"url":"https://example/7","createdAt":"drop","node_id":"drop"}]"#;
    let reduced = reduce_tool_output("process_exec", github, false);
    assert_eq!(reduced.adapter, OutputAdapter::GithubJson);
    assert_eq!(reduced.before_tokens, estimated_tokens(github.len()));
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
    assert_eq!(reduced.before_tokens, estimated_tokens(git.len()));
    assert!(reduced.after_tokens <= reduced.before_tokens);
    assert!(reduced.text.contains("@@ -1,4 +1,4 @@"));
    assert!(reduced.text.contains("-old\n+new"));
    let log = concat!(
        "commit abcdef\nAuthor: Example <example.invalid>\nDate: yesterday\n\n",
        "    keep the subject\n"
    );
    let reduced = reduce_tool_output("process_exec", log, false);
    assert!(reduced.text.contains("Author: Example"));
    assert!(reduced.text.contains("keep the subject"));
    assert!(!reduced.text.contains("Date:"));

    let packages = concat!(
        "Downloading crate-a\nDownloading crate-b\n",
        "npm WARN src/index.js:4 deprecated call\n",
        "ERROR build failed at build.gradle:8\n",
    );
    let reduced = reduce_tool_output("process_exec", packages, true);
    assert_eq!(reduced.adapter, OutputAdapter::PackageManager);
    assert_eq!(reduced.before_tokens, estimated_tokens(packages.len()));
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
    assert_eq!(reduced.before_tokens, estimated_tokens(stack.len()));
    assert!(reduced.after_tokens < reduced.before_tokens);
    assert!(reduced.text.contains("Caused by: root"));
    assert!(reduced.text.contains("src/app.js:11:1"));
    assert!(!reduced.text.contains("src/app.js:29:1"));

    let listing = (0..60)
        .map(|index| format!("src/file-{index:02}.rs"))
        .chain((0..20).map(|index| format!("node_modules/pkg-{index}/index.js")))
        .collect::<Vec<_>>()
        .join("\n");
    let reduced = reduce_tool_output("process_exec", &listing, false);
    assert_eq!(reduced.adapter, OutputAdapter::Directory);
    assert_eq!(reduced.before_tokens, estimated_tokens(listing.len()));
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
    for (name, fixture, failed) in fixtures {
        let reduced = reduce_tool_output("process_exec", &fixture, failed);
        assert_eq!(
            reduced.before_tokens,
            estimated_tokens(fixture.len()),
            "{name}"
        );
        assert_eq!(
            reduced.after_tokens,
            estimated_tokens(reduced.text.len()),
            "{name}"
        );
        assert!(reduced.after_tokens <= reduced.before_tokens, "{name}");
        let expected = match name {
            "listing" => (875, 11),
            "grep" => (400, 9),
            "cargo" => (60, 40),
            "3kb-file" => (800, 14),
            _ => unreachable!("fixture names are closed"),
        };
        assert_eq!((reduced.before_tokens, reduced.after_tokens), expected);
    }

    let unique = (0..2_000)
        .map(|index| format!("unique successful output line {index}"))
        .collect::<Vec<_>>()
        .join("\n");
    let reduced = reduce_tool_output("process_exec", &unique, false);
    assert!(reduced.text.len() <= REDUCED_TOOL_OUTPUT_MAX_BYTES);
    assert!(reduced.text.contains("raw transcript in artifact"));
}
