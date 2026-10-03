# varyk-sql: the database package

Date: 2026-10-03.

**Status.** Design, not yet implemented. `varyk-sql` is the official
Varyk package for SQL databases, written in Varyk with a facade over
sqlx. It needs the facade features of Varyk milestone 5b3
(`Varyk-Lang/varyk`, `docs/specs/2026-10-03-milestone-5b3-design.md`,
"M5b3 §n") and is built after the Varyk release that ships them. Varyk
is experimental and pre-1.0: anything here may change.

## 1. Summary and scope

A program adds the package with `varyk add sql`, and then

```varyk
struct Config {
    database_url: string,
}

struct User {
    id: i64,
    name: string,
}

async fn load() -> Result<Vec<User>, Error> {
    let config: Config = env::parse()?;
    let db = sql::connect(config.database_url).await?;
    db.migrate("migrations").await?;
    db.run("insert into users (name) values (?)", "Ada").await?;
    db.all("select id, name from users").await
}

async fn main() {
    match load().await {
        Ok(users) => {
            for user in users {
                println!("{} {}", user.id, user.name);
            }
        }
        Err(e) => log::error("{}", e.message()),
    }
}
```

(`main` returns nothing in Varyk, so the work is in `load` and `main`
matches on it; `all` takes its `T` from `load`'s return type.)

is a working service against SQLite, Postgres, or MySQL, chosen by the
URL. The package aims at what an ordinary production service does with a
database and nothing more: connect over TLS, run migrations, query, write
inside transactions, log, and test against an in-memory SQLite.

### 1.1 Decisions

- **sqlx, through its `Any` driver.** One pool type, `AnyPool`, serves all
  three databases, picked from the URL scheme at run time. Every item of
  the facade is then free of `#[cfg]`, which Varyk does not import, and a
  program compiled with two drivers chooses between them by configuration
  alone. The cost is that `Any` decodes a fixed set of column types
  (section 2.6); other types are cast in SQL.
- **Scalars only as values**, by M5b3 §2.2; one query form, with the
  database's own placeholders passed through.
- **Migrations at run time**, from a folder, with sqlx's own file format
  and history table, so `sqlx migrate run` from CI works on the same
  folder. Embedding the files in the executable needs sqlx's `migrate!`
  macro, which resolves paths inside this crate, not the program's, and
  waits for a compiler feature (section 8).
- **One pool setting**, `max_connections`, through `connect_with`.
  Everything else is sqlx's default.
- **Its own repository and release**, so a sqlx or driver change does not
  wait for a compiler release and the other way round.

## 2. Surface

Everything a program sees is in `src/lib.vr`:

```varyk
pub mod db;
pub use db::connect;
pub use db::connect_with;
pub use db::Pool;
pub use db::Tx;
```

Named with the key `sql` (`varyk add sql`), the items are `sql::connect`,
`sql::connect_with`, `sql::Pool`, and `sql::Tx`. The facade `src/db.rs`
declares them with the shapes of M5b3 §2.

### 2.1 Connecting

| Call | Gives | Does |
|---|---|---|
| `sql::connect(url)` | `Result<Pool, Error>` | opens a pool on the database the URL names, with sqlx's defaults (10 connections, 30 s to acquire one) |
| `sql::connect_with(url, max_connections)` | `Result<Pool, Error>` | the same, with at most `max_connections` (a `u32`, at least 1) connections |

`url` is a `string`, usually read from `DATABASE_URL` with `env::parse`.
`connect_with(url, 0)` is an `Error`, since `varyk check` cannot refuse
it.
The scheme picks the driver: `sqlite:`, `postgres:`, or `mysql:`. A URL
for a driver the program was not built with, a URL that does not parse,
and a database that cannot be reached are each an `Error`; none of the
messages contains the URL (section 3).

`Pool` is a handle: `db.clone()` is another handle to the same pool, for
a started task that needs one (M5b1 §2.3). `Shared<T>` for handlers is
milestone 5b4's business.

### 2.2 Migrations

| Call | Gives | Does |
|---|---|---|
| `db.migrate(folder)` | `Result<bool, Error>` | applies, in order, every `.sql` file in `folder` not yet recorded in the database's `_sqlx_migrations` table, and gives `true` (Varyk has no `()`, M5b3 §2.4; sqlx's migrator reports no count) |

`folder` is a `string`, a path relative to the working directory, usually
`"migrations"`; it may come from configuration. Files are named `<version>_<name>.sql`,
sqlx's format, so the same folder works with `sqlx migrate add` and
`sqlx migrate run`. sqlx takes a database lock on Postgres and MySQL
while applying, so several replicas starting together apply each
migration once; SQLite has one writer. A failed migration is an `Error`
naming the file's version and the database's message. Down migrations
(`.down.sql` files) are ignored.

