//! Bounded, read-only PDF admission and text extraction.
//!
//! This deliberately implements only the PDF surface Haider needs: page-tree
//! inspection plus text-showing operators in page content streams. Unsupported
//! filters and encrypted content fail closed. Every decompression and output
//! path is bounded before untrusted bytes reach a provider request.

use flate2::read::ZlibDecoder;
use std::collections::{BTreeMap, BTreeSet};
use std::io::Read as _;

/// First-class PDF attachment cap. The RPC upload envelope allows one extra
/// MiB so the daemon can return the PDF-specific typed rejection.
pub const MAX_PDF_BYTES: usize = 32 * 1024 * 1024;
/// Page-tree admission cap. This uses the strictest active Anthropic native
/// document limit (100 pages for sub-1M context windows), so an admitted PDF
/// remains valid if its durable native-delivery receipt is replayed.
pub const MAX_PDF_PAGES: u32 = 100;
/// A single page cannot consume the whole model context.
pub const MAX_PDF_PAGE_TEXT_CHARS: usize = 50_000;
/// Total extracted text bound across every page.
pub const MAX_PDF_TOTAL_TEXT_CHARS: usize = 200_000;
/// Decompression-bomb bound applied independently to each content stream.
pub const MAX_PDF_STREAM_BYTES: usize = 8 * 1024 * 1024;

/// Cheap admission metadata derived from the actual page tree.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct PdfMetadata {
    pub pages: u32,
    pub encrypted: bool,
}

/// Bounded daemon-owned extraction result.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ExtractedPdfText {
    pub text: String,
    pub pages_extracted: u32,
    pub total_pages: u32,
    pub truncated: bool,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PdfErrorKind {
    Malformed,
    Encrypted,
    Unsupported,
    NoExtractableText,
    DecompressionLimit,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PdfError {
    pub kind: PdfErrorKind,
    pub message: String,
}

impl PdfError {
    fn new(kind: PdfErrorKind, message: impl Into<String>) -> Self {
        Self {
            kind,
            message: message.into(),
        }
    }
}

impl std::fmt::Display for PdfError {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str(&self.message)
    }
}

impl std::error::Error for PdfError {}

#[derive(Debug, Clone)]
struct PdfObject {
    body: Vec<u8>,
}

#[derive(Debug)]
struct ParsedPdf {
    objects: BTreeMap<(u32, u16), PdfObject>,
    pages: Vec<(u32, u16)>,
    encrypted: bool,
}

/// Parses page-tree metadata without decoding page content.
///
/// The minimal internal parser answers first; when it cannot see the page
/// tree (fully-compressed PDFs keep page dictionaries inside object streams)
/// the real-world backend supplies the count instead.
pub fn inspect_pdf(bytes: &[u8]) -> Result<PdfMetadata, PdfError> {
    let internal = parse_pdf(bytes).map(|parsed| PdfMetadata {
        pages: u32::try_from(parsed.pages.len()).unwrap_or(u32::MAX),
        encrypted: parsed.encrypted,
    });
    match internal {
        Ok(metadata) if metadata.pages > 0 => Ok(metadata),
        other => inspect_real_world(bytes).map_or(other, Ok),
    }
}

/// Page-tree inspection through `lopdf` for PDFs the minimal parser cannot
/// read (object streams, xref streams). `lopdf` has panic paths on exotic
/// inputs, so it runs under `catch_unwind`; a panic means "no answer here",
/// never a crash.
fn inspect_real_world(bytes: &[u8]) -> Option<PdfMetadata> {
    let owned = bytes.to_vec();
    std::panic::catch_unwind(move || {
        let document = lopdf::Document::load_mem(&owned).ok()?;
        let pages = u32::try_from(document.get_pages().len()).ok()?;
        (pages > 0).then_some(PdfMetadata {
            pages,
            encrypted: document.trailer.get(b"Encrypt").is_ok(),
        })
    })
    .ok()
    .flatten()
}

/// Real-world extraction backend: `pdf_extract` handles the generator
/// population the deliberately-minimal internal parser does not (object
/// streams, xref streams, ToUnicode CMaps, subsetted fonts — e.g. every
/// Chrome-printed PDF). It runs under `catch_unwind` because it has panic
/// paths on exotic font programs; a panic or error falls back to the
/// internal pipeline rather than surfacing.
fn extract_pages_real_world(bytes: &[u8]) -> Option<Vec<String>> {
    let owned = bytes.to_vec();
    std::panic::catch_unwind(move || pdf_extract::extract_text_from_mem_by_pages(&owned).ok())
        .ok()
        .flatten()
}

