use std::time::Duration;

use axum::Router;
use axum::body::Body;
use axum::http::{HeaderMap, Method, Request, StatusCode};
use http_body_util::BodyExt;
use serde_json::{Value, json};
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tower::ServiceExt;

use localmail::config::Config;
use localmail::util::b64_std;
use localmail::{AppState, router};

fn app() -> (Router, AppState) {
    let state = AppState::new(Config::default()).unwrap();
    (router(state.clone()), state)
}

struct Res {
    status: StatusCode,
    headers: HeaderMap,
    text: String,
}

impl Res {
    fn json(&self) -> Value {
        serde_json::from_str(&self.text).unwrap_or_else(|e| panic!("not JSON ({e}): {}", self.text))
    }
    fn header(&self, name: &str) -> &str {
        self.headers.get(name).and_then(|v| v.to_str().ok()).unwrap_or_default()
    }
}

async fn call(app: &Router, method: Method, uri: &str, headers: &[(&str, &str)], body: impl Into<Body>) -> Res {
    let mut req = Request::builder().method(method).uri(uri);
    for (k, v) in headers {
        req = req.header(*k, *v);
    }
    let resp = app.clone().oneshot(req.body(body.into()).unwrap()).await.unwrap();
    let status = resp.status();
    let headers = resp.headers().clone();
    let bytes = resp.into_body().collect().await.unwrap().to_bytes();
    Res { status, headers, text: String::from_utf8_lossy(&bytes).into_owned() }
}

async fn get(app: &Router, uri: &str) -> Res {
    call(app, Method::GET, uri, &[], Body::empty()).await
}

async fn post_json(app: &Router, uri: &str, headers: &[(&str, &str)], body: Value) -> Res {
    let mut h = headers.to_vec();
    h.push(("content-type", "application/json"));
    call(app, Method::POST, uri, &h, body.to_string()).await
}

const SG: (&str, &str) = ("authorization", "Bearer SG.test");
const PM: (&str, &str) = ("x-postmark-server-token", "test");
const RS: (&str, &str) = ("authorization", "Bearer re_test");
const AWS: (&str, &str) = (
    "authorization",
    "AWS4-HMAC-SHA256 Credential=AKIDTEST/20261101/eu-west-2/ses/aws4_request, SignedHeaders=host, Signature=abc",
);

/// The newest message to `to`.
async fn latest(app: &Router, to: &str) -> Value {
    let r = get(app, &format!("/dev/api/messages/latest?to={to}")).await;
    assert_eq!(r.status, StatusCode::OK, "{}", r.text);
    r.json()
}

async fn attempts(app: &Router) -> Vec<Value> {
    get(app, "/dev/api/attempts").await.json()["attempts"].as_array().unwrap().clone()
}

// ---------------------------------------------------------------------------
// SendGrid
// ---------------------------------------------------------------------------

#[tokio::test]
async fn sendgrid_send_stores_each_personalization() {
    let (app, _) = app();
    let r = post_json(
        &app,
        "/v3/mail/send",
        &[SG],
        json!({
            "personalizations": [
                { "to": [{ "email": "alice@example.com", "name": "Alice" }], "cc": [{ "email": "carol@example.com" }],
                  "bcc": [{ "email": "audit@example.com" }], "substitutions": { "-name-": "Alice" } },
                { "to": [{ "email": "bob@example.com" }], "subject": "Just for Bob", "substitutions": { "-name-": "Bob" } }
            ],
            "from": { "email": "app@acme.test", "name": "Acme" },
            "reply_to": { "email": "help@acme.test" },
            "subject": "Hello -name-",
            "content": [{ "type": "text/plain", "value": "Hi -name-" }, { "type": "text/html", "value": "<p>Hi -name-</p>" }],
            "attachments": [{ "content": b64_std("a,b\n1,2\n"), "filename": "report.csv", "type": "text/csv" }],
            "categories": ["welcome"],
            "custom_args": { "user_id": "42" }
        }),
    )
    .await;
    assert_eq!(r.status, StatusCode::ACCEPTED, "{}", r.text);
    assert_eq!(r.text, "");
    assert_eq!(r.header("x-message-id").len(), 22);

    let alice = latest(&app, "alice@example.com").await;
    assert_eq!(alice["subject"], "Hello Alice");
    assert_eq!(alice["text"], "Hi Alice");
    assert_eq!(alice["html"], "<p>Hi Alice</p>");
    assert_eq!(alice["from"], json!({ "name": "Acme", "email": "app@acme.test" }));
    assert_eq!(alice["cc"][0]["email"], "carol@example.com");
    assert_eq!(alice["bcc"][0]["email"], "audit@example.com");
    assert_eq!(alice["reply_to"][0]["email"], "help@acme.test");
    assert_eq!(alice["tags"], json!(["welcome"]));
    assert_eq!(alice["details"]["custom_args"]["user_id"], "42");
    assert_eq!(alice["attachments"][0]["filename"], "report.csv");
    assert_eq!(alice["provider_message_id"], r.header("x-message-id"));
    assert!(alice["request"]["personalizations"].is_array());
    // Bcc recipients aren't in the headers, like a real send.
    assert!(!alice["headers"].as_array().unwrap().iter().any(|h| h["name"] == "Bcc"));
    // The Bcc recipient can be found.
    assert_eq!(latest(&app, "audit@example.com").await["id"], alice["id"]);

    let bob = latest(&app, "bob@example.com").await;
    assert_eq!(bob["subject"], "Just for Bob");
    assert_eq!(bob["text"], "Hi Bob");

    let attempt = &attempts(&app).await[0];
    assert_eq!(attempt["status"], 202);
    assert_eq!(attempt["outcome"], "accepted");
    assert_eq!(attempt["messages"].as_array().unwrap().len(), 2);
}

