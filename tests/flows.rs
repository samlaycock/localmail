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

// ---------------------------------------------------------------------------
// Gmail
// ---------------------------------------------------------------------------

const BEARER: (&str, &str) = ("authorization", "Bearer ya29.test");

fn b64url(data: &str) -> String {
    b64_std(data).replace('+', "-").replace('/', "_").trim_end_matches('=').to_string()
}

fn has_header(m: &Value, name: &str) -> bool {
    m["headers"].as_array().unwrap().iter().any(|h| h["name"].as_str().unwrap().eq_ignore_ascii_case(name))
}

#[tokio::test]
async fn gmail_sends_raw_mime_and_uploads() {
    let (app, _) = app();
    let mime =
        "To: Alice <alice@example.com>\r\nBcc: audit@example.com\r\nSubject: Your code\r\n\r\nYour code is 482913\r\n";
    let r = post_json(
        &app,
        "/gmail/v1/users/me@acme.test/messages/send",
        &[BEARER],
        json!({ "raw": b64url(mime), "threadId": "t1" }),
    )
    .await;
    assert_eq!(r.status, StatusCode::OK, "{}", r.text);
    let body = r.json();
    assert_eq!(body["labelIds"], json!(["SENT"]));
    assert_eq!(body["threadId"], "t1");
    assert_eq!(body["id"].as_str().unwrap().len(), 16);
    let m = latest(&app, "alice@example.com").await;
    assert_eq!(m["provider"], "gmail");
    assert_eq!(m["provider_message_id"], body["id"]);
    assert_eq!(m["subject"], "Your code");
    // The userId is the sender when the message has no From; Bcc is delivered but not shown.
    assert_eq!(m["from"]["email"], "me@acme.test");
    assert_eq!(m["bcc"][0]["email"], "audit@example.com");
    assert!(!has_header(&m, "bcc"));
    assert_eq!(m["details"]["threadId"], "t1");
    assert_eq!(latest(&app, "audit@example.com").await["id"], m["id"]);

    // uploadType=media: the bare message.
    let r = call(
        &app,
        Method::POST,
        "/upload/gmail/v1/users/me/messages/send?uploadType=media",
        &[BEARER, ("content-type", "message/rfc822")],
        "From: app@acme.test\r\nTo: media@example.com\r\nSubject: Media\r\n\r\nhi\r\n",
    )
    .await;
    assert_eq!(r.status, StatusCode::OK, "{}", r.text);
    assert_eq!(latest(&app, "media@example.com").await["subject"], "Media");

    // uploadType=multipart: JSON metadata, then the message (what googleapis sends with `media`).
    let body = "--b1\r\nContent-Type: application/json; charset=UTF-8\r\n\r\n{\"threadId\":\"t9\"}\r\n--b1\r\nContent-Type: message/rfc822\r\n\r\nFrom: app@acme.test\r\nTo: multi@example.com\r\nSubject: Multi\r\n\r\nhi\r\n--b1--\r\n";
    let r = call(
        &app,
        Method::POST,
        "/upload/gmail/v1/users/me/messages/send?uploadType=multipart",
        &[BEARER, ("content-type", "multipart/related; boundary=b1")],
        body,
    )
    .await;
    assert_eq!(r.status, StatusCode::OK, "{}", r.text);
    assert_eq!(r.json()["threadId"], "t9");
    assert_eq!(latest(&app, "multi@example.com").await["text"], "hi");
}

#[tokio::test]
async fn gmail_user_id_is_the_sender() {
    let (app, _) = app();
    // Clients percent-encode the address in the path.
    let r = post_json(
        &app,
        "/gmail/v1/users/ann%40acme.test/messages/send",
        &[BEARER],
        json!({ "raw": b64url("To: bob@example.com\r\nSubject: Hi\r\n\r\nhi") }),
    )
    .await;
    assert_eq!(r.status, StatusCode::OK, "{}", r.text);
    let m = latest(&app, "bob@example.com").await;
    assert_eq!(m["from"]["email"], "ann@acme.test");
    assert!(has_header(&m, "from"));
    assert_eq!(m["endpoint"], "POST /gmail/v1/users/ann@acme.test/messages/send");

    // A From in the message is kept.
    let raw = b64url("From: Ann <ann@acme.test>\r\nTo: carol@example.com\r\nSubject: Hi\r\n\r\nhi");
    post_json(&app, "/gmail/v1/users/ann@acme.test/messages/send", &[BEARER], json!({ "raw": raw })).await;
    assert_eq!(latest(&app, "carol@example.com").await["from"]["name"], "Ann");
}

#[tokio::test]
async fn gmail_errors_look_like_googles() {
    let (app, _) = app();
    let path = "/gmail/v1/users/me/messages/send";
    let raw = b64url("From: a@acme.test\r\nTo: b@example.com\r\nSubject: x\r\n\r\nx");
    let r = post_json(&app, path, &[], json!({ "raw": raw })).await;
    assert_eq!(r.status, StatusCode::UNAUTHORIZED);
    assert_eq!(r.json()["error"]["status"], "UNAUTHENTICATED");
    assert_eq!(r.json()["error"]["errors"][0]["reason"], "required");

    let r = post_json(&app, path, &[BEARER], json!({})).await;
    assert_eq!(r.status, StatusCode::BAD_REQUEST);
    assert_eq!(r.json()["error"]["status"], "INVALID_ARGUMENT");
    assert!(r.json()["error"]["message"].as_str().unwrap().starts_with("'raw' RFC822 payload"));

    let r =
        post_json(&app, path, &[BEARER], json!({ "raw": b64url("From: a@acme.test\r\nSubject: x\r\n\r\nx") })).await;
    assert_eq!(r.json()["error"]["message"], "Recipient address required");

    let r = post_json(
        &app,
        path,
        &[BEARER],
        json!({ "raw": b64url("To: ratelimit@localmail.test\r\nSubject: x\r\n\r\nx") }),
    )
    .await;
    assert_eq!(r.status, StatusCode::TOO_MANY_REQUESTS);
    assert_eq!(r.json()["error"]["errors"][0]["reason"], "rateLimitExceeded");

    let r = call(
        &app,
        Method::POST,
        "/upload/gmail/v1/users/me/messages/send?uploadType=media",
        &[BEARER, ("content-type", "application/json")],
        "{}",
    )
    .await;
    assert_eq!(r.status, StatusCode::BAD_REQUEST);
    assert!(r.json()["error"]["message"].as_str().unwrap().contains("message/*"));
}

