//! Readability-style content scoring, written clean-room from the published
//! description of the Mozilla Readability scoring core: paragraph-ish nodes
//! contribute points (base + comma count + capped text length) to their
//! parent and half to their grandparent; containers start from a tag prior
//! and a class/id weight; the winning candidate is the highest score after a
//! link-density penalty.

use std::collections::HashMap;

use crate::dom::{DOCUMENT, Dom, NodeKind};

/// Elements whose subtree text is never document content.
pub(crate) fn is_invisible(name: &str) -> bool {
    matches!(
        name,
        "script"
            | "style"
            | "noscript"
            | "template"
            | "head"
            | "title"
            | "iframe"
            | "svg"
            | "object"
            | "embed"
            | "link"
            | "meta"
    )
}

/// Block-level elements (paragraph detection + markdown separation).
pub(crate) fn is_block(name: &str) -> bool {
    matches!(
        name,
        "address"
            | "article"
            | "aside"
            | "blockquote"
            | "details"
            | "dialog"
            | "div"
            | "dl"
            | "dt"
            | "dd"
            | "fieldset"
            | "figure"
            | "figcaption"
            | "footer"
            | "form"
            | "h1"
            | "h2"
            | "h3"
            | "h4"
            | "h5"
            | "h6"
            | "header"
            | "hr"
            | "li"
            | "main"
            | "nav"
            | "ol"
            | "p"
            | "pre"
            | "section"
            | "summary"
            | "table"
            | "tbody"
            | "td"
            | "tfoot"
            | "th"
            | "thead"
            | "tr"
            | "ul"
    )
}

/// Class/id fragments that mark probable content.
const POSITIVE: &[&str] = &[
    "article", "body", "content", "entry", "hentry", "h-entry", "main", "page", "post", "text",
    "blog", "story",
];
/// Class/id fragments that mark probable boilerplate/chrome.
const NEGATIVE: &[&str] = &[
    "banner",
    "combx",
    "comment",
    "community",
    "consent",
    "cookie",
    "disqus",
    "extra",
    "footer",
    "gdpr",
    "masthead",
    "menu",
    "modal",
    "navbar",
    "newsletter",
    "outbrain",
    "overlay",
    "popup",
    "promo",
    "related",
    "remark",
    "rss",
    "share",
    "shoutbox",
    "sidebar",
    "skyscraper",
    "social",
    "sponsor",
    "subscribe",
    "supplemental",
    "widget",
];

fn class_id_lower(dom: &Dom, id: usize) -> String {
    let mut joined = String::new();
    for attribute in ["class", "id"] {
        if let Some(value) = dom.attr(id, attribute) {
            joined.push_str(&value.to_ascii_lowercase());
            joined.push(' ');
        }
    }
    joined
}

fn class_weight(dom: &Dom, id: usize) -> f64 {
    let joined = class_id_lower(dom, id);
    let mut weight = 0.0;
    if POSITIVE.iter().any(|marker| joined.contains(marker)) {
        weight += 25.0;
    }
    if NEGATIVE.iter().any(|marker| joined.contains(marker)) {
        weight -= 25.0;
    }
    weight
}

/// TRUE for elements the markdown pass drops as boilerplate: page chrome by
/// tag, or a negative class/id weight without positive evidence.
pub(crate) fn is_boilerplate(dom: &Dom, id: usize) -> bool {
    let Some(name) = dom.name(id) else {
        return false;
    };
    if matches!(
        name,
        "nav"
            | "footer"
            | "aside"
            | "form"
            | "button"
            | "select"
            | "option"
            | "label"
            | "input"
            | "dialog"
    ) {
        return true;
    }
    if is_invisible(name) {
        return true;
    }
    let joined = class_id_lower(dom, id);
    NEGATIVE.iter().any(|marker| joined.contains(marker))
        && !POSITIVE.iter().any(|marker| joined.contains(marker))
}

/// Per-node visible-text metrics, accumulated bottom-up in one linear pass
/// (arena ids are document-ordered, so children always have larger ids than
/// their parents and a reverse scan is a post-order accumulation).
pub(crate) struct TextMetrics {
    text_len: Vec<usize>,
    link_len: Vec<usize>,
    commas: Vec<usize>,
}

