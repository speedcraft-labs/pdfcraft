//! A document's XFA form: laying it out, keeping its datasets and fields in step, and taking it out.

use crate::edit::EditCtx;
use crate::{Document, EditError, FieldValue, XfaLayout, guard, xfa};

/// Lay a dynamic XFA form out (pages and fields) and give its widgets appearances.
pub(crate) fn xfa_layout(doc: &mut pdfcraft_cos::Document) -> Result<XfaLayout, String> {
    let mut report = pdfcraft_xfa::render_into(doc).map_err(|e| e.to_string())?;
    xfa_redraw(doc)?;
    // The form's own values: one its field's font can't draw is drawn with `?` and reported.
    report.warnings.extend(undrawn_field_warnings(doc));
    Ok(report)
}

/// Give the fields of a laid-out XFA form their appearances. A signed signature box keeps the
/// picture its signer's application drew.
pub(crate) fn xfa_redraw(doc: &mut pdfcraft_cos::Document) -> Result<(), String> {
    for f in pdfcraft_forms::fields(doc) {
        if f.kind == pdfcraft_forms::FieldKind::Signature && doc.get(f.obj).as_dict().is_some_and(|d| d.contains(b"V")) {
            continue;
        }
        pdfcraft_forms::redraw_field(doc, &f.name).map_err(|e| format!("{}: {e}", f.name))?;
    }
    Ok(())
}

/// A warning for each field whose appearance PdfCraft drew with `?` for a character its font
/// can't draw (laying out an XFA form's own values).
pub(crate) fn undrawn_field_warnings(doc: &pdfcraft_cos::Document) -> Vec<String> {
    pdfcraft_forms::undrawn_fields(doc)
        .into_iter()
        .map(|(name, c)| {
            format!("form field {name:?} holds \"{}\" (U+{:04X}), which its font can't draw: it shows ? in its place", c.escape_debug(), u32::from(c))
        })
        .collect()
}

/// The form's fields as the XFA data layer wants them.
fn xfa_field_data(doc: &pdfcraft_cos::Document) -> Vec<pdfcraft_xfa::FieldDatum> {
    pdfcraft_forms::fields(doc).into_iter().map(xfa_field_datum).collect()
}

fn xfa_field_datum(f: pdfcraft_forms::Field) -> pdfcraft_xfa::FieldDatum {
    use pdfcraft_forms::FieldKind as K;
    let data = match f.kind {
        K::Text | K::Combo | K::List => pdfcraft_xfa::FieldData::Text(f.value.join("\n")),
        K::CheckBox => pdfcraft_xfa::FieldData::Check(!f.value.is_empty()),
        K::Radio => pdfcraft_xfa::FieldData::Radio(f.value.first().cloned()),
        K::PushButton | K::Signature => pdfcraft_xfa::FieldData::None,
    };
    pdfcraft_xfa::FieldDatum { obj: f.obj, name: f.name, data }
}

fn xfa_field_datum_ref(f: &pdfcraft_forms::Field) -> pdfcraft_xfa::FieldDatum {
    use pdfcraft_forms::FieldKind as K;
    let data = match f.kind {
        K::Text | K::Combo | K::List => pdfcraft_xfa::FieldData::Text(f.value.join("\n")),
        K::CheckBox => pdfcraft_xfa::FieldData::Check(!f.value.is_empty()),
        K::Radio => pdfcraft_xfa::FieldData::Radio(f.value.first().cloned()),
        K::PushButton | K::Signature => pdfcraft_xfa::FieldData::None,
    };
    pdfcraft_xfa::FieldDatum { obj: f.obj, name: f.name.clone(), data }
}

/// Full-sync per-field warning details, kept in field order so incremental writes can replace
/// just the diagnostics for fields they touched.
#[derive(Clone, Debug)]
pub(crate) struct XfaDatasetWarningCache {
    fields: Vec<pdfcraft_cos::ObjRef>,
    by_field: std::collections::HashMap<pdfcraft_cos::ObjRef, Vec<pdfcraft_xfa::DatasetFieldWarning>>,
    warning_paths: Vec<pdfcraft_xfa::data::DataPath>,
}

