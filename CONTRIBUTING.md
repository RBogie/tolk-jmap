# Contributing to Tolk

Thanks for helping. Bug reports, test results from mail apps and servers
nobody has tried yet, and pull requests are all welcome.

## Reporting

- **Bugs and feature requests:** open an issue; the templates ask for what
  helps most (mail app, EAS version, Tolk version, JMAP server).
- **A mail app or JMAP server we have not tested:** work through
  [docs/testing-a-client.md](docs/testing-a-client.md) and report the result
  with the "Client test report" template, whether it works or not.
- **Security problems:** not in a public issue; see [SECURITY.md](SECURITY.md).

## Building and testing

The Rust version is pinned in `rust-toolchain.toml`; `rustup` installs it on
the first `cargo` command. Before opening a pull request, run what CI runs:

```sh
cargo fmt --all --check
cargo clippy --all-targets --locked -- -D warnings
cargo test --locked
cargo deny --locked check   # cargo install cargo-deny
```

`cargo test` needs no JMAP server: the integration tests drive the real
router against an in-process fake (`tests/support/fake_jmap.rs`). To try a
change against a real server, see [scenarios/README.md](scenarios/README.md).
The [README](README.md#development) has a map of the code.

## Pull requests

- Keep a pull request to one change, with tests for what it fixes or adds.
- For a change users notice, add a line under `## Unreleased` in
  [CHANGELOG.md](CHANGELOG.md), written for users rather than developers.
- Comments explain why the code is the way it is, where the code itself does
  not show it; not what changed or why in this pull request (that belongs in
  the description).
- CI must pass: formatting, Clippy, tests, `cargo deny`, the image build for
  every platform and CodeQL. Pull requests are merged with a merge commit.
- Signed commits are welcome but not required.

## License

Tolk is licensed under the [GNU AGPL v3.0](LICENSE) (`AGPL-3.0-only`). By
contributing, you agree that your contribution is licensed the same way.
