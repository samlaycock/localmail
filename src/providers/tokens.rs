//! OAuth token endpoints for the mailbox APIs, so Gmail and Microsoft Graph clients can get an
//! access token offline before they send:
//!
//! - Google (`POST /token`, as at `oauth2.googleapis.com`, and the older `/oauth2/v4/token`):
//!   the `refresh_token`, `authorization_code` and service-account JWT bearer grants.
//! - Microsoft identity platform (`POST /{tenant}/oauth2/v2.0/token`): the `client_credentials`,
//!   `refresh_token`, `authorization_code`, `password` and on-behalf-of grants.
//!
//! Any client and secret are accepted, but the parameters each grant needs are checked, with the
//! provider's error format. (Microsoft's SDKs insist on an https authority, so apps reach the
//! Microsoft endpoint by rewriting URLs; `{tenant}/v2.0/.well-known/openid-configuration` is
//! served for MSAL's discovery.) A refresh token, code or assertion of `revoked` fails with
//! `invalid_grant`, and a client secret of `invalid` with `invalid_client`, to test those paths.
//! Issued tokens carry the user they're for (the service account's `sub`, the `username`), so a
//! send to `users/me` knows its mailbox.

use std::collections::BTreeMap;

use axum::body::Bytes;
use axum::extract::{Path, State};
use axum::http::{HeaderMap, StatusCode};
use axum::response::{IntoResponse, Response};
use axum::routing::post;
use axum::{Json, Router};
use serde_json::{Value, json};

use super::{basic, header};
use crate::attempts::AttemptNote;
use crate::message::Provider;
use crate::state::AppState;
use crate::util::random_from;

/// Access tokens last an hour, as both providers' do.
const EXPIRES_IN: u64 = 3599;
const URL_SAFE: &[u8] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789-_";
/// Parameters that are secrets, left out of the attempts log.
const SECRETS: [&str; 7] =
    ["client_secret", "refresh_token", "code", "code_verifier", "assertion", "client_assertion", "password"];

pub fn routes() -> Router<AppState> {
    Router::new()
        .route("/token", post(google))
        .route("/oauth2/v4/token", post(google))
        .route("/{tenant}/oauth2/v2.0/token", post(microsoft))
        .route("/{tenant}/v2.0/.well-known/openid-configuration", axum::routing::get(openid_configuration))
}

/// The request's parameters: a form (or, for Google, JSON), plus HTTP Basic client credentials.
fn params(headers: &HeaderMap, body: &[u8]) -> BTreeMap<String, String> {
    let json = header(headers, "content-type").is_some_and(|c| c.to_ascii_lowercase().contains("json"));
    let mut out: BTreeMap<String, String> = match serde_json::from_slice::<Value>(body) {
        Ok(Value::Object(m)) if json => m.into_iter().map(|(k, v)| (k, crate::util::value_to_string(&v))).collect(),
        _ => url::form_urlencoded::parse(body).into_owned().collect(),
    };
    out.retain(|_, v| !v.trim().is_empty());
    if let Some((id, secret)) = basic(headers) {
        out.entry("client_id".into()).or_insert(id);
        if !secret.is_empty() {
            out.entry("client_secret".into()).or_insert(secret);
        }
    }
    out
}

/// The parameters for the attempts log, with the secrets left out.
fn redacted(params: &BTreeMap<String, String>) -> String {
    let shown: serde_json::Map<String, Value> = params
        .iter()
        .map(|(k, v)| {
            let v = if SECRETS.contains(&k.as_str()) { "(redacted)".to_string() } else { v.clone() };
            (k.clone(), Value::String(v))
        })
        .collect();
    serde_json::to_string_pretty(&Value::Object(shown)).unwrap_or_default()
}

fn b64url(data: &[u8]) -> String {
    crate::util::b64_std(data).replace('+', "-").replace('/', "_").trim_end_matches('=').to_string()
}

/// The JSON object in a dotted token's segment, if one decodes (a JWT's claims, or the user part
/// of a localmail Google token).
fn claims(token: &str) -> Vec<Value> {
    token
        .split('.')
        .filter_map(crate::util::b64_decode_lenient)
        .filter_map(|b| serde_json::from_slice::<Value>(&b).ok())
        .filter(Value::is_object)
        .collect()
}

/// The mailbox an access token is for: localmail's own tokens, and real JWTs with a `upn`,
/// `preferred_username` or `email` claim.
pub fn mailbox_of(token: &str) -> Option<String> {
    claims(token).iter().find_map(|c| {
        ["email", "upn", "preferred_username", "unique_name"]
            .iter()
            .filter_map(|k| c.get(*k).and_then(Value::as_str))
            .find(|e| crate::address::is_valid_email(e))
            .map(String::from)
    })
}

