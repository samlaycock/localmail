---
"localmail": minor
---

Mailboxes: the inbox lists every address that has received mail, with unread counts, and shows one address's mail (To, Cc or Bcc) at its own link, `/?to=<address>`. Recipients of an open message link to their mailboxes, and Mark all read and Delete all apply to the open mailbox. The dev API adds `GET /dev/api/mailboxes`; `POST /dev/api/messages/read` accepts the message filters; and the `unread` count from `GET /dev/api/messages` now counts the matching messages rather than the whole inbox.
