# Contributing to Runtimo

## Getting Started

1. Clone the repository: `git clone https://github.com/moeshawky/runtimo.git`
2. Build: `cargo build --workspace`
3. Test: `cargo test --workspace`

## Development Standards

- **Rust Edition 2021**, MSRV **1.70.0**
- `cargo fmt --check` must be clean before commits
- `cargo clippy --all-targets` must have zero new warnings
- `cargo test --workspace` must be green
- Conventional Commits format: `type(scope): description`

## Testing

```bash
cargo test -p runtimo-core --lib          # unit tests
cargo test -p runtimo-core --test integration   # integration tests
cargo test -p runtimo-core --test robust        # property-based tests
cargo test -p runtimo-core --doc           # doc tests
```

## Release Process

See [CHANGELOG.md](CHANGELOG.md) for version history. Releases follow semantic versioning.

## License

MIT — see [LICENSE](LICENSE).