#[tokio::test]
async fn sendgrid_errors_look_like_sendgrids() {
    let (app, state) = app();
    let to = json!([{ "to": [{ "email": "a@example.com" }] }]);
    let content = json!([{ "type": "text/plain", "value": "x" }]);

    let r = post_json(&app, "/v3/mail/send", &[], json!({})).await;
    assert_eq!(r.status, StatusCode::UNAUTHORIZED);
    assert_eq!(r.json()["errors"][0]["message"], "authorization required");

    let r =
        post_json(&app, "/v3/mail/send", &[SG], json!({ "personalizations": to, "subject": "s", "content": content }))
            .await;
    assert_eq!(r.status, StatusCode::BAD_REQUEST);
    assert_eq!(r.json()["errors"][0]["field"], "from");
    assert!(r.json()["errors"][0]["message"].as_str().unwrap().starts_with("The from object must be provided"));

    let r = post_json(
        &app,
        "/v3/mail/send",
        &[SG],
        json!({ "personalizations": to, "from": { "email": "x@y.test" }, "content": content }),
    )
    .await;
    assert_eq!(r.json()["errors"][0]["field"], "subject");

    let r = post_json(
        &app,
        "/v3/mail/send",
        &[SG],
        json!({ "personalizations": to, "from": { "email": "x@y.test" }, "subject": "s" }),
    )
    .await;
    assert_eq!(r.json()["errors"][0]["field"], "content");

    let r = post_json(
        &app,
        "/v3/mail/send",
        &[SG],
        json!({ "personalizations": [{ "to": [{ "email": "nope" }] }], "from": { "email": "x@y.test" }, "subject": "s", "content": content }),
    )
    .await;
    assert_eq!(r.json()["errors"][0]["field"], "personalizations.0.to.0.email");

    let r = call(&app, Method::POST, "/v3/mail/send", &[SG], "{not json").await;
    assert_eq!(r.status, StatusCode::BAD_REQUEST);

    // Sandbox mode answers 200, and needs a real boolean.
    let send = |sandbox: Value| {
        json!({ "personalizations": to, "from": { "email": "x@y.test" }, "subject": "s", "content": content,
                "mail_settings": { "sandbox_mode": { "enable": sandbox } } })
    };
    assert_eq!(post_json(&app, "/v3/mail/send", &[SG], send(json!(true))).await.status, StatusCode::OK);
    let r = post_json(&app, "/v3/mail/send", &[SG], send(json!("true"))).await;
    assert_eq!(r.json()["errors"][0]["field"], "mail_settings.sandbox_mode.enable");

    assert_eq!(state.store.count(&Default::default()), 1);
    assert_eq!(attempts(&app).await.iter().filter(|a| a["outcome"] == "failed").count(), 7);
}

#[tokio::test]
async fn sendgrid_dynamic_templates_keep_their_data() {
    let (app, _) = app();
    let r = post_json(
        &app,
        "/v3/mail/send",
        &[SG],
        json!({
            "personalizations": [{ "to": [{ "email": "t@example.com" }],
                "dynamic_template_data": { "code": "918273", "link": "https://app.test/verify?t=abc" } }],
            "from": { "email": "app@acme.test" },
            "template_id": "d-1234567890abcdef"
        }),
    )
    .await;
    assert_eq!(r.status, StatusCode::ACCEPTED, "{}", r.text);
    let m = latest(&app, "t@example.com").await;
    assert_eq!(m["template"]["id"], "d-1234567890abcdef");
    assert_eq!(m["template"]["data"]["code"], "918273");
    let x = get(&app, &format!("/dev/api/messages/{}/extract", m["id"])).await.json();
    assert_eq!(x["otp"], "918273");
    assert_eq!(x["link"], "https://app.test/verify?t=abc");
}

// ---------------------------------------------------------------------------
// Postmark
// ---------------------------------------------------------------------------

