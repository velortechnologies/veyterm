# Reconnect and message-order recovery

- Heartbeats detect a lost connection; the client reconnects automatically.
- Disconnect invalidates pending history reads and clears old streaming activity.
- History is correlated with the session, connection generation and request order.
- Duplicate terminal events cannot revive a completed run.
- Requests have a 30-second acknowledgment deadline. An uncertain chat send is
  reported rather than automatically replayed, to avoid duplicate actions.
- Outgoing messages remain FIFO and use `queueMode: "steer"`.

## Local fault test

```sh
cargo build --release --locked
python3 tests/wss_reconnect.py
```

Requires Python 3, OpenSSL and Unix PTYs. The fixture creates a disposable
pinned-TLS mock Gateway with a dummy token, withholds heartbeat pongs and a
message acknowledgment, and checks reconnection, no replay and a subsequent send.
`VEYTERM_TEST_BINARY` selects an alternative release binary.

These checks do not visually inspect the TUI or prove that all Gateway/model-side
ordering failures are resolved.
