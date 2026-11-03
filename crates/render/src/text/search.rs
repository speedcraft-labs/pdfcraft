//! Literal search in normalized page text, retaining the original glyph owners.
use super::{PageText, fold_case};
use std::ops::Range;
use std::sync::{Arc, Mutex, PoisonError};

/// The options a folded page form depends on: whether case is folded. Whitespace collapsing is
/// part of the form itself; whole-word matching reads the form but never changes it.
#[derive(Clone, Copy, PartialEq, Eq)]
pub(super) struct FoldOptions {
    pub(super) sensitive: bool,
}

/// A page's text in search form: normalized characters, each with the glyph that owns it.
pub(super) type FoldedPage = Vec<(char, usize)>;

/// How many bytes of folded forms one page keeps: 4 MiB is a quarter-million characters, past
/// any real page, while bounding what a hostile document can pin per page.
pub(super) const FOLD_BUDGET: usize = 4 * 1024 * 1024;

/// The folded search forms of one page, kept across queries (#128).
///
/// The cache lives in the `PageText` and dies with it: the renderer hands each page out behind
/// an `Arc` and replaces that entry when the page changes, so a rebuilt page starts cold. The
/// page's fields stay public, though, so every entry also carries a digest of everything the
/// fold reads (glyph texts, line indices, word gaps), recomputed on every lookup: a page edited
/// in place is simply refolded and can never be answered from a stale form. Entries are keyed by
/// the options they were built with and evicted least-recently-used first once the byte budget
/// would be exceeded. Search is not the realtime path, so a plain mutex is enough.
pub(super) struct FoldCache {
    inner: Mutex<Inner>,
}

struct Inner {
    budget: usize,
    bytes: usize,
    /// Bumped on every access; an entry remembers the stamp of its last hit.
    stamp: u32,
    /// One entry per distinct `FoldOptions`; only as long as the option combinations used.
    entries: Vec<Entry>,
    /// How many forms were folded, for the cache tests.
    builds: usize,
}

struct Entry {
    options: FoldOptions,
    fingerprint: u64,
    last_used: u32,
    folded: Arc<FoldedPage>,
}

impl Default for FoldCache {
    fn default() -> Self {
        Self::with_budget(FOLD_BUDGET)
    }
}

impl std::fmt::Debug for FoldCache {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let inner = self.lock();
        f.debug_struct("FoldCache").field("bytes", &inner.bytes).field("budget", &inner.budget).field("entries", &inner.entries.len()).finish()
    }
}

/// A word-wise FNV-style digest of everything [`fold_page`] reads: each glyph's text (with its
/// length, so glyph boundaries survive), the line indices and the word gaps. One pass over the
/// page, no allocation — an order of magnitude cheaper than the fold it guards.
fn fingerprint(page: &PageText) -> u64 {
    const PRIME: u64 = 0x100_0000_01b3;
    let mut hash: u64 = 0xcbf2_9ce4_8422_2325;
    let mix = |hash: &mut u64, bytes: &[u8]| {
        let (chunks, remainder) = bytes.as_chunks::<8>();
        for chunk in chunks {
            *hash = (*hash ^ u64::from_le_bytes(*chunk)).wrapping_mul(PRIME).rotate_left(17);
        }
        for &b in remainder {
            *hash = (*hash ^ u64::from(b)).wrapping_mul(PRIME);
        }
    };
    for glyph in &page.glyphs {
        mix(&mut hash, glyph.text.as_bytes());
        hash = (hash ^ glyph.text.len() as u64).wrapping_mul(PRIME);
    }
    mix(&mut hash, bytemuck::cast_slice(page.line_of.as_slice()));
    mix(&mut hash, bytemuck::cast_slice(page.space_before.as_slice()));
    hash ^ ((page.glyphs.len() as u64) << 32) ^ page.line_of.len() as u64 ^ page.space_before.len() as u64
}

impl FoldCache {
    pub(super) fn with_budget(budget: usize) -> Self {
        Self { inner: Mutex::new(Inner { budget, bytes: 0, stamp: 0, entries: Vec::new(), builds: 0 }) }
    }

