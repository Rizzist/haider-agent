//! Arena DOM and a bounded, single-pass HTML parser.
//!
//! The parser is written for HOSTILE input: iteration is linear over the
//! source, open-element depth is capped, the node count is capped, and a
//! mismatched close tag scans only the bounded open stack. It is a
//! reduction-quality parser (quote-aware tags, raw-text elements, implied
//! closes for `p`/`li`/table cells), not a spec HTML5 tree builder.

/// The synthetic document root node id.
pub(crate) const DOCUMENT: usize = 0;

/// Maximum open-element depth; opens past it become self-closing so their
/// children attach to a bounded ancestor chain.
const MAX_OPEN_DEPTH: usize = 256;
/// Maximum nodes materialized for one document; tags past it are skipped
/// (their text still flows into the current open element).
const MAX_NODES: usize = 250_000;

/// Void elements never take children.
const VOID: &[&str] = &[
    "area", "base", "br", "col", "embed", "hr", "img", "input", "link", "meta", "param", "source",
    "track", "wbr",
];
/// Raw-text elements: content runs to the matching close tag, unparsed.
const RAW_TEXT: &[&str] = &["script", "style", "textarea", "title"];
/// Elements whose open tag implicitly closes an open `<p>`.
const CLOSES_P: &[&str] = &[
    "address",
    "article",
    "aside",
    "blockquote",
    "details",
    "dialog",
    "div",
    "dl",
    "fieldset",
    "figure",
    "footer",
    "form",
    "h1",
    "h2",
    "h3",
    "h4",
    "h5",
    "h6",
    "header",
    "hr",
    "li",
    "main",
    "nav",
    "ol",
    "p",
    "pre",
    "section",
    "table",
    "ul",
];

#[derive(Debug)]
pub(crate) enum NodeKind {
    Document,
    Element {
        name: String,
        attrs: Vec<(String, String)>,
    },
    Text(String),
}

#[derive(Debug)]
pub(crate) struct Node {
    pub parent: Option<usize>,
    pub kind: NodeKind,
    pub children: Vec<usize>,
}

#[derive(Debug)]
pub(crate) struct Dom {
    pub nodes: Vec<Node>,
}

impl Dom {
    pub(crate) fn name(&self, id: usize) -> Option<&str> {
        match self.nodes.get(id).map(|node| &node.kind) {
            Some(NodeKind::Element { name, .. }) => Some(name.as_str()),
            _ => None,
        }
    }

    pub(crate) fn attr(&self, id: usize, attribute: &str) -> Option<&str> {
        match self.nodes.get(id).map(|node| &node.kind) {
            Some(NodeKind::Element { attrs, .. }) => attrs
                .iter()
                .find(|(name, _)| name == attribute)
                .map(|(_, value)| value.as_str()),
            _ => None,
        }
    }

    pub(crate) fn children(&self, id: usize) -> &[usize] {
        self.nodes
            .get(id)
            .map(|node| node.children.as_slice())
            .unwrap_or_default()
    }

    pub(crate) fn parent(&self, id: usize) -> Option<usize> {
        self.nodes.get(id).and_then(|node| node.parent)
    }

    /// Document-order ids of every element node.
    pub(crate) fn element_ids(&self) -> impl Iterator<Item = usize> + '_ {
        (0..self.nodes.len()).filter(|id| matches!(self.nodes[*id].kind, NodeKind::Element { .. }))
    }

    /// First element (document order) whose name is in `names`.
    pub(crate) fn find_first(&self, names: &[&str]) -> Option<usize> {
        self.element_ids()
            .find(|id| self.name(*id).is_some_and(|name| names.contains(&name)))
    }

    /// `content` attribute of the first `<meta>` whose `attribute` equals
    /// `value` (ASCII case-insensitive).
    pub(crate) fn find_meta_content(&self, attribute: &str, value: &str) -> Option<&str> {
        self.element_ids()
            .filter(|id| self.name(*id) == Some("meta"))
            .find(|id| {
                self.attr(*id, attribute)
                    .is_some_and(|found| found.eq_ignore_ascii_case(value))
            })
            .and_then(|id| self.attr(id, "content"))
    }

    /// Concatenated descendant text (raw, entities NOT decoded), skipping
    /// non-visible containers. Iterative — hostile nesting cannot recurse.
    pub(crate) fn raw_text(&self, id: usize) -> String {
        let mut output = String::new();
        let mut stack = vec![id];
        while let Some(current) = stack.pop() {
            match &self.nodes[current].kind {
                NodeKind::Text(text) => output.push_str(text),
                NodeKind::Element { name, .. }
                    if current != id && crate::score::is_invisible(name) => {}
                _ => {
                    for child in self.nodes[current].children.iter().rev() {
                        stack.push(*child);
                    }
                }
            }
        }
        output
    }
}

