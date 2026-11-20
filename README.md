# Veyterm

A terminal for autonomous agents. A Rust + Ratatui client for OpenClaw Gateway.

## Build and run

```sh
git clone https://github.com/velortechnologies/veyterm.git
cd veyterm
cargo build --release
export VEYTERM_TOKEN='your-gateway-token'
./target/release/veyterm gateway.example:18789
```

The endpoint is a positional argument; `wss://` is implied. Only encrypted
WebSocket connections are supported. Authentication reads `VEYTERM_TOKEN`, not
command-line arguments. Run `veyterm --help` for all options.

For a private or self-signed Gateway, use `--tls-sha256 HEX` with the SHA-256
fingerprint of its DER leaf certificate, obtained through a trusted channel.
Normal connections verify the certificate against public roots. `--tls-insecure`
explicitly disables certificate trust and hostname checks; use it only for
controlled testing. It cannot be combined with `--tls-sha256`.

## Device pairing

Veyterm generates an Ed25519 identity at `~/.config/veyterm/device.key` with mode
0600. On first connection, approve the pending device on your Gateway:

```sh
openclaw devices list
openclaw devices approve REQUEST_ID
```

Then restart Veyterm. It requests `operator.read` and `operator.write`, not admin
access. The environment token remains the authentication source; tokens returned
by the Gateway are not persisted.

## Features

- Searchable session picker, history, streaming replies and live tool-call arguments.
- Model, thinking and reasoning controls, slash-command completion and file downloads.
- Context usage and provider quota display, refreshed periodically.
- Automatic reconnect and heartbeat detection; uncertain sends are not silently replayed.
- Additional messages steer the active run; cancellation is explicit.
- Optional NDJSON conversation journals and snapshots.

Select a session directly with `veyterm HOST --session NAME_OR_ID` (or `-s`).
Names must be exact and unique; full session keys and IDs are also accepted.

## Keyboard shortcuts

- Arrow keys and Enter: navigate and select a session.
- Ctrl-C on empty input or F8: cancel the current run.
- Ctrl-Q: quit.
- Esc: clear search, then return to the session picker.

## Journals and snapshots

`veyterm HOST -s SESSION --log session.ndjson` appends messages, user commands,
assistant tool calls and connection events from this invocation. It does not
backfill history or record authentication, binary attachments, polling or tool
results. New journal files use mode 0600. Tool arguments and conversation text
can contain sensitive data; treat journals as private.

`/save FILENAME` overwrites a snapshot of the currently loaded conversation in
NDJSON format. It works offline and does not fetch older history.

## Development

```sh
cargo check --locked
cargo test --release --locked
cargo build --release --locked
python3 tests/wss_reconnect.py
```

The reconnect fixture requires Python 3, OpenSSL and Unix PTYs. It runs a local
mock Gateway, not a production server. See [reconnect behavior](docs/reconnect.md)
and [overlapping turns](docs/overlapping-turns.md) for protocol details and limits.