// ---------------------------------------------------------------------------
// Microsoft Graph
// ---------------------------------------------------------------------------

#[tokio::test]
async fn graph_send_mail_json_and_mime() {
    let (app, _) = app();
    let r = post_json(
        &app,
        "/v1.0/users/sender@acme.test/sendMail",
        &[BEARER, ("client-request-id", "abc-123")],
        json!({
            "message": {
                "subject": "Quarterly report",
                "body": { "contentType": "HTML", "content": "<p>Attached.</p>" },
                "toRecipients": [{ "emailAddress": { "address": "alice@example.com", "name": "Alice" } }],
                "ccRecipients": [{ "emailAddress": { "address": "carol@example.com" } }],
                "bccRecipients": [{ "emailAddress": { "address": "audit@example.com" } }],
                "replyTo": [{ "emailAddress": { "address": "help@acme.test" } }],
                "internetMessageHeaders": [{ "name": "x-campaign", "value": "q3" }],
                "categories": ["reports"],
                "attachments": [{ "@odata.type": "#microsoft.graph.fileAttachment", "name": "q3.csv", "contentType": "text/csv", "contentBytes": b64_std("a,b\n") }]
            },
            "saveToSentItems": false
        }),
    )
    .await;
    assert_eq!(r.status, StatusCode::ACCEPTED, "{}", r.text);
    assert_eq!(r.text, "");
    assert_eq!(r.header("client-request-id"), "abc-123");
    let m = latest(&app, "alice@example.com").await;
    assert_eq!(m["provider"], "graph");
    assert_eq!(m["from"]["email"], "sender@acme.test");
    assert_eq!(m["html"], "<p>Attached.</p>");
    assert_eq!(m["cc"][0]["email"], "carol@example.com");
    assert_eq!(m["bcc"][0]["email"], "audit@example.com");
    assert_eq!(m["reply_to"][0]["email"], "help@acme.test");
    assert_eq!(m["tags"], json!(["reports"]));
    assert_eq!(m["details"]["saveToSentItems"], false);
    assert_eq!(m["attachments"][0]["filename"], "q3.csv");
    assert!(has_header(&m, "x-campaign"));

    // MIME, base64-encoded as text/plain.
    let mime = "From: me@acme.test\r\nTo: mime@example.com\r\nBcc: hidden@example.com\r\nSubject: Raw\r\n\r\nhello\r\n";
    let r =
        call(&app, Method::POST, "/v1.0/me/sendMail", &[BEARER, ("content-type", "text/plain")], b64_std(mime)).await;
    assert_eq!(r.status, StatusCode::ACCEPTED, "{}", r.text);
    let m = latest(&app, "mime@example.com").await;
    assert_eq!(m["subject"], "Raw");
    assert_eq!(m["bcc"][0]["email"], "hidden@example.com");
    assert!(!has_header(&m, "bcc"));
}

#[tokio::test]
async fn graph_errors_look_like_graphs() {
    let (app, _) = app();
    let msg = |to: &str| json!({ "message": { "subject": "x", "body": { "contentType": "Text", "content": "x" }, "toRecipients": [{ "emailAddress": { "address": to } }] } });
    // No Authorization header at all gets through (the Graph JS SDK drops it over plain http),
    // with a warning in the attempts log; one that isn't a bearer token doesn't.
    let r = post_json(&app, "/v1.0/me/sendMail", &[], msg("a@example.com")).await;
    assert_eq!(r.status, StatusCode::ACCEPTED, "{}", r.text);
    assert!(attempts(&app).await[0]["warnings"][0].as_str().unwrap().contains("no access token"));
    let r = post_json(&app, "/v1.0/me/sendMail", &[("authorization", "Basic dGVzdA==")], msg("a@example.com")).await;
    assert_eq!(r.status, StatusCode::UNAUTHORIZED);
    assert_eq!(r.json()["error"]["code"], "InvalidAuthenticationToken");
    assert!(r.json()["error"]["innerError"]["request-id"].is_string());
    assert!(r.header("www-authenticate").starts_with("Bearer realm"));

    // The Python SDK's PascalCase names work too.
    let r = post_json(
        &app,
        "/v1.0/me/sendMail",
        &[BEARER],
        json!({ "Message": { "@odata.type": "#microsoft.graph.message", "Subject": "Pascal", "Body": { "ContentType": "text", "Content": "x" },
                "ToRecipients": [{ "EmailAddress": { "Address": "pascal@example.com" } }] }, "SaveToSentItems": false }),
    )
    .await;
    assert_eq!(r.status, StatusCode::ACCEPTED, "{}", r.text);
    assert_eq!(latest(&app, "pascal@example.com").await["subject"], "Pascal");

    let r = post_json(&app, "/v1.0/me/sendMail", &[BEARER], json!({ "message": { "subject": "x" } })).await;
    assert_eq!(r.status, StatusCode::BAD_REQUEST);
    assert_eq!(r.json()["error"]["code"], "ErrorInvalidRecipients");

    let mut bad_header = msg("a@example.com");
    bad_header["message"]["internetMessageHeaders"] = json!([{ "name": "Campaign", "value": "x" }]);
    let r = post_json(&app, "/v1.0/me/sendMail", &[BEARER], bad_header).await;
    assert_eq!(r.json()["error"]["code"], "InvalidInternetMessageHeader");

    let r = post_json(&app, "/v1.0/me/sendMail", &[BEARER], msg("ratelimit@localmail.test")).await;
    assert_eq!(r.status, StatusCode::TOO_MANY_REQUESTS);
    assert_eq!(r.header("retry-after"), "10");
    assert_eq!(r.json()["error"]["code"], "TooManyRequests");
}

