# localmail

## 0.2.0

### Minor Changes

- [#2](https://github.com/samlaycock/localmail/pull/2) [`bba22b9`](https://github.com/samlaycock/localmail/commit/bba22b989f2a5fa78abc40d74d4a36fb46870446) Thanks [@samlaycock](https://github.com/samlaycock)! - Mailboxes: the inbox lists every address that has received mail, with unread counts, and shows one address's mail (To, Cc or Bcc) at its own link, `/?to=<address>`. Recipients of an open message link to their mailboxes, and Mark all read and Delete all apply to the messages listed (the open mailbox, narrowed by any search or provider filter). The dev API adds `GET /dev/api/mailboxes`; `POST /dev/api/messages/read` accepts the message filters; and the `unread` count from `GET /dev/api/messages` now counts the matching messages rather than the whole inbox.

## 0.1.0

### Minor Changes

- [`2f5ceca`](https://github.com/samlaycock/localmail/commit/2f5ceca26b0e8eb48efb356a3b9024b8f8449464) Thanks [@samlaycock](https://github.com/samlaycock)! - First release of localmail, a local SMTP server and email API emulator for offline development, published as a Docker image (`ghcr.io/samlaycock/localmail`) for linux/amd64 and linux/arm64. It keeps every message instead of delivering it, and shows them in a web inbox (HTML, text, headers, source, attachments and the original API request). Besides SMTP (any AUTH PLAIN/LOGIN credentials), it emulates the sending APIs of Amazon SES (v2 `SendEmail`; v1 `SendEmail`, `SendRawEmail` and `SendTemplatedEmail`), SendGrid (`/v3/mail/send`), Postmark (single, batch and template sends) and Resend (single and batch sends), with each provider's request validation, response shapes, message ID formats and error bodies, so the official SDKs work unchanged. Scenario addresses (`reject@`, `unauthorized@`, `ratelimit@`, `error@`, `slow-<duration>@` and `flaky-<n>@localmail.test`) make a send fail or slow down the way the provider would. For tests, a dev API lists, deletes and long-polls for messages from a cursor, and extracts one-time codes and links; an attempts log records every send, accepted or not.