impl XfaDatasetWarningCache {
    fn from_write(
        form: &[pdfcraft_forms::Field],
        tpl: Option<&pdfcraft_xfa::model::Template>,
        write: &pdfcraft_xfa::DatasetsWrite,
        field_warnings: &[pdfcraft_xfa::DatasetFieldWarning],
    ) -> Option<Self> {
        // Packet-level warnings do not belong to one field, so leave those documents on the full
        // diagnostic path. Field warnings can be safely refreshed by object reference.
        let tpl = tpl?;
        if pdfcraft_xfa::format_field_warnings(field_warnings) != write.warnings {
            return None;
        }
        let mut by_field: std::collections::HashMap<_, Vec<_>> = std::collections::HashMap::new();
        let mut warning_paths = Vec::with_capacity(field_warnings.len());
        for warning in field_warnings {
            if let Some(field) = warning.field {
                by_field.entry(field).or_default().push(warning.clone());
                warning_paths.push(pdfcraft_xfa::data_path(tpl, &warning.som));
            }
        }
        Some(Self { fields: form.iter().map(|field| field.obj).collect(), by_field, warning_paths })
    }

    fn replace_fields(&mut self, fields: &std::collections::HashSet<pdfcraft_cos::ObjRef>, warnings: &[pdfcraft_xfa::DatasetFieldWarning]) {
        for field in fields {
            self.by_field.remove(field);
        }
        for warning in warnings {
            if let Some(field) = warning.field {
                self.by_field.entry(field).or_default().push(warning.clone());
            }
        }
    }

    fn warnings(&self) -> Vec<String> {
        let ordered: Vec<_> = self.fields.iter().filter_map(|field| self.by_field.get(field)).flatten().cloned().collect();
        pdfcraft_xfa::format_field_warnings(&ordered)
    }

    fn changed_path_may_affect_warnings(
        &self,
        doc: &pdfcraft_cos::Document,
        tpl: &pdfcraft_xfa::model::Template,
        changed: &[pdfcraft_xfa::FieldDatum],
    ) -> bool {
        changed.iter().any(|field| {
            let som = doc
                .get(field.obj)
                .as_dict()
                .and_then(|dict| dict.get(pdfcraft_xfa::SOM_KEY))
                .map(|value| doc.resolve(value))
                .and_then(|value| value.as_string().map(|value| value.to_text()))
                .unwrap_or_else(|| field.name.clone());
            let path = pdfcraft_xfa::data_path(tpl, &som);
            self.warning_paths.iter().any(|warning| {
                let related = path.as_slice().starts_with(warning.as_slice()) || warning.as_slice().starts_with(path.as_slice());
                let siblings = path.len() > 1 && path.len() == warning.len() && path[..path.len() - 1] == warning[..warning.len() - 1];
                related || siblings
            })
        })
    }
}

/// Full-sync warnings and their per-field details for the values and datasets present now.
pub(crate) fn xfa_dataset_warnings(
    doc: &pdfcraft_cos::Document,
    tpl: Option<&pdfcraft_xfa::model::Template>,
    form: &[pdfcraft_forms::Field],
) -> Result<(Option<XfaDatasetWarningCache>, Vec<String>), String> {
    let data: Vec<_> = form.iter().map(xfa_field_datum_ref).collect();
    let mut warning_doc = doc.clone();
    let (write, field_warnings) = pdfcraft_xfa::write_datasets_with_details(&mut warning_doc, tpl, &data, None).map_err(|e| e.to_string())?;
    let cache = XfaDatasetWarningCache::from_write(form, tpl, &write, &field_warnings);
    Ok((cache, write.warnings))
}

/// Fields whose values were already out of sync with XFA datasets when the document opened.
/// Keeping these names lets later incremental edits preserve the first full-sync result.
pub(crate) fn xfa_initial_dataset_dirty_fields(
    doc: &pdfcraft_cos::Document,
    tpl: Option<&pdfcraft_xfa::model::Template>,
    form: &[pdfcraft_forms::Field],
) -> std::collections::HashSet<String> {
    let data: Vec<_> = form.iter().map(xfa_field_datum_ref).collect();
    let dirty_soms: std::collections::HashSet<_> = pdfcraft_xfa::unsynced_soms_with(doc, tpl, &data).into_iter().collect();
    form.iter()
        .filter(|field| {
            let object = doc.get(field.obj);
            let som = object
                .as_dict()
                .and_then(|dict| dict.get(pdfcraft_xfa::SOM_KEY))
                .map(|value| doc.resolve(value))
                .and_then(|value| value.as_string().map(|value| value.to_text()))
                .unwrap_or_else(|| field.name.clone());
            dirty_soms.contains(&som)
        })
        .map(|field| field.name.clone())
        .collect()
}

