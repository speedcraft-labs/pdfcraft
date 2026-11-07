use super::host::{arg, colour, error, function, host, read, s, set_value, strings, text, value, write};
use super::model::{DISPLAY_HIDDEN, DISPLAY_VISIBLE, FieldType, Request};
use super::{Attribute, Context, JsArray, JsFunction, JsObject, JsResult, JsString, JsValue, NativeFunction, ObjectInitializer, js_string};

macro_rules! getter {
    ($ctx:expr, |$f:ident, $c:ident| $body:expr) => {{
        fn get(this: &JsValue, _: &[JsValue], $c: &mut Context) -> JsResult<JsValue> {
            let name = field_name(this, $c)?;
            let $f = read($c, &name)?;
            Ok($body)
        }
        function($ctx, NativeFunction::from_fn_ptr(get))
    }};
}

fn field_name(this: &JsValue, ctx: &mut Context) -> JsResult<JsString> {
    let object = this.as_object().ok_or_else(|| error("the field object has no receiver"))?;
    let name = object.get(js_string!("name"), ctx)?;
    Ok(JsString::from(text(&name, ctx)?))
}

#[derive(Clone)]
struct FieldAccessors {
    value_get: JsFunction,
    value_set: JsFunction,
    as_string: JsFunction,
    default: JsFunction,
    kind: JsFunction,
    readonly_get: JsFunction,
    readonly_set: JsFunction,
    required_get: JsFunction,
    required_set: JsFunction,
    display_get: JsFunction,
    display_set: JsFunction,
    hidden_get: JsFunction,
    hidden_set: JsFunction,
    num_items: JsFunction,
    char_limit: JsFunction,
    export_values: JsFunction,
    indices: JsFunction,
    text_color_get: JsFunction,
    text_color_set: JsFunction,
    fill_color_get: JsFunction,
    fill_color_set: JsFunction,
    get_item_at: JsFunction,
    is_box_checked: JsFunction,
    check_this_box: JsFunction,
    set_focus: JsFunction,
}

