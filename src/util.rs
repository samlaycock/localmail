use std::collections::HashMap;
use std::time::{SystemTime, UNIX_EPOCH};

use base64::Engine;
use base64::engine::general_purpose::{STANDARD, STANDARD_NO_PAD, URL_SAFE_NO_PAD};
use rand::Rng;
use serde_json::Value;

/// Real time in Unix seconds.
pub fn wall_clock() -> i64 {
    SystemTime::now().duration_since(UNIX_EPOCH).map(|d| d.as_secs() as i64).unwrap_or_default()
}

/// Real time in Unix milliseconds.
pub fn now_ms() -> i64 {
    SystemTime::now().duration_since(UNIX_EPOCH).map(|d| d.as_millis() as i64).unwrap_or_default()
}

const UNITS: [(char, i64); 5] = [('w', 604_800), ('d', 86_400), ('h', 3_600), ('m', 60), ('s', 1)];

/// Parse a duration in seconds: `90`, `90s`, `15m`, `2h`, `31d`, `1w` or combinations such as
/// `1m30s`.
pub fn parse_duration(raw: &str) -> Result<i64, String> {
    let body = raw.trim();
    let invalid = || format!("invalid duration '{raw}' (use seconds, or e.g. 30s, 2m, 1m30s)");
    if !body.starts_with(|c: char| c.is_ascii_digit()) {
        return Err(invalid());
    }
    if let Ok(secs) = body.parse::<i64>() {
        return Ok(secs);
    }
    let (mut total, mut digits) = (0i64, String::new());
    for c in body.chars() {
        if c.is_ascii_digit() {
            digits.push(c);
            continue;
        }
        let unit = UNITS.iter().find(|(u, _)| *u == c).ok_or_else(invalid)?.1;
        let n: i64 = digits.parse().map_err(|_| invalid())?;
        total = n.checked_mul(unit).and_then(|v| total.checked_add(v)).ok_or_else(invalid)?;
        digits.clear();
    }
    if !digits.is_empty() {
        return Err(invalid());
    }
    Ok(total)
}

/// Random hex, `bytes` bytes long.
pub fn random_hex(bytes: usize) -> String {
    (0..bytes).map(|_| format!("{:02x}", rand::random::<u8>())).collect()
}

/// Random characters from `alphabet`.
pub fn random_from(alphabet: &[u8], len: usize) -> String {
    let mut rng = rand::thread_rng();
    (0..len).map(|_| alphabet[rng.gen_range(0..alphabet.len())] as char).collect()
}

pub fn b64_std(data: impl AsRef<[u8]>) -> String {
    STANDARD.encode(data)
}

/// Lenient standard (or URL-safe) base64: whitespace and missing padding are fine.
pub fn b64_decode_lenient(s: &str) -> Option<Vec<u8>> {
    let cleaned: String = s.chars().filter(|c| !c.is_whitespace()).collect();
    STANDARD
        .decode(&cleaned)
        .ok()
        .or_else(|| STANDARD_NO_PAD.decode(cleaned.trim_end_matches('=')).ok())
        .or_else(|| URL_SAFE_NO_PAD.decode(cleaned.trim_end_matches('=')).ok())
}

/// Unix seconds as an RFC 3339 timestamp in UTC, e.g. `2026-10-31T13:00:00Z`.
pub fn iso8601(secs: i64) -> String {
    let days = secs.div_euclid(86_400);
    let rem = secs.rem_euclid(86_400);
    // Howard Hinnant's civil-from-days.
    let z = days + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z.rem_euclid(146_097);
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let y = yoe + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = doy - (153 * mp + 2) / 5 + 1;
    let m = if mp < 10 { mp + 3 } else { mp - 9 };
    let y = if m <= 2 { y + 1 } else { y };
    format!("{y:04}-{m:02}-{d:02}T{:02}:{:02}:{:02}Z", rem / 3600, rem % 3600 / 60, rem % 60)
}

pub fn parse_form(bytes: &[u8]) -> HashMap<String, String> {
    url::form_urlencoded::parse(bytes).into_owned().collect()
}

