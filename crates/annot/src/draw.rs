use super::create::{finite, num_array};
use super::lookup::{annot_dict, annots, page_refs};
use super::modify::refuse_foreign_text;
use super::types::NOTE_SIZE;
use super::{AnnotError, Dict, Document, ObjRef, Object, Stream, appearance};
use pdfcraft_cos::serialize;

/// (Re)generate `/AP /N` for the annotation `r` from its dictionary, for an edit that keeps its
/// text (a restyle, a new size or colour).
///
/// A comment whose text the standard fonts can't draw, with an appearance another program drew
/// (with a font that has those characters), keeps that appearance: redrawing it would put `?` in
/// their place, so the edit is refused (`AnnotError::Undrawable`). One PdfCraft drew with `?`
/// (marked `pdfcraft_fonts::UNDRAWN_MARK`) is redrawn.
pub fn set_appearance(doc: &mut Document, r: ObjRef) -> Result<(), AnnotError> {
    refuse_foreign_text(doc, &annot_dict(doc, r))?;
    draw_appearance(doc, r)
}

/// Put `before` back when redrawing fails, so a refused edit does not keep the new rectangle or style.
pub(crate) fn redraw_or_restore(doc: &mut Document, r: ObjRef, before: Dict) -> Result<(), AnnotError> {
    if let Err(e) = set_appearance(doc, r) {
        doc.set(r, Object::Dict(before));
        return Err(e);
    }
    Ok(())
}

/// Draw `r`'s normal appearance from its dictionary, whatever its text: characters the standard
/// fonts can't draw come out as `?`, and the annotation is marked (`pdfcraft_fonts::UNDRAWN_MARK`,
/// cleared when all of it can be drawn) so the edit is reported and flattening won't make that
/// `?` permanent. For new text (a new comment, changed contents), as Acrobat keeps text its font
/// can't show, and for display-only copies.
pub(crate) fn draw_appearance(doc: &mut Document, r: ObjRef) -> Result<(), AnnotError> {
    let d = annot_dict(doc, r);
    let subtype = String::from_utf8_lossy(d.name(b"Subtype").unwrap_or_default()).into_owned();
    let Some(stream) = build_appearance(doc, &d) else { return Err(AnnotError::Unsupported(subtype)) };
    apply_appearance(doc, r, &d, stream)
}

/// Draw the appearance for an annotation dictionary: the expensive half of [`set_appearance`].
/// Pure from the dictionary and the objects it points at, so [`AppearanceCache`] can reuse it.
fn build_appearance(doc: &Document, d: &Dict) -> Option<Stream> {
    let mut stream = appearance::build(d)?;
    // Picture-stamp restyling must retain the placement's page-axis correction.
    if d.get(b"PCPicture").is_some() {
        let matrix = d.get(b"AP").and_then(|ap| {
            let ap = doc.resolve(ap);
            let normal = doc.resolve(ap.as_dict()?.get(b"N")?);
            let Object::Stream(normal) = normal.as_ref() else { return None };
            normal.dict.get(b"Matrix").cloned()
        });
        if let Some(matrix) = matrix {
            stream.dict.set(b"Matrix".to_vec(), matrix);
        }
    }
    Some(stream)
}

/// Store a built appearance on the annotation: the cheap half of [`set_appearance`].
fn apply_appearance(doc: &mut Document, r: ObjRef, d: &Dict, stream: Stream) -> Result<(), AnnotError> {
    // Characters the standard fonts can't draw came out as `?`: mark the annotation with the first.
    let missing = appearance::undrawable_text(d).map(|(_, c)| c);
    let ap = doc.add(Object::Stream(stream));
    // A copy (a shared /AP is left alone) that keeps unknown entries; the old down and rollover
    // appearances would show the previous look on press or hover, so they go with the old /N.
    let mut apd = d.get(b"AP").map(|a| doc.resolve(a)).and_then(|a| a.as_dict().cloned()).unwrap_or_default();
    apd.remove(b"D");
    apd.remove(b"R");
    apd.set(b"N".to_vec(), Object::Ref(ap));
    // A note icon has a fixed size (`NOTE_SIZE`); viewers draw it at the upper-left corner of `/Rect`.
    // The appearance's box is mapped onto `/Rect`, so any other `/Rect` (a large one, or one with
    // no width, as LaTeX `pdfcomment` writes) would stretch the icon or shrink it to nothing.
    let icon_rect = matches!(d.name(b"Subtype"), Some(b"Text" | b"FileAttachment"))
        .then(|| d.get(b"Rect")?.as_array()?.iter().map(|o| o.as_f64()).collect::<Option<Vec<f64>>>())
        .flatten()
        .filter(|q| q.len() == 4 && finite(q))
        .map(|q| {
            let (x, top) = (q[0].min(q[2]), q[1].max(q[3]));
            [x, top - NOTE_SIZE, x + NOTE_SIZE, top]
        });
    doc.update_dict(r, |d| {
        d.set(b"AP".to_vec(), Object::Dict(apd));
        if let Some(rect) = icon_rect {
            d.set(b"Rect".to_vec(), num_array(&rect));
        }
        d.remove(b"AS");
        pdfcraft_fonts::set_undrawn_mark(d, missing);
    })?;
    Ok(())
}

