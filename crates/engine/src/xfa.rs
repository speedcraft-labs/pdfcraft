//! XFA scripting in the engine: a form's scripted events (initialize and calculate on open,
//! exit, validate and calculate after a field changes, click for buttons) run through
//! [`pdfcraft_js::xfa`] over the live form tree [`pdfcraft_xfa::form_tree`] builds, and what
//! they change or ask for is applied here: values go to the fields (and so to the datasets),
//! presence and access changes become overrides kept in the PDF, added or removed rows change
//! the datasets, and any of those lays the form out again.

use std::collections::{HashMap, HashSet};
use std::sync::Arc;

use pdfcraft_js::xfa::{XfaDoc, XfaEffect, XfaEvent, XfaKind, XfaNode};
use pdfcraft_js::xfa_session::{Script, XfaSession};
use pdfcraft_xfa::model::Template;
use pdfcraft_xfa::{FormNode, LiveForm, NodeKind, ScriptEvent};

use crate::js::JsOutput;
use crate::js::Request;

/// Most scripts run for one event (calculates of a whole form, say).
const MAX_SCRIPTS_PER_EVENT: usize = 2_000;
/// Most times the form is laid out again for one event (a script adding rows in a loop).
const MAX_RELAYOUTS_PER_EVENT: usize = 50;
/// Most message boxes, console lines and errors kept for one event.
const MAX_ALERTS_PER_EVENT: usize = 100;
const MAX_CONSOLE_PER_EVENT: usize = 1_000;
const MAX_ERRORS_PER_EVENT: usize = 100;
/// Time the scripts of one event may take in all before the rest are skipped: opening (and
/// the calculations every change runs) is held tighter than a click.
const OPEN_BUDGET_MS: u64 = 3_000;
const EVENT_BUDGET_MS: u64 = 5_000;

/// Engine limits for one script: initialize, calculate and validate scripts run without the
/// user asking (on open, on every change), so they get fewer loop iterations than a click.
fn limits_for(activity: &str) -> pdfcraft_js::Limits {
    match activity {
        "click" | "change" | "exit" | "enter" => pdfcraft_js::Limits::default(),
        _ => pdfcraft_js::Limits { loop_iterations: 100_000, recursion: 64 },
    }
}

/// Longest one script may run before it is abandoned (it can't be interrupted, only left
/// behind): scripts that run unasked are held tighter than a click's.
fn timeout_for(activity: &str) -> std::time::Duration {
    std::time::Duration::from_millis(match activity {
        "click" | "change" | "exit" | "enter" => 3_000,
        _ => 1_000,
    })
}

/// Requests only a click may make (anything else asking is noted and ignored).
fn click_only(e: &XfaEffect) -> Option<&'static str> {
    match e {
        XfaEffect::Print => Some("print"),
        XfaEffect::SaveAs => Some("save"),
        XfaEffect::LaunchUrl(_) => Some("open a link"),
        XfaEffect::ShowPanel(_) => Some("show a side panel"),
        XfaEffect::SetFocus(_) => Some("move the focus"),
        _ => None,
    }
}

/// A wall clock where there is one (not in the browser build, where the script and relayout
/// caps alone bound an event).
#[derive(Clone, Copy)]
struct Clock {
    #[cfg(not(target_arch = "wasm32"))]
    started: std::time::Instant,
    budget_ms: u64,
}

impl Clock {
    fn start(budget_ms: u64) -> Self {
        Clock {
            #[cfg(not(target_arch = "wasm32"))]
            started: std::time::Instant::now(),
            budget_ms,
        }
    }

    fn spent(&self) -> bool {
        #[cfg(not(target_arch = "wasm32"))]
        {
            self.started.elapsed() >= std::time::Duration::from_millis(self.budget_ms)
        }
        #[cfg(target_arch = "wasm32")]
        {
            let _ = self.budget_ms;
            false
        }
    }
}

/// What the scripts of an event changed in the form (for the note opening leaves).
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub(crate) struct Changes {
    pub values: usize,
    pub rows: usize,
    pub visibility: usize,
}

impl Changes {
    pub fn is_empty(&self) -> bool {
        self.values == 0 && self.rows == 0 && self.visibility == 0
    }

    /// "3 values, 1 row" — what changed, for a note.
    pub fn describe(&self) -> String {
        let mut parts = Vec::new();
        let n = |k: usize, one: &str, many: &str| format!("{k} {}", if k == 1 { one } else { many });
        if self.values > 0 {
            parts.push(n(self.values, "field value", "field values"));
        }
        if self.rows > 0 {
            parts.push(n(self.rows, "added or removed row", "added or removed rows"));
        }
        if self.visibility > 0 {
            parts.push(n(self.visibility, "shown, hidden or locked object", "shown, hidden or locked objects"));
        }
        parts.join(", ")
    }
}

/// What a run of the form's scripts shares with the edit around it.
pub(crate) struct XfaRun<'a> {
    pub page_count: usize,
    pub out: &'a mut JsOutput,
    /// The datasets stream this edit already wrote, replaced in place by later writes.
    pub datasets: Option<pdfcraft_cos::ObjRef>,
    /// AcroForm values written by XFA scripts during this event.
    pub changed_fields: std::collections::BTreeSet<String>,
    /// The event changed layout or field visibility, so the caller must rebuild the view.
    pub structure_changed: bool,
    /// A script ran past its time limit and was abandoned: the form's scripts should stay off.
    pub ran_away: bool,
    /// What every time limit of the scripts is multiplied by (the event's budget, the engine's
    /// start and each script's own): 1, unless the session was given another
    /// ([`crate::Session::with_xfa_time_scale`]).
    pub time_scale: u32,
}

/// The live form node `n`, with its descendants, as the script engine's snapshot of it.
fn to_js(n: &FormNode) -> XfaNode {
    XfaNode {
        name: n.name.clone(),
        som: n.som.clone(),
        kind: n.kind.map(|k| match k {
            NodeKind::Subform => XfaKind::Subform,
            NodeKind::Field => XfaKind::Field,
            NodeKind::ExclGroup => XfaKind::ExclGroup,
            NodeKind::Draw => XfaKind::Draw,
            NodeKind::Area => XfaKind::Area,
        }),
        value: n.value.clone(),
        numeric: n.numeric,
        presence: n.presence.clone(),
        access: n.access.clone(),
        repeatable: n.repeatable,
        occur_min: n.occur_min,
        occur_max: n.occur_max,
        index: n.index,
        children: n.children.iter().map(to_js).collect(),
        formatted: n.display_picture.as_deref().and_then(|p| pdfcraft_xfa::formatted_value(p, &n.value)),
    }
}

fn find_container<'a>(n: &'a FormNode, som: &str, depth: usize) -> Option<&'a FormNode> {
    if depth > 64 {
        return None;
    }
    if n.som == som && n.kind != Some(NodeKind::Field) {
        return Some(n);
    }
    n.children.iter().find_map(|c| find_container(c, som, depth + 1))
}

fn count_instances(root: &FormNode, parent_som: &str, name: &str) -> usize {
    let parent = if parent_som.is_empty() { Some(root) } else { find_container(root, parent_som, 0) };
    parent.map_or(0, |p| p.children.iter().filter(|c| c.name == name).count())
}

/// An instance SOM (`…table[0].row[2]`) split into its parent's data path (areas left out,
/// see [`pdfcraft_xfa::data_path`]) and SOM, its name and its index.
fn split_instance(tpl: &Template, som: &str) -> Option<(pdfcraft_xfa::DataPath, String, String, usize)> {
    let mut path = pdfcraft_xfa::data_path(tpl, som);
    let (name, index) = path.pop()?;
    let parent_som = som.rsplit_once('.').map(|(p, _)| p).unwrap_or("").to_string();
    Some((path, parent_som, name, index))
}

/// A SOM path without its indexes (`form.page1.qty`), for matching names scripts pass.
fn plain_som(som: &str) -> String {
    let mut out = String::with_capacity(som.len());
    let mut skip = false;
    for c in som.chars() {
        match c {
            '[' => skip = true,
            ']' => skip = false,
            _ if !skip => out.push(c),
            _ => {}
        }
    }
    out
}

#[derive(Clone, Debug, PartialEq, Eq)]
enum CalcToken {
    Ident(String),
    Symbol(char),
    Literal,
}

