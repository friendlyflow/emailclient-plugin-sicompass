# Changelog

## 0.3.1

- Mail is fetched and sent in the background again, folders are listed while
  the inbox is fetched, and the message list is cached in SQLite.
- A failure in the background reports one error instead of leaving
  "Loading..." on screen for good.
- Sending over an smtp:// server requires STARTTLS.
- Scroll mode no longer reads ahead into messages you have not opened, because
  fetching a message marks it read on the server.

## 0.3.0

Email is a program of its own now, instead of a sandboxed WebAssembly component.
Sicompass starts it and talks to it, one per tab, and it runs with your rights, so
its entry in the Store says what it does before you install it, and installing it is
your approval.

- Sign-in with Google now goes through Sicompass's browser sign-in. Requests time out after 30 seconds.
- One build for each of Linux (x86_64 and arm64, static), macOS (Apple Silicon and
  Intel) and Windows.
- Needs a Sicompass that runs plugin programs. An older Sicompass keeps the 0.2
  version it has.
