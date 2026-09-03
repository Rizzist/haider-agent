//! Shared append-only storage for streamed assistant text.
//!
//! A turn owns one [`ReplyArena`]. Provider deltas are moved into immutable
//! chunks and every downstream representation carries a [`ReplyText`] byte
//! range into that arena. Cloning a range never clones reply bytes.

use bytes::Bytes;
use serde::{Deserialize, Deserializer, Serialize, Serializer};
use std::fmt;
use std::hash::{Hash, Hasher};
use std::io;
use std::ops::Range;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, OnceLock, RwLock};

#[derive(Debug)]
struct ReplyChunk {
    start: usize,
    bytes: Bytes,
}

#[derive(Debug, Default)]
struct ReplyArenaInner {
    chunks: RwLock<Vec<ReplyChunk>>,
    len: AtomicUsize,
    digest: OnceLock<blake3::Hash>,
    provider_views: OnceLock<Vec<IncrementalJsonViewDigest>>,
}

#[derive(Debug)]
struct IncrementalJsonViewDigest {
    prefix: Vec<u8>,
    hasher_before_suffix: blake3::Hasher,
    byte_len_before_suffix: u64,
}

#[derive(Debug)]
struct IncrementalJsonViewHasher {
    prefix: Vec<u8>,
    hasher: blake3::Hasher,
    byte_len: u64,
}

/// Shared storage retained by reply-range handles.
#[derive(Clone, Debug, Default)]
pub struct ReplyArena {
    inner: Arc<ReplyArenaInner>,
}

impl ReplyArena {
    /// Returns a handle over every byte appended so far.
    #[must_use]
    pub fn snapshot(&self) -> ReplyText {
        ReplyText {
            arena: self.clone(),
            range: 0..self.len(),
        }
    }

    #[must_use]
    pub fn len(&self) -> usize {
        self.inner.len.load(Ordering::Acquire)
    }

    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }

    #[must_use]
    pub fn chunk_count(&self) -> usize {
        self.inner
            .chunks
            .read()
            .unwrap_or_else(|error| error.into_inner())
            .len()
    }

    /// Creates a checked sub-range. Both endpoints must be UTF-8 boundaries.
    #[must_use]
    pub fn slice(&self, range: Range<usize>) -> Option<ReplyText> {
        let len = self.len();
        if range.start > range.end || range.end > len {
            return None;
        }
        let chunks = self
            .inner
            .chunks
            .read()
            .unwrap_or_else(|error| error.into_inner());
        if !is_char_boundary(&chunks, range.start, len)
            || !is_char_boundary(&chunks, range.end, len)
        {
            return None;
        }
        Some(ReplyText {
            arena: self.clone(),
            range,
        })
    }
}

/// Unique append authority for a reply arena. It is intentionally not
/// cloneable: consumers receive only [`ReplyText`] ranges.
#[derive(Debug)]
pub struct ReplyArenaWriter {
    arena: ReplyArena,
    hasher: blake3::Hasher,
    sealed: bool,
    provider_views: Vec<IncrementalJsonViewHasher>,
}

impl Default for ReplyArenaWriter {
    fn default() -> Self {
        Self::new()
    }
}

impl ReplyArenaWriter {
    #[must_use]
    pub fn new() -> Self {
        Self {
            arena: ReplyArena::default(),
            hasher: blake3::Hasher::new(),
            sealed: false,
            provider_views: Vec::new(),
        }
    }

    /// Starts the exact provider-view block hash before the first reply
    /// delta. `prefix` ends immediately before the JSON string scalar. The
    /// final `suffix` argument documents the expected simple shape; the
    /// sealed hash state can also accept a structurally richer suffix chosen
    /// by the provider view once tool/native fields are known.
    #[must_use]
    pub fn with_incremental_json_view(mut self, prefix: &[u8], _suffix: &[u8]) -> Self {
        self.add_incremental_json_view_prefix(prefix);
        self
    }

    /// Seeds the three canonical assistant-message prefixes used by durable
    /// replay. This lets journal deltas rebuild a provider-neutral arena once
    /// while retaining exact candidate hash states for Anthropic/OpenAI,
    /// Chat-compatible, and Gemini views.
    #[must_use]
    pub fn with_standard_provider_json_views(mut self) -> Self {
        for prefix in [
            br#"{"content":[{"text":"#.as_slice(),
            br#"{"content":"#.as_slice(),
            br#"{"parts":[{"text":"#.as_slice(),
        ] {
            self.add_incremental_json_view_prefix(prefix);
        }
        self
    }

