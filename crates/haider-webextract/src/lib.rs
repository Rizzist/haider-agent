//! haider-webextract — readable-content extraction for fetched web pages.
//!
//! A pure HTML-in/markdown-out library: parse HTML into a small arena DOM,
//! select the main content root with a Readability-style density score
//! (link density, text length, paragraph contributions — written clean-room
//! from the published description of the algorithm, no vendored code), and
//! serialize that subtree to GFM-flavoured markdown with real table support.
//!
//! Deliberately dependency-free (std only) so it compiles unchanged for the
//! Android cdylib and every cross target. Input is hostile (model-directed
//! fetches); the parser is single-pass, depth- and node-capped, and never
//! recurses on input nesting during parsing.

mod dom;
mod markdown;
mod score;
mod text;

/// The extracted readable view of one HTML document.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ExtractedDoc {
    /// Document title (`<title>`, `og:title`, or the first `<h1>`).
    pub title: Option<String>,
    /// Author attribution when the page declares one.
    pub byline: Option<String>,
    /// GFM markdown for the selected main-content subtree.
    pub markdown: String,
    /// TRUE when boilerplate was excluded — the markdown deliberately does
    /// not represent the whole document.
    pub elided: bool,
}

/// Extracts the readable main content of `html` as markdown. `url` is the
/// document URL used to resolve relative links; pass an empty string when
/// unknown (relative links are then omitted, their text kept).
#[must_use]
pub fn extract(html: &str, url: &str) -> ExtractedDoc {
    let dom = dom::parse(html);
    let metrics = score::TextMetrics::compute(&dom);
    let root = score::select_content_root(&dom, &metrics);
    let rendered = markdown::render(&dom, root, url);
    let title = extract_title(&dom);
    let byline = extract_byline(&dom);
    let body = dom.find_first(&["body"]).unwrap_or(dom::DOCUMENT);
    // Honest elision flag: pruned boilerplate inside the root, or a root
    // that excludes part of the document's visible text.
    let elided = rendered.pruned || metrics.text_len(root) < metrics.text_len(body);
    ExtractedDoc {
        title,
        byline,
        markdown: rendered.markdown,
        elided,
    }
}

fn extract_title(dom: &dom::Dom) -> Option<String> {
    if let Some(id) = dom.find_first(&["title"]) {
        let title = text::normalize_whitespace(&text::decode_entities(&dom.raw_text(id)));
        if !title.is_empty() {
            return Some(title);
        }
    }
    if let Some(content) = dom.find_meta_content("property", "og:title") {
        let title = text::normalize_whitespace(&text::decode_entities(content));
        if !title.is_empty() {
            return Some(title);
        }
    }
    let h1 = dom.find_first(&["h1"])?;
    let title = text::normalize_whitespace(&text::decode_entities(&dom.raw_text(h1)));
    (!title.is_empty()).then_some(title)
}

fn extract_byline(dom: &dom::Dom) -> Option<String> {
    if let Some(content) = dom.find_meta_content("name", "author") {
        let byline = text::normalize_whitespace(&text::decode_entities(content));
        if !byline.is_empty() {
            return Some(byline);
        }
    }
    // First element whose class/id/rel/itemprop names an author/byline and
    // whose text is short enough to be attribution rather than content.
    for id in dom.element_ids() {
        let marked = ["class", "id", "rel", "itemprop"].iter().any(|attribute| {
            dom.attr(id, attribute).is_some_and(|value| {
                let value = value.to_ascii_lowercase();
                value.contains("author") || value.contains("byline")
            })
        });
        if !marked {
            continue;
        }
        let byline = text::normalize_whitespace(&text::decode_entities(&dom.raw_text(id)));
        if !byline.is_empty() && byline.len() <= 150 {
            return Some(byline);
        }
    }
    None
}
