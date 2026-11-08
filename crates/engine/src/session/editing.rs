#[cfg(test)]
use super::check_refresh_panic;
use super::{
    Arc, DocId, Document, Edit, EditCtx, EditError, Label, MAX_UNDO, ObjectKind, OnceLock, RenderPool, SaveOptions, Scope, Session,
    XFA_REMOVED_PREFIX, XFA_SCRIPTS_OFF, check_permission, comment_list, display_bytes, display_config, guard, info_fields, inspect, js, keys_after,
    note_warnings, render_threads, run_edit, scan_colours, scope_of, shown_under_redactions, signatures_of, sync_xfa_state, undrawn_notices,
    undrawn_notices_for_fields, uses_scripts, write_incremental, xfa, xfa_sync_changed_datasets, xfa_sync_datasets,
};
use crate::{EditNotice, MarkKind};

struct FastFormPathConfig<'a> {
    allowed: bool,
    encrypted: bool,
    has_signatures: bool,
    signed: bool,
    js_off: bool,
    name: &'a str,
    calculation_cache: &'a js::CalculationCache,
    data_template: Option<&'a pdfcraft_xfa::model::Template>,
}

struct XfaScriptEditConfig<'a> {
    is_xfa: bool,
    incremental: bool,
    known_form_changes: Option<&'a std::collections::HashSet<String>>,
    form: &'a [pdfcraft_forms::Field],
    data_template: Option<&'a pdfcraft_xfa::model::Template>,
    edit_plan: Option<Arc<xfa::XfaEditPlan>>,
    initial_dataset_dirty: &'a std::collections::HashSet<String>,
    dataset_warnings: &'a mut Option<crate::xfa_form::XfaDatasetWarningCache>,
}

fn apply_fast_form_path(
    next: &mut pdfcraft_cos::Document,
    form: &[pdfcraft_forms::Field],
    edit: &Edit,
    cx: &mut EditCtx,
    known_form_changes: &mut Option<std::collections::HashSet<String>>,
    config: FastFormPathConfig<'_>,
) -> Result<bool, EditError> {
    let mut simple_field_applied = false;
    let fast_field_applied = if config.allowed && !config.encrypted && !config.has_signatures && !config.signed {
        let actionful = form.iter().any(|field| {
            !field.actions.is_empty()
                || !field.actions.unsupported.is_empty()
                || field.actions.scripts.format.is_some()
                || field.actions.scripts.keystroke.is_some()
                || field.actions.scripts.validate.is_some()
                || field.actions.scripts.calculate.is_some()
        });
        if actionful {
            if !config.js_off && uses_scripts(edit) {
                let mut runner = js::JsRunner::new(next, config.name);
                runner.calculation_cache = config.calculation_cache.clone();
                cx.js = Some(runner);
            }
            try_indexed_field_edit(next, form, edit, cx.js.as_mut(), cx.xfa.as_deref().or(config.data_template))?
        } else {
            simple_field_applied = try_simple_field_edit(next, form, edit)?;
            simple_field_applied
        }
    } else {
        false
    };
    if !fast_field_applied && !config.js_off && uses_scripts(edit) {
        let mut runner = js::JsRunner::new(next, config.name);
        runner.calculation_cache = config.calculation_cache.clone();
        cx.js = Some(runner);
    }
    if !fast_field_applied {
        guard(|| run_edit(next, edit, cx))
            .unwrap_or_else(|m| Err(EditError::Invalid(format!("{} failed unexpectedly ({m}); the document was not changed", edit.label()))))?;
    }
    if let (Some(known), Some(js)) = (known_form_changes.as_mut(), cx.js.as_ref()) {
        known.extend(js.touched_fields.iter().cloned());
    }
    Ok(simple_field_applied)
}

fn verify_redaction_edit(
    edit: &Edit,
    original: &pdfcraft_cos::Document,
    next: &pdfcraft_cos::Document,
    cx: &EditCtx,
    info: &pdfcraft_render::DocInfo,
    password: Option<&str>,
) -> Result<(), EditError> {
    if let Edit::ApplyRedactions { pages } = edit {
        let marks: Vec<pdfcraft_redact::Mark> =
            pdfcraft_redact::marks(original).into_iter().filter(|m| pages.as_ref().is_none_or(|p| p.contains(&m.page))).collect();
        guard(|| shown_under_redactions(next, &marks, &cx.redaction_overlays, &info.pages, password))
            .unwrap_or_else(|m| Err(EditError::Invalid(format!("{} failed unexpectedly ({m}); the document was not changed", edit.label()))))?;
    }
    Ok(())
}

