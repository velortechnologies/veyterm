# Overlapping terminal turns

Veyterm sends normal chat messages with explicit `queueMode: "steer"`. Additional
messages steer the active run instead of inheriting the session interrupt policy.
Ctrl-C on empty input or F8 requests cancellation. Server configuration is unchanged.

UI activity is tracked by run ID. An old terminal event or steering acknowledgment
must not finish a newer run, consume its text or erase an optimistic user message
through a delayed history response.

Regression tests cover old aborts during new streaming, delayed history and empty
steering receipts while the original run remains active:

```sh
cargo test --release --locked
```

This is client-side handling, not a guarantee against every server-side concurrency
or long-session issue.
