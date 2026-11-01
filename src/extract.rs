//! Pull one-time codes and links out of a message, for tests that sign up, verify an address or
//! reset a password.

use std::sync::OnceLock;

use regex::Regex;
use serde::Serialize;

#[derive(Debug, Clone, Serialize, PartialEq, Eq)]
pub struct Link {
    pub href: String,
    /// The link's text (HTML), or empty for a URL found in plain text.
    pub text: String,
}

/// What to extract (`extract=otp,links`, `regex=…`, `otp_pattern=…`, `link_contains=…`).
#[derive(Debug, Clone, Default)]
pub struct Request {
    pub otp: bool,
    pub links: bool,
    pub otp_pattern: Option<Regex>,
    pub regex: Option<Regex>,
    pub link_contains: Option<String>,
}

impl Request {
    pub fn parse(
        extract: Option<&str>,
        otp_pattern: Option<&str>,
        regex: Option<&str>,
        link_contains: Option<&str>,
    ) -> Result<Request, String> {
        let mut req = Request::default();
        for what in extract.unwrap_or_default().split(',').map(str::trim).filter(|w| !w.is_empty()) {
            match what {
                "otp" | "code" => req.otp = true,
                "links" | "link" => req.links = true,
                other => return Err(format!("unknown extractor '{other}' (use otp or links)")),
            }
        }
        let compile = |p: &str, what: &str| Regex::new(p).map_err(|e| format!("invalid {what}: {e}"));
        if let Some(p) = otp_pattern.filter(|p| !p.is_empty()) {
            req.otp_pattern = Some(compile(p, "otp_pattern")?);
            req.otp = true;
        }
        if let Some(p) = regex.filter(|p| !p.is_empty()) {
            req.regex = Some(compile(p, "regex")?);
        }
        if let Some(l) = link_contains.filter(|l| !l.is_empty()) {
            req.link_contains = Some(l.to_string());
            req.links = true;
        }
        Ok(req)
    }

    pub fn is_empty(&self) -> bool {
        !self.otp && !self.links && self.regex.is_none()
    }

    pub fn run(&self, text: &str, html: Option<&str>) -> serde_json::Value {
        let mut out = serde_json::Map::new();
        if self.otp {
            let otp = match &self.otp_pattern {
                Some(re) => first_match(re, text),
                None => otp(text),
            };
            out.insert("otp".into(), otp.into());
        }
        if self.links {
            let mut found = links(text, html);
            if let Some(needle) = &self.link_contains {
                found.retain(|l| l.href.contains(needle.as_str()));
            }
            out.insert("link".into(), found.first().map(|l| l.href.clone()).into());
            out.insert("links".into(), serde_json::to_value(found).unwrap_or_default());
        }
        if let Some(re) = &self.regex {
            out.insert("regex".into(), first_match(re, text).into());
        }
        serde_json::Value::Object(out)
    }
}

/// The first capture group, or the whole match if there isn't one.
fn first_match(re: &Regex, text: &str) -> Option<String> {
    let caps = re.captures(text)?;
    caps.get(1).or_else(|| caps.get(0)).map(|m| m.as_str().to_string())
}

