# Contributing to Syncplayer

Syncplayer is still an experimental prototype. Before investing in a large change, open an issue to
confirm that it fits the current architecture and scope. Architecture decisions live in
[`docs/adr/`](docs/adr/).

## Development setup

Install the prerequisites listed in the [README](README.md#prerequisites), then run:

```sh
cargo build
cargo test --locked --all-targets
cargo fmt --all -- --check
cargo clippy --locked --all-targets -- -D warnings
```

All four commands should pass before opening a pull request.

## Pull requests

- Keep changes focused and explain the problem they solve.
- Add or update tests when behavior changes.
- Update user-facing documentation and architecture records when appropriate.
- Do not commit media, build artifacts, credentials, machine-specific paths, or private network
  details.
- Use commit messages that describe the change in the imperative mood.

By contributing, you agree that your contribution will be licensed under the project's
MIT OR Apache-2.0 terms.
