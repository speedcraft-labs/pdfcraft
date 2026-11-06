//! Synthetic edit journeys for craft-journeys.
//!
//! Usage: `edit_bench pages|form [FILE] [FIELD]`. The document is opened before timing; the
//! reported duration is one user-visible Session edit; craft-bench supplies the paired repeats.

use std::error::Error;
use std::sync::Arc;
use std::time::Instant;

use pdfcraft_engine::{AddedText, Edit, FieldValue, FormFieldKind, Session};
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

fn differential(path: &str) -> Result<(), Box<dyn Error>> {
    use pdfcraft_cos::{Document, SaveOptions, write_incremental};
    use pdfcraft_forms::{Field, FieldKind};

    fn value(field: &Field) -> Option<FieldValue> {
        if field.widgets.is_empty() {
            return None;
        }
        match field.kind {
            FieldKind::Text => Some(FieldValue::Text("1".into())),
            FieldKind::CheckBox => Some(FieldValue::Check(field.value.is_empty())),
            FieldKind::Radio => field.widgets.iter().find_map(|widget| widget.on_state.clone()).map(|state| FieldValue::Radio(Some(state))),
            FieldKind::Combo | FieldKind::List => field.options.first().map(|(export, _)| FieldValue::Choice(vec![export.clone()])),
            FieldKind::PushButton | FieldKind::Signature => None,
        }
    }

    let bytes = Arc::new(std::fs::read(path)?);
    let original = Document::open(bytes)?;
    let all = pdfcraft_forms::fields(&original);
    let mut compared = 0usize;
    let mut direct = 0usize;
    let mut indexed = 0usize;
    let mut actionful = 0usize;
    for field in &all {
        let Some(next) = value(field) else { continue };
        if !field.actions.is_empty() || !field.actions.unsupported.is_empty() {
            actionful += 1;
        }
        let mut fast = original.clone();
        let mut slow = original.clone();
        let fast_result = match pdfcraft_forms::set_simple_field_value(&mut fast, &all, &field.name, &next) {
            Ok(true) => {
                direct += 1;
                Ok(())
            }
            Ok(false) => {
                indexed += 1;
                pdfcraft_forms::set_value_with_fields(&mut fast, &all, &field.name, &next, &mut pdfcraft_forms::NoScripts)
            }
            Err(error) => Err(error),
        };
        let slow_result = pdfcraft_forms::set_value(&mut slow, &field.name, &next);
        if format!("{fast_result:?}") != format!("{slow_result:?}") {
            return Err(format!("fast and general setters return different results for {:?}: {fast_result:?} vs {slow_result:?}", field.name).into());
        }
        let fast_bytes = write_incremental(&fast, &SaveOptions::default())?;
        let slow_bytes = write_incremental(&slow, &SaveOptions::default())?;
        if fast_bytes != slow_bytes {
            return Err(format!("fast and general setters differ for {:?}", field.name).into());
        }
        compared += 1;
    }
    println!("DIFFERENTIAL_FIELDS={compared}");
    println!("DIFFERENTIAL_DIRECT={direct}");
    println!("DIFFERENTIAL_INDEXED={indexed}");
    println!("DIFFERENTIAL_ACTIONFUL={actionful}");
    Ok(())
}

fn main() -> Result<(), Box<dyn Error>> {
    let mut args = std::env::args().skip(1);
    let journey = args.next().ok_or("usage: edit_bench pages|form [FILE] [FIELD]")?;
    let file = args.next();
    let field_name = args.next();
    if journey == "differential" {
        return differential(file.as_deref().ok_or("usage: edit_bench differential FILE")?);
    }
    let (pages, fields) = match journey.as_str() {
        "pages" if file.is_none() => (PAGE_COUNT, 0),
        "form" if file.is_none() => (1, FIELD_COUNT),
        "pages" | "form" => return Err("synthetic journeys do not take a file".into()),
        "pages-real" | "form-real" if file.is_some() => (0, 0),
        _ => return Err(format!("unknown journey {journey:?}").into()),
    };
    let input = match file {
        Some(ref path) => Arc::new(std::fs::read(path)?),
        None => Arc::new(pdf(pages, fields)),
    };
    let mut session = Session::new().with_clock(fixed_clock);
    let id = session.open(file.as_deref().unwrap_or("synthetic-edit.pdf"), None, input, None)?;
    let selected_field = if journey == "form-real" {
        let doc = session.get(id).ok_or("the document did not open")?;
        let field = field_name
            .as_deref()
            .and_then(|name| doc.form.iter().find(|field| field.name == name))
            .or_else(|| {
                doc.form.iter().find(|field| {
                    field.kind == FormFieldKind::Text
                        && !field.read_only()
                        && !field.widgets.is_empty()
                        && (field.actions.calculate != pdfcraft_forms::af::Calculate::None
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
            session.apply(id, Edit::SetFieldValue { name: name.clone(), value: FieldValue::Text(format!("value{}", run % 2)) })?;
        } else {
            session.apply(id, Edit::SetFieldValue { name: "field0000".into(), value: FieldValue::Text(format!("value{}", run % 2)) })?;
        }
        samples.push(start.elapsed().as_nanos());
    }
    samples.sort_unstable();
    let doc = session.get(id).ok_or("the document disappeared")?;
    println!("CRAFT_JOURNEY_NS={}", samples[RUNS / 2]);
    println!("CRAFT_OUTPUT_SHA256={}", digest(doc.bytes.as_ref()));
    println!(
        "{journey}: {} pages, {} fields, {}, {} bytes",
        doc.info.pages.len(),
        doc.form.len(),
        selected_field.as_deref().unwrap_or("synthetic"),
        doc.bytes.len()
    );
    Ok(())
}