/// Tokenize the deliberately small, side-effect-free subset for which a calculation's field
/// reads can be proven. Anything unfamiliar makes the caller run every calculation.
fn calc_tokens(source: &str) -> Option<Vec<CalcToken>> {
    let bytes = source.as_bytes();
    let mut tokens = Vec::new();
    let mut at = 0usize;
    while at < bytes.len() {
        let byte = *bytes.get(at)?;
        if byte.is_ascii_whitespace() {
            at += 1;
        } else if bytes.get(at..at + 2) == Some(b"//") {
            at += 2;
            while at < bytes.len() && bytes.get(at) != Some(&b'\n') {
                at += 1;
            }
        } else if bytes.get(at..at + 2) == Some(b"/*") {
            at += 2;
            let tail = bytes.get(at..)?;
            let end = tail.windows(2).position(|window| window == b"*/")?;
            at += end + 2;
        } else if byte == b'\'' || byte == b'"' {
            let quote = byte;
            at += 1;
            let mut closed = false;
            while at < bytes.len() {
                match bytes.get(at).copied()? {
                    b'\\' => at = at.saturating_add(2),
                    value if value == quote => {
                        at += 1;
                        closed = true;
                        break;
                    }
                    b'\n' | b'\r' => return None,
                    _ => at += 1,
                }
            }
            if !closed {
                return None;
            }
            tokens.push(CalcToken::Literal);
        } else if byte.is_ascii_alphabetic() || byte == b'_' || byte == b'$' {
            let start = at;
            at += 1;
            while bytes.get(at).is_some_and(|b| b.is_ascii_alphanumeric() || *b == b'_' || *b == b'$') {
                at += 1;
            }
            tokens.push(CalcToken::Ident(std::str::from_utf8(bytes.get(start..at)?).ok()?.to_string()));
        } else if byte.is_ascii_digit() {
            at += 1;
            while bytes.get(at).is_some_and(|b| b.is_ascii_alphanumeric() || matches!(*b, b'.' | b'_')) {
                at += 1;
            }
            tokens.push(CalcToken::Literal);
        } else if byte.is_ascii() {
            tokens.push(CalcToken::Symbol(char::from(byte)));
            at += 1;
        } else {
            return None;
        }
    }
    Some(tokens)
}

const CALC_BUILTINS: &[&str] = &[
    "abs",
    "at",
    "avg",
    "ceil",
    "choose",
    "concat",
    "count",
    "date2num",
    "datefmt",
    "exists",
    "floor",
    "hasvalue",
    "isodate2num",
    "left",
    "len",
    "lower",
    "ltrim",
    "max",
    "min",
    "mod",
    "num2date",
    "oneof",
    "parse",
    "replace",
    "right",
    "round",
    "rtrim",
    "space",
    "str",
    "stuff",
    "substr",
    "sum",
    "upper",
    "within",
    "wordnum",
    "Number",
    "String",
    "parseInt",
    "parseFloat",
    "isNaN",
    "Math",
    "return",
    "and",
    "or",
    "not",
    "true",
    "false",
    "null",
    "undefined",
    "NaN",
    "Infinity",
    "this",
    "rawValue",
    "value",
];

fn is_calc_builtin(name: &str) -> bool {
    CALC_BUILTINS.contains(&name) || {
        let lower = name.to_ascii_lowercase();
        CALC_BUILTINS.contains(&lower.as_str())
    }
}

fn is_calc_dynamic(name: &str) -> bool {
    matches!(
        name,
        "xfa"
            | "event"
            | "resolveNode"
            | "resolveNodes"
            | "somExpression"
            | "parent"
            | "instanceManager"
            | "presence"
            | "access"
            | "layout"
            | "host"
            | "datasets"
            | "data"
            | "form"
            | "template"
            | "execEvent"
            | "eval"
            | "uuid"
            | "get"
            | "post"
            | "put"
            | "now"
            | "today"
            | "locale"
            | "date"
            | "time"
            | "var"
            | "let"
            | "const"
            | "function"
            | "if"
            | "else"
            | "for"
            | "while"
            | "switch"
            | "try"
            | "catch"
            | "throw"
            | "new"
    )
}

/// Field dependencies for one calculate script. This intentionally accepts only direct field
/// references and deterministic expressions; XFA navigation, control flow, dynamic indexing,
/// side effects, and unknown identifiers keep the full-form calculation path.
struct CalcFieldIndex {
    by_suffix: HashMap<String, Vec<String>>,
    by_leaf: HashMap<String, Vec<String>>,
}

impl CalcFieldIndex {
    fn new(fields: &HashSet<String>) -> Self {
        let mut by_suffix: HashMap<String, Vec<String>> = HashMap::new();
        let mut by_leaf: HashMap<String, Vec<String>> = HashMap::new();
        for som in fields {
            let plain = plain_som(som);
            let parts: Vec<_> = plain.split('.').collect();
            for start in 0..parts.len() {
                by_suffix.entry(parts[start..].join(".")).or_default().push(som.clone());
            }
            if let Some(leaf) = parts.last() {
                by_leaf.entry((*leaf).to_string()).or_default().push(som.clone());
            }
        }
        Self { by_suffix, by_leaf }
    }
}

#[cfg(test)]
fn calc_script_reads(source: &str, target: &str, fields: &HashSet<String>) -> Option<HashSet<String>> {
    calc_script_reads_indexed(source, target, &CalcFieldIndex::new(fields))
}

fn calc_script_reads_indexed(source: &str, target: &str, fields: &CalcFieldIndex) -> Option<HashSet<String>> {
    let tokens = calc_tokens(source)?;
    let mut field_references = HashMap::new();
    let mut qualified_parts = HashSet::new();
    for (index, token) in tokens.iter().enumerate() {
        let CalcToken::Ident(name) = token else { continue };
        let Some(candidates) = fields.by_leaf.get(name) else { continue };
        let mut path = vec![name.as_str()];
        let mut cursor = index;
        while cursor >= 2
            && matches!(tokens.get(cursor - 1), Some(CalcToken::Symbol('.')))
            && matches!(tokens.get(cursor - 2), Some(CalcToken::Ident(_)))
        {
            let Some(CalcToken::Ident(prefix)) = tokens.get(cursor - 2) else { break };
            path.push(prefix.as_str());
            cursor -= 2;
        }
        path.reverse();
        let qualified = path.join(".");
        let som = if path.len() > 1 {
            fields.by_suffix.get(&qualified).and_then(|items| (items.len() == 1).then(|| items.first().cloned()).flatten())?
        } else {
            (candidates.len() == 1).then(|| candidates.first().cloned()).flatten()?
        };
        field_references.insert(index, som);
        for part in cursor..=index {
            if matches!(tokens.get(part), Some(CalcToken::Ident(_))) {
                qualified_parts.insert(part);
            }
        }
    }
    let mut reads = HashSet::new();
    for (index, token) in tokens.iter().enumerate() {
        let CalcToken::Ident(name) = token else {
            if matches!(token, CalcToken::Symbol('[' | ']' | '{' | '}')) || (matches!(token, CalcToken::Symbol(';')) && index + 1 != tokens.len()) {
                return None;
            }
            continue;
        };
        if is_calc_dynamic(name) || name.starts_with('$') {
            return None;
        }

        if name == "this" {
            let is_own_value = matches!(tokens.get(index + 1), Some(CalcToken::Symbol('.')))
                && matches!(tokens.get(index + 2), Some(CalcToken::Ident(property)) if property == "rawValue" || property == "value");
            if !is_own_value {
                return None;
            }
            reads.insert(target.to_string());
            continue;
        }
        if name == "rawValue" || name == "value" {
            if !matches!(index.checked_sub(1).and_then(|i| tokens.get(i)), Some(CalcToken::Symbol('.'))) {
                return None;
            }
            if matches!(tokens.get(index + 1), Some(CalcToken::Symbol('.'))) {
                return None;
            }
            continue;
        }

        if let Some(som) = field_references.get(&index) {
            reads.insert(som.clone());
            if assignment_after_field(&tokens, index) && som != target {
                return None;
            }
        } else if !is_calc_builtin(name) && !qualified_parts.contains(&index) {
            return None;
        }

        if matches!(tokens.get(index + 1), Some(CalcToken::Symbol('('))) && !is_calc_builtin(name) {
            return None;
        }
    }
    reads.insert(target.to_string());
    Some(reads)
}