// ---------------------------------------------------------------------------
// Mailgun
// ---------------------------------------------------------------------------

const MG: (&str, &str) = ("authorization", "Basic YXBpOmtleS10ZXN0"); // api:key-test

/// A multipart/form-data body: text fields, then files as (field, filename, type, content).
fn multipart(fields: &[(&str, &str)], files: &[(&str, &str, &str, &[u8])]) -> (String, Vec<u8>) {
    let boundary = "lmboundary";
    let mut body = vec![];
    for (k, v) in fields {
        body.extend(format!("--{boundary}\r\nContent-Disposition: form-data; name=\"{k}\"\r\n\r\n{v}\r\n").as_bytes());
    }
    for (k, filename, ct, content) in files {
        body.extend(
            format!("--{boundary}\r\nContent-Disposition: form-data; name=\"{k}\"; filename=\"{filename}\"\r\nContent-Type: {ct}\r\n\r\n").as_bytes(),
        );
        body.extend(*content);
        body.extend(b"\r\n");
    }
    body.extend(format!("--{boundary}--\r\n").as_bytes());
    (format!("multipart/form-data; boundary={boundary}"), body)
}

#[tokio::test]
async fn mailgun_messages_batches_and_mime() {
    let (app, _) = app();
    let (ct, body) = multipart(
        &[
            ("from", "Acme <app@mg.acme.test>"),
            ("to", "alice@example.com"),
            ("to", "bob@example.com"),
            ("subject", "Hi %recipient.name%"),
            ("text", "Your code is %recipient.code%"),
            (
                "recipient-variables",
                r#"{"alice@example.com":{"name":"Alice","code":"111111"},"bob@example.com":{"name":"Bob","code":"222222"}}"#,
            ),
            ("o:tag", "welcome"),
            ("o:testmode", "yes"),
            ("h:Reply-To", "help@acme.test"),
            ("h:X-Campaign", "fall"),
            ("v:user_id", "42"),
        ],
        &[("attachment", "report.csv", "text/csv", b"a,b\n")],
    );
    let r = call(&app, Method::POST, "/v3/mg.acme.test/messages", &[MG, ("content-type", &ct)], body).await;
    assert_eq!(r.status, StatusCode::OK, "{}", r.text);
    let body = r.json();
    assert_eq!(body["message"], "Queued. Thank you.");
    let id = body["id"].as_str().unwrap();
    assert!(id.starts_with('<') && id.ends_with("@mg.acme.test>"), "{id}");

    let alice = latest(&app, "alice@example.com").await;
    assert_eq!(alice["subject"], "Hi Alice");
    assert_eq!(alice["text"], "Your code is 111111");
    assert_eq!(alice["to"].as_array().unwrap().len(), 1);
    assert_eq!(alice["provider_message_id"], id);
    assert_eq!(alice["tags"], json!(["welcome"]));
    assert_eq!(alice["reply_to"][0]["email"], "help@acme.test");
    assert_eq!(alice["details"]["options"]["testmode"], "yes");
    assert_eq!(alice["details"]["variables"]["user_id"], "42");
    assert_eq!(alice["attachments"][0]["filename"], "report.csv");
    assert!(has_header(&alice, "x-campaign"));
    assert_eq!(latest(&app, "bob@example.com").await["subject"], "Hi Bob");

    // URL-encoded forms work too.
    let r = call(
        &app,
        Method::POST,
        "/v3/mg.acme.test/messages",
        &[MG, ("content-type", "application/x-www-form-urlencoded")],
        form(&[
            ("from", "app@mg.acme.test"),
            ("to", "carol@example.com, dan@example.com"),
            ("subject", "Plain"),
            ("html", "<p>x</p>"),
        ]),
    )
    .await;
    assert_eq!(r.status, StatusCode::OK, "{}", r.text);
    assert_eq!(latest(&app, "dan@example.com").await["to"].as_array().unwrap().len(), 2);

    let (ct, body) = multipart(
        &[("to", "mime@example.com")],
        &[(
            "message",
            "message.mime",
            "message/rfc822",
            b"From: app@mg.acme.test\r\nTo: mime@example.com\r\nSubject: MIME\r\n\r\nraw\r\n",
        )],
    );
    let r = call(&app, Method::POST, "/v3/mg.acme.test/messages.mime", &[MG, ("content-type", &ct)], body).await;
    assert_eq!(r.status, StatusCode::OK, "{}", r.text);
    assert_eq!(latest(&app, "mime@example.com").await["subject"], "MIME");
}