### 2.3 Queries

On a `Pool` and on a `Tx`:

| Call | Gives | Does |
|---|---|---|
| `db.one(query, values..)` | `Result<T, Error>` | the first row, read as `T`; an `Error` when there is none |
| `db.first(query, values..)` | `Result<Option<T>, Error>` | the first row, or `None` |
| `db.all(query, values..)` | `Result<Vec<T>, Error>` | every row |
| `db.run(query, values..)` | `Result<u64, Error>` | runs a statement; the number of rows it changed |

`query` is literal text (M5b3 §2.3): a query built from input does not
compile, which is the package's guard against SQL injection. The values
go after it, as many as the query has placeholders (M5b3 §2.2):
`bool`, `string`, floats, integers up to `i64`, and `Option` of each.
The placeholders are the database's own, `?` for SQLite and MySQL, `$1`
for Postgres, passed through unchanged; a program that targets two
databases keeps two queries. Before running, the shared binding function
compares the number of values with the number of placeholders the
prepared statement reports, and a mismatch is an `Error` naming both
counts; should a driver report no count, the check is skipped and the
database's own applies. The check is the package's because Postgres and
MySQL reject a mismatch themselves, but SQLite binds a missing value as
`NULL` and ignores an extra one, which would be wrong data with no
error.

`T` is chosen from where the result goes (M5b3 §2.1): `let user: User =
db.one(..).await?`. A handler that answers "not found" uses `first` and
matches on `None`; `one` is for a row that must exist.

`one` reads at most one row: it adds no `limit`, so a query that can
match many rows should say `limit 1` itself.

### 2.4 Transactions

| Call | Gives | Does |
|---|---|---|
| `db.begin()` | `Result<Tx, Error>` | starts a transaction on one connection of the pool |
| `tx.one`, `tx.first`, `tx.all`, `tx.run` | as on a pool | inside the transaction; each is a `mut self` method, so `tx` is `let mut` |
| `tx.commit()` | `Result<bool, Error>` | a `mut self` method: commits and gives `true` (Varyk has no `()`, M5b3 §2.4); a second `commit`, or a query after one, is an `Error` "this transaction is finished" |

A `Tx` dropped without `commit` rolls back, which sqlx does when the
transaction is dropped. There is no `rollback` call: return early, with
`?` or otherwise, and the drop rolls back. Nested `begin` on a `Tx`
(savepoints) is not offered.

### 2.5 Reading rows

A row is read into `T` by column name through a small serde deserializer
over sqlx's row: each field of `T` takes the column of the same name
(`#[rename]` on the field changes it, M5a §2.2), columns `T` has no field
for are ignored, and a field without a column is an `Error` naming the
field, unless it has `#[default]` or is skipped. Every selected column
must be of a type the driver decodes (section 2.6), whether or not `T`
reads it: sqlx's `Any` converts the whole row before the deserializer
sees it, so `select *` on a table with a `timestamptz` column fails
even when `User` has no such field. Name the columns, and cast the
others. A `NULL` goes into an
`Option` field as `None` and into any other field as an `Error` naming
the column.

When `T` is a number, `bool`, or `string`, the row must have exactly one
column, so `let n: i64 = db.one("select count(*) from users").await?`
works, and so does `Option` of a scalar for a column that may be `NULL`
(`let oldest: Option<i64> = db.one("select max(age) from users")`).
`Vec` of a scalar, and a struct holding a struct, are not readable from a
row (an `Error`); a nested type has no column.

### 2.6 Column types

sqlx's `Any` driver decodes booleans, integers, floats, text, and bytes
(which Varyk cannot hold), and encodes the same. The drivers do not
agree on kinds (SQLite reports every integer as a 64-bit one and every
float as a double, and has no boolean kind), so the deserializer applies
one rule for all three: a number field accepts any integer or float
column whose value fits the field's type, and is an `Error` naming the
column when it does not; a `bool` field accepts a boolean column and an
integer `0` or `1`; a `string` field accepts text. Any other column
type (`uuid`, `timestamp`, `numeric`, `json`, `bytea`, arrays) is cast in
the query, in the database's own syntax: `select id::text from ..` on
Postgres, `select cast(id as char) from ..` on MySQL. Varyk has no
date, uuid, or bytes type yet; when it does, this list grows.

## 3. Errors

Every failure is a `varyk_std::Error` (M5b3 §2.4) whose message
`varyk-sql` writes:

- a database error carries the database's own message (which names a
  constraint or column where there is one) and never its "detail" field,
  which on Postgres may hold the row's values;
- a connection failure says which step failed (parsing the URL, the
  driver, reaching the database) and never includes the URL, so a
  password cannot reach a log through an error;
- a row-reading failure names the field or column (section 2.5);
- a finished transaction, an empty `one`, and a missing driver each have
  a fixed message.

