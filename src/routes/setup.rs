//! What the web UI shows about this server, as JSON: `GET /dev/api/info` (the version, addresses
//! and providers, for every page) and `GET /dev/api/setup` (the endpoints and SDK snippets for
//! this server's addresses, for the setup page).

use axum::Json;
use axum::extract::State;
use axum::http::header;
use axum::response::{IntoResponse, Response};
use serde_json::{Value, json};

use crate::message::Provider;
use crate::state::AppState;

fn no_store() -> [(header::HeaderName, &'static str); 1] {
    [(header::CACHE_CONTROL, "no-store")]
}

pub async fn info(State(state): State<AppState>) -> Response {
    let providers: Vec<Value> =
        Provider::ALL.iter().map(|p| json!({ "id": p.id(), "name": p.name(), "color": p.color() })).collect();
    let body = json!({
        "name": "localmail",
        "version": env!("CARGO_PKG_VERSION"),
        "base_url": state.config.base_url,
        "smtp": smtp_address(&state),
        "scenario_domain": state.config.scenario_domain,
        "attempts_capacity": crate::attempts::CAPACITY,
        "providers": providers,
    });
    (no_store(), Json(body)).into_response()
}

fn smtp_address(state: &AppState) -> String {
    let host = url::Url::parse(&state.config.base_url)
        .ok()
        .and_then(|u| u.host_str().map(String::from))
        .unwrap_or_else(|| "localhost".into());
    format!("{host}:{}", state.config.smtp_port)
}

fn snippet(title: &str, code: &str) -> Value {
    json!({ "title": title, "code": code })
}

pub async fn setup(State(state): State<AppState>) -> Response {
    let base = &state.config.base_url;
    let smtp = smtp_address(&state);
    let (smtp_host, smtp_port) = smtp.rsplit_once(':').unwrap_or((&smtp, "2525"));
    let host_port = base.trim_start_matches("http://").trim_start_matches("https://");
    let domain = &state.config.scenario_domain;

    let endpoints: Vec<Value> = [
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
    .map(|(label, url)| json!({ "label": label, "url": url }))
    .collect();

    let snippets = vec![
        snippet(
            "SMTP: Nodemailer",
            &format!(
                "nodemailer.createTransport({{ host: '{smtp_host}', port: {smtp_port}, secure: false, ignoreTLS: true }})"
            ),
        ),
        snippet("SMTP: Python", &format!("import smtplib\nsmtplib.SMTP('{smtp_host}', {smtp_port}).send_message(msg)")),
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
        snippet(
            "Mailjet: mailjet_rest (Python)",
            &format!("Client(auth=('key', 'secret'), version='v3.1', api_url='{base}/')"),
        ),
        snippet("Brevo: @getbrevo/brevo", &format!("new BrevoClient({{ apiKey: 'test', baseUrl: '{base}/v3' }})")),
        snippet("Brevo: brevo-python", &format!("Brevo(api_key='test', base_url='{base}/v3')")),
        snippet("SparkPost: sparkpost (Node)", &format!("new SparkPost('test', {{ origin: '{base}' }})")),
        snippet(
            "SparkPost: python-sparkpost",
            &format!(
                "sp = SparkPost('test')\nsp.transmissions.base_uri = '{base}/api/v1'  # base_uri otherwise forces https"
            ),
        ),
        snippet(
            "Mandrill: @mailchimp/mailchimp_transactional",
            &format!(
                "const client = require('@mailchimp/mailchimp_transactional')('test');\nclient.basePath = '{base}/api/1.3';"
            ),
        ),
        snippet(
            "Mandrill: mailchimp-transactional (Python)",
            &format!("client = MailchimpTransactional.Client('test')\nclient.api_client.host = '{base}/api/1.3'"),
        ),
        snippet(
            "MailerSend: mailersend (Node)",
            &format!(
                "const ms = new MailerSend({{ apiKey: 'test' }});\nms.email.baseUrl = '{base}/v1';  // there's no option for it"
            ),
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
    ];

    let body = json!({
        "base_url": base,
        "smtp": smtp,
        "scenario_domain": domain,
        "endpoints": endpoints,
        "snippets": snippets,
        "test": snippet(
            "Wait for an email and read its code",
            &format!(
                "cursor=$(curl -s {base}/dev/api/cursor | jq .cursor)\n# ... trigger the email ...\ncurl -s \"{base}/dev/api/messages/wait?to=alice@example.com&after=$cursor&timeout=30s&extract=otp,links\" \\\n  | jq '.extracted'"
            )
        ),
    });
    (no_store(), Json(body)).into_response()
}