    fn add_incremental_json_view_prefix(&mut self, prefix: &[u8]) {
        if self.provider_views.iter().any(|view| view.prefix == prefix) {
            return;
        }
        let mut hasher = blake3::Hasher::new();
        hasher.update(prefix);
        hasher.update(b"\"");
        self.provider_views.push(IncrementalJsonViewHasher {
            prefix: prefix.to_vec(),
            hasher,
            byte_len: u64::try_from(prefix.len())
                .unwrap_or(u64::MAX)
                .saturating_add(1),
        });
    }

    /// Moves one UTF-8 provider delta into the arena and returns its range.
    #[must_use]
    pub fn append(&mut self, text: String) -> ReplyText {
        assert!(!self.sealed, "cannot append to a sealed reply arena");
        self.append_bytes(Bytes::from(text))
    }

    /// Appends an existing shared range by adopting its immutable chunk
    /// allocations. This is used by durable replay to rebuild one logical
    /// arena from independently decoded delta records without copying bytes.
    #[must_use]
    pub fn append_shared(&mut self, text: &ReplyText) -> ReplyText {
        let start = self.len();
        for segment in text.segments() {
            let _ = self.append_bytes(segment);
        }
        ReplyText {
            arena: self.arena.clone(),
            range: start..self.len(),
        }
    }

    fn append_bytes(&mut self, bytes: Bytes) -> ReplyText {
        assert!(!self.sealed, "cannot append to a sealed reply arena");
        self.hasher.update(&bytes);
        if let Ok(text) = std::str::from_utf8(&bytes) {
            for view in &mut self.provider_views {
                if let Err(error) =
                    hash_json_string_contents(&mut view.hasher, &mut view.byte_len, text)
                {
                    debug_assert!(false, "UTF-8 reply scalar must serialize: {error}");
                }
            }
        } else {
            debug_assert!(false, "reply chunks must remain valid UTF-8");
        }
        let mut chunks = self
            .arena
            .inner
            .chunks
            .write()
            .unwrap_or_else(|error| error.into_inner());
        let start = chunks
            .last()
            .map_or(0, |chunk| chunk.start.saturating_add(chunk.bytes.len()));
        assert!(
            bytes.len() <= usize::MAX.saturating_sub(start),
            "reply arena byte length exhausted usize"
        );
        let end = start + bytes.len();
        if !bytes.is_empty() {
            chunks.push(ReplyChunk { start, bytes });
        }
        self.arena.inner.len.store(end, Ordering::Release);
        ReplyText {
            arena: self.arena.clone(),
            range: start..end,
        }
    }

    #[must_use]
    pub fn snapshot(&self) -> ReplyText {
        self.arena.snapshot()
    }

    #[must_use]
    pub fn len(&self) -> usize {
        self.arena.len()
    }

    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }

    /// Freezes the incremental content hash and consumes append authority.
    #[must_use]
    pub fn seal(mut self) -> ReplyText {
        self.sealed = true;
        let _ = self.arena.inner.digest.set(self.hasher.finalize());
        let mut provider_views = Vec::with_capacity(self.provider_views.len());
        for mut view in self.provider_views {
            view.hasher.update(b"\"");
            view.byte_len = view.byte_len.saturating_add(1);
            provider_views.push(IncrementalJsonViewDigest {
                prefix: view.prefix,
                hasher_before_suffix: view.hasher,
                byte_len_before_suffix: view.byte_len,
            });
        }
        if !provider_views.is_empty() {
            let _ = self.arena.inner.provider_views.set(provider_views);
        }
        self.arena.snapshot()
    }
}