No call panics: every sqlx `Result` is mapped, and the deserializer
returns errors.

## 4. Production use

What a service does with the package, each either covered or written
down in the README:

- **Configuration.** `DATABASE_URL` through `env::parse` into the
  program's config struct, as in section 1; the README shows it and the
  `.env` form for development.
- **TLS.** sqlx is built with its `tls-rustls` feature (ring and webpki
  roots in sqlx 0.8), so
  `sslmode=require` on Postgres and `ssl-mode=REQUIRED` on MySQL work in a
  minimal container with no CA bundle, and `varyk build --release` stays
  one native executable with no OpenSSL. The README shows the URL forms.
- **Migrations.** `db.migrate("migrations")` at startup, or `sqlx migrate
  run` from CI on the same folder; a `Dockerfile` copies `migrations/`
  beside the executable. The README has the `Dockerfile` lines.
- **Logging.** sqlx logs each statement at debug level through `tracing`,
  which `varyk-std` sets up when the program logs (M5a §2.6): the SQL
  text and the timing, never the values. Nothing to build.
- **Tests.** `sql::connect_with("sqlite::memory:", 1)` in an async
  `#[test]` under `varyk test`; a test returns nothing, so it opens the
  `Result`s with `match` and `assert`. sqlx gives each `:memory:` pool
  one shared database, alive while the pool is, so the database lives
  for one test; one connection keeps the test's queries serial, so no
  two of them contend for SQLite's single writer. With one, a query on
  the pool while a `Tx` is open waits for the transaction's connection
  and fails after sqlx's acquire timeout, so a test finishes the
  transaction first. The README says all of this.
- **Health.** `db.run("select 1")` as the readiness check.
- **Building.** The default `sqlite` feature compiles SQLite's C source
  on the first build and needs a C compiler (Xcode's command-line tools,
  `build-essential`); the README says so and notes the first build takes
  a few minutes; a service on Postgres or MySQL alone adds the package
  with `varyk add sql --no-default-features --features postgres` and
  skips it.
- **Versions.** The package depends on `varyk-std` with a minor-version
  requirement; a program and the package must resolve to one `varyk-std`
  or the generated Rust has two `Error` types. Each breaking Varyk release
  is followed by a `varyk-sql` release, and the README keeps a table of
  which `varyk-sql` works with which `varyk`.

## 5. Package layout

```text
varyk-sql/
  Cargo.toml
  src/lib.vr          pub mod db; mod tests; pub use ..
  src/db.rs           the facade: Pool, Tx, connect, connect_with, row reading, errors
  src/tests.vr        the Varyk #[test]s of section 6
  migrations/         the test schema
  demo/users/         the users service of section 1, a Varyk program with
                      sql = { package = "varyk-sql", path = "../.." } and
                      its own migrations/
  docs/specs/         this file
  README.md, LICENSE-MIT, LICENSE-APACHE, CONTRIBUTING.md, SECURITY.md
  .github/workflows/  ci.yml, release-please.yml, commit-messages.yml
  release-please-config.json, .release-please-manifest.json
```

No `tests/`, `examples/`, or `benches/` directory: a Varyk package may
not have them (V0401), so the tests are a module and the demo lives
under `demo/`.

`Cargo.toml`:

- `[lib] path = "src/lib.vr"` (M5b2 §1.2); `edition = "2024"`,
  `rust-version = "1.85"`, dual license, `repository`, `keywords`, and
  four more that the first `cargo publish` needs or crates.io shows:
  `description` (required, or publishing fails), `homepage =
  "https://varyk.com"`, `categories = ["database"]`, and `authors =
  ["Vlad Mickevic"]`;
- `[dependencies]`: `varyk-std` (minor-version requirement), `sqlx` 0.8
  with `default-features = false` (its defaults pull in the macros,
  which nothing here uses) and the features `runtime-tokio`, `any`,
  `migrate`, and `tls-rustls`, and
  `serde` for the deserializer (`varyk-std` re-exports serde for
  signatures; the deserializer's impls are easier against the crate
  directly);
- `[features]`: `default = ["sqlite"]`, `sqlite = ["sqlx/sqlite"]`,
  `postgres = ["sqlx/postgres"]`, `mysql = ["sqlx/mysql"]`; a program
  adds a driver with `varyk add sql --features postgres`, and `varyk`
  passes features through (M5b2 §4.3).

`src/db.rs` holds, in order: a call to sqlx's `install_default_drivers`,
which `Any` needs, at the top of `connect` (sqlx guards it with a `Once`,
so repeated calls are harmless); `Pool` wrapping `sqlx::AnyPool`, with
`#[derive(Clone)]` so Varyk's `db.clone()` works (M4 §2.10), and `Tx`
wrapping `Option<sqlx::Transaction<'static, sqlx::Any>>`; the four
query methods on each, sharing one binding function over `Value`; the
row deserializer; and the error mapping of section 3. Published, it is
an ordinary crate (M3 §2.6) with `build = false` and the generated
`src/lib.rs`, so a Rust program can use it too.