fn assignment_after_field(tokens: &[CalcToken], index: usize) -> bool {
    if index >= 2
        && matches!(
            (tokens.get(index - 2), tokens.get(index - 1)),
            (Some(CalcToken::Symbol('+')), Some(CalcToken::Symbol('+'))) | (Some(CalcToken::Symbol('-')), Some(CalcToken::Symbol('-')))
        )
    {
        return true;
    }
    let mut at = index.saturating_add(1);
    if matches!(tokens.get(at), Some(CalcToken::Symbol('.'))) {
        at = at.saturating_add(1);
        if !matches!(tokens.get(at), Some(CalcToken::Ident(property)) if property == "rawValue" || property == "value") {
            return false;
        }
        at = at.saturating_add(1);
    }
    let symbol = |offset| match tokens.get(at + offset) {
        Some(CalcToken::Symbol(value)) => Some(*value),
        _ => None,
    };
    if matches!(
        (symbol(0), symbol(1)),
        (Some('+'), Some('='))
            | (Some('-'), Some('='))
            | (Some('*'), Some('='))
            | (Some('/'), Some('='))
            | (Some('%'), Some('='))
            | (Some('&'), Some('='))
            | (Some('|'), Some('='))
            | (Some('^'), Some('='))
            | (Some('+'), Some('+'))
            | (Some('-'), Some('-'))
    ) || matches!(
        (symbol(0), symbol(1), symbol(2)),
        (Some('*'), Some('*'), Some('='))
            | (Some('&'), Some('&'), Some('='))
            | (Some('|'), Some('|'), Some('='))
            | (Some('?'), Some('?'), Some('='))
            | (Some('<'), Some('<'), Some('='))
            | (Some('>'), Some('>'), Some('='))
    ) || matches!((symbol(0), symbol(1), symbol(2), symbol(3)), (Some('>'), Some('>'), Some('>'), Some('=')))
    {
        return true;
    }
    symbol(0) == Some('=')
        && !matches!(symbol(1), Some('=' | '>'))
        && !matches!(at.checked_sub(1).and_then(|i| tokens.get(i)), Some(CalcToken::Symbol('=' | '!' | '<' | '>')))
}

struct Calculation {
    event: ScriptEvent,
    dependencies: HashSet<String>,
}

pub(crate) struct CalculationPlan {
    calculations: Vec<Calculation>,
    som_by_name: HashMap<String, HashSet<String>>,
    som_by_path: HashMap<pdfcraft_xfa::data::DataPath, HashSet<String>>,
    path_by_som: HashMap<String, pdfcraft_xfa::data::DataPath>,
}

impl CalculationPlan {
    fn build(live: &LiveForm, tpl: &Template, by_som: &HashMap<String, String>) -> Option<Self> {
        if live.truncated {
            return None;
        }
        fn collect_fields(node: &FormNode, fields: &mut HashSet<String>, depth: usize) -> bool {
            if depth > 64 {
                return false;
            }
            if node.kind == Some(NodeKind::Field) {
                fields.insert(node.som.clone());
            }
            node.children.iter().all(|child| collect_fields(child, fields, depth + 1))
        }
        let mut fields = HashSet::new();
        if !collect_fields(&live.root, &mut fields, 0) {
            return None;
        }
        let field_index = CalcFieldIndex::new(&fields);
        let mut calculations = Vec::new();
        for event in live.events.iter().filter(|event| event.activity == "calculate") {
            if !fields.contains(&event.som) {
                return None;
            }
            let dependencies = calc_script_reads_indexed(&event.script, &event.som, &field_index)?;
            calculations.push(Calculation { event: event.clone(), dependencies });
        }
        let mut som_by_name: HashMap<String, HashSet<String>> = HashMap::new();
        let mut som_by_path: HashMap<pdfcraft_xfa::data::DataPath, HashSet<String>> = HashMap::new();
        let mut path_by_som = HashMap::new();
        for som in &fields {
            if let Some(name) = by_som.get(som) {
                som_by_name.entry(name.clone()).or_default().insert(som.clone());
            }
            let path = pdfcraft_xfa::data_path(tpl, som);
            som_by_path.entry(path.clone()).or_default().insert(som.clone());
            path_by_som.insert(som.clone(), path);
        }
        Some(Self { calculations, som_by_name, som_by_path, path_by_som })
    }

    fn affected(&self, changed_names: &[String]) -> Option<HashSet<usize>> {
        if changed_names.is_empty() {
            return None;
        }
        let mut dirty = HashSet::new();
        for name in changed_names {
            dirty.extend(self.som_by_name.get(name)?.iter().cloned());
        }
        if dirty.is_empty() {
            return None;
        }
        let changed_paths: HashSet<_> = dirty.iter().filter_map(|som| self.path_by_som.get(som)).cloned().collect();
        for path in changed_paths {
            dirty.extend(self.som_by_path.get(&path)?.iter().cloned());
        }
        let mut selected = HashSet::new();
        loop {
            let mut added = false;
            for (index, calculation) in self.calculations.iter().enumerate() {
                if selected.contains(&index) || calculation.dependencies.is_disjoint(&dirty) {
                    continue;
                }
                selected.insert(index);
                let target = calculation.event.som.clone();
                dirty.insert(target.clone());
                if let Some(path) = self.path_by_som.get(&target) {
                    dirty.extend(self.som_by_path.get(path)?.iter().cloned());
                }
                added = true;
            }
            if !added {
                break;
            }
        }
        Some(selected)
    }
}

/// The stable part of an XFA field edit: which named fields have change events and which
/// calculations depend on each field. Values may change between edits; the event/dependency
/// graph does not unless the form's structure changes.
pub(crate) struct XfaEditPlan {
    calculations: Option<Arc<CalculationPlan>>,
    som_by_name: HashMap<String, HashSet<String>>,
    eventful_soms: HashSet<String>,
    has_calculations: bool,
}

impl XfaEditPlan {
    fn build(doc: &pdfcraft_cos::Document, tpl: &Template) -> Arc<Self> {
        let data = pdfcraft_xfa::data_of(doc);
        let overrides = pdfcraft_xfa::overrides(doc);
        let live = pdfcraft_xfa::form_tree(tpl, data.as_ref(), &overrides);
        let by_som = pdfcraft_xfa::fields_by_som(doc);
        let mut som_by_name: HashMap<String, HashSet<String>> = HashMap::new();
        for (som, name) in &by_som {
            som_by_name.entry(name.clone()).or_default().insert(som.clone());
        }
        let eventful_soms = live
            .events
            .iter()
            .filter(|event| matches!(event.activity.as_str(), "change" | "exit" | "validate"))
            .map(|event| event.som.clone())
            .collect();
        let has_calculations = live.events.iter().any(|event| event.activity == "calculate");
        let calculations = CalculationPlan::build(&live, tpl, &by_som).map(Arc::new);
        Arc::new(Self { calculations, som_by_name, eventful_soms, has_calculations })
    }

    /// Whether an ordinary field change has a known event or dependent calculation. `None`
    /// means the plan cannot prove the edit is independent, so callers must use the full path.
    pub(crate) fn needs_run(&self, names: &[String]) -> Option<bool> {
        if names.is_empty() {
            return None;
        }
        let mut eventful = false;
        for name in names {
            let soms = self.som_by_name.get(name)?;
            eventful |= soms.iter().any(|som| self.eventful_soms.contains(som));
        }
        let calculations = if self.has_calculations {
            let plan = self.calculations.as_ref()?;
            !plan.affected(names)?.is_empty()
        } else {
            false
        };
        Some(eventful || calculations)
    }
}

/// Precompute the script dependency graph while opening the document, before a user edit is
/// timed. The index is reused until a structural XFA edit changes the form instance tree.
pub(crate) fn xfa_edit_plan(doc: &pdfcraft_cos::Document, tpl: &Template) -> Arc<XfaEditPlan> {
    XfaEditPlan::build(doc, tpl)
}

/// Does `name` from a script (`qty`, `page1.qty`, `xfa.form.form.page1.qty`, or a container
/// such as `table`) name this SOM or one of its ancestors?
fn som_matches(som: &str, name: &str) -> bool {
    let plain = plain_som(som);
    let n = plain_som(name.trim());
    let n = n.strip_prefix("xfa.form.").or_else(|| n.strip_prefix("$form.")).unwrap_or(&n);
    let want: Vec<&str> = n.split('.').filter(|s| !s.is_empty()).collect();
    let have: Vec<&str> = plain.split('.').filter(|s| !s.is_empty()).collect();
    !want.is_empty() && have.windows(want.len()).any(|w| w == want.as_slice())
}