/// Feeds a compact JSON string scalar into BLAKE3 without retaining either
/// quote. Holding one pending byte lets this stay independent of how serde
/// partitions its writes while allocating no reply-sized escape buffer.
fn hash_json_string_contents(
    hasher: &mut blake3::Hasher,
    byte_len: &mut u64,
    text: &str,
) -> io::Result<()> {
    struct ContentsWriter<'a> {
        hasher: &'a mut blake3::Hasher,
        byte_len: &'a mut u64,
        opened: bool,
        pending: Option<u8>,
    }

    impl io::Write for ContentsWriter<'_> {
        fn write(&mut self, bytes: &[u8]) -> io::Result<usize> {
            let original_len = bytes.len();
            if bytes.is_empty() {
                return Ok(0);
            }
            let bytes = if self.opened {
                bytes
            } else {
                let Some((b'"', contents)) = bytes.split_first() else {
                    return Err(io::Error::other(
                        "JSON string serializer omitted its opening quote",
                    ));
                };
                self.opened = true;
                contents
            };
            if bytes.is_empty() {
                return Ok(original_len);
            }
            if let Some(pending) = self.pending.take() {
                self.hasher.update(&[pending]);
                *self.byte_len = self.byte_len.saturating_add(1);
            }
            if bytes.len() > 1 {
                let contents = &bytes[..bytes.len() - 1];
                self.hasher.update(contents);
                *self.byte_len = self
                    .byte_len
                    .saturating_add(u64::try_from(contents.len()).unwrap_or(u64::MAX));
            }
            self.pending = bytes.last().copied();
            Ok(original_len)
        }

        fn flush(&mut self) -> io::Result<()> {
            Ok(())
        }
    }

    let mut writer = ContentsWriter {
        hasher,
        byte_len,
        opened: false,
        pending: None,
    };
    serde_json::to_writer(&mut writer, text).map_err(io::Error::other)?;
    if writer.opened && writer.pending == Some(b'"') {
        Ok(())
    } else {
        Err(io::Error::other(
            "JSON string serializer omitted its closing quote",
        ))
    }
}

fn is_char_boundary(chunks: &[ReplyChunk], offset: usize, len: usize) -> bool {
    if offset == 0 || offset == len {
        return true;
    }
    chunks.iter().find_map(|chunk| {
        let end = chunk.start.saturating_add(chunk.bytes.len());
        (chunk.start <= offset && offset <= end).then(|| {
            let local = offset.saturating_sub(chunk.start);
            std::str::from_utf8(&chunk.bytes)
                .ok()
                .is_some_and(|text| text.is_char_boundary(local))
        })
    }) == Some(true)
}

/// A shared byte range into a [`ReplyArena`].
#[derive(Clone)]
pub struct ReplyText {
    arena: ReplyArena,
    range: Range<usize>,
}

