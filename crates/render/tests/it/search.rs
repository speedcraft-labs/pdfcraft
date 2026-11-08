use pdfcraft_render::{PageText, TextGlyph, text::fold_case};
use std::ops::Range;

fn page(parts: &[&str]) -> PageText {
    let mut page = PageText::default();
    page.glyphs = parts.iter().map(|text| TextGlyph { text: (*text).into(), rect: [0.0; 4], quad: [[0.0; 2]; 4], direction: [1.0, 0.0] }).collect();
    page.line_of = vec![0; parts.len()];
    page.space_before = vec![false; parts.len()];
    page
}

// The original straightforward search is an independent oracle for valid page metadata.
fn reference(page: &PageText, query: &str, sensitive: bool, whole: bool, limit: usize) -> Vec<Range<usize>> {
    let fold = |s: &str| if sensitive { s.to_owned() } else { fold_case(s) };
    let needle: Vec<char> = fold(query).split_whitespace().collect::<Vec<_>>().join(" ").chars().collect();
    if needle.is_empty() || limit == 0 {
        return Vec::new();
    }
    let mut chars = Vec::new();
    for (i, glyph) in page.glyphs.iter().enumerate() {
        if i > 0 && (page.space_before[i] || page.line_of[i] != page.line_of[i - 1]) && chars.last().is_some_and(|&(c, _)| c != ' ') {
            chars.push((' ', i));
        }
        for c in fold(&glyph.text).chars() {
            let c = if c.is_whitespace() { ' ' } else { c };
            if c != ' ' || chars.last().is_none_or(|&(last, _)| last != ' ') {
                chars.push((c, i));
            }
        }
    }
    let word = |i: Option<usize>| i.and_then(|i| chars.get(i)).is_some_and(|&(c, _)| c.is_alphanumeric());
    let mut hits = Vec::new();
    let mut start = 0;
    while start + needle.len() <= chars.len() {
        let end = start + needle.len();
        if (!whole || (!word(start.checked_sub(1)) && !word(Some(end)))) && chars[start..end].iter().map(|&(c, _)| c).eq(needle.iter().copied()) {
            hits.push(chars[start].1..chars[end - 1].1 + 1);
            if hits.len() == limit {
                break;
            }
            start = end;
        } else {
            start += 1;
        }
    }
    hits
}

#[test]
fn search_tolerates_missing_layout_metadata() {
    let mut text = page(&["a", "b", "c"]);
    text.line_of.clear();
    text.space_before.clear();
    assert_eq!(text.find("abc"), vec![0..3]);
    text.space_before = vec![false, true];
    assert_eq!(text.find("a bc"), vec![0..3]);
    text.line_of = vec![0, 1];
    assert_eq!(text.find("a bc"), vec![0..3]);
}

#[test]
fn whole_word_rejection_keeps_overlapping_candidates() {
    let text = page(&["xaa-aa-aa"]);
    assert_eq!(text.find_opts("aa-aa", true, true), vec![0..1]);
    // Accepted matches still do not overlap, even within one multi-character glyph.
    assert_eq!(page(&["aaaaa"]).find("aaa"), vec![0..1]);
    assert_eq!(page(&["aaaaa"]).find("aa"), vec![0..1, 0..1]);
}

#[test]
fn search_matches_reference_for_unicode_gaps_limits_and_word_boundaries() {
    let alphabet = ["a", "b", " ", "-", "İ", "ı", "ς", "Σ", "\t", "\n", "ffi", "", "界", "e\u{301}", "العربية"];
    let mut seed = 1234567u64;
    let mut next = || {
        seed = seed.wrapping_mul(6364136223846793005).wrapping_add(1);
        (seed >> 32) as usize
    };
    for case in 0..2000 {
        let parts: Vec<_> = (0..10 + case % 70).map(|_| alphabet[next() % alphabet.len()]).collect();
        let query = if case % 2 == 0 {
            parts.iter().skip(case % 8).take(1 + case % 9).copied().collect::<String>()
        } else {
            (0..1 + case % 9).map(|_| alphabet[next() % alphabet.len()]).collect::<String>()
        };
        let mut text = page(&parts);
        for i in 1..parts.len() {
            text.space_before[i] = next() % 11 == 0;
            text.line_of[i] = text.line_of[i - 1] + u32::from(next() % 17 == 0);
        }
        for sensitive in [false, true] {
            for whole in [false, true] {
                for limit in [0, 1, 2, usize::MAX] {
                    assert_eq!(
                        text.find_opts_limited(&query, sensitive, whole, limit),
                        reference(&text, &query, sensitive, whole, limit),
                        "case {case}: {query:?}, sensitive={sensitive}, whole={whole}, limit={limit}"
                    );
                }
            }
        }
    }
}