/// Applies the exact per-page, aggregate and honest-marker bounds to
/// pre-extracted page texts. Returns `None` when every page is blank so the
/// caller can fall through to the internal pipeline's typed verdict.
fn bound_real_world_pages(page_texts: &[String], total_pages: u32) -> Option<ExtractedPdfText> {
    let mut output = String::new();
    let mut pages_extracted = 0_u32;
    let mut truncated = false;
    for (page_index, raw) in page_texts.iter().enumerate() {
        let page_number = u32::try_from(page_index + 1).unwrap_or(u32::MAX);
        pages_extracted = page_number;
        let trimmed = raw.trim();
        if trimmed.is_empty() {
            continue;
        }
        let page_was_truncated = trimmed.chars().count() > MAX_PDF_PAGE_TEXT_CHARS;
        let page_text = take_chars(trimmed, MAX_PDF_PAGE_TEXT_CHARS);
        let heading = format!("[pdf page {page_number}]\n");
        let separator = if output.is_empty() { "" } else { "\n\n" };
        let required =
            separator.chars().count() + heading.chars().count() + page_text.chars().count();
        if output.chars().count().saturating_add(required) > MAX_PDF_TOTAL_TEXT_CHARS {
            let prefix = format!("{separator}{heading}");
            let room = MAX_PDF_TOTAL_TEXT_CHARS
                .saturating_sub(output.chars().count())
                .saturating_sub(prefix.chars().count());
            output.push_str(&prefix);
            output.push_str(&take_chars(&page_text, room));
            truncated = true;
            break;
        }
        output.push_str(separator);
        output.push_str(&heading);
        output.push_str(&page_text);
        if page_was_truncated {
            truncated = true;
            break;
        }
    }
    if output.trim().is_empty() {
        return None;
    }
    if pages_extracted < total_pages {
        truncated = true;
    }
    if truncated {
        let marker = format!("[pdf truncated: {pages_extracted} of {total_pages} pages]");
        let reserved = marker.chars().count() + 2;
        let keep = MAX_PDF_TOTAL_TEXT_CHARS.saturating_sub(reserved);
        output = take_chars(&output, keep);
        while output.ends_with(char::is_whitespace) {
            output.pop();
        }
        output.push_str("\n\n");
        output.push_str(&marker);
    }
    Some(ExtractedPdfText {
        text: output,
        pages_extracted,
        total_pages,
        truncated,
    })
}

/// Extracts text page by page under independent per-page, per-stream and
/// aggregate bounds. Truncation always ends with the stable honest marker.
///
/// Extraction ladder: the internal parser is the encryption authority; the
/// `pdf_extract` backend then gets the first attempt (it covers the modern
/// generator population); the internal pipeline is the fallback that also
/// owns the typed verdict when both produce nothing.
pub fn extract_text_bounded(bytes: &[u8]) -> Result<ExtractedPdfText, PdfError> {
    let parsed = parse_pdf(bytes);
    if let Ok(parsed) = &parsed
        && parsed.encrypted
    {
        return Err(PdfError::new(
            PdfErrorKind::Encrypted,
            "this PDF is encrypted; remove the password and attach it again",
        ));
    }
    let internal_pages = parsed.as_ref().map(|parsed| parsed.pages.len()).ok();
    if let Some(page_texts) = extract_pages_real_world(bytes) {
        let total_pages = u32::try_from(
            internal_pages
                .unwrap_or(page_texts.len())
                .max(page_texts.len()),
        )
        .unwrap_or(u32::MAX);
        if let Some(extracted) = bound_real_world_pages(&page_texts, total_pages) {
            return Ok(extracted);
        }
    }
    match parsed {
        Ok(parsed) => extract_text_bounded_internal(&parsed),
        Err(error) => {
            // The minimal parser could not read it and neither could the
            // real-world backend. An /Encrypt marker makes the actionable
            // verdict "remove the password", not "malformed".
            if bytes
                .windows(b"/Encrypt".len())
                .any(|window| window == b"/Encrypt")
            {
                return Err(PdfError::new(
                    PdfErrorKind::Encrypted,
                    "this PDF is encrypted; remove the password and attach it again",
                ));
            }
            Err(error)
        }
    }
}