struct Error {
    status: StatusCode,
    error: &'static str,
    description: String,
    /// The AADSTS code, for Microsoft.
    code: Option<u32>,
}

fn err(error: &'static str, description: impl Into<String>) -> Error {
    Error { status: StatusCode::BAD_REQUEST, error, description: description.into(), code: None }
}

fn note(provider: Provider, endpoint: &str, grant: &str, params: &BTreeMap<String, String>) -> AttemptNote {
    let endpoint = if grant.is_empty() { endpoint.to_string() } else { format!("{endpoint} ({grant})") };
    let mut note = AttemptNote::new(provider, &endpoint);
    note.request = Some(redacted(params));
    note
}

fn finish(mut note: AttemptNote, result: Result<Value, Error>, error: impl Fn(&Error) -> Response) -> Response {
    let mut resp = match result {
        Ok(body) => {
            let mut resp = Json(body).into_response();
            let h = resp.headers_mut();
            h.insert("cache-control", "no-store".parse().unwrap());
            h.insert("pragma", "no-cache".parse().unwrap());
            resp
        }
        Err(e) => {
            note.error = Some(e.error.into());
            note.error_description = Some(e.description.clone());
            error(&e)
        }
    };
    note.attach(&mut resp);
    resp
}

// ---------------------------------------------------------------------------
// Google
// ---------------------------------------------------------------------------

const JWT_BEARER: &str = "urn:ietf:params:oauth:grant-type:jwt-bearer";
const GOOGLE_SCOPE: &str = "https://www.googleapis.com/auth/gmail.send";

async fn google(State(_): State<AppState>, headers: HeaderMap, body: Bytes) -> Response {
    let params = params(&headers, &body);
    let grant = params.get("grant_type").cloned().unwrap_or_default();
    let note = note(Provider::Gmail, "POST /token", &grant, &params);
    finish(note, google_grant(&grant, &params), |e| {
        (e.status, Json(json!({ "error": e.error, "error_description": e.description }))).into_response()
    })
}

/// `ya29.<the user, as base64url JSON>.<random>`: shaped like a Google access token, and telling
/// a Gmail send whose mailbox it is.
fn google_token(user: Option<&str>) -> String {
    let user = b64url(json!({ "localmail": "google", "email": user }).to_string().as_bytes());
    format!("ya29.{user}.{}", random_from(URL_SAFE, 64))
}

fn google_grant(grant: &str, p: &BTreeMap<String, String>) -> Result<Value, Error> {
    let get = |k: &str| p.get(k).map(String::as_str);
    let need = |k: &str| get(k).ok_or_else(|| err("invalid_request", format!("Missing required parameter: {k}")));
    let client = || -> Result<(), Error> {
        get("client_id").ok_or_else(|| err("invalid_request", "Could not determine client ID from request."))?;
        match get("client_secret") {
            None => Err(err("invalid_request", "client_secret is missing.")),
            Some("invalid") => Err(Error {
                status: StatusCode::UNAUTHORIZED,
                ..err("invalid_client", "The provided client secret is invalid.")
            }),
            Some(_) => Ok(()),
        }
    };
    let scope = get("scope").unwrap_or(GOOGLE_SCOPE);
    match grant {
        "refresh_token" => {
            client()?;
            if need("refresh_token")? == "revoked" {
                return Err(err("invalid_grant", "Token has been expired or revoked."));
            }
            Ok(
                json!({ "access_token": google_token(None), "expires_in": EXPIRES_IN, "scope": scope, "token_type": "Bearer" }),
            )
        }
        "authorization_code" => {
            client()?;
            if need("code")? == "revoked" {
                return Err(err("invalid_grant", "Bad Request"));
            }
            get("redirect_uri").ok_or_else(|| err("invalid_request", "Missing parameter: redirect_uri"))?;
            Ok(json!({
                "access_token": google_token(None),
                "expires_in": EXPIRES_IN,
                "refresh_token": format!("1//{}", random_from(URL_SAFE, 100)),
                "scope": scope,
                "token_type": "Bearer",
            }))
        }
        JWT_BEARER => {
            let assertion = get("assertion").ok_or_else(|| err("invalid_request", "Bad Request"))?;
            if assertion == "revoked" {
                return Err(err("invalid_grant", "Invalid JWT Signature."));
            }
            let c = claims(assertion)
                .into_iter()
                .find(|c| c.get("iss").is_some())
                .ok_or_else(|| err("invalid_request", "Bad Request"))?;
            let scope = c.get("scope").and_then(Value::as_str).filter(|s| !s.trim().is_empty());
            let Some(scope) = scope else {
                return Err(err("invalid_scope", "Invalid OAuth scope or ID token audience provided."));
            };
            // With domain-wide delegation, `sub` is the user to act as.
            let user = c.get("sub").or_else(|| c.get("iss")).and_then(Value::as_str);
            Ok(
                json!({ "access_token": google_token(user), "scope": scope, "token_type": "Bearer", "expires_in": EXPIRES_IN }),
            )
        }
        other => Err(err("unsupported_grant_type", format!("Invalid grant_type: {other}"))),
    }
}