#[tokio::test]
async fn postmark_single_batch_and_template_sends() {
    let (app, _) = app();
    let r = post_json(
        &app,
        "/email",
        &[PM],
        json!({ "From": "Acme <app@acme.test>", "To": "\"Doe, Jane\" <jane@example.com>, sam@example.com", "Subject": "Hi",
                "HtmlBody": "<b>Hi</b>", "TextBody": "Hi", "Tag": "welcome", "Metadata": { "a": "1" },
                "Headers": [{ "Name": "X-Campaign", "Value": "fall" }], "MessageStream": "broadcast",
                "Attachments": [{ "Name": "logo.png", "Content": b64_std([0x89, b'P', b'N', b'G']), "ContentType": "image/png", "ContentID": "cid:logo.png" }] }),
    )
    .await;
    assert_eq!(r.status, StatusCode::OK, "{}", r.text);
    let body = r.json();
    assert_eq!(body["ErrorCode"], 0);
    assert_eq!(body["Message"], "OK");
    assert_eq!(body["To"], "Doe, Jane <jane@example.com>, sam@example.com");
    assert!(body["SubmittedAt"].as_str().unwrap().ends_with("+00:00"));
    let m = latest(&app, "sam@example.com").await;
    assert_eq!(m["provider_message_id"], body["MessageID"]);
    assert_eq!(m["to"][0]["name"], "Doe, Jane");
    assert_eq!(m["tags"], json!(["welcome"]));
    assert_eq!(m["details"]["MessageStream"], "broadcast");
    assert!(m["headers"].as_array().unwrap().iter().any(|h| h["name"] == "X-Campaign" && h["value"] == "fall"));
    assert_eq!(m["attachments"][0]["content_id"], "logo.png");
    assert_eq!(m["attachments"][0]["inline"], true);

    // A batch answers 200 with a result per message; a rejected recipient fails on its own.
    let item = |to: &str| json!({ "From": "app@acme.test", "To": to, "Subject": "Batch", "TextBody": "x" });
    let r = post_json(
        &app,
        "/email/batch",
        &[PM],
        json!([
            item("one@example.com"),
            item("reject@localmail.test"),
            json!({ "From": "app@acme.test", "Subject": "x", "TextBody": "x" })
        ]),
    )
    .await;
    assert_eq!(r.status, StatusCode::OK);
    let results = r.json();
    assert_eq!(results[0]["ErrorCode"], 0);
    assert_eq!(results[1]["ErrorCode"], 406);
    assert!(results[1]["Message"].as_str().unwrap().contains("reject@localmail.test"));
    assert_eq!(results[2]["ErrorCode"], 300);
    assert_eq!(results[2]["Message"], "Zero recipients specified");
    assert_eq!(attempts(&app).await[0]["outcome"], "partial");

    let r = post_json(
        &app,
        "/email/withTemplate",
        &[PM],
        json!({ "From": "app@acme.test", "To": "tpl@example.com", "TemplateAlias": "password-reset", "TemplateModel": { "code": "55-21-90", "name": "Ann" } }),
    )
    .await;
    assert_eq!(r.status, StatusCode::OK, "{}", r.text);
    let m = latest(&app, "tpl@example.com").await;
    assert_eq!(
        m["template"],
        json!({ "field": "TemplateAlias", "id": "password-reset", "data": { "code": "55-21-90", "name": "Ann" }, "rendered": false })
    );

    let r = post_json(&app, "/email/batchWithTemplates", &[PM], json!({ "Messages": [{ "From": "app@acme.test", "To": "bt@example.com", "TemplateId": 123, "TemplateModel": {} }] })).await;
    assert_eq!(r.json()[0]["ErrorCode"], 0);
    assert_eq!(latest(&app, "bt@example.com").await["template"]["id"], "123");
}

#[tokio::test]
async fn postmark_errors_look_like_postmarks() {
    let (app, _) = app();
    let ok = json!({ "From": "app@acme.test", "To": "a@example.com", "Subject": "x", "TextBody": "x" });
    let r = post_json(&app, "/email", &[], ok.clone()).await;
    assert_eq!(r.status, StatusCode::UNAUTHORIZED);
    assert_eq!(r.json()["ErrorCode"], 10);

    let r = post_json(&app, "/email", &[PM], json!({ "From": "app@acme.test", "To": "a@example.com", "Subject": "x" }))
        .await;
    assert_eq!(r.status, StatusCode::UNPROCESSABLE_ENTITY);
    assert_eq!(r.json(), json!({ "ErrorCode": 300, "Message": "Provide either email TextBody or HtmlBody or both." }));

    let r = call(&app, Method::POST, "/email", &[PM], "[oops").await;
    assert_eq!(r.json()["ErrorCode"], 402);

    let r = post_json(
        &app,
        "/email/withTemplate",
        &[PM],
        json!({ "From": "app@acme.test", "To": "a@example.com", "TemplateModel": {} }),
    )
    .await;
    assert_eq!(r.json()["ErrorCode"], 1101);

    let mut to_rate_limit = ok.clone();
    to_rate_limit["To"] = json!("ratelimit@localmail.test");
    assert_eq!(post_json(&app, "/email", &[PM], to_rate_limit).await.status, StatusCode::TOO_MANY_REQUESTS);
}

// ---------------------------------------------------------------------------
// Resend
// ---------------------------------------------------------------------------

#[tokio::test]
async fn resend_send_and_batches() {
    let (app, state) = app();
    let r = post_json(
        &app,
        "/emails",
        &[RS],
        json!({ "from": "Acme <app@acme.test>", "to": "rita@example.com", "cc": ["c1@example.com"], "reply_to": "help@acme.test",
                "subject": "Welcome", "html": "<p>Welcome</p>", "text": "Welcome", "headers": { "X-Entity-Ref-ID": "123" },
                "tags": [{ "name": "category", "value": "welcome" }], "scheduled_at": "in 1 min",
                "attachments": [{ "filename": "a.txt", "content": b64_std("hello") }, { "filename": "b.pdf", "path": "https://cdn.test/b.pdf" }] }),
    )
    .await;
    assert_eq!(r.status, StatusCode::OK, "{}", r.text);
    let id = r.json()["id"].as_str().unwrap().to_string();
    let m = latest(&app, "rita@example.com").await;
    assert_eq!(m["provider_message_id"], id);
    assert_eq!(m["tags"], json!(["category=welcome"]));
    assert_eq!(m["details"]["scheduled_at"], "in 1 min");
    assert_eq!(m["attachments"].as_array().unwrap().len(), 2);

    // Strict batches are all or nothing.
    let good = json!({ "from": "app@acme.test", "to": ["b1@example.com"], "subject": "B", "text": "x" });
    let bad = json!({ "from": "app@acme.test", "to": ["b2@example.com"], "text": "x" });
    let before = state.store.count(&Default::default());
    let r = post_json(&app, "/emails/batch", &[RS], json!([good, bad])).await;
    assert_eq!(r.status, StatusCode::UNPROCESSABLE_ENTITY);
    assert_eq!(
        r.json(),
        json!({ "statusCode": 422, "message": "Missing `subject` field.", "name": "missing_required_field" })
    );
    assert_eq!(state.store.count(&Default::default()), before);

    // Permissive batches send what they can.
    let r = post_json(&app, "/emails/batch", &[RS, ("x-batch-validation", "permissive")], json!([good, bad])).await;
    assert_eq!(r.status, StatusCode::OK, "{}", r.text);
    let body = r.json();
    assert_eq!(body["data"].as_array().unwrap().len(), 1);
    assert_eq!(body["errors"], json!([{ "index": 1, "message": "Missing `subject` field." }]));

    let r = post_json(&app, "/emails", &[], good.clone()).await;
    assert_eq!(r.status, StatusCode::UNAUTHORIZED);
    assert_eq!(r.json()["name"], "missing_api_key");

    let r = post_json(
        &app,
        "/emails",
        &[RS],
        json!({ "from": "not an address", "to": "a@example.com", "subject": "x", "text": "x" }),
    )
    .await;
    assert_eq!(r.json()["name"], "validation_error");

    let mut limited = good.clone();
    limited["to"] = json!(["ratelimit+resend@localmail.test"]);
    let r = post_json(&app, "/emails", &[RS], limited).await;
    assert_eq!(r.status, StatusCode::TOO_MANY_REQUESTS);
    assert_eq!(r.header("retry-after"), "1");
    assert_eq!(r.json()["name"], "rate_limit_exceeded");
}

