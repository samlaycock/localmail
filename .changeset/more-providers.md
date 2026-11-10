---
"localmail": minor
---

Emulate eight more sending APIs: Gmail (`messages.send`, raw and uploaded), Microsoft Graph (`sendMail`, JSON and MIME), Mailgun (`messages` and `messages.mime`, with batch sends), Mailjet (Send API v3.1), Brevo (`/v3/smtp/email`, with `messageVersions`), SparkPost (transmissions), Mailchimp Transactional / Mandrill (`send`, `send-template`, `send-raw`) and MailerSend (`email` and `bulk-email`). Each validates requests and answers, and fails, in its provider's format, and the scenario addresses work with all of them. Google's and Microsoft's OAuth token endpoints are emulated too (`POST /token`, `POST /{tenant}/oauth2/v2.0/token`), so Gmail and Graph clients can get a token offline; a token issued for a user makes `users/me` and `/me` sends come from that user.
