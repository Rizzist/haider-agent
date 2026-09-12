//! DOM-subtree → GFM markdown serialization: headings, paragraphs, lists,
//! links, images, emphasis, code blocks, blockquotes, and real tables
//! (header/body rows to GFM pipes). Boilerplate elements are pruned and the
//! pruning is reported so the caller can disclose the elision.

use crate::dom::{Dom, NodeKind};
use crate::score;
use crate::text;

pub(crate) struct Rendered {
    pub markdown: String,
    pub pruned: bool,
}

pub(crate) fn render(dom: &Dom, root: usize, base_url: &str) -> Rendered {
    let mut renderer = Renderer {
        dom,
        base: base_url,
        pruned: false,
    };
    let mut output = String::new();
    renderer.render_blocks(root, &mut output);
    let markdown = output.trim_matches('\n').to_owned();
    Rendered {
        markdown,
        pruned: renderer.pruned,
    }
}

struct Renderer<'a> {
    dom: &'a Dom,
    base: &'a str,
    pruned: bool,
}

impl Renderer<'_> {
    /// Renders `id`'s children as block content: consecutive inline nodes
    /// accumulate into paragraphs; block elements flush and recurse.
    fn render_blocks(&mut self, id: usize, out: &mut String) {
        let mut inline_buffer = String::new();
        for child in self.dom.children(id).to_vec() {
            let block_name = self
                .dom
                .name(child)
                .filter(|name| score::is_block(name))
                .map(str::to_owned);
            if let Some(name) = block_name {
                flush_paragraph(out, &mut inline_buffer);
                self.render_block(child, &name, out);
            } else {
                self.render_inline(child, &mut inline_buffer);
            }
        }
        flush_paragraph(out, &mut inline_buffer);
    }

    fn render_block(&mut self, id: usize, name: &str, out: &mut String) {
        if score::is_boilerplate(self.dom, id) {
            self.pruned = true;
            return;
        }
        match name {
            "h1" | "h2" | "h3" | "h4" | "h5" | "h6" => {
                let mut buffer = String::new();
                self.render_inline_children(id, &mut buffer);
                let heading = buffer.trim();
                if !heading.is_empty() {
                    let level = usize::from(name.as_bytes()[1] - b'0');
                    push_block(out, &format!("{} {heading}", "#".repeat(level)));
                }
            }
            "p" | "dt" | "dd" | "figcaption" | "summary" | "address" => {
                let has_block_child = self
                    .dom
                    .children(id)
                    .iter()
                    .any(|child| self.dom.name(*child).is_some_and(score::is_block));
                if has_block_child {
                    self.render_blocks(id, out);
                } else {
                    let mut buffer = String::new();
                    self.render_inline_children(id, &mut buffer);
                    flush_paragraph(out, &mut buffer);
                }
            }
            "ul" | "ol" => self.render_list(id, name == "ol", out),
            "pre" => self.render_code_block(id, out),
            "blockquote" => {
                let mut inner = String::new();
                self.render_blocks(id, &mut inner);
                let inner = inner.trim_matches('\n');
                if !inner.is_empty() {
                    let quoted = inner
                        .lines()
                        .map(|line| {
                            if line.is_empty() {
                                ">".to_owned()
                            } else {
                                format!("> {line}")
                            }
                        })
                        .collect::<Vec<_>>()
                        .join("\n");
                    push_block(out, &quoted);
                }
            }
            "table" => self.render_table(id, out),
            "hr" => push_block(out, "---"),
            _ => self.render_blocks(id, out),
        }
    }

    fn render_inline_children(&mut self, id: usize, buffer: &mut String) {
        for child in self.dom.children(id).to_vec() {
            self.render_inline(child, buffer);
        }
    }

    fn render_inline(&mut self, id: usize, buffer: &mut String) {
        match &self.dom.nodes[id].kind {
            NodeKind::Text(raw) => append_text(buffer, &text::decode_entities(raw)),
            NodeKind::Document => {}
            NodeKind::Element { name, .. } => {
                if score::is_boilerplate(self.dom, id) {
                    self.pruned = true;
                    return;
                }
                match name.as_str() {
                    "br" => buffer.push('\n'),
                    "a" => self.render_link(id, buffer),
                    "img" => self.render_image(id, buffer),
                    "strong" | "b" => self.render_wrapped(id, "**", buffer),
                    "em" | "i" | "cite" | "var" => self.render_wrapped(id, "*", buffer),
                    "del" | "s" | "strike" => self.render_wrapped(id, "~~", buffer),
                    "code" | "kbd" | "samp" | "tt" => {
                        let raw = text::normalize_whitespace(&text::decode_entities(
                            &self.dom.raw_text(id),
                        ));
                        if !raw.is_empty() {
                            let fence = if raw.contains('`') { "``" } else { "`" };
                            ensure_inline_gap(buffer);
                            buffer.push_str(fence);
                            buffer.push_str(&raw);
                            buffer.push_str(fence);
                        }
                    }
                    _ => self.render_inline_children(id, buffer),
                }
            }
        }
    }

    fn render_wrapped(&mut self, id: usize, marker: &str, buffer: &mut String) {
        let mut inner = String::new();
        self.render_inline_children(id, &mut inner);
        let inner = inner.trim();
        if inner.is_empty() {
            return;
        }
        ensure_inline_gap(buffer);
        buffer.push_str(marker);
        buffer.push_str(inner);
        buffer.push_str(marker);
    }

    fn render_link(&mut self, id: usize, buffer: &mut String) {
        let mut label = String::new();
        self.render_inline_children(id, &mut label);
        let label = label.trim().replace(['[', ']'], "");
        let url = self
            .dom
            .attr(id, "href")
            .map(text::decode_entities)
            .and_then(|href| text::resolve_url(self.base, &href));
        match (label.is_empty(), url) {
            (false, Some(url)) => {
                ensure_inline_gap(buffer);
                buffer.push_str(&format!("[{label}]({url})"));
            }
            (false, None) => append_text(buffer, &label),
            (true, Some(url)) => {
                ensure_inline_gap(buffer);
                buffer.push_str(&format!("<{url}>"));
            }
            (true, None) => {}
        }
    }

    fn render_image(&mut self, id: usize, buffer: &mut String) {
        let alt = self
            .dom
            .attr(id, "alt")
            .map(|alt| text::normalize_whitespace(&text::decode_entities(alt)))
            .unwrap_or_default();
        let src = self
            .dom
            .attr(id, "src")
            .map(text::decode_entities)
            .and_then(|src| text::resolve_url(self.base, &src));
        match src {
            Some(src) => {
                ensure_inline_gap(buffer);
                buffer.push_str(&format!("![{alt}]({src})"));
            }
            None => append_text(buffer, &alt),
        }
    }

    fn render_list(&mut self, id: usize, ordered: bool, out: &mut String) {
        let mut lines = Vec::new();
        let mut index = 0usize;
        for child in self.dom.children(id).to_vec() {
            if self.dom.name(child) != Some("li") {
                continue;
            }
            if score::is_boilerplate(self.dom, child) {
                self.pruned = true;
                continue;
            }
            index += 1;
            let marker = if ordered {
                format!("{index}. ")
            } else {
                "- ".to_owned()
            };
            let mut item = String::new();
            self.render_blocks(child, &mut item);
            let item = item.trim_matches('\n');
            if item.is_empty() {
                continue;
            }
            let indent = " ".repeat(marker.len());
            let mut rendered = String::new();
            for (line_index, line) in item.lines().enumerate() {
                if line_index == 0 {
                    rendered.push_str(&marker);
                } else {
                    rendered.push('\n');
                    if !line.is_empty() {
                        rendered.push_str(&indent);
                    }
                }
                rendered.push_str(line);
            }
            lines.push(rendered);
        }
        if !lines.is_empty() {
            push_block(out, &lines.join("\n"));
        }
    }

    fn render_code_block(&mut self, id: usize, out: &mut String) {
        let raw = text::decode_entities(&self.dom.raw_text(id));
        let raw = raw.trim_matches('\n');
        if raw.is_empty() {
            return;
        }
        let language = self
            .dom
            .children(id)
            .iter()
            .find(|child| self.dom.name(**child) == Some("code"))
            .and_then(|code| self.dom.attr(*code, "class"))
            .and_then(|class| {
                class
                    .split_ascii_whitespace()
                    .find_map(|token| token.strip_prefix("language-"))
            })
            .unwrap_or("")
            .to_owned();
        let mut fence = "```".to_owned();
        while raw.contains(&fence) {
            fence.push('`');
        }
        push_block(out, &format!("{fence}{language}\n{raw}\n{fence}"));
    }

    fn render_table(&mut self, id: usize, out: &mut String) {
        let mut header: Option<Vec<String>> = None;
        let mut rows: Vec<Vec<String>> = Vec::new();
        self.collect_table_rows(id, &mut header, &mut rows);
        let mut all_rows = rows;
        let header = match header {
            Some(header) => header,
            None if !all_rows.is_empty() => all_rows.remove(0),
            None => return,
        };
        let columns = header
            .len()
            .max(all_rows.iter().map(Vec::len).max().unwrap_or(0));
        if columns == 0 {
            return;
        }
        let mut table = String::new();
        push_table_row(&mut table, &header, columns);
        table.push('\n');
        table.push_str(&format!("|{}", " --- |".repeat(columns)));
        for row in &all_rows {
            table.push('\n');
            push_table_row(&mut table, row, columns);
        }
        push_block(out, &table);
    }

    fn collect_table_rows(
        &mut self,
        id: usize,
        header: &mut Option<Vec<String>>,
        rows: &mut Vec<Vec<String>>,
    ) {
        for child in self.dom.children(id).to_vec() {
            match self.dom.name(child) {
                Some("tr") => {
                    let mut cells = Vec::new();
                    let mut all_th = true;
                    let mut any_cell = false;
                    for cell in self.dom.children(child).to_vec() {
                        match self.dom.name(cell) {
                            Some("td") => {
                                all_th = false;
                                any_cell = true;
                            }
                            Some("th") => any_cell = true,
                            _ => continue,
                        }
                        let mut buffer = String::new();
                        self.render_blocks(cell, &mut buffer);
                        cells.push(table_cell_text(&buffer));
                    }
                    if !any_cell {
                        continue;
                    }
                    if header.is_none() && all_th && rows.is_empty() {
                        *header = Some(cells);
                    } else {
                        rows.push(cells);
                    }
                }
                Some("thead" | "tbody" | "tfoot") => {
                    self.collect_table_rows(child, header, rows);
                }
                Some("caption") => {
                    // Captions render ahead of the table as plain text via
                    // the block pass; inside collection they are skipped.
                }
                _ => {}
            }
        }
    }
}