/// Display appearances built for annotations without any, reused while the annotation
/// dictionaries they were drawn from are unchanged (#126).
///
/// A content edit (moving a paragraph, retyping text, …) leaves every annotation dictionary
/// alone, but the display copy is rebuilt from the edited document on every refresh: without
/// this cache each refresh redrew every appearance (27k on one CAD file, ~0.7 s per edit
/// commit). An entry is keyed by its annotation's reference and validated by a digest of
/// everything [`build_appearance`] reads — the annotation dictionary's direct entries, plus
/// the objects the build reads through by reference (the `/AP` dictionary a picture-stamp
/// restyle takes its `/Matrix` from) — so an edited annotation is redrawn and every untouched
/// one is reused, on comment edits, structural refreshes, undo and redo alike. The digest is
/// of content, not object identity, so a snapshot restore of equal bytes is still a hit.
/// Entries whose annotation is no longer listed (a deletion, or an edit that gave the comment
/// its own `/AP`) are dropped by [`AppearanceCache::retain_listed`], and the byte budget
/// evicts the least recently used entries instead of clearing the cache.
#[derive(Clone, Debug)]
pub struct AppearanceCache {
    entries: std::collections::HashMap<ObjRef, Entry>,
    /// `(annotation, stamp of its use)` in use order. An entry used again leaves a stale
    /// record behind; eviction skips those, and [`Self::compact`] drops them.
    order: std::collections::VecDeque<(ObjRef, u64)>,
    stamp: u64,
    bytes: usize,
    budget: usize,
    hits: u64,
    builds: u64,
}

#[derive(Clone, Debug)]
struct Entry {
    digest: u128,
    stream: Stream,
    last_used: u64,
}

impl AppearanceCache {
    /// Cached appearance data restarts past this instead of growing without bound.
    const MAX_BYTES: usize = 64 << 20;

    /// A cache with a smaller byte budget (tests and tools).
    pub fn with_budget(budget: usize) -> Self {
        Self { entries: Default::default(), order: Default::default(), stamp: 0, bytes: 0, budget, hits: 0, builds: 0 }
    }

    /// Forget every entry (memory pressure, or a document the cache no longer belongs to).
    pub fn clear(&mut self) {
        *self = Self::with_budget(self.budget);
    }

    fn get(&mut self, r: ObjRef, digest: u128) -> Option<Stream> {
        let stream = {
            let entry = self.entries.get_mut(&r)?;
            if entry.digest != digest {
                return None;
            }
            self.stamp = self.stamp.saturating_add(1);
            entry.last_used = self.stamp;
            entry.stream.clone()
        };
        self.order.push_back((r, self.stamp));
        self.compact();
        self.hits += 1;
        Some(stream)
    }

    fn insert(&mut self, r: ObjRef, digest: u128, stream: &Stream) {
        self.builds += 1;
        if let Some(old) = self.entries.remove(&r) {
            self.bytes = self.bytes.saturating_sub(old.stream.raw.len());
        }
        // One entry over the whole budget is served, just not kept.
        if stream.raw.len() > self.budget {
            return;
        }
        self.bytes = self.bytes.saturating_add(stream.raw.len());
        self.stamp = self.stamp.saturating_add(1);
        let last_used = self.stamp;
        self.entries.insert(r, Entry { digest, stream: stream.clone(), last_used });
        self.order.push_back((r, last_used));
        while self.bytes > self.budget {
            let Some((candidate, at)) = self.order.pop_front() else { break };
            // A record whose entry was used since is stale; the newer record evicts it.
            if self.entries.get(&candidate).is_some_and(|e| e.last_used == at)
                && let Some(evicted) = self.entries.remove(&candidate)
            {
                self.bytes = self.bytes.saturating_sub(evicted.stream.raw.len());
            }
        }
    }

    /// Drop the entries of annotations no page lists any more (a deleted comment, or one whose
    /// edit wrote its own `/AP`). Called once per display pass with every listed annotation.
    pub fn retain_listed(&mut self, listed: &std::collections::HashSet<ObjRef>) {
        let mut bytes = self.bytes;
        self.entries.retain(|r, e| {
            if listed.contains(r) {
                true
            } else {
                bytes = bytes.saturating_sub(e.stream.raw.len());
                false
            }
        });
        self.bytes = bytes;
        self.order.retain(|(r, _)| self.entries.contains_key(r));
    }

    /// Drop the stale use records a re-touched entry left behind, so the queue stays
    /// proportional to the entries.
    fn compact(&mut self) {
        if self.order.len() > self.entries.len().saturating_mul(2).saturating_add(64) {
            self.order.retain(|(r, at)| self.entries.get(r).is_some_and(|e| e.last_used == *at));
        }
    }

    /// Cache hits so far (reused appearances); for tests.
    pub fn hits(&self) -> u64 {
        self.hits
    }