/// Keep the XFA datasets packet in step with the fields after an edit. Returns what could not
/// be written.
/// `datasets`: the stream this edit already wrote, replaced in place rather than added again
/// (and set to the one written).
/// `tpl`: the form's parsed template when the document has one cached (else it is parsed).
pub(crate) fn xfa_sync_datasets(
    doc: &mut pdfcraft_cos::Document,
    tpl: Option<&pdfcraft_xfa::model::Template>,
    datasets: &mut Option<pdfcraft_cos::ObjRef>,
) -> Result<Vec<String>, String> {
    let data = xfa_field_data(doc);
    let r = pdfcraft_xfa::write_datasets_with(doc, tpl, &data, *datasets).map_err(|e| e.to_string())?;
    if r.stream.is_some() {
        *datasets = r.stream;
    }
    Ok(r.warnings)
}

/// Keep only dirty field values in the datasets packet. `form` is the cached field index for the
/// current revision; unchanged field warnings are reused while warning-producing dirty fields
/// trigger a full diagnostic scan, without widening the packet mutation.
pub(crate) fn xfa_sync_changed_datasets(
    doc: &mut pdfcraft_cos::Document,
    tpl: Option<&pdfcraft_xfa::model::Template>,
    datasets: &mut Option<pdfcraft_cos::ObjRef>,
    form: &[pdfcraft_forms::Field],
    changed: &std::collections::HashSet<String>,
    cached_warnings: &mut Option<XfaDatasetWarningCache>,
) -> Result<Vec<String>, String> {
    if changed.is_empty() {
        if let Some(warnings) = cached_warnings {
            return Ok(warnings.warnings());
        }
        let (cache, warnings) = xfa_dataset_warnings(doc, tpl, form)?;
        *cached_warnings = cache;
        return Ok(warnings);
    }
    let data: Vec<pdfcraft_xfa::FieldDatum> = form
        .iter()
        .filter(|field| changed.contains(&field.name))
        .map(|field| xfa_field_datum(pdfcraft_forms::current_field_value(doc, field)))
        .collect();
    if data.is_empty() {
        if let Some(warnings) = cached_warnings {
            return Ok(warnings.warnings());
        }
        let (cache, warnings) = xfa_dataset_warnings(doc, tpl, form)?;
        *cached_warnings = cache;
        return Ok(warnings);
    }
    // Keep a pre-write snapshot only for the conservative fallback below. Most field edits raise
    // no dirty-field warning, so they avoid rebuilding diagnostics across every field.
    let mut warning_doc = doc.clone();
    let (r, r_field_warnings) = pdfcraft_xfa::write_datasets_with_details(doc, tpl, &data, *datasets).map_err(|e| e.to_string())?;
    if r.stream.is_some() {
        *datasets = r.stream;
    }
    let field_warnings = pdfcraft_xfa::format_field_warnings(&r_field_warnings);
    let changed_objects: std::collections::HashSet<_> = data.iter().map(|field| field.obj).collect();
    let warning_path_may_change =
        tpl.is_some_and(|tpl| cached_warnings.as_ref().is_some_and(|warnings| warnings.changed_path_may_affect_warnings(doc, tpl, &data)));
    let previously_warned =
        cached_warnings.as_ref().is_some_and(|warnings| changed_objects.iter().any(|field| warnings.by_field.contains_key(field)));
    if field_warnings == r.warnings
        && r_field_warnings.is_empty()
        && !warning_path_may_change
        && !previously_warned
        && let Some(warnings) = cached_warnings
    {
        warnings.replace_fields(&changed_objects, &r_field_warnings);
        return Ok(warnings.warnings());
    }

    // Warning-producing fields can share or nest datasets nodes, so their write may change the
    // warning status of other fields. Rebuild the full result from the pre-write snapshot.
    let all_data: Vec<_> = form
        .iter()
        .map(|field| {
            if changed.contains(&field.name) { xfa_field_datum(pdfcraft_forms::current_field_value(doc, field)) } else { xfa_field_datum_ref(field) }
        })
        .collect();
    let (write, field_warnings) =
        pdfcraft_xfa::write_datasets_with_details(&mut warning_doc, tpl, &all_data, *datasets).map_err(|e| e.to_string())?;
    *cached_warnings = XfaDatasetWarningCache::from_write(form, tpl, &write, &field_warnings);
    let warnings = write.warnings;
    Ok(warnings)
}

