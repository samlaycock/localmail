//! Scenario addresses: sending to `<scenario>@localmail.test` makes the send itself fail (or slow
//! down) in the way the provider would, so an app's error handling and retries can be tested.
//! A `+suffix` is ignored (`ratelimit+signup@localmail.test`), so tests can keep them apart.

use std::collections::HashMap;
use std::sync::Mutex;
use std::time::Duration;

use serde::Serialize;
use sha2::{Digest, Sha256};

use crate::address::Addr;

/// Longest delay `slow-<duration>` may add.
pub const MAX_DELAY_SECS: i64 = 60;
/// Most failures `flaky-<n>` may ask for.
pub const MAX_FLAKY: u32 = 100;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Scenario {
    /// Permanent rejection of the recipient.
    Reject,
    /// The credentials are refused.
    Unauthorized,
    /// Too many requests.
    RateLimit,
    /// A server error.
    Error,
    /// Answer (successfully) after this many seconds.
    Slow(i64),
    /// Fail with a server error this many times, then succeed once.
    Flaky(u32),
}

impl Scenario {
    pub fn parse(local: &str) -> Result<Scenario, String> {
        let name = local.split('+').next().unwrap_or_default().to_ascii_lowercase();
        Ok(match name.as_str() {
            "reject" => Scenario::Reject,
            "unauthorized" => Scenario::Unauthorized,
            "ratelimit" => Scenario::RateLimit,
            "error" => Scenario::Error,
            other => {
                if let Some(d) = other.strip_prefix("slow-") {
                    let secs = crate::util::parse_duration(d)?;
                    if !(1..=MAX_DELAY_SECS).contains(&secs) {
                        return Err(format!("slow-<duration> must be between 1s and {MAX_DELAY_SECS}s"));
                    }
                    Scenario::Slow(secs)
                } else if let Some(n) = other.strip_prefix("flaky-") {
                    match n.parse::<u32>() {
                        Ok(n) if (1..=MAX_FLAKY).contains(&n) => Scenario::Flaky(n),
                        _ => return Err(format!("flaky-<n> needs a number of failures from 1 to {MAX_FLAKY}")),
                    }
                } else {
                    return Err(format!("'{other}' isn't a scenario"));
                }
            }
        })
    }

    /// Which scenario wins when a send has several: the first in this order.
    fn precedence(self) -> u8 {
        match self {
            Scenario::Unauthorized => 0,
            Scenario::RateLimit => 1,
            Scenario::Error => 2,
            Scenario::Flaky(_) => 3,
            Scenario::Reject => 4,
            Scenario::Slow(_) => 5,
        }
    }
}

/// How a scenario makes a send fail.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Fail {
    Reject,
    Unauthorized,
    RateLimit,
    Error,
}

/// What the scenario addresses among a send's recipients ask for.
#[derive(Debug, Default)]
pub struct Plan {
    pub delay: Option<Duration>,
    /// The failure, and the scenario address that caused it.
    pub failure: Option<(Fail, String)>,
    /// The scenario address that applied (for the record), failing or not.
    pub applied: Option<String>,
    /// Addresses at the scenario domain that aren't scenarios (probably typos).
    pub warnings: Vec<String>,
}

/// Counts failures of `flaky-<n>` sends, per scenario address and message.
#[derive(Debug, Default)]
pub struct FlakyCounters(Mutex<HashMap<String, u32>>);

impl FlakyCounters {
    pub fn reset(&self) -> usize {
        let mut map = self.0.lock().unwrap();
        let n = map.len();
        map.clear();
        n
    }

    pub fn snapshot(&self) -> Vec<FlakyCounter> {
        let mut out: Vec<_> =
            self.0.lock().unwrap().iter().map(|(k, v)| FlakyCounter { key: k.clone(), failures: *v }).collect();
        out.sort_by(|a, b| a.key.cmp(&b.key));
        out
    }

    /// Whether this attempt should fail: true for the first `n` attempts, then false once, after
    /// which the count starts again.
    fn fail(&self, key: &str, n: u32) -> bool {
        let mut map = self.0.lock().unwrap();
        let count = map.entry(key.to_string()).or_insert(0);
        if *count < n {
            *count += 1;
            true
        } else {
            map.remove(key);
            false
        }
    }
}

#[derive(Debug, Serialize)]
pub struct FlakyCounter {
    pub key: String,
    pub failures: u32,
}

/// The scenario an address names, if it's at the scenario domain.
pub fn scenario_of(addr: &str, domain: &str) -> Option<Result<Scenario, String>> {
    let (local, d) = addr.trim().rsplit_once('@')?;
    d.eq_ignore_ascii_case(domain).then(|| Scenario::parse(local))
}

/// A fingerprint of a send, so retries of the same message share a flaky counter.
pub fn fingerprint<'a>(from: Option<&Addr>, recipients: impl Iterator<Item = &'a Addr>, subject: &str) -> String {
    let mut rcpts: Vec<String> = recipients.map(Addr::key).collect();
    rcpts.sort();
    rcpts.dedup();
    let mut h = Sha256::new();
    h.update(from.map(Addr::key).unwrap_or_default());
    h.update([0]);
    h.update(rcpts.join(","));
    h.update([0]);
    h.update(subject);
    h.finalize().iter().take(8).map(|b| format!("{b:02x}")).collect()
}

