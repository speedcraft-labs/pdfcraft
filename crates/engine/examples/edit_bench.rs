use std::error::Error;
use std::sync::Arc;
use std::time::Instant;
use pdfcraft_engine::{Edit, FieldValue, FormFieldKind, Session};

fn fixed_clock() -> i64 { 1_700_000_000 }

fn main() -> Result<(), Box<dyn Error>> {
    let mut args = std::env::args().skip(1);
    let mode = args.next().ok_or("usage: edit_bench edit|open|save FILE [FIELD] [OUTPUT]")?;
    let path = args.next().ok_or("missing PDF path")?;
    let name = args.next();
    let output = args.next();
    let bytes = Arc::new(std::fs::read(&path)?);
    let mut session = Session::new().with_clock(fixed_clock);
    let start = Instant::now();
    let id = session.open(&path, Some(path.clone()), bytes, None)?;
    let open_ns = start.elapsed().as_nanos();
    println!("OPEN_NS={open_ns}");
    if mode == "open" { return Ok(()); }
    let doc = session.get(id).ok_or("document missing")?;
    let field = name.as_deref().and_then(|n| doc.form.iter().find(|f| f.name == n)).or_else(|| doc.form.iter().find(|f| {
        f.kind == FormFieldKind::Text && !f.read_only() && !f.widgets.is_empty() &&
        (f.actions.calculate != pdfcraft_forms::af::Calculate::None || f.actions.format != pdfcraft_forms::af::Format::None ||
         f.actions.validate != pdfcraft_forms::af::Validate::None || f.actions.scripts.calculate.is_some() ||
         f.actions.scripts.format.is_some() || f.actions.scripts.keystroke.is_some() || f.actions.scripts.validate.is_some())
    })).or_else(|| doc.form.iter().find(|f| f.kind == FormFieldKind::Text && !f.read_only() && !f.widgets.is_empty())).ok_or("no editable text field")?;
    let field_name = field.name.clone();
    let start = Instant::now();
    session.apply(id, Edit::SetFieldValue { name: field_name.clone(), value: FieldValue::Text("1".into()) })?;
    println!("EDIT_NS={}", start.elapsed().as_nanos());
    println!("FIELD={field_name}");
    if mode == "save" {
        let bytes = session.save_bytes(id)?;
        let output = output.ok_or("save requires output path")?;
        std::fs::write(output, bytes.as_ref())?;
        println!("SAVED_BYTES={}", bytes.len());
    }
    Ok(())
}