/// A generated field as the runner tracks it: kind, value and on state.
#[derive(Clone)]
struct FieldInfo {
    kind: pdfcraft_forms::FieldKind,
    value: Vec<String>,
    on_state: String,
    /// List boxes that take several values (one per line in the data).
    multi: bool,
}

fn field_infos(doc: &pdfcraft_cos::Document) -> HashMap<String, FieldInfo> {
    pdfcraft_forms::fields(doc)
        .into_iter()
        .map(|f| {
            let on_state = f.widgets.first().and_then(|w| w.on_state.clone()).unwrap_or_else(|| "1".into());
            let multi = f.kind == pdfcraft_forms::FieldKind::List && f.has(pdfcraft_forms::flags::MULTI_SELECT);
            (f.name, FieldInfo { kind: f.kind, value: f.value, on_state, multi })
        })
        .collect()
}

/// The data text of a field's value.
fn data_text(info: &FieldInfo) -> String {
    match info.kind {
        pdfcraft_forms::FieldKind::CheckBox => {
            if info.value.is_empty() {
                String::new()
            } else {
                info.on_state.clone()
            }
        }
        _ => info.value.join("\n"),
    }
}

/// Runs the scripts of one event (opening, a change, a click) against one document: what
/// they change is kept in memory (data, field values, overrides) and written once, when the
/// event ends or the form has to be laid out again.
struct Runner<'a, 'b> {
    doc: &'a mut pdfcraft_cos::Document,
    tpl: &'a Template,
    run: &'a mut XfaRun<'b>,
    edit_plan: Option<Arc<XfaEditPlan>>,
    clock: Clock,
    runs: usize,
    skipped: usize,
    relayouts: usize,
    relayouts_skipped: bool,
    dropped_output: usize,
    truncated_warned: bool,
    data: Option<pdfcraft_xfa::DataNode>,
    ov: pdfcraft_xfa::Overrides,
    ov_dirty: bool,
    pending: Vec<pdfcraft_xfa::DataOp>,
    /// Field values to write, by name (the last wins).
    field_changes: std::collections::BTreeMap<String, Vec<String>>,
    by_som: Option<HashMap<String, String>>,
    /// Widget names by the data node they bind to (built with `by_som`).
    by_path: Option<HashMap<pdfcraft_xfa::DataPath, Vec<String>>>,
    fields: Option<HashMap<String, FieldInfo>>,
    /// The live form as last built; dropped when a script changed the form.
    cached: Option<LiveForm>,
    /// The script engine for this event: one thread for all its scripts, each run in a fresh
    /// context on the form as it is then (`cached`, kept up to date in place for values and
    /// rebuilt when the structure changed).
    session: Option<XfaSession>,
    changes: Changes,
}

impl<'a, 'b> Runner<'a, 'b> {
    fn new(
        doc: &'a mut pdfcraft_cos::Document,
        tpl: &'a Template,
        run: &'a mut XfaRun<'b>,
        budget_ms: u64,
        edit_plan: Option<Arc<XfaEditPlan>>,
    ) -> Self {
        let data = pdfcraft_xfa::data_of(doc);
        let ov = pdfcraft_xfa::overrides(doc);
        let budget_ms = budget_ms.saturating_mul(u64::from(run.time_scale));
        Runner {
            doc,
            tpl,
            run,
            edit_plan,
            clock: Clock::start(budget_ms),
            runs: 0,
            skipped: 0,
            relayouts: 0,
            relayouts_skipped: false,
            dropped_output: 0,
            truncated_warned: false,
            data,
            ov,
            ov_dirty: false,
            pending: Vec::new(),
            field_changes: Default::default(),
            by_som: None,
            by_path: None,
            fields: None,
            cached: None,
            session: None,
            changes: Changes::default(),
        }
    }

    /// The live form no longer matches the data (its structure changed): rebuilt for the next
    /// script.
    fn invalidate(&mut self) {
        self.cached = None;
    }

    /// The script engine for this event.
    fn session(&mut self) -> &mut XfaSession {
        let doc_info = XfaDoc { file_name: String::new(), page: 0, page_count: self.run.page_count };
        let scale = self.run.time_scale;
        self.session.get_or_insert_with(|| XfaSession::new(doc_info).with_time_scale(scale))
    }

    /// Give the object at `som` the value `text` in the data and in the live form (refreshed
    /// in place, as the tree builder would set it; a value changes nothing else, so the tree
    /// is not rebuilt for it).
    fn value_op(&mut self, som: &str, path: pdfcraft_xfa::DataPath, text: String) {
        let op = pdfcraft_xfa::DataOp::Value { path, text };
        self.data.get_or_insert_with(Default::default).apply(&op);
        self.pending.push(op);
        let refreshed = match self.cached.as_mut() {
            Some(live) => pdfcraft_xfa::refresh_value(&mut live.root, self.tpl, self.data.as_ref(), som).is_some(),
            None => true,
        };
        if !refreshed {
            self.invalidate();
        }
    }

    fn live(&mut self) -> &LiveForm {
        if self.cached.is_none() {
            let live = pdfcraft_xfa::form_tree(self.tpl, self.data.as_ref(), &self.ov);
            if live.truncated && !self.truncated_warned {
                self.truncated_warned = true;
                self.error(format!(
                    "The form has more than {} objects or 20000 scripted events; its scripts see only the first ones",
                    pdfcraft_xfa::MAX_FORM_NODES
                ));
            }
            self.cached = Some(live);
        }
        self.cached.get_or_insert_with(Default::default)
    }

    fn by_som(&mut self) -> &HashMap<String, String> {
        if self.by_som.is_none() {
            let by_som = pdfcraft_xfa::fields_by_som(self.doc);
            let mut by_path: HashMap<pdfcraft_xfa::DataPath, Vec<String>> = HashMap::new();
            for (som, name) in &by_som {
                by_path.entry(pdfcraft_xfa::data_path(self.tpl, som)).or_default().push(name.clone());
            }
            for names in by_path.values_mut() {
                names.sort();
            }
            self.by_path = Some(by_path);
            self.by_som = Some(by_som);
        }
        self.by_som.get_or_insert_with(Default::default)
    }

    /// The widgets bound to the data node at `path`, in name order.
    fn widgets_at(&mut self, path: &pdfcraft_xfa::DataPath) -> Vec<String> {
        self.by_som();
        self.by_path.as_ref().and_then(|m| m.get(path)).cloned().unwrap_or_default()
    }

    fn fields(&mut self) -> &mut HashMap<String, FieldInfo> {
        if self.fields.is_none() {
            self.fields = Some(field_infos(self.doc));
        }
        self.fields.get_or_insert_with(Default::default)
    }

    fn error(&mut self, e: String) {
        if self.run.out.errors.len() < MAX_ERRORS_PER_EVENT {
            self.run.out.errors.push(e);
        } else {
            self.dropped_output += 1;
        }
    }

    /// Keep console `lines`, up to [`MAX_CONSOLE_PER_EVENT`] for one event; the rest are counted.
    fn console(&mut self, lines: impl IntoIterator<Item = String>) {
        for l in lines {
            if self.run.out.console.len() < MAX_CONSOLE_PER_EVENT {
                self.run.out.console.push(l);
            } else {
                self.dropped_output += 1;
            }
        }
    }

    /// Show message `m`, up to [`MAX_ALERTS_PER_EVENT`] for one event; the rest are counted.
    fn alert(&mut self, m: crate::js::Message) {
        if self.run.out.alerts.len() < MAX_ALERTS_PER_EVENT {
            self.run.out.alerts.push(m);
        } else {
            self.dropped_output += 1;
        }
    }

    /// Change the data's structure (and the in-memory copy scripts read): the form is rebuilt
    /// for the next script.
    fn data_op(&mut self, op: pdfcraft_xfa::DataOp) {
        self.run.structure_changed = true;
        self.data.get_or_insert_with(Default::default).apply(&op);
        self.pending.push(op);
        self.invalidate();
    }