fn apply_xfa_script_edits(
    next: &mut pdfcraft_cos::Document,
    edit: &Edit,
    cx: &mut EditCtx,
    config: XfaScriptEditConfig<'_>,
    scope: &mut Scope,
) -> Result<Vec<String>, EditError> {
    let XfaScriptEditConfig {
        is_xfa,
        incremental: incremental_xfa,
        known_form_changes,
        form,
        data_template,
        edit_plan,
        initial_dataset_dirty,
        dataset_warnings,
    } = config;
    fn collect(edit: &Edit, changed: &mut Vec<String>, reset: &mut bool) {
        match edit {
            Edit::SetFieldValue { name, .. } => changed.push(name.clone()),
            Edit::ResetForm { .. } => *reset = true,
            Edit::Batch { edits, .. } => edits.iter().for_each(|edit| collect(edit, changed, reset)),
            _ => {}
        }
    }
    fn only_field_values(edit: &Edit) -> bool {
        match edit {
            Edit::SetFieldValue { .. } => true,
            Edit::Batch { edits, .. } => !edits.is_empty() && edits.iter().all(only_field_values),
            _ => false,
        }
    }
    let mut changed = Vec::new();
    let mut reset = false;
    collect(edit, &mut changed, &mut reset);
    let incremental_change = incremental_xfa && !reset && only_field_values(edit);
    if let Some(template) = cx.xfa.clone() {
        let calculation_inputs: Vec<String> = known_form_changes.into_iter().flat_map(|names| names.iter().cloned()).collect();
        let mut all_changed_names = changed.clone();
        all_changed_names.extend(calculation_inputs.iter().cloned());
        let has_som_mapping = all_changed_names.iter().any(|name| {
            form.iter()
                .find(|field| field.name == *name)
                .is_some_and(|field| next.get(field.obj).as_dict().is_some_and(|dict| dict.get(pdfcraft_xfa::SOM_KEY).is_some()))
        });
        let has_change_work = (!changed.is_empty() || reset)
            && if incremental_change {
                edit_plan.as_ref().and_then(|plan| plan.needs_run(&all_changed_names)).unwrap_or_else(|| {
                    xfa::has_calculation_scripts(&template) || (has_som_mapping && xfa::has_change_scripts(&template, &changed, &calculation_inputs))
                })
            } else {
                xfa::has_calculation_scripts(&template) || (has_som_mapping && xfa::has_change_scripts(&template, &changed, &calculation_inputs))
            };
        // Scripts read the datasets, so sync before running them. With no script work, the
        // post-edit sync below is enough and avoids parsing the data packet twice.
        if has_change_work && is_xfa && matches!(scope_of(edit), Scope::Form | Scope::Full | Scope::Content) {
            let datasets = &mut cx.xfa_datasets;
            let mut dirty_fields: std::collections::HashSet<String> = changed.iter().cloned().collect();
            dirty_fields.extend(known_form_changes.into_iter().flat_map(|fields| fields.iter().cloned()));
            dirty_fields.extend(initial_dataset_dirty.iter().cloned());
            let incremental_sync = incremental_change && *scope != Scope::Full;
            let _pre_script_notes = if incremental_sync {
                guard(|| xfa_sync_changed_datasets(next, data_template.or(Some(&template)), datasets, form, &dirty_fields, dataset_warnings))
            } else {
                guard(|| {
                    let notes = xfa_sync_datasets(next, data_template.or(Some(&template)), datasets)?;
                    *dataset_warnings = None;
                    Ok(notes)
                })
            }
            .unwrap_or_else(|m| Err(format!("writing the XFA data failed unexpectedly ({m})")))
            .map_err(EditError::Write)?;
        }
        if has_change_work {
            let mut run = xfa::XfaRun {
                page_count: cx.pages,
                out: &mut cx.xfa_out,
                datasets: cx.xfa_datasets,
                changed_fields: Default::default(),
                structure_changed: false,
                ran_away: false,
                time_scale: cx.xfa_time_scale,
            };
            let ran = guard(|| xfa::on_changes(next, &template, &changed, &calculation_inputs, incremental_change, edit_plan.clone(), &mut run))
                .unwrap_or_else(|m| Err(format!("the form's scripts failed unexpectedly ({m})")));
            cx.xfa_datasets = run.datasets;
            cx.xfa_changed_fields.extend(std::mem::take(&mut run.changed_fields));
            if run.structure_changed {
                *scope = Scope::Full;
            }
            cx.xfa_ran_away |= run.ran_away;
            if let Err(error) = ran {
                cx.xfa_out.errors.push(error);
            }
        }
    }
    let mut notes = Vec::new();
    if is_xfa && matches!(scope_of(edit), Scope::Form | Scope::Full | Scope::Content) {
        let (script_template, datasets) = (cx.xfa.as_deref(), &mut cx.xfa_datasets);
        let data_template = data_template.or(script_template);
        let dirty_fields = if incremental_change && *scope != Scope::Full {
            let mut names: std::collections::HashSet<String> = changed.iter().cloned().collect();
            names.extend(cx.xfa_changed_fields.iter().cloned());
            names.extend(known_form_changes.into_iter().flat_map(|fields| fields.iter().cloned()));
            names.extend(initial_dataset_dirty.iter().cloned());
            Some(names)
        } else {
            None
        };
        notes = if let Some(names) = dirty_fields.as_ref() {
            guard(|| xfa_sync_changed_datasets(next, data_template, datasets, form, names, dataset_warnings))
        } else {
            guard(|| {
                let notes = xfa_sync_datasets(next, data_template, datasets)?;
                *dataset_warnings = None;
                Ok(notes)
            })
        }
        .unwrap_or_else(|m| Err(format!("writing the XFA data failed unexpectedly ({m})")))
        .map_err(EditError::Write)?;
        // The pre-script sync can report transient warnings that calculations later resolve.
        // The full path reports the final dataset state, so expose only these post-script notes.
        cx.xfa_out.errors.extend(notes.iter().cloned());
        // Rebuilding from stale data would discard accepted widget values.
        if notes.is_empty() {
            let changed_layout_names = dirty_fields;
            let reflow = guard(|| xfa::relayout_changed_text(next, data_template, form, changed_layout_names.as_ref()))
                .unwrap_or_else(|m| Err(format!("laying out the XFA text failed unexpectedly ({m})")))
                .map_err(EditError::Write)?;
            if let Some(warnings) = reflow {
                notes.extend(warnings);
                // Layout replaces pages and widgets, even if the page count is unchanged.
                // Undo/redo must rebuild these too instead of refreshing only field values.
                *scope = Scope::Full;
            }
        }
    }
    Ok(notes)
}

/// Whether the edit, or any edit inside a batch, deletes pages.
fn deletes_pages(edit: &Edit) -> bool {
    match edit {
        Edit::DeletePages { .. } => true,
        Edit::Batch { edits, .. } => edits.iter().any(deletes_pages),
        _ => false,
    }
}

/// The renderer and forms readers can differ only in spacing around a field-name token. Keep
/// their parallel field rows aligned when updating cached renderer values.
fn same_form_name(a: &str, b: &str) -> bool {
    a == b || a.chars().filter(|c| !c.is_whitespace()).eq(b.chars().filter(|c| !c.is_whitespace()))
}

fn try_simple_field_edit(next: &mut pdfcraft_cos::Document, form: &[pdfcraft_forms::Field], edit: &Edit) -> Result<bool, EditError> {
    let Edit::SetFieldValue { name, value } = edit else {
        return Ok(false);
    };
    match guard(|| pdfcraft_forms::set_simple_field_value(next, form, name, value)) {
        Ok(result) => result.map_err(Into::into),
        Err(message) => Err(EditError::Invalid(format!("{} failed unexpectedly ({message}); the document was not changed", edit.label()))),
    }
}