impl FieldAccessors {
    fn new(ctx: &mut Context) -> Self {
        let value_get = {
            fn get(this: &JsValue, _: &[JsValue], c: &mut Context) -> JsResult<JsValue> {
                let name = field_name(this, c)?;
                value(c, &name)
            }
            function(ctx, NativeFunction::from_fn_ptr(get))
        };
        let value_set = {
            fn set(this: &JsValue, args: &[JsValue], c: &mut Context) -> JsResult<JsValue> {
                let name = field_name(this, c)?;
                let vals = strings(&arg(args, 0), c)?;
                write(c, &name, |f| set_value(f, vals))?;
                Ok(JsValue::undefined())
            }
            function(ctx, NativeFunction::from_fn_ptr(set))
        };
        let as_string = getter!(ctx, |f, _c| s(&match f.kind {
            FieldType::CheckBox | FieldType::RadioButton => f.value.first().cloned().unwrap_or_else(|| "Off".into()),
            _ => f.value.join(","),
        }));
        let default = getter!(ctx, |f, _c| s(&f.default.first().cloned().unwrap_or_default()));
        let kind = getter!(ctx, |f, _c| s(f.kind.name()));
        let readonly_get = getter!(ctx, |f, _c| JsValue::from(f.readonly));
        let readonly_set = {
            fn set(this: &JsValue, args: &[JsValue], c: &mut Context) -> JsResult<JsValue> {
                let name = field_name(this, c)?;
                let on = arg(args, 0).to_boolean();
                write(c, &name, |f| f.readonly = on)?;
                Ok(JsValue::undefined())
            }
            function(ctx, NativeFunction::from_fn_ptr(set))
        };
        let required_get = getter!(ctx, |f, _c| JsValue::from(f.required));
        let required_set = {
            fn set(this: &JsValue, args: &[JsValue], c: &mut Context) -> JsResult<JsValue> {
                let name = field_name(this, c)?;
                let on = arg(args, 0).to_boolean();
                write(c, &name, |f| f.required = on)?;
                Ok(JsValue::undefined())
            }
            function(ctx, NativeFunction::from_fn_ptr(set))
        };
        let display_get = getter!(ctx, |f, _c| JsValue::from(f.display));
        let display_set = {
            fn set(this: &JsValue, args: &[JsValue], c: &mut Context) -> JsResult<JsValue> {
                let name = field_name(this, c)?;
                let d = arg(args, 0).to_number(c)? as i32;
                write(c, &name, |f| f.display = d.clamp(0, 3))?;
                Ok(JsValue::undefined())
            }
            function(ctx, NativeFunction::from_fn_ptr(set))
        };
        let hidden_get = getter!(ctx, |f, _c| JsValue::from(f.display == DISPLAY_HIDDEN));
        let hidden_set = {
            fn set(this: &JsValue, args: &[JsValue], c: &mut Context) -> JsResult<JsValue> {
                let name = field_name(this, c)?;
                let on = arg(args, 0).to_boolean();
                write(c, &name, |f| f.display = if on { DISPLAY_HIDDEN } else { DISPLAY_VISIBLE })?;
                Ok(JsValue::undefined())
            }
            function(ctx, NativeFunction::from_fn_ptr(set))
        };
        let num_items = getter!(ctx, |f, _c| JsValue::from(f.options.len() as f64));
        let char_limit = getter!(ctx, |f, _c| JsValue::from(f.char_limit.unwrap_or(0) as f64));
        let export_values = getter!(ctx, |f, c| {
            let vals: Vec<JsValue> = f.options.iter().map(|o| s(&o.0)).collect();
            JsArray::from_iter(vals, c).into()
        });
        let indices = getter!(ctx, |f, c| {
            let idx: Vec<JsValue> =
                f.options.iter().enumerate().filter(|(_, o)| f.value.contains(&o.0)).map(|(i, _)| JsValue::from(i as f64)).collect();
            if idx.len() == 1 { idx[0].clone() } else { JsArray::from_iter(idx, c).into() }
        });
        let text_color_get = getter!(ctx, |f, c| colour(&f.text_color, c));
        let text_color_set = {
            fn set(this: &JsValue, args: &[JsValue], c: &mut Context) -> JsResult<JsValue> {
                let name = field_name(this, c)?;
                let v = strings(&arg(args, 0), c)?;
                write(c, &name, |f| f.text_color = Some(v))?;
                Ok(JsValue::undefined())
            }
            function(ctx, NativeFunction::from_fn_ptr(set))
        };
        let fill_color_get = getter!(ctx, |f, c| colour(&f.fill_color, c));
        let fill_color_set = {
            fn set(this: &JsValue, args: &[JsValue], c: &mut Context) -> JsResult<JsValue> {
                let name = field_name(this, c)?;
                let v = strings(&arg(args, 0), c)?;
                write(c, &name, |f| f.fill_color = Some(v))?;
                Ok(JsValue::undefined())
            }
            function(ctx, NativeFunction::from_fn_ptr(set))
        };

        fn get_item_at(this: &JsValue, args: &[JsValue], c: &mut Context) -> JsResult<JsValue> {
            let name = field_name(this, c)?;
            let f = read(c, &name)?;
            let i = arg(args, 0).to_number(c)?;
            // bExportValue defaults to true.
            let export = args.get(1).is_none_or(JsValue::to_boolean);
            let i = if i < 0.0 { f.options.len().saturating_sub(1) } else { i as usize };
            let o = f.options.get(i).ok_or_else(|| error(format!("{} has no item {i}", f.name)))?;
            Ok(s(if export { &o.0 } else { &o.1 }))
        }
        fn is_box_checked(this: &JsValue, args: &[JsValue], c: &mut Context) -> JsResult<JsValue> {
            let name = field_name(this, c)?;
            let f = read(c, &name)?;
            let i = arg(args, 0).to_number(c)? as usize;
            let on = f.options.get(i).is_some_and(|o| f.value.first() == Some(&o.0));
            Ok(JsValue::from(on))
        }
        fn check_this_box(this: &JsValue, args: &[JsValue], c: &mut Context) -> JsResult<JsValue> {
            let name = field_name(this, c)?;
            let f = read(c, &name)?;
            let i = arg(args, 0).to_number(c)? as usize;
            let on = args.get(1).is_none_or(JsValue::to_boolean);
            let export = f.options.get(i).map(|o| o.0.clone()).ok_or_else(|| error(format!("{} has no widget {i}", f.name)))?;
            write(c, &name, |f| {
                if on {
                    f.value = vec![export];
                } else if f.value.first() == Some(&export) {
                    f.value.clear();
                }
            })?;
            Ok(JsValue::undefined())
        }
        fn set_focus(this: &JsValue, _: &[JsValue], c: &mut Context) -> JsResult<JsValue> {
            let name = field_name(this, c)?;
            host(c)?.borrow_mut().request(Request::Focus(name.to_std_string_escaped()));
            Ok(JsValue::undefined())
        }

        Self {
            value_get,
            value_set,
            as_string,
            default,
            kind,
            readonly_get,
            readonly_set,
            required_get,
            required_set,
            display_get,
            display_set,
            hidden_get,
            hidden_set,
            num_items,
            char_limit,
            export_values,
            indices,
            text_color_get,
            text_color_set,
            fill_color_get,
            fill_color_set,
            get_item_at: function(ctx, NativeFunction::from_fn_ptr(get_item_at)),
            is_box_checked: function(ctx, NativeFunction::from_fn_ptr(is_box_checked)),
            check_this_box: function(ctx, NativeFunction::from_fn_ptr(check_this_box)),
            set_focus: function(ctx, NativeFunction::from_fn_ptr(set_focus)),
        }
    }
}