impl TextMetrics {
    pub(crate) fn compute(dom: &Dom) -> Self {
        let count = dom.nodes.len();
        let mut text_len = vec![0usize; count];
        let mut link_len = vec![0usize; count];
        let mut commas = vec![0usize; count];
        for id in (1..count).rev() {
            let node = &dom.nodes[id];
            let Some(parent) = node.parent else {
                continue;
            };
            match &node.kind {
                NodeKind::Text(text) => {
                    let visible = text.chars().filter(|c| !c.is_whitespace()).count();
                    text_len[id] += visible;
                    commas[id] += text.matches([',', '，']).count();
                }
                NodeKind::Element { name, .. } => {
                    if is_invisible(name) {
                        continue; // Never propagates to the parent.
                    }
                    if name == "a" {
                        // The whole subtree is link text from the parent's view.
                        link_len[id] = text_len[id];
                    }
                }
                NodeKind::Document => continue,
            }
            text_len[parent] += text_len[id];
            link_len[parent] += link_len[id];
            commas[parent] += commas[id];
        }
        Self {
            text_len,
            link_len,
            commas,
        }
    }

    pub(crate) fn text_len(&self, id: usize) -> usize {
        self.text_len.get(id).copied().unwrap_or(0)
    }

    fn link_len(&self, id: usize) -> usize {
        self.link_len.get(id).copied().unwrap_or(0)
    }

    fn commas(&self, id: usize) -> usize {
        self.commas.get(id).copied().unwrap_or(0)
    }
}

fn has_block_child(dom: &Dom, id: usize) -> bool {
    dom.children(id)
        .iter()
        .any(|child| dom.name(*child).is_some_and(is_block))
}

fn inside_invisible(dom: &Dom, id: usize) -> bool {
    let mut current = dom.parent(id);
    while let Some(ancestor) = current {
        if ancestor == DOCUMENT {
            return false;
        }
        if dom.name(ancestor).is_some_and(is_invisible) {
            return true;
        }
        current = dom.parent(ancestor);
    }
    false
}

/// Tag prior for a scoring container (the Readability initialization step).
fn tag_prior(name: &str) -> f64 {
    match name {
        "div" | "article" | "section" | "main" => 5.0,
        "pre" | "td" | "blockquote" => 3.0,
        "address" | "ol" | "ul" | "dl" | "dd" | "dt" | "li" | "form" => -3.0,
        "h1" | "h2" | "h3" | "h4" | "h5" | "h6" | "th" => -5.0,
        _ => 0.0,
    }
}

fn initial_score(dom: &Dom, id: usize) -> f64 {
    let name = dom.name(id).unwrap_or("");
    tag_prior(name) + class_weight(dom, id)
}

/// Minimum candidate subtree text (visible chars) before the winner replaces
/// the whole body — a page with no real article keeps its body root and lets
/// the caller's fallback logic decide.
const MIN_CANDIDATE_TEXT: usize = 140;
/// Minimum paragraph text before it contributes points.
const MIN_PARAGRAPH_TEXT: usize = 25;

/// Selects the main-content root: the highest-scoring candidate after the
/// link-density penalty, or the body when nothing scores.
pub(crate) fn select_content_root(dom: &Dom, metrics: &TextMetrics) -> usize {
    let body = dom.find_first(&["body"]).unwrap_or(DOCUMENT);
    let mut scores: HashMap<usize, f64> = HashMap::new();
    for id in dom.element_ids() {
        let name = dom.name(id).unwrap_or("");
        let paragraph_like = matches!(name, "p" | "td" | "pre" | "blockquote")
            || (name == "div" && !has_block_child(dom, id));
        if !paragraph_like || inside_invisible(dom, id) {
            continue;
        }
        let text = metrics.text_len(id);
        if text < MIN_PARAGRAPH_TEXT {
            continue;
        }
        #[allow(clippy::cast_precision_loss)]
        let points = 1.0 + metrics.commas(id) as f64 + (text as f64 / 100.0).min(3.0);
        let Some(parent) = dom.parent(id).filter(|p| *p != DOCUMENT) else {
            continue;
        };
        *scores
            .entry(parent)
            .or_insert_with(|| initial_score(dom, parent)) += points;
        if let Some(grandparent) = dom.parent(parent).filter(|g| *g != DOCUMENT) {
            *scores
                .entry(grandparent)
                .or_insert_with(|| initial_score(dom, grandparent)) += points / 2.0;
        }
    }
    let mut best: Option<(usize, f64)> = None;
    for (id, score) in &scores {
        let text = metrics.text_len(*id);
        if text == 0 {
            continue;
        }
        #[allow(clippy::cast_precision_loss)]
        let density = metrics.link_len(*id) as f64 / text as f64;
        let adjusted = score * (1.0 - density);
        // Deterministic tie-break: prefer the earlier (outer) node id.
        let replaces = match best {
            None => true,
            Some((best_id, best_score)) => {
                adjusted > best_score || (adjusted == best_score && *id < best_id)
            }
        };
        if replaces {
            best = Some((*id, adjusted));
        }
    }
    match best {
        Some((id, score)) if score > 0.0 && metrics.text_len(id) >= MIN_CANDIDATE_TEXT => id,
        _ => body,
    }
}