impl ReplyText {
    #[must_use]
    pub fn len(&self) -> usize {
        self.range.len()
    }

    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.range.is_empty()
    }

    #[must_use]
    pub fn is_blank(&self) -> bool {
        let mut blank = true;
        self.visit_strs(|segment| {
            if blank && segment.chars().any(|character| !character.is_whitespace()) {
                blank = false;
            }
        });
        blank
    }

    #[must_use]
    pub fn char_count(&self) -> usize {
        let mut count = 0_usize;
        self.visit_strs(|segment| count = count.saturating_add(segment.chars().count()));
        count
    }

    #[must_use]
    pub fn starts_with(&self, prefix: &str) -> bool {
        prefix.len() <= self.len()
            && self
                .segments()
                .iter()
                .flat_map(|segment| segment.iter())
                .copied()
                .take(prefix.len())
                .eq(prefix.bytes())
    }

    #[must_use]
    pub fn ends_with(&self, suffix: &str) -> bool {
        if suffix.len() > self.len() {
            return false;
        }
        self.slice(self.len() - suffix.len()..self.len())
            .is_some_and(|tail| tail.starts_with(suffix))
    }

    /// Searches across provider chunk boundaries without joining the reply.
    #[must_use]
    pub fn contains(&self, needle: &str) -> bool {
        if needle.is_empty() {
            return true;
        }
        if needle.len() > self.len() {
            return false;
        }
        let pattern = needle.as_bytes();
        let mut prefix = vec![0_usize; pattern.len()];
        let mut matched = 0_usize;
        for index in 1..pattern.len() {
            while matched > 0 && pattern[index] != pattern[matched] {
                matched = prefix[matched - 1];
            }
            if pattern[index] == pattern[matched] {
                matched += 1;
            }
            prefix[index] = matched;
        }
        matched = 0;
        for byte in self.segments().iter().flat_map(|segment| segment.iter()) {
            while matched > 0 && *byte != pattern[matched] {
                matched = prefix[matched - 1];
            }
            if *byte == pattern[matched] {
                matched += 1;
                if matched == pattern.len() {
                    return true;
                }
            }
        }
        false
    }

    #[must_use]
    pub fn byte_range(&self) -> Range<usize> {
        self.range.clone()
    }

    /// Creates a UTF-8-aligned range relative to this handle.
    #[must_use]
    pub fn slice(&self, range: Range<usize>) -> Option<Self> {
        if range.start > range.end || range.end > self.len() {
            return None;
        }
        self.arena.slice(
            self.range.start.saturating_add(range.start)
                ..self.range.start.saturating_add(range.end),
        )
    }

    #[must_use]
    pub fn shares_arena_with(&self, other: &Self) -> bool {
        Arc::ptr_eq(&self.arena.inner, &other.arena.inner)
    }

    #[must_use]
    pub fn is_prefix_of(&self, other: &Self) -> bool {
        self.len() <= other.len()
            && self
                .segments()
                .iter()
                .flat_map(|segment| segment.iter())
                .copied()
                .eq(other
                    .segments()
                    .iter()
                    .flat_map(|segment| segment.iter())
                    .copied()
                    .take(self.len()))
    }

    /// Joins adjacent ranges from the same arena without copying bytes.
    #[must_use]
    pub fn try_join(&self, next: &Self) -> Option<Self> {
        (self.shares_arena_with(next) && self.range.end == next.range.start).then(|| Self {
            arena: self.arena.clone(),
            range: self.range.start..next.range.end,
        })
    }

    /// Incremental BLAKE3 of the complete arena, available after the append
    /// authority has sealed it. Sub-ranges deliberately do not invent hashes.
    #[must_use]
    pub fn arena_digest(&self) -> Option<blake3::Hash> {
        self.arena.inner.digest.get().copied()
    }

    /// Returns a provider-view block address that was fed incrementally as
    /// the arena received deltas. Only the complete arena range can carry the
    /// address. The prefix must match a state seeded before the first delta;
    /// the now-known structural suffix is appended to a clone of that state.
    #[must_use]
    pub fn incremental_json_view(
        &self,
        prefix: &[u8],
        suffix: &[u8],
    ) -> Option<(blake3::Hash, u64)> {
        if self.range != (0..self.arena.len()) {
            return None;
        }
        let view = self
            .arena
            .inner
            .provider_views
            .get()?
            .iter()
            .find(|view| view.prefix == prefix)?;
        let mut hasher = view.hasher_before_suffix.clone();
        hasher.update(suffix);
        Some((
            hasher.finalize(),
            view.byte_len_before_suffix
                .saturating_add(u64::try_from(suffix.len()).unwrap_or(u64::MAX)),
        ))
    }

    /// Whether this complete range originated in a writer that was fed as a
    /// streamed provider reply. Provider-view construction uses this bit to
    /// distinguish ordinary prompt scalars (which have no delta-time hash)
    /// from replies that must never fall back to a finalization-time pass.
    #[must_use]
    pub fn has_incremental_json_views(&self) -> bool {
        self.range == (0..self.arena.len())
            && self
                .arena
                .inner
                .provider_views
                .get()
                .is_some_and(|views| !views.is_empty())
    }

    /// Returns immutable byte slices for this range. `Bytes::slice` shares
    /// chunk storage; only the small vector and slice headers are allocated.
    #[must_use]
    pub fn segments(&self) -> Vec<Bytes> {
        if self.range.is_empty() {
            return Vec::new();
        }
        let chunks = self
            .arena
            .inner
            .chunks
            .read()
            .unwrap_or_else(|error| error.into_inner());
        chunks
            .iter()
            .filter_map(|chunk| {
                let chunk_end = chunk.start.saturating_add(chunk.bytes.len());
                let start = self.range.start.max(chunk.start);
                let end = self.range.end.min(chunk_end);
                (start < end).then(|| {
                    chunk
                        .bytes
                        .slice(start.saturating_sub(chunk.start)..end.saturating_sub(chunk.start))
                })
            })
            .collect()
    }

    /// Calls `visit` once per UTF-8 segment without joining the reply.
    pub fn visit_strs(&self, mut visit: impl FnMut(&str)) {
        // Snapshot cheap Bytes slice headers first. User callbacks and I/O
        // must never hold the arena's read lock and stall the append owner.
        for segment in self.segments() {
            if let Ok(segment) = std::str::from_utf8(&segment) {
                visit(segment);
            } else {
                debug_assert!(false, "reply range must remain UTF-8 aligned");
            }
        }
    }

    /// Returns the range as a single `Bytes` view when it occupies one chunk.
    #[must_use]
    pub fn contiguous_bytes(&self) -> Option<Bytes> {
        let mut segments = self.segments();
        (segments.len() <= 1).then(|| segments.pop().unwrap_or_default())
    }

    /// Copies into an owned compatibility string. Durable and provider write
    /// paths should prefer [`Self::visit_strs`] or [`Self::write_to`].
    #[must_use]
    pub fn to_owned_string(&self) -> String {
        let mut text = String::with_capacity(self.len());
        self.visit_strs(|segment| text.push_str(segment));
        text
    }

    /// Copies at most `max_chars` Unicode scalar values for bounded UI or
    /// delivery previews. The boolean reports whether content was omitted.
    #[must_use]
    pub fn to_owned_prefix(&self, max_chars: usize) -> (String, bool) {
        let mut text = String::new();
        let mut chars = 0_usize;
        let mut truncated = false;
        self.visit_strs(|segment| {
            if truncated {
                return;
            }
            for character in segment.chars() {
                if chars == max_chars {
                    truncated = true;
                    break;
                }
                text.push(character);
                chars = chars.saturating_add(1);
            }
        });
        (text, truncated)
    }

    pub fn write_to(&self, writer: &mut impl io::Write) -> io::Result<()> {
        let mut result = Ok(());
        self.visit_strs(|segment| {
            if result.is_ok() {
                result = writer.write_all(segment.as_bytes());
            }
        });
        result
    }

    /// Writes one canonical compact-JSON string scalar in bounded windows.
    pub fn write_json_string_to<W: io::Write + ?Sized>(&self, writer: &mut W) -> io::Result<()> {
        const WINDOW_BYTES: usize = 16 * 1_024;
        writer.write_all(b"\"")?;
        let mut result = Ok(());
        self.visit_strs(|segment| {
            let mut start = 0;
            while result.is_ok() && start < segment.len() {
                let mut end = start.saturating_add(WINDOW_BYTES).min(segment.len());
                while end > start && !segment.is_char_boundary(end) {
                    end -= 1;
                }
                match serde_json::to_vec(&segment[start..end]) {
                    Ok(encoded) => result = writer.write_all(&encoded[1..encoded.len() - 1]),
                    Err(error) => result = Err(io::Error::other(error)),
                }
                start = end;
            }
        });
        result?;
        writer.write_all(b"\"")
    }
}

