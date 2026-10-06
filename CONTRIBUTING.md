# Contributing

varyk-sql is the official SQL package for Varyk. Varyk is experimental
and pre-1.0: anything may change, and a new feature or a breaking change
bumps the minor version. Contributions are welcome; small, focused pull requests are the
easiest to review. The project has one maintainer, so reviews are best
effort and a pull request may wait a while; a reminder after two weeks
is welcome.

## Build and test

You need a stable Rust toolchain through [rustup](https://rustup.rs),
a C compiler for SQLite (Xcode's command-line tools, `build-essential`),
and `varyk` (`cargo install varyk --version '^0.7' --locked`, the
latest 0.7 release, as CI uses). The package is a Varyk package
(`src/lib.vr`), so it is built only by `varyk`; plain `cargo build`
does not work here. Before opening a pull request, run the same checks
CI runs:

```sh
varyk check
varyk test
rustfmt --edition 2024 --check src/db.rs
cd "$(varyk publish --assemble-only)"
cargo clippy --all-targets -- -D warnings
```

`varyk publish --assemble-only` writes the plain Rust crate the package
publishes as, and prints its folder; clippy runs there.
`.github/workflows/ci.yml` has the exact steps.

The tests run on an in-memory SQLite database unless `DATABASE_URL`
names another. CI also runs them on Postgres and MySQL: `varyk test`
takes no `--features`, so for a server add its driver to the
manifest's `default = ["sqlite"]` line for the run (keep `sqlite`: one
test always uses it), and set `RUST_TEST_THREADS=1`, since the tests
share the server's database. The `Test on Postgres` and `Test on MySQL`
steps of `.github/workflows/ci.yml` show the URLs.

The minimum supported Rust version is 1.85 and CI checks it: no
let-chains or other later features.

Every Monday, and on demand from the Actions tab, the "Latest varyk"
workflow builds and tests varyk-sql with the newest varyk on crates.io,
moving `varyk-std` to its version for that run. It is not a required
check; a red run means a new varyk needs a varyk-sql release.

## Where things are

- `src/lib.vr` is everything a program sees; `src/db.rs`, the facade
  over sqlx, is the only Rust. It never panics: no `unwrap`, `expect`,
  or indexing that can fail, and every sqlx error becomes a
  `varyk_std::Error` whose message names no URL and no database
  "detail" field.
- `src/tests.vr` holds the tests, as a module: a Varyk package may not
  have a `tests/`, `examples/`, or `benches/` directory. Each test reads
  `DATABASE_URL` and uses `sqlite::memory:` when it is not set.
- `docs/specs/` holds the design; read it before changing what the
  package offers.

Rust code follows Varyk's
[AGENTS.md](https://github.com/Varyk-Lang/varyk/blob/main/AGENTS.md):
the smallest change that works, failures with plain-word messages, and
safe by default.

## Commit messages and releases

Releases are cut by
[release-please](https://github.com/googleapis/release-please) from the
commit history on `main`, so every commit that lands on `main` carries a
conventional prefix. A pull request with one concern is squash-merged
with a conventional title; a pull request with several concerns is
rebase-merged with one conventional commit per concern.

| Prefix | Use it for | Effect on the release |
|---|---|---|
| `feat:` | a new capability | bumps the version, listed in the changelog |
| `fix:` | a bug fix | bumps the version, listed in the changelog |
| `docs:` | documentation only | no bump, not listed |
| `test:` | tests only | no bump, not listed |
| `ci:` | workflows and release automation | no bump, not listed |
| `chore:` | maintenance, dependencies, manifests | no bump, not listed |
| `refactor:` | code change with no behavior change | no bump, not listed |

A `!` after the prefix (`feat!:`) marks a breaking change. Before 1.0,
`feat` and a breaking change bump the minor version and `fix` bumps the
patch version.

Keep `<` and `>` out of commit subjects and `BREAKING CHANGE:` footers:
write `Option of T`, not `Option<T>`. release-please copies them into
the release pull request and reads that body as HTML, where an unclosed
`<T>` hides what follows it. The `Commit messages` check enforces this
on every pull request.

## Licensing of contributions

Unless you explicitly state otherwise, any contribution intentionally
submitted for inclusion in varyk-sql by you, as defined in the
Apache-2.0 license, shall be dual licensed under MIT or Apache-2.0,
without any additional terms or conditions. The name "Varyk" is a
trademark and is not covered by those licenses; see Varyk's
[TRADEMARKS.md](https://github.com/Varyk-Lang/varyk/blob/main/TRADEMARKS.md).

## Reporting bugs

Open an issue with the smallest Varyk program that shows the problem,
the database and its version, and the error message. For security
problems, see the security policy shared by every Varyk-Lang
repository:
<https://github.com/Varyk-Lang/varyk-sql/security/policy>.