fn extract_text_bounded_internal(parsed: &ParsedPdf) -> Result<ExtractedPdfText, PdfError> {
    let total_pages = u32::try_from(parsed.pages.len()).unwrap_or(u32::MAX);
    let mut output = String::new();
    let mut pages_extracted = 0_u32;
    let mut truncated = false;

    for (page_index, page_id) in parsed.pages.iter().enumerate() {
        let page_number = u32::try_from(page_index + 1).unwrap_or(u32::MAX);
        pages_extracted = page_number;
        let page = parsed
            .objects
            .get(page_id)
            .ok_or_else(|| PdfError::new(PdfErrorKind::Malformed, "PDF page object is missing"))?;
        let content_ids = references_after_key(&page.body, b"Contents");
        let mut page_text = String::new();
        let mut page_was_truncated = false;
        for content_id in content_ids {
            let Some(content) = parsed.objects.get(&content_id) else {
                return Err(PdfError::new(
                    PdfErrorKind::Malformed,
                    "PDF page references a missing content stream",
                ));
            };
            let decoded = decode_stream(&content.body)?;
            let separator_chars = usize::from(!page_text.is_empty() && !page_text.ends_with('\n'));
            let remaining = MAX_PDF_PAGE_TEXT_CHARS
                .saturating_sub(page_text.chars().count())
                .saturating_sub(separator_chars);
            let (next, stream_was_truncated) = extract_content_text(&decoded, remaining);
            if !next.trim().is_empty() {
                if !page_text.is_empty() && !page_text.ends_with('\n') {
                    page_text.push('\n');
                }
                page_text.push_str(&next);
            }
            if stream_was_truncated {
                page_was_truncated = true;
                break;
            }
        }
        if page_text.trim().is_empty() {
            continue;
        }

        let heading = format!("[pdf page {page_number}]\n");
        let separator = if output.is_empty() { "" } else { "\n\n" };
        let required =
            separator.chars().count() + heading.chars().count() + page_text.chars().count();
        if output.chars().count().saturating_add(required) > MAX_PDF_TOTAL_TEXT_CHARS {
            let prefix = format!("{separator}{heading}");
            let room = MAX_PDF_TOTAL_TEXT_CHARS
                .saturating_sub(output.chars().count())
                .saturating_sub(prefix.chars().count());
            output.push_str(&prefix);
            output.push_str(&take_chars(&page_text, room));
            truncated = true;
            break;
        }
        output.push_str(separator);
        output.push_str(&heading);
        output.push_str(&page_text);
        if page_was_truncated {
            truncated = true;
            break;
        }
    }

    if output.trim().is_empty() {
        return Err(PdfError::new(
            PdfErrorKind::NoExtractableText,
            "no extractable text — this PDF appears to be scanned images",
        ));
    }
    if pages_extracted < total_pages {
        truncated = true;
    }
    if truncated {
        let marker = format!("[pdf truncated: {pages_extracted} of {total_pages} pages]");
        let reserved = marker.chars().count() + 2;
        let keep = MAX_PDF_TOTAL_TEXT_CHARS.saturating_sub(reserved);
        output = take_chars(&output, keep);
        while output.ends_with(char::is_whitespace) {
            output.pop();
        }
        output.push_str("\n\n");
        output.push_str(&marker);
    }

    Ok(ExtractedPdfText {
        text: output,
        pages_extracted,
        total_pages,
        truncated,
    })
}

fn parse_pdf(bytes: &[u8]) -> Result<ParsedPdf, PdfError> {
    if bytes.len() < 8 || !bytes.starts_with(b"%PDF-") {
        return Err(PdfError::new(
            PdfErrorKind::Malformed,
            "file is not a valid PDF (missing %PDF header)",
        ));
    }
    if !bytes
        .windows(b"%%EOF".len())
        .any(|window| window == b"%%EOF")
    {
        return Err(PdfError::new(
            PdfErrorKind::Malformed,
            "PDF is truncated (missing %%EOF marker)",
        ));
    }
    let objects = parse_objects(bytes)?;
    if objects.is_empty() {
        return Err(PdfError::new(
            PdfErrorKind::Malformed,
            "PDF contains no readable indirect objects",
        ));
    }
    let encrypted = trailer_has_name(bytes, b"Encrypt");
    let pages = ordered_pages(&objects);
    if pages.is_empty() {
        return Err(PdfError::new(
            PdfErrorKind::Malformed,
            "PDF contains no readable pages",
        ));
    }
    Ok(ParsedPdf {
        objects,
        pages,
        encrypted,
    })
}

fn trailer_has_name(bytes: &[u8], name: &[u8]) -> bool {
    let mut cursor = 0;
    let mut trailer = None;
    while let Some(at) = find_keyword(bytes, b"trailer", cursor) {
        trailer = Some(at + b"trailer".len());
        cursor = at + b"trailer".len();
    }
    trailer.is_some_and(|at| find_name(bytes, name, at).is_some())
}