/// Parses `html` into an arena DOM. Never fails: malformed input degrades to
/// text/skipped markup, mirroring browser error recovery loosely.
pub(crate) fn parse(html: &str) -> Dom {
    let mut dom = Dom {
        nodes: vec![Node {
            parent: None,
            kind: NodeKind::Document,
            children: Vec::new(),
        }],
    };
    // Open-element stack; DOCUMENT is always index 0 and never popped.
    let mut stack: Vec<usize> = vec![DOCUMENT];
    let mut position = 0usize;
    while position < html.len() {
        let Some(offset) = html[position..].find('<') else {
            append_text(&mut dom, &stack, &html[position..]);
            break;
        };
        if offset > 0 {
            append_text(&mut dom, &stack, &html[position..position + offset]);
        }
        let start = position + offset;
        let rest = &html[start..];
        if rest.starts_with("<!--") {
            position = html[start..]
                .find("-->")
                .map_or(html.len(), |end| start + end + 3);
            continue;
        }
        if rest.starts_with("<!") || rest.starts_with("<?") {
            position = html[start..]
                .find('>')
                .map_or(html.len(), |end| start + end + 1);
            continue;
        }
        let Some(end) = tag_end(html, start) else {
            // Unterminated tag: everything after is markup noise.
            break;
        };
        let tag = &html[start + 1..end];
        position = end + 1;
        let closing = tag.starts_with('/');
        let name = tag
            .trim_start_matches('/')
            .split(|character: char| character.is_ascii_whitespace() || character == '/')
            .next()
            .unwrap_or_default()
            .to_ascii_lowercase();
        if name.is_empty() || !name.starts_with(|c: char| c.is_ascii_alphabetic()) {
            // `<3` and friends are text in browsers; keep the literal bytes.
            append_text(&mut dom, &stack, &html[start..position]);
            continue;
        }
        if closing {
            close_element(&dom, &mut stack, &name);
            continue;
        }
        if dom.nodes.len() >= MAX_NODES {
            continue; // Node cap: skip further markup, keep collecting text.
        }
        apply_implied_closes(&dom, &mut stack, &name);
        let self_closing =
            tag.ends_with('/') || VOID.contains(&name.as_str()) || stack.len() >= MAX_OPEN_DEPTH;
        let raw_text = RAW_TEXT.contains(&name.as_str());
        let attrs = parse_attributes(tag);
        let parent = *stack.last().unwrap_or(&DOCUMENT);
        let id = dom.nodes.len();
        dom.nodes.push(Node {
            parent: Some(parent),
            kind: NodeKind::Element {
                name: name.clone(),
                attrs,
            },
            children: Vec::new(),
        });
        dom.nodes[parent].children.push(id);
        if raw_text && !tag.ends_with('/') {
            // Raw-text content runs to the matching close tag (or EOF).
            let (content_end, after_close) = find_raw_close(html, position, &name);
            if content_end > position && dom.nodes.len() < MAX_NODES {
                let text_id = dom.nodes.len();
                dom.nodes.push(Node {
                    parent: Some(id),
                    kind: NodeKind::Text(html[position..content_end].to_owned()),
                    children: Vec::new(),
                });
                dom.nodes[id].children.push(text_id);
            }
            position = after_close;
        } else if !self_closing {
            stack.push(id);
        }
    }
    dom
}

fn append_text(dom: &mut Dom, stack: &[usize], text: &str) {
    if text.is_empty() || dom.nodes.len() >= MAX_NODES {
        return;
    }
    let parent = *stack.last().unwrap_or(&DOCUMENT);
    let id = dom.nodes.len();
    dom.nodes.push(Node {
        parent: Some(parent),
        kind: NodeKind::Text(text.to_owned()),
        children: Vec::new(),
    });
    dom.nodes[parent].children.push(id);
}

/// Close the nearest open element with `name` (popping everything above it);
/// unmatched closes are ignored. The stack is depth-capped, so this scan is
/// O(MAX_OPEN_DEPTH) worst case per close tag.
fn close_element(dom: &Dom, stack: &mut Vec<usize>, name: &str) {
    for position in (1..stack.len()).rev() {
        if dom.name(stack[position]) == Some(name) {
            stack.truncate(position);
            return;
        }
    }
}