#[tokio::test]
async fn mailgun_errors_look_like_mailguns() {
    let (app, _) = app();
    let urlencoded = ("content-type", "application/x-www-form-urlencoded");
    let ok = form(&[("from", "app@mg.acme.test"), ("to", "a@example.com"), ("subject", "x"), ("text", "x")]);
    let r = call(&app, Method::POST, "/v3/mg.acme.test/messages", &[urlencoded], ok).await;
    assert_eq!(r.status, StatusCode::UNAUTHORIZED);
    assert_eq!(r.text, "Forbidden");

    let r = call(&app, Method::POST, "/v3/mg.acme.test/messages", &[MG, urlencoded], form(&[("to", "a@example.com")]))
        .await;
    assert_eq!(r.status, StatusCode::BAD_REQUEST);
    assert_eq!(r.json(), json!({ "message": "from parameter is missing" }));

    let r = call(
        &app,
        Method::POST,
        "/v3/mg.acme.test/messages",
        &[MG, urlencoded],
        form(&[("from", "app@mg.acme.test"), ("to", "a@example.com"), ("subject", "x")]),
    )
    .await;
    assert_eq!(
        r.json()["message"],
        "Need at least one of 'text', 'html', 'amp-html' or 'template' parameters specified"
    );
}

// ---------------------------------------------------------------------------
// Mailjet
// ---------------------------------------------------------------------------

const MJ: (&str, &str) = ("authorization", "Basic a2V5OnNlY3JldA=="); // key:secret

#[tokio::test]
async fn mailjet_send_v31() {
    let (app, _) = app();
    let r = post_json(
        &app,
        "/v3.1/send",
        &[MJ],
        json!({
            "Globals": { "From": { "Email": "app@acme.test", "Name": "Acme" }, "Subject": "Hello {{var:name:\"there\"}}" },
            "Messages": [
                { "To": [{ "Email": "alice@example.com", "Name": "Alice" }], "Cc": [{ "Email": "carol@example.com" }],
                  "TextPart": "Code {{var:code}}", "TemplateLanguage": true, "Variables": { "name": "Alice", "code": "123456" },
                  "CustomID": "signup-1", "Headers": { "X-Campaign": "fall" },
                  "Attachments": [{ "ContentType": "text/plain", "Filename": "a.txt", "Base64Content": b64_std("hi") }] },
                { "To": [{ "Email": "bob@example.com" }], "HTMLPart": "<p>Hi</p>", "TemplateLanguage": true }
            ]
        }),
    )
    .await;
    assert_eq!(r.status, StatusCode::OK, "{}", r.text);
    let body = r.json();
    assert_eq!(body["Messages"][0]["Status"], "success");
    assert_eq!(body["Messages"][0]["CustomID"], "signup-1");
    assert_eq!(body["Messages"][0]["To"][0]["Email"], "alice@example.com");
    assert!(body["Messages"][0]["To"][0]["MessageID"].is_u64());
    assert_eq!(body["Messages"][0]["Cc"][0]["Email"], "carol@example.com");
    let alice = latest(&app, "alice@example.com").await;
    assert_eq!(alice["provider_message_id"], body["Messages"][0]["To"][0]["MessageUUID"]);
    assert_eq!(alice["subject"], "Hello Alice");
    assert_eq!(alice["text"], "Code 123456");
    assert_eq!(alice["from"]["name"], "Acme");
    assert_eq!(alice["details"]["CustomID"], "signup-1");
    assert_eq!(alice["attachments"][0]["filename"], "a.txt");
    assert_eq!(latest(&app, "bob@example.com").await["subject"], "Hello there");

    // An invalid message fails on its own: the valid one is still sent, and the answer is a 400.
    let r = post_json(
        &app,
        "/v3.1/send",
        &[MJ],
        json!({ "Messages": [
            { "From": { "Email": "app@acme.test" }, "To": [{ "Email": "ok@example.com" }], "TextPart": "x" },
            { "From": { "Email": "app@acme.test" }, "To": [{ "Email": "not-an-address" }], "TextPart": "x" }
        ] }),
    )
    .await;
    assert_eq!(r.status, StatusCode::BAD_REQUEST);
    let body = r.json();
    assert_eq!(body["Messages"][1]["Status"], "error");
    assert_eq!(body["Messages"][1]["Errors"][0]["ErrorCode"], "mj-0013");
    assert_eq!(body["Messages"][1]["Errors"][0]["ErrorRelatedTo"], json!(["To[0].Email"]));
    assert_eq!(body["Messages"][0]["Status"], "success");
    assert_eq!(get(&app, "/dev/api/messages?to=ok@example.com").await.json()["total"], 1);
    assert!(
        body["Messages"][0]["To"][0]["MessageHref"]
            .as_str()
            .unwrap()
            .starts_with("https://api.mailjet.com/v3/message/")
    );

    let r = post_json(
        &app,
        "/v3.1/send",
        &[MJ],
        json!({ "Messages": [{ "From": { "Email": "app@acme.test" }, "To": [{ "Email": "a@example.com" }] }] }),
    )
    .await;
    assert_eq!(r.json()["Messages"][0]["Errors"][0]["ErrorCode"], "send-0003");

    let r = post_json(&app, "/v3.1/send", &[], json!({ "Messages": [] })).await;
    assert_eq!(r.status, StatusCode::UNAUTHORIZED);
    assert_eq!(r.text, "Not authorized");
    let r = call(&app, Method::POST, "/v3.1/send", &[MJ], "{oops").await;
    assert_eq!(r.json()["ErrorCode"], "mj-0002");
}

// ---------------------------------------------------------------------------
// Brevo
// ---------------------------------------------------------------------------

const BV: (&str, &str) = ("api-key", "xkeysib-test");