    /// Give the object at `som` the value `value`: in the data, and in every widget bound to
    /// that data node (each valued by its own kind: a check box by its on state, a list box by
    /// lines). Returns whether anything changed.
    fn set_value(&mut self, som: &str, value: &str) -> bool {
        use pdfcraft_forms::FieldKind as K;
        let path = pdfcraft_xfa::data_path(self.tpl, som);
        let names = self.widgets_at(&path);
        // The data text: the widget at `som` says how its kind writes the value; without one,
        // the first widget bound to the node, else the value itself.
        let primary = self.by_som().get(som).cloned().or_else(|| names.first().cloned());
        let mut text = value.to_string();
        let mut changed = false;
        for name in &names {
            let Some(info) = self.fields().get(name).cloned() else { continue };
            let new_value: Vec<String> = match info.kind {
                K::CheckBox => {
                    if pdfcraft_xfa::data::is_on(value, &info.on_state) || value == info.on_state {
                        vec![info.on_state.clone()]
                    } else {
                        Vec::new()
                    }
                }
                K::Radio => {
                    let v = value.trim();
                    if v.is_empty() { Vec::new() } else { vec![v.to_string()] }
                }
                // Read-only fields still take values from scripts (calculated totals are
                // read-only); buttons and signatures hold none.
                K::PushButton | K::Signature => continue,
                K::List if info.multi => value.lines().map(str::trim).filter(|l| !l.is_empty()).map(str::to_string).collect(),
                _ => vec![value.to_string()],
            };
            let same = match info.kind {
                K::CheckBox => info.value.is_empty() == new_value.is_empty(),
                K::Radio => info.value.first() == new_value.first(),
                K::List if info.multi => info.value == new_value,
                _ => info.value.first().map_or(value.is_empty(), |v| v == value),
            };
            let updated = FieldInfo { value: new_value.clone(), ..info };
            if primary.as_deref() == Some(name.as_str()) {
                text = data_text(&updated);
            }
            if same {
                continue;
            }
            self.fields().insert(name.clone(), updated);
            self.field_changes.insert(name.clone(), new_value);
            self.run.changed_fields.insert(name.clone());
            changed = true;
        }
        if self.data.as_ref().and_then(|d| d.text_at(&path)).unwrap_or("") != text {
            self.value_op(som, path, text);
            changed = true;
        }
        if changed {
            self.changes.values += 1;
        }
        changed
    }

    /// Write what the scripts changed so far: the data as one rewrite, the field values, the
    /// overrides.
    fn flush(&mut self) -> Result<(), String> {
        let mut wrote = false;
        if !self.pending.is_empty() {
            let ops = std::mem::take(&mut self.pending);
            let r = pdfcraft_xfa::write_data_ops(self.doc, &ops, self.run.datasets).map_err(|e| e.to_string())?;
            if r.stream.is_some() {
                self.run.datasets = r.stream;
            }
            self.console(r.warnings);
            wrote = true;
        }
        if !self.field_changes.is_empty() {
            let changes: Vec<pdfcraft_forms::FieldChange> = std::mem::take(&mut self.field_changes)
                .into_iter()
                .map(|(name, value)| pdfcraft_forms::FieldChange { name, value: Some(value), ..Default::default() })
                .collect();
            pdfcraft_forms::apply_script_changes(self.doc, &changes, &mut pdfcraft_forms::NoScripts).map_err(|e| e.to_string())?;
            self.fields = None;
        }
        if self.ov_dirty {
            pdfcraft_xfa::set_overrides(self.doc, &self.ov).map_err(|e| e.to_string())?;
            self.ov_dirty = false;
        }
        if wrote {
            // What the packet now holds (a value it could not take is reported above).
            self.data = pdfcraft_xfa::data_of(self.doc);
            self.invalidate();
        }
        Ok(())
    }

    /// Lay the form out again and give the new widgets their appearances.
    fn relayout(&mut self) -> Result<(), String> {
        if self.relayouts >= MAX_RELAYOUTS_PER_EVENT {
            if !self.relayouts_skipped {
                self.relayouts_skipped = true;
                self.error(format!(
                    "The form's scripts asked to lay it out again more than {MAX_RELAYOUTS_PER_EVENT} times; the form is laid out once more when they finish"
                ));
            }
            return Ok(());
        }
        self.run.structure_changed = true;
        self.relayouts += 1;
        self.flush()?;
        let report = pdfcraft_xfa::rerender(self.doc, self.tpl).map_err(|e| e.to_string())?;
        self.console(report.warnings);
        crate::xfa_redraw(self.doc)?;
        self.console(crate::undrawn_field_warnings(self.doc));
        self.by_som = None;
        self.by_path = None;
        self.fields = None;
        self.invalidate();
        Ok(())
    }

    /// `xfa.host.resetData`: the fields back to their defaults, in the form and the data.
    fn reset(&mut self, names: &[String]) -> Result<(), String> {
        self.flush()?;
        let by_som = self.by_som().clone();
        let listed: Option<Vec<String>> = if names.is_empty() {
            None
        } else {
            Some(by_som.iter().filter(|(som, _)| names.iter().any(|n| som_matches(som, n))).map(|(_, f)| f.clone()).collect())
        };
        if listed.as_ref().is_some_and(Vec::is_empty) {
            return Ok(());
        }
        pdfcraft_forms::reset(self.doc, listed.as_deref()).map_err(|e| e.to_string())?;
        self.fields = None;
        let infos = self.fields().clone();
        for (som, name) in &by_som {
            if listed.as_ref().is_some_and(|l| !l.contains(name)) {
                continue;
            }
            let Some(info) = infos.get(name) else { continue };
            if matches!(info.kind, pdfcraft_forms::FieldKind::PushButton | pdfcraft_forms::FieldKind::Signature) {
                continue;
            }
            let path = pdfcraft_xfa::data_path(self.tpl, som);
            let text = data_text(info);
            if self.data.as_ref().and_then(|d| d.text_at(&path)).unwrap_or("") != text {
                self.data_op(pdfcraft_xfa::DataOp::Value { path, text });
                self.changes.values += 1;
            }
        }
        self.invalidate();
        Ok(())
    }

    /// Apply what a script of `ev` did. Returns whether the form has to be laid out again.
    fn apply_effects(&mut self, ev: &ScriptEvent, effects: &[XfaEffect]) -> Result<bool, String> {
        let mut relayout = false;
        // Instances of each repeating subform before the script ran, as the effects leave them.
        let mut counts: HashMap<(String, String), usize> = HashMap::new();
        {
            // A copy of the shared reference: `live()` borrows `self` mutably.
            let tpl = self.tpl;
            let live = self.live();
            for e in effects {
                if let XfaEffect::AddInstance { som } | XfaEffect::RemoveInstance { som } = e
                    && let Some((_, parent_som, name, _)) = split_instance(tpl, som)
                {
                    let have = count_instances(&live.root, &parent_som, &name);
                    counts.entry((parent_som, name)).or_insert(have);
                }
            }
        }
        for e in effects {
            // Only a click script may print, save, open links or move the focus (not the
            // calculations a click sets off, nor anything run on open or on a change).
            if let Some(what) = click_only(e)
                && ev.activity != "click"
            {
                let note = format!("{} ({}): a script may only {what} when a button is clicked; ignored", ev.som, ev.activity);
                self.console([note]);
                continue;
            }
            match e {
                XfaEffect::SetValue { som, value } => {
                    self.set_value(som, value);
                }
                XfaEffect::SetPresence { som, presence } => {
                    if self.ov.presence.insert(som.clone(), presence.clone()).as_ref() != Some(presence) {
                        self.ov_dirty = true;
                        self.run.structure_changed = true;
                        self.changes.visibility += 1;
                        self.invalidate();
                        relayout = true;
                    }
                }
                XfaEffect::SetAccess { som, access } => {
                    if self.ov.access.insert(som.clone(), access.clone()).as_ref() != Some(access) {
                        self.ov_dirty = true;
                        self.run.structure_changed = true;
                        self.changes.visibility += 1;
                        self.invalidate();
                        relayout = true;
                    }
                }
                XfaEffect::AddInstance { som } | XfaEffect::RemoveInstance { som } => {
                    let Some((parent, parent_som, name, index)) = split_instance(self.tpl, som) else { continue };
                    let key = (parent_som, name.clone());
                    let have = counts.get(&key).copied().unwrap_or(0);
                    if matches!(e, XfaEffect::AddInstance { .. }) {
                        counts.insert(key, have + 1);
                        self.data_op(pdfcraft_xfa::DataOp::Instances { parent, name, count: (index + 1).max(have + 1) });
                    } else {
                        // The data holds every shown instance before one is taken out.
                        counts.insert(key, have.saturating_sub(1));
                        self.data_op(pdfcraft_xfa::DataOp::Instances { parent: parent.clone(), name: name.clone(), count: have });
                        let mut path = parent;
                        path.push((name, index));
                        self.data_op(pdfcraft_xfa::DataOp::Remove(path));
                    }
                    self.changes.rows += 1;
                    relayout = true;
                }
                XfaEffect::MessageBox(m) => self.alert(m.clone()),
                XfaEffect::ResetData(names) => self.reset(names)?,
                XfaEffect::Print => self.run.out.requests.push(Request::Print),
                XfaEffect::SaveAs => self.run.out.requests.push(Request::SaveAs),
                XfaEffect::LaunchUrl(u) => self.run.out.requests.push(Request::LaunchUrl(u.clone())),
                XfaEffect::ShowPanel(p) => self.run.out.requests.push(Request::ShowPanel(*p)),
                XfaEffect::SetFocus(som) => {
                    if let Some(name) = self.by_som().get(som).cloned() {
                        self.run.out.requests.push(Request::Focus(name));
                    }
                }
                XfaEffect::Beep => self.run.out.requests.push(Request::Beep),
                XfaEffect::Recalculate => {}
                XfaEffect::Relayout => relayout = true,
            }
        }
        Ok(relayout)
    }