fn parse_objects(bytes: &[u8]) -> Result<BTreeMap<(u32, u16), PdfObject>, PdfError> {
    let mut objects = BTreeMap::new();
    let mut cursor = 0;
    while cursor < bytes.len() {
        let Some((object_id, generation, body_start)) = find_object_header(bytes, cursor) else {
            break;
        };
        let body_end = object_body_end(bytes, body_start).ok_or_else(|| {
            PdfError::new(
                PdfErrorKind::Malformed,
                format!("PDF object {object_id} has no endobj marker"),
            )
        })?;
        objects.insert(
            (object_id, generation),
            PdfObject {
                body: bytes[body_start..body_end].to_vec(),
            },
        );
        cursor = body_end.saturating_add(b"endobj".len());
    }
    Ok(objects)
}

fn find_object_header(bytes: &[u8], mut cursor: usize) -> Option<(u32, u16, usize)> {
    while cursor < bytes.len() {
        if bytes[cursor].is_ascii_digit() && (cursor == 0 || is_pdf_whitespace(bytes[cursor - 1])) {
            if let Some((object, after_object)) = parse_u32(bytes, cursor) {
                let after_object = skip_ws(bytes, after_object);
                if let Some((generation, after_generation)) = parse_u32(bytes, after_object) {
                    let after_generation = skip_ws(bytes, after_generation);
                    if keyword_at(bytes, after_generation, b"obj")
                        && let Ok(generation) = u16::try_from(generation)
                    {
                        return Some((
                            object,
                            generation,
                            skip_ws(bytes, after_generation + b"obj".len()),
                        ));
                    }
                }
            }
        }
        cursor += 1;
    }
    None
}

fn object_body_end(bytes: &[u8], body_start: usize) -> Option<usize> {
    let first_endobj = find_keyword(bytes, b"endobj", body_start)?;
    let stream = find_keyword(bytes, b"stream", body_start);
    if stream.is_none_or(|stream| stream > first_endobj) {
        return Some(first_endobj);
    }
    let endstream = find_keyword(bytes, b"endstream", stream? + b"stream".len())?;
    find_keyword(bytes, b"endobj", endstream + b"endstream".len())
}

fn ordered_pages(objects: &BTreeMap<(u32, u16), PdfObject>) -> Vec<(u32, u16)> {
    let page_set: BTreeSet<_> = objects
        .iter()
        .filter_map(|(id, object)| has_name_value(&object.body, b"Type", b"Page").then_some(*id))
        .collect();
    let catalog_pages = objects.values().find_map(|object| {
        has_name_value(&object.body, b"Type", b"Catalog")
            .then(|| {
                references_after_key(&object.body, b"Pages")
                    .into_iter()
                    .next()
            })
            .flatten()
    });
    let mut ordered = Vec::new();
    let mut visiting = BTreeSet::new();
    if let Some(root) = catalog_pages {
        walk_page_tree(objects, root, &page_set, &mut visiting, &mut ordered);
        return ordered;
    }
    Vec::new()
}

fn walk_page_tree(
    objects: &BTreeMap<(u32, u16), PdfObject>,
    id: (u32, u16),
    pages: &BTreeSet<(u32, u16)>,
    visiting: &mut BTreeSet<(u32, u16)>,
    ordered: &mut Vec<(u32, u16)>,
) {
    if !visiting.insert(id) {
        return;
    }
    if pages.contains(&id) {
        ordered.push(id);
    } else if let Some(object) = objects.get(&id) {
        for kid in references_after_key(&object.body, b"Kids") {
            walk_page_tree(objects, kid, pages, visiting, ordered);
        }
    }
}

fn decode_stream(body: &[u8]) -> Result<Vec<u8>, PdfError> {
    let stream = find_keyword(body, b"stream", 0).ok_or_else(|| {
        PdfError::new(
            PdfErrorKind::Malformed,
            "PDF content object is not a stream",
        )
    })?;
    let data_start = stream_data_start(body, stream + b"stream".len());
    let endstream = find_keyword(body, b"endstream", data_start).ok_or_else(|| {
        PdfError::new(
            PdfErrorKind::Malformed,
            "PDF content stream has no endstream marker",
        )
    })?;
    let data_end = direct_length(body)
        .and_then(|length| data_start.checked_add(length))
        .filter(|end| *end <= endstream)
        .unwrap_or(endstream);
    let data = &body[data_start..data_end];
    let filters = names_after_key(body, b"Filter");
    if filters.is_empty() {
        if data.len() > MAX_PDF_STREAM_BYTES {
            return Err(PdfError::new(
                PdfErrorKind::DecompressionLimit,
                "PDF page content exceeds the bounded decode limit",
            ));
        }
        return Ok(data.to_vec());
    }
    if filters.len() != 1 || !matches!(filters[0].as_slice(), b"FlateDecode" | b"Fl") {
        return Err(PdfError::new(
            PdfErrorKind::Unsupported,
            "PDF uses an unsupported content-stream filter",
        ));
    }
    let mut decoded = Vec::new();
    ZlibDecoder::new(data)
        .take((MAX_PDF_STREAM_BYTES + 1) as u64)
        .read_to_end(&mut decoded)
        .map_err(|_| {
            PdfError::new(
                PdfErrorKind::Malformed,
                "PDF has a malformed compressed content stream",
            )
        })?;
    if decoded.len() > MAX_PDF_STREAM_BYTES {
        return Err(PdfError::new(
            PdfErrorKind::DecompressionLimit,
            "PDF page content exceeds the bounded decode limit",
        ));
    }
    Ok(decoded)
}

