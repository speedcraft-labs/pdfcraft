//! Synthetic edit journeys for craft-journeys.
//!
//! Usage: `edit_bench pages|form [FILE] [FIELD]`. The document is opened before timing; the
//! reported duration is one user-visible Session edit; craft-bench supplies the paired repeats.

use std::error::Error;
use std::sync::Arc;
use std::time::Instant;

use pdfcraft_engine::{AddedText, Edit, FieldValue, FormFieldKind, Session, export::Exporter};
use sha2::{Digest, Sha256};

const PAGE_COUNT: usize = 100_000;
const FIELD_COUNT: usize = 3_129;
const RUNS: usize = 1;

fn fixed_clock() -> i64 {
    1_700_000_000
}

fn pdf(pages: usize, fields: usize) -> Vec<u8> {
    let page_start = 3usize;
    let form_id = page_start + pages;
    let field_start = form_id + 1;
    let object_count = if fields == 0 { page_start + pages - 1 } else { form_id + fields };
    let mut objects = Vec::with_capacity(object_count + 1);
    objects.push(String::new());
    let acroform = if fields == 0 { String::new() } else { format!(" /AcroForm {form_id} 0 R") };
    objects.push(format!("<< /Type /Catalog /Pages 2 0 R{acroform} >>"));
    let kids = (page_start..page_start + pages).map(|id| format!("{id} 0 R")).collect::<Vec<_>>().join(" ");
    objects.push(format!("<< /Type /Pages /Count {pages} /Kids [{kids}] >>"));
    let widget_refs = (field_start..field_start + fields).map(|id| format!("{id} 0 R")).collect::<Vec<_>>().join(" ");
    for page in 0..pages {
        let annots = if fields > 0 && page == 0 { format!(" /Annots [{widget_refs}]") } else { String::new() };
        objects.push(format!("<< /Type /Page /Parent 2 0 R /MediaBox [0 0 612 792]{annots} >>"));
    }
    if fields > 0 {
        objects.push(format!(
            "<< /Fields [{widget_refs}] /NeedAppearances true /DA (/Helv 10 Tf 0 g) /DR << /Font << /Helv << /Type /Font /Subtype /Type1 /BaseFont /Helvetica >> >> >> >>"
        ));
        for i in 0..fields {
            let x = 10 + (i % 100) * 5;
            let y = 10 + (i / 100) * 20;
            objects.push(format!(
                "<< /Type /Annot /Subtype /Widget /FT /Tx /T (field{i:04}) /V (old) /Rect [{x} {y} {} {}] /P {page_start} 0 R >>",
                x + 30,
                y + 15
            ));
        }
    }

    let mut bytes = b"%PDF-1.7\n%\xE2\xE3\xCF\xD3\n".to_vec();
    let mut offsets = vec![0usize; object_count + 1];
    for id in 1..=object_count {
        offsets[id] = bytes.len();
        bytes.extend_from_slice(format!("{id} 0 obj\n{}\nendobj\n", objects[id]).as_bytes());
    }
    let xref = bytes.len();
    bytes.extend_from_slice(format!("xref\n0 {}\n0000000000 65535 f \n", object_count + 1).as_bytes());
    for offset in offsets.iter().skip(1) {
        bytes.extend_from_slice(format!("{offset:010} 00000 n \n").as_bytes());
    }
    bytes.extend_from_slice(format!("trailer\n<< /Size {} /Root 1 0 R >>\nstartxref\n{xref}\n%%EOF\n", object_count + 1).as_bytes());
    bytes
}

fn digest(bytes: &[u8]) -> String {
    Sha256::digest(bytes).iter().map(|byte| format!("{byte:02x}")).collect()
}