    /// Run one event script against the form as it is now. `None`: it was skipped (the
    /// event's budget is spent).
    fn run(&mut self, ev: &ScriptEvent, new_text: &str) -> Result<Option<pdfcraft_js::xfa::XfaOutcome>, String> {
        if self.runs >= MAX_SCRIPTS_PER_EVENT || self.clock.spent() || self.run.ran_away {
            self.skipped += 1;
            return Ok(None);
        }
        self.runs += 1;
        #[cfg(test)]
        self.check_live();
        let event = XfaEvent { activity: ev.activity.clone(), target: ev.som.clone(), new_text: new_text.to_string(), prev_text: String::new() };
        let root = to_js(&self.live().root);
        let script = Script {
            source: ev.script.clone(),
            formcalc: ev.formcalc,
            event,
            root,
            limits: limits_for(&ev.activity),
            timeout: timeout_for(&ev.activity).saturating_mul(self.run.time_scale),
        };
        let o = self.session().run(script);
        if o.abandoned {
            self.run.ran_away = true;
            // The thread is left to the runaway script: nothing more is sent to it.
            self.session = None;
        }
        self.console(o.console.iter().cloned());
        for n in &o.notes {
            self.error(format!("{} ({}): {n}", ev.som, ev.activity));
        }
        // A script that fails keeps what it changed before the error, as JavaScript does: forms
        // often trip over an object an earlier version had, late in a script whose first lines
        // did the work (opening the next fields, showing a subform). An abandoned script, or
        // one the engine failed, reports no changes.
        if let Some(e) = &o.error {
            self.error(format!("{} ({}): {e}", ev.som, ev.activity));
        }
        if self.apply_effects(ev, &o.effects)? {
            self.relayout()?;
        }
        Ok(Some(o))
    }

    /// Every calculate script, in document order: the field takes the script's result unless
    /// the script set a value itself.
    fn calculates(&mut self) -> Result<(), String> {
        let events: Vec<ScriptEvent> = self.live().events.iter().filter(|e| e.activity == "calculate").cloned().collect();
        self.calculate_events(events)
    }

    /// After an ordinary field edit, run only the scripts whose statically visible reads
    /// intersect the edited fields (and their dependents). Any script outside a small pure
    /// subset, a structural change, or an unknown field name retains the full-path behavior.
    fn calculates_after_change(&mut self, names: &[String], additional_names: &[String], incremental: bool) -> Result<(), String> {
        if incremental && !self.run.structure_changed {
            let mut changed_names = names.to_vec();
            changed_names.extend(additional_names.iter().cloned());
            changed_names.extend(self.run.changed_fields.iter().cloned());
            if let Some(plan) = self.edit_plan.as_ref().and_then(|plan| plan.calculations.as_ref())
                && let Some(selected) = plan.affected(&changed_names)
            {
                let events = plan
                    .calculations
                    .iter()
                    .enumerate()
                    .filter_map(|(index, calculation)| selected.contains(&index).then(|| calculation.event.clone()))
                    .collect();
                return self.calculate_events(events);
            }
        }
        self.calculates()
    }

    fn calculate_events(&mut self, events: Vec<ScriptEvent>) -> Result<(), String> {
        for ev in &events {
            let Some(o) = self.run(ev, "")? else { continue };
            if o.error.is_some() || o.effects.iter().any(|e| matches!(e, XfaEffect::SetValue { som, .. } if *som == ev.som)) {
                continue;
            }
            if let Some(result) = o.result {
                self.set_value(&ev.som, &result);
            }
        }
        Ok(())
    }

    fn events_of(&mut self, som: &str, activity: &str) -> Vec<ScriptEvent> {
        self.live().events.iter().filter(|e| e.som == som && e.activity == activity).cloned().collect()
    }

    /// End the event: write what is left, lay the form out if a capped relayout is owed, and
    /// report what the limits cut.
    fn finish(mut self) -> Result<Changes, String> {
        #[cfg(test)]
        self.check_live();
        if self.relayouts_skipped {
            self.relayouts = 0;
            self.relayouts_skipped = false;
            self.relayout()?;
        }
        self.flush()?;
        self.session = None;
        if self.skipped > 0 {
            let why = if self.run.ran_away {
                "one ran too long".to_string()
            } else if self.runs >= MAX_SCRIPTS_PER_EVENT {
                format!("more than {MAX_SCRIPTS_PER_EVENT} scripts ran")
            } else {
                format!("the scripts took more than {} seconds", self.clock.budget_ms / 1000)
            };
            self.run.out.errors.push(format!("The form's scripts were stopped: {why}; {} more were skipped", self.skipped));
        }
        if self.dropped_output > 0 {
            self.run.out.errors.push(format!("The form's scripts produced too much output; {} more messages were left out", self.dropped_output));
        }
        Ok(self.changes)
    }
}

impl Runner<'_, '_> {
    /// The live form kept up to date in place must be the form a fresh build gives: every
    /// field's SOM, value, presence and access (tests run this before every script and at the
    /// end of every event). A difference is a bug in the in-place refresh, so the test binary
    /// is ended at once: a panic here would be caught by the engine's guard and turned into a
    /// script error the test might not look at.
    #[cfg(test)]
    fn check_live(&mut self) {
        let Some(live) = self.cached.as_ref() else { return };
        let fresh = pdfcraft_xfa::form_tree(self.tpl, self.data.as_ref(), &self.ov);
        let (mut have, mut want) = (Vec::new(), Vec::new());
        collect_fields(&live.root, &mut have, 0);
        collect_fields(&fresh.root, &mut want, 0);
        if have != want {
            let differ: Vec<String> =
                have.iter().zip(want.iter()).filter(|(a, b)| a != b).take(5).map(|(a, b)| format!("live {a:?} vs fresh {b:?}")).collect();
            eprintln!("the live form diverged from a fresh build ({} vs {} fields): {}", have.len(), want.len(), differ.join("; "));
            std::process::abort();
        }
    }
}

/// Every field's (SOM, value, presence, access), in document order.
#[cfg(test)]
fn collect_fields(n: &FormNode, out: &mut Vec<(String, String, String, String)>, depth: usize) {
    if depth > 64 {
        return;
    }
    if n.kind == Some(NodeKind::Field) {
        out.push((n.som.clone(), n.value.clone(), n.presence.clone(), n.access.clone()));
    }
    for c in &n.children {
        collect_fields(c, out, depth + 1);
    }
}

