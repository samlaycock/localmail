# Contributing to localmail

Thanks for helping out. Bug reports, provider fixes (a provider whose real API answers differently from ours) and new features are all welcome. For anything large, please open an issue first so we can agree on the approach.

## Development

You need a recent stable Rust toolchain. Node.js is needed for changesets.

```sh
cargo run                        # HTTP on http://localhost:9402, SMTP on localhost:2525
cargo run -- --in-memory         # the same, starting with an empty inbox
cargo test                       # unit tests and integration tests for every provider, SMTP and the dev API
cargo clippy --all-targets -- -D warnings
cargo fmt
docker build -t localmail .      # the release image
scripts/docker-smoke.sh localmail
```

CI runs `cargo fmt --check`, clippy with `-D warnings`, `cargo test` and `cargo audit` on every pull request.

- The pages share `assets/app.js` and `assets/app.css`. Both are compiled into the binary, so rebuild after changing them.
- Each provider API is one file in `src/providers/`. It reads its provider's request into `Draft`s, or into the provider's validation error. It then hands them to `send::submit`, which applies scenario addresses and stores the messages, and answers in its provider's format. Error bodies and messages should match what the real API returns. Cite the provider's documentation in the pull request.
- SMTP is `src/smtp.rs`, scenario addresses are in `src/scenarios.rs`, and code and link extraction is in `src/extract.rs`.
- Messages are kept as MIME (`src/message.rs` builds it for API sends) in SQLite (`src/store.rs`).

## Changesets

Every pull request needs a changeset: a short Markdown file under `.changeset/` that says what changed and whether it's a `patch`, `minor` or `major` change. It becomes the changelog entry.

```sh
npm ci
npm run changeset                # describe a change users will notice
npm run changeset -- --empty     # for CI, tests, docs and other invisible changes
```

CI fails if a pull request has no changeset.

## Releases

Releases are managed with [changesets](https://github.com/changesets/changesets). `package.json` holds the version, and `npm run version` copies it into `Cargo.toml` and `Cargo.lock`.

1. Changesets land on `main` with their pull requests.
2. The release workflow keeps a "Release <version>" pull request open. It applies the pending changesets: the version bump and `CHANGELOG.md`. CI runs on it, including a Docker build and smoke test.
3. Merging that pull request does the release:
   - builds the image natively for amd64 and arm64;
   - pushes `ghcr.io/samlaycock/localmail:<version>`, `:<major>.<minor>` (and `:<major>` from 1.0) and `:latest`;
   - tags `v<version>` and creates the GitHub release from the changelog.

## License

By contributing, you agree that your contributions will be dual licensed under the MIT and Apache-2.0 licenses, as described in the [README](README.md#license).