#[tokio::test]
async fn brevo_send_and_message_versions() {
    let (app, _) = app();
    let r = post_json(
        &app,
        "/v3/smtp/email",
        &[BV],
        json!({
            "sender": { "name": "Acme", "email": "app@acme.test" },
            "to": [{ "email": "alice@example.com", "name": "Alice" }],
            "replyTo": { "email": "help@acme.test" },
            "subject": "Hi {{ params.name }}",
            "htmlContent": "<p>Your code is {{ params.code }}</p>",
            "params": { "name": "Alice", "code": "424242" },
            "tags": ["otp"],
            "headers": { "X-Campaign": "fall" },
            "attachment": [{ "name": "a.txt", "content": b64_std("hi") }]
        }),
    )
    .await;
    assert_eq!(r.status, StatusCode::CREATED, "{}", r.text);
    let id = r.json()["messageId"].as_str().unwrap().to_string();
    assert!(id.ends_with("@smtp-relay.mailin.fr>"));
    let m = latest(&app, "alice@example.com").await;
    assert_eq!(m["provider_message_id"], id);
    assert_eq!(m["subject"], "Hi Alice");
    assert_eq!(m["html"], "<p>Your code is 424242</p>");
    assert_eq!(m["reply_to"][0]["email"], "help@acme.test");
    assert_eq!(m["tags"], json!(["otp"]));
    assert_eq!(m["attachments"][0]["filename"], "a.txt");

    let r = post_json(
        &app,
        "/v3/smtp/email",
        &[BV],
        json!({
            "sender": { "email": "app@acme.test" },
            "subject": "Hello {{ params.name }}",
            "textContent": "Hi {{ params.name }}",
            "messageVersions": [
                { "to": [{ "email": "v1@example.com" }], "params": { "name": "One" } },
                { "to": [{ "email": "v2@example.com" }], "params": { "name": "Two" }, "subject": "Custom for {{ params.name }}" }
            ]
        }),
    )
    .await;
    assert_eq!(r.status, StatusCode::CREATED, "{}", r.text);
    assert_eq!(r.json()["messageIds"].as_array().unwrap().len(), 2);
    assert_eq!(latest(&app, "v1@example.com").await["subject"], "Hello One");
    assert_eq!(latest(&app, "v2@example.com").await["subject"], "Custom for Two");
}

#[tokio::test]
async fn brevo_errors_look_like_brevos() {
    let (app, _) = app();
    let ok = json!({ "sender": { "email": "app@acme.test" }, "to": [{ "email": "a@example.com" }], "subject": "x", "textContent": "x" });
    let r = post_json(&app, "/v3/smtp/email", &[], ok.clone()).await;
    assert_eq!(r.status, StatusCode::UNAUTHORIZED);
    assert_eq!(r.json()["code"], "unauthorized");

    let mut no_sender = ok.clone();
    no_sender.as_object_mut().unwrap().remove("sender");
    let r = post_json(&app, "/v3/smtp/email", &[BV], no_sender).await;
    assert_eq!(r.status, StatusCode::BAD_REQUEST);
    assert_eq!(r.json()["code"], "missing_parameter");

    let mut bad_to = ok.clone();
    bad_to["to"] = json!([{ "email": "nope" }]);
    assert_eq!(post_json(&app, "/v3/smtp/email", &[BV], bad_to).await.json()["code"], "invalid_parameter");
}

// ---------------------------------------------------------------------------
// SparkPost
// ---------------------------------------------------------------------------

const SP: (&str, &str) = ("authorization", "sp-test-key");

#[tokio::test]
async fn sparkpost_transmissions() {
    let (app, _) = app();
    let r = post_json(
        &app,
        "/api/v1/transmissions",
        &[SP],
        json!({
            "options": { "sandbox": true },
            "campaign_id": "signup",
            "substitution_data": { "product": "Acme" },
            "recipients": [
                { "address": { "email": "alice@example.com", "name": "Alice" }, "substitution_data": { "code": "777777" }, "tags": ["new"] },
                { "address": "bob@example.com", "substitution_data": { "code": "888888" } },
                { "address": { "email": "not-an-address" } }
            ],
            "content": {
                "from": { "email": "app@acme.test", "name": "Acme" },
                "subject": "{{product}} code",
                "text": "Your code is {{code}}",
                "inline_images": [{ "name": "logo", "type": "image/png", "data": b64_std([0x89, b'P', b'N', b'G']) }]
            }
        }),
    )
    .await;
    // The invalid recipient makes it a 422, but the transmission was still created.
    assert_eq!(r.status, StatusCode::UNPROCESSABLE_ENTITY, "{}", r.text);
    assert_eq!(r.json()["errors"][0]["code"], "2000");
    let results = &r.json()["results"];
    assert_eq!(results["total_accepted_recipients"], 2);
    assert_eq!(results["total_rejected_recipients"], 1);
    assert_eq!(results["rcpt_to_errors"][0]["description"], "Invalid email address: not-an-address");
    let alice = latest(&app, "alice@example.com").await;
    assert_eq!(alice["provider_message_id"], results["id"]);
    assert_eq!(alice["subject"], "Acme code");
    assert_eq!(alice["text"], "Your code is 777777");
    assert_eq!(alice["tags"], json!(["new"]));
    assert_eq!(alice["details"]["campaign_id"], "signup");
    assert_eq!(alice["attachments"][0]["content_id"], "logo");
    let bob = latest(&app, "bob@example.com").await;
    assert_eq!(bob["text"], "Your code is 888888");
    assert_eq!(bob["provider_message_id"], results["id"]);

    let r = post_json(&app, "/api/v1/transmissions", &[], json!({})).await;
    assert_eq!(r.status, StatusCode::UNAUTHORIZED);
    let r = post_json(
        &app,
        "/api/v1/transmissions",
        &[SP],
        json!({ "content": { "from": "a@acme.test", "subject": "x", "text": "x" } }),
    )
    .await;
    assert_eq!(r.status, StatusCode::UNPROCESSABLE_ENTITY);
    assert_eq!(r.json()["errors"][0]["code"], "1400");
}

// ---------------------------------------------------------------------------
// Mandrill
// ---------------------------------------------------------------------------