## 6. Testing and definition of done

- `varyk test` in the package, async tests in `src/tests.vr` on
  `connect_with("sqlite::memory:", 1)`: the schema applied by `migrate`,
  each of
  `one`, `first`, `all`, and `run` with zero and several values; `one` on
  no row; `first` giving `None`; a scalar `T`; a `NULL` into an `Option`
  and into a plain field; a missing column; `#[rename]`; a transaction
  committed, one dropped, and `commit` twice; `connect_with(url, 1)`;
  a URL for a missing driver, a bad URL, and an unreachable database each
  giving an `Error` without the URL in its message; too few and too
  many values, each an `Error`; a second `migrate` succeeding with the
  row count of `_sqlx_migrations` unchanged (read with `one`).
- The same tests against Postgres and MySQL, selected by `DATABASE_URL`,
  with the two placeholders styles.
- `demo/users` builds and prints the expected output under `varyk run`
  on SQLite.
- CI, in two jobs named `test` (stable) and `msrv` (1.85), the names the
  repository's branch ruleset requires: `varyk check` and the tests on
  SQLite in both; in `test` only, the tests on Postgres and MySQL
  service containers (the 1.85 build gains nothing from them), `rustfmt
  --check
  src/db.rs`, and `cargo clippy` in the crate `varyk publish
  --assemble-only` writes (plain cargo cannot build a package whose
  target is `src/lib.vr`, M5b2 §1.2); `cargo package` on that assembled
  crate loads as a dependency (M5b2 §4.4).
- No `unwrap`, `expect`, or other crash-on-absence call in `src/db.rs`.
- README: install, the fifteen-minute example, configuration, TLS,
  migrations with the `Dockerfile` lines, transactions, the column-type
  table with the cast rule, testing, and the version table.

Done when CI is green, the first release is on crates.io, and the roadmap
item in `Varyk-Lang/varyk` is checked.

## 7. Repository and release

Mirrors `Varyk-Lang/varyk`:

- release-please with `release-type: rust`, one package at the root,
  `bump-minor-pre-major` and `bump-patch-for-minor-pre-major`; the
  release workflow is `varyk`'s with one step changed: where `varyk`'s
  runs `cargo publish`, this one installs `varyk` and runs `varyk
  publish`, which assembles the plain crate and then runs `cargo publish`
  (M3 §2.6), with `CARGO_REGISTRY_TOKEN` from the `release` environment
  as there; the package is at the repository root, so the manifest key
  is `.`, the config sets `include-component-in-tag` to `false`, the tag
  is `v0.1.0`, and the copied tag check reads that one key and looks for
  that tag; no angle
  brackets in commit subjects (the release-notes rule of `varyk`'s
  CONTRIBUTING.md), with the commit-messages workflow copied;
- the CI workflow keeps `varyk`'s job names, `test` and `msrv`, since the
  branch ruleset names them as required checks (section 6);
- `SECURITY.md`, copied from `varyk` with the crate name changed: a
  database package for network-facing services is where a disclosure
  policy matters most;
- the first release is `0.1.0`; a breaking change bumps the minor;
- dual MIT/Apache-2.0 license; `TRADEMARKS.md` of `varyk` applies to the
  `varyk-` name, so the crate is published by the organisation;
- `varyk` itself is installed in CI from crates.io (`cargo install
  varyk`), pinned to the minimum version the package needs; a `path`
  checkout is used only while the compiler feature is unreleased.

## 8. Not in this version

Pool options beyond `max_connections` (timeouts, idle, lifetime);
`rollback` as a call, and savepoints; streaming rows; embedding the
migration files in the executable; down migrations; date, uuid, bytes,
numeric, JSON, and array columns without a cast; a query spanning two
databases' placeholder styles; sqlx's compile-time checked `query!`;
Postgres `listen`/`notify` and `copy`; `Shared<Pool>` for handlers (5b4);
MongoDB and Redis (their own packages, after 5b4).

## 9. Open questions

- Should migrations be embedded in the executable, with a compiler
  feature that lets a facade include files of the program's package?
- Should `Value` grow lists and maps so Postgres arrays and JSON columns
  bind without a cast (M5b3 §10)?
- Should the package offer a `rollback` call and savepoints, or does the
  drop-rolls-back rule cover every case a service has?
- Should `one` add `limit 1`, which would mean parsing or wrapping SQL?
- When Varyk has date and uuid types, should they be read from native
  columns through `Any`, which cannot decode them, or should the package
  drop `Any` for an enum of concrete pools?