// ---------------------------------------------------------------------------
// Microsoft identity platform
// ---------------------------------------------------------------------------

async fn microsoft(State(_): State<AppState>, Path(tenant): Path<String>, headers: HeaderMap, body: Bytes) -> Response {
    let params = params(&headers, &body);
    let grant = params.get("grant_type").cloned().unwrap_or_default();
    let note = note(Provider::Graph, &format!("POST /{tenant}/oauth2/v2.0/token"), &grant, &params);
    finish(note, microsoft_grant(&tenant, &grant, &params), aad_error)
}

fn aadsts(status: StatusCode, error: &'static str, code: u32, message: impl Into<String>) -> Error {
    Error { status, error, description: message.into(), code: Some(code) }
}

fn missing(param: &str) -> Error {
    aadsts(
        StatusCode::BAD_REQUEST,
        "invalid_request",
        900144,
        format!("The request body must contain the following parameter: '{param}'."),
    )
}

/// `{error, error_description, error_codes, timestamp, trace_id, correlation_id, error_uri}`.
fn aad_error(e: &Error) -> Response {
    let code = e.code.unwrap_or(90000);
    let (trace, correlation) = (uuid::Uuid::new_v4().to_string(), uuid::Uuid::new_v4().to_string());
    let timestamp = crate::util::iso8601(crate::util::wall_clock()).replace('T', " ");
    let mut body = json!({
        "error": e.error,
        "error_description": format!("AADSTS{code}: {} Trace ID: {trace} Correlation ID: {correlation} Timestamp: {timestamp}", e.description),
        "error_codes": [code],
        "timestamp": timestamp,
        "trace_id": trace,
        "correlation_id": correlation,
    });
    // Entra leaves the link out for these.
    if ![70003, 1002012].contains(&code) {
        body["error_uri"] = json!(format!("https://login.microsoftonline.com/error?code={code}"));
    }
    (e.status, Json(body)).into_response()
}