pub(crate) fn init_field_accessors(ctx: &mut Context) {
    let accessors = FieldAccessors::new(ctx);
    ctx.insert_data(accessors);
}

fn field_accessors(ctx: &mut Context) -> FieldAccessors {
    ctx.get_data::<FieldAccessors>().cloned().unwrap_or_else(|| {
        let accessors = FieldAccessors::new(ctx);
        ctx.insert_data(accessors.clone());
        accessors
    })
}

pub(crate) fn field_object(ctx: &mut Context, name: &str) -> JsObject {
    let accessors = field_accessors(ctx);
    let rw = Attribute::CONFIGURABLE | Attribute::ENUMERABLE;
    ObjectInitializer::new(ctx)
        .property(js_string!("name"), s(name), Attribute::ENUMERABLE)
        .accessor(js_string!("value"), Some(accessors.value_get), Some(accessors.value_set), rw)
        .accessor(js_string!("valueAsString"), Some(accessors.as_string), None, rw)
        .accessor(js_string!("defaultValue"), Some(accessors.default), None, rw)
        .accessor(js_string!("type"), Some(accessors.kind), None, rw)
        .accessor(js_string!("readonly"), Some(accessors.readonly_get), Some(accessors.readonly_set), rw)
        .accessor(js_string!("required"), Some(accessors.required_get), Some(accessors.required_set), rw)
        .accessor(js_string!("display"), Some(accessors.display_get), Some(accessors.display_set), rw)
        .accessor(js_string!("hidden"), Some(accessors.hidden_get), Some(accessors.hidden_set), rw)
        .accessor(js_string!("numItems"), Some(accessors.num_items), None, rw)
        .accessor(js_string!("charLimit"), Some(accessors.char_limit), None, rw)
        .accessor(js_string!("exportValues"), Some(accessors.export_values), None, rw)
        .accessor(js_string!("currentValueIndices"), Some(accessors.indices), None, rw)
        .accessor(js_string!("textColor"), Some(accessors.text_color_get), Some(accessors.text_color_set), rw)
        .accessor(js_string!("fillColor"), Some(accessors.fill_color_get), Some(accessors.fill_color_set), rw)
        .property(js_string!("getItemAt"), accessors.get_item_at, Attribute::WRITABLE | Attribute::CONFIGURABLE)
        .property(js_string!("isBoxChecked"), accessors.is_box_checked, Attribute::WRITABLE | Attribute::CONFIGURABLE)
        .property(js_string!("checkThisBox"), accessors.check_this_box, Attribute::WRITABLE | Attribute::CONFIGURABLE)
        .property(js_string!("setFocus"), accessors.set_focus, Attribute::WRITABLE | Attribute::CONFIGURABLE)
        .build()
}

fn compact_field_object(ctx: &mut Context, name: &str) -> JsObject {
    let accessors = field_accessors(ctx);
    ObjectInitializer::new(ctx)
        .property(js_string!("name"), s(name), Attribute::ENUMERABLE)
        .accessor(js_string!("value"), Some(accessors.value_get), Some(accessors.value_set), Attribute::CONFIGURABLE | Attribute::ENUMERABLE)
        .build()
}