/// One markdown table row, padded to `columns` cells.
fn push_table_row(table: &mut String, cells: &[String], columns: usize) {
    table.push('|');
    for index in 0..columns {
        table.push(' ');
        table.push_str(cells.get(index).map_or("", String::as_str));
        table.push_str(" |");
    }
}

/// Flattens block-rendered cell content to one escaped table-cell line.
fn table_cell_text(rendered: &str) -> String {
    let flattened = rendered
        .trim_matches('\n')
        .replace("\n\n", " — ")
        .replace('\n', " ");
    text::normalize_whitespace(&flattened).replace('|', "\\|")
}

/// Appends inline text with whitespace runs collapsed to single spaces.
fn append_text(buffer: &mut String, decoded: &str) {
    for character in decoded.chars() {
        if character.is_whitespace() {
            if !buffer.is_empty() && !buffer.ends_with(char::is_whitespace) {
                buffer.push(' ');
            }
        } else {
            buffer.push(character);
        }
    }
}

/// A space before an inline atom (link/emphasis/code) when text abuts it.
fn ensure_inline_gap(buffer: &mut String) {
    if !buffer.is_empty() && !buffer.ends_with(char::is_whitespace) && !buffer.ends_with('(') {
        buffer.push(' ');
    }
}

fn flush_paragraph(out: &mut String, buffer: &mut String) {
    let paragraph = buffer.trim();
    if !paragraph.is_empty() {
        push_block(out, paragraph);
    }
    buffer.clear();
}

fn push_block(out: &mut String, block: &str) {
    if !out.is_empty() {
        while out.ends_with(' ') {
            out.pop();
        }
        if !out.ends_with("\n\n") {
            if out.ends_with('\n') {
                out.push('\n');
            } else {
                out.push_str("\n\n");
            }
        }
    }
    out.push_str(block);
}