fn try_indexed_field_edit(
    next: &mut pdfcraft_cos::Document,
    form: &[pdfcraft_forms::Field],
    edit: &Edit,
    scripts: Option<&mut js::JsRunner>,
    xfa: Option<&pdfcraft_xfa::model::Template>,
) -> Result<bool, EditError> {
    let Edit::SetFieldValue { name, value } = edit else {
        return Ok(false);
    };
    let mut scripts = scripts;
    let result = match scripts.as_deref_mut() {
        Some(scripts) => guard(|| pdfcraft_forms::set_value_with_fields(next, form, name, value, scripts)),
        None => guard(|| pdfcraft_forms::set_value_with_fields(next, form, name, value, &mut pdfcraft_forms::NoScripts)),
    };
    match result {
        Ok(Ok(())) => Ok(true),
        Ok(Err(refused @ pdfcraft_forms::FormError::Format(_))) => {
            let Some(typed) = xfa.and_then(|template| xfa::typed_date(next, Some(template), name, value)) else {
                return Err(refused.into());
            };
            // The failed format attempt can update `/V` before drawing returns its error. The
            // general setter rereads the form for the typed-date retry; keep the indexed path in
            // step with it instead of reusing the pre-attempt snapshot.
            let retry_fields = pdfcraft_forms::fields(next);
            let retry = match scripts {
                Some(scripts) => guard(|| pdfcraft_forms::set_value_with_fields(next, &retry_fields, name, &typed, scripts)),
                None => guard(|| pdfcraft_forms::set_value_with_fields(next, &retry_fields, name, &typed, &mut pdfcraft_forms::NoScripts)),
            };
            match retry {
                Ok(Ok(())) => Ok(true),
                Ok(Err(error)) => Err(error.into()),
                Err(message) => Err(EditError::Invalid(format!("{} failed unexpectedly ({message}); the document was not changed", edit.label()))),
            }
        }
        Ok(Err(error)) => Err(error.into()),
        Err(message) => Err(EditError::Invalid(format!("{} failed unexpectedly ({message}); the document was not changed", edit.label()))),
    }
}

impl Session {
    /// Apply an edit. On success the previous state is undoable and the view data is refreshed.
    pub fn apply(&mut self, id: DocId, edit: Edit) -> Result<(), EditError> {
        self.apply_with_form_fast_path(id, edit, true)
    }

    /// Apply an edit using the general form setter, for differential benchmarks and tests.
    #[doc(hidden)]
    pub fn apply_without_form_fast_path(&mut self, id: DocId, edit: Edit) -> Result<(), EditError> {
        self.apply_with_form_fast_path(id, edit, false)
    }

