//! Streaming journal boundary. Delay incomplete lines so credentials split
//! across OS reads are classified together, with PEM and bounded quoted-secret state across lines.

#[derive(Clone, Debug, Default)]
pub struct OutputRedactor {
    pending: Vec<u8>,
    state: crate::redact::RedactionState,
    discarding_line: bool,
}

impl OutputRedactor {
    pub fn push(&mut self, bytes: &[u8]) -> String {
        String::from_utf8_lossy(&self.push_bytes(bytes)).into_owned()
    }

    /// Journal transport preserves non-secret bytes, including invalid UTF-8.
    pub fn push_bytes(&mut self, bytes: &[u8]) -> Vec<u8> {
        let mut output = Vec::new();
        for segment in bytes.split_inclusive(|byte| *byte == b'\n') {
            let ends = segment.ends_with(b"\n");
            if !self.discarding_line {
                if self.pending.len().saturating_add(segment.len())
                    > crate::PROCESS_MAX_OUTPUT_BYTES
                {
                    // Classify the bounded prefix before discarding it so an
                    // open quote cannot recover on a later line or PEM marker.
                    let remaining = crate::PROCESS_MAX_OUTPUT_BYTES - self.pending.len();
                    self.pending.extend_from_slice(&segment[..remaining]);
                    let _ = self.finish_bytes();
                    self.state.discard_oversized_line();
                    self.discarding_line = true;
                    output.extend_from_slice(b"[REDACTED:oversized_output_line]\n");
                } else {
                    self.pending.extend_from_slice(segment);
                }
            }
            if ends {
                if !self.discarding_line {
                    output.extend_from_slice(&self.finish_bytes());
                }
                self.discarding_line = false;
            }
        }
        output
    }

    pub fn finish(&mut self) -> String {
        String::from_utf8_lossy(&self.finish_bytes()).into_owned()
    }

    pub fn finish_bytes(&mut self) -> Vec<u8> {
        let bytes = std::mem::take(&mut self.pending);
        if bytes.is_empty() {
            return bytes;
        }
        let line = String::from_utf8_lossy(&bytes);
        let (text, newline) = line
            .strip_suffix('\n')
            .map_or((line.as_ref(), ""), |text| (text, "\n"));
        // Classify the same visible text the process adapter will see. ANSI
        // styling inside a token must not hide its prefix until after this
        // security boundary. Control sequences can themselves contain secrets,
        // so only ANSI-free, non-secret lines retain their original bytes.
        let plain = crate::shell::strip_ansi(text);
        // A hidden PEM delimiter still controls subsequent visible lines.
        let classified = if text.contains("PRIVATE KEY-----")
            && (text.contains("-----BEGIN") || text.contains("-----END"))
        {
            text
        } else {
            &plain
        };
        let redacted = crate::redact::redact_line_with_state(
            &format!("{classified}{newline}"),
            &mut self.state,
        );
        if redacted.replacements == 0 && plain == text {
            bytes
        } else {
            crate::shell::strip_ansi(&redacted.text).into_bytes()
        }
    }
}

/// Matches the journal's per-stream, complete-line redaction. Interleaving
/// stderr inside a stdout token must not break classification of that token.
pub fn redact_process_output(chunks: &[crate::ProcessOutputChunk]) -> crate::ToolResult<String> {
    use base64::Engine as _;
    let mut redactors = [OutputRedactor::default(), OutputRedactor::default()];
    let mut output = String::new();
    for chunk in chunks {
        let bytes = base64::engine::general_purpose::STANDARD
            .decode(&chunk.chunk_b64)
            .map_err(|error| crate::ToolError::cas(format!("invalid process chunk: {error}")))?;
        let index = usize::from(chunk.stream == haider_protocol::item::OutputStream::Stderr);
        output.push_str(&redactors[index].push(&bytes));
    }
    for redactor in &mut redactors {
        output.push_str(&redactor.finish());
    }
    Ok(output)
}

#[cfg(test)]
#[path = "output_redactor_tests.rs"]
mod tests;