/// What a document whose form script ran away is told.
pub(crate) const XFA_SCRIPTS_OFF: &str =
    "A script of this form ran too long and was abandoned; the form's scripts are off for this document (reopen it to run them again)";

/// Most XFA warnings kept per document.
const MAX_XFA_WARNINGS: usize = 50;

/// Add `new` to `list` (no repeats, at most [`MAX_XFA_WARNINGS`]).
pub(crate) fn note_warnings(list: &mut Vec<String>, new: &[String]) {
    for w in new {
        if !list.contains(w) && list.len() < MAX_XFA_WARNINGS {
            list.push(w.clone());
        }
    }
}

/// A data file's text: UTF-8 or UTF-16 (by a byte order mark, or how `<` is written), or
/// Latin-1 when its XML declaration says so. `None`: not text at all (FDF); `Err`: an
/// encoding this can't read.
pub(crate) fn decode_data_file(bytes: &[u8]) -> Option<Result<String, String>> {
    if bytes.starts_with(b"%FDF") {
        return None;
    }
    let declared = |text: &str| -> Option<String> {
        let head = text.get(..text.len().min(256))?;
        let decl = head.strip_prefix('\u{feff}').unwrap_or(head).trim_start().strip_prefix("<?xml")?;
        let decl = decl.split_once("?>")?.0;
        let enc = decl.split_once("encoding=")?.1;
        let quote = enc.chars().next()?;
        Some(enc.get(1..)?.split(quote).next()?.trim().to_ascii_lowercase())
    };
    if let Some((text, _)) = pdfcraft_xfa::packets::decode(bytes) {
        match declared(&text).as_deref() {
            None | Some("utf-8" | "utf8" | "utf-16" | "utf-16le" | "utf-16be" | "us-ascii" | "ascii") => return Some(Ok(text)),
            Some("iso-8859-1" | "latin1" | "latin-1" | "windows-1252" | "cp1252") if !bytes.starts_with(&[0xEF, 0xBB, 0xBF]) => {
                // Each byte is the character it names (the 0x80–0x9F block of Windows-1252
                // differs, and is rare in form data).
                return Some(Ok(bytes.iter().map(|b| char::from(*b)).collect()));
            }
            Some(other) => return Some(Err(format!("the data file's encoding {other:?} is not supported (UTF-8, UTF-16 and ISO-8859-1 are)"))),
        }
    }
    // Not valid UTF-8/UTF-16: Latin-1 by declaration, else refused.
    let latin1: String = bytes.iter().map(|b| char::from(*b)).collect();
    match declared(&latin1).as_deref() {
        Some("iso-8859-1" | "latin1" | "latin-1" | "windows-1252" | "cp1252") => Some(Ok(latin1)),
        _ => Some(Err("the data file is not UTF-8, UTF-16 or ISO-8859-1 text".into())),
    }
}

/// Take the XFA out of `doc` and note what went with it: the packets, data no field showed,
/// buttons that did nothing but run scripts, and fields whose value differs from the form's
/// data (data PdfCraft could not read into them, now lost).
pub(crate) fn strip_xfa(doc: &mut pdfcraft_cos::Document, cx: &mut EditCtx) -> Result<(), EditError> {
    let fields = pdfcraft_forms::fields(doc);
    let data = xfa_field_data(doc);
    let differ = pdfcraft_xfa::read_values(doc, &data)
        .into_iter()
        .filter(|(name, value)| {
            let Some(f) = fields.iter().find(|f| f.name == *name) else { return false };
            match value {
                pdfcraft_xfa::FieldData::Text(t) => f.value.join("\n") != *t && !t.trim().is_empty(),
                pdfcraft_xfa::FieldData::Check(on) => f.value.is_empty() == *on,
                pdfcraft_xfa::FieldData::Radio(sel) => f.value.first() != sel.as_ref(),
                pdfcraft_xfa::FieldData::None => false,
            }
        })
        .count();
    let r = pdfcraft_xfa::strip(doc).map_err(|e| EditError::Write(e.to_string()))?;
    cx.xfa_removed.extend(r.notes());
    if differ > 0 {
        cx.xfa_removed.push(format!(
            "{differ} field(s) held a value other than the form's XFA data (data PdfCraft could not read into them); the data's version is gone"
        ));
    }
    Ok(())
}

