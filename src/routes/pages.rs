use axum::extract::State;
use axum::http::header;
use axum::response::{IntoResponse, Response};
use serde_json::json;

use crate::html::{data_script, esc, page};
use crate::message::Provider;
use crate::state::AppState;

// The assets are compiled in and can change with every upgrade, so browsers must not reuse a
// stale copy against newer HTML. They're small and served locally, so always revalidating is cheap.
pub async fn css() -> impl IntoResponse {
    (
        [(header::CONTENT_TYPE, "text/css; charset=utf-8"), (header::CACHE_CONTROL, "no-cache")],
        include_str!("../../assets/app.css"),
    )
}

pub async fn js() -> impl IntoResponse {
    (
        [(header::CONTENT_TYPE, "application/javascript; charset=utf-8"), (header::CACHE_CONTROL, "no-cache")],
        include_str!("../../assets/app.js"),
    )
}

fn providers_json() -> serde_json::Value {
    Provider::ALL.iter().map(|p| json!({ "id": p.id(), "name": p.name(), "color": p.color() })).collect()
}

pub async fn inbox(State(state): State<AppState>) -> Response {
    let data = json!({ "providers": providers_json(), "smtp": smtp_address(&state), "base": state.config.base_url });
    let body = format!(
        r#"<div class="inbox" id="lm-inbox">
  <section class="list-pane">
    <div class="list-toolbar">
      <input id="lm-search" type="search" placeholder="Search" aria-label="Search messages">
      <select id="lm-provider" aria-label="Filter by provider"><option value="">All</option></select>
    </div>
    <div class="list-actions">
      <span id="lm-count" class="muted small"></span>
      <span class="spacer"></span>
      <button class="btn small ghost" id="lm-read-all" type="button">Mark all read</button>
      <button class="btn small ghost danger" id="lm-delete-all" type="button">Delete all</button>
    </div>
    <ol class="messages" id="lm-messages" aria-label="Messages"></ol>
  </section>
  <section class="view-pane" id="lm-view" aria-live="polite"></section>
</div>
{}"#,
        data_script(&data)
    );
    page("Inbox", "inbox", &body).into_response()
}

