# localmail

[![CI](https://github.com/samlaycock/localmail/actions/workflows/ci.yml/badge.svg)](https://github.com/samlaycock/localmail/actions/workflows/ci.yml)
[![Release](https://img.shields.io/github/v/release/samlaycock/localmail)](https://github.com/samlaycock/localmail/releases)
[![License: MIT OR Apache-2.0](https://img.shields.io/badge/license-MIT%20OR%20Apache--2.0-blue)](#license)

A local SMTP server and email API emulator for offline development and testing. It's a single Rust binary (or Docker image) with no external services. It keeps every message instead of delivering it and shows them in a web inbox. It's a sibling of [localoidc](https://github.com/samlaycock/localoidc), [localsaml](https://github.com/samlaycock/localsaml) and [localflake](https://github.com/samlaycock/localflake).

- **SMTP and the provider APIs.** Besides SMTP, it emulates the sending APIs of **Amazon SES** (v2 and v1), **SendGrid**, **Postmark** and **Resend**. Point the official SDK at localmail and your code runs unchanged: the same request validation, response shapes, message ID formats and error bodies as the real service.
- **A web inbox.** Read each message's HTML (sandboxed), text, headers, MIME source and attachments, plus the exact API request your app sent. One-time codes and links are pulled out for you.
- **Failures on demand.** Send to a [scenario address](#scenario-addresses) such as `ratelimit@localmail.test` or `flaky-2@localmail.test`, and the send fails the way the provider would: a 429 with SendGrid's body, SES's `MessageRejected`, an SMTP `451`, and so on. Your app's error handling and retries get tested without a real outage.
- **Built for automated tests.** A [dev API](#test-automation) waits for the next email to an address and returns its one-time code or links. Wait from a cursor so earlier messages never confuse a test, and use unique addresses so parallel tests don't interfere.
- **Every attempt recorded.** The [attempts log](#attempts) keeps every send, accepted or not: validation errors, scenario failures and SMTP transcripts.

> [!WARNING]
> localmail is for development and testing only. It accepts any credentials and stores whatever it's sent. Its dev API is unauthenticated and answers cross-origin requests, so any page you visit while it runs can read your captured email. Never expose it to the internet or send real data through it.

## Contents

- [Installation](#installation)
- [Quick start](#quick-start)
- [Pointing your app at localmail](#pointing-your-app-at-localmail)
- [What each provider emulates](#what-each-provider-emulates)
- [Scenario addresses](#scenario-addresses)
- [Test automation](#test-automation)
- [The inbox](#the-inbox)
- [Attempts](#attempts)
- [Configuration](#configuration)
- [Contributing](#contributing)
- [License](#license)

## Installation

**Docker** (linux/amd64 and linux/arm64):

```sh
docker run --rm -p 9402:9402 -p 2525:2525 -v localmail:/data ghcr.io/samlaycock/localmail
```

The image is tagged `:<version>`, `:<major>.<minor>` (and `:<major>` from 1.0) and `:latest`. It runs as a non-root user and keeps messages in the `/data` volume. Pass flags after the image name, e.g. `docker run … ghcr.io/samlaycock/localmail --in-memory`.

**From source** (needs a recent stable Rust toolchain):

```sh
cargo install --git https://github.com/samlaycock/localmail --locked
localmail
```

Or clone the repository and run `cargo run --release`.

localmail serves HTTP on port 9402 and SMTP on port 2525, so it can run next to localoidc (9400) and localsaml (9401).

## Quick start

Start localmail with either command above, then send something:

```sh
# Over SMTP
printf 'Subject: Hello\r\n\r\nYour code is 482913\r\n' | curl -s smtp://localhost:2525 \
  --mail-from app@example.com --mail-rcpt alice@example.com -T -

# Through the Resend API (any token will do)
curl -s http://localhost:9402/emails -H 'Authorization: Bearer re_test' -H 'Content-Type: application/json' \
  -d '{"from":"Acme <app@example.com>","to":"bob@example.com","subject":"Welcome","html":"<p>Hi Bob</p>"}'
```

Open http://localhost:9402 to read them. The **Setup** page lists every endpoint, with copy-paste snippets for each SDK.

## Pointing your app at localmail

Every API accepts any credentials, but it still expects the kind its provider uses: a bearer token, a Postmark server token, or a SigV4 `Authorization` header. A request with none fails the way the real API would.

| Provider | Endpoint | Configure the SDK |
|---|---|---|
| SMTP | `localhost:2525` | Any client. AUTH PLAIN and LOGIN accept any credentials. There's no TLS, so turn STARTTLS off (Nodemailer: `secure: false, ignoreTLS: true`). |
| Amazon SES v2 | `POST /v2/email/outbound-emails` | `AWS_ENDPOINT_URL_SESV2=http://localhost:9402`, or `endpoint` (JS v3) / `endpoint_url` (boto3) |
| Amazon SES v1 | `POST /` with `Action=SendEmail`, `SendRawEmail` or `SendTemplatedEmail` | `AWS_ENDPOINT_URL_SES=http://localhost:9402` (`AWS_ENDPOINT_URL` sets both) |
| SendGrid | `POST /v3/mail/send` | Node: `sgMail.client.setDefaultRequest('baseUrl', 'http://localhost:9402')`, called *after* `setApiKey`, which resets it. Python: `SendGridAPIClient(key, host='http://localhost:9402')` |
| Postmark | `POST /email`, `/email/batch`, `/email/withTemplate`, `/email/batchWithTemplates` | Node: `new ServerClient(token, { useHttps: false, requestHost: 'localhost:9402' })`. Python (postmarker): `PostmarkClient(server_token=t, root_api_url='http://localhost:9402/')` |
| Resend | `POST /emails`, `/emails/batch` | Node: `RESEND_BASE_URL=http://localhost:9402` or `new Resend(key, { baseUrl })`. Python: `RESEND_API_URL=http://localhost:9402` |

For example, with the AWS SDK for JavaScript v3:

```js
const ses = new SESv2Client({
  endpoint: 'http://localhost:9402',
  region: 'us-east-1',
  credentials: { accessKeyId: 'test', secretAccessKey: 'test' },
});
await ses.send(new SendEmailCommand({ FromEmailAddress: 'app@example.com', /* … */ }));
```

These SDKs are exercised against localmail: `@aws-sdk/client-sesv2`, `@aws-sdk/client-ses`, `@sendgrid/mail`, `postmark`, `resend` and `nodemailer`. Errors reach your code as each SDK's own error types, for example Postmark's `InactiveRecipientsError` or AWS's `MessageRejected` and `TooManyRequestsException`.

## What each provider emulates

Only sending: localmail doesn't send webhooks, receive email or manage suppression lists, domains or templates. Each API validates requests the way its provider does, answers with the provider's response shape and message ID format, and fails with the provider's error format and status codes.

- **Amazon SES v2** (`SendEmail`). Supports `Simple` content (with `Headers` and `Attachments`), `Raw` MIME, and `Template` content. Inline `TemplateContent` is rendered with `TemplateData` (`{{name}}` placeholders); a stored `TemplateName` or `TemplateArn` is kept with its data. Errors set `x-amzn-ErrorType` (`MessageRejected`, `BadRequestException`, `TooManyRequestsException`, …) and the region in the credential scope appears in SES's messages. Allows at most 50 recipients.
- **Amazon SES v1** (`SendEmail`, `SendRawEmail`, `SendTemplatedEmail`). Form-encoded requests with XML responses and `ErrorResponse` errors. Throttling is v1's `400 Throttling`. Other actions answer `InvalidAction`.
- **SendGrid** (`/v3/mail/send`). Each personalization becomes its own message, with its own subject, headers, `substitutions` and `dynamic_template_data`. The response is `202` with `X-Message-Id`, or `200` in sandbox mode, and errors come back as `{"errors":[{"message","field","help"}]}` using SendGrid's messages. Allows at most 1,000 recipients and 1,000 personalizations.
- **Postmark** (`/email`, `/email/withTemplate`, and both batch endpoints). Returns `To`, `SubmittedAt`, `MessageID`, `ErrorCode` and `Message`, with Postmark's error codes (`10`, `300`, `402`, `406`, `1101`, …). Batches answer `200` with a result per message, so one bad message doesn't fail the rest. Allows at most 50 recipients and 500 messages per batch.
- **Resend** (`/emails` and `/emails/batch`). Errors are `{"statusCode","message","name"}`. Batches are all-or-nothing unless sent with `x-batch-validation: permissive`, which returns `errors` by index. Allows at most 50 `to` addresses and 100 emails per batch. Attachments given by `path` aren't fetched (localmail is offline); a note takes their place.

**Template sends.** SendGrid dynamic templates, Postmark templates, SES stored templates and Resend templates live in the provider account, which localmail can't see. Such a message records the template's ID and data, and its body lists the data, so codes and links in the data can still be [extracted](#test-automation).

**Error messages.** Error bodies follow each provider's documented format. The message text matches the provider's own where it's documented, and is close to it elsewhere. Postmark doesn't document its 429 and 500 bodies, and its SDK goes by the HTTP status.

## Scenario addresses

Send to one of these addresses at `localmail.test` (a reserved TLD that can never be a real address), and the send fails or slows down the way the provider would. A `+suffix` is ignored, so `ratelimit+signup@localmail.test` works too.

| Address | SMTP | APIs |
|---|---|---|
| `reject@localmail.test` | `550 5.1.1` at `RCPT TO`; the other recipients still get the message | A permanent rejection. SES `MessageRejected` (address not verified); Postmark `406` (inactive recipient), per message in batches; SendGrid and Resend `400`/`422`. |
| `unauthorized@localmail.test` | `530 5.7.0` | The credentials are refused: `401`, or SES `403 UnrecognizedClientException` |
| `ratelimit@localmail.test` | `451 4.7.1` | `429` with the provider's body and rate-limit headers. SES v1 answers `400 Throttling`. |
| `error@localmail.test` | `451 4.3.0` | `500` with the provider's body |
| `slow-5s@localmail.test` | Accepted after the delay | Accepted after the delay (up to 60s), for testing client timeouts |
| `flaky-2@localmail.test` | `451` twice, then accepted | `500` twice, then accepted; then the count starts again |

- When a send has several scenario addresses, the strongest wins: `unauthorized`, then `ratelimit`, `error`, `flaky` and `reject`. A `slow` delay applies before any of them.
- `flaky-<n>` counts retries of the same message (same sender, recipients and subject). `GET /dev/api/scenarios` shows the counters and `POST /dev/api/scenarios/reset` clears them.
- An address at `localmail.test` that isn't a scenario (a typo such as `rejcet@`) is delivered normally, with a warning in the [attempts log](#attempts).
- Change the domain with `--scenario-domain`.

## Test automation

The dev API lives under `/dev/api` on the HTTP port. Its query parameters are checked, so a typo (`subjet=`) is a `400` rather than a filter that silently matches everything.

The usual pattern: read the cursor, trigger the email, then wait for it.

```sh
cursor=$(curl -s http://localhost:9402/dev/api/cursor | jq .cursor)
# ... sign up as alice-7f3a@example.com ...
curl -s "http://localhost:9402/dev/api/messages/wait?to=alice-7f3a@example.com&after=$cursor&timeout=30s&extract=otp,links"
```

```json
{
  "message": { "id": 43, "subject": "Verify your email", "provider": "postmark", "text": "…", "html": "…", "…": "…" },
  "extracted": {
    "otp": "482913",
    "link": "http://localhost:3000/verify?token=…",
    "links": [{ "href": "http://localhost:3000/verify?token=…", "text": "Verify email" }]
  }
}
```

In Playwright:

```ts
const mail = 'http://localhost:9402/dev/api';
const { cursor } = await (await fetch(`${mail}/cursor`)).json();
await page.getByRole('button', { name: 'Send code' }).click();
const { extracted } = await (await fetch(`${mail}/messages/wait?to=${email}&after=${cursor}&extract=otp`)).json();
await page.getByLabel('Code').fill(extracted.otp);
```

**Waiting.** `GET /dev/api/messages/wait` returns the first message after the cursor that matches the filters. If none has arrived yet, it holds the request open until one does or the `timeout` (default `30s`, at most `300s`) runs out.

- The cursor is the newest message id. Ids only ever grow, even after deletes (and across restarts, unless `--in-memory`), so `after=<cursor>` never matches an older message.
- Without `after`, the first matching message ever stored is returned.
- A timeout answers `408` with the messages that did arrive after the cursor (`recent`). A wrong filter is the usual reason, and `recent` shows what to fix.

**Filters**, for `wait`, listing and deleting:

| Parameter | Matches |
|---|---|
| `to` | Any recipient (To, Cc or Bcc), case-insensitively |
| `from` | The sender's address |
| `subject` / `subject_contains` | The whole subject / part of it, case-insensitively |
| `provider` | `smtp`, `ses`, `sendgrid`, `postmark` or `resend` |
| `after` | Messages newer than this cursor |
| `q` | Free text over subject, sender and recipients |

**Extraction**, for `wait` and `GET /dev/api/messages/{id}` (`extract=otp,links`), or always on with `GET /dev/api/messages/{id}/extract`:

- `otp`: the most likely one-time code. This is 4–8 digits (or `123 456` / `123-456`), preferring one next to a word such as "code" or "verification", then six digits. Years, prices and numbers inside URLs don't count. A code mixing letters and digits (`K7Q9ZP`) is taken only next to such a word. Override the heuristic with `otp_pattern=<regex>`, whose first group is returned.
- `links`: every `http(s)` link, from the HTML (with its text) and the plain text. `link` is the first one. `link_contains=/reset` keeps only matching links.
- `regex=<pattern>`: the first match (or its first group) anywhere in the subject and bodies.

Codes and links are searched in the subject, the text body, the HTML and, for [template sends](#what-each-provider-emulates), the template data.

**Everything else:**

| Endpoint | |
|---|---|
| `GET /dev/api/cursor` | `{"cursor": <newest id>}` |
| `GET /dev/api/messages` | Matching messages, newest first: `{messages, total, unread, cursor}`. Accepts the filters plus `limit` (default 50) and `offset`. |
| `DELETE /dev/api/messages` | Delete matching messages (all of them without filters): `{"deleted": n}` |
| `GET /dev/api/messages/{id}` | Everything about a message: addresses, headers, text, HTML, attachments, provider details, the original request and, for SMTP, the envelope. `latest` is the newest (matching the filters). |
| `DELETE /dev/api/messages/{id}` | Delete one |
| `GET /dev/api/messages/{id}/raw` | The MIME source (`?download` for an `.eml` file) |
| `GET /dev/api/messages/{id}/html` | The HTML body, sandboxed, with `cid:` images resolved |
| `GET /dev/api/messages/{id}/attachments/{n}` | An attachment (`?download` to save it) |
| `POST /dev/api/messages/{id}/read`, `POST /dev/api/messages/read` | Mark one (body `{"read": false}` to unmark) or all as read |
| `GET`, `DELETE /dev/api/attempts` | The [attempts log](#attempts) |
| `GET /dev/api/scenarios`, `POST /dev/api/scenarios/reset` | Scenario addresses, flaky counters |

**Isolating tests.** Give each test its own recipient, e.g. `alice-<uuid>@example.com`, and filter on it. Parallel workers then never see each other's mail, and nothing needs clearing between tests. `DELETE /dev/api/messages?to=…` tidies up one test's messages.

## The inbox

http://localhost:9402 shows messages as they arrive, newest first.

- Search by subject, sender or recipient, or filter by provider.
- Use `j` and `k` to move through the list.
- Each message shows its HTML (in a sandboxed frame with no scripts, links opening in a new tab), text, headers, MIME source, attachments and the request your app sent to the API.
- The details tab shows what the provider was told: tags, metadata, custom args, message stream, configuration set and region, plus the SMTP envelope.
- A detected one-time code and links sit above the message, with copy buttons.
- Bcc recipients are shown, although real providers leave them out of the headers. Over SMTP, they're the envelope recipients the headers don't name.

## Attempts

http://localhost:9402/attempts lists the last 1,000 sends over SMTP and every API, accepted or not, newest first.

- Each row shows the provider, endpoint, status (HTTP status or SMTP reply code), recipients and outcome: messages stored, `partial` for a batch where some messages failed, or the error.
- Click a row to see the request (attachments shortened) and the response. For SMTP, you see the transcript.
- Requests that failed validation are there too, so you can see exactly why a provider would have rejected your app's request.
- The log is kept in memory, so a restart clears it.

`GET /dev/api/attempts` returns `{"attempts": [...], "latest_id": …, "capacity": 1000}` and accepts `provider`, `to`, `errors=true`, `limit` and `after=<id>`. Use it to check that your app retried after a `flaky` failure.

## Configuration

Settings come from flags, `LOCALMAIL_*` environment variables or `./localmail.toml` (see [`localmail.example.toml`](localmail.example.toml)), in that order of precedence.

| Flag | Default | |
|---|---|---|
| `--host` | `127.0.0.1` | Interface for both servers |
| `--port`, `-p` | `9402` | Web UI, dev API and provider APIs |
| `--smtp-port` | `2525` | SMTP |
| `--base-url` | `http://localhost:<port>` | For links and the setup page's snippets |
| `--data-dir` | `~/.localmail` | Where messages are kept |
| `--in-memory` | off | Keep messages in memory only; every start is a clean inbox |
| `--max-messages` | `5000` | The oldest are deleted beyond this |
| `--max-message-size` | `41943040` (40 MB) | Largest message over SMTP and the APIs |
| `--scenario-domain` | `localmail.test` | Domain of the [scenario addresses](#scenario-addresses) |

### State

- Messages live in the SQLite database `~/.localmail/messages.db` (`--data-dir`), so they survive a restart. Delete the file, or call `DELETE /dev/api/messages`, to start fresh.
- `--in-memory` keeps nothing on disk.
- The attempts log and flaky counters are always in memory.

### Docker

- In the image, messages live in `/data` (`LOCALMAIL_DATA_DIR`), and both servers bind `0.0.0.0` (`LOCALMAIL_HOST`).
- From other containers, use the localmail container's name as the host, e.g. `smtp://localmail:2525` or `http://localmail:9402`.
- Set `LOCALMAIL_BASE_URL` to the URL your browser uses, so the setup page's snippets are right.

## Contributing

Issues and pull requests are welcome. See [CONTRIBUTING.md](CONTRIBUTING.md) for how to build, test and release.

## License

Licensed under either of

- Apache License, Version 2.0 ([LICENSE-APACHE](LICENSE-APACHE) or https://www.apache.org/licenses/LICENSE-2.0)
- MIT license ([LICENSE-MIT](LICENSE-MIT) or https://opensource.org/licenses/MIT)

at your option.

Unless you explicitly state otherwise, any contribution intentionally submitted for inclusion in the work by you, as defined in the Apache-2.0 license, shall be dual licensed as above, without any additional terms or conditions.