    fn lock(&self) -> std::sync::MutexGuard<'_, Inner> {
        // Everything the guard protects is derived from the page, so a panicked builder leaves
        // nothing half-written behind and recovery is always sound.
        self.inner.lock().unwrap_or_else(PoisonError::into_inner)
    }

    #[cfg(test)]
    pub(super) fn builds(&self) -> usize {
        self.lock().builds
    }

    /// Bytes of folded forms currently kept. The budget counts the entries' character buffers.
    #[cfg(test)]
    pub(super) fn bytes(&self) -> usize {
        self.lock().bytes
    }

    /// The page's folded form for `options`, building (and caching) it when absent or stale. A
    /// form that alone exceeds the budget is still returned, just not kept.
    pub(super) fn lookup_or_build(&self, page: &PageText, options: FoldOptions) -> Arc<FoldedPage> {
        // The digest is taken before the lock: it is the only slow part of a hit.
        let fingerprint = fingerprint(page);
        let mut inner = self.lock();
        inner.stamp = inner.stamp.saturating_add(1);
        let stamp = inner.stamp;
        if let Some(entry) = inner.entries.iter_mut().find(|entry| entry.options == options && entry.fingerprint == fingerprint) {
            entry.last_used = stamp;
            return entry.folded.clone();
        }
        if let Some(stale) = inner.entries.iter().position(|entry| entry.options == options) {
            drop_stale(&mut inner, stale);
        }
        inner.builds = inner.builds.saturating_add(1);
        let folded = Arc::new(fold_page(page, options));
        let size = folded.len().saturating_mul(std::mem::size_of::<(char, usize)>());
        if size <= inner.budget {
            // `size` fits the budget and `bytes` never exceeds it, so the subtraction holds.
            while inner.bytes > inner.budget - size {
                let stalest = inner.entries.iter().enumerate().min_by_key(|(_, entry)| entry.last_used).map(|(i, _)| i);
                match stalest {
                    Some(i) => drop_stale(&mut inner, i),
                    None => break,
                }
            }
            inner.stamp = inner.stamp.saturating_add(1);
            let stamp = inner.stamp;
            inner.bytes += size;
            inner.entries.push(Entry { options, fingerprint, last_used: stamp, folded: folded.clone() });
        }
        folded
    }
}

/// Retire entry `i`, returning its bytes to the budget.
fn drop_stale(inner: &mut Inner, i: usize) {
    let entry = inner.entries.remove(i);
    inner.bytes = inner.bytes.saturating_sub(entry.folded.len().saturating_mul(std::mem::size_of::<(char, usize)>()));
}

/// The per-query fold of a page into normalized characters with their owning glyphs.
pub(super) fn fold_page(page: &PageText, options: FoldOptions) -> FoldedPage {
    let fold = |s: &str| if options.sensitive { s.to_owned() } else { fold_case(s) };
    let mut chars: Vec<(char, usize)> = Vec::new();
    for (i, glyph) in page.glyphs.iter().enumerate() {
        let line_break = i.checked_sub(1).and_then(|previous| page.line_of.get(previous)).zip(page.line_of.get(i)).is_some_and(|(a, b)| a != b);
        let gap = page.space_before.get(i).copied().unwrap_or(false) || line_break;
        if i > 0 && gap && chars.last().is_some_and(|c| c.0 != ' ') {
            chars.push((' ', i));
        }
        for c in fold(&glyph.text).chars() {
            let c = if c.is_whitespace() { ' ' } else { c };
            if c != ' ' || chars.last().is_none_or(|last| last.0 != ' ') {
                chars.push((c, i));
            }
        }
    }
    chars
}

pub(super) fn find(page: &PageText, query: &str, sensitive: bool, whole_words: bool, limit: usize) -> Vec<Range<usize>> {
    if limit == 0 {
        return Vec::new();
    }
    let fold = |s: &str| if sensitive { s.to_owned() } else { fold_case(s) };
    let needle: Vec<char> = fold(query).split_whitespace().collect::<Vec<_>>().join(" ").chars().collect();
    if needle.is_empty() {
        return Vec::new();
    }
    let chars = page.folded_form(sensitive);
    if needle.len() > chars.len() {
        return Vec::new();
    }
    // KMP reuses a matched prefix instead of comparing a long query again at every position.
    // The table is bounded by the page's already materialized character count.
    let mut prefix = vec![0; needle.len()];
    let mut matched = 0;
    for (i, &c) in needle.iter().enumerate().skip(1) {
        matched = advance(&needle, &prefix, matched, c);
        if let Some(slot) = prefix.get_mut(i) {
            *slot = matched;
        }
    }
    matched = 0;
    let word = |i: Option<usize>| i.and_then(|i| chars.get(i)).is_some_and(|c| c.0.is_alphanumeric());
    let mut hits = Vec::new();
    for (i, &(c, owner)) in chars.iter().enumerate() {
        matched = advance(&needle, &prefix, matched, c);
        if matched != needle.len() {
            continue;
        }
        let end = i.saturating_add(1);
        let start = end.saturating_sub(needle.len());
        let bounded = !whole_words || (!word(start.checked_sub(1)) && !word(Some(end)));
        if bounded {
            if let Some((_, first)) = chars.get(start)
                && let Some(last) = owner.checked_add(1)
            {
                hits.push(*first..last);
                if hits.len() == limit {
                    break;
                }
            }
            // Only accepted hits consume their full span: literal search is non-overlapping.
            matched = 0;
        } else {
            // A word-boundary rejection must still allow an overlapping later candidate.
            matched = prefix.last().copied().unwrap_or_default();
        }
    }
    hits
}

/// Extend a prefix, falling back through strictly shorter proper prefixes on a mismatch.
fn advance(needle: &[char], prefix: &[usize], mut matched: usize, c: char) -> usize {
    while matched > 0 && needle.get(matched) != Some(&c) {
        matched = prefix.get(matched - 1).copied().unwrap_or_default();
    }
    if needle.get(matched) == Some(&c) { matched.saturating_add(1) } else { 0 }
}
