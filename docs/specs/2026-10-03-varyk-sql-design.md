# varyk-sql: the database package

Date: 2026-10-03.

**Status.** Implemented in 0.1.0. `varyk-sql` is the official
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

is a working service against SQLite, Postgres, or MySQL, chosen by the
URL. (`main` returns nothing in Varyk, so the work is in `load` and
`main` matches on it; `all` takes its `T` from `load`'s return type.)
The package aims at what an ordinary production service does with a
database and nothing more: connect over TLS, run migrations, query, write
inside transactions, log, and test against an in-memory SQLite.

### 1.1 Decisions

- **sqlx, through its `Any` driver.** One pool type, `AnyPool`, serves all
  three databases, picked from the URL scheme at run time. Every item of
  the facade is then free of `#[cfg]`, which Varyk does not import, and a
  program compiled with two drivers chooses between them by configuration
  alone. The cost is that `Any` decodes a fixed set of column types
  (section 2.6); other types are cast in SQL.
  0.3 replaces this with concrete pools, one per driver
  (`2026-10-07-varyk-sql-0.3-design.md` §1.1).
- **Scalars only as values**, by M5b3 §2.2; one query form, with the
  database's own placeholders passed through.
  0.4 makes `$1`, `$2`, … the form on every database
  (`2026-10-09-varyk-sql-0.4-design.md` §1).
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
mod tests;
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
An in-memory SQLite URL gives a pool of one connection whatever the
count (section 4, Tests).
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
`"migrations"`; it may come from configuration. Files are named
`<version>_<name>.sql`, sqlx's format, so the same folder works with `sqlx migrate add` and
`sqlx migrate run`. sqlx takes a database lock on Postgres and MySQL
while applying, so several replicas starting together apply each
migration once; SQLite has one writer. A failed migration is an `Error`
naming the file's version and the database's message. sqlx returns from
any failure without releasing its session lock, so `migrate` runs on a
connection it acquires (`Migrator::run_direct`) and, on Postgres and
MySQL, closes that connection on a failure, ending the session and the
lock; otherwise the connection would go back to the pool holding it and
another replica's `migrate` would wait forever. Down migrations
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
databases keeps two queries. On SQLite and MySQL, before running, the
shared binding function compares the number of values with the number
of placeholders the prepared statement reports, and a mismatch is an
`Error` naming both counts; should a driver report no count, the check
is skipped and the database's own applies. The check is the package's
because SQLite binds a missing value as `NULL` and ignores an extra
one, which would be wrong data with no error. On Postgres the statement
is not prepared first (sqlx would cache the server's inferred parameter
types and reuse them for the values' own, and a `$1::uuid` cast would
not prepare through the `Any` driver): the server rejects too few
values, and an extra value is ignored. Nor is a Postgres statement kept
prepared after it runs (`persistent(false)`): sqlx's statement cache
keys a statement by its text with the parameter types of its first run
on the connection and sends later values in binary into those types
unchecked, so a statement first run with `None` (bound as an `i64`)
would store a later string's bytes as an integer. Each Postgres run is
parsed again with its own values' types; SQLite and MySQL keep sqlx's
cache.

On Postgres a value goes as it is into an integer, float, or text
column. A parameter for a column of another type is written
`$1::text::<type>`: `$1::text::boolean` for a boolean when the value
may be `None`, since a `None` is an `int8` `NULL` and Postgres refuses
`$1::boolean` ("cannot cast type bigint to boolean") as well as a plain
`$1` into a `boolean` column; and `$1::text::uuid` or
`$1::text::timestamptz` with the value as a `string`. A `None` compared
with a column that is not an integer is written `$1::text` (or
`$1::text::<type>`): `where ($1::text is null or nick = $1::text)`,
since a plain `nick = $1` with `None` fails with "operator does not
exist: character varying = bigint".

Each call on a `Pool` acquires a connection of the pool, so two calls
may run on two connections, and a statement that reads what an earlier
one left on its connection does not work on a pool. A generated id is
read with `insert .. returning id` through `one` on Postgres and
SQLite; on MySQL, which has no `returning`, `tx.run(insert)` and then
`tx.one("select last_insert_id()")` inside one `Tx`, since `select
last_insert_id()` on a pool may give 0 or another request's id. The
same holds for any per-connection state: session variables, temporary
tables, `found_rows()`. It is used inside one `Tx` and dropped before
`commit` (on Postgres, `create temporary table .. on commit drop`),
since it stays on the connection for the next request that gets it.
The README says so.

`T` is chosen from where the result goes (M5b3 §2.1): `let user: User =
db.one(..).await?`. A handler that answers "not found" uses `first` and
matches on `None`; `one` is for a row that must exist.

`one` reads at most one row: it adds no `limit`, so a query that can
match many rows should say `limit 1` itself. On Postgres and MySQL
`one` and `first` read the rest of the result to its end and drop it:
sqlx's `fetch_optional` there stops at the first row, so a statement
that fails on a later row (`1 / (x - 2)` over three rows on Postgres; a
subquery giving two rows for a later row on MySQL) would give `Ok`. On
Postgres the error would be lost on a pool and surface on a
transaction's next statement; on MySQL the leftover error would be the
next statement's on that connection, on a pool perhaps another
request's. Reading to the end makes it an `Error` from `one` or
`first`, which fails a `Tx` (section 2.4). SQLite computes rows as they
are read, so `fetch_optional` is used as it is there.

### 2.4 Transactions

| Call | Gives | Does |
|---|---|---|
| `db.begin()` | `Result<Tx, Error>` | starts a transaction on one connection of the pool |
| `tx.one`, `tx.first`, `tx.all`, `tx.run` | as on a pool | inside the transaction; each is a `mut self` method, so `tx` is `let mut` |
| `tx.commit()` | `Result<bool, Error>` | a `mut self` method: commits and gives `true` (Varyk has no `()`, M5b3 §2.4); a second `commit`, or a query after one, is an `Error` "this transaction is finished" |

A statement in a `Tx` that fails in the database (sqlx's
`Error::Database` from preparing or running it: a duplicate key, a
deadlock, a missing table, bad SQL) marks the `Tx` failed. From then on
every query on the `Tx` is an `Error`, "a statement in this transaction
failed; it will roll back", without touching the database, and `commit`
rolls back and is an `Error`, "the transaction was rolled back: a
statement in it failed"; the `Tx` is finished after it as after any
`commit`. The rule is the same on all three databases: Postgres aborts
the transaction at the failure and its `COMMIT` is then a rollback that
sqlx reports as success; a MySQL deadlock (1213) rolls back and ends the
transaction while autocommit stays on, and SQLite ends a transaction on
its own after some errors, so a later statement on the `Tx` would commit
by itself. An `Error` from reading the rows into `T` leaves the `Tx` as
it was, and so does the package's own count of values on SQLite and
MySQL; on Postgres too few values is a database error and fails the
`Tx`.

A `Tx` dropped without `commit` rolls back, which sqlx does when the
transaction is dropped. There is no `rollback` call: return early, with
`?` or otherwise, and the drop rolls back. Nested `begin` on a `Tx`
(savepoints) is not offered.

### 2.5 Reading rows

A row is read into `T` by column name through a small serde deserializer
over sqlx's row: each field of `T` takes the column of the same name
(`#[rename]` on the field changes it, M5a §2.2), columns `T` has no field
for are ignored, and a field without a column is an `Error` naming the
field, unless it has `#[default]` or is skipped; an `Option` field
without a column is `None`, as in `json::parse`. Every selected column
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
(`let last: Option<i64> = db.one("select max(id) from users")`).
`Vec` or `HashMap` of a scalar, and a struct holding a struct, are not
readable from a row (an `Error`); a nested type has no column.

### 2.6 Column types

sqlx's `Any` driver decodes booleans, integers, floats, text, and bytes
(which Varyk cannot hold), and encodes the same. The drivers do not
agree on kinds (SQLite reports every integer as a 64-bit one and every
float as a double, and has no boolean kind), so the deserializer applies
one rule for all three: an integer column goes into an integer field
when the value is in the field's range, and into a float field as Rust's
`as` converts; a float column goes into a float field, and into an
integer field only when it has no fractional part and is in range; a
`bool` field accepts a boolean column and an integer `0` or `1`; a
`string` field accepts text, and a bytes column holding UTF-8 (MySQL
reports a `text` column as bytes); anything else is an `Error` naming
the column. Any other column
type (`uuid`, `timestamp`, `numeric`, `json`, `bytea`, arrays) is cast in
the query, in the database's own syntax: `select id::text from ..` on
Postgres, `select cast(id as char) from ..` on MySQL. Varyk has no
date, uuid, or bytes type yet; when it does, this list grows.

MySQL's unsigned integers are read wrong with no `Error`, which the
package cannot detect: sqlx's `Any` maps `smallint unsigned` to its
16-bit, `int unsigned` to its 32-bit, and `bigint unsigned` to its
64-bit integer kind, reads them as signed, and keeps no unsigned flag
in the column's type (`AnyTypeInfo` holds only the kind). A value at or
above 2^15 in a `smallint unsigned` (2^31 in an `int unsigned`, 2^63 in
a `bigint unsigned`) wraps to a negative number: 40000 reads as -25536,
3000000000 as -1294967296, and 18446744073709551615 as -1. The README
says to read a `smallint unsigned` or an `int unsigned` with `cast(x as
signed)` into an `i64`, and a `bigint unsigned` that may reach 2^63
with `cast(x as char)` into a `string`. The cast goes around the
selected expression, `cast(max(x) as signed)`: `max`, `min`,
`distinct`, and a subquery keep the column's unsigned type and wrap
too, while `x + 0` and `coalesce` widen it.

## 3. Errors

Every failure is a `varyk_std::Error` (M5b3 §2.4) whose message
`varyk-sql` writes:

- a database error carries the database's own message (which names a
  constraint or column where there is one) and never its "detail" field,
  which on Postgres may hold the row's values;
