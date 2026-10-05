# Project Instructions

emailclient-plugin-sicompass was split out of the
[sicompass](https://github.com/friendlyflow/sicompass) workspace, and its git
history before that point is the history of `lib/lib_emailclient` (earlier
`lib/lib_emailclient-rs`) there. Work on it is usually driven from a sicompass
checkout next to this one (`../sicompass`), whose `/commit-and-push`,
`/release`, `/sync` and `/update-cargo` take this repo's name as their first
argument and then follow the skills in this repo's `.claude/skills/`.

It is a sicompass **plugin process**: a program (`src/main.rs`) built with the
SDK's `plugin` feature, which sicompass starts and talks to over its stdin and
stdout. It runs with the user's rights. The Store installs it from this repo's
GitHub releases, one build per platform. The plugin platform is described in
`../sicompass/docs/plugin-platform.md`.

- `plugin.json` is the manifest. Its `name` is `emailclient` and its
  `displayName` `email client` is the settings section (the keys the built-in
  had, so saved values carry over). Permissions, which declare what the plugin
  does and are shown to the user before install: `sockets` on 993 (IMAPS), 465
  (SMTPS) and 587 (SMTP with STARTTLS) of any server (the mail server is the
  user's choice), `allowedHosts` for Google's token and userinfo endpoints,
  and `storage`.
- `locales/<lang>.ftl`, every id prefixed `emailclient-`, in all four
  languages. `src/localize.rs` asks the app (`host::translate`), and in the
  unit tests, which run outside sicompass, reads `en-US.ftl`.
- `src/lib.rs` is the provider (`EmailClientProvider`, `impl Plugin`), and
  `src/main.rs` makes it the program.

## How it works

Every call from the app has a 10-second deadline, after which the app ends the
plugin, and an IMAP round trip can take longer, so the mail servers are never
contacted on a call:

- **The worker** (`worker.rs`) is one thread for the plugin's life, holding the
  one IMAP connection. The UI side sends it `Job`s over a channel and it
  answers `Done`s, JSON both ways. `poll` drains them, and `apply_done` puts
  each answer in the result slot the rendering code already looked in. A
  worker whose thread ended (a panic in a job) is noticed there too
  (`worker_ended`), so nothing waits on it, and the next job starts another.
  Tests that inject a `MockImap` stay on the synchronous path (`bg_enabled`).
- **IDLE** (`idle.rs`) is a thread per watched folder, raising the flag `poll`
  turns into `needs_refresh`. It waits in 10-second rounds and checks between
  them whether it was stopped. A refreshed OAuth token reaches it through a
  shared slot.
- **The Google sign-in** (`oauth2.rs`) runs on a thread of its own: it asks
  the app to run the browser redirect
  (`sicompass_sdk::plugin::desktop::oauth_redirect`, RFC 8252: the app opens
  the browser and listens once on a loopback port), then exchanges the code
  and asks Google for the address. `PendingAuthorize::poll` picks the result
  up. Outside sicompass (the tests) the app is not there, so it fails at once
  without reaching Google.
- **IMAP** is the blocking `imap` 3 crate over `connection::ImapStream`: a
  `std::net::TcpStream` (resolved with `ToSocketAddrs`, connected with
  `connect_timeout`) and rustls with ring, with the webpki roots. `imap://` in
  the clear is refused unless every address is loopback, which only the fake
  server in `fake_imap_tests.rs` is.
- **SMTP** is a few commands by hand in `net.rs` (`Smtp`), implicit TLS for
  `smtps://` and STARTTLS for `smtp://`. lettre only builds the message.
- **The envelope cache** (`cache.rs`) is one JSON file per account in
  `storage_dir()/cache`, opened lazily so only the worker ever opens it. It
  was SQLite. Two tabs are two processes, and the last write wins, which is
  harmless for a copy of what the server has.
- **The sign-in** (OAuth tokens, and the servers and address it filled in) is
  kept in the plugin's storage folder
  (`sicompass_sdk::plugin::storage_dir()/email.json`, in the shape of a
  settings file), since a plugin cannot write the app's settings. The settings
  the manifest declares are read at `init`, and a saved sign-in takes over.
- **HTTP** (Google's endpoints only) goes through `src/http.rs`, a small
  client in the shape of `reqwest::blocking` over `ureq` (rustls with ring and
  bundled roots). A token refresh before a send runs on a call from the app,
  so a request times out after 8 seconds.
- **Undo** entries are `ProviderOp`s: the IMAP action's name and its fields as
  an FFON list (`encode_op`, `decode_op`).
- The tests never reach a real mail server or Google: a fake IMAP server on
  loopback, one-shot HTTP servers on loopback, and the sign-in failing outside
  sicompass.

## Environment (Nix)

The toolchain comes from the flake dev shell in [flake.nix](flake.nix): Rust
from rust-overlay with this computer's plugin target (static musl on Linux,
which nixpkgs' rustc has no std for) and `jq`. Nothing is installed
system-wide.

- **Check once per session**, then stick with the answer: `command -v cargo`.
  - Non-empty: the shell is inside `nix develop`, so run `cargo ...` directly.
  - Empty: prefix every toolchain command with `nix develop -c`.
- `nix develop -c <cmd>` prints a `warning: Git tree ... is dirty` line on
  stderr first. That warning is noise, not a failure.
- Evaluate the flake through `git+file://$PWD`, never a plain path (a plain path
  copies `target/` into the store and hangs), and always under `timeout`.
- The version lives in `plugin.json` and in `[package] version` in `Cargo.toml`.
  Bump both together.

## Generated files that are committed

- `THIRD-PARTY-LICENSES.html`: `cargo about generate about.hbs -o
  THIRD-PARTY-LICENSES.html` (cargo-about 0.9.2, the version the `licenses.yml`
  workflow pins). Regenerate and commit it with any dependency change. The
  workflow fails if it drifts.

## Code Style

Follow standard Rust idioms. Use `#[allow(...)]` sparingly and only when
justified. In `README.md`, do not use em dashes or semicolons. Use commas
instead, or split into separate sentences.

## Testing

- After implementing changes, always run the tests before finishing:
  `cargo test`, and `./scripts/release-plugin.sh --dry-run`, which also builds
  this computer's release and verifies it the way the Store will.
- When adding new code, write or update tests.
- If tests fail, fix the code. Never leave a task with failing tests.

## Test Integrity

- Never remove or weaken test assertions to make a failing test pass. Fix the
  code instead.
- If a test itself is genuinely wrong and needs changing, **ask the user
  first** before modifying it.

## Releasing

A release is a `vX.Y.Z` tag on `main`, equal to `plugin.json`'s version. See
`.claude/skills/release/SKILL.md`. Before tagging, run
`nix develop -c ./scripts/release-plugin.sh --dry-run` (needs the
`sicompass-plugin` tool: `cargo install --git
https://github.com/friendlyflow/sicompass-plugin-sdk sicompass-plugin`). The
release workflow signs with the `PLUGIN_SIGNING_KEY` secret and checks it
against the `PLUGIN_PUBLIC_KEY` variable, the key the sicompass store list
names. The secret key file is `~/.config/sicompass/plugin-keys/emailclient.key`
on the maintainer's machine. Never print, copy or commit it.

The SDK comes from crates.io (the source is `../sicompass-plugin-sdk`). The
commented-out `[patch]` in `Cargo.toml` is for working on them together, and
stays commented on main.

A release has one archive per platform. The release workflow builds them on
five runners (Linux x86_64 and arm64 as static musl, macOS arm64 and x86_64,
Windows x86_64), then packs, signs and verifies them in one job.