fn verify_saved_pdf(path: &str) -> Result<(), Box<dyn Error>> {
    let bytes = Arc::new(std::fs::read(path)?);
    let mut session = Session::new();
    let id = session.open(path, Some(path.to_owned()), bytes, None)?;
    let doc = session.get(id).ok_or("the saved PDF did not open")?;
    let page_count = doc.info.pages.len();
    let pages = (0..page_count).collect::<Vec<_>>();
    let text = Exporter::new(doc).text_of(&pages)?;
    let mut fields = doc.form.iter().map(|field| (field.name.clone(), field.value.clone())).collect::<Vec<_>>();
    fields.sort();
    let mut semantic = format!("pages={page_count}\n{text}\n");
    for (name, values) in fields {
        semantic.push_str(&format!("field:{name}:{values:?}\n"));
    }
    println!("CRAFT_OUTPUT_SHA256={}", digest(semantic.as_bytes()));
    Ok(())
}

fn differential(path: &str) -> Result<(), Box<dyn Error>> {
    use pdfcraft_forms::{Field, FieldKind};

    fn editable(field: &Field) -> bool {
        if field.read_only() || field.widgets.is_empty() {
            return false;
        }
        match field.kind {
            FieldKind::Text | FieldKind::CheckBox => true,
            FieldKind::Radio => field.widgets.iter().any(|widget| widget.on_state.is_some()),
            FieldKind::Combo => field.has(pdfcraft_forms::flags::EDIT) || !field.options.is_empty(),
            FieldKind::List => !field.options.is_empty(),
            FieldKind::PushButton | FieldKind::Signature => false,
        }
    }

    fn value(field: &Field) -> Option<FieldValue> {
        if field.widgets.is_empty() {
            return None;
        }
        match field.kind {
            FieldKind::Text => Some(FieldValue::Text(if field.value.first().is_some_and(|value| value == "1") { "2" } else { "1" }.into())),
            FieldKind::CheckBox => Some(FieldValue::Check(field.value.is_empty())),
            FieldKind::Radio => field
                .widgets
                .iter()
                .filter_map(|widget| widget.on_state.clone())
                .find(|state| field.value.first() != Some(state))
                .or_else(|| field.widgets.iter().find_map(|widget| widget.on_state.clone()))
                .map(|state| FieldValue::Radio(Some(state))),
            FieldKind::Combo if field.has(pdfcraft_forms::flags::EDIT) && field.options.is_empty() => {
                Some(FieldValue::Choice(vec!["differential-value".into()]))
            }
            FieldKind::Combo | FieldKind::List => field
                .options
                .iter()
                .find(|(export, _)| field.value.first() != Some(export))
                .or_else(|| field.options.first())
                .map(|(export, _)| FieldValue::Choice(vec![export.clone()])),
            FieldKind::PushButton | FieldKind::Signature => None,
        }
    }

    let bytes = Arc::new(std::fs::read(path)?);
    let mut fast = Session::new().with_clock(fixed_clock);
    let fast_id = fast.open(path, None, Arc::clone(&bytes), None)?;
    let mut slow = Session::new().with_clock(fixed_clock);
    let slow_id = slow.open(path, None, bytes, None)?;
    let all = fast.get(fast_id).ok_or("the fast document did not open")?.form.to_vec();
    let shard_count = std::env::var("PDFCRAFT_DIFF_SHARDS").ok().and_then(|value| value.parse::<usize>().ok()).unwrap_or(1);
    let shard = std::env::var("PDFCRAFT_DIFF_SHARD").ok().and_then(|value| value.parse::<usize>().ok()).unwrap_or(0);
    let field_filter = std::env::var("PDFCRAFT_DIFF_FIELD").ok();
    if shard_count == 0 || shard >= shard_count {
        return Err(format!("invalid differential shard {shard} of {shard_count}").into());
    }
    let mut compared = 0usize;
    let mut actionful = 0usize;
    let mut editable_fields = 0usize;
    let mut seen = std::collections::HashSet::new();
    for field in &all {
        if !editable(field) || !seen.insert(field.name.clone()) {
            continue;
        }
        let field_shard = editable_fields % shard_count;
        editable_fields += 1;
        if field_shard != shard || field_filter.as_deref().is_some_and(|name| field.name != name) {
            continue;
        }
        let Some(next) = value(field) else { return Err(format!("editable field {:?} has no differential value", field.name).into()) };
        if !field.actions.is_empty() || !field.actions.unsupported.is_empty() {
            actionful += 1;
        }
        let edit = Edit::SetFieldValue { name: field.name.clone(), value: next };
        let fast_result = fast.apply(fast_id, edit.clone());
        let slow_result = slow.apply_without_form_fast_path(slow_id, edit);
        if format!("{fast_result:?}") != format!("{slow_result:?}") {
            return Err(
                format!("fast and general Session edits return different results for {:?}: {fast_result:?} vs {slow_result:?}", field.name).into()
            );
        }
        let fast_doc = fast.get(fast_id).ok_or("the fast document disappeared")?;
        let slow_doc = slow.get(slow_id).ok_or("the general document disappeared")?;
        let fast_bytes = fast_doc.bytes.as_ref();
        let slow_bytes = slow_doc.bytes.as_ref();
        if fast_bytes != slow_bytes {
            return Err(format!("fast and general setters differ for {:?}", field.name).into());
        }
        if format!("{:?}", fast_doc.info.fields) != format!("{:?}", slow_doc.info.fields) {
            return Err(format!("fast and general cached field values differ for {:?}", field.name).into());
        }
        let fast_output = fast.take_js_output(fast_id);
        let slow_output = slow.take_js_output(slow_id);
        if fast_output != slow_output {
            return Err(format!("fast and general JavaScript output differs for {:?}: {fast_output:?} vs {slow_output:?}", field.name).into());
        }
        compared += 1;
    }
    println!("DIFFERENTIAL_FILE={path}");
    println!("DIFFERENTIAL_FIELDS={compared}");
    println!("DIFFERENTIAL_EDITABLE_FIELDS={editable_fields}");
    println!("DIFFERENTIAL_SHARD={shard}/{shard_count}");
    println!("DIFFERENTIAL_ACTIONFUL={actionful}");
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::differential;

    #[test]
    #[ignore = "requires the SF-85P corpus file and compares all editable fields"]
    fn sf85p_every_editable_field_matches_the_full_xfa_path() {
        let path = std::env::var("PDFCRAFT_SF85P_PATH").expect("set PDFCRAFT_SF85P_PATH to the SF-85P corpus file");
        differential(&path).expect("fast and full XFA edit paths should match for every editable field");
    }
}