/// Reflow a laid-out XFA form when an edited multiline field can grow. The datasets must
/// already hold the final values, including calculation results. Fixed-height fields keep
/// their existing geometry; forms without scripts parse their template only when needed.
/// `Some` means pages/widgets were rebuilt and the caller must fully refresh its snapshot.
pub(crate) fn relayout_changed_text(
    doc: &mut pdfcraft_cos::Document,
    cached: Option<&Template>,
    before: &[pdfcraft_forms::Field],
    changed_names: Option<&HashSet<String>>,
) -> Result<Option<Vec<String>>, String> {
    if pdfcraft_xfa::existing_layout(doc).is_none() {
        return Ok(None);
    }
    let current = &*doc;
    let mut changed = before
        .iter()
        .filter(|f| f.kind == pdfcraft_forms::FieldKind::Text && f.has(pdfcraft_forms::flags::MULTILINE))
        .filter(|f| changed_names.is_none_or(|names| names.contains(&f.name)))
        .filter_map(|f| {
            let object = current.get(f.obj);
            let dict = object.as_dict()?;
            let value = dict.get(b"V").and_then(|v| current.resolve(v).as_string().map(|s| s.to_text())).unwrap_or_default();
            if f.value.first().map_or("", String::as_str) == value {
                return None;
            }
            dict.get(pdfcraft_xfa::SOM_KEY).and_then(|s| current.resolve(s).as_string().map(|s| s.to_text()))
        })
        .peekable();
    if changed.peek().is_none() {
        return Ok(None);
    }
    let template = match cached {
        Some(tpl) => std::borrow::Cow::Borrowed(tpl),
        None => std::borrow::Cow::Owned(pdfcraft_xfa::template_of(current).map_err(|e| e.to_string())?),
    };
    let reflow = changed.any(|som| pdfcraft_xfa::field_grows_with_text(&template, &som));
    drop(changed);
    if !reflow {
        return Ok(None);
    }
    let report = pdfcraft_xfa::rerender(doc, &template).map_err(|e| e.to_string())?;
    crate::xfa_redraw(doc)?;
    Ok(Some(report.warnings))
}

/// On open: initialize scripts, then calculations. Returns what they changed.
pub(crate) fn on_open(doc: &mut pdfcraft_cos::Document, tpl: &Template, run: &mut XfaRun) -> Result<Changes, String> {
    let mut r = Runner::new(doc, tpl, run, OPEN_BUDGET_MS, None);
    let events: Vec<ScriptEvent> = r.live().events.iter().filter(|e| e.activity == "initialize" || e.activity == "docReady").cloned().collect();
    for ev in &events {
        r.run(ev, "")?;
    }
    r.calculates()?;
    r.finish()
}

/// Whether setting a field can run an XFA event at all. Avoid constructing the live form and
/// parsing its entire datasets tree on edits when a template only has open/click events.
pub(crate) fn has_calculation_scripts(tpl: &Template) -> bool {
    use pdfcraft_xfa::model::{Field, Node, Script};

    fn calculations(scripts: &[Script]) -> bool {
        scripts.iter().any(|script| script.activity == "calculate" && !script.text.trim().is_empty())
    }
    fn field(field: &Field) -> bool {
        field.calculate.as_ref().is_some_and(|script| !script.text.trim().is_empty()) || calculations(&field.scripts)
    }
    fn nodes(children: &[Node], depth: usize) -> bool {
        if depth > 64 {
            return false;
        }
        children.iter().any(|node| match node {
            Node::Subform(subform) => calculations(&subform.scripts) || nodes(&subform.children, depth + 1),
            Node::Area(area) => nodes(&area.children, depth + 1),
            Node::Field(form_field) => field(form_field),
            Node::ExclGroup(group) => calculations(&group.scripts) || group.fields.iter().any(field),
            Node::Draw(_) => false,
        })
    }
    calculations(&tpl.root.scripts)
        || nodes(&tpl.root.children, 1)
        || tpl.root.page_set.as_ref().is_some_and(|page_set| page_set.areas.iter().any(|area| nodes(&area.items, 1)))
}

pub(crate) fn has_change_scripts(tpl: &Template, names: &[String], additional_names: &[String]) -> bool {
    use pdfcraft_xfa::model::{Field, Node, Script};

    fn has_activity(scripts: &[Script], activities: &[&str]) -> bool {
        scripts.iter().any(|script| !script.text.trim().is_empty() && activities.contains(&script.activity.as_str()))
    }
    fn leaf(name: &str) -> &str {
        name.rsplit('.').next().unwrap_or(name).split('[').next().unwrap_or(name)
    }
    fn field_needs_change(field: &Field, names: &[String], seen: &mut std::collections::HashSet<String>) -> bool {
        if field.calculate.as_ref().is_some_and(|script| !script.text.trim().is_empty()) || has_activity(&field.scripts, &["calculate"]) {
            return true;
        }
        let Some(field_name) = field.common.name.as_deref() else { return false };
        let field_leaf = leaf(field_name);
        seen.insert(field_leaf.to_string());
        names.iter().any(|name| leaf(name) == field_leaf)
            && (field.validate.as_ref().is_some_and(|script| !script.text.trim().is_empty())
                || has_activity(&field.scripts, &["change", "exit", "validate"]))
    }
    fn nodes(children: &[Node], depth: usize, names: &[String], seen: &mut std::collections::HashSet<String>) -> bool {
        if depth > 64 {
            return false;
        }
        children.iter().any(|node| match node {
            Node::Subform(subform) => {
                has_activity(&subform.scripts, &["change", "exit", "validate", "calculate"]) || nodes(&subform.children, depth + 1, names, seen)
            }
            Node::Area(area) => nodes(&area.children, depth + 1, names, seen),
            Node::Field(form_field) => field_needs_change(form_field, names, seen),
            Node::ExclGroup(group) => {
                has_activity(&group.scripts, &["change", "exit", "validate", "calculate"])
                    || group.fields.iter().any(|field| field_needs_change(field, names, seen))
            }
            Node::Draw(_) => false,
        })
    }
    if has_activity(&tpl.root.scripts, &["change", "exit", "validate", "calculate"]) {
        return true;
    }
    let names: Vec<_> = names.iter().chain(additional_names).cloned().collect();
    let mut seen = std::collections::HashSet::new();
    if nodes(&tpl.root.children, 1, &names, &mut seen)
        || tpl.root.page_set.as_ref().is_some_and(|page_set| page_set.areas.iter().any(|area| nodes(&area.items, 1, &names, &mut seen)))
    {
        return true;
    }
    names.iter().any(|name| !seen.contains(leaf(name)))
}

/// After fields `names` took new values: each one's change, exit and validate scripts, then
/// the calculations once (or only those, after a reset). A validate script that answers false,
/// or fails, shows its message; the value stays, as Acrobat marks the field invalid rather
/// than reverting it.
pub(crate) fn on_changes(
    doc: &mut pdfcraft_cos::Document,
    tpl: &Template,
    names: &[String],
    additional_names: &[String],
    incremental: bool,
    edit_plan: Option<Arc<XfaEditPlan>>,
    run: &mut XfaRun,
) -> Result<(), String> {
    let mut r = Runner::new(doc, tpl, run, OPEN_BUDGET_MS, edit_plan);
    for name in names {
        let som = r.by_som().iter().find(|(_, f)| *f == name).map(|(s, _)| s.clone());
        let Some(som) = som else { continue };
        let value = r.fields().get(name).map(|f| f.value.join("\n")).unwrap_or_default();
        for ev in r.events_of(&som, "change").into_iter().chain(r.events_of(&som, "exit")) {
            r.run(&ev, &value)?;
        }
        for ev in r.events_of(&som, "validate") {
            let Some(o) = r.run(&ev, &value)? else { continue };
            if (o.result_bool == Some(false) || o.error.is_some()) && !value.trim().is_empty() {
                r.alert(crate::js::Message::plain(ev.message.clone().unwrap_or_else(|| format!("{name}: the value is not valid"))));
            }
        }
    }
    r.calculates_after_change(names, additional_names, incremental)?;
    r.finish().map(|_| ())
}

/// The form's data changed outside its scripts (a data file imported, a picture chosen): lay
/// the form out again from the data, then, when `calculate`, run every calculation.
pub(crate) fn on_data_replaced(doc: &mut pdfcraft_cos::Document, tpl: &Template, calculate: bool, run: &mut XfaRun) -> Result<(), String> {
    let mut r = Runner::new(doc, tpl, run, OPEN_BUDGET_MS, None);
    r.relayout()?;
    if calculate {
        r.calculates()?;
    }
    r.finish().map(|_| ())
}

/// A text value typed into the laid-out XFA date field `name` by another of its edit picture's
/// patterns, written in the first ([`pdfcraft_xfa::typed_date`]); `None` for anything else.
/// `tpl` is the template the scripts use, when they run; otherwise it is read from `doc`.
pub(crate) fn typed_date(
    doc: &pdfcraft_cos::Document,
    tpl: Option<&Template>,
    name: &str,
    value: &pdfcraft_forms::FieldValue,
) -> Option<pdfcraft_forms::FieldValue> {
    let pdfcraft_forms::FieldValue::Text(text) = value else { return None };
    let som = pdfcraft_xfa::fields_by_som(doc).into_iter().find_map(|(som, n)| (n == name).then_some(som))?;
    let read;
    let tpl = match tpl {
        Some(t) => t,
        None => {
            read = pdfcraft_xfa::template_of(doc).ok()?;
            &read
        }
    };
    pdfcraft_xfa::typed_date(tpl, &som, text).map(pdfcraft_forms::FieldValue::Text)
}