#[test]
fn long_repeated_prefix_search_preserves_the_last_match_and_limit() {
    let mut parts = vec!["a"; 131072];
    parts.push("b");
    let text = page(&parts);
    let query = format!("{}b", "a".repeat(4095));
    assert_eq!(text.find_opts_limited(&query, false, false, 1), vec![126977..131073]);
    assert!(text.find(&format!("{}c", "a".repeat(4095))).is_empty());
}

/// Each query runs twice: the first call fills the page's fold cache (#128), the second reads
/// it. Both must report exactly the uncached oracle's hits, for every option combination.
#[test]
fn repeat_queries_report_the_first_search_s_hits_from_the_fold_cache() {
    let cases: &[(&[&str], &[u32], &[bool])] = &[
        (&["İstanbul", "IRMAK", "Straße"], &[0, 0, 0], &[false, true, true]),
        (&["under   state", "understate"], &[0, 0], &[false, true]),
        (&["brown", "fox", "jumps"], &[0, 1, 1], &[false, false, true]),
        (&[" ", "a", " ", " ", "b"], &[0, 0, 0, 0, 0], &[false, false, false, false, false]),
        (&["e\u{301}clair", "CAFE", "café"], &[0, 0, 1], &[false, false, false]),
    ];
    let queries = ["istanbul", "ırmak", "straße", "strasse", "under state", "brown fox", "a b", "café", "cafe", "éclair", "É", ""];
    for (parts, line_of, space_before) in cases {
        let mut text = page(parts);
        text.line_of = line_of.to_vec();
        text.space_before = space_before.to_vec();
        for query in queries {
            for sensitive in [false, true] {
                for whole in [false, true] {
                    for limit in [1, 3, usize::MAX] {
                        let (cold, expected) =
                            (text.find_opts_limited(query, sensitive, whole, limit), reference(&text, query, sensitive, whole, limit));
                        let cached = text.find_opts_limited(query, sensitive, whole, limit);
                        assert_eq!(cold, expected, "{query:?} on {:?}: first (cold) search", parts);
                        assert_eq!(cached, expected, "{query:?} on {:?}: repeat (cached) search", parts);
                    }
                }
            }
        }
    }
}