#[tokio::test]
async fn mandrill_send_template_and_raw() {
    let (app, _) = app();
    let r = post_json(
        &app,
        "/api/1.0/messages/send.json",
        &[],
        json!({
            "key": "md-test",
            "message": {
                "from_email": "app@acme.test", "from_name": "Acme",
                "subject": "Hi *|NAME|*", "text": "Code *|code|*",
                "to": [{ "email": "alice@example.com", "name": "Alice" }, { "email": "bob@example.com", "type": "cc" }, { "email": "broken" }],
                "global_merge_vars": [{ "name": "name", "content": "friend" }],
                "merge_vars": [{ "rcpt": "alice@example.com", "vars": [{ "name": "NAME", "content": "Alice" }, { "name": "code", "content": "121212" }] }],
                "headers": { "Reply-To": "help@acme.test" },
                "tags": ["otp"],
                "metadata": { "user_id": "42" }
            }
        }),
    )
    .await;
    assert_eq!(r.status, StatusCode::OK, "{}", r.text);
    let entries = r.json();
    assert_eq!(entries.as_array().unwrap().len(), 3);
    assert_eq!(entries[0]["status"], "sent");
    assert_eq!(entries[0]["_id"].as_str().unwrap().len(), 32);
    assert_eq!(
        entries[2],
        json!({ "email": "broken", "status": "invalid", "reject_reason": null, "queued_reason": null, "_id": entries[2]["_id"] })
    );
    // Without preserve_recipients, each recipient has a message of their own.
    let alice = latest(&app, "alice@example.com").await;
    assert_eq!(alice["subject"], "Hi Alice");
    assert_eq!(alice["text"], "Code 121212");
    assert_eq!(alice["cc"], json!([]));
    assert_eq!(alice["reply_to"][0]["email"], "help@acme.test");
    assert_eq!(alice["details"]["metadata"]["user_id"], "42");
    let bob = latest(&app, "bob@example.com").await;
    assert_eq!(bob["subject"], "Hi friend");
    assert_eq!(bob["to"], json!([]));

    let r = post_json(
        &app,
        "/api/1.0/messages/send-template",
        &[],
        json!({ "key": "md-test", "template_name": "welcome", "template_content": [], "message": { "to": [{ "email": "tpl@example.com" }], "global_merge_vars": [{ "name": "code", "content": "999" }] } }),
    )
    .await;
    assert_eq!(r.status, StatusCode::OK, "{}", r.text);
    let m = latest(&app, "tpl@example.com").await;
    assert_eq!(m["template"]["id"], "welcome");
    assert_eq!(m["template"]["data"]["merge_vars"]["code"], "999");

    let r = post_json(
        &app,
        "/api/1.0/messages/send-raw",
        &[],
        json!({ "key": "md-test", "raw_message": "From: app@acme.test\r\nTo: raw@example.com\r\nSubject: Raw\r\n\r\nhi\r\n", "async": true }),
    )
    .await;
    assert_eq!(r.json()[0]["status"], "queued");
    assert_eq!(latest(&app, "raw@example.com").await["subject"], "Raw");

    // Required parameters are checked before the key; both are 400 ValidationErrors.
    let r = post_json(&app, "/api/1.3/messages/send", &[], json!({})).await;
    assert_eq!(r.status, StatusCode::BAD_REQUEST);
    assert_eq!(
        r.json(),
        json!({ "status": "error", "code": -1, "name": "ValidationError", "message": "You must specify a message value" })
    );
    let r = post_json(&app, "/api/1.3/messages/send", &[], json!({ "message": {} })).await;
    assert_eq!(r.json()["message"], "You must specify a key value");
    let r = post_json(
        &app,
        "/api/1.3/messages/send",
        &[],
        json!({ "key": "k", "message": { "from_email": "a@acme.test" } }),
    )
    .await;
    assert_eq!(r.status, StatusCode::BAD_REQUEST);
    assert_eq!(r.json()["code"], -2);
    let r = post_json(
        &app,
        "/api/1.3/messages/send-template",
        &[],
        json!({ "key": "k", "template_name": "t", "message": {} }),
    )
    .await;
    assert_eq!(r.json()["message"], "You must specify a template_content value");

    // A rejected recipient is a per-recipient status, not an error.
    let r = post_json(
        &app,
        "/api/1.0/messages/send",
        &[],
        json!({ "key": "k", "message": { "from_email": "a@acme.test", "text": "x", "to": [{ "email": "reject@localmail.test" }, { "email": "fine@example.com" }] } }),
    )
    .await;
    assert_eq!(r.status, StatusCode::OK);
    assert_eq!(r.json()[0]["status"], "rejected");
    assert_eq!(r.json()[1]["status"], "sent");
}

// ---------------------------------------------------------------------------
// MailerSend
// ---------------------------------------------------------------------------

