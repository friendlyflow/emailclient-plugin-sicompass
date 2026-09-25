# emailclient-plugin-sicompass

*Email, in Sicompass.*

This plugin is part of [Sicompass](https://github.com/friendlyflow/sicompass), a
keyboard-first, accessibility-first way to use your entire computer.

The email client shows your mail folders as a list. Right enters a folder, its
messages are rows, and right on a message reads it. Reply, reply all and
forward are rows under the message, and compose writes a new one. Marking read
or unread, starring, deleting, archiving and moving can be undone with ctrl+z.

Sign in with Google on the first screen, after setting a client ID and client
secret in Settings, under email client. New mail arrives in the background.

The email client asks to reach any mail server on the internet, on the ports
mail uses (993, 465 and 587), because your mail server is yours to choose. It
never reaches your own computer or local network. The Store shows that before
you install it, and installing it is your approval.

## Install

In Sicompass, open store, then programs, and press Enter on install next to
emailclient. The Store checks the release's signature before installing it,
and keeps it up to date.

## Building from source

```bash
nix develop          # the toolchain, with the wasm32-wasip2 target
cargo test           # natively, against a fake IMAP server
cargo build --release --target wasm32-wasip2
cp target/wasm32-wasip2/release/emailclient_plugin.wasm plugin.wasm
```

`./scripts/release-plugin.sh --dry-run` does the build, checks the component
against `plugin.json`, and signs and verifies it with a throwaway key, the way
a release is made.

## Related repositories

- [sicompass](https://github.com/friendlyflow/sicompass), the application
- [sicompass-plugin-sdk](https://github.com/friendlyflow/sicompass-plugin-sdk),
  the SDK, the WASM plugin kit and the cloud backup library

## Community

Join the conversation on
[Discord](https://discord.com/channels/1464152138753249313/1464152139231137894).

## License

#### Open source license

If you are creating an open source application under a license compatible with
the GNU GPL license v3, you may use this project under the terms of the GPLv3.
See [LICENSE](LICENSE).

## Contributing

Contributions are welcome. Whether it is code, documentation, or feedback, your
input helps make computing more accessible for everyone.