/// True when the script uses only field names and values and does not inspect field objects
/// through another property or reflection. Its field objects can then omit the unused Acrobat API
/// accessors while retaining live `/V` reads and writes.
pub(crate) fn compact_field_shape(script: &str) -> bool {
    if !script.contains("getField") || script.contains('[') || script.contains("delete") {
        return false;
    }
    const SAFE: &[&str] = &["getField", "getNthFieldName", "numFields", "name", "value", "length", "indexOf", "test", "printf", "replace"];
    let bytes = script.as_bytes();
    let mut i = 0;
    while i < bytes.len() {
        match bytes[i] {
            b'\'' | b'"' | b'`' => {
                let quote = bytes[i];
                i += 1;
                while i < bytes.len() {
                    if bytes[i] == b'\\' {
                        i = (i + 2).min(bytes.len());
                    } else if bytes[i] == quote {
                        i += 1;
                        break;
                    } else {
                        i += 1;
                    }
                }
            }
            b'/' if bytes.get(i + 1) == Some(&b'/') => {
                i += 2;
                while bytes.get(i).is_some_and(|b| *b != b'\n') {
                    i += 1;
                }
            }
            b'/' if bytes.get(i + 1) == Some(&b'*') => {
                i += 2;
                while i + 1 < bytes.len() && !bytes[i..].starts_with(b"*/") {
                    i += 1;
                }
                i = (i + 2).min(bytes.len());
            }
            b'.' if i == 0 || bytes[i - 1] != b'\\' => {
                i += 1;
                let start = i;
                while bytes.get(i).is_some_and(|b| b.is_ascii_alphanumeric() || *b == b'_' || *b == b'$') {
                    i += 1;
                }
                if start == i {
                    continue;
                }
                let Ok(property) = std::str::from_utf8(&bytes[start..i]) else { return false };
                if !SAFE.contains(&property) {
                    return false;
                }
            }
            _ => i += 1,
        }
    }
    true
}

/// Whether a Calculate script only reads fields and writes `event.value`. The caller also
/// checks that all field names are statically known and that there are no document scripts.
/// Such scripts can run together when their reads do not depend on another script in the batch.
fn regex_literal_start(tokens: &[String]) -> bool {
    matches!(
        tokens.last().map(String::as_str),
        None | Some(
            "(" | "[" | "{" | "=" | ":" | "," | ";" | "?" | "!" | "~" | "&&" | "||" | "return" | "case" | "throw" | "=>" | "+" | "-" | "*" | "%"
        )
    )
}

fn regex_literal_end(bytes: &[u8], start: usize) -> Option<usize> {
    let mut i = start + 1;
    let mut in_class = false;
    while let Some(&byte) = bytes.get(i) {
        match byte {
            b'\\' => i = (i + 2).min(bytes.len()),
            b'[' if !in_class => {
                in_class = true;
                i += 1;
            }
            b']' if in_class => {
                in_class = false;
                i += 1;
            }
            b'/' if !in_class => {
                i += 1;
                while bytes.get(i).is_some_and(u8::is_ascii_alphabetic) {
                    i += 1;
                }
                return Some(i);
            }
            b'\n' | b'\r' => return None,
            _ => i += 1,
        }
    }
    None
}