/// A JWT shaped like an Entra ID token, not really signed.
fn jwt(claims: &Value) -> String {
    let head = b64url(br#"{"typ":"JWT","alg":"RS256","kid":"localmail"}"#);
    format!("{head}.{}.{}", b64url(claims.to_string().as_bytes()), random_from(URL_SAFE, 86))
}

/// An access token for Graph. Graph SDKs pass it on without reading it; localmail reads its `upn`
/// to know whose mailbox `/me` is.
fn microsoft_token(tenant: &str, client_id: &str, scope: &str, user: Option<&str>) -> String {
    let now = crate::util::wall_clock();
    let mut claims = json!({
        "aud": "https://graph.microsoft.com",
        "iss": format!("https://sts.windows.net/{tenant}/"),
        "iat": now, "nbf": now, "exp": now + EXPIRES_IN as i64,
        "appid": client_id,
        "tid": tenant,
        "ver": "1.0",
    });
    match user {
        Some(u) => {
            claims["upn"] = json!(u);
            claims["scp"] =
                json!(scope.split_whitespace().filter_map(|s| s.rsplit('/').next()).collect::<Vec<_>>().join(" "));
        }
        None => claims["roles"] = json!(["Mail.Send"]),
    }
    jwt(&claims)
}

/// The metadata MSAL reads from `{authority}/v2.0/.well-known/openid-configuration`. Its URLs are
/// Microsoft's: MSAL insists on https, so an app reaches localmail by rewriting them in its HTTP
/// client.
async fn openid_configuration(Path(tenant): Path<String>) -> Json<Value> {
    let base = format!("https://login.microsoftonline.com/{tenant}");
    Json(json!({
        "issuer": format!("{base}/v2.0"),
        "authorization_endpoint": format!("{base}/oauth2/v2.0/authorize"),
        "token_endpoint": format!("{base}/oauth2/v2.0/token"),
        "device_authorization_endpoint": format!("{base}/oauth2/v2.0/devicecode"),
        "end_session_endpoint": format!("{base}/oauth2/v2.0/logout"),
        "jwks_uri": format!("https://login.microsoftonline.com/{tenant}/discovery/v2.0/keys"),
        "response_types_supported": ["code", "id_token", "code id_token", "id_token token"],
        "scopes_supported": ["openid", "profile", "email", "offline_access"],
        "token_endpoint_auth_methods_supported": ["client_secret_post", "private_key_jwt", "client_secret_basic"],
        "tenant_region_scope": null,
        "cloud_instance_name": "microsoftonline.com",
    }))
}

fn microsoft_grant(tenant: &str, grant: &str, p: &BTreeMap<String, String>) -> Result<Value, Error> {
    let get = |k: &str| p.get(k).map(String::as_str);
    let need = |k: &str| get(k).ok_or_else(|| missing(k));
    if grant.is_empty() {
        return Err(missing("grant_type"));
    }
    let client_id = need("client_id")?;
    if get("client_secret") == Some("invalid") {
        return Err(aadsts(
            StatusCode::UNAUTHORIZED,
            "invalid_client",
            7000215,
            format!(
                "Invalid client secret provided. Ensure the secret being sent in the request is the client secret value, not the client secret ID, for a secret added to app '{client_id}'."
            ),
        ));
    }
    let expired =
        || aadsts(StatusCode::BAD_REQUEST, "invalid_grant", 70008, "The provided access grant is expired or revoked.");
    // Delegated grants: a refresh token, and for MSAL its `client_info` and (with `openid`) an
    // ID token for the account.
    let delegated = |scope: &str, user: Option<&str>| {
        let oid = uuid::Uuid::new_v4().to_string();
        let mut body = json!({
            "token_type": "Bearer",
            "scope": scope,
            "expires_in": EXPIRES_IN,
            "ext_expires_in": EXPIRES_IN,
            "access_token": microsoft_token(tenant, client_id, scope, user),
            "refresh_token": format!("1.A{}", random_from(URL_SAFE, 120)),
            "client_info": b64url(json!({ "uid": oid, "utid": tenant }).to_string().as_bytes()),
        });
        if scope.split_whitespace().any(|s| s == "openid") {
            let now = crate::util::wall_clock();
            let mut claims = json!({
                "aud": client_id,
                "iss": format!("https://login.microsoftonline.com/{tenant}/v2.0"),
                "iat": now, "nbf": now, "exp": now + EXPIRES_IN as i64,
                "oid": oid, "sub": oid, "tid": tenant, "ver": "2.0",
            });
            if let Some(u) = user {
                claims["preferred_username"] = json!(u);
            }
            body["id_token"] = json!(jwt(&claims));
        }
        body
    };
    match grant {
        "client_credentials" => {
            let scope = get("scope").ok_or_else(|| {
                aadsts(
                    StatusCode::BAD_REQUEST,
                    "invalid_request",
                    90014,
                    "The required field 'scope' is missing from the credential. Ensure that you have all the necessary parameters for the login request.",
                )
            })?;
            if !scope.split_whitespace().all(|s| s.ends_with("/.default")) {
                return Err(aadsts(
                    StatusCode::BAD_REQUEST,
                    "invalid_scope",
                    1002012,
                    format!(
                        "The provided value for scope {scope} is not valid. Client credential flows must have a scope value with /.default suffixed to the resource identifier (application ID URI)."
                    ),
                ));
            }
            if get("client_secret").is_none() && get("client_assertion").is_none() {
                return Err(aadsts(
                    StatusCode::UNAUTHORIZED,
                    "invalid_client",
                    7000216,
                    "'client_assertion', 'client_secret' or 'request' is required for the 'client_credentials' grant type.",
                ));
            }
            Ok(json!({
                "token_type": "Bearer",
                "expires_in": EXPIRES_IN,
                "ext_expires_in": EXPIRES_IN,
                "access_token": microsoft_token(tenant, client_id, scope, None),
            }))
        }
        "refresh_token" => {
            if need("refresh_token")? == "revoked" {
                return Err(expired());
            }
            Ok(delegated(need("scope")?, None))
        }
        "authorization_code" => {
            if need("code")? == "revoked" {
                return Err(expired());
            }
            need("redirect_uri")?;
            Ok(delegated(need("scope")?, None))
        }
        "password" => {
            let user = need("username")?;
            need("password")?;
            Ok(delegated(need("scope")?, Some(user)))
        }
        JWT_BEARER => {
            let assertion = need("assertion")?;
            if assertion == "revoked" {
                return Err(expired());
            }
            need("requested_token_use")?;
            let user = mailbox_of(assertion);
            Ok(delegated(need("scope")?, user.as_deref()))
        }
        other => Err(aadsts(
            StatusCode::BAD_REQUEST,
            "unsupported_grant_type",
            70003,
            format!("The app requested an unsupported grant type '{other}'."),
        )),
    }
}