    fn apply_with_form_fast_path(&mut self, id: DocId, edit: Edit, allow_form_fast_path: bool) -> Result<(), EditError> {
        let now = self.now();
        let today = self.today();
        let js_off = self.js_off;
        let xfa_time_scale = self.xfa_time_scale.unwrap_or(1);
        let doc = self.doc_mut(id)?;
        doc.edit_notices.clear();
        let name = doc.name.clone();
        let is_xfa = doc.info.xfa.is_some();
        let xfa_data_template = doc.xfa_data_template.clone();
        let mut xfa_dataset_warnings = doc.xfa_dataset_warnings.clone();
        let xfa_edit_plan = doc.xfa_edit_plan.clone();
        let mut scope = scope_of(&edit);
        let mut cx = EditCtx::new(now, doc.generation ^ (id.0 << 48));
        if !js_off {
            cx.xfa = doc.xfa_template.clone();
        }
        cx.xfa_time_scale = xfa_time_scale;
        cx.pages = doc.info.pages.len();
        cx.today = today;
        let reason = doc.read_only_reason.clone().unwrap_or_default();
        let signed = doc.is_signed();
        let editor = doc.editor.as_mut().ok_or(EditError::ReadOnly(reason))?;
        if let Some(p) = editor.cos.permissions() {
            check_permission(&edit, &p)?;
        }
        // Rewriting an OCR word (invisible text over a scan) also hides the scanned word: cover it
        // in the paper's colour and draw the new text in the ink's.
        let edit = match edit {
            Edit::EditTextBlock { page, block, text, mut style } => {
                // Only invisible text is sampled; the blocks are usually cached from opening it.
                if style.cover.is_none()
                    && let Some(info) = doc.info.pages.get(page)
                    && let Some(b) = editor.text_blocks(page).ok().and_then(|b| b.get(block).filter(|b| b.invisible).cloned())
                    && let Some((paper, ink)) = guard(|| scan_colours::sample(&editor.cos, info, page, &b)).ok().flatten()
                {
                    style.cover = Some(paper);
                    style.color = style.color.or(Some(ink));
                }
                Edit::EditTextBlock { page, block, text, style }
            }
            other => other,
        };
        // Nothing to bake: leave undo history and the dirty flag alone.
        if matches!(&edit, Edit::FlattenFillSign) && !pdfcraft_annot::has_visible_fill_sign(&editor.cos) {
            return Ok(());
        }
        let mut next = editor.cos.clone();
        cx.renamed_fields.clear();
        cx.redaction_overlays.clear();
        // `next` is a copy: if the edit fails or crashes, the document is unchanged.
        let mut known_form_changes = match &edit {
            Edit::SetFieldValue { name, .. } => {
                let mut fields = std::collections::HashSet::new();
                fields.insert(name.clone());
                if js_off {
                    fields.extend(
                        doc.form
                            .iter()
                            .filter(|field| {
                                field.actions.calculate != pdfcraft_forms::af::Calculate::None || field.actions.scripts.calculate.is_some()
                            })
                            .map(|field| field.name.clone()),
                    );
                }
                Some(fields)
            }
            _ => None,
        };
        let simple_field_applied = apply_fast_form_path(
            &mut next,
            &doc.form,
            &edit,
            &mut cx,
            &mut known_form_changes,
            FastFormPathConfig {
                allowed: allow_form_fast_path,
                encrypted: doc.info.encrypted,
                has_signatures: !doc.signatures.is_empty(),
                signed,
                js_off,
                name: &name,
                calculation_cache: &editor.calculation_cache,
                data_template: xfa_data_template.as_deref(),
            },
        )?;
        // Redaction removed what its own interpreter places under the marks. The marks were drawn
        // over the page as the renderer shows it, so the renderer has the last word.
        verify_redaction_edit(&edit, &editor.cos, &next, &cx, &doc.info, doc.password.as_deref())?;
        // Renamed fields are said, not slipped in: a repair-log note (with the caveat that
        // scripts were left alone) and a count in the undo step's label.
        let renamed_note = |renamed: &[(String, String)]| {
            (!renamed.is_empty()).then(|| {
                let shown: Vec<String> = renamed.iter().take(5).map(|(old, new)| format!("{old} → {new}")).collect();
                let more = renamed.len().saturating_sub(shown.len());
                let more = if more > 0 { format!(", and {more} more") } else { String::new() };
                format!(
                    "inserted pages renamed {} form field{} to keep their names separate ({}{more}); scripts that refer to the old names were not rewritten",
                    renamed.len(),
                    if renamed.len() == 1 { "" } else { "s" },
                    shown.join(", ")
                )
            })
        };
        if let Some(note) = renamed_note(&cx.renamed_fields) {
            next.note_repair(note);
        }
        // Only Insert from File renames fields; its label says so in a wording the interface
        // translates (the count lives in the repair note).
        let label = match (&edit, cx.renamed_fields.len()) {
            (_, 0) => edit.label(),
            (Edit::InsertPagesFrom { name, .. }, n) => Label::with(
                if n == 1 { "Insert pages from {name} — 1 field renamed" } else { "Insert pages from {name} — fields renamed" },
                name.as_str(),
            ),
            _ => edit.label(),
        };
        // An XFA form's scripts answer the change: exit and validate scripts of the field, then
        // every calculation (and a reset recalculates). They read the data, so it is brought up
        // to date first.
        let xfa_notes = apply_xfa_script_edits(
            &mut next,
            &edit,
            &mut cx,
            XfaScriptEditConfig {
                is_xfa,
                incremental: allow_form_fast_path,
                known_form_changes: known_form_changes.as_ref(),
                form: &doc.form,
                data_template: xfa_data_template.as_deref(),
                edit_plan: xfa_edit_plan.clone(),
                initial_dataset_dirty: &doc.xfa_initial_dataset_dirty,
                dataset_warnings: &mut xfa_dataset_warnings,
            },
            &mut scope,
        )?;
        if let Some(known) = known_form_changes.as_mut() {
            known.extend(cx.xfa_changed_fields.iter().cloned());
        }
        // Signed documents are only ever saved incrementally: an edit that needs a full rewrite
        // (applying redactions, changing security, sanitizing) would invalidate the signatures.
        // So does any edit to a file rebuilt from object headers: it has no chain to append to.
        // Deleted pages remain in earlier incremental revisions (#3407), so rewrite
        // unsigned files in full. Signed files must retain their signed bytes.
        if !signed && deletes_pages(&edit) {
            next.require_full_save();
        }
        let rewrites = |c: &pdfcraft_cos::Document| c.full_save_required() || c.encryption_changed();
        if signed && ((rewrites(&next) && !rewrites(&editor.cos)) || (next.is_modified() && next.revisions().is_empty())) {
            return Err(EditError::SignedRewrite(edit.label().to_string()));
        }
        let notices = edit_notices(&edit, &editor.cos, &next, scope, &doc.form, known_form_changes.as_ref());
        let previous = std::mem::replace(&mut editor.cos, next);
        let keys = keys_after(&edit).unwrap_or_else(|| editor.keys.clone());
        let previous_keys = std::mem::replace(&mut editor.keys, keys);
        editor.undo.push((label, previous, previous_keys, scope));
        if editor.undo.len() > MAX_UNDO {
            editor.undo.remove(0);
        }
        editor.redo.clear();
        Self::adopt_keys(doc);
        let refreshed = guard(|| {
            if scope == Scope::Content {
                Self::refresh_content(doc, Some(&edit))
            } else {
                Self::refresh_scoped(doc, scope, Some(&edit), simple_field_applied, &cx.xfa_changed_fields, known_form_changes.as_ref())
            }
        })
        .unwrap_or_else(|m| Err(EditError::Reopen(m)));
        if let Err(e) = refreshed {
            // Roll back: the edit produced something we cannot display.
            if let Some(ed) = doc.editor.as_mut()
                && let Some((_, prev, keys, _)) = ed.undo.pop()
            {
                ed.cos = prev;
                ed.keys = keys;
            }
            Self::adopt_keys(doc);
            if !matches!(guard(|| Self::refresh(doc)), Ok(Ok(()))) {
                Self::show_released(doc);
            }
            return Err(e);
        }
        doc.dirty = true;
        doc.generation += 1;
        doc.edit_notices = notices;
        if !cx.xfa_removed.is_empty() {
            let removed: Vec<String> = cx.xfa_removed.iter().map(|n| format!("{XFA_REMOVED_PREFIX}{n}")).collect();
            doc.xfa_warnings.retain(|w| w.starts_with(XFA_REMOVED_PREFIX));
            note_warnings(&mut doc.xfa_warnings, &removed);
            note_warnings(&mut doc.info.warnings, &removed);
        }
        note_warnings(&mut doc.xfa_warnings, &xfa_notes);
        let notes = doc.xfa_warnings.clone();
        note_warnings(&mut doc.info.warnings, &notes);
        if matches!(&edit, Edit::SetFieldValue { .. }) {
            if let Some(editor) = doc.editor.as_mut() {
                editor.calculation_cache = cx.js.as_ref().map(|js| js.calculation_cache.clone()).unwrap_or_default();
            }
        } else if let Some(editor) = doc.editor.as_mut() {
            editor.calculation_cache = js::CalculationCache::default();
        }
        if let Some(js) = cx.js {
            doc.js_output.append(js.output);
        }
        doc.js_output.append(cx.xfa_out);
        if cx.xfa_ran_away {
            doc.xfa_template = None;
            doc.xfa_scripts_off = true;
            doc.js_output.errors.push(XFA_SCRIPTS_OFF.into());
        }
        doc.xfa_dataset_warnings = xfa_dataset_warnings;
        doc.xfa_edit_plan = if scope == Scope::Full || cx.xfa_ran_away { None } else { xfa_edit_plan };
        Ok(())
    }

    pub fn undo(&mut self, id: DocId) -> Result<Label, EditError> {
        let doc = self.doc_mut(id)?;
        let editor = doc.editor.as_mut().ok_or(EditError::NothingToUndo)?;
        let (label, prev, keys, scope) = editor.undo.pop().ok_or(EditError::NothingToUndo)?;
        editor.calculation_cache = js::CalculationCache::default();
        let current = std::mem::replace(&mut editor.cos, prev);
        let current_keys = std::mem::replace(&mut editor.keys, keys);
        editor.redo.push((label.clone(), current, current_keys, scope));
        Self::adopt_keys(doc);
        let refreshed =
            guard(|| Self::refresh_scoped(doc, scope, None, false, &Default::default(), None)).unwrap_or_else(|m| Err(EditError::Reopen(m)));
        if let Err(e) = refreshed {
            if let Some(ed) = doc.editor.as_mut()
                && let Some((label, current, current_keys, scope)) = ed.redo.pop()
            {
                let prev = std::mem::replace(&mut ed.cos, current);
                let keys = std::mem::replace(&mut ed.keys, current_keys);
                ed.undo.push((label, prev, keys, scope));
            }
            Self::adopt_keys(doc);
            if !matches!(guard(|| Self::refresh(doc)), Ok(Ok(()))) {
                Self::show_released(doc);
            }
            return Err(e);
        }
        doc.edit_notices.clear();
        Self::sync_dirty(doc);
        doc.generation += 1;
        doc.xfa_dataset_warnings = None;
        if scope == Scope::Full {
            doc.xfa_edit_plan = None;
        }
        Ok(label)
    }