fn extract_content_text(content: &[u8], max_chars: usize) -> (String, bool) {
    let mut cursor = 0;
    let mut operands = Vec::<Vec<u8>>::new();
    let mut output = String::new();
    let mut truncated = false;
    while cursor < content.len() {
        cursor = skip_content_ws(content, cursor);
        if cursor >= content.len() {
            break;
        }
        match content[cursor] {
            b'%' => {
                cursor = content[cursor..]
                    .iter()
                    .position(|byte| matches!(byte, b'\r' | b'\n'))
                    .map_or(content.len(), |offset| cursor + offset + 1);
            }
            b'(' => {
                let (value, next) = parse_literal_string(content, cursor);
                operands.push(value);
                cursor = next;
            }
            b'<' if content.get(cursor + 1) != Some(&b'<') => {
                let (value, next) = parse_hex_string(content, cursor);
                operands.push(value);
                cursor = next;
            }
            byte if is_operator_byte(byte) => {
                let start = cursor;
                while cursor < content.len() && is_operator_byte(content[cursor]) {
                    cursor += 1;
                }
                let operator = &content[start..cursor];
                match operator {
                    b"Tj" | b"TJ" | b"'" | b"\"" => {
                        for operand in operands.drain(..) {
                            truncated |= push_chars_bounded(
                                &mut output,
                                &decode_pdf_string(&operand),
                                max_chars,
                            );
                        }
                        if matches!(operator, b"'" | b"\"") {
                            push_newline_bounded(&mut output, max_chars);
                        }
                    }
                    b"Td" | b"TD" | b"T*" | b"ET" => {
                        operands.clear();
                        push_newline_bounded(&mut output, max_chars);
                    }
                    _ => operands.clear(),
                }
            }
            _ => cursor += 1,
        }
    }
    let output = output.trim().to_owned();
    (output, truncated)
}

fn parse_literal_string(bytes: &[u8], start: usize) -> (Vec<u8>, usize) {
    let mut output = Vec::new();
    let mut cursor = start + 1;
    let mut depth = 1_u32;
    while cursor < bytes.len() && depth > 0 {
        match bytes[cursor] {
            b'\\' => {
                cursor += 1;
                if cursor >= bytes.len() {
                    break;
                }
                match bytes[cursor] {
                    b'n' => output.push(b'\n'),
                    b'r' => output.push(b'\r'),
                    b't' => output.push(b'\t'),
                    b'b' => output.push(8),
                    b'f' => output.push(12),
                    b'\r' => {
                        if bytes.get(cursor + 1) == Some(&b'\n') {
                            cursor += 1;
                        }
                    }
                    b'\n' => {}
                    digit @ b'0'..=b'7' => {
                        let mut value = u16::from(digit - b'0');
                        let mut digits = 1;
                        while digits < 3
                            && bytes
                                .get(cursor + 1)
                                .is_some_and(|byte| matches!(byte, b'0'..=b'7'))
                        {
                            cursor += 1;
                            value = value * 8 + u16::from(bytes[cursor] - b'0');
                            digits += 1;
                        }
                        output.push(u8::try_from(value & 0xff).unwrap_or_default());
                    }
                    escaped => output.push(escaped),
                }
            }
            b'(' => {
                depth += 1;
                output.push(b'(');
            }
            b')' => {
                depth -= 1;
                if depth > 0 {
                    output.push(b')');
                }
            }
            byte => output.push(byte),
        }
        cursor += 1;
    }
    (output, cursor)
}

fn parse_hex_string(bytes: &[u8], start: usize) -> (Vec<u8>, usize) {
    let mut nibbles = Vec::new();
    let mut cursor = start + 1;
    while cursor < bytes.len() && bytes[cursor] != b'>' {
        if let Some(value) = hex_value(bytes[cursor]) {
            nibbles.push(value);
        }
        cursor += 1;
    }
    if nibbles.len() % 2 == 1 {
        nibbles.push(0);
    }
    let output = nibbles
        .chunks_exact(2)
        .map(|pair| pair[0] * 16 + pair[1])
        .collect();
    (output, cursor.saturating_add(1))
}

