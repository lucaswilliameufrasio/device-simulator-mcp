# Contributing

## Development

Use Rust and Node versions declared by the project. Initialize new Rust
dependencies with `cargo add` and keep `Cargo.lock` committed.

Run the complete local checks before opening a pull request:

```bash
cargo fmt --all --check
cargo clippy --locked --all-targets --all-features -- -D warnings
cargo build --locked --all-targets
cargo test --locked --all-targets
pnpm --dir docs-site install --frozen-lockfile
pnpm --dir docs-site build
```

## Commits

Use Conventional Commits with English subjects, for example:

```text
feat: add Android emulator capture
fix: preserve spaces in typed text
docs: explain private release downloads
```

## Releases

Releases follow the Acari workflow. Run `Prepare Release` from `main`, merge
the generated release PR, then push the resulting `vX.Y.Z` tag.
