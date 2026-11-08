//! Paired repeated-search benchmark over sampled pages from the local PDF corpus.
//!
//! Build first, then run under scripts/bench-run and the shared bench lock:
//! `cargo run --release -p pdfcraft-render --example search_corpus_bench --locked -- report.json`
use craft_bench::{Config, Paired, report::Revs};
use pdfcraft_render::{PageRenderer, RenderConfig, text::fold_case};
use std::{
    ops::Range,
    path::{Path, PathBuf},
    sync::Arc,
};

struct Input {
    page: pdfcraft_render::PageText,
    queries: Vec<(String, bool, bool)>,
}

fn reference(page: &pdfcraft_render::PageText, query: &str, sensitive: bool, whole: bool) -> Vec<Range<usize>> {
    let fold = |s: &str| if sensitive { s.to_owned() } else { fold_case(s) };
    let needle: Vec<char> = fold(query).split_whitespace().collect::<Vec<_>>().join(" ").chars().collect();
    if needle.is_empty() {
        return Vec::new();
    }
    let mut chars = Vec::new();
    for (i, glyph) in page.glyphs.iter().enumerate() {
        let line_break = i.checked_sub(1).and_then(|j| page.line_of.get(j)).zip(page.line_of.get(i)).is_some_and(|(a, b)| a != b);
        if i > 0 && (page.space_before.get(i).copied().unwrap_or(false) || line_break) && chars.last().is_some_and(|&(c, _)| c != ' ') {
            chars.push((' ', i));
        }
        for c in fold(&glyph.text).chars() {
            let c = if c.is_whitespace() { ' ' } else { c };
            if c != ' ' || chars.last().is_none_or(|&(last, _)| last != ' ') {
                chars.push((c, i));
            }
        }
    }
    if needle.len() > chars.len() {
        return Vec::new();
    }
    let mut prefix = vec![0; needle.len()];
    let mut matched = 0;
    for (i, &c) in needle.iter().enumerate().skip(1) {
        matched = advance(&needle, &prefix, matched, c);
        if let Some(slot) = prefix.get_mut(i) {
            *slot = matched;
        }
    }
    matched = 0;
    let word = |i: Option<usize>| i.and_then(|i| chars.get(i)).is_some_and(|&(c, _)| c.is_alphanumeric());
    let mut hits = Vec::new();
    for (i, &(c, owner)) in chars.iter().enumerate() {
        matched = advance(&needle, &prefix, matched, c);
        if matched != needle.len() {
            continue;
        }
        let end = i.saturating_add(1);
        let start = end.saturating_sub(needle.len());
        if !whole || (!word(start.checked_sub(1)) && !word(Some(end))) {
            if let (Some((_, first)), Some(last)) = (chars.get(start), owner.checked_add(1)) {
                hits.push(*first..last);
            }
            matched = 0;
        } else {
            matched = prefix.last().copied().unwrap_or_default();
        }
    }
    hits
}

fn advance(needle: &[char], prefix: &[usize], mut matched: usize, c: char) -> usize {
    while matched > 0 && needle.get(matched) != Some(&c) {
        matched = prefix.get(matched - 1).copied().unwrap_or_default();
    }
    if needle.get(matched) == Some(&c) { matched.saturating_add(1) } else { 0 }
}

fn search_old(input: &Input) -> Vec<Vec<Range<usize>>> {
    input.queries.iter().map(|(q, sensitive, whole)| reference(&input.page, q, *sensitive, *whole)).collect()
}

fn search_new(input: &Input) -> Vec<Vec<Range<usize>>> {
    input.queries.iter().map(|(q, sensitive, whole)| input.page.find_opts(q, *sensitive, *whole)).collect()
}

fn collect_pdfs(dir: &Path, out: &mut Vec<PathBuf>) -> std::io::Result<()> {
    for entry in std::fs::read_dir(dir)? {
        let entry = entry?;
        let path = entry.path();
        if path.is_dir() {
            collect_pdfs(&path, out)?;
        } else if path.extension().and_then(|ext| ext.to_str()).is_some_and(|ext| ext.eq_ignore_ascii_case("pdf")) {
            out.push(path);
        }
    }
    Ok(())
}

fn main() -> Result<(), Box<dyn std::error::Error>> {
    let corpus = std::env::var_os("ARTCRAFT_CORPUS").map(PathBuf::from).ok_or("ARTCRAFT_CORPUS must point to hyper/corpus")?;
    let out = std::env::args_os().nth(1).map(PathBuf::from).unwrap_or_else(|| PathBuf::from("pdfcraft-search.json"));
    let mut files = Vec::new();
    collect_pdfs(&corpus, &mut files)?;
    files.sort();
    let mut inputs = Vec::new();
    for path in files {
        let bytes = std::fs::read(&path)?;
        if bytes.len() < 100_000 {
            continue;
        }
        let mut renderer = PageRenderer::new(Arc::new(bytes), RenderConfig::default());
        let count = renderer.page_count();
        if count == 0 {
            continue;
        }
        let indices = [0, count / 4, count / 2, count.saturating_mul(3) / 4, count - 1];
        for page_index in indices.into_iter().collect::<std::collections::BTreeSet<_>>() {
            let Ok(page) = renderer.filtered_text(page_index, Default::default()) else { continue };
            if page.glyphs.is_empty() {
                continue;
            }
            let words: Vec<_> = page.plain_text().split_whitespace().map(str::to_owned).filter(|w| w.chars().count() >= 3).collect();
            let queries = (0..8)
                .map(|i| {
                    let word = words.get((i * 37) % words.len().max(1)).cloned().unwrap_or_else(|| "pdfcraft-missing-query".into());
                    (word, i % 2 == 0, i % 3 == 0)
                })
                .collect();
            let name = format!("{}#page-{}", path.strip_prefix(&corpus).unwrap_or(&path).display(), page_index + 1);
            inputs.push((name, Input { page, queries }));
        }
    }
    if inputs.is_empty() {
        return Err(format!("no searchable PDFs found under {}", corpus.display()).into());
    }
    let old_rev = std::env::var("PDFCRAFT_SEARCH_BASE_REV").unwrap_or_else(|_| "legacy-kmp".into());
    let new_rev = std::env::var("PDFCRAFT_SEARCH_HEAD_REV").unwrap_or_else(|_| "cached-craft-text".into());
    let report = Paired::new("pdfcraft repeated page search, corpus pages")
        .old(search_old)
        .new(search_new)
        .inputs(inputs)
        .equal()
        .run(&Config { revs: Revs { old: Some(old_rev), new: Some(new_rev) }, ..Config::default() })?;
    std::fs::write(&out, report.to_json())?;
    println!("{}", report.to_markdown());
    report.require_publishable()?;
    Ok(())
}