fn decode_pdf_string(bytes: &[u8]) -> String {
    if bytes.starts_with(&[0xfe, 0xff]) {
        return String::from_utf16_lossy(
            &bytes[2..]
                .chunks_exact(2)
                .map(|pair| u16::from_be_bytes([pair[0], pair[1]]))
                .collect::<Vec<_>>(),
        );
    }
    bytes
        .iter()
        .map(|byte| match *byte {
            b'\t' | b'\n' | b'\r' => char::from(*byte),
            0x20..=0x7e => char::from(*byte),
            0x80..=0xff => char::from_u32(u32::from(*byte)).unwrap_or('\u{fffd}'),
            _ => ' ',
        })
        .collect()
}

fn has_name_value(body: &[u8], key: &[u8], value: &[u8]) -> bool {
    let mut search_from = 0;
    while let Some(key_at) = find_name(body, key, search_from) {
        let cursor = skip_ws(body, key_at + key.len() + 1);
        if body.get(cursor) == Some(&b'/')
            && body
                .get(cursor + 1..cursor + 1 + value.len())
                .is_some_and(|candidate| candidate == value)
            && body
                .get(cursor + 1 + value.len())
                .is_none_or(|byte| is_pdf_delimiter(*byte))
        {
            return true;
        }
        search_from = key_at + key.len() + 1;
    }
    false
}

fn references_after_key(body: &[u8], key: &[u8]) -> Vec<(u32, u16)> {
    let Some(key_at) = find_name(body, key, 0) else {
        return Vec::new();
    };
    let mut cursor = skip_ws(body, key_at + key.len() + 1);
    let array = body.get(cursor) == Some(&b'[');
    if array {
        cursor += 1;
    }
    let end = if array {
        body[cursor..]
            .iter()
            .position(|byte| *byte == b']')
            .map_or(body.len(), |offset| cursor + offset)
    } else {
        body.len().min(cursor.saturating_add(64))
    };
    let mut refs = Vec::new();
    while cursor < end {
        cursor = skip_ws(body, cursor);
        let Some((object, after_object)) = parse_u32(body, cursor) else {
            cursor += 1;
            continue;
        };
        let after_object = skip_ws(body, after_object);
        let Some((generation, after_generation)) = parse_u32(body, after_object) else {
            cursor = after_object.saturating_add(1);
            continue;
        };
        let after_generation = skip_ws(body, after_generation);
        if keyword_at(body, after_generation, b"R")
            && let Ok(generation) = u16::try_from(generation)
        {
            refs.push((object, generation));
            cursor = after_generation + 1;
            if !array {
                break;
            }
        } else {
            cursor = after_generation.saturating_add(1);
        }
    }
    refs
}

fn names_after_key(body: &[u8], key: &[u8]) -> Vec<Vec<u8>> {
    let Some(key_at) = find_name(body, key, 0) else {
        return Vec::new();
    };
    let mut cursor = skip_ws(body, key_at + key.len() + 1);
    let array = body.get(cursor) == Some(&b'[');
    if array {
        cursor += 1;
    }
    let mut names = Vec::new();
    loop {
        cursor = skip_ws(body, cursor);
        if body.get(cursor) != Some(&b'/') {
            break;
        }
        let start = cursor + 1;
        cursor = start;
        while cursor < body.len() && !is_pdf_delimiter(body[cursor]) {
            cursor += 1;
        }
        names.push(body[start..cursor].to_vec());
        if !array || body.get(skip_ws(body, cursor)) == Some(&b']') {
            break;
        }
    }
    names
}

fn direct_length(body: &[u8]) -> Option<usize> {
    let key_at = find_name(body, b"Length", 0)?;
    let cursor = skip_ws(body, key_at + b"/Length".len());
    let (length, after) = parse_u32(body, cursor)?;
    let after = skip_ws(body, after);
    (!keyword_at(body, after, b"R"))
        .then(|| usize::try_from(length).ok())
        .flatten()
}

fn find_name(bytes: &[u8], name: &[u8], mut cursor: usize) -> Option<usize> {
    while cursor + name.len() < bytes.len() {
        if bytes[cursor] == b'/'
            && bytes.get(cursor + 1..cursor + 1 + name.len()) == Some(name)
            && bytes
                .get(cursor + 1 + name.len())
                .is_none_or(|byte| is_pdf_delimiter(*byte))
        {
            return Some(cursor);
        }
        cursor += 1;
    }
    None
}