// ---------------------------------------------------------------------------
// Amazon SES
// ---------------------------------------------------------------------------

#[tokio::test]
async fn ses_v2_simple_raw_and_template() {
    let (app, _) = app();
    let r = post_json(
        &app,
        "/v2/email/outbound-emails",
        &[AWS],
        json!({ "FromEmailAddress": "app@acme.test", "Destination": { "ToAddresses": ["s1@example.com"], "BccAddresses": ["s1-bcc@example.com"] },
                "ReplyToAddresses": ["help@acme.test"], "EmailTags": [{ "Name": "flow", "Value": "signup" }], "ConfigurationSetName": "default",
                "Content": { "Simple": { "Subject": { "Data": "Simple" }, "Body": { "Text": { "Data": "Plain" }, "Html": { "Data": "<i>Rich</i>" } },
                  "Attachments": [{ "FileName": "x.txt", "RawContent": b64_std("x") }] } } }),
    )
    .await;
    assert_eq!(r.status, StatusCode::OK, "{}", r.text);
    let id = r.json()["MessageId"].as_str().unwrap().to_string();
    assert!(id.starts_with("0100") && id.ends_with("-000000"));
    let m = latest(&app, "s1@example.com").await;
    assert_eq!(m["provider_message_id"], id);
    assert_eq!(m["bcc"][0]["email"], "s1-bcc@example.com");
    assert_eq!(m["tags"], json!(["flow=signup"]));
    assert_eq!(m["details"]["region"], "eu-west-2");
    assert_eq!(m["details"]["ConfigurationSetName"], "default");

    let raw = "From: Raw Sender <raw@acme.test>\r\nTo: s2@example.com\r\nSubject: Raw one\r\n\r\nRaw body\r\n";
    let r = post_json(
        &app,
        "/v2/email/outbound-emails",
        &[AWS],
        json!({ "Destination": { "ToAddresses": ["s2@example.com", "hidden@example.com"] }, "Content": { "Raw": { "Data": b64_std(raw) } } }),
    )
    .await;
    assert_eq!(r.status, StatusCode::OK, "{}", r.text);
    let m = latest(&app, "hidden@example.com").await;
    assert_eq!(m["subject"], "Raw one");
    assert_eq!(m["from"]["email"], "raw@acme.test");
    assert_eq!(m["bcc"][0]["email"], "hidden@example.com");

    // Inline template content is rendered.
    let r = post_json(
        &app,
        "/v2/email/outbound-emails",
        &[AWS],
        json!({ "FromEmailAddress": "app@acme.test", "Destination": { "ToAddresses": ["s3@example.com"] },
                "Content": { "Template": { "TemplateContent": { "Subject": "Hi {{name}}", "Text": "Code: {{code}}" }, "TemplateData": "{\"name\":\"Ola\",\"code\":\"246810\"}" } } }),
    )
    .await;
    assert_eq!(r.status, StatusCode::OK, "{}", r.text);
    let m = latest(&app, "s3@example.com").await;
    assert_eq!(m["subject"], "Hi Ola");
    assert_eq!(m["text"], "Code: 246810");
}

#[tokio::test]
async fn ses_v2_errors_use_amzn_error_types() {
    let (app, _) = app();
    let send = |to: &str| {
        json!({ "FromEmailAddress": "app@acme.test", "Destination": { "ToAddresses": [to] },
                "Content": { "Simple": { "Subject": { "Data": "x" }, "Body": { "Text": { "Data": "x" } } } } })
    };
    let r = post_json(&app, "/v2/email/outbound-emails", &[], send("a@example.com")).await;
    assert_eq!(r.status, StatusCode::FORBIDDEN);
    assert_eq!(r.header("x-amzn-errortype"), "MissingAuthenticationTokenException");

    let r = post_json(&app, "/v2/email/outbound-emails", &[AWS], send("reject@localmail.test")).await;
    assert_eq!(r.status, StatusCode::BAD_REQUEST);
    assert_eq!(r.header("x-amzn-errortype"), "MessageRejected");
    assert!(r.json()["message"].as_str().unwrap().contains("region EU-WEST-2: reject@localmail.test"));

    let r = post_json(&app, "/v2/email/outbound-emails", &[AWS], send("ratelimit@localmail.test")).await;
    assert_eq!(r.status, StatusCode::TOO_MANY_REQUESTS);
    assert_eq!(r.header("x-amzn-errortype"), "TooManyRequestsException");

    let r = post_json(&app, "/v2/email/outbound-emails", &[AWS], send("unauthorized@localmail.test")).await;
    assert_eq!(r.status, StatusCode::FORBIDDEN);
    assert_eq!(r.header("x-amzn-errortype"), "UnrecognizedClientException");

    let r = post_json(&app, "/v2/email/outbound-emails", &[AWS], json!({ "Content": {} })).await;
    assert_eq!(r.header("x-amzn-errortype"), "BadRequestException");
}