pub fn parallel_safe_calculation(script: &str) -> bool {
    if !script.contains("event.value") || script.contains("delete") || script.contains('[') {
        return false;
    }
    const PROPERTIES: &[&str] = &[
        "value",
        "valueAsString",
        "name",
        "readonly",
        "required",
        "display",
        "hidden",
        "length",
        "indexOf",
        "test",
        "replace",
        "getField",
        "getNthFieldName",
        "numFields",
        "printf",
        "printd",
        "printx",
        "scand",
        "round",
        "floor",
        "ceil",
        "abs",
        "min",
        "max",
    ];
    const CALLS: &[&str] = &[
        "if",
        "for",
        "while",
        "function",
        "getField",
        "getNthFieldName",
        "Number",
        "String",
        "Array",
        "parseInt",
        "parseFloat",
        "isNaN",
        "indexOf",
        "test",
        "replace",
        "printf",
        "printd",
        "printx",
        "scand",
        "round",
        "floor",
        "ceil",
        "abs",
        "min",
        "max",
    ];
    let bytes = script.as_bytes();
    let mut tokens = Vec::<String>::new();
    let mut i = 0;
    while i < bytes.len() {
        match bytes[i] {
            b'\'' | b'"' | b'`' => {
                let quote = bytes[i];
                i += 1;
                while i < bytes.len() {
                    if bytes[i] == b'\\' {
                        i = (i + 2).min(bytes.len());
                    } else if bytes[i] == quote {
                        i += 1;
                        break;
                    } else {
                        i += 1;
                    }
                }
                tokens.push("<string>".into());
            }
            b'/' if bytes.get(i + 1) == Some(&b'/') => {
                i += 2;
                while bytes.get(i).is_some_and(|b| *b != b'\n') {
                    i += 1;
                }
            }
            b'/' if bytes.get(i + 1) == Some(&b'*') => {
                i += 2;
                while i + 1 < bytes.len() && !bytes[i..].starts_with(b"*/") {
                    i += 1;
                }
                i = (i + 2).min(bytes.len());
            }
            b'/' if regex_literal_start(&tokens) => {
                let Some(end) = regex_literal_end(bytes, i) else { return false };
                tokens.push("<regex>".into());
                i = end;
            }
            c if c.is_ascii_alphabetic() || c == b'_' || c == b'$' => {
                let start = i;
                i += 1;
                while bytes.get(i).is_some_and(|b| b.is_ascii_alphanumeric() || *b == b'_' || *b == b'$') {
                    i += 1;
                }
                let Ok(identifier) = std::str::from_utf8(&bytes[start..i]) else { return false };
                tokens.push(identifier.to_owned());
            }
            c if c.is_ascii_whitespace() => i += 1,
            c => {
                let token = match (c, bytes.get(i + 1), bytes.get(i + 2)) {
                    (b'=', Some(b'='), Some(b'=')) => "===".to_owned(),
                    (b'!', Some(b'='), Some(b'=')) => "!==".to_owned(),
                    (b'+', Some(b'+'), _) => "++".to_owned(),
                    (b'-', Some(b'-'), _) => "--".to_owned(),
                    (b'+', Some(b'='), _) => "+=".to_owned(),
                    (b'-', Some(b'='), _) => "-=".to_owned(),
                    (b'*', Some(b'='), _) => "*=".to_owned(),
                    (b'/', Some(b'='), _) => "/=".to_owned(),
                    (b'=', Some(b'='), _) => "==".to_owned(),
                    (b'!', Some(b'='), _) => "!=".to_owned(),
                    _ => char::from(c).to_string(),
                };
                i += token.len();
                tokens.push(token);
            }
        }
    }
    for (i, token) in tokens.iter().enumerate() {
        if token == "." && tokens.get(i.wrapping_sub(1)).is_none_or(|previous| previous != "\\") {
            let Some(property) = tokens.get(i + 1).map(String::as_str) else { return false };
            if !PROPERTIES.contains(&property) {
                return false;
            }
            let assignment = tokens.get(i + 2).is_some_and(|next| matches!(next.as_str(), "=" | "+=" | "-=" | "*=" | "/=" | "++" | "--"));
            if assignment {
                let event_value = property == "value" && tokens.get(i.wrapping_sub(1)).is_some_and(|name| name == "event");
                let literal_field_flag = matches!(property, "readonly" | "required" | "display" | "hidden")
                    && i >= 4
                    && tokens.get(i - 1).is_some_and(|token| token == ")")
                    && tokens.get(i - 2).is_some_and(|token| token == "<string>")
                    && tokens.get(i - 3).is_some_and(|token| token == "(")
                    && tokens.get(i - 4).is_some_and(|token| token == "getField");
                if !event_value && !literal_field_flag {
                    return false;
                }
            }
            if property == "rc" && tokens.get(i.wrapping_sub(1)).is_some_and(|name| name == "event") {
                return false;
            }
        }
        let identifier = token.as_bytes().first().is_some_and(|c| c.is_ascii_alphabetic() || matches!(*c, b'_' | b'$'));
        if identifier && tokens.get(i + 1).is_some_and(|next| next == "(") && !CALLS.contains(&token.as_str()) {
            return false;
        }
        if matches!(token.as_str(), "app" | "console" | "xfa" | "Date" | "eval" | "Function" | "event")
            && token == "event"
            && tokens.get(i + 1).is_some_and(|next| next == ".")
            && tokens.get(i + 2).is_some_and(|property| property != "value")
        {
            return false;
        }
        if matches!(token.as_str(), "app" | "console" | "xfa" | "Date" | "eval" | "Function" | "Math") && token != "Math" {
            return false;
        }
    }
    true
}

