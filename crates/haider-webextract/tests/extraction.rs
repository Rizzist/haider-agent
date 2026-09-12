use haider_webextract::extract;

#[test]
fn article_fixture_matches_markdown_golden() {
    let html = include_str!("fixtures/article.html");
    let expected = include_str!("fixtures/article.md").trim();
    let extracted = extract(html, "https://example.test/articles/rust");
    assert_eq!(extracted.title.as_deref(), Some("Rust memory safety"));
    assert_eq!(extracted.byline.as_deref(), Some("By Ada Lovelace"));
    assert!(extracted.elided);
    assert_eq!(extracted.markdown, expected);
    assert!(!extracted.markdown.contains("Subscribe"));
    assert!(!extracted.markdown.contains("tracking"));
}

#[test]
fn extraction_reduces_representative_page_tokens() {
    let html = include_str!("fixtures/article.html");
    let markdown = extract(html, "https://example.test/articles/rust").markdown;
    let estimated_input_tokens = html.len().div_ceil(4);
    let estimated_output_tokens = markdown.len().div_ceil(4);
    assert!(
        estimated_output_tokens < estimated_input_tokens,
        "estimated tokens did not reduce: {estimated_output_tokens} >= {estimated_input_tokens}"
    );
}

#[test]
fn real_rust_book_fixture_matches_golden_and_reduces_tokens() {
    let html = include_str!("fixtures/rust-book.html");
    let expected = include_str!("fixtures/rust-book.md").trim();
    let extracted = extract(
        html,
        "https://doc.rust-lang.org/book/ch04-01-what-is-ownership.html",
    );
    assert_eq!(extracted.markdown, expected);
    assert!(extracted.elided);
    assert!(extracted.markdown.contains("## What Is Ownership?"));
    assert!(!extracted.markdown.contains("Keyboard shortcuts"));
    assert!(extracted.markdown.len().div_ceil(4) < html.len().div_ceil(4));
}

#[test]
fn malformed_and_relative_links_are_safe() {
    let extracted = extract(
        "<main><p>Read <a href=\"/guide\">the guide</a> &amp; stay safe.</p><script>x</script>",
        "https://example.test/articles/rust",
    );
    assert_eq!(
        extracted.markdown,
        "Read [the guide](https://example.test/guide) & stay safe."
    );
}
