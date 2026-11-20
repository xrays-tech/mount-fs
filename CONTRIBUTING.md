# Contributing

Thanks for your interest in `mount-fs`.

## Development setup

- Linux host with FUSE support.
- Rust toolchain (stable, 1.85 or newer). Install via
  [rustup](https://rustup.rs/):

  ```sh
  rustup toolchain install stable
  rustup component add rustfmt clippy
  ```

## Build, format, lint, and test

```sh
cargo build
cargo fmt --all
cargo clippy --all-targets -- -D warnings
cargo test
```

All four must pass before a change is ready to submit. CI runs the same checks
(`cargo fmt --all -- --check`, `cargo clippy --all-targets -- -D warnings`,
`cargo build`, `cargo test`) plus `cargo-deny`, so keep the tree clean.

## Commit style

Use short, imperative subject lines (for example, "Add doctor engine check").
Keep commits focused and describe the motivation in the body when it is not
obvious. Reference milestone identifiers (for example `M1`) where relevant.

## License

By contributing you agree that your contributions are licensed under the MIT
License (see [LICENSE](LICENSE)).