/// A button's click script (or any event, by SOM path and activity), then calculations.
pub(crate) fn on_event(doc: &mut pdfcraft_cos::Document, tpl: &Template, som: &str, activity: &str, run: &mut XfaRun) -> Result<(), String> {
    let mut r = Runner::new(doc, tpl, run, EVENT_BUDGET_MS, None);
    let events = r.events_of(som, activity);
    if events.is_empty() {
        return Err(format!("{som} has no {activity} script"));
    }
    for ev in events {
        r.run(&ev, "")?;
    }
    // The calculations that follow get a budget of their own.
    r.clock = Clock::start(OPEN_BUDGET_MS.saturating_mul(u64::from(r.run.time_scale)));
    r.calculates()?;
    r.finish().map(|_| ())
}

/// The SOM path of a generated field whose template has a click script.
pub(crate) fn clickable_som(doc: &pdfcraft_cos::Document, tpl: &Template, field: &pdfcraft_forms::Field) -> Option<String> {
    // Page-master copies have distinct PDF field names but share a SOM path. Reversing
    // fields_by_som would lose every copy except the last: use the clicked field itself.
    let object = doc.get(field.obj);
    let som = object.as_dict()?.get(pdfcraft_xfa::SOM_KEY).map(|s| doc.resolve(s))?.as_string()?.to_text();
    let live = pdfcraft_xfa::form_tree(tpl, pdfcraft_xfa::data_of(doc).as_ref(), &pdfcraft_xfa::overrides(doc));
    live.events.iter().any(|e| e.som == som && e.activity == "click").then_some(som)
}

/// The parsed template of a laid-out XFA form, including forms without scripts, for the
/// dataset-binding and text-layout paths that still need its SOM-to-data mapping.
pub(crate) fn data_template(doc: &pdfcraft_cos::Document) -> Option<Arc<Template>> {
    pdfcraft_xfa::template_of(doc).ok().map(Arc::new)
}

/// Push buttons PdfCraft generated for XFA click scripts carry no PDF action (the script is
/// XFA's, not Acrobat JavaScript): give them one that runs the script by the field's SOM path.
pub(crate) fn mark_script_buttons(doc: &pdfcraft_cos::Document, fields: &mut [pdfcraft_forms::Field]) {
    for f in fields.iter_mut() {
        if f.kind == pdfcraft_forms::FieldKind::PushButton
            && f.button.is_none()
            && std::iter::once(f.obj)
                .chain(f.widgets.iter().map(|w| w.obj))
                .any(|r| doc.get(r).as_dict().is_some_and(|d| d.contains(pdfcraft_xfa::CLICK_KEY)))
        {
            f.button = Some(pdfcraft_forms::af::ButtonAction::Script(String::new()));
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn field_set(names: &[&str]) -> HashSet<String> {
        names.iter().map(|name| (*name).to_string()).collect()
    }

    #[test]
    fn simple_calculation_reads_are_tracked_by_field_som() {
        let fields = field_set(&["form[0].first[0]", "form[0].last[0]", "form[0].full[0]"]);
        let reads = calc_script_reads("this.rawValue = first + last;", "form[0].full[0]", &fields).unwrap();
        assert_eq!(reads, field_set(&["form[0].first[0]", "form[0].last[0]", "form[0].full[0]"]));
    }

    #[test]
    fn unsupported_or_side_effecting_calculations_fall_back_to_full_recalculation() {
        let fields = field_set(&["form[0].source[0]", "form[0].other[0]", "form[0].target[0]"]);
        assert!(calc_script_reads("xfa.resolveNode('source').rawValue", "form[0].target[0]", &fields).is_none());
        assert!(calc_script_reads("other.rawValue = source;", "form[0].target[0]", &fields).is_none());
        assert!(calc_script_reads("other.rawValue++;", "form[0].target[0]", &fields).is_none());
        assert!(calc_script_reads("++other.rawValue;", "form[0].target[0]", &fields).is_none());
        assert!(calc_script_reads("this.rawValue = unknownGlobal;", "form[0].target[0]", &fields).is_none());
    }

    #[test]
    fn duplicate_leaf_names_need_a_qualified_reference() {
        let fields = field_set(&["form[0].row[0].amount[0]", "form[0].row[1].amount[0]", "form[0].total[0]"]);
        assert!(calc_script_reads("this.rawValue = amount;", "form[0].total[0]", &fields).is_none());
    }

    #[test]
    fn qualified_field_references_match_a_unique_form_path_suffix() {
        let fields = field_set(&["form[0].person[0].first[0]", "form[0].other[0].first[0]", "form[0].full[0]"]);
        let reads = calc_script_reads("this.rawValue = person.first;", "form[0].full[0]", &fields).unwrap();
        assert_eq!(reads, field_set(&["form[0].person[0].first[0]", "form[0].full[0]"]));
    }

    #[test]
    fn changed_fields_select_transitive_calculation_dependents_only() {
        let dependencies = |som: &str| HashSet::from([som.to_string()]);
        let event = |som: &str| ScriptEvent {
            som: som.to_string(),
            activity: "calculate".to_string(),
            script: "this.rawValue".to_string(),
            formcalc: false,
            message: None,
        };
        let plan = CalculationPlan {
            calculations: vec![
                Calculation { event: event("form[0].middle[0]"), dependencies: dependencies("form[0].input[0]") },
                Calculation { event: event("form[0].result[0]"), dependencies: dependencies("form[0].middle[0]") },
                Calculation { event: event("form[0].other[0]"), dependencies: dependencies("form[0].unrelated[0]") },
            ],
            som_by_name: HashMap::from([("input".to_string(), HashSet::from(["form[0].input[0]".to_string()]))]),
            som_by_path: HashMap::new(),
            path_by_som: HashMap::new(),
        };
        assert_eq!(plan.affected(&["input".to_string()]).unwrap(), HashSet::from([0, 1]));
    }

    #[test]
    fn calculated_outputs_dirty_every_field_on_the_same_data_path() {
        let input = "form[0].input[0]".to_string();
        let calculated = "form[0].calculated[0]".to_string();
        let alias = "form[0].alias[0]".to_string();
        let result = "form[0].result[0]".to_string();
        let shared_path = pdfcraft_xfa::data::DataPath::default();
        let event = |som: &str| ScriptEvent {
            som: som.to_string(),
            activity: "calculate".to_string(),
            script: "this.rawValue".to_string(),
            formcalc: false,
            message: None,
        };
        let plan = CalculationPlan {
            calculations: vec![
                Calculation { event: event(&calculated), dependencies: HashSet::from([input.clone()]) },
                Calculation { event: event(&result), dependencies: HashSet::from([alias.clone()]) },
            ],
            som_by_name: HashMap::from([("input".to_string(), HashSet::from([input]))]),
            som_by_path: HashMap::from([(shared_path.clone(), HashSet::from([calculated.clone(), alias.clone()]))]),
            path_by_som: HashMap::from([(calculated, shared_path.clone()), (alias, shared_path)]),
        };
        assert_eq!(plan.affected(&["input".to_string()]).unwrap(), HashSet::from([0, 1]));
    }

    #[test]
    fn script_names_match_indexed_som_paths() {
        assert!(som_matches("form[0].page1[0].qty[0]", "qty"));
        assert!(som_matches("form[0].page1[0].qty[0]", "page1.qty"));
        assert!(som_matches("form[0].page1[0].qty[0]", "xfa.form.form.page1.qty"));
        assert!(som_matches("form[0].page1[0].table[0].row[2].amount[0]", "table"));
        assert!(!som_matches("form[0].page1[0].qty[0]", "qt"));
        assert!(!som_matches("form[0].page1[0].price[0]", "qty"));
        assert_eq!(
            split_instance(&Template::default(), "form[0].table[0].row[2]").map(|(p, ps, n, i)| (p.len(), ps, n, i)),
            Some((2, "form[0].table[0]".into(), "row".into(), 2))
        );
    }
}