#[tokio::test]
async fn mailersend_email_and_bulk() {
    let (app, _) = app();
    let r = post_json(
        &app,
        "/v1/email",
        &[BEARER],
        json!({
            "from": { "email": "app@acme.test", "name": "Acme" },
            "to": [{ "email": "alice@example.com", "name": "Alice" }, { "email": "bob@example.com" }],
            "subject": "Hi {{ name }}",
            "html": "<p>Code {{ code }}</p>",
            "personalization": [
                { "email": "alice@example.com", "data": { "name": "Alice", "code": "135790" } },
                { "email": "bob@example.com", "data": { "name": "Bob", "code": "246802" } }
            ],
            "tags": ["otp"],
            "headers": [{ "name": "X-Campaign", "value": "fall" }],
            "attachments": [{ "content": b64_std("hi"), "filename": "a.txt", "disposition": "attachment" }]
        }),
    )
    .await;
    assert_eq!(r.status, StatusCode::ACCEPTED, "{}", r.text);
    assert_eq!(r.header("x-message-id").len(), 24);
    let alice = latest(&app, "alice@example.com").await;
    assert_eq!(alice["subject"], "Hi Alice");
    assert_eq!(alice["html"], "<p>Code 135790</p>");
    assert_eq!(alice["tags"], json!(["otp"]));
    assert_eq!(alice["attachments"][0]["filename"], "a.txt");
    assert_eq!(latest(&app, "bob@example.com").await["subject"], "Hi Bob");

    let email = |to: &str| json!({ "from": { "email": "app@acme.test" }, "to": [{ "email": to }], "subject": "Bulk", "text": "x" });
    let r =
        post_json(&app, "/v1/bulk-email", &[BEARER], json!([email("b1@example.com"), json!({ "subject": "no from" })]))
            .await;
    assert_eq!(r.status, StatusCode::ACCEPTED, "{}", r.text);
    assert_eq!(r.json()["message"], "The bulk email is being processed.");
    assert_eq!(latest(&app, "b1@example.com").await["subject"], "Bulk");

    let r = post_json(&app, "/v1/email", &[], email("a@example.com")).await;
    assert_eq!(r.status, StatusCode::UNAUTHORIZED);
    assert_eq!(r.json(), json!({ "message": "Unauthenticated." }));
    let r = post_json(
        &app,
        "/v1/email",
        &[BEARER],
        json!({ "from": { "email": "app@acme.test" }, "subject": "x", "text": "x" }),
    )
    .await;
    assert_eq!(r.status, StatusCode::UNPROCESSABLE_ENTITY);
    assert_eq!(
        r.json(),
        json!({ "message": "The given data was invalid.", "errors": { "to": ["The to field is required."] } })
    );
}

// ---------------------------------------------------------------------------
// OAuth token endpoints
// ---------------------------------------------------------------------------

const FORM: (&str, &str) = ("content-type", "application/x-www-form-urlencoded");

