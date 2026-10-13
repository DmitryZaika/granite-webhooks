//! Reading a price list the way the CRM's JavaScript does: the list is
//! loose JSON (flags 0/1 or booleans, numbers sometimes strings), so values
//! are read with JS's rules for `??`, truthiness and numbers.

use serde_json::Value;

/// `obj[key]`, with a missing key and `null` both "nullish".
pub fn get<'a>(obj: Option<&'a Value>, key: &str) -> Option<&'a Value> {
    obj?.get(key).filter(|v| !v.is_null())
}

/// The array at `obj[key]`, or none.
pub fn arr<'a>(obj: Option<&'a Value>, key: &str) -> &'a [Value] {
    get(obj, key)
        .and_then(Value::as_array)
        .map_or(&[], Vec::as_slice)
}

/// `on(v)`: `true` or `1`.
pub fn on(v: Option<&Value>) -> bool {
    match v {
        Some(Value::Bool(b)) => *b,
        Some(Value::Number(n)) => n.as_f64() == Some(1.0),
        _ => false,
    }
}

/// JS truthiness.
pub fn truthy(v: Option<&Value>) -> bool {
    match v {
        None | Some(Value::Null) => false,
        Some(Value::Bool(b)) => *b,
        Some(Value::Number(n)) => n.as_f64().is_some_and(|x| x != 0.0 && !x.is_nan()),
        Some(Value::String(s)) => !s.is_empty(),
        Some(_) => true,
    }
}

/// JS `Number(v)` for a JSON value.
pub fn to_number(v: &Value) -> f64 {
    match v {
        Value::Null => 0.0,
        Value::Bool(b) => f64::from(u8::from(*b)),
        Value::Number(n) => n.as_f64().unwrap_or(f64::NAN),
        Value::String(s) => string_to_number(s),
        Value::Array(a) => match a.as_slice() {
            [] => 0.0,
            [one] if !one.is_array() && !one.is_object() => match one {
                Value::Null => 0.0,
                Value::String(s) => string_to_number(s),
                other => to_number(other),
            },
            _ => f64::NAN,
        },
        Value::Object(_) => f64::NAN,
    }
}

#[allow(clippy::cast_precision_loss)]
fn string_to_number(s: &str) -> f64 {
    let t = s.trim();
    if t.is_empty() {
        return 0.0;
    }
    match t {
        "Infinity" | "+Infinity" => return f64::INFINITY,
        "-Infinity" => return f64::NEG_INFINITY,
        _ => {}
    }
    let radix = |p: &str, r: u32| u64::from_str_radix(p, r).map_or(f64::NAN, |n| n as f64);
    if let Some(h) = t.strip_prefix("0x").or_else(|| t.strip_prefix("0X")) {
        return radix(h, 16);
    }
    if let Some(b) = t.strip_prefix("0b").or_else(|| t.strip_prefix("0B")) {
        return radix(b, 2);
    }
    if let Some(o) = t.strip_prefix("0o").or_else(|| t.strip_prefix("0O")) {
        return radix(o, 8);
    }
    let valid = t
        .chars()
        .all(|c| c.is_ascii_digit() || matches!(c, '.' | 'e' | 'E' | '+' | '-'));
    if !valid || t.contains("inf") || t.contains("nan") {
        return f64::NAN;
    }
    t.parse().unwrap_or(f64::NAN)
}

/// `obj[key] ?? fallback` as a number (nullish → `None`).
pub fn num(obj: Option<&Value>, key: &str) -> Option<f64> {
    get(obj, key).map(to_number)
}

/// `obj[key] === s` for a string.
pub fn str_is(obj: Option<&Value>, key: &str, s: &str) -> bool {
    get(obj, key).and_then(Value::as_str) == Some(s)
}

/// A string field (`undefined` when it is not a string).
pub fn string<'a>(obj: Option<&'a Value>, key: &str) -> Option<&'a str> {
    get(obj, key).and_then(Value::as_str)
}

/// `String(n)` for a number, as keys spell it.
pub fn num_str(v: f64) -> String {
    if v.is_nan() {
        return "NaN".into();
    }
    if v.is_infinite() {
        return if v > 0.0 { "Infinity" } else { "-Infinity" }.into();
    }
    if v == 0.0 {
        return "0".into();
    }
    let abs = v.abs();
    if (1e-6..1e21).contains(&abs) {
        return format!("{v}");
    }
    // Exponent form, as JS writes very large and very small numbers.
    let s = format!("{v:e}");
    let (mantissa, exp) = s.split_once('e').unwrap_or((&s, "0"));
    let exp: i32 = exp.parse().unwrap_or(0);
    format!("{mantissa}e{}{exp}", if exp >= 0 { "+" } else { "" })
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn numbers_read_like_javascript() {
        assert!((to_number(&json!("12.5")) - 12.5).abs() < f64::EPSILON);
        assert!(to_number(&json!(" ")).abs() < f64::EPSILON);
        assert!(to_number(&json!("abc")).is_nan());
        assert!(to_number(&json!("inf")).is_nan());
        assert!((to_number(&json!("0x10")) - 16.0).abs() < f64::EPSILON);
        assert_eq!(num_str(4.0), "4");
        assert_eq!(num_str(4.5), "4.5");
        assert_eq!(num_str(-0.0), "0");
        assert_eq!(num_str(0.1 + 0.2), "0.30000000000000004");
        assert_eq!(num_str(1e21), "1e+21");
        assert_eq!(num_str(1e-7), "1e-7");
        assert!(on(Some(&json!(1))) && on(Some(&json!(true))) && !on(Some(&json!("1"))));
    }
}
