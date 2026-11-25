---
"localmail": minor
---

The web UI is rebuilt as a React app with shadcn/ui components. It works as before, with a few changes:

- Each message has its own link, `/messages/<id>`. Old `/#/m/<id>` links still open the message.
- The search and provider filter are kept in the URL, so a filtered view can be shared or bookmarked.
- The three inbox panes can be resized, and there's a light, dark and system theme switch.
- `GET /dev/api/info` and `GET /dev/api/setup` return what the pages show about the server.