/// Pop up to (and including) the nearest open element named in `targets`,
/// stopping the search at any element named in `boundaries`.
fn close_within(dom: &Dom, stack: &mut Vec<usize>, targets: &[&str], boundaries: &[&str]) {
    for position in (1..stack.len()).rev() {
        let Some(name) = dom.name(stack[position]) else {
            continue;
        };
        if targets.contains(&name) {
            stack.truncate(position);
            return;
        }
        if boundaries.contains(&name) {
            return;
        }
    }
}

fn apply_implied_closes(dom: &Dom, stack: &mut Vec<usize>, incoming: &str) {
    match incoming {
        "li" => close_within(dom, stack, &["li"], &["ul", "ol"]),
        "dt" | "dd" => close_within(dom, stack, &["dt", "dd"], &["dl"]),
        "td" | "th" => close_within(dom, stack, &["td", "th"], &["tr", "table"]),
        "tr" => {
            close_within(dom, stack, &["td", "th"], &["tr", "table"]);
            close_within(dom, stack, &["tr"], &["table", "thead", "tbody", "tfoot"]);
        }
        "thead" | "tbody" | "tfoot" => {
            close_within(dom, stack, &["td", "th"], &["tr", "table"]);
            close_within(dom, stack, &["tr"], &["table"]);
            close_within(dom, stack, &["thead", "tbody", "tfoot"], &["table"]);
        }
        _ => {}
    }
    if CLOSES_P.contains(&incoming) {
        close_within(
            dom,
            stack,
            &["p"],
            &[
                "div",
                "section",
                "article",
                "td",
                "th",
                "li",
                "blockquote",
                "table",
            ],
        );
    }
}

/// Quote-aware scan for the `>` ending the tag opened at `start`.
fn tag_end(html: &str, start: usize) -> Option<usize> {
    let mut quote: Option<char> = None;
    for (offset, character) in html[start..].char_indices() {
        match (quote, character) {
            (None, '"' | '\'') => quote = Some(character),
            (Some(open), _) if character == open => quote = None,
            (None, '>') => return Some(start + offset),
            _ => {}
        }
    }
    None
}

/// Finds `</name` (ASCII case-insensitive) at/after `from`. Returns
/// (content end, position after the close tag's `>`); EOF ends both.
fn find_raw_close(html: &str, from: usize, name: &str) -> (usize, usize) {
    let bytes = html.as_bytes();
    let mut index = from;
    while index + name.len() + 2 <= html.len() {
        let Some(offset) = html[index..].find('<') else {
            break;
        };
        let at = index + offset;
        let candidate_start = at + 2;
        let candidate_end = candidate_start + name.len();
        if candidate_end <= html.len()
            && bytes[at + 1] == b'/'
            && html[candidate_start..candidate_end].eq_ignore_ascii_case(name)
            && bytes
                .get(candidate_end)
                .is_none_or(|next| matches!(next, b'>' | b' ' | b'\t' | b'\n' | b'\r' | b'/'))
        {
            let after = html[candidate_end..]
                .find('>')
                .map_or(html.len(), |end| candidate_end + end + 1);
            return (at, after);
        }
        index = at + 1;
    }
    (html.len(), html.len())
}

fn parse_attributes(tag: &str) -> Vec<(String, String)> {
    let mut attrs = Vec::new();
    // Skip the tag name.
    let mut rest = tag
        .trim_start_matches('/')
        .trim_start_matches(|c: char| !c.is_ascii_whitespace())
        .trim_start();
    while !rest.is_empty() && rest != "/" {
        let name_end = rest
            .find(|c: char| c.is_ascii_whitespace() || c == '=' || c == '/')
            .unwrap_or(rest.len());
        let name = rest[..name_end].to_ascii_lowercase();
        rest = rest[name_end..].trim_start();
        if name.is_empty() {
            rest = rest.get(1..).unwrap_or("").trim_start();
            continue;
        }
        let value = if let Some(after_equals) = rest.strip_prefix('=') {
            let after_equals = after_equals.trim_start();
            let mut characters = after_equals.chars();
            match characters.next() {
                Some(quote @ ('"' | '\'')) => {
                    let inner = &after_equals[1..];
                    let end = inner.find(quote).unwrap_or(inner.len());
                    rest = inner.get(end + 1..).unwrap_or("").trim_start();
                    inner[..end].to_owned()
                }
                Some(_) => {
                    let end = after_equals
                        .find(|c: char| c.is_ascii_whitespace())
                        .unwrap_or(after_equals.len());
                    let value = after_equals[..end].to_owned();
                    rest = after_equals[end..].trim_start();
                    value
                }
                None => {
                    rest = "";
                    String::new()
                }
            }
        } else {
            String::new()
        };
        attrs.push((name, value));
    }
    attrs
}
