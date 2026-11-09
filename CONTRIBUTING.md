<!-- SPDX-License-Identifier: Apache-2.0 -->

# Contributing to Castellum

Thanks for considering a contribution. This is a small project run by one
maintainer, so a short, focused change is the easiest to review.

## Before you start

- Open an issue first for anything bigger than a bug fix or a small improvement,
  so we agree on the direction before you spend time on it.
- By submitting a contribution you agree it is licensed under the Apache License 2.0
  (section 5 of the licence), the same as the rest of the project.
- Be kind: see [CODE_OF_CONDUCT.md](CODE_OF_CONDUCT.md).
- Security problems go through [SECURITY.md](SECURITY.md), never a public issue.

## Setup

Follow "Build and run" and "Tests" in [README.md](README.md). You need Rust, Xcode
and `xcodegen`. Use a throwaway mail account or the local GreenMail server for
testing; never put real credentials or real mail in a commit, fixture or issue.

## Making a change

1. Fork, then branch from `main`.
2. Keep each commit to one logical change, with a
   [Conventional Commits](https://www.conventionalcommits.org) message,
   for example `fix(imap): handle a bare LF in a literal`.
3. Add or update tests. Rust: `cargo test --manifest-path native/Cargo.toml`.
   Swift: the `Castellum` scheme's tests.
4. Run `cargo fmt --all` and `cargo clippy --all-targets -- -D warnings` in `native/`.
5. New source files start with an SPDX header:
   `// SPDX-License-Identifier: Apache-2.0` and a copyright line.
6. Open a pull request describing what changed and why, and how you tested it.

## Ground rules

- Local-first: no new network call, telemetry or third-party service without
  an issue and a clear opt-in.
- Fixtures must be synthetic: use `example.com`, `example.org` or `.test` addresses.
- Do not commit secrets, signing material, Apple Team IDs or personal data.
