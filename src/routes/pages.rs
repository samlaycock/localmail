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
  <nav class="mailbox-pane" aria-label="Mailboxes">
    <div class="mailbox-title">Mailboxes</div>
    <input id="lm-mbox-filter" type="search" placeholder="Filter addresses" aria-label="Filter mailboxes">
    <ul class="mailboxes" id="lm-mailboxes"></ul>
  </nav>
  <section class="list-pane">
    <div class="mailbox-head" id="lm-mailbox-head" hidden></div>
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
        (
            "Gmail: messages.send (raw, or uploaded with uploadType=media|multipart)",
            format!("POST {base}[/upload]/gmail/v1/users/{{userId}}/messages/send"),
        ),
        (
            "Microsoft Graph: sendMail (JSON or base64 MIME)",
            format!("POST {base}/v1.0/me/sendMail, /v1.0/users/{{id}}/sendMail"),
        ),
        ("Mailgun: messages and MIME messages", format!("POST {base}/v3/{{domain}}/messages[.mime]")),
        ("Mailjet: Send API v3.1", format!("POST {base}/v3.1/send")),
        ("Brevo: transactional email", format!("POST {base}/v3/smtp/email")),
        ("SparkPost: transmissions", format!("POST {base}/api/v1/transmissions")),
        (
            "Mandrill (Mailchimp Transactional): send, send-template, send-raw",
            format!("POST {base}/api/1.0/messages/send[-template|-raw][.json]"),
        ),
        ("MailerSend: email and bulk email", format!("POST {base}/v1/email, /v1/bulk-email")),
        ("Google OAuth: token (refresh token, code, service account JWT)", format!("POST {base}/token")),
        (
            "Microsoft identity platform: token (client credentials, refresh token, code, password)",
            format!("POST {base}/{{tenant}}/oauth2/v2.0/token"),
        ),
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
        snippet(
            "Gmail: googleapis (Node)",
            &format!(
                "const auth = new google.auth.OAuth2();\nauth.setCredentials({{ access_token: 'test' }});\nconst gmail = google.gmail({{ version: 'v1', auth, rootUrl: '{base}/' }});\n// media uploads ignore the client's rootUrl: pass it per call too\ngmail.users.messages.send({{ userId: 'me', media }}, {{ rootUrl: '{base}/' }});"
            ),
        ),
        snippet(
            "Gmail: google-api-python-client",
            &format!(
                "build('gmail', 'v1', credentials=Credentials(token='test'),\n      client_options={{'api_endpoint': '{base}'}})\n# send with body={{'raw': ...}}: media uploads always go over https"
            ),
        ),
        snippet(
            "Microsoft Graph: @microsoft/microsoft-graph-client",
            &format!(
                "Client.init({{ baseUrl: '{base}', authProvider: (done) => done(null, 'test') }})\n// the SDK drops the token over plain http; localmail lets that through"
            ),
        ),
        snippet(
            "Microsoft Graph: msgraph-sdk (Python)",
            &format!(
                "auth = AzureIdentityAuthenticationProvider(credential, allowed_hosts=['localhost'])\nadapter = GraphRequestAdapter(auth)\nadapter.base_url = '{base}/v1.0'\nclient = GraphServiceClient(request_adapter=adapter)"
            ),
        ),
        snippet(
            "Mailgun: mailgun.js",
            &format!("new Mailgun(FormData).client({{ username: 'api', key: 'test', url: '{base}' }})"),
        ),
        snippet("Mailgun: mailgun (Python)", &format!("Client(auth=('api', 'test'), api_url='{base}')")),
        snippet(
            "Mailjet: node-mailjet",
            &format!(
                "Mailjet.Request.protocol = 'http://';  // https is otherwise hard-coded\nMailjet.apiConnect('key', 'secret', {{ config: {{ host: '{host_port}' }} }})"
            ),
        ),
        snippet("Mailjet: mailjet_rest (Python)", &format!("Client(auth=('key', 'secret'), version='v3.1', api_url='{base}/')")),
        snippet("Brevo: @getbrevo/brevo", &format!("new BrevoClient({{ apiKey: 'test', baseUrl: '{base}/v3' }})")),
        snippet("Brevo: brevo-python", &format!("Brevo(api_key='test', base_url='{base}/v3')")),
        snippet("SparkPost: sparkpost (Node)", &format!("new SparkPost('test', {{ origin: '{base}' }})")),
        snippet(
            "SparkPost: python-sparkpost",
            &format!("sp = SparkPost('test')\nsp.transmissions.base_uri = '{base}/api/v1'  # base_uri otherwise forces https"),
        ),
        snippet(
            "Mandrill: @mailchimp/mailchimp_transactional",
            &format!("const client = require('@mailchimp/mailchimp_transactional')('test');\nclient.basePath = '{base}/api/1.3';"),
        ),
        snippet(
            "Mandrill: mailchimp-transactional (Python)",
            &format!("client = MailchimpTransactional.Client('test')\nclient.api_client.host = '{base}/api/1.3'"),
        ),
        snippet(
            "MailerSend: mailersend (Node)",
            &format!("const ms = new MailerSend({{ apiKey: 'test' }});\nms.email.baseUrl = '{base}/v1';  // there's no option for it"),
        ),
        snippet("MailerSend: mailersend (Python)", &format!("MailerSendClient(api_key='test', base_url='{base}/v1')")),
        snippet(
            "Google OAuth: google-auth-library (Node 9.6+)",
            &format!(
                "new OAuth2Client({{ clientId, clientSecret,\n  endpoints: {{ oauth2TokenUrl: '{base}/token' }} }})\n// service-account JWT clients can't be pointed elsewhere: use a fixed access token"
            ),
        ),
        snippet(
            "Google OAuth: google-auth (Python)",
            &format!(
                "Credentials(None, refresh_token=rt, client_id=cid, client_secret=secret,\n            token_uri='{base}/token')\n# service accounts: info['token_uri'] = '{base}/token'"
            ),
        ),
        snippet(
            "Microsoft identity: @azure/msal-node",
            &format!(
                "new ConfidentialClientApplication({{ auth: {{ clientId, clientSecret,\n  authority: 'https://login.microsoftonline.com/<tenant>',  // MSAL needs https here\n  authorityMetadata: JSON.stringify({{\n    token_endpoint: '{base}/<tenant>/oauth2/v2.0/token',\n    authorization_endpoint: '{base}/<tenant>/oauth2/v2.0/authorize',\n    issuer: 'https://login.microsoftonline.com/<tenant>/v2.0',\n    jwks_uri: 'https://login.microsoftonline.com/<tenant>/discovery/v2.0/keys' }}) }} }})"
            ),
        ),
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