impl Default for ReplyText {
    fn default() -> Self {
        ReplyArena::default().snapshot()
    }
}

impl From<String> for ReplyText {
    fn from(text: String) -> Self {
        let mut writer = ReplyArenaWriter::new();
        let _ = writer.append(text);
        writer.seal()
    }
}

impl From<&str> for ReplyText {
    fn from(text: &str) -> Self {
        Self::from(text.to_owned())
    }
}

impl From<ReplyText> for String {
    fn from(text: ReplyText) -> Self {
        text.to_owned_string()
    }
}

impl fmt::Debug for ReplyText {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str("\"")?;
        let mut result = Ok(());
        self.visit_strs(|segment| {
            if result.is_ok() {
                result = formatter.write_str(segment);
            }
        });
        result?;
        formatter.write_str("\"")
    }
}

impl fmt::Display for ReplyText {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        let mut result = Ok(());
        self.visit_strs(|segment| {
            if result.is_ok() {
                result = formatter.write_str(segment);
            }
        });
        result
    }
}

impl PartialEq for ReplyText {
    fn eq(&self, other: &Self) -> bool {
        if self.len() != other.len() {
            return false;
        }
        if self.shares_arena_with(other) && self.range == other.range {
            return true;
        }
        segments_equal(&self.segments(), &other.segments())
    }
}

impl Eq for ReplyText {}