pub async fn attempts() -> Response {
    let body = format!(
        r#"<section class="card">
  <div class="eyebrow">Attempts</div>
  <h1>Every send, accepted or not</h1>
  <p class="muted">The last {capacity} sends over SMTP and the provider APIs, newest first, including the ones that failed validation or hit a scenario address. Click one to see the request and the response. Tests can read the same list from <code>GET /dev/api/attempts</code>.</p>
  <div class="req-toolbar">
    <select id="att-provider" aria-label="Filter by provider"><option value="">All providers</option>{options}</select>
    <label class="check"><input type="checkbox" id="att-errors"> Failures only</label>
    <span class="req-spacer"></span>
    <button class="btn small" id="att-pause" type="button">Pause</button>
    <button class="btn small ghost" id="att-clear" type="button">Clear</button>
  </div>
  <div id="lm-attempts"></div>
</section>
{data}"#,
        capacity = crate::attempts::CAPACITY,
        options = Provider::ALL
            .iter()
            .map(|p| format!(r#"<option value="{}">{}</option>"#, p.id(), esc(p.name())))
            .collect::<String>(),
        data = data_script(&json!({ "providers": providers_json() })),
    );
    page("Attempts", "attempts", &body).into_response()
}

fn smtp_address(state: &AppState) -> String {
    let host = url::Url::parse(&state.config.base_url)
        .ok()
        .and_then(|u| u.host_str().map(String::from))
        .unwrap_or_else(|| "localhost".into());
    format!("{host}:{}", state.config.smtp_port)
}

fn snippet(title: &str, code: &str) -> String {
    format!(
        r#"<div class="snippet"><div class="token-head"><b>{}</b><button type="button" class="btn small" data-copy-next>Copy</button></div><pre class="code">{}</pre></div>"#,
        esc(title),
        esc(code)
    )
}

pub async fn setup(State(state): State<AppState>) -> Response {
    let base = &state.config.base_url;
    let smtp = smtp_address(&state);
    let (smtp_host, smtp_port) = smtp.rsplit_once(':').unwrap_or((&smtp, "2525"));
    let host_port = base.trim_start_matches("http://").trim_start_matches("https://");
    let domain = &state.config.scenario_domain;

    let endpoints: String = [
        ("SMTP (any AUTH PLAIN/LOGIN credentials, no TLS)", smtp.clone()),
        ("Amazon SES v2: SendEmail", format!("POST {base}/v2/email/outbound-emails")),
        ("Amazon SES v1: SendEmail, SendRawEmail, SendTemplatedEmail", format!("POST {base}/ (Action=…)")),
        ("SendGrid: Mail Send", format!("POST {base}/v3/mail/send")),
        (
            "Postmark: email, batch and template sends",
            format!("POST {base}/email[/batch|/withTemplate|/batchWithTemplates]"),
        ),
        ("Resend: send and batch", format!("POST {base}/emails[/batch]")),
        ("Dev API (messages, wait, attempts, scenarios)", format!("{base}/dev/api/…")),
    ]
    .iter()
    .map(|(label, url)| format!("<tr><td>{}</td><td><code>{}</code></td></tr>", esc(label), esc(url)))
    .collect();

    let scenarios: String = crate::scenarios::docs(domain)
        .iter()
        .map(|d| format!("<tr><td><code>{}</code></td><td>{}</td></tr>", esc(&d.address), esc(d.effect)))
        .collect();

    let snippets = [
        snippet(
            "SMTP: Nodemailer",
            &format!(
                "nodemailer.createTransport({{ host: '{smtp_host}', port: {smtp_port}, secure: false, ignoreTLS: true }})"
            ),
        ),
        snippet(
            "SMTP: Python",
            &format!("import smtplib\nsmtplib.SMTP('{smtp_host}', {smtp_port}).send_message(msg)"),
        ),
        snippet(
            "Amazon SES: any AWS SDK (environment)",
            &format!(
                "AWS_ENDPOINT_URL_SESV2={base}   # SES v2 clients\nAWS_ENDPOINT_URL_SES={base}     # SES v1 clients\nAWS_ACCESS_KEY_ID=test AWS_SECRET_ACCESS_KEY=test AWS_REGION=us-east-1"
            ),
        ),
        snippet(
            "Amazon SES: AWS SDK for JavaScript v3",
            &format!(
                "new SESv2Client({{ endpoint: '{base}', region: 'us-east-1',\n  credentials: {{ accessKeyId: 'test', secretAccessKey: 'test' }} }})"
            ),
        ),
        snippet(
            "Amazon SES: boto3",
            &format!(
                "boto3.client('sesv2', endpoint_url='{base}', region_name='us-east-1',\n             aws_access_key_id='test', aws_secret_access_key='test')"
            ),
        ),
        snippet(
            "SendGrid: @sendgrid/mail",
            &format!(
                "sgMail.setApiKey('SG.test');\n// after setApiKey, which resets the base URL:\nsgMail.client.setDefaultRequest('baseUrl', '{base}');"
            ),
        ),
        snippet("SendGrid: Python", &format!("SendGridAPIClient('SG.test', host='{base}')")),
        snippet(
            "Postmark: postmark (Node)",
            &format!("new postmark.ServerClient('test', {{ useHttps: false, requestHost: '{host_port}' }})"),
        ),
        snippet(
            "Postmark: postmarker (Python)",
            &format!("PostmarkClient(server_token='test', root_api_url='{base}/')"),
        ),
        snippet(
            "Resend: resend (Node)",
            &format!("RESEND_BASE_URL={base}\n// or: new Resend('re_test', {{ baseUrl: '{base}' }})"),
        ),
        snippet("Resend: resend (Python)", &format!("RESEND_API_URL={base}\n# or: resend.api_url = '{base}'")),
    ]
    .concat();

    let body = format!(
        r#"<section class="card hero">
  <div class="eyebrow">Local email for development</div>
  <h1>Send to <span class="accent">{smtp}</span> or the provider APIs at <span class="accent">{base}</span></h1>
  <p class="muted">localmail keeps every message instead of delivering it. Point an SMTP client or your email provider's SDK at it, and read what was sent in the <a href="/">inbox</a> or through the dev API. Any credentials are accepted, but the APIs still expect the kind their provider uses (a bearer token, a server token, a SigV4 signature).</p>
</section>
<section class="card">
  <h2>Endpoints</h2>
  <table class="endpoints">{endpoints}</table>
</section>
<section class="card">
  <h2>Pointing an SDK at localmail</h2>
  <div class="snippets">{snippets}</div>
</section>
<section class="card">
  <h2>Scenario addresses</h2>
  <p class="muted">Send to one of these to make the send itself fail (or slow down) the way the provider would. A <code>+suffix</code> is ignored, e.g. <code>ratelimit+signup@{domain}</code>. Flaky counters can be reset with <code>POST /dev/api/scenarios/reset</code>.</p>
  <table class="endpoints">{scenarios}</table>
</section>
<section class="card">
  <h2>Test automation</h2>
  {test}
</section>"#,
        smtp = esc(&smtp),
        base = esc(base),
        domain = esc(domain),
        test = snippet(
            "Wait for an email and read its code",
            &format!(
                "cursor=$(curl -s {base}/dev/api/cursor | jq .cursor)\n# ... trigger the email ...\ncurl -s \"{base}/dev/api/messages/wait?to=alice@example.com&after=$cursor&timeout=30s&extract=otp,links\" \\\n  | jq '.extracted'"
            )
        ),
    );
    page("Setup", "setup", &body).into_response()
}