fn form(pairs: &[(&str, &str)]) -> String {
    url::form_urlencoded::Serializer::new(String::new()).extend_pairs(pairs).finish()
}

#[tokio::test]
async fn ses_v1_query_api() {
    let (app, _) = app();
    let hdrs = [AWS, ("content-type", "application/x-www-form-urlencoded")];
    let r = call(
        &app,
        Method::POST,
        "/",
        &hdrs,
        form(&[
            ("Action", "SendEmail"),
            ("Version", "2010-12-01"),
            ("Source", "Acme <app@acme.test>"),
            ("Destination.ToAddresses.member.1", "v1a@example.com"),
            ("Destination.ToAddresses.member.2", "v1b@example.com"),
            ("Message.Subject.Data", "From v1"),
            ("Message.Body.Text.Data", "Body"),
            ("Tags.member.1.Name", "flow"),
            ("Tags.member.1.Value", "invite"),
        ]),
    )
    .await;
    assert_eq!(r.status, StatusCode::OK, "{}", r.text);
    assert!(r.header("content-type").starts_with("text/xml"));
    assert!(r.text.contains("<SendEmailResponse xmlns=\"http://ses.amazonaws.com/doc/2010-12-01/\">"));
    let m = latest(&app, "v1b@example.com").await;
    assert_eq!(m["subject"], "From v1");
    assert_eq!(m["to"].as_array().unwrap().len(), 2);
    assert_eq!(m["tags"], json!(["flow=invite"]));
    assert!(r.text.contains(&format!("<MessageId>{}</MessageId>", m["provider_message_id"].as_str().unwrap())));

    let raw = "From: app@acme.test\r\nTo: v1raw@example.com\r\nSubject: Raw v1\r\n\r\nbody\r\n";
    let r =
        call(&app, Method::POST, "/", &hdrs, form(&[("Action", "SendRawEmail"), ("RawMessage.Data", &b64_std(raw))]))
            .await;
    assert!(r.text.contains("<SendRawEmailResult>"), "{}", r.text);
    assert_eq!(latest(&app, "v1raw@example.com").await["subject"], "Raw v1");

    let r = call(
        &app,
        Method::POST,
        "/",
        &hdrs,
        form(&[
            ("Action", "SendTemplatedEmail"),
            ("Source", "app@acme.test"),
            ("Destination.ToAddresses.member.1", "v1t@example.com"),
            ("Template", "Welcome"),
            ("TemplateData", "{\"name\":\"Kim\"}"),
        ]),
    )
    .await;
    assert_eq!(r.status, StatusCode::OK, "{}", r.text);
    assert_eq!(latest(&app, "v1t@example.com").await["template"]["id"], "Welcome");

    // Throttling is a 400 in v1.
    let r = call(
        &app,
        Method::POST,
        "/",
        &hdrs,
        form(&[
            ("Action", "SendEmail"),
            ("Source", "app@acme.test"),
            ("Destination.ToAddresses.member.1", "ratelimit@localmail.test"),
            ("Message.Subject.Data", "x"),
            ("Message.Body.Text.Data", "x"),
        ]),
    )
    .await;
    assert_eq!(r.status, StatusCode::BAD_REQUEST);
    assert!(r.text.contains("<Code>Throttling</Code>") && r.text.contains("Maximum sending rate exceeded."));

    let r = call(&app, Method::POST, "/", &hdrs, form(&[("Action", "GetSendQuota")])).await;
    assert!(r.text.contains("<Code>InvalidAction</Code>"));

    let r = call(
        &app,
        Method::POST,
        "/",
        &[("content-type", "application/x-www-form-urlencoded")],
        form(&[("Action", "SendEmail")]),
    )
    .await;
    assert_eq!(r.status, StatusCode::FORBIDDEN);
    assert!(r.text.contains("<Code>MissingAuthenticationToken</Code>"));
}

// ---------------------------------------------------------------------------
// Scenarios
// ---------------------------------------------------------------------------

#[tokio::test]
async fn flaky_sends_fail_then_succeed() {
    let (app, _) = app();
    let send = json!({ "from": "app@acme.test", "to": "flaky-2@localmail.test", "subject": "Retry me", "text": "x" });
    let statuses: Vec<u16> = {
        let mut out = vec![];
        for _ in 0..4 {
            out.push(post_json(&app, "/emails", &[RS], send.clone()).await.status.as_u16());
        }
        out
    };
    assert_eq!(statuses, [500, 500, 200, 500]);
    let s = get(&app, "/dev/api/scenarios").await.json();
    assert_eq!(s["flaky_counters"][0]["failures"], 1);
    assert_eq!(call(&app, Method::POST, "/dev/api/scenarios/reset", &[], Body::empty()).await.json()["reset"], 1);
    assert_eq!(post_json(&app, "/emails", &[RS], send).await.status, StatusCode::INTERNAL_SERVER_ERROR);
}