    pub fn redo(&mut self, id: DocId) -> Result<Label, EditError> {
        let doc = self.doc_mut(id)?;
        let editor = doc.editor.as_mut().ok_or(EditError::NothingToRedo)?;
        let (label, next, keys, scope) = editor.redo.pop().ok_or(EditError::NothingToRedo)?;
        editor.calculation_cache = js::CalculationCache::default();
        let current = std::mem::replace(&mut editor.cos, next);
        let current_keys = std::mem::replace(&mut editor.keys, keys);
        editor.undo.push((label.clone(), current, current_keys, scope));
        Self::adopt_keys(doc);
        let refreshed =
            guard(|| Self::refresh_scoped(doc, scope, None, false, &Default::default(), None)).unwrap_or_else(|m| Err(EditError::Reopen(m)));
        if let Err(e) = refreshed {
            if let Some(ed) = doc.editor.as_mut()
                && let Some((label, current, current_keys, scope)) = ed.undo.pop()
            {
                let next = std::mem::replace(&mut ed.cos, current);
                let keys = std::mem::replace(&mut ed.keys, current_keys);
                ed.redo.push((label, next, keys, scope));
            }
            Self::adopt_keys(doc);
            if !matches!(guard(|| Self::refresh(doc)), Ok(Ok(()))) {
                Self::show_released(doc);
            }
            return Err(e);
        }
        doc.edit_notices.clear();
        Self::sync_dirty(doc);
        doc.generation += 1;
        doc.xfa_dataset_warnings = None;
        if scope == Scope::Full {
            doc.xfa_edit_plan = None;
        }
        Ok(label)
    }

    /// Unsaved when the working graph is not the last opened or saved file. Undo back to that
    /// file is clean; undo past a later save is not, because the restored snapshot's own base
    /// is an earlier revision.
    pub(super) fn sync_dirty(doc: &mut Document) {
        doc.dirty = doc.editor.as_ref().is_some_and(|e| e.unsaved_file || e.cos.is_modified() || !Arc::ptr_eq(e.cos.bytes(), &e.saved));
    }

    /// Use the current state's password for viewing (protection edits change it).
    pub(super) fn adopt_keys(doc: &mut Document) {
        if let Some(e) = doc.editor.as_ref() {
            doc.password = e.keys.render.clone();
            doc.config.password = e.keys.render.as_deref().map(Arc::from);
        }
    }

    /// Rebuild the working bytes and the view data that `scope` may have changed. Metadata edits
    /// keep the renderer; other scopes also replace it.
    /// Comment and form edits skip the full re-inspection (seconds on very large files): the
    /// comment list and field values are re-read from the object graph instead.
    fn refresh_scoped(
        doc: &mut Document,
        scope: Scope,
        edit: Option<&Edit>,
        simple_field_applied: bool,
        xfa_changed_fields: &std::collections::BTreeSet<String>,
        known_form_changes: Option<&std::collections::HashSet<String>>,
    ) -> Result<(), EditError> {
        let known_xfa_fields = (simple_field_applied && doc.info.xfa.is_some()).then_some(xfa_changed_fields);
        if scope == Scope::Form
            && let Some(edit) = edit
            && simple_field_applied
            && Self::refresh_simple_field_edit(doc, edit)?
        {
            return Ok(());
        }
        if scope == Scope::Form
            && let Some(edit) = edit
            && Self::refresh_field_edit(doc, edit, known_xfa_fields, known_form_changes)?
        {
            return Ok(());
        }
        if scope == Scope::Full || doc.info.encrypted != doc.editor.as_ref().is_some_and(|e| e.cos.output_handler().is_some()) {
            return Self::refresh(doc);
        }
        if scope == Scope::Content {
            return Self::refresh_content(doc, None);
        }
        if scope == Scope::Links {
            return Self::refresh_links(doc);
        }
        // Metadata edits keep the renderer, which holds the working file anyway.
        if scope != Scope::Metadata {
            Self::release_working(doc);
        }
        let Some(editor) = doc.editor.as_mut() else { return Ok(()) };
        // Comment edits, and form edits (a script may change annotation properties), may change
        // annotation dictionaries, invalidating cached appearances.
        if matches!(scope, Scope::Comments | Scope::Form) {
            editor.appearances.clear();
        }
        let bytes = if editor.cos.is_modified() {
            Arc::new(write_incremental(&editor.cos, &SaveOptions::default()).map_err(|e| EditError::Write(e.to_string()))?)
        } else {
            editor.cos.bytes().clone()
        };
        if scope == Scope::Metadata {
            for (field, value) in info_fields(&mut doc.info, &editor.cos) {
                *field = value;
            }
            doc.info.custom = pdfcraft_organize::custom_info(&editor.cos);
            doc.info.file_size = bytes.len();
            #[cfg(test)]
            check_refresh_panic();
            if !doc.signatures.is_empty() {
                let signatures = signatures_of(&editor.cos, &bytes, &doc.trust, &doc.sig_cache);
                doc.signatures = Arc::new(signatures);
            }
            doc.bytes = bytes;
            return Ok(());
        }
        let mut form = pdfcraft_forms::fields(&editor.cos);
        xfa::mark_script_buttons(&editor.cos, &mut form);
        let form = Arc::new(form);
        match scope {
            Scope::Metadata => return Ok(()),
            Scope::Comments => {
                doc.info.annotations = comment_list(&editor.cos);
                // Link indexes are positions in /Annots, so comment insertion/deletion can move them.
                doc.links = OnceLock::new();
            }
            Scope::Form => {
                // PERF: keystroke path, index once. A linear find per field is O(F^2);
                // forms reach 10k fields.
                let mut by_name = std::collections::HashMap::with_capacity(form.len());
                for x in form.iter() {
                    // First match wins (do not simplify to collect: it keeps the last).
                    by_name.entry(x.name.as_str()).or_insert(x);
                }
                for f in &mut doc.info.fields {
                    if let Some(ff) = by_name.get(f.name.as_str()) {
                        f.value = match ff.kind {
                            pdfcraft_forms::FieldKind::CheckBox | pdfcraft_forms::FieldKind::Radio => {
                                Some(ff.value.first().cloned().unwrap_or_else(|| "Off".into()))
                            }
                            _ if ff.value.is_empty() => None,
                            _ => Some(ff.value.join(", ")),
                        };
                    }
                }
            }
            Scope::Links => return Self::refresh_links(doc),
            // Handled above; kept as a full refresh so the match stays total.
            Scope::Full => return Self::refresh(doc),
            // Handled above (content-only refresh); the arm keeps the match total.
            Scope::Content => return Self::refresh_content(doc, None),
        }
        doc.info.file_size = bytes.len();
        doc.form = form;
        let signatures = if doc.signatures.is_empty() { Vec::new() } else { signatures_of(&editor.cos, &bytes, &doc.trust, &doc.sig_cache) };
        doc.display = display_bytes(Some(editor), &bytes);
        doc.signatures = Arc::new(signatures);
        doc.renderer = RenderPool::new(doc.display.clone(), render_threads(), display_config(&doc.config));
        doc.bytes = bytes;
        Ok(())
    }