/// Render a JSON value as a plain string (strings unquoted).
pub fn value_to_string(v: &Value) -> String {
    match v {
        Value::String(s) => s.clone(),
        other => other.to_string(),
    }
}

/// Strings longer than this are cut short when a request is kept for display.
const MAX_KEPT_STRING: usize = 4096;

/// A copy of a request for display: long strings (attachment contents, mostly) are replaced by a
/// note of their length. The message itself keeps everything.
pub fn elide_long_strings(v: &Value) -> Value {
    match v {
        Value::String(s) if s.len() > MAX_KEPT_STRING => {
            let mut cut = MAX_KEPT_STRING / 4;
            while !s.is_char_boundary(cut) {
                cut -= 1;
            }
            Value::String(format!("{}… ({} characters in all)", &s[..cut], s.chars().count()))
        }
        Value::Array(items) => Value::Array(items.iter().map(elide_long_strings).collect()),
        Value::Object(map) => Value::Object(map.iter().map(|(k, v)| (k.clone(), elide_long_strings(v))).collect()),
        other => other.clone(),
    }
}

/// Flatten JSON into `key: value` lines (nested keys dotted), e.g. for template data.
pub fn flatten_json(v: &Value) -> Vec<(String, String)> {
    fn walk(prefix: &str, v: &Value, out: &mut Vec<(String, String)>) {
        match v {
            Value::Object(map) => {
                for (k, v) in map {
                    let key = if prefix.is_empty() { k.clone() } else { format!("{prefix}.{k}") };
                    walk(&key, v, out);
                }
            }
            Value::Array(items) => {
                for (i, v) in items.iter().enumerate() {
                    walk(&format!("{prefix}[{i}]"), v, out);
                }
            }
            other => out.push((prefix.to_string(), value_to_string(other))),
        }
    }
    let mut out = vec![];
    walk("", v, &mut out);
    out
}

/// Replace `{{ name }}` and `{{ a.b }}` placeholders (Handlebars-style, as SES templates use)
/// with values from `data`. Anything fancier (blocks, helpers) is left as it is.
pub fn render_placeholders(template: &str, data: &Value) -> String {
    let re = placeholder_regex();
    re.replace_all(template, |caps: &regex::Captures| {
        let path = &caps[1];
        let mut cur = data;
        for part in path.split('.') {
            match cur.get(part) {
                Some(v) => cur = v,
                None => return caps[0].to_string(),
            }
        }
        value_to_string(cur)
    })
    .into_owned()
}

fn placeholder_regex() -> &'static regex::Regex {
    static RE: std::sync::OnceLock<regex::Regex> = std::sync::OnceLock::new();
    RE.get_or_init(|| regex::Regex::new(r"\{\{\{?\s*([A-Za-z_][\w.-]*)\s*\}?\}\}").unwrap())
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn durations() {
        assert_eq!(parse_duration("90"), Ok(90));
        assert_eq!(parse_duration("15m"), Ok(900));
        assert_eq!(parse_duration("1m30s"), Ok(90));
        for bad in ["", "s", "5x", "1m5", "-1", "1.5s"] {
            assert!(parse_duration(bad).is_err(), "{bad}");
        }
    }

    #[test]
    fn timestamps() {
        assert_eq!(iso8601(0), "1970-01-01T00:00:00Z");
        assert_eq!(iso8601(1_793_451_600), "2026-10-31T13:00:00Z");
    }

    #[test]
    fn placeholders() {
        let data = json!({"name": "Ada", "code": 123456, "user": {"plan": "pro"}});
        assert_eq!(
            render_placeholders("Hi {{name}}, code {{ code }} on {{{user.plan}}} {{missing}}", &data),
            "Hi Ada, code 123456 on pro {{missing}}"
        );
    }

    #[test]
    fn flattening() {
        let flat = flatten_json(&json!({"a": 1, "b": {"c": "x"}, "d": ["y"]}));
        assert_eq!(flat, vec![("a".into(), "1".into()), ("b.c".into(), "x".into()), ("d[0]".into(), "y".into())]);
    }
}
