//! Email addresses as the provider APIs take them: a bare `alice@example.com`, or
//! `Alice <alice@example.com>` (Postmark and Resend), possibly in a comma-separated list.

use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Addr {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub name: Option<String>,
    pub email: String,
}

impl Addr {
    pub fn new(name: Option<&str>, email: &str) -> Self {
        Self { name: name.map(str::trim).filter(|n| !n.is_empty()).map(String::from), email: email.trim().to_string() }
    }

    /// `Name <email>`, or just the address.
    pub fn display(&self) -> String {
        match &self.name {
            Some(n) => format!("{n} <{}>", self.email),
            None => self.email.clone(),
        }
    }

    /// Lower-cased address, for matching.
    pub fn key(&self) -> String {
        self.email.to_ascii_lowercase()
    }

    pub fn is_valid(&self) -> bool {
        is_valid_email(&self.email)
    }
}

/// A plausible address: one `@`, something on each side, a dot-free-or-dotted domain without
/// spaces. Deliberately loose: the point is to catch obvious mistakes the providers reject too.
pub fn is_valid_email(email: &str) -> bool {
    let Some((local, domain)) = email.rsplit_once('@') else { return false };
    !local.is_empty()
        && !domain.is_empty()
        && !email.chars().any(|c| c.is_whitespace() || c == '<' || c == '>' || c == ',')
        && !domain.starts_with('.')
        && !domain.ends_with('.')
        && !domain.contains("..")
        && !local.contains('@')
}

/// Parse one `Name <email>` or `email`.
pub fn parse_one(raw: &str) -> Addr {
    let raw = raw.trim();
    if let (Some(open), true) = (raw.rfind('<'), raw.ends_with('>')) {
        let name = raw[..open].trim().trim_matches('"').replace("\\\"", "\"");
        return Addr::new(Some(&name), &raw[open + 1..raw.len() - 1]);
    }
    Addr::new(None, raw)
}

/// Parse a comma-separated list, respecting quoted names (`"Doe, Jane" <jane@example.com>`).
pub fn parse_list(raw: &str) -> Vec<Addr> {
    let mut out = vec![];
    let (mut current, mut quoted, mut angle) = (String::new(), false, false);
    let mut chars = raw.chars().peekable();
    while let Some(c) = chars.next() {
        match c {
            '\\' if quoted => {
                current.push(c);
                if let Some(next) = chars.next() {
                    current.push(next);
                }
            }
            '"' => {
                quoted = !quoted;
                current.push(c);
            }
            '<' if !quoted => {
                angle = true;
                current.push(c);
            }
            '>' if !quoted => {
                angle = false;
                current.push(c);
            }
            ',' | ';' if !quoted && !angle => {
                if !current.trim().is_empty() {
                    out.push(parse_one(&current));
                }
                current.clear();
            }
            c => current.push(c),
        }
    }
    if !current.trim().is_empty() {
        out.push(parse_one(&current));
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parsing() {
        assert_eq!(parse_one("a@b.test"), Addr::new(None, "a@b.test"));
        assert_eq!(parse_one("Acme <hi@acme.test>"), Addr::new(Some("Acme"), "hi@acme.test"));
        assert_eq!(parse_one("\"Doe, Jane\" <jane@x.test>").name.as_deref(), Some("Doe, Jane"));
        let list = parse_list("a@x.test, \"Doe, Jane\" <jane@x.test>;B <b@x.test>");
        assert_eq!(list.iter().map(|a| a.email.as_str()).collect::<Vec<_>>(), ["a@x.test", "jane@x.test", "b@x.test"]);
        assert!(parse_list(" , ").is_empty());
    }

    #[test]
    fn validity() {
        for ok in ["a@b", "a.b+c@d.example", "x@localmail.test"] {
            assert!(is_valid_email(ok), "{ok}");
        }
        for bad in ["", "a", "@b", "a@", "a b@c", "a@.b", "a@b.", "a@b..c", "a@b@c"] {
            assert!(!is_valid_email(bad), "{bad}");
        }
    }
}