- a connection failure says which step failed (parsing the URL, the
  driver, reaching the database) and never includes the URL, so a
  password cannot reach a log through an error;
- a row-reading failure names the field or column (section 2.5), except
  a MySQL column of a type the driver cannot read, which sqlx reports as
  an `AnyDriverError` with no column, given as "the query uses a type
  varyk-sql cannot read or write; cast it in the query";
- a finished transaction, a transaction rolled back at `commit`
  (section 2.4), an empty `one`, and a missing driver each have a fixed
  message.

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
  `Result`s with `match` and `assert`. sqlx's `Any` driver parses the
  URL again for each connection, and each parse of `sqlite::memory:`
  names a new database, so `connect` and `connect_with` keep a pool on
  an in-memory SQLite database (`:memory:` or `mode=memory`) at one
  connection, whatever the count, and never close it: the database is
  alive while the pool is, so it lives for one test, and the test's
  queries are serial, so no two of them contend for SQLite's single
  writer. With one, a query on
  the pool while a `Tx` is open waits for the transaction's connection
  and fails after sqlx's acquire timeout, so a test finishes the
  transaction first. The README says all of this.
- **Generated ids.** `insert .. returning id` through `one` on Postgres
  and SQLite, and `last_insert_id()` inside the inserting `Tx` on MySQL
  (section 2.3); the README shows both.
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
  README.md, LICENSE-MIT, LICENSE-APACHE, CONTRIBUTING.md
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
  `migrate`, and `tls-rustls`,
  `serde` for the deserializer (`varyk-std` re-exports serde for
  signatures; the deserializer's impls are easier against the crate
  directly), and `futures-core` 0.3 for the `Stream` trait, to read a
  Postgres result to its end (section 2.3; sqlx already depends on it);
