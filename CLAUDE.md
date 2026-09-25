# Project Instructions

emailclient_plugin_sicompass was split out of the
[sicompass](https://github.com/friendlyflow/sicompass) workspace, and its git
history before that point is the history of `lib/lib_emailclient` (earlier
`lib/lib_emailclient-rs`) there. Work on it is usually driven from a sicompass
checkout next to this one (`../sicompass`), whose `/commit-and-push`,
`/release`, `/sync` and `/update-cargo` take this repo's name as their first
argument and then follow the skills in this repo's `.claude/skills/`.

It is a sicompass **WASM plugin**: a `cdylib` built for `wasm32-wasip2` with
`sicompass-pdk`, installed by the sicompass Store from this repo's GitHub
releases. The plugin platform is described in
`../sicompass/docs/plugin-platform.md` and `../sicompass/docs/wasm-plugins.md`.

- `plugin.json` is the manifest. Its `name` is `emailclient` and its
  `displayName` `email client` is the settings section (the keys the built-in
  had, so saved values carry over). It asks for `sockets` on 993 (IMAPS), 465
  (SMTPS) and 587 (SMTP with STARTTLS) of any server (the mail server is the
  user's choice; the host never lets it reach the local network), approved at
  install, `allowedHosts` for Google's token and userinfo endpoints, and
  `storage`.
- `locales/<lang>.ftl`, every id prefixed `emailclient-`, in all four
  languages.

## The sandbox, and what it changes

A call into the plugin's UI instance has a 10-second deadline, and an IMAP
round trip can take longer, so the network is never touched there:

- **The worker** (`worker.rs`, `worker::WORKER_TASK`) is one long-lived host
  task holding the one IMAP connection. The UI sends it `Job`s through the task
  inbox (`tasks.send`) and it answers `Done`s with `tasks.emit`, JSON both ways.
  `apply_done` puts an answer in the result slot the rendering code already
  looked in. Natively (the tests) the same loop is a thread with channels.
  Tests that inject a `MockImap` stay on the synchronous path (`bg_enabled`).
- **IDLE** (`idle.rs`, `idle::IDLE_TASK`) is its own task, emitting `changed`.
  A refreshed OAuth token reaches it through its inbox.
- **The Google sign-in** (`oauth2.rs`, `oauth2::OAUTH_TASK`) is a task that
  asks the host to run the browser redirect (`desktop.oauth-redirect`: a plugin
  cannot listen), then exchanges the code. Natively the tests play the browser
  against a loopback listener.
- **IMAP** is the blocking `imap` 3 crate over `connection::ImapStream`: a host
  socket (`sockets.resolve`, then `std::net::TcpStream`) and rustls with ring,
  with the webpki roots. `imap://` in the clear is refused unless every address
  is loopback, which only the fake server in `fake_imap_tests.rs` is.
- **SMTP** is a few commands by hand in `net.rs` (`Smtp`), implicit TLS for
  `smtps://` and STARTTLS for `smtp://`. lettre only builds the message.
- **The envelope cache** (`cache.rs`) is one JSON file per account in
  `/storage/cache`, opened lazily so only the worker ever opens it. It was
  SQLite, which needs C emulation libraries to build for WASI and has no file
  locks there.
- **The sign-in** (OAuth tokens, and the servers and address it filled in) is
  kept in the plugin's storage folder (`/storage/email.json`, in the shape of a
  settings file), since a plugin cannot write the app's settings. The settings
  the manifest declares are read at `init`, and a saved sign-in takes over.
- **HTTP** (Google's endpoints only) goes through `src/http.rs`: the host's
  `net.fetch` in the sandbox, reqwest natively.
- **Undo** entries are `ProviderOp`s: the IMAP action's name and its fields as
  an FFON list (`encode_op`, `decode_op`).

## Environment (Nix)

The toolchain comes from the flake dev shell in [flake.nix](flake.nix): Rust
from rust-overlay with the `wasm32-wasip2` target (nixpkgs' rustc has no `std`
for it), `wasm-tools`, `jq`, and clang for ring's C (`CC_wasm32_wasip2`: the
host's gcc cannot target wasm, and ring needs no libc headers). Nothing is
installed system-wide.

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
  `cargo test` (natively), and `./scripts/release-plugin.sh --dry-run`, which
  also builds the component and audits its imports.
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

The SDK and the pdk come from crates.io (the source is
`../sicompass-plugin-sdk`). The commented-out `[patch]` in `Cargo.toml` is for
working on them together, and stays commented on main.
