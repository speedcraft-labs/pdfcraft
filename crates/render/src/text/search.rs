//! Literal search in normalized page text, retaining the original glyph owners.
use super::{PageText, fold_case};
use std::ops::Range;

pub(super) fn find(page: &PageText, query: &str, sensitive: bool, whole_words: bool, limit: usize) -> Vec<Range<usize>> {
    if limit == 0 {
        return Vec::new();
    }
    let fold = |s: &str| if sensitive { s.to_owned() } else { fold_case(s) };
    let needle: Vec<char> = fold(query).split_whitespace().collect::<Vec<_>>().join(" ").chars().collect();
    if needle.is_empty() {
        return Vec::new();
    }
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

/// Searches many literals in one page-text traversal. Results retain input pattern order and the
/// same per-pattern non-overlap and whole-word rules as repeated `find` calls.
pub(super) fn find_many(page: &PageText, queries: &[String], sensitive: bool, whole_words: bool) -> Vec<Vec<Range<usize>>> {
    use craft_text::matching::{find_literals_overlapping, overlapping_literal_matcher};

    if queries.len() == 1 {
        return vec![find(page, &queries[0], sensitive, whole_words, usize::MAX)];
    }
    let mut results = vec![Vec::new(); queries.len()];
    let fold = |s: &str| if sensitive { s.to_owned() } else { fold_case(s) };
    let mut needles = Vec::new();
    let mut indices = Vec::new();
    for (i, query) in queries.iter().enumerate() {
        let normalized = fold(query).split_whitespace().collect::<Vec<_>>().join(" ");
        if !normalized.is_empty() {
            needles.push(normalized);
            indices.push(i);
        }
    }
    if needles.is_empty() {
        return results;
    }

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
    let haystack: String = chars.iter().map(|(c, _)| c).collect();
    let refs: Vec<_> = needles.iter().map(String::as_str).collect();
    let Ok(matcher) = overlapping_literal_matcher(&refs, false) else { return results };
    let byte_starts: Vec<_> = haystack.char_indices().map(|(byte, _)| byte).chain(std::iter::once(haystack.len())).collect();
    let is_word = |i: usize| chars.get(i).is_some_and(|c| c.0.is_alphanumeric());
    let mut last_end = vec![0; needles.len()];
    let mut matched_once = vec![false; needles.len()];
    for hit in find_literals_overlapping(&matcher, &haystack) {
        let Ok(start) = byte_starts.binary_search(&hit.start) else { continue };
        let Ok(end) = byte_starts.binary_search(&hit.end) else { continue };
        if start >= end || end > chars.len() || (matched_once[hit.pattern] && start < last_end[hit.pattern]) {
            continue;
        }
        if whole_words && (start > 0 && is_word(start - 1) || end < chars.len() && is_word(end)) {
            continue;
        }
        let Some(first) = chars.get(start).map(|(_, owner)| *owner) else { continue };
        let Some(last) = chars.get(end - 1).map(|(_, owner)| owner.saturating_add(1)) else { continue };
        results[indices[hit.pattern]].push(first..last);
        last_end[hit.pattern] = end;
        matched_once[hit.pattern] = true;
    }
    results
}

/// Extend a prefix, falling back through strictly shorter proper prefixes on a mismatch.
fn advance(needle: &[char], prefix: &[usize], mut matched: usize, c: char) -> usize {
    while matched > 0 && needle.get(matched) != Some(&c) {
        matched = prefix.get(matched - 1).copied().unwrap_or_default();
    }
    if needle.get(matched) == Some(&c) { matched.saturating_add(1) } else { 0 }
}

#[cfg(test)]
mod tests {
    use super::super::TextGlyph;
    use super::*;
    use proptest::prelude::*;

    fn page(s: &str) -> PageText {
        let glyphs =
            s.chars().map(|c| TextGlyph { text: c.to_string(), rect: [0.0; 4], quad: [[0.0; 2]; 4], direction: [1.0, 0.0] }).collect::<Vec<_>>();
        let len = glyphs.len();
        PageText { glyphs, line_of: vec![0; len], space_before: vec![false; len] }
    }

    proptest! {
        #[test]
        fn multi_search_matches_repeated_search(text in "[a-c ]{0,80}", queries in prop::collection::vec("[a-c ]{0,10}", 1..8), sensitive in any::<bool>(), whole in any::<bool>()) {
            let page = page(&text);
            let actual = find_many(&page, &queries, sensitive, whole);
            let expected: Vec<_> = queries.iter().map(|q| find(&page, q, sensitive, whole, usize::MAX)).collect();
            prop_assert_eq!(actual, expected);
        }
    }
}