/// Work out what the scenario addresses among `recipients` do to this send. Flaky counters are
/// advanced here, so call it once per attempt. With `with_reject` false, `reject` addresses are
/// left for the caller (batches that reject messages one by one).
pub fn plan<'a>(
    counters: &FlakyCounters,
    domain: &str,
    recipients: impl Iterator<Item = &'a Addr>,
    fingerprint: &str,
    with_reject: bool,
) -> Plan {
    let mut plan = Plan::default();
    let mut found: Vec<(Scenario, String)> = vec![];
    for a in recipients {
        match scenario_of(&a.email, domain) {
            Some(Ok(s)) => {
                if !found.iter().any(|(_, addr)| addr.eq_ignore_ascii_case(&a.email)) {
                    found.push((s, a.email.clone()));
                }
            }
            Some(Err(e)) => plan.warnings.push(format!("{}: {e}", a.email)),
            None => {}
        }
    }
    found.sort_by_key(|(s, _)| s.precedence());
    plan.delay = found
        .iter()
        .filter_map(|(s, _)| match s {
            Scenario::Slow(secs) => Some(*secs),
            _ => None,
        })
        .max()
        .map(|secs| Duration::from_secs(secs as u64));
    for (s, addr) in &found {
        let fail = match s {
            Scenario::Unauthorized => Some(Fail::Unauthorized),
            Scenario::RateLimit => Some(Fail::RateLimit),
            Scenario::Error => Some(Fail::Error),
            Scenario::Flaky(n) => {
                plan.applied.get_or_insert_with(|| addr.clone());
                counters.fail(&format!("{}|{fingerprint}", addr.to_ascii_lowercase()), *n).then_some(Fail::Error)
            }
            Scenario::Reject if with_reject => Some(Fail::Reject),
            Scenario::Reject => None,
            Scenario::Slow(_) => {
                plan.applied.get_or_insert_with(|| addr.clone());
                None
            }
        };
        if let Some(f) = fail {
            plan.applied = Some(addr.clone());
            plan.failure = Some((f, addr.clone()));
            break;
        }
    }
    plan
}

/// The first `reject` scenario address among `recipients`.
pub fn rejected<'a>(domain: &str, mut recipients: impl Iterator<Item = &'a Addr>) -> Option<String> {
    recipients.find(|a| matches!(scenario_of(&a.email, domain), Some(Ok(Scenario::Reject)))).map(|a| a.email.clone())
}

/// One scenario, for `GET /dev/api/scenarios` and the setup page.
#[derive(Debug, Serialize)]
pub struct Doc {
    pub address: String,
    pub effect: &'static str,
}

pub fn docs(domain: &str) -> Vec<Doc> {
    vec![
        Doc {
            address: format!("reject@{domain}"),
            effect: "The recipient is rejected permanently (SMTP 550, a 4xx from the APIs).",
        },
        Doc {
            address: format!("unauthorized@{domain}"),
            effect: "The credentials are refused (SMTP 530, HTTP 401 or 403).",
        },
        Doc {
            address: format!("ratelimit@{domain}"),
            effect: "Too many requests (SMTP 451 4.7.1, HTTP 429; SES v1 answers 400 Throttling).",
        },
        Doc { address: format!("error@{domain}"), effect: "A server error (SMTP 451 4.3.0, HTTP 500)." },
        Doc {
            address: format!("slow-5s@{domain}"),
            effect: "The send succeeds after a delay (up to 60s), for client timeouts.",
        },
        Doc {
            address: format!("flaky-2@{domain}"),
            effect: "The same message fails with a server error twice, then succeeds; then the count starts again.",
        },
    ]
}

#[cfg(test)]
mod tests {
    use super::*;

    fn addrs(list: &[&str]) -> Vec<Addr> {
        list.iter().map(|a| Addr::new(None, a)).collect()
    }

    #[test]
    fn parsing() {
        let d = "localmail.test";
        assert_eq!(scenario_of("reject@localmail.test", d), Some(Ok(Scenario::Reject)));
        assert_eq!(scenario_of("RateLimit+signup@LocalMail.test", d), Some(Ok(Scenario::RateLimit)));
        assert_eq!(scenario_of("slow-1m@localmail.test", d), Some(Ok(Scenario::Slow(60))));
        assert_eq!(scenario_of("flaky-3+x@localmail.test", d), Some(Ok(Scenario::Flaky(3))));
        assert!(matches!(scenario_of("rejcet@localmail.test", d), Some(Err(_))));
        assert!(matches!(scenario_of("slow-2m@localmail.test", d), Some(Err(_))));
        assert_eq!(scenario_of("reject@example.com", d), None);
    }

    #[test]
    fn precedence_and_flaky() {
        let c = FlakyCounters::default();
        let d = "localmail.test";
        let p =
            plan(&c, d, addrs(&["a@x.test", "reject@localmail.test", "ratelimit@localmail.test"]).iter(), "f", true);
        assert_eq!(p.failure, Some((Fail::RateLimit, "ratelimit@localmail.test".into())));
        let p = plan(&c, d, addrs(&["reject@localmail.test"]).iter(), "f", false);
        assert_eq!(p.failure, None);

        let rcpts = addrs(&["flaky-2@localmail.test"]);
        let outcomes: Vec<bool> = (0..5).map(|_| plan(&c, d, rcpts.iter(), "f", true).failure.is_some()).collect();
        assert_eq!(outcomes, [true, true, false, true, true]);
        // Another message has its own count.
        assert!(plan(&c, d, rcpts.iter(), "g", true).failure.is_some());
        assert_eq!(c.reset(), 2);
    }
}
