# Contributing

Thank you for helping build Brain for AI.

## Development setup

Install the Rust toolchain pinned by `rust-toolchain.toml`. The same file also
installs `rustfmt` and `clippy`.

Before opening a pull request, run:

```console
cargo fmt --all -- --check
cargo clippy --locked --workspace --all-targets --all-features -- -D warnings
cargo test --locked --workspace --all-features
cargo build --locked --release -p brain-cli --all-features
cargo build --locked --release -p brain-cli --no-default-features
python3 tools/check-licenses.py
```

Keep changes focused, include tests for behavior changes, and update migrations without modifying migrations that have already shipped. Never commit credentials, private memory databases, or machine-specific paths.

All contributions are accepted under the Apache License, Version 2.0.