fn re(cell: &'static OnceLock<Regex>, pattern: &str) -> &'static Regex {
    cell.get_or_init(|| Regex::new(pattern).unwrap())
}

const KEYWORDS: [&str; 12] = [
    "code",
    "otp",
    "verification",
    "verify",
    "one-time",
    "one time",
    "passcode",
    "pin",
    "token",
    "security",
    "login",
    "confirm",
];

/// The most likely one-time code: 4–8 digits (or `123 456` / `123-456`), preferring ones near a
/// word like "code" or "verification", then six digits, then the first. Years and digits inside
/// longer numbers, URLs or IDs don't count. Mixed letter-and-digit codes are only taken next to a
/// keyword.
pub fn otp(text: &str) -> Option<String> {
    static DIGITS: OnceLock<Regex> = OnceLock::new();
    static ALNUM: OnceLock<Regex> = OnceLock::new();
    static URL: OnceLock<Regex> = OnceLock::new();
    let url = re(&URL, r"(?i)\bhttps?://\S+");
    // URLs are full of digits that aren't codes.
    let text = url.replace_all(text, " ");
    // `start` is a byte offset into `text` (lower-casing can change lengths, so slice first).
    let near_keyword = |start: usize| {
        let mut from = start.saturating_sub(80);
        while !text.is_char_boundary(from) {
            from -= 1;
        }
        let before = text[from..start].to_lowercase();
        KEYWORDS.iter().any(|k| before.contains(k))
    };

    let mut candidates: Vec<(i32, usize, String)> = vec![];
    let digits = re(&DIGITS, r"(?:^|[^\w.,/#:-])(\d{3}[ -]\d{3}|\d{4,8})(?:$|[^\w/@-])");
    let bytes = text.as_bytes();
    for caps in digits.captures_iter(&text) {
        let m = caps.get(1).unwrap();
        // "1,234,567" or "12.50": part of a bigger number.
        if m.end() + 1 < bytes.len() && matches!(bytes[m.end()], b'.' | b',') && bytes[m.end() + 1].is_ascii_digit() {
            continue;
        }
        let code: String = m.as_str().chars().filter(char::is_ascii_digit).collect();
        let near = near_keyword(m.start());
        let year = code.len() == 4 && matches!(code.parse::<u32>(), Ok(1900..=2099));
        if year && !near {
            continue;
        }
        let score = if near { 100 } else { 0 } + if code.len() == 6 { 10 } else { 0 };
        candidates.push((score, m.start(), code));
    }
    let alnum = re(&ALNUM, r"(?:^|[^\w-])([A-Z0-9]{4,10})(?:$|[^\w-])");
    for caps in alnum.captures_iter(&text) {
        let m = caps.get(1).unwrap();
        let s = m.as_str();
        if s.chars().any(|c| c.is_ascii_digit()) && s.chars().any(|c| c.is_ascii_uppercase()) && near_keyword(m.start())
        {
            candidates.push((90, m.start(), s.to_string()));
        }
    }
    candidates.sort_by(|a, b| b.0.cmp(&a.0).then(a.1.cmp(&b.1)));
    candidates.into_iter().next().map(|(_, _, c)| c)
}

/// Links from the HTML (`<a href>`, with their text) and then URLs in the text, without repeats.
pub fn links(text: &str, html: Option<&str>) -> Vec<Link> {
    static ANCHOR: OnceLock<Regex> = OnceLock::new();
    static TAG: OnceLock<Regex> = OnceLock::new();
    static URL: OnceLock<Regex> = OnceLock::new();
    let mut out: Vec<Link> = vec![];
    let mut push = |href: String, text: String| {
        if !href.is_empty() && !out.iter().any(|l| l.href == href) {
            out.push(Link { href, text });
        }
    };
    if let Some(html) = html {
        let anchor = re(&ANCHOR, r#"(?is)<a\b[^>]*?\bhref\s*=\s*(?:"([^"]*)"|'([^']*)'|([^\s>]+))[^>]*>(.*?)</a\s*>"#);
        let tag = re(&TAG, r"(?s)<[^>]*>");
        for caps in anchor.captures_iter(html) {
            let href = caps.get(1).or(caps.get(2)).or(caps.get(3)).map(|m| m.as_str()).unwrap_or_default();
            let href = decode_entities(href.trim());
            if !(href.starts_with("http://") || href.starts_with("https://")) {
                continue;
            }
            let label = tag.replace_all(caps.get(4).map(|m| m.as_str()).unwrap_or_default(), "");
            let label = decode_entities(&label).split_whitespace().collect::<Vec<_>>().join(" ");
            push(href, label);
        }
    }
    let url = re(&URL, r#"(?i)\bhttps?://[^\s<>"'\])]+"#);
    for m in url.find_iter(text) {
        let href = m.as_str().trim_end_matches(['.', ',', ';', ':', '!', '?']);
        push(href.to_string(), String::new());
    }
    out
}

pub fn decode_entities(s: &str) -> String {
    s.replace("&amp;", "&")
        .replace("&lt;", "<")
        .replace("&gt;", ">")
        .replace("&quot;", "\"")
        .replace("&#39;", "'")
        .replace("&#x27;", "'")
        .replace("&nbsp;", " ")
        .replace("&#160;", " ")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn codes() {
        assert_eq!(otp("Your verification code is 482913. It expires in 10 minutes."), Some("482913".into()));
        assert_eq!(otp("Order #12345 placed in 2026.\nYour code: 0042"), Some("0042".into()));
        assert_eq!(otp("Use 123 456 to sign in (code)"), Some("123456".into()));
        assert_eq!(otp("Copyright 2026. Visit https://x.test/a/123456"), None);
        assert_eq!(otp("Your one-time passcode: K7Q9ZP"), Some("K7Q9ZP".into()));
        assert_eq!(otp("Total: 1,234,567 points"), None);
        assert_eq!(otp("Hi 4821"), Some("4821".into()));
        assert_eq!(otp("İİİİ ẞ code 135790"), Some("135790".into()));
        let table = crate::message::html_to_text(
            "<table><tr><td>Code</td><td>123456</td></tr></table><style>.x{color:#222222}</style>",
        );
        assert_eq!(otp(&table), Some("123456".into()));
    }

    #[test]
    fn link_extraction() {
        let html = r#"<p><a href="https://app.test/verify?token=a&amp;b=1">Verify <b>email</b></a> <a href="mailto:x@y">mail</a></p>"#;
        let found = links("Or paste https://app.test/verify?token=a&b=1. Help: https://app.test/help", Some(html));
        assert_eq!(
            found,
            vec![
                Link { href: "https://app.test/verify?token=a&b=1".into(), text: "Verify email".into() },
                Link { href: "https://app.test/help".into(), text: String::new() },
            ]
        );
    }
}