fn cached_compact_field_object(ctx: &mut Context, name: &str) -> JsResult<JsObject> {
    if let Some(object) = host(ctx)?.borrow().compact_field_objects.get(name).cloned() {
        return Ok(object);
    }
    let object = compact_field_object(ctx, name);
    host(ctx)?.borrow_mut().compact_field_objects.insert(name.to_owned(), object.clone());
    Ok(object)
}

// ── the document (global object) ────────────────────────────────────────────────────────────

pub(crate) fn get_field(_: &JsValue, args: &[JsValue], ctx: &mut Context) -> JsResult<JsValue> {
    let name = text(&arg(args, 0), ctx)?;
    let (exists, child, simple_fields) = {
        let h = host(ctx)?.borrow();
        (h.field_index.contains_key(&name), h.parent_index.get(&name).cloned(), h.simple_fields)
    };
    if exists {
        if simple_fields {
            return Ok(cached_compact_field_object(ctx, &name)?.into());
        }
        return Ok(field_object(ctx, &name).into());
    }
    // A parent name (e.g. "total" for "total.0"): its first kid stands in, as for groups.
    let Some(kid) = child else { return Ok(JsValue::null()) };
    if simple_fields {
        return Ok(cached_compact_field_object(ctx, &kid)?.into());
    }
    Ok(field_object(ctx, &kid).into())
}

pub(crate) fn get_nth_field_name(_: &JsValue, args: &[JsValue], ctx: &mut Context) -> JsResult<JsValue> {
    let i = arg(args, 0).to_number(ctx)?;
    let name = host(ctx)?.borrow().fields.get(i.max(0.0) as usize).map(|f| f.name.clone());
    Ok(name.map_or(JsValue::null(), |n| s(&n)))
}

pub(crate) fn reset_form(_: &JsValue, args: &[JsValue], ctx: &mut Context) -> JsResult<JsValue> {
    let names = strings(&arg(args, 0), ctx)?;
    host(ctx)?.borrow_mut().request(Request::Reset(names));
    Ok(JsValue::undefined())
}

pub(crate) fn print(_: &JsValue, _: &[JsValue], ctx: &mut Context) -> JsResult<JsValue> {
    host(ctx)?.borrow_mut().request(Request::Print);
    Ok(JsValue::undefined())
}

pub(crate) fn submit_form(_: &JsValue, args: &[JsValue], ctx: &mut Context) -> JsResult<JsValue> {
    let a = arg(args, 0);
    let url = match a.as_object() {
        Some(o) if !o.is_array() => {
            let u = o.get(js_string!("cURL"), ctx)?;
            text(&u, ctx)?
        }
        _ => text(&a, ctx)?,
    };
    host(ctx)?.borrow_mut().request(Request::Submit(url));
    Ok(JsValue::undefined())
}

pub(crate) fn noop(_: &JsValue, _: &[JsValue], _: &mut Context) -> JsResult<JsValue> {
    Ok(JsValue::undefined())
}

pub(crate) fn num_fields(_: &JsValue, _: &[JsValue], ctx: &mut Context) -> JsResult<JsValue> {
    Ok(JsValue::from(host(ctx)?.borrow().fields.len() as f64))
}

pub(crate) fn page_num_get(_: &JsValue, _: &[JsValue], ctx: &mut Context) -> JsResult<JsValue> {
    Ok(JsValue::from(host(ctx)?.borrow().doc.page as f64))
}

pub(crate) fn page_num_set(_: &JsValue, args: &[JsValue], ctx: &mut Context) -> JsResult<JsValue> {
    let n = arg(args, 0).to_number(ctx)?;
    let mut h = host(ctx)?.borrow_mut();
    let last = h.doc.num_pages.saturating_sub(1);
    let p = (n.max(0.0) as usize).min(last);
    h.doc.page = p;
    h.request(Request::GoToPage(p));
    Ok(JsValue::undefined())
}