/// An unsigned JWT with these claims (the token endpoints don't check signatures).
fn jwt(claims: Value) -> String {
    format!("{}.{}.sig", b64url(r#"{"alg":"RS256","typ":"JWT"}"#), b64url(&claims.to_string()))
}

#[tokio::test]
async fn google_token_grants_and_errors() {
    let (app, _) = app();
    let r = call(
        &app,
        Method::POST,
        "/token",
        &[FORM],
        form(&[
            ("grant_type", "refresh_token"),
            ("client_id", "cid.apps.googleusercontent.com"),
            ("client_secret", "s3cret"),
            ("refresh_token", "1//abc"),
        ]),
    )
    .await;
    assert_eq!(r.status, StatusCode::OK, "{}", r.text);
    let body = r.json();
    assert!(body["access_token"].as_str().unwrap().starts_with("ya29."));
    assert_eq!(body["token_type"], "Bearer");
    assert_eq!(body["expires_in"], 3599);
    assert_eq!(r.header("cache-control"), "no-store");
    // The attempt is logged without its secrets.
    let attempt = &attempts(&app).await[0];
    assert_eq!(attempt["provider"], "gmail");
    assert_eq!(attempt["endpoint"], "POST /token (refresh_token)");
    let logged = attempt["request"].as_str().unwrap();
    assert!(
        logged.contains("cid.apps.googleusercontent.com") && !logged.contains("s3cret") && !logged.contains("1//abc")
    );

    // A service account acting as a user: the token knows the mailbox, so `users/me` sends
    // without a From are from that user, as with Gmail.
    let assertion = jwt(
        json!({ "iss": "sa@proj.iam.gserviceaccount.com", "sub": "ann@acme.test", "scope": "https://www.googleapis.com/auth/gmail.send", "aud": "https://oauth2.googleapis.com/token" }),
    );
    let r = call(
        &app,
        Method::POST,
        "/token",
        &[FORM],
        form(&[("grant_type", "urn:ietf:params:oauth:grant-type:jwt-bearer"), ("assertion", &assertion)]),
    )
    .await;
    assert_eq!(r.status, StatusCode::OK, "{}", r.text);
    let token = format!("Bearer {}", r.json()["access_token"].as_str().unwrap());
    let r = post_json(
        &app,
        "/gmail/v1/users/me/messages/send",
        &[("authorization", &token)],
        json!({ "raw": b64url("To: bob@example.com\r\nSubject: From Ann\r\n\r\nhi") }),
    )
    .await;
    assert_eq!(r.status, StatusCode::OK, "{}", r.text);
    assert_eq!(latest(&app, "bob@example.com").await["from"]["email"], "ann@acme.test");

    let google = |pairs: &[(&str, &str)]| call(&app, Method::POST, "/token", &[FORM], form(pairs));
    let r = google(&[
        ("grant_type", "refresh_token"),
        ("client_id", "c"),
        ("client_secret", "s"),
        ("refresh_token", "revoked"),
    ])
    .await;
    assert_eq!(r.status, StatusCode::BAD_REQUEST);
    assert_eq!(
        r.json(),
        json!({ "error": "invalid_grant", "error_description": "Token has been expired or revoked." })
    );
    let r = google(&[("grant_type", "refresh_token"), ("refresh_token", "x")]).await;
    assert_eq!(r.json()["error_description"], "Could not determine client ID from request.");
    let r = google(&[("grant_type", "refresh_token"), ("client_id", "c")]).await;
    assert_eq!(r.json()["error_description"], "client_secret is missing.");
    let r = google(&[("grant_type", "refresh_token"), ("client_id", "c"), ("client_secret", "s")]).await;
    assert_eq!(r.json()["error_description"], "Missing required parameter: refresh_token");
    let r = google(&[]).await;
    assert_eq!(r.json(), json!({ "error": "unsupported_grant_type", "error_description": "Invalid grant_type: " }));
    let r = google(&[("grant_type", "client_credentials"), ("client_id", "c")]).await;
    assert_eq!(r.json()["error"], "unsupported_grant_type");
    let r = google(&[
        ("grant_type", "refresh_token"),
        ("client_id", "c"),
        ("client_secret", "invalid"),
        ("refresh_token", "x"),
    ])
    .await;
    assert_eq!(r.status, StatusCode::UNAUTHORIZED);
    assert_eq!(
        r.json(),
        json!({ "error": "invalid_client", "error_description": "The provided client secret is invalid." })
    );
    assert_eq!(attempts(&app).await[0]["outcome"], "failed");
}

#[tokio::test]
async fn microsoft_token_grants_and_errors() {
    let (app, _) = app();
    let path = "/contoso.onmicrosoft.com/oauth2/v2.0/token";
    let ms = |pairs: Vec<(&'static str, &'static str)>| {
        let app = app.clone();
        async move { call(&app, Method::POST, path, &[FORM], form(&pairs)).await }
    };
    let r = ms(vec![
        ("grant_type", "client_credentials"),
        ("client_id", "app-1"),
        ("client_secret", "s"),
        ("scope", "https://graph.microsoft.com/.default"),
    ])
    .await;
    assert_eq!(r.status, StatusCode::OK, "{}", r.text);
    let body = r.json();
    assert_eq!(body["token_type"], "Bearer");
    assert_eq!(body["ext_expires_in"], 3599);
    assert_eq!(body["access_token"].as_str().unwrap().split('.').count(), 3);
    assert!(body.get("refresh_token").is_none());
    assert_eq!(attempts(&app).await[0]["provider"], "graph");

    // A user's token: `/me` sends are from them.
    let r = ms(vec![
        ("grant_type", "password"),
        ("client_id", "app-1"),
        ("username", "ann@contoso.test"),
        ("password", "pw"),
        ("scope", "Mail.Send offline_access"),
    ])
    .await;
    assert_eq!(r.status, StatusCode::OK, "{}", r.text);
    assert!(r.json()["refresh_token"].is_string());
    let token = format!("Bearer {}", r.json()["access_token"].as_str().unwrap());
    let send = json!({ "message": { "subject": "Hi", "body": { "contentType": "Text", "content": "x" }, "toRecipients": [{ "emailAddress": { "address": "bob@example.com" } }] } });
    let r = post_json(&app, "/v1.0/me/sendMail", &[("authorization", &token)], send).await;
    assert_eq!(r.status, StatusCode::ACCEPTED, "{}", r.text);
    assert_eq!(latest(&app, "bob@example.com").await["from"]["email"], "ann@contoso.test");

    let r = ms(vec![("client_id", "app-1")]).await;
    assert_eq!(r.status, StatusCode::BAD_REQUEST);
    let body = r.json();
    assert_eq!(body["error"], "invalid_request");
    assert_eq!(body["error_codes"], json!([900144]));
    assert!(
        body["error_description"]
            .as_str()
            .unwrap()
            .starts_with("AADSTS900144: The request body must contain the following parameter: 'grant_type'.")
    );
    let r = ms(vec![
        ("grant_type", "client_credentials"),
        ("client_id", "app-1"),
        ("client_secret", "s"),
        ("scope", "Mail.Send"),
    ])
    .await;
    assert_eq!(r.json()["error"], "invalid_scope");
    let r = ms(vec![
        ("grant_type", "client_credentials"),
        ("client_id", "app-1"),
        ("scope", "https://graph.microsoft.com/.default"),
    ])
    .await;
    assert_eq!(r.status, StatusCode::UNAUTHORIZED);
    assert_eq!(r.json()["error_codes"], json!([7000216]));
    let r = ms(vec![("grant_type", "client_credentials"), ("client_id", "app-1"), ("client_secret", "s")]).await;
    assert_eq!(r.json()["error_codes"], json!([90014]));
    let r = ms(vec![("grant_type", "nope"), ("client_id", "app-1")]).await;
    assert_eq!(r.json()["error"], "unsupported_grant_type");
    assert!(r.json().get("error_uri").is_none());

    // MSAL's delegated flows get client_info, and an ID token with `openid`.
    let r = ms(vec![
        ("grant_type", "authorization_code"),
        ("client_id", "app-1"),
        ("code", "c"),
        ("redirect_uri", "http://localhost/cb"),
        ("scope", "openid Mail.Send"),
    ])
    .await;
    assert_eq!(r.status, StatusCode::OK, "{}", r.text);
    assert!(r.json()["client_info"].is_string());
    assert_eq!(r.json()["id_token"].as_str().unwrap().split('.').count(), 3);
    // MSAL adds ?client-request-id=…
    let r = call(
        &app,
        Method::POST,
        &format!("{path}?client-request-id=abc"),
        &[FORM],
        form(&[
            ("grant_type", "client_credentials"),
            ("client_id", "a"),
            ("client_secret", "s"),
            ("scope", "https://graph.microsoft.com/.default"),
        ]),
    )
    .await;
    assert_eq!(r.status, StatusCode::OK);
    let r = get(&app, "/contoso.onmicrosoft.com/v2.0/.well-known/openid-configuration").await;
    assert_eq!(
        r.json()["token_endpoint"],
        "https://login.microsoftonline.com/contoso.onmicrosoft.com/oauth2/v2.0/token"
    );
    let r = ms(vec![
        ("grant_type", "refresh_token"),
        ("client_id", "app-1"),
        ("refresh_token", "revoked"),
        ("scope", "Mail.Send"),
    ])
    .await;
    assert_eq!(r.json()["error"], "invalid_grant");
    assert_eq!(r.json()["error_codes"], json!([70008]));
}