#[tokio::test]
async fn slow_sends_succeed_late_and_typos_warn() {
    let (app, _) = app();
    let started = std::time::Instant::now();
    let r = post_json(&app, "/emails", &[RS], json!({ "from": "a@acme.test", "to": ["slow-1s@localmail.test", "rejcet@localmail.test"], "subject": "s", "text": "x" })).await;
    assert_eq!(r.status, StatusCode::OK);
    assert!(started.elapsed() >= Duration::from_secs(1));
    let attempt = &attempts(&app).await[0];
    assert_eq!(attempt["scenario"], "slow-1s@localmail.test");
    assert!(attempt["warnings"][0].as_str().unwrap().contains("rejcet@localmail.test"));
    assert_eq!(latest(&app, "slow-1s@localmail.test").await["scenario"], "slow-1s@localmail.test");
}

// ---------------------------------------------------------------------------
// Dev API
// ---------------------------------------------------------------------------

async fn resend(app: &Router, to: &str, subject: &str, html: &str) {
    let r = post_json(
        app,
        "/emails",
        &[RS],
        json!({ "from": "app@acme.test", "to": to, "subject": subject, "html": html }),
    )
    .await;
    assert_eq!(r.status, StatusCode::OK, "{}", r.text);
}

#[tokio::test]
async fn wait_returns_the_next_match_after_the_cursor() {
    let (app, _) = app();
    resend(&app, "w@example.com", "Old code", "<p>Your code is 111111</p>").await;
    let cursor = get(&app, "/dev/api/cursor").await.json()["cursor"].as_i64().unwrap();

    let sender = app.clone();
    tokio::spawn(async move {
        tokio::time::sleep(Duration::from_millis(200)).await;
        resend(&sender, "someone-else@example.com", "Not this one", "x").await;
        resend(&sender, "w@example.com", "New code", r#"<p>Your verification code is <b>222333</b>. <a href="https://app.test/v?c=222333&amp;u=w">Verify</a></p>"#).await;
    });
    let r = get(&app, &format!("/dev/api/messages/wait?to=W@example.com&after={cursor}&timeout=5s&extract=otp,links"))
        .await;
    assert_eq!(r.status, StatusCode::OK, "{}", r.text);
    let body = r.json();
    assert_eq!(body["message"]["subject"], "New code");
    assert_eq!(body["extracted"]["otp"], "222333");
    assert_eq!(body["extracted"]["link"], "https://app.test/v?c=222333&u=w");
    assert_eq!(body["extracted"]["links"][0]["text"], "Verify");

    // Without a cursor, the first match ever.
    let r = get(&app, "/dev/api/messages/wait?to=w@example.com&subject_contains=old&timeout=0").await;
    assert_eq!(r.json()["message"]["subject"], "Old code");

    let r = get(&app, &format!("/dev/api/messages/wait?to=nobody@example.com&after={cursor}&timeout=1")).await;
    assert_eq!(r.status, StatusCode::REQUEST_TIMEOUT);
    let body = r.json();
    assert_eq!(body["error"], "timeout");
    assert_eq!(body["recent"].as_array().unwrap().len(), 2);

    let r = get(&app, "/dev/api/messages/wait?subjet=x").await;
    assert_eq!(r.status, StatusCode::BAD_REQUEST);
    assert!(r.json()["error_description"].as_str().unwrap().contains("subjet"));

    let r = get(&app, "/dev/api/messages/wait?extract=otp&regex=([").await;
    assert_eq!(r.status, StatusCode::BAD_REQUEST);
}

#[tokio::test]
async fn list_filter_read_and_delete() {
    let (app, _) = app();
    resend(&app, "a@example.com", "Alpha", "x").await;
    resend(&app, "b@example.com", "Beta", "x").await;
    resend(&app, "a@example.com", "Gamma", "x").await;

    let r = get(&app, "/dev/api/messages?to=a@example.com").await.json();
    assert_eq!(r["total"], 2);
    assert_eq!(r["messages"][0]["subject"], "Gamma");
    // Unread among the matching messages.
    assert_eq!(r["unread"], 2);
    assert_eq!(get(&app, "/dev/api/messages").await.json()["unread"], 3);
    assert_eq!(get(&app, "/dev/api/messages?q=bet").await.json()["total"], 1);
    assert_eq!(get(&app, "/dev/api/messages?provider=resend&subject=alpha").await.json()["total"], 1);
    assert_eq!(get(&app, "/dev/api/messages?provider=mailchimp").await.status, StatusCode::BAD_REQUEST);

    let id = latest(&app, "b@example.com").await["id"].as_i64().unwrap();
    assert_eq!(
        call(&app, Method::POST, &format!("/dev/api/messages/{id}/read"), &[], Body::empty()).await.status,
        StatusCode::NO_CONTENT
    );
    assert_eq!(get(&app, "/dev/api/messages").await.json()["unread"], 2);

    let r = call(&app, Method::DELETE, "/dev/api/messages?to=a@example.com", &[], Body::empty()).await;
    assert_eq!(r.json()["deleted"], 2);
    assert_eq!(
        call(&app, Method::DELETE, &format!("/dev/api/messages/{id}"), &[], Body::empty()).await.status,
        StatusCode::NO_CONTENT
    );
    assert_eq!(get(&app, &format!("/dev/api/messages/{id}")).await.status, StatusCode::NOT_FOUND);
    // Deleting doesn't move the cursor back.
    assert_eq!(get(&app, "/dev/api/cursor").await.json()["cursor"], 3);
}

#[tokio::test]
async fn html_is_sandboxed_with_inline_images() {
    let (app, _) = app();
    let r = post_json(
        &app,
        "/emails",
        &[RS],
        json!({ "from": "app@acme.test", "to": "h@example.com", "subject": "Img", "html": "<img src=\"cid:logo\"><script>alert(1)</script>",
                "attachments": [{ "filename": "logo.png", "content": b64_std("png"), "content_type": "image/png", "content_id": "logo" }] }),
    )
    .await;
    assert_eq!(r.status, StatusCode::OK, "{}", r.text);
    let id = latest(&app, "h@example.com").await["id"].as_i64().unwrap();
    let r = get(&app, &format!("/dev/api/messages/{id}/html")).await;
    assert!(r.header("content-security-policy").contains("sandbox"));
    assert!(r.text.contains(&format!("src=\"/dev/api/messages/{id}/cid/logo\"")));
    let r = get(&app, &format!("/dev/api/messages/{id}/cid/logo")).await;
    assert_eq!(r.status, StatusCode::OK);
    assert_eq!(r.header("content-type"), "image/png");
    assert!(r.header("content-security-policy").starts_with("sandbox"));
    assert_eq!(r.text, "png");
    let r = get(&app, &format!("/dev/api/messages/{id}/raw")).await;
    assert!(r.text.contains("Subject: Img"));

    // A doctype stays first, so the email isn't rendered in quirks mode.
    resend(
        &app,
        "doc@example.com",
        "Doctype",
        "<!DOCTYPE html><html><head><title>t</title></head><body>hi</body></html>",
    )
    .await;
    let id = latest(&app, "doc@example.com").await["id"].as_i64().unwrap();
    let r = get(&app, &format!("/dev/api/messages/{id}/html")).await;
    assert!(r.text.starts_with("<!DOCTYPE html><html><head><base target=\"_blank\"><title>"), "{}", r.text);
}

#[tokio::test]
async fn pages_render() {
    let (app, _) = app();
    for page in ["/", "/attempts", "/setup", "/assets/app.css", "/assets/app.js", "/health"] {
        assert_eq!(get(&app, page).await.status, StatusCode::OK, "{page}");
    }
    assert!(get(&app, "/setup").await.text.contains("AWS_ENDPOINT_URL_SESV2"));
}

#[tokio::test]
async fn messages_survive_a_restart() {
    let dir = std::env::temp_dir().join(format!("localmail-test-{}", uuid::Uuid::new_v4()));
    let config = Config { data_dir: Some(dir.clone()), ..Default::default() };
    {
        let state = AppState::new(config.clone()).unwrap();
        resend(&router(state), "keep@example.com", "Kept", "x").await;
    }
    let state = AppState::new(config).unwrap();
    let app = router(state);
    assert_eq!(latest(&app, "keep@example.com").await["subject"], "Kept");
    resend(&app, "keep@example.com", "Second", "x").await;
    assert_eq!(latest(&app, "keep@example.com").await["id"], 2);
    std::fs::remove_dir_all(dir).ok();
}

// ---------------------------------------------------------------------------
// SMTP
// ---------------------------------------------------------------------------

struct Client {
    io: BufReader<tokio::net::TcpStream>,
}

impl Client {
    async fn connect(port: u16) -> Self {
        let stream = tokio::net::TcpStream::connect(("127.0.0.1", port)).await.unwrap();
        let mut c = Client { io: BufReader::new(stream) };
        assert!(c.reply().await.starts_with("220"));
        c
    }

    /// A whole (possibly multi-line) reply.
    async fn reply(&mut self) -> String {
        let mut all = String::new();
        loop {
            let mut line = String::new();
            self.io.read_line(&mut line).await.unwrap();
            all.push_str(&line);
            if line.as_bytes().get(3) != Some(&b'-') {
                return all;
            }
        }
    }

    async fn cmd(&mut self, line: &str) -> String {
        self.io.get_mut().write_all(format!("{line}\r\n").as_bytes()).await.unwrap();
        self.reply().await
    }
}

#[tokio::test]
async fn smtp_conversation() {
    let state = AppState::new(Config::default()).unwrap();
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let port = listener.local_addr().unwrap().port();
    tokio::spawn(localmail::smtp::serve(listener, state.clone()));
    let app = router(state.clone());

    let mut c = Client::connect(port).await;
    let ehlo = c.cmd("EHLO test.local").await;
    assert!(ehlo.contains("AUTH PLAIN LOGIN") && ehlo.contains("SIZE"), "{ehlo}");
    assert!(c.cmd(&format!("AUTH PLAIN {}", b64_std("\0me\0secret"))).await.starts_with("235"));
    assert!(c.cmd("RCPT TO:<x@example.com>").await.starts_with("503"));
    assert!(c.cmd("MAIL FROM:<app@acme.test> SIZE=100").await.starts_with("250"));
    assert!(c.cmd("RCPT TO:<to@example.com>").await.starts_with("250"));
    assert!(c.cmd("RCPT TO:<secret@example.com>").await.starts_with("250"));
    assert!(c.cmd("RCPT TO:<reject@localmail.test>").await.starts_with("550"));
    assert!(c.cmd("DATA").await.starts_with("354"));
    let data = "From: Acme <app@acme.test>\r\nTo: to@example.com\r\nSubject: Over SMTP\r\n\r\nLine one\r\n..starts with a dot\r\n.";
    let done = c.cmd(data).await;
    assert!(done.starts_with("250 2.0.0 OK: queued as"), "{done}");
    assert!(c.cmd("QUIT").await.starts_with("221"));

    let m = latest(&app, "secret@example.com").await;
    assert_eq!(m["subject"], "Over SMTP");
    assert_eq!(m["provider"], "smtp");
    assert_eq!(m["to"][0]["email"], "to@example.com");
    assert_eq!(m["bcc"][0]["email"], "secret@example.com");
    assert_eq!(m["text"], "Line one\r\n.starts with a dot\r\n");
    assert_eq!(m["envelope"]["auth_user"], "me");
    assert_eq!(m["envelope"]["rcpt_to"], json!(["to@example.com", "secret@example.com"]));

    // Flaky over SMTP: 451 at the end of DATA, then accepted.
    let mut c = Client::connect(port).await;
    c.cmd("HELO x").await;
    let mut codes = vec![];
    for _ in 0..2 {
        c.cmd("MAIL FROM:<a@acme.test>").await;
        c.cmd("RCPT TO:<flaky-1@localmail.test>").await;
        c.cmd("DATA").await;
        codes.push(c.cmd("Subject: flaky\r\n\r\nx\r\n.").await[..3].to_string());
    }
    assert_eq!(codes, ["451", "250"]);

    let smtp_attempts: Vec<Value> =
        get(&app, "/dev/api/attempts?provider=smtp").await.json()["attempts"].as_array().unwrap().clone();
    assert_eq!(smtp_attempts.len(), 4);
    assert_eq!(smtp_attempts[3]["status"], 550);
    assert!(smtp_attempts[0]["request"].as_str().unwrap().contains("RCPT TO:<flaky-1@localmail.test>"));
}

#[tokio::test]
async fn legacy_charsets_are_decoded() {
    let (app, _) = app();
    // 日本語 in ISO-2022-JP, in the subject (an encoded word) and the body.
    let jp = b"\x1b$BF|K\\8l\x1b(B";
    let mut raw = format!(
        "From: a@acme.test\r\nTo: jp@example.com\r\nSubject: =?ISO-2022-JP?B?{}?=\r\nContent-Type: text/plain; charset=ISO-2022-JP\r\n\r\n",
        b64_std(jp)
    )
    .into_bytes();
    raw.extend_from_slice(jp);
    let r = call(
        &app,
        Method::POST,
        "/",
        &[AWS, ("content-type", "application/x-www-form-urlencoded")],
        form(&[("Action", "SendRawEmail"), ("RawMessage.Data", &b64_std(&raw))]),
    )
    .await;
    assert_eq!(r.status, StatusCode::OK, "{}", r.text);
    let m = latest(&app, "jp@example.com").await;
    assert_eq!(m["subject"], "日本語");
    assert_eq!(m["text"], "日本語");
}

#[tokio::test]
async fn mailboxes_list_each_recipient_with_counts() {
    let (app, _) = app();
    resend(&app, "alice@example.com", "One", "x").await;
    resend(&app, "Alice@Example.com", "Two", "x").await;
    resend(&app, "bob@example.com", "Three", "x").await;
    // Alice twice on one message (To and Bcc) still counts it once.
    let r = post_json(
        &app,
        "/emails",
        &[RS],
        json!({ "from": "app@acme.test", "to": ["carol@example.com", "alice@example.com"], "bcc": "alice@example.com", "subject": "Four", "text": "x" }),
    )
    .await;
    assert_eq!(r.status, StatusCode::OK, "{}", r.text);

    let r = get(&app, "/dev/api/mailboxes").await.json();
    let boxes: Vec<(String, u64, u64)> = r["mailboxes"]
        .as_array()
        .unwrap()
        .iter()
        .map(|b| {
            (b["address"].as_str().unwrap().to_string(), b["total"].as_u64().unwrap(), b["unread"].as_u64().unwrap())
        })
        .collect();
    // Most recent mail first; addresses are matched case-insensitively.
    assert_eq!(
        boxes,
        vec![("alice@example.com".into(), 3, 3), ("carol@example.com".into(), 1, 1), ("bob@example.com".into(), 1, 1)]
    );
    assert_eq!((r["total"].as_u64(), r["messages"].as_u64(), r["unread"].as_u64()), (Some(3), Some(4), Some(4)));

    // Mark one mailbox read: only its messages change, and the list's unread count follows the filter.
    let r = call(&app, Method::POST, "/dev/api/messages/read?to=alice@example.com", &[], Body::empty()).await;
    assert_eq!(r.json()["updated"], 3);
    assert_eq!(get(&app, "/dev/api/messages?to=alice@example.com").await.json()["unread"], 0);
    assert_eq!(get(&app, "/dev/api/messages?to=bob@example.com").await.json()["unread"], 1);
    let r = get(&app, "/dev/api/mailboxes").await.json();
    assert_eq!(r["unread"], 1);
    assert_eq!(r["mailboxes"][1]["unread"], 0, "carol's message was Alice's too");

    // And back to unread.
    let r = call(
        &app,
        Method::POST,
        "/dev/api/messages/read?to=bob@example.com",
        &[("content-type", "application/json")],
        json!({ "read": false }).to_string(),
    )
    .await;
    assert_eq!(r.json()["updated"], 1);

    // Filtering and limits.
    let r = get(&app, "/dev/api/mailboxes?q=ALI").await.json();
    assert_eq!(r["mailboxes"].as_array().unwrap().len(), 1);
    let r = get(&app, "/dev/api/mailboxes?limit=1").await.json();
    assert_eq!((r["mailboxes"].as_array().unwrap().len(), r["total"].as_u64()), (1, Some(3)));
    assert_eq!(get(&app, "/dev/api/mailboxes?to=x").await.status, StatusCode::BAD_REQUEST);
    assert_eq!(get(&app, "/dev/api/mailboxes?q=100%25_").await.json()["total"], 0);
}
