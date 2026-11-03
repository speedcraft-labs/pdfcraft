//! Synthetic search microbenchmark, including normalization and glyph-range mapping.
//! Run with `cargo run -p pdfcraft-render --example search_bench --locked` on both revisions
//! with the same toolchain/profile. Timings are diagnostic, never a CI deadline.
use pdfcraft_render::{PageText, TextGlyph, text::fold_case};
use std::{
    hint::black_box,
    time::{Duration, Instant},
};

fn stages(pages: &[PageText], query: &str) -> [Duration; 4] {
    let mut times = [Duration::ZERO; 4];
    for page in pages {
        let start = Instant::now();
        let glyphs: Vec<_> = page
            .glyphs
            .iter()
            .enumerate()
            .map(|(i, glyph)| {
                let line_break = i.checked_sub(1).and_then(|p| page.line_of.get(p)).zip(page.line_of.get(i)).is_some_and(|(a, b)| a != b);
                (i, glyph.text.as_str(), page.space_before.get(i).copied().unwrap_or(false) || line_break)
            })
            .collect();
        times[0] += start.elapsed();
        let start = Instant::now();
        let mut chars = Vec::new();
        for (i, glyph, gap) in glyphs {
            if i > 0 && gap && chars.last().is_some_and(|(c, _)| *c != ' ') {
                chars.push((' ', i));
            }
            for c in fold_case(glyph).chars() {
                let c = if c.is_whitespace() { ' ' } else { c };
                if c != ' ' || chars.last().is_none_or(|(last, _)| *last != ' ') {
                    chars.push((c, i));
                }
            }
        }
        let needle: Vec<char> = fold_case(query).split_whitespace().collect::<Vec<_>>().join(" ").chars().collect();
        times[1] += start.elapsed();
        let start = Instant::now();
        let mut prefix = vec![0; needle.len()];
        let mut matched = 0;
        for (i, &c) in needle.iter().enumerate().skip(1) {
            matched = advance(&needle, &prefix, matched, c);
            prefix[i] = matched;
        }
        matched = 0;
        let mut offsets = Vec::new();
        for (i, &(c, _)) in chars.iter().enumerate() {
            matched = advance(&needle, &prefix, matched, c);
            if matched == needle.len() {
                offsets.push((i + 1 - needle.len(), i));
                matched = 0;
            }
        }
        times[2] += start.elapsed();
        let start = Instant::now();
        let hits: Vec<_> = offsets.into_iter().map(|(start, end)| chars[start].1..chars[end].1 + 1).collect();
        black_box(hits);
        times[3] += start.elapsed();
    }
    times
}

fn advance(needle: &[char], prefix: &[usize], mut matched: usize, c: char) -> usize {
    while matched > 0 && needle.get(matched) != Some(&c) {
        matched = prefix[matched - 1];
    }
    if needle.get(matched) == Some(&c) { matched + 1 } else { 0 }
}

fn main() {
    let text = "The quick brown fox jumps over the lazy dog. ".repeat(100);
    let glyphs: Vec<_> =
        text.chars().map(|c| TextGlyph { text: c.to_string(), rect: [0.0; 4], quad: [[0.0; 2]; 4], direction: [1.0, 0.0] }).collect();
    let n = glyphs.len();
    let pages: Vec<_> = (0..300)
        .map(|_| {
            let mut page = PageText::default();
            page.glyphs = glyphs.clone();
            page.line_of = vec![0; n];
            page.space_before = vec![false; n];
            page
        })
        .collect();
    for pass in ["first", "repeat"] {
        let start = Instant::now();
        for page in &pages {
            black_box(page.find(black_box("brown fox")));
        }
        println!("300 synthetic pages, {pass}: {:.3} ms", start.elapsed().as_secs_f64() * 1000.0);
    }
    let timings = stages(&pages, "brown fox");
    println!(
        "stages (synthetic, ms): extraction {:.3}, folding {:.3}, matching {:.3}, mapping {:.3}",
        timings[0].as_secs_f64() * 1000.0,
        timings[1].as_secs_f64() * 1000.0,
        timings[2].as_secs_f64() * 1000.0,
        timings[3].as_secs_f64() * 1000.0
    );
    for (name, text, query) in [
        ("repeated prefix", "a".repeat(131072), format!("{}b", "a".repeat(4095))),
        ("ordinary", "The quick brown fox jumps over the lazy dog. ".repeat(3000), "brown fox".into()),
        ("unicode", "İSTANBUL κόσμος Straße العربية 日本語 ".repeat(3000), "istanbul".into()),
    ] {
        let glyphs: Vec<_> =
            text.chars().map(|c| TextGlyph { text: c.to_string(), rect: [0.0; 4], quad: [[0.0; 2]; 4], direction: [1.0, 0.0] }).collect();
        let n = glyphs.len();
        let mut page = PageText::default();
        page.glyphs = glyphs;
        page.line_of = vec![0; n];
        page.space_before = vec![false; n];
        black_box(page.find(&query));
        let mut times = Vec::new();
        for _ in 0..7 {
            let start = Instant::now();
            black_box(page.find(black_box(&query)));
            times.push(start.elapsed().as_secs_f64() * 1000.0);
        }
        times.sort_by(f64::total_cmp);
        if let Some(median) = times.get(3) {
            println!("{name}: median {median:.3} ms (7 runs, {n} glyphs)");
        }
    }
}