/// Keep the document's XFA state in step with its file after an edit: a script may have laid
/// the form out again (rows added, subforms shown), an edit may have taken the XFA out
/// (flattened, or made static), or an undo put it back.
pub(crate) fn sync_xfa_state(doc: &mut Document) {
    let Some(cos) = doc.editor.as_ref().map(|e| &e.cos) else { return };
    match pdfcraft_xfa::existing_layout(cos) {
        Some(report) => {
            if doc.xfa.is_none() {
                // Back after an undo: the XFA's own warnings apply again, the strip's don't.
                doc.xfa_warnings.retain(|w| !w.starts_with(XFA_REMOVED_PREFIX));
                if doc.xfa_data_template.is_none() {
                    doc.xfa_data_template = xfa::data_template(cos);
                }
                if doc.xfa_template.is_none() && !doc.xfa_scripts_off {
                    doc.xfa_template = doc.xfa_data_template.clone().filter(|template| pdfcraft_xfa::has_scripts(template));
                }
            }
            doc.xfa = Some(report);
        }
        None => {
            doc.xfa = None;
            doc.xfa_template = None;
            doc.xfa_data_template = None;
            doc.xfa_initial_dataset_dirty.clear();
            // Only what the strip said stays (the XFA's own warnings no longer apply).
            doc.xfa_warnings.retain(|w| w.starts_with(XFA_REMOVED_PREFIX));
        }
    }
}

/// How the warnings of a strip start, so they can be told from the XFA's own.
pub(crate) const XFA_REMOVED_PREFIX: &str = "XFA removed: ";

/// Is this an XFA data file (an XDP, a datasets document, a data element or a data document),
/// rather than XFDF, Acrobat's `<fields>` XML or a table? By its root element.
pub(crate) fn is_xfa_data_file(text: &str) -> bool {
    let text = text.get(..text.len().min(65_536)).unwrap_or(text);
    let mut rest = text.trim_start_matches('\u{feff}').trim_start();
    // Past the XML declaration, comments and processing instructions.
    let mut guard = 0;
    while guard < 100 {
        guard += 1;
        if let Some(r) = rest.strip_prefix("<?") {
            rest = r.split_once("?>").map_or("", |(_, r)| r).trim_start();
        } else if let Some(r) = rest.strip_prefix("<!--") {
            rest = r.split_once("-->").map_or("", |(_, r)| r).trim_start();
        } else if let Some(r) = rest.strip_prefix("<!") {
            rest = r.split_once('>').map_or("", |(_, r)| r).trim_start();
        } else {
            break;
        }
    }
    let Some(tag) = rest.strip_prefix('<') else { return false };
    let name = tag.split(|c: char| c.is_whitespace() || c == '>' || c == '/').next().unwrap_or("");
    let local = name.rsplit(':').next().unwrap_or(name);
    !local.is_empty() && !matches!(local, "xfdf" | "fields" | "html" | "body")
}

/// The content type a picture is saved under, from its bytes: the kinds a laid-out form shows.
pub(crate) fn image_content_type(bytes: &[u8]) -> Option<&'static str> {
    match pdfcraft_xfa::image::kind(bytes)? {
        pdfcraft_xfa::image::Kind::Jpeg => Some("image/jpeg"),
        pdfcraft_xfa::image::Kind::Png => Some("image/png"),
        pdfcraft_xfa::image::Kind::Gif => Some("image/gif"),
        pdfcraft_xfa::image::Kind::Bmp => Some("image/bmp"),
        pdfcraft_xfa::image::Kind::Tiff => Some("image/tiff"),
    }
}