- `[features]`: `default = ["sqlite"]`, `sqlite = ["sqlx/sqlite"]`,
  `postgres = ["sqlx/postgres"]`, `mysql = ["sqlx/mysql"]`; a program
  adds a driver with `varyk add sql --features postgres`, and `varyk`
  passes features through (M5b2 §4.3).

`src/db.rs` holds, in order: `connect_with`, which calls sqlx's
`install_default_drivers` first (`Any` needs it; sqlx guards it with a
`Once`, so repeated calls are harmless), and `connect`, which delegates
to it with sqlx's default; `Pool` wrapping `sqlx::AnyPool`, with
`#[derive(Clone)]` so Varyk's `db.clone()` works (M4 §2.10), and `Tx`
wrapping `Option<sqlx::Transaction<'static, sqlx::Any>>`; the four
query methods on each, sharing one binding function over `Value`; the
row deserializer; and the error mapping of section 3. Published, it is
an ordinary crate (M3 §2.6) with `build = false` and the generated
`src/lib.rs`, so a Rust program can use it too.

## 6. Testing and definition of done

- `varyk test` in the package, async tests in `src/tests.vr` on
  `connect_with(url, 1)`, with `url` from the next bullet
  (`sqlite::memory:` by default): the schema applied by `migrate`,
  each of
  `one`, `first`, `all`, and `run` with zero and several values; `one` on
  no row; `first` giving `None`; a scalar `T`; a `NULL` into an `Option`
  and into a plain field; a missing column; `#[rename]`; a transaction
  committed, one dropped, and `commit` twice; `connect_with(url, 1)`;
  a URL for a missing driver, a bad URL, and an unreachable database each
  giving an `Error` without the URL in its message; too few and too
  many values, each an `Error`; a second `migrate` succeeding with the
  row count of `_sqlx_migrations` unchanged (read with `one`).