    /// The actionless setter can change only one field's value, so update its cached field and
    /// appearance directly. Actionful edits keep the full field read below because scripts may
    /// change other fields, flags, and widget visibility.
    fn refresh_simple_field_edit(doc: &mut Document, edit: &Edit) -> Result<bool, EditError> {
        let Edit::SetFieldValue { name, .. } = edit else { return Ok(false) };
        if doc.info.encrypted || doc.info.xfa.is_some() || !doc.signatures.is_empty() {
            return Ok(false);
        }
        let mut matches = doc.form.iter().enumerate().filter(|(_, field)| field.name == *name);
        let Some((index, old_field)) = matches.next() else { return Ok(false) };
        if matches.next().is_some() {
            return Ok(false);
        }
        let old_field = old_field.clone();
        let updated = {
            let Some(editor) = doc.editor.as_ref() else { return Ok(false) };
            pdfcraft_forms::current_field_value(&editor.cos, &old_field)
        };
        if old_field.value == updated.value && old_field.widgets.iter().zip(&updated.widgets).all(|(a, b)| a.state == b.state) {
            return Ok(false);
        }

        let source_revision_display = Arc::ptr_eq(&doc.display, &doc.bytes);
        let mut display = if source_revision_display {
            let Some(editor) = doc.editor.as_ref() else { return Ok(false) };
            editor.cos.clone()
        } else {
            match pdfcraft_cos::Document::open_with_password(doc.display.clone(), doc.password.as_deref()) {
                Ok(display) => display,
                Err(_) => return Ok(false),
            }
        };
        {
            let Some(editor) = doc.editor.as_mut() else { return Ok(false) };
            let updated = if source_revision_display {
                pdfcraft_forms::update_field_display_from_source_revision(&editor.cos, &mut display, &updated, &mut editor.field_appearances)
            } else {
                pdfcraft_forms::update_field_display(&editor.cos, &mut display, &updated, &mut editor.field_appearances)
            };
            if !updated {
                return Ok(false);
            }
        }

        Self::release_working(doc);
        let Some(editor) = doc.editor.as_ref() else { return Ok(true) };
        let bytes = if editor.cos.is_modified() {
            Arc::new(write_incremental(&editor.cos, &SaveOptions::default()).map_err(|error| EditError::Write(error.to_string()))?)
        } else {
            editor.cos.bytes().clone()
        };
        let display = match write_incremental(&display, &SaveOptions::default()) {
            Ok(display) => Arc::new(display),
            Err(_) => return Ok(false),
        };

        if let Some(field) = Arc::make_mut(&mut doc.form).get_mut(index) {
            *field = updated.clone();
        }
        if let Some(field) = doc.info.fields.iter_mut().find(|field| same_form_name(&field.name, name)) {
            field.value = match updated.kind {
                pdfcraft_forms::FieldKind::CheckBox | pdfcraft_forms::FieldKind::Radio => {
                    Some(updated.value.first().cloned().unwrap_or_else(|| "Off".into()))
                }
                _ if updated.value.is_empty() => None,
                _ => Some(updated.value.join(", ")),
            };
        }
        doc.info.file_size = bytes.len();
        doc.display = display;
        doc.renderer = RenderPool::new(doc.display.clone(), render_threads(), display_config(&doc.config));
        doc.bytes = bytes;
        Ok(true)
    }