impl PartialEq<str> for ReplyText {
    fn eq(&self, other: &str) -> bool {
        self.len() == other.len()
            && self
                .segments()
                .iter()
                .flat_map(|segment| segment.iter())
                .copied()
                .eq(other.bytes())
    }
}

fn segments_equal(left: &[Bytes], right: &[Bytes]) -> bool {
    let mut left = left.iter().flat_map(|segment| segment.iter());
    let mut right = right.iter().flat_map(|segment| segment.iter());
    left.by_ref().eq(right.by_ref())
}

impl PartialEq<&str> for ReplyText {
    fn eq(&self, other: &&str) -> bool {
        self == *other
    }
}

impl PartialEq<String> for ReplyText {
    fn eq(&self, other: &String) -> bool {
        self == other.as_str()
    }
}

impl Hash for ReplyText {
    fn hash<H: Hasher>(&self, state: &mut H) {
        // `Hasher::write(a); write(b)` is not required to equal
        // `Hasher::write(a + b)`. Feed a stable byte stream so equal replies
        // hash identically regardless of provider delta boundaries.
        self.visit_strs(|segment| {
            for byte in segment.bytes() {
                state.write_u8(byte);
            }
        });
    }
}

impl Serialize for ReplyText {
    fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: Serializer,
    {
        if let Some(bytes) = self.contiguous_bytes() {
            let text = std::str::from_utf8(&bytes).map_err(serde::ser::Error::custom)?;
            return serializer.serialize_str(text);
        }
        serializer.serialize_str(&self.to_owned_string())
    }
}

impl<'de> Deserialize<'de> for ReplyText {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        String::deserialize(deserializer).map(Self::from)
    }
}

#[cfg(test)]
#[allow(clippy::expect_used, clippy::unwrap_used)]
mod tests {
    use super::{ReplyArenaWriter, ReplyText};
    use std::sync::Arc;

    #[test]
    fn ranges_share_append_only_chunks_and_keep_utf8_boundaries() {
        let mut writer = ReplyArenaWriter::new();
        let first = writer.append("hel".to_owned());
        let second = writer.append("lo 🌍".to_owned());
        let arena = first.arena.clone();
        let whole = writer.seal();

        assert_eq!(first, "hel");
        assert_eq!(second, "lo 🌍");
        assert_eq!(whole, "hello 🌍");
        assert!(first.shares_arena_with(&whole));
        assert!(second.shares_arena_with(&whole));
        assert_eq!(whole.byte_range(), 0..10);
        assert!(
            arena.slice(0..9).is_none(),
            "must reject a split code point"
        );
        assert_eq!(arena.chunk_count(), 2);
    }

    #[test]
    fn legacy_json_and_messagepack_remain_string_scalars() {
        let mut writer = ReplyArenaWriter::new();
        let _ = writer.append("a\n".to_owned());
        let _ = writer.append("b".to_owned());
        let text = writer.seal();

        assert_eq!(serde_json::to_vec(&text).expect("json"), br#""a\nb""#);
        let encoded = rmp_serde::to_vec_named(&text).expect("messagepack");
        assert_eq!(
            rmp_serde::from_slice::<String>(&encoded).expect("legacy scalar"),
            "a\nb"
        );
        assert_eq!(
            serde_json::from_slice::<ReplyText>(br#""old journal""#).expect("decode"),
            "old journal"
        );
    }

    #[test]
    fn one_mib_reply_has_one_chunk_allocation_and_releases_with_last_handle() {
        let mut writer = ReplyArenaWriter::new();
        let delta = writer.append("x".repeat(1024 * 1024));
        let whole = writer.seal();
        let consumer = whole.clone();
        let weak = Arc::downgrade(&whole.arena.inner);

        assert!(delta.shares_arena_with(&whole));
        assert!(consumer.shares_arena_with(&whole));
        assert_eq!(whole.arena.chunk_count(), 1);
        assert_eq!(
            delta.contiguous_bytes().expect("delta bytes").as_ptr(),
            whole.contiguous_bytes().expect("whole bytes").as_ptr()
        );

        drop(delta);
        drop(consumer);
        assert!(
            weak.upgrade().is_some(),
            "the final handle still owns the arena"
        );
        drop(whole);
        assert!(
            weak.upgrade().is_none(),
            "the last handle releases the reply allocation"
        );
    }
}
