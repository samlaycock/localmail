//! The path every API send takes after the provider adapter has read the request: scenario
//! addresses are applied, the messages are stored, and an [`AttemptNote`] records what happened.
//! The adapter then answers in its provider's format.

use crate::address::Addr;
use crate::attempts::AttemptNote;
use crate::message::{Draft, NewMessage, Provider};
use crate::scenarios::{self, Fail};
use crate::state::AppState;

/// Why a send (or one message of a batch) failed. Each adapter turns these into its provider's
/// error response.
#[derive(Debug, Clone)]
pub enum Failure {
    /// No credentials, or not the kind the provider expects.
    MissingAuth,
    /// The credentials were refused (the `unauthorized` scenario).
    BadAuth,
    /// The request is invalid, as the provider would say.
    Invalid(Invalid),
    /// The recipient was rejected (the `reject` scenario).
    Rejected(String),
    RateLimited,
    ServerError,
}

#[derive(Debug, Clone, Default)]
pub struct Invalid {
    pub message: String,
    /// The field at fault, in the provider's notation.
    pub field: Option<String>,
    /// A provider-specific error code or name (Postmark's ErrorCode, Resend's name, SES's type).
    pub code: Option<String>,
    /// An HTTP status other than the adapter's usual one for invalid requests.
    pub status: Option<u16>,
}

impl Failure {
    pub fn invalid(message: impl Into<String>) -> Failure {
        Failure::Invalid(Invalid { message: message.into(), ..Default::default() })
    }

    pub fn field(message: impl Into<String>, field: &str) -> Failure {
        Failure::Invalid(Invalid { message: message.into(), field: Some(field.into()), ..Default::default() })
    }

    pub fn coded(message: impl Into<String>, code: impl Into<String>) -> Failure {
        Failure::Invalid(Invalid { message: message.into(), code: Some(code.into()), ..Default::default() })
    }

    /// A short code and description for the attempts log.
    pub fn describe(&self) -> (String, String) {
        match self {
            Failure::MissingAuth => {
                ("missing_credentials".into(), "No credentials, or not the kind the provider expects".into())
            }
            Failure::BadAuth => ("unauthorized".into(), "Credentials refused (unauthorized scenario)".into()),
            Failure::Invalid(i) => ("invalid_request".into(), i.message.clone()),
            Failure::Rejected(addr) => ("rejected".into(), format!("{addr} was rejected (reject scenario)")),
            Failure::RateLimited => ("rate_limited".into(), "Too many requests (ratelimit scenario)".into()),
            Failure::ServerError => ("server_error".into(), "Server error (error or flaky scenario)".into()),
        }
    }
}

impl From<Fail> for Failure {
    fn from(f: Fail) -> Failure {
        match f {
            Fail::Unauthorized => Failure::BadAuth,
            Fail::RateLimit => Failure::RateLimited,
            Fail::Error => Failure::ServerError,
            Fail::Reject => Failure::Rejected(String::new()),
        }
    }
}

/// A message that was stored.
#[derive(Debug, Clone)]
pub struct Sent {
    /// localmail's id.
    pub id: i64,
    /// The provider-format id returned to the client.
    pub message_id: String,
    pub to: Vec<Addr>,
}

#[derive(Debug, Clone)]
pub enum ItemResult {
    Sent(Sent),
    Failed(Failure),
}

pub struct Outcome {
    /// The whole request failed, or one result per message.
    pub result: Result<Vec<ItemResult>, Failure>,
    pub note: AttemptNote,
}

fn fail(mut note: AttemptNote, failure: Failure) -> Outcome {
    let (code, description) = failure.describe();
    note.error = Some(code);
    note.error_description = Some(description);
    Outcome { result: Err(failure), note }
}

/// Store a request's messages. `items` is the adapter's reading of the request: a failure for the
/// whole of it, or one draft (or invalid message) per message. With `per_item`, invalid and
/// rejected messages fail on their own (Postmark batches, Resend's permissive batches); otherwise
/// any failure fails the whole request.
pub async fn submit(
    state: &AppState,
    provider: Provider,
    endpoint: &str,
    items: Result<Vec<Result<Draft, Failure>>, Failure>,
    per_item: bool,
) -> Outcome {
    let mut note = AttemptNote::new(provider, endpoint);
    let items = match items {
        Ok(items) => items,
        Err(f) => return fail(note, f),
    };
    let drafts: Vec<&Draft> = items.iter().filter_map(|i| i.as_ref().ok()).collect();
    note.from = drafts.iter().find_map(|d| d.from.as_ref()).map(Addr::display);
    let mut recipients: Vec<&Addr> = vec![];
    for a in drafts.iter().flat_map(|d| d.recipients()) {
        if !recipients.iter().any(|r| r.key() == a.key()) {
            recipients.push(a);
        }
    }
    note.recipients = recipients.iter().map(|a| a.email.clone()).collect();
    note.subject = drafts.first().map(|d| d.subject.clone()).filter(|s| !s.is_empty());

    if !per_item && let Some(Err(f)) = items.iter().find(|i| i.is_err()) {
        return fail(note, f.clone());
    }

    let domain = &state.config.scenario_domain;
    let subjects: String = drafts.iter().map(|d| d.subject.as_str()).collect::<Vec<_>>().join("\n");
    let fingerprint =
        scenarios::fingerprint(drafts.first().and_then(|d| d.from.as_ref()), recipients.iter().copied(), &subjects);
    let plan = scenarios::plan(&state.flaky, domain, recipients.iter().copied(), &fingerprint, !per_item);
    note.warnings = plan.warnings.iter().map(|w| format!("not a scenario address: {w}")).collect();
    note.scenario = plan.applied.clone();
    if let Some(delay) = plan.delay {
        tokio::time::sleep(delay).await;
    }
    if let Some((f, addr)) = plan.failure {
        let failure = match f {
            Fail::Reject => Failure::Rejected(addr),
            other => other.into(),
        };
        return fail(note, failure);
    }

    let mut results = Vec::with_capacity(items.len());
    for item in items {
        let draft = match item {
            Ok(d) => d,
            Err(f) => {
                results.push(ItemResult::Failed(f));
                continue;
            }
        };
        if per_item && let Some(addr) = scenarios::rejected(domain, draft.recipients()) {
            note.scenario.get_or_insert_with(|| addr.clone());
            results.push(ItemResult::Failed(Failure::Rejected(addr)));
            continue;
        }
        let message_id = provider.new_message_id();
        let to = draft.to.clone();
        let msg = NewMessage::from_draft(draft, provider, endpoint, message_id.clone(), plan.applied.clone());
        match state.store.insert(&msg) {
            Some(id) => {
                note.messages.push(id);
                results.push(ItemResult::Sent(Sent { id, message_id, to }));
            }
            None => results.push(ItemResult::Failed(Failure::ServerError)),
        }
    }
    let failed: Vec<&Failure> = results
        .iter()
        .filter_map(|r| match r {
            ItemResult::Failed(f) => Some(f),
            _ => None,
        })
        .collect();
    if let Some(first) = failed.first() {
        let (code, description) = first.describe();
        note.error = Some(code);
        note.error_description = Some(if failed.len() > 1 {
            format!("{description} (and {} more failed)", failed.len() - 1)
        } else {
            description
        });
        note.partial = failed.len() < results.len();
    }
    Outcome { result: Ok(results), note }
}