    /// A field edit changes a small part of the form. Keep the existing display revision and
    /// replace the appearances for fields whose values changed, including calculation results.
    fn refresh_field_edit(
        doc: &mut Document,
        edit: &Edit,
        xfa_changed_fields: Option<&std::collections::BTreeSet<String>>,
        known_form_changes: Option<&std::collections::HashSet<String>>,
    ) -> Result<bool, EditError> {
        let Edit::SetFieldValue { name, .. } = edit else { return Ok(false) };
        if doc.info.encrypted || !doc.signatures.is_empty() {
            return Ok(false);
        }
        if doc.info.fields.len() != doc.form.len() {
            return Ok(false);
        }
        if doc.info.fields.iter().zip(doc.form.iter()).any(|(info, form)| !same_form_name(&info.name, &form.name)) {
            return Ok(false);
        }
        let mut changed_indices = Vec::new();
        {
            let Some(editor) = doc.editor.as_ref() else { return Ok(false) };
            for (index, field) in Arc::make_mut(&mut doc.form).iter_mut().enumerate() {
                if xfa_changed_fields.is_some_and(|names| field.name != *name && !names.contains(&field.name)) {
                    continue;
                }
                if xfa_changed_fields.is_none() && known_form_changes.is_some_and(|names| !names.contains(&field.name)) {
                    continue;
                }
                let old_value = field.value.clone();
                let old_flags = field.flags;
                let old_widgets: Vec<_> =
                    field.widgets.iter().map(|widget| (widget.state.clone(), widget.locked, widget.hidden, widget.read_only, widget.list)).collect();
                pdfcraft_forms::refresh_field_value(&editor.cos, field);
                if field.flags != old_flags
                    || field
                        .widgets
                        .iter()
                        .zip(&old_widgets)
                        .any(|(widget, old)| widget.locked != old.1 || widget.hidden != old.2 || widget.read_only != old.3)
                {
                    // The edit changed metadata outside the value cache; leave it to the full reader.
                    return Ok(false);
                }
                if old_value != field.value
                    || field.widgets.iter().zip(&old_widgets).any(|(widget, old)| widget.state != old.0 || widget.list != old.4)
                {
                    changed_indices.push(index);
                }
            }
        }
        if changed_indices.is_empty() && xfa_changed_fields.is_some() {
            Self::release_working(doc);
            let Some(editor) = doc.editor.as_ref() else { return Ok(true) };
            let bytes = if editor.cos.is_modified() {
                Arc::new(write_incremental(&editor.cos, &SaveOptions::default()).map_err(|error| EditError::Write(error.to_string()))?)
            } else {
                editor.cos.bytes().clone()
            };
            doc.info.file_size = bytes.len();
            doc.bytes = bytes;
            return Ok(true);
        }
        if changed_indices.is_empty() && known_form_changes.is_some() {
            Self::release_working(doc);
            let Some(editor) = doc.editor.as_ref() else { return Ok(true) };
            let bytes = if editor.cos.is_modified() {
                Arc::new(write_incremental(&editor.cos, &SaveOptions::default()).map_err(|error| EditError::Write(error.to_string()))?)
            } else {
                editor.cos.bytes().clone()
            };
            doc.info.file_size = bytes.len();
            doc.bytes = bytes;
            return Ok(true);
        }
        if xfa_changed_fields.is_none() && !changed_indices.iter().any(|&index| doc.form[index].name == *name) {
            return Ok(false);
        }
        let changed: Vec<_> = changed_indices.iter().map(|&index| doc.form[index].clone()).collect();

        // When the display bytes are the source bytes, the editor graph already contains the
        // updated field values. Clone that graph for the display-only appearance changes instead
        // of reparsing the whole PDF on every form edit. Reopen the display copy only when opening
        // the document had to add display-only appearances of its own.
        let source_revision_display = Arc::ptr_eq(&doc.display, &doc.bytes);
        let mut display = if source_revision_display {
            let Some(editor) = doc.editor.as_ref() else { return Ok(false) };
            editor.cos.clone()
        } else {
            match pdfcraft_cos::Document::open_with_password(doc.display.clone(), doc.password.as_deref()) {
                Ok(display) => display,
                Err(_) => return Ok(false),
            }
        };
        {
            let Some(editor) = doc.editor.as_mut() else { return Ok(false) };
            for field in &changed {
                let updated = if source_revision_display {
                    pdfcraft_forms::update_field_display_from_source_revision(&editor.cos, &mut display, field, &mut editor.field_appearances)
                } else {
                    pdfcraft_forms::update_field_display(&editor.cos, &mut display, field, &mut editor.field_appearances)
                };
                if !updated {
                    return Ok(false);
                }
            }
        }

        Self::release_working(doc);
        let Some(editor) = doc.editor.as_ref() else { return Ok(true) };
        let bytes = if editor.cos.is_modified() {
            Arc::new(write_incremental(&editor.cos, &SaveOptions::default()).map_err(|error| EditError::Write(error.to_string()))?)
        } else {
            editor.cos.bytes().clone()
        };
        let display = match write_incremental(&display, &SaveOptions::default()) {
            Ok(display) => Arc::new(display),
            Err(_) => return Ok(false),
        };
        for &index in &changed_indices {
            let field = &mut doc.info.fields[index];
            let updated = &doc.form[index];
            field.value = match updated.kind {
                pdfcraft_forms::FieldKind::CheckBox | pdfcraft_forms::FieldKind::Radio => {
                    Some(updated.value.first().cloned().unwrap_or_else(|| "Off".into()))
                }
                _ if updated.value.is_empty() => None,
                _ => Some(updated.value.join(", ")),
            };
        }
        doc.info.file_size = bytes.len();
        doc.display = display;
        doc.renderer = RenderPool::new(doc.display.clone(), render_threads(), display_config(&doc.config));
        doc.bytes = bytes;
        Ok(true)
    }

    /// Which derived lists a content-only edit can change: added content (`None`, from
    /// undo/redo, re-reads both: always correct, rarely paid).
    pub(crate) fn content_refresh_lists(edit: Option<&Edit>) -> (bool, bool) {
        if let Some(Edit::Batch { edits, .. }) = edit {
            // A batch of content edits (from a script, say) changes what its parts change.
            return edits.iter().fold((false, false), |(added, marks), part| {
                let (a, m) = Self::content_refresh_lists(Some(part));
                (added || a, marks || m)
            });
        }
        let touches_added = edit.is_none_or(|e| {
            matches!(
                e,
                Edit::AddText { .. }
                    | Edit::AddImage { .. }
                    | Edit::UpdateContent { .. }
                    | Edit::DeleteContent { .. }
                    | Edit::ReplaceImage { .. }
                    | Edit::PasteContent { .. }
                    | Edit::EditPageImage { .. }
            ) || matches!(e, Edit::MoveObjects { objects, .. } if objects.iter().any(|o| o.kind == ObjectKind::Added))
        });
        let touches_marks = edit.is_none_or(|e| {
            matches!(e, Edit::AddHeaderFooter { .. } | Edit::AddWatermark { .. } | Edit::AddBackground { .. } | Edit::RemoveMarks { .. })
        });
        (touches_added, touches_marks)
    }