fn main() -> Result<(), Box<dyn Error>> {
    let mut args = std::env::args().skip(1);
    let journey = args.next().ok_or("usage: edit_bench pages|form|open-real|edit-real|form-real|save-real|verify-real [FILE] [FIELD|OUTPUT]")?;
    let file = args.next();
    let next = args.next();
    if journey == "verify-real" {
        return verify_saved_pdf(file.as_deref().ok_or("verify-real needs a saved PDF path")?);
    }
    let (field_name, output_path) = if journey == "save-real" { (args.next(), next) } else { (next, None) };
    if journey == "differential" {
        return differential(file.as_deref().ok_or("usage: edit_bench differential FILE")?);
    }
    let (pages, fields) = match journey.as_str() {
        "pages" if file.is_none() => (PAGE_COUNT, 0),
        "form" if file.is_none() => (1, FIELD_COUNT),
        "pages" | "form" => return Err("synthetic journeys do not take a file".into()),
        "pages-real" | "form-real" | "open-real" | "edit-real" | "save-real" if file.is_some() => (0, 0),
        _ => return Err(format!("unknown journey {journey:?}").into()),
    };
    let input = match file {
        Some(ref path) => Arc::new(std::fs::read(path)?),
        None => Arc::new(pdf(pages, fields)),
    };
    let mut session = Session::new().with_clock(fixed_clock);
    let open_start = Instant::now();
    let id = session.open(file.as_deref().unwrap_or("synthetic-edit.pdf"), None, input, None)?;
    let open_ns = open_start.elapsed().as_nanos();
    let selected_field = if journey == "form-real" || journey == "edit-real" || journey == "save-real" {
        let doc = session.get(id).ok_or("the document did not open")?;
        let field = field_name
            .as_deref()
            .and_then(|name| doc.form.iter().find(|field| field.name == name))
            .or_else(|| {
                doc.form.iter().find(|field| {
                    field.kind == FormFieldKind::Text
                        && !field.read_only()
                        && !field.widgets.is_empty()
                        && (journey == "edit-real"
                            || field.actions.calculate != pdfcraft_forms::af::Calculate::None
                            || field.actions.format != pdfcraft_forms::af::Format::None
                            || field.actions.validate != pdfcraft_forms::af::Validate::None
                            || field.actions.scripts.calculate.is_some()
                            || field.actions.scripts.format.is_some()
                            || field.actions.scripts.keystroke.is_some()
                            || field.actions.scripts.validate.is_some())
                })
            })
            .or_else(|| doc.form.iter().find(|field| field.kind == FormFieldKind::Text && !field.read_only() && !field.widgets.is_empty()))
            .ok_or("the PDF has no editable text field")?;
        Some(field.name.clone())
    } else {
        None
    };
    let mut samples = Vec::with_capacity(RUNS);
    let mut output_digest = None;
    if journey == "open-real" {
        samples.push(open_ns);
    } else if journey == "edit-real" {
        let name = selected_field.as_ref().ok_or("PDF has no editable text field")?;
        let start = Instant::now();
        session.apply(id, Edit::SetFieldValue { name: name.clone(), value: FieldValue::Text("1".into()) })?;
        samples.push(start.elapsed().as_nanos());
    } else if journey == "save-real" {
        let name = selected_field.as_ref().ok_or("PDF has no editable text field")?;
        session.apply(id, Edit::SetFieldValue { name: name.clone(), value: FieldValue::Text("1".into()) })?;
        let output = output_path.as_deref().ok_or("save-real needs an output path")?;
        let start = Instant::now();
        session.save_in_place(id, output)?;
        samples.push(start.elapsed().as_nanos());
        output_digest = Some(digest(&std::fs::read(output)?));
    } else {
        for run in 0..RUNS {
            let start = Instant::now();
            if journey == "pages" || journey == "pages-real" {
                session.apply(
                    id,
                    Edit::AddText {
                        page: 0,
                        text: AddedText { rect: [20.0, 250.0, 180.0, 280.0], text: format!("Edit {}", run % 2), ..Default::default() },
                    },
                )?;
            } else if let Some(name) = selected_field.as_ref() {
                session.apply(id, Edit::SetFieldValue { name: name.clone(), value: FieldValue::Text("1".into()) })?;
            } else {
                session.apply(id, Edit::SetFieldValue { name: "field0000".into(), value: FieldValue::Text(format!("value{}", run % 2)) })?;
            }
            samples.push(start.elapsed().as_nanos());
        }
    }
    samples.sort_unstable();
    let doc = session.get(id).ok_or("the document disappeared")?;
    let reported_ns = if journey == "open-real" || journey == "edit-real" || journey == "save-real" { samples[0] } else { samples[RUNS / 2] };
    let output_identity = if journey == "edit-real" {
        let name = selected_field.as_deref().ok_or("PDF has no editable text field")?;
        let field = doc.form.iter().find(|field| field.name == name).ok_or("the edited PDF field disappeared")?;
        digest(format!("{:?}", field.value).as_bytes())
    } else {
        output_digest.unwrap_or_else(|| digest(doc.bytes.as_ref()))
    };
    println!("CRAFT_JOURNEY_NS={reported_ns}");
    println!("CRAFT_OUTPUT_SHA256={output_identity}");
    println!(
        "{journey}: {} pages, {} fields, {}, {} bytes",
        doc.info.pages.len(),
        doc.form.len(),
        selected_field.as_deref().or_else(|| (journey == "open-real").then_some("open")).unwrap_or("synthetic"),
        doc.bytes.len()
    );
    Ok(())
}