fn find_keyword(bytes: &[u8], keyword: &[u8], mut cursor: usize) -> Option<usize> {
    while cursor + keyword.len() <= bytes.len() {
        if keyword_at(bytes, cursor, keyword) {
            return Some(cursor);
        }
        cursor += 1;
    }
    None
}

fn keyword_at(bytes: &[u8], cursor: usize, keyword: &[u8]) -> bool {
    bytes.get(cursor..cursor + keyword.len()) == Some(keyword)
        && (cursor == 0 || is_pdf_delimiter(bytes[cursor - 1]))
        && bytes
            .get(cursor + keyword.len())
            .is_none_or(|byte| is_pdf_delimiter(*byte))
}

fn parse_u32(bytes: &[u8], mut cursor: usize) -> Option<(u32, usize)> {
    let start = cursor;
    let mut value = 0_u32;
    while cursor < bytes.len() && bytes[cursor].is_ascii_digit() {
        value = value
            .checked_mul(10)?
            .checked_add(u32::from(bytes[cursor] - b'0'))?;
        cursor += 1;
    }
    (cursor > start).then_some((value, cursor))
}

fn skip_ws(bytes: &[u8], mut cursor: usize) -> usize {
    loop {
        while cursor < bytes.len() && is_pdf_whitespace(bytes[cursor]) {
            cursor += 1;
        }
        if bytes.get(cursor) != Some(&b'%') {
            return cursor;
        }
        while cursor < bytes.len() && !matches!(bytes[cursor], b'\r' | b'\n') {
            cursor += 1;
        }
    }
}

fn skip_content_ws(bytes: &[u8], cursor: usize) -> usize {
    let mut cursor = cursor;
    while cursor < bytes.len()
        && (is_pdf_whitespace(bytes[cursor]) || matches!(bytes[cursor], b'[' | b']'))
    {
        cursor += 1;
    }
    cursor
}

fn stream_data_start(body: &[u8], mut cursor: usize) -> usize {
    if body.get(cursor) == Some(&b'\r') {
        cursor += 1;
    }
    if body.get(cursor) == Some(&b'\n') {
        cursor += 1;
    }
    cursor
}

fn is_pdf_whitespace(byte: u8) -> bool {
    matches!(byte, 0 | b'\t' | b'\n' | 0x0c | b'\r' | b' ')
}

fn is_pdf_delimiter(byte: u8) -> bool {
    is_pdf_whitespace(byte)
        || matches!(
            byte,
            b'(' | b')' | b'<' | b'>' | b'[' | b']' | b'{' | b'}' | b'/' | b'%'
        )
}

fn is_operator_byte(byte: u8) -> bool {
    byte.is_ascii_alphabetic() || matches!(byte, b'*' | b'\'' | b'\"')
}

fn hex_value(byte: u8) -> Option<u8> {
    match byte {
        b'0'..=b'9' => Some(byte - b'0'),
        b'a'..=b'f' => Some(byte - b'a' + 10),
        b'A'..=b'F' => Some(byte - b'A' + 10),
        _ => None,
    }
}

fn push_newline_bounded(output: &mut String, max_chars: usize) {
    if output.chars().count() < max_chars && !output.is_empty() && !output.ends_with('\n') {
        output.push('\n');
    }
}

fn push_chars_bounded(output: &mut String, value: &str, max_chars: usize) -> bool {
    let room = max_chars.saturating_sub(output.chars().count());
    output.extend(value.chars().take(room));
    value.chars().count() > room
}

fn take_chars(value: &str, count: usize) -> String {
    value.chars().take(count).collect()
}

/// Crate marker used by the workspace self-test.
pub const CRATE_NAME: &str = "haider-pdf";

#[cfg(test)]
mod tests {
    use super::*;