/// A real extraction: the same differential as above, over pages rendered from a PDF with
/// diacritics, drawn spaces, mixed case and a phrase that crosses a line break.
#[test]
fn cached_search_over_rendered_pdf_pages_matches_the_oracle() {
    use pdfcraft_render::{PageRenderer, RenderConfig, RenderRequest, RequestKind};
    use std::sync::Arc;

    fn stream(contents: &[u8]) -> Vec<u8> {
        let mut s = format!("<< /Length {} >>\nstream\n", contents.len()).into_bytes();
        s.extend_from_slice(contents);
        s.extend_from_slice(b"\nendstream");
        s
    }

    // WinAnsi octal escapes: É = \311, é = \351, ç = \347, ß = \337.
    let contents = [
        b"BT /F1 12 Tf 72 700 Td (Cafe CAFE caf\\351 \\311CLAIR \\351clair) Tj ET".to_vec(),
        b"BT /F1 12 Tf 72 700 Td (under   state) Tj 72 680 Td (understate Stra\\337e) Tj ET".to_vec(),
        b"BT /F1 12 Tf 72 700 Td (brown) Tj 72 680 Td (fox jumps) Tj ET".to_vec(),
    ];
    let n = contents.len();
    let kids: String = (0..n).map(|i| format!("{} 0 R ", 5 + 2 * i)).collect();
    let mut objects: Vec<Vec<u8>> = vec![
        b"<< /Type /Catalog /Pages 2 0 R >>".to_vec(),
        format!("<< /Type /Pages /Kids [{kids}] /Count {n} >>").into_bytes(),
        b"<< /Type /Font /Subtype /Type1 /BaseFont /Helvetica /Encoding /WinAnsiEncoding >>".to_vec(),
        b"<< /Font << /F1 3 0 R >> >>".to_vec(),
    ];
    for (i, content) in contents.iter().enumerate() {
        objects.push(format!("<< /Type /Page /Parent 2 0 R /MediaBox [0 0 400 800] /Resources 4 0 R /Contents {} 0 R >>", 6 + 2 * i).into_bytes());
        objects.push(stream(content));
    }
    let mut pdf = b"%PDF-1.7\n".to_vec();
    let mut offsets = Vec::new();
    for (i, object) in objects.iter().enumerate() {
        offsets.push(pdf.len());
        pdf.extend_from_slice(format!("{} 0 obj\n", i + 1).as_bytes());
        pdf.extend_from_slice(object);
        pdf.extend_from_slice(b"\nendobj\n");
    }
    let xref = pdf.len();
    pdf.extend_from_slice(format!("xref\n0 {}\n0000000000 65535 f \n", objects.len() + 1).as_bytes());
    for offset in offsets {
        pdf.extend_from_slice(format!("{offset:010} 00000000 n \n").as_bytes());
    }
    pdf.extend_from_slice(format!("trailer << /Size {} /Root 1 0 R >>\nstartxref\n{xref}\n%%EOF\n", objects.len() + 1).as_bytes());

    let mut renderer = PageRenderer::new(Arc::new(pdf), RenderConfig::default());
    let queries = ["café", "cafe", "éclair", "eclair", "under state", "understate", "straße", "strasse", "brown fox", "fox jumps", "CAFÉ"];
    for page in 0..n {
        let out = renderer.render(RenderRequest { page, kind: RequestKind::Text, tile: None, scale: 1.0, tag: 0, preview: false });
        assert!(out.error.is_none(), "page {} text error: {:?}", page + 1, out.error);
        let text = out.text.unwrap_or_default();
        assert!(!text.is_empty(), "page {} has text", page + 1);
        for query in queries {
            for sensitive in [false, true] {
                for whole in [false, true] {
                    for limit in [2, usize::MAX] {
                        let expected = reference(&text, query, sensitive, whole, limit);
                        assert_eq!(
                            text.find_opts_limited(query, sensitive, whole, limit),
                            expected,
                            "page {}, {query:?}, sensitive={sensitive}, whole={whole}, limit={limit}",
                            page + 1
                        );
                        assert_eq!(
                            text.find_opts_limited(query, sensitive, whole, limit),
                            expected,
                            "page {}, {query:?}: repeat (cached) search",
                            page + 1
                        );
                    }
                }
            }
        }
    }
}

#[test]
fn search_hits_match_the_previous_algorithm_on_every_corpus_pdf_page() {
    use pdfcraft_render::{PageRenderer, RenderConfig};
    use std::{
        path::{Path, PathBuf},
        sync::Arc,
    };

    let Ok(corpus) = std::env::var("ARTCRAFT_CORPUS") else { return };
    fn pdfs(dir: &Path, out: &mut Vec<PathBuf>) {
        let Ok(entries) = std::fs::read_dir(dir) else { return };
        for entry in entries.flatten() {
            let path = entry.path();
            if path.is_dir() {
                pdfs(&path, out);
            } else if path.extension().and_then(|ext| ext.to_str()).is_some_and(|ext| ext.eq_ignore_ascii_case("pdf")) {
                out.push(path);
            }
        }
    }

    let corpus = PathBuf::from(corpus);
    let mut files = Vec::new();
    pdfs(&corpus, &mut files);
    files.sort();
    assert!(!files.is_empty(), "no PDF files in {}", corpus.display());
    for file in files {
        let Ok(bytes) = std::fs::read(&file) else { panic!("failed to read {}", file.display()) };
        let mut renderer = PageRenderer::new(Arc::new(bytes), RenderConfig::default());
        for page_index in 0..renderer.page_count() {
            let Ok(page) = renderer.filtered_text(page_index, Default::default()) else {
                panic!("failed to extract {} page {}", file.display(), page_index + 1)
            };
            let first_word = page.plain_text().split_whitespace().next().unwrap_or("absent-corpus-query").to_owned();
            for query in ["the", "United States", first_word.as_str()] {
                for (sensitive, whole, limit) in [(false, false, usize::MAX), (false, true, 5), (true, false, 5), (true, true, usize::MAX)] {
                    let expected = reference(&page, query, sensitive, whole, limit);
                    assert_eq!(
                        page.find_opts_limited(query, sensitive, whole, limit),
                        expected,
                        "{} page {} query={query:?} sensitive={sensitive} whole={whole} limit={limit}",
                        file.display(),
                        page_index + 1
                    );
                }
            }
        }
    }
}