- The same tests against Postgres and MySQL: each test reads
  `DATABASE_URL` with `env::parse` into a struct whose one field is an
  `Option<string>`, and uses `sqlite::memory:` when it is `None`; a test
  with values picks its query by the URL's scheme with an `if`, since
  each query is a literal; the schema in `migrations/` is portable
  across the three (explicit ids, no autoincrement); a test keys the
  rows it writes by its own name and deletes them first, so a shared
  server database and a rerun do not collide; and CI sets
  `RUST_TEST_THREADS=1` for these runs, which the test binaries honour,
  so tests sharing one server database run one at a time. `varyk test`
  cannot select cargo features and the package's default is `sqlite`,
  so each server leg of CI edits the manifest's `default = ["sqlite"]`
  line to add its driver before running.
- `demo/users` builds and prints the expected output under `varyk run`
  on SQLite.
- CI, in two jobs named `test` (stable) and `msrv` (1.85), the names the
  repository's branch ruleset requires: `varyk check` and the tests on
  SQLite in both; in `test` only, the tests on Postgres and MySQL
  service containers (the 1.85 build gains nothing from them), `rustfmt
  --check
  src/db.rs`, and `cargo clippy` in the crate `varyk publish
  --assemble-only` writes (plain cargo cannot build a package whose
  target is `src/lib.vr`, M5b2 §1.2).
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
- no `SECURITY.md` of its own: the organization's shared policy (in
  `Varyk-Lang/.github`) applies to every repository, already names
  `varyk-sql` in its scope, and shows in the Security tab; a file here
  would override it with a shorter copy; a doc that points to it (the
  README, CONTRIBUTING.md) links
  https://github.com/Varyk-Lang/varyk-sql/security/policy by URL, so the
  link works in a clone and on crates.io;
- the first release is `0.1.0`; a breaking change bumps the minor;
- dual MIT/Apache-2.0 license; `TRADEMARKS.md` of `varyk` applies to the
  `varyk-` name, so the crate is published by the organisation;
- `varyk` itself is installed in CI from crates.io (`cargo install
  varyk`), within the minor version the package needs (`--version
  '^0.6'`); before 0.6.0 shipped, CI built the compiler from a `path`
  checkout with `VARYK_STD_PATH` (M5b2 §4.7).

## 8. Not in this version

Pool options beyond `max_connections` (timeouts, idle, lifetime);
`rollback` as a call, and savepoints; streaming rows; embedding the
migration files in the executable; down migrations; date, uuid, bytes,
numeric, JSON, and array columns without a cast; a query spanning two
databases' placeholder styles; sqlx's compile-time checked `query!`;
Postgres `listen`/`notify` and `copy`; `Shared<Pool>` for handlers (5b4);
MongoDB and Redis (their own packages, after 5b4).
0.4 lets one query text use `$n` on every database, which leaves a query
spanning two placeholder styles only as a mix of `?` and `$n`, now an
`Error` (`2026-10-09-varyk-sql-0.4-design.md` §2.2).
Time, uuid, and bytes columns need no cast from 0.3, which leaves
`date` and the rest here (`2026-10-07-varyk-sql-0.3-design.md` §3, §8).

## 9. Open questions

- Should migrations be embedded in the executable, with a compiler
  feature that lets a facade include files of the program's package?
- Should `Value` grow lists and maps so Postgres arrays and JSON columns
  bind without a cast (M5b3 §10)?
- Should the package offer a `rollback` call and savepoints, or does the
  drop-rolls-back rule cover every case a service has?
- Should `one` add `limit 1`, which would mean parsing or wrapping SQL?
- Should a database error's message be cleaned of the values Postgres
  and MySQL quote in it (a duplicate key), so a log never holds them?
- When Varyk has date and uuid types, should they be read from native
  columns through `Any`, which cannot decode them, or should the package
  drop `Any` for an enum of concrete pools?
  Answered in 0.3: an enum of concrete pools
  (`2026-10-07-varyk-sql-0.3-design.md` §1.1).