    /// Appearances built so far; for tests.
    pub fn builds(&self) -> u64 {
        self.builds
    }

    /// Entries currently held; for tests.
    pub fn len(&self) -> usize {
        self.entries.len()
    }

    /// Whether no entry is held; for tests.
    pub fn is_empty(&self) -> bool {
        self.entries.is_empty()
    }

    /// Appearance bytes currently held; for tests.
    pub fn bytes(&self) -> usize {
        self.bytes
    }
}

impl Default for AppearanceCache {
    fn default() -> Self {
        Self::with_budget(Self::MAX_BYTES)
    }
}

/// A 128-bit FNV-1a mix, one pass over the bytes.
fn mix(hash: &mut u128, bytes: &[u8]) {
    const PRIME: u128 = 0x0000_0000_0100_0000_0000_0000_0000_013b;
    for &b in bytes {
        *hash = (*hash ^ u128::from(b)).wrapping_mul(PRIME);
    }
}

/// A digest of everything [`build_appearance`] reads from the document: each direct entry of
/// the annotation dictionary — a reference hashed by identity, because an appearance embeds
/// the reference, never the object behind it — plus, when the dictionary has an `/AP`, that
/// dictionary resolved and its `/N` with it, the two objects the picture-stamp restyle reads
/// its `/Matrix` through. Deterministic and content-addressed, so an object replaced with
/// equal content (undo's snapshot restore) digests the same.
fn appearance_digest(doc: &Document, d: &Dict) -> u128 {
    let mut hash: u128 = 0x6c62_272e_07bb_0142_62b8_2175_6295_c58d;
    let mut scratch = Vec::new();
    for (k, v) in d.iter() {
        mix(&mut hash, &(k.len() as u128).to_le_bytes());
        mix(&mut hash, k);
        serialize(v, &mut scratch);
        mix(&mut hash, &scratch);
        if k.as_slice() == b"AP" {
            let ap = doc.resolve(v);
            serialize(&ap, &mut scratch);
            mix(&mut hash, &scratch);
            if let Some(n) = ap.as_dict().and_then(|ap| ap.get(b"N")) {
                serialize(&doc.resolve(n), &mut scratch);
                mix(&mut hash, &scratch);
            }
        }
    }
    hash
}

/// A copy of `doc` in which every annotation without a normal appearance (`/AP /N`) has the one
/// [`appearance::build`] draws from its dictionary, for displaying the document (`None` when no
/// annotation needs one). Many files carry comments without appearances (FreeText, Ink, notes,
/// stamps, …) that viewers draw from the dictionary. `doc` is not changed: saving writes the file
/// as it was. Links, form fields and pop-ups keep their own handling.
pub fn with_missing_appearances(doc: &Document) -> Option<Document> {
    with_missing_appearances_cached(doc, &mut AppearanceCache::default())
}

/// A copy of `doc` with [`AppearanceCache`] reuse across refreshes: annotations whose
/// dictionary (and the objects its appearance reads through) is unchanged since the cached
/// build keep their stored appearance instead of redrawing it. See [`AppearanceCache`] for
/// when entries stay valid.
pub fn with_missing_appearances_cached(doc: &Document, cache: &mut AppearanceCache) -> Option<Document> {
    let mut copy: Option<Document> = None;
    // An annotation listed more than once, on one page or several, is drawn once. `has_normal`
    // reads `doc`, which never gets the appearance, so every repeat used to add another stream to
    // the copy, and the display bytes grew with the number of listings.
    let mut seen = std::collections::HashSet::new();
    for page in page_refs(doc).ok()? {
        for r in annots(doc, page).iter().filter_map(Object::as_ref) {
            if !seen.insert(r) {
                continue;
            }
            let d = annot_dict(doc, r);
            if matches!(d.name(b"Subtype"), None | Some(b"Link" | b"Widget" | b"Popup")) {
                continue;
            }
            let has_normal = d.get(b"AP").is_some_and(|ap| doc.resolve(ap).as_dict().is_some_and(|ap| ap.contains(b"N")));
            if has_normal {
                continue;
            }
            let digest = appearance_digest(doc, &d);
            if let Some(stream) = cache.get(r, digest) {
                // Fails only when `r` isn't a dictionary, which the cached build ruled out.
                apply_appearance(copy.get_or_insert_with(|| doc.clone()), r, &d, stream).ok();
                continue;
            }
            // Another program's stamp gets a drawing for display only (ISO 32000-2 Table 184). Its
            // `/AP`, if any, has no `/N` to keep here.
            let other_stamp = || {
                let mut bare = d.clone();
                bare.remove(b"AP");
                appearance::display_stamp(&bare)
            };
            let Some(stream) = build_appearance(doc, &d).or_else(other_stamp) else { continue };
            cache.insert(r, digest, &stream);
            // Fails only when `r` isn't a dictionary, which the build just ruled out.
            apply_appearance(copy.get_or_insert_with(|| doc.clone()), r, &d, stream).ok();
        }
    }
    cache.retain_listed(&seen);
    copy
}