/// After the XFA data changed outside the fields (an import, a picture): a laid-out form is
/// laid out again from the data (then, when `calculate`, its calculations run), and the
/// fields take the data's values.
pub(crate) fn xfa_after_data_change(doc: &mut pdfcraft_cos::Document, cx: &mut EditCtx, calculate: bool) -> Result<(), EditError> {
    if let Some(tpl) = cx.xfa.clone() {
        let mut run = xfa::XfaRun {
            page_count: cx.pages,
            out: &mut cx.xfa_out,
            datasets: cx.xfa_datasets,
            changed_fields: Default::default(),
            structure_changed: false,
            ran_away: false,
            time_scale: cx.xfa_time_scale,
        };
        let r = guard(|| xfa::on_data_replaced(doc, &tpl, calculate, &mut run))
            .unwrap_or_else(|m| Err(format!("laying the form out again failed unexpectedly ({m})")));
        cx.xfa_datasets = run.datasets;
        cx.xfa_ran_away |= run.ran_away;
        if let Err(e) = r {
            cx.xfa_out.errors.push(e);
        }
    }
    let taken = xfa_values_from_datasets(doc).map_err(EditError::Write)?;
    if !taken.is_empty() {
        cx.xfa_out.console.push(format!("{} field value(s) taken from the imported data", taken.len()));
    }
    // Not laid out again (a static form, or scripts off): fields the data no longer holds are
    // emptied, so the data replaced is the data shown.
    if cx.xfa.is_none() && calculate {
        let data = xfa_field_data(doc);
        let present: std::collections::HashSet<String> = pdfcraft_xfa::read_values(doc, &data).into_iter().map(|(n, _)| n).collect();
        for f in pdfcraft_forms::fields(doc) {
            if present.contains(&f.name) || f.value.is_empty() {
                continue;
            }
            use pdfcraft_forms::FieldKind as K;
            let empty = match f.kind {
                K::Text => FieldValue::Text(String::new()),
                K::CheckBox => FieldValue::Check(false),
                K::Radio => FieldValue::Radio(None),
                K::Combo | K::List => FieldValue::Choice(Vec::new()),
                K::PushButton | K::Signature => continue,
            };
            if let Err(e) = pdfcraft_forms::set_value(doc, &f.name, &empty) {
                cx.xfa_out.errors.push(format!("{}: not emptied by the import: {e}", f.name));
            }
        }
    }
    Ok(())
}

/// Give the fields the values the XFA datasets hold (a form filled in another viewer). Returns
/// the names of the fields that changed.
pub(crate) fn xfa_values_from_datasets(doc: &mut pdfcraft_cos::Document) -> Result<Vec<String>, String> {
    let data = xfa_field_data(doc);
    let fields = pdfcraft_forms::fields(doc);
    // The first field of each name, as a lookup by name would find it.
    let mut by_name = std::collections::HashMap::new();
    for f in &fields {
        by_name.entry(f.name.as_str()).or_insert(f);
    }
    let mut updates = Vec::new();
    for (name, value) in pdfcraft_xfa::read_values(doc, &data) {
        let Some(f) = by_name.get(name.as_str()) else { continue };
        let new = match value {
            pdfcraft_xfa::FieldData::Text(t) if matches!(f.kind, pdfcraft_forms::FieldKind::Combo | pdfcraft_forms::FieldKind::List) => {
                // Saved values, one per line for a multi-select list; shown text stands for
                // its saved value (an older viewer may have written it).
                let saved = |v: &str| f.options.iter().find(|(e, s)| e == v || s == v).map_or(v.to_string(), |(e, _)| e.clone());
                let picked: Vec<String> = if f.has(pdfcraft_forms::flags::MULTI_SELECT) {
                    t.lines().map(str::trim).filter(|l| !l.is_empty()).map(saved).collect()
                } else {
                    vec![saved(t.trim())]
                };
                (f.value != picked).then_some(FieldValue::Choice(picked))
            }
            pdfcraft_xfa::FieldData::Text(t) => (f.value.join("\n") != t).then_some(FieldValue::Text(t)),
            pdfcraft_xfa::FieldData::Check(on) => (f.value.is_empty() == on).then_some(FieldValue::Check(on)),
            pdfcraft_xfa::FieldData::Radio(sel) => (f.value.first() != sel.as_ref()).then_some(FieldValue::Radio(sel)),
            pdfcraft_xfa::FieldData::None => None,
        };
        if let Some(v) = new {
            updates.push((name, v));
        }
    }
    // All at once: one by one, each value would read the whole form again, which took minutes
    // on a form with thousands of fields.
    let Ok(results) = pdfcraft_forms::set_values(doc, &updates) else { return Ok(Vec::new()) };
    Ok(updates.into_iter().zip(results).filter(|(_, r)| r.is_ok()).map(|((name, _), _)| name).collect())
}
