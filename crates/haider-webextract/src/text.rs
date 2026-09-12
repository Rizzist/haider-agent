//! Entity decoding, whitespace normalization, and minimal URL resolution.

/// Decodes the common named entities plus numeric (`&#…;`/`&#x…;`) forms.
/// Unknown or malformed candidates stay literal. The terminator scan walks
/// char boundaries only, so multibyte input can never split a codepoint.
pub(crate) fn decode_entities(text: &str) -> String {
    let mut output = String::with_capacity(text.len());
    let mut rest = text;
    while let Some(position) = rest.find('&') {
        output.push_str(&rest[..position]);
        rest = &rest[position..];
        let Some(end) = rest
            .char_indices()
            .take_while(|(index, _)| *index < 12)
            .find(|(_, character)| *character == ';')
            .map(|(index, _)| index)
        else {
            output.push('&');
            rest = &rest[1..];
            continue;
        };
        let entity = &rest[1..end];
        match decode_entity(entity) {
            Some(decoded) => {
                output.push(decoded);
                rest = &rest[end + 1..];
            }
            None => {
                output.push('&');
                rest = &rest[1..];
            }
        }
    }
    output.push_str(rest);
    output
}

fn decode_entity(entity: &str) -> Option<char> {
    match entity {
        "amp" => Some('&'),
        "lt" => Some('<'),
        "gt" => Some('>'),
        "quot" => Some('"'),
        "apos" => Some('\''),
        "nbsp" => Some(' '),
        "mdash" => Some('—'),
        "ndash" => Some('–'),
        "hellip" => Some('…'),
        "lsquo" => Some('\u{2018}'),
        "rsquo" => Some('\u{2019}'),
        "ldquo" => Some('\u{201C}'),
        "rdquo" => Some('\u{201D}'),
        "laquo" => Some('«'),
        "raquo" => Some('»'),
        "copy" => Some('©'),
        "reg" => Some('®'),
        "trade" => Some('™'),
        "sect" => Some('§'),
        "middot" => Some('·'),
        "bull" => Some('•'),
        "times" => Some('×'),
        "deg" => Some('°'),
        _ => entity
            .strip_prefix('#')
            .and_then(|number| {
                number.strip_prefix(['x', 'X']).map_or_else(
                    || number.parse::<u32>().ok(),
                    |hex| u32::from_str_radix(hex, 16).ok(),
                )
            })
            .and_then(char::from_u32),
    }
}

/// Collapses ASCII/Unicode whitespace runs to single spaces and trims.
pub(crate) fn normalize_whitespace(text: &str) -> String {
    let mut output = String::with_capacity(text.len());
    let mut pending_space = false;
    for character in text.chars() {
        if character.is_whitespace() {
            pending_space = !output.is_empty();
        } else {
            if pending_space {
                output.push(' ');
                pending_space = false;
            }
            output.push(character);
        }
    }
    output
}

/// Resolves `href` against `base` well enough for markdown links: absolute
/// http(s) passes through, protocol-relative/rooted/path-relative resolve
/// against the base origin, and non-navigable schemes return `None`.
pub(crate) fn resolve_url(base: &str, href: &str) -> Option<String> {
    let href = href.trim();
    if href.is_empty() || href.starts_with('#') {
        return None;
    }
    let lower = href.to_ascii_lowercase();
    if lower.starts_with("javascript:")
        || lower.starts_with("data:")
        || lower.starts_with("mailto:")
        || lower.starts_with("tel:")
        || lower.starts_with("vbscript:")
        || lower.starts_with("blob:")
        || lower.starts_with("file:")
        || lower.starts_with("about:")
    {
        return None;
    }
    if lower.starts_with("http://") || lower.starts_with("https://") {
        return Some(href.to_owned());
    }
    let scheme_end = base.find("://")?;
    let scheme = &base[..scheme_end];
    if scheme != "http" && scheme != "https" {
        return None;
    }
    let after_scheme = &base[scheme_end + 3..];
    let host_end = after_scheme
        .find(['/', '?', '#'])
        .unwrap_or(after_scheme.len());
    if after_scheme[..host_end].is_empty() {
        return None;
    }
    let origin = format!("{scheme}://{}", &after_scheme[..host_end]);
    if let Some(without_slashes) = href.strip_prefix("//") {
        if without_slashes.is_empty() {
            return None;
        }
        return Some(format!("{scheme}://{without_slashes}"));
    }
    if href.starts_with('/') {
        return Some(format!("{origin}{href}"));
    }
    if href.starts_with('?') {
        let path = after_scheme[host_end..]
            .split(['?', '#'])
            .next()
            .unwrap_or("");
        let path = if path.is_empty() { "/" } else { path };
        return Some(format!("{origin}{path}{href}"));
    }
    // Path-relative: resolve against the base path's directory. Dot-segment
    // normalization is deliberately skipped — links stay honest either way.
    let path = after_scheme[host_end..]
        .split(['?', '#'])
        .next()
        .unwrap_or("");
    let directory = path.rfind('/').map_or("/", |index| &path[..index + 1]);
    Some(format!("{origin}{directory}{href}"))
}
