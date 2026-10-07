# Working on varyk-sql

This file is for anyone, human or AI agent, making changes to this
repository. [CONTRIBUTING.md](CONTRIBUTING.md) has the details; this is
the short list of rules.

## What this is

varyk-sql is the official SQL package for Varyk: SQLite, Postgres, and
MySQL through sqlx. `src/lib.vr` is everything a program sees,
`src/db.rs` is the only Rust (the facade over sqlx), and `src/tests.vr`
holds the tests. The design is in `docs/specs/`; read it before changing
what the package offers.

## The gate

Run the checks CONTRIBUTING.md lists ("Build and test") before every
pull request: `varyk check`, `varyk test`, rustfmt on `src/db.rs`, and
clippy on the crate `varyk publish --assemble-only` writes. CI's `test`
and `msrv` jobs are required checks: keep those job names, and keep the
code building on Rust 1.85 (no let-chains).

## Rules

- **No crash on absence.** `src/db.rs` never panics: no `unwrap`,
  `expect`, or indexing that can fail. Every failure is a
  `varyk_std::Error`.
- **No secret in an error.** `src/db.rs` never writes the database URL, a
  password, Postgres's "detail" field, a bound value, or a value read
  from a row into a message; only the database's own message text
  passes through.
- **Queries stay literal.** The query text is a literal; never add a
  way to build one from input.
- **Commits.** Conventional prefixes (`feat:`, `fix:`, `docs:`, ...),
  no `<` or `>` in subjects or `BREAKING CHANGE:` footers, and no
  attribution lines (no `Co-Authored-By`, no "Generated with").
- **Docs move with the code.** README.md, CONTRIBUTING.md,
  `docs/specs/`, and the demo's comments are updated in the same change
  whenever what they describe changes: status, versions, install,
  usage, behavior, features, links. Check them against the change
  before every pull request.