    fn fixture(contents: &[&str], encrypted: bool) -> Vec<u8> {
        let page_ids: Vec<_> = (0..contents.len()).map(|index| 3 + index as u32).collect();
        let content_ids: Vec<_> = (0..contents.len())
            .map(|index| 3 + contents.len() as u32 + index as u32)
            .collect();
        let mut pdf =
            String::from("%PDF-1.4\n1 0 obj\n<< /Type /Catalog /Pages 2 0 R >>\nendobj\n");
        pdf.push_str(&format!(
            "2 0 obj\n<< /Type /Pages /Count {} /Kids [{}] >>\nendobj\n",
            contents.len(),
            page_ids
                .iter()
                .map(|id| format!("{id} 0 R"))
                .collect::<Vec<_>>()
                .join(" ")
        ));
        for (page, content) in page_ids.iter().zip(&content_ids) {
            pdf.push_str(&format!(
                "{page} 0 obj\n<< /Type /Page /Parent 2 0 R /Contents {content} 0 R >>\nendobj\n"
            ));
        }
        for (content_id, content) in content_ids.iter().zip(contents) {
            pdf.push_str(&format!(
                "{content_id} 0 obj\n<< /Length {} >>\nstream\n{content}\nendstream\nendobj\n",
                content.len() + 1
            ));
        }
        if encrypted {
            pdf.push_str("trailer\n<< /Root 1 0 R /Encrypt 99 0 R >>\n");
        } else {
            pdf.push_str("trailer\n<< /Root 1 0 R >>\n");
        }
        pdf.push_str("%%EOF\n");
        pdf.into_bytes()
    }

    #[test]
    fn page_tree_order_and_text_operators_are_extracted() {
        let pdf = fixture(
            &["BT (first page) Tj ET", "BT [(second) 10 ( page)] TJ ET"],
            false,
        );
        assert_eq!(
            inspect_pdf(&pdf),
            Ok(PdfMetadata {
                pages: 2,
                encrypted: false
            })
        );
        let extracted = extract_text_bounded(&pdf).expect("text extracts");
        assert_eq!(extracted.pages_extracted, 2);
        assert!(extracted.text.contains("first page"));
        assert!(extracted.text.contains("second page"));
        assert!(!extracted.truncated);
    }

    #[test]
    fn image_only_and_encrypted_pdfs_fail_actionably() {
        let image_only = fixture(&["q 100 0 0 100 0 0 cm /Im0 Do Q"], false);
        let error = extract_text_bounded(&image_only).expect_err("image-only fails");
        assert_eq!(error.kind, PdfErrorKind::NoExtractableText);
        assert!(error.message.contains("scanned images"));

        let encrypted = fixture(&["BT (secret) Tj ET"], true);
        assert!(inspect_pdf(&encrypted).expect("metadata").encrypted);
        let error = extract_text_bounded(&encrypted).expect_err("encrypted fails");
        assert_eq!(error.kind, PdfErrorKind::Encrypted);
        assert!(error.message.contains("remove the password"));
    }

    #[test]
    fn per_page_bound_emits_the_exact_honest_marker() {
        let text = "x".repeat(MAX_PDF_PAGE_TEXT_CHARS + 50);
        let operator = format!("BT ({text}) Tj ET");
        let pdf = fixture(&[&operator, "BT (unreached) Tj ET"], false);
        let extracted = extract_text_bounded(&pdf).expect("bounded extraction");
        assert!(extracted.truncated);
        assert_eq!(extracted.pages_extracted, 1);
        assert!(extracted.text.ends_with("[pdf truncated: 1 of 2 pages]"));
        assert!(extracted.text.chars().count() <= MAX_PDF_TOTAL_TEXT_CHARS);
        assert!(!extracted.text.contains("unreached"));
    }

    #[test]
    fn aggregate_bound_emits_the_exact_honest_marker() {
        let text = "x".repeat(45_000);
        let operators = (0..5)
            .map(|_| format!("BT ({text}) Tj ET"))
            .collect::<Vec<_>>();
        let contents = operators.iter().map(String::as_str).collect::<Vec<_>>();
        let pdf = fixture(&contents, false);
        let extracted = extract_text_bounded(&pdf).expect("bounded extraction");

        assert!(extracted.truncated);
        assert_eq!(extracted.pages_extracted, 5);
        assert!(extracted.text.ends_with("[pdf truncated: 5 of 5 pages]"));
        assert!(extracted.text.chars().count() <= MAX_PDF_TOTAL_TEXT_CHARS);
    }

    #[test]
    fn malformed_non_pdf_is_rejected() {
        let error = inspect_pdf(b"not a pdf").expect_err("invalid");
        assert_eq!(error.kind, PdfErrorKind::Malformed);
        assert!(error.message.contains("%PDF"));
    }

    #[test]
    fn numeric_comments_between_objects_do_not_hide_later_pages() {
        let mut pdf = fixture(&["BT (visible) Tj ET"], false);
        let insertion = pdf
            .windows(b"2 0 obj".len())
            .position(|window| window == b"2 0 obj")
            .expect("page tree object");
        pdf.splice(insertion..insertion, b"% generated 2026\n".iter().copied());

        assert_eq!(inspect_pdf(&pdf).expect("commented PDF").pages, 1);
        assert!(
            extract_text_bounded(&pdf)
                .expect("commented PDF extracts")
                .text
                .contains("visible")
        );
    }
}