    /// Rebuild working bytes, display bytes and renderer after a content-only edit: page
    /// content streams, resources and added content/marks. The page tree, annotations, form
    /// fields, links, outline and fonts are unchanged, so the full re-inspection (seconds on
    /// very large files) is skipped. `edit` selects which derived lists are re-read (`None`,
    /// from undo/redo, re-reads both: always correct, rarely paid). The cached display
    /// appearances are kept: content edits leave every annotation dictionary alone.
    fn refresh_content(doc: &mut Document, edit: Option<&Edit>) -> Result<(), EditError> {
        let display_is_source = Arc::ptr_eq(&doc.display, &doc.bytes);
        Self::release_working(doc);
        sync_xfa_state(doc);
        let Some(editor) = doc.editor.as_mut() else { return Ok(()) };
        let bytes = if editor.cos.is_modified() {
            Arc::new(write_incremental(&editor.cos, &SaveOptions::default()).map_err(|e| EditError::Write(e.to_string()))?)
        } else {
            editor.cos.bytes().clone()
        };
        // Validate the written xref chain and catalog without re-materializing the unchanged
        // page tree: Hayro's constructor eagerly walks every page.
        pdfcraft_render::check_open(bytes.clone(), doc.password.as_deref()).map_err(|e| EditError::Reopen(e.to_string()))?;
        let (touches_added, touches_marks) = Self::content_refresh_lists(edit);
        if touches_added || touches_marks {
            doc.marker_scan = OnceLock::new();
        }
        doc.info.file_size = bytes.len();
        let signatures = if doc.signatures.is_empty() {
            Vec::new()
        } else {
            crate::signatures_after_content_edit(&editor.cos, &bytes, &doc.trust, &doc.sig_cache, &doc.signatures)
        };
        doc.display = if display_is_source { bytes.clone() } else { display_bytes(Some(editor), &bytes) };
        doc.signatures = Arc::new(signatures);
        let render_config = display_config(&doc.config);
        let renderer = RenderPool::new_deferred(doc.display.clone(), render_threads(), render_config);
        doc.renderer = renderer;
        doc.bytes = bytes;
        doc.fonts_stale = true;
        Ok(())
    }

    /// Let go of the working file before the next one is written (gap 13). The working file is
    /// the original plus one appended revision, so on a 1 GiB file keeping the old one (in the
    /// working bytes, the display bytes and the renderer) while writing the new one held three
    /// copies at once; this keeps two. Until the refresh finishes, the working and display bytes
    /// are the original's (already held by the editor, so free) and the renderer draws nothing.
    /// A failed edit rolls back and refreshes again, which replaces all three; where a refresh
    /// still fails, [`Self::show_released`] draws the original rather than nothing. Save writes
    /// from the editor, never from these.
    pub(crate) fn release_working(doc: &mut Document) {
        let Some(original) = doc.editor.as_ref().map(|e| e.cos.bytes().clone()) else { return };
        // Dropping the pool stops its workers; each lets go of the file when it exits.
        doc.renderer = RenderPool::new_inline(Arc::new(Vec::new()), doc.config.clone());
        doc.display = original.clone();
        doc.bytes = original;
        #[cfg(test)]
        check_refresh_panic();
    }

    /// After a refresh failed: draw the display bytes it left (the original, once released),
    /// so the page still shows a document.
    pub(super) fn show_released(doc: &mut Document) {
        doc.renderer = RenderPool::new(doc.display.clone(), render_threads(), display_config(&doc.config));
    }

    /// Rebuild working bytes, inspection and renderer from the current edit state.
    pub(crate) fn refresh(doc: &mut Document) -> Result<(), EditError> {
        Self::refresh_with_indexes(doc, true, true)
    }

    /// Reinspect a link edit while preserving the page-content marker scan cache.
    fn refresh_links(doc: &mut Document) -> Result<(), EditError> {
        Self::refresh_with_indexes(doc, false, true)
    }

    fn refresh_with_indexes(doc: &mut Document, invalidate_markers: bool, invalidate_links: bool) -> Result<(), EditError> {
        // A full refresh may follow structural or annotation changes: cached appearances
        // may be stale, so they are rebuilt below.
        if let Some(editor) = doc.editor.as_mut() {
            editor.appearances.clear();
        }
        Self::release_working(doc);
        sync_xfa_state(doc);
        let Some(editor) = doc.editor.as_mut() else { return Ok(()) };
        let bytes = if editor.cos.is_modified() {
            Arc::new(write_incremental(&editor.cos, &SaveOptions::default()).map_err(|e| EditError::Write(e.to_string()))?)
        } else {
            editor.cos.bytes().clone()
        };
        let info = inspect(bytes.clone(), doc.password.as_deref()).map_err(|e| EditError::Reopen(e.to_string()))?;
        // Keep the user's layer choices where the layers still exist.
        let mut info = info;
        for l in &mut info.layers {
            if let Some(old) = doc.info.layers.iter().find(|o| o.id == l.id) {
                l.visible = old.visible;
            }
        }
        note_warnings(&mut info.warnings, &doc.xfa_warnings);
        // As in `build_document`: the inspection reads only `/Info`, so fields stated only in XMP
        // are filled in again after every full refresh.
        for (field, value) in info_fields(&mut info, &editor.cos) {
            if field.is_none() {
                *field = value;
            }
        }
        doc.info = info;
        doc.fonts_stale = false;
        let mut form = pdfcraft_forms::fields(&editor.cos);
        xfa::mark_script_buttons(&editor.cos, &mut form);
        doc.form = Arc::new(form);
        if invalidate_markers {
            doc.marker_scan = OnceLock::new();
        }
        if invalidate_links {
            doc.links = OnceLock::new();
        }
        let signatures = signatures_of(&editor.cos, &bytes, &doc.trust, &doc.sig_cache);
        doc.display = display_bytes(Some(editor), &bytes);
        doc.signatures = Arc::new(signatures);
        doc.renderer = RenderPool::new(doc.display.clone(), render_threads(), display_config(&doc.config));
        doc.bytes = bytes;
        Ok(())
    }
}

/// What `edit`, taking `prev` to `next`, wants said: never silent (AGENTS.md §4).
fn edit_notices(
    edit: &Edit,
    prev: &pdfcraft_cos::Document,
    next: &pdfcraft_cos::Document,
    scope: Scope,
    form: &[pdfcraft_forms::Field],
    known_form_changes: Option<&std::collections::HashSet<String>>,
) -> Vec<EditNotice> {
    // What it drew with `?` (a value or comment text its font can't draw, stored as typed).
    let mut notices: Vec<EditNotice> = if let (Scope::Form, Some(changed_fields)) = (scope, known_form_changes) {
        guard(|| undrawn_notices_for_fields(prev, next, form, changed_fields)).unwrap_or_default().into_iter().map(EditNotice::Undrawn).collect()
    } else if matches!(scope_of(edit), Scope::Form | Scope::Comments | Scope::Full) {
        guard(|| undrawn_notices(prev, next)).unwrap_or_default().into_iter().map(EditNotice::Undrawn).collect()
    } else {
        Vec::new()
    };
    // Pages whose own white backdrop Remove background couldn't turn back on.
    if matches!(edit, Edit::RemoveMarks { kind: MarkKind::Background }) {
        let pages = guard(|| pdfcraft_edit::backdrops_left_off(next)).unwrap_or_default();
        if !pages.is_empty() {
            notices.push(EditNotice::BackdropsLeftOff { pages });
        }
    }
    notices
}
