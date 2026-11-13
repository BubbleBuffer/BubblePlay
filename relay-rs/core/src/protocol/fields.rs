//! Go's map helpers, shared by the validators.

use serde_json::{Map, Value};

/// Go's str helper: the value if it is a JSON string, otherwise the empty
/// string.
pub(super) fn text<'a>(m: &'a Map<String, Value>, key: &str) -> &'a str {
    m.get(key).and_then(Value::as_str).unwrap_or("")
}

/// Go's isBoolean helper: true only for a JSON true or false.
pub(super) fn is_boolean(m: &Map<String, Value>, key: &str) -> bool {
    matches!(m.get(key), Some(Value::Bool(_)))
}

/// Go's has helper.
pub(super) fn has(m: &Map<String, Value>, key: &str) -> bool {
    m.contains_key(key)
}

/// Go's number helper: a finite JSON number. Missing keys, null, non-numbers
/// and numbers that do not fit a float64 (which Go's encoding/json rejects)
/// all yield None.
pub(super) fn number(m: &Map<String, Value>, key: &str) -> Option<f64> {
    m.get(key)?.as_f64().filter(|value| value.is_finite())
}

/// Go's integer helper: a finite number in min..=max with no fractional part.
///
/// trunc mirrors Go's "value == math.Trunc(value)".
pub(super) fn integer(m: &Map<String, Value>, key: &str, min: f64, max: f64) -> bool {
    match number(m, key) {
        Some(value) => value >= min && value <= max && value.trunc() == value,
        None => false,
    }
}

/// Go's uint32Value helper: integer in 0..=4294967295.
pub(super) fn uint32_value(m: &Map<String, Value>, key: &str) -> bool {
    integer(m, key, 0.0, 4_294_967_295.0)
}

/// Go's object helper: the value if it is a JSON object. null, arrays and
/// scalars are None, just as Go's decoder leaves the map nil.
pub(super) fn object<'a>(m: &'a Map<String, Value>, key: &str) -> Option<&'a Map<String, Value>> {
    m.get(key)?.as_object()
}

/// Go's only helper: every key of the object is in keys.
pub(super) fn only(m: &Map<String, Value>, keys: &[&str]) -> bool {
    m.keys().all(|key| keys.contains(&key.as_str()))
}

/// Go's exact helper: the object has exactly these keys.
pub(super) fn exact(m: &Map<String, Value>, keys: &[&str]) -> bool {
    m.len() == keys.len() && only(m, keys)
}

/// Go's boundedString helper: a JSON string whose byte length is min..=max.
///
/// Go also checks utf8.ValidString; a &str is always valid UTF-8, so that
/// check is implicit here.
pub(super) fn bounded_string(m: &Map<String, Value>, key: &str, min: usize, max: usize) -> bool {
    match m.get(key).and_then(Value::as_str) {
        Some(value) => value.len() >= min && value.len() <= max,
        None => false,
    }
}
