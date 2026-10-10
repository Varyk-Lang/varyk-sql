# varyk-sql

The official SQL package for [Varyk](https://varyk.com), a language
for backend services that compiles to Rust: SQLite, Postgres, and MySQL
through sqlx.

varyk-sql is on [crates.io](https://crates.io/crates/varyk-sql): 0.4
and 0.3 work with varyk 0.8, 0.2 with varyk 0.7, and 0.1 with varyk 0.6
(see [Versions](#versions)). Varyk is experimental and pre-1.0: anything
here may change.

The package covers what an ordinary service does with a database and
nothing more: connect, over TLS when the URL asks for it, run
migrations, query, write inside transactions, log, and test against an
in-memory SQLite database.

## Install

```sh
cargo install varyk --version '^0.8' --locked
varyk init users
cd users
varyk add sql
```

`varyk add sql` runs `cargo add varyk-sql --rename sql`, so the
manifest gets `sql = { version = "0.4.0", package = "varyk-sql" }` and
code names the package `sql::`. The default driver is SQLite, compiled
from its C source on the first build: that needs a C compiler (Xcode's
command-line tools on macOS, `build-essential` on Debian and Ubuntu)
and takes a few minutes, once. A server driver is a feature:

```sh
varyk add sql --features postgres                        # Postgres, and SQLite for tests
varyk add sql --no-default-features --features postgres  # Postgres only, skips the SQLite build
varyk add sql --features mysql                           # MySQL, and SQLite for tests
```

## A first service

`src/main.vr`:

```varyk
struct Config {
    database_url: string,
}

struct User {
    id: i64,
    name: string,
    created_at: Time,
}

async fn load() -> Result<Vec<User>, Error> {
    let config: Config = env::parse()?;
    let db = sql::connect(config.database_url).await?;
    db.migrate("migrations").await?;
    db.run("insert into users (name, created_at) values ($1, $2)", "Ada", Time::now()).await?;
    db.all("select id, name, created_at from users").await
}

async fn main() {
    match load().await {
        Ok(users) => {
            for user in users {
                println!("{} {} {}", user.id, user.name, user.created_at);
            }
        }
        Err(e) => log::error("{}", e.message()),
    }
}
```

`migrations/0001_users.sql`:

```sql
create table users (
    id integer primary key,
    name text not null,
    created_at text not null
);
```

`integer primary key` numbers new rows by itself only on SQLite; on
Postgres write `id integer primary key generated always as identity`,
and on MySQL `id integer primary key auto_increment`. To read the id a
new row got, see [Generated ids](#generated-ids). SQLite has no time
type, so `created_at` is `text` there and holds the time as text (see
[Column types](#column-types)); on Postgres write `created_at
timestamptz not null`, and on MySQL `created_at datetime(6) not null`,
whose `(6)` keeps the microseconds a `Time` has.

`.env`:

```sh
DATABASE_URL=sqlite::memory:
```

`varyk run` prints the user and the time the row was added, as `{}`
writes a `Time`: `1 Ada 2026-10-07T12:00:00.123456Z`. `main` returns
nothing in Varyk, so the work is in `load`, and `main` matches on its
result; `all` takes its row type from `load`'s return type. The same
program is in [`demo/users`](demo/users). The query's `$1` and `$2`
are placeholders on every database (see [Queries](#queries)).

## Configuration

The database URL comes from the environment through `env::parse`, as
above: the field `database_url` reads `DATABASE_URL`. A variable set in
the process environment wins; otherwise `env::parse` reads `.env` in the
current directory, which `varyk init` lists in `.gitignore`, so a
development password stays out of the repository. In production, set
`DATABASE_URL` in the service's environment.

The URL's scheme picks the driver:

| Database | URL |
|---|---|
| SQLite, a file | `sqlite://data/users.db?mode=rwc` (`rwc` creates the file) |
| SQLite, in memory | `sqlite::memory:` |
| Postgres | `postgres://user:password@host:5432/users` |
| MySQL | `mysql://user:password@host:3306/users` |

A URL for a driver the program was not built with, a URL that does not
parse, and a database that cannot be reached are each an `Error`. No
error message contains the URL, so a password cannot reach a log that
way.

| Call | Gives |
|---|---|
| `sql::connect(url)` | `Result<sql::Pool, Error>`: a pool of up to 10 connections, which waits up to 30 s for a free one |
| `sql::connect_with(url, max_connections)` | the same, with at most `max_connections` (a `u32`, at least 1) |

`db.clone()` is another handle to the same pool, for a started task
that needs one.

An in-memory SQLite database is a pool of one connection whatever the
count, kept open as long as the pool, since the database lives on that
connection. So a query on the pool while a transaction is open waits
for the transaction's connection and fails after 30 s: finish the
transaction first. Should sqlx drop the connection (after an I/O error,
say), the one that replaces it opens a new, empty database.

## TLS

sqlx is built with rustls and the webpki root certificates, so TLS
works in a minimal container with no CA bundle and no OpenSSL. The URL
asks for it:

| Database | Encrypted | Encrypted, certificate and host name checked |
|---|---|---|
| Postgres | `?sslmode=require` | `?sslmode=verify-full` |
| MySQL | `?ssl-mode=REQUIRED` | `?ssl-mode=VERIFY_IDENTITY` |

`require` and `REQUIRED` encrypt but accept any certificate; prefer the
checked form. A database whose certificate a private CA signed names
the CA's file: `&sslrootcert=/path/ca.pem` on Postgres,
`&ssl-ca=/path/ca.pem` on MySQL.

## Migrations

`db.migrate("migrations")` applies, in order, every `.sql` file in the
folder not yet recorded in the database's `_sqlx_migrations` table, and
gives `true`. Call it at startup, as above. The files are named
`<version>_<name>.sql` (`0001_users.sql`), sqlx's format, so the same
folder works with sqlx's own CLI, `sqlx migrate add` to write a new
file and `sqlx migrate run` to apply the folder to `DATABASE_URL` from
CI.

On Postgres and MySQL sqlx takes a lock while it applies, so replicas
starting together apply each migration once. A failed migration is an
`Error` naming its version and the database's message, and it releases
the lock, so another replica's `migrate` does not wait on it. `.down.sql`
files are ignored.

The folder is a path relative to the working directory, so the
container holds it beside the executable. `varyk build --release`
prints the executable's path (`target/varyk/cache/release/users`). The
runtime image below has Debian 12's C library, so the executable is
built on Debian 12 (or a system with an older glibc), here in a build
stage:

```dockerfile
FROM rust:1-bookworm AS build
RUN cargo install varyk --version '^0.8' --locked
WORKDIR /src
COPY . .
RUN varyk build --release

FROM gcr.io/distroless/cc-debian12
WORKDIR /app
COPY --from=build /src/target/varyk/cache/release/users ./users
COPY migrations ./migrations
CMD ["./users"]
```

`COPY . .` sends the whole folder to the build, so a `.dockerignore`
beside the `Dockerfile` keeps the host's build output and secrets out
of it; `varyk init` writes this one since varyk 0.7.1:

```text
target
.env
```

Docker reuses the cached `cargo install` layer on a rebuild, so to pick
up a newer varyk, build once with `docker build --no-cache .`.

## Queries

On a pool and on a transaction:

| Call | Gives |
|---|---|
| `db.one(query, values..)` | `Result<T, Error>`: the first row as a `T`, or an `Error` when there is none |
| `db.first(query, values..)` | `Result<Option<T>, Error>`: the first row, or `None` |
| `db.all(query, values..)` | `Result<Vec<T>, Error>`: every row |
| `db.run(query, values..)` | `Result<u64, Error>`: runs a statement; the number of rows it changed |

The query is literal text: a query built from input does not compile,
which is the package's guard against SQL injection. The values go after
it, one per placeholder: `bool`, `string`, floats, integers up to
`i64`, `Time`, `Uuid`, `Bytes`, and an `Option` of each, where `None` is
`NULL`. [Column types](#column-types) says which column each one goes
into.

```varyk
let user: Option<User> = db.first("select id, name from users where id = $1", id).await?;
```

The placeholders are `$1`, `$2`, and so on, on every database. `$2`
may come before `$1`, and `$1` may be written twice; it takes the same
value each time. SQLite and MySQL also take `?`, one per value in order, as
before. A `$` inside a string, a quoted name, or a comment is text, not
a placeholder. On MySQL the package reads quotes by the server's default
rules: if the server's `sql_mode` has `NO_BACKSLASH_ESCAPES` or
`ANSI_QUOTES`, keep `?`, `$`, and backslashes out of the quoted text of a
query that uses `$1`, or write it with `?`.

- On SQLite and MySQL the number of values is checked against the
  number of placeholders before the query runs, and a mismatch is an
  `Error` naming both. So is a query that mixes `?` and `$1`,
  numbers its placeholders other than `$1` up to the number of values, each used, or,
  when it holds a `$`, has a quote or comment that never closes. On Postgres the server
  rejects too few values and ignores an extra one.
- A `::` cast is Postgres's own syntax. The optional filter `where
  ($1::text is null or name = $1)` runs only there, so a program that
  runs on two databases picks it with an `if`; `where ($1 is null or
  name = $1)`, with no cast, runs on SQLite and MySQL.
- On Postgres a `None` is a `NULL` with no type, which takes its type
  from where the parameter is first used: in `set done = $1` and in
  `where done = $1` it takes the type of `done`, with no cast. Where
  nothing there gives it a type, as in `$1 is null`, Postgres answers
  "could not determine data type of parameter $1": cast the parameter
  at its first use, as in the optional filter `where ($1::text is null
  or name = $1)`.

`one` or `first`: a handler that answers "not found" uses `first` and
matches on `None`; `one` is for a row that must exist. Neither adds a
`limit`, so a query that can match many rows says `limit 1` itself. On
Postgres and MySQL both read the rest of the result and drop it, so a
statement that fails on a later row is an `Error`, not the first row,
and the connection's next statement is not handed that error.

### Generated ids

Each call on a pool may run on a different connection of it, so a
statement that reads what an earlier one left on its connection does
not work on a pool. Read a new row's id in the statement that makes it:

```varyk
// Postgres and SQLite; `$1` runs on every database, but MySQL has no `returning`
let id: i64 = db.one("insert into users (name, created_at) values ($1, $2) returning id", name, Time::now()).await?;
```

MySQL has no `returning`, and `select last_insert_id()` on a pool may
run on another connection than the insert and give 0 or another
request's id. Run both in one transaction, which holds one connection:

```varyk
let mut tx = db.begin().await?;
let _added = tx.run("insert into users (name, created_at) values (?, ?)", name, Time::now()).await?;
let id: i64 = tx.one("select last_insert_id()").await?;
tx.commit().await?;
```

The same holds for any state a database keeps per connection: session
variables, temporary tables, `found_rows()`. Use it inside one
transaction, and drop it before `commit` (on Postgres, `create
temporary table .. on commit drop`), since it stays on the connection
for the next request that gets it.

## Reading rows

`T` comes from where the result goes: `let user: User =
db.one(..).await?`. A struct is read by column name: each field takes
the column of its name, or of its `#[rename("..")]`; columns the struct
has no field for are ignored, whatever their type; a field with no
column is an `Error` naming it, unless it is an `Option` (then `None`)
or has `#[default]`. A `NULL` goes into an `Option` field as `None` and
into any other field as an `Error` naming the column.

When `T` is a number, `bool`, `string`, `Time`, `Uuid`, or `Bytes`, or
an `Option` of one, the row has exactly one column:

```varyk
let n: i64 = db.one("select count(*) from users").await?;
let last: Option<i64> = db.one("select max(id) from users").await?;
```

A struct holding a struct, a `Vec`, and a `HashMap` cannot be read from
a row.

## Column types

A `Time`, a `Uuid`, and a `Bytes` each go into a column of their own on
each database, and back, with no cast:

| Database | `Time` | `Uuid` | `Bytes` |
|---|---|---|---|
| Postgres | `timestamptz` | `uuid` | `bytea` |
| MySQL | `datetime(6)` | `char(36)` | `blob` |
| SQLite | `text` | `text` | `blob` |

- A `Uuid` goes into MySQL and SQLite as its written form, 36
  characters in lower case, so it reads the same in a SQL shell as in
  JSON. Keep a MySQL `char(36)` at a text collation: MySQL names a text
  column with a binary collation (`ascii_bin`, `utf8mb4_bin`) as bytes,
  which a `Uuid` field refuses and a `Bytes` field reads as they are,
  not as base64. A `binary(16)` column holds a `Uuid` as its 16 bytes.
- A `Time` goes into SQLite as text of a fixed width,
  `2026-10-07T12:00:00.000000Z`, always with six fraction digits, so
  `order by` and `<` in SQL put times in the order Varyk does. A SQLite
  `default current_timestamp` writes `2026-10-07 12:00:00`, with a space
  and no zone, which a `Time` does not read: write the time from Varyk,
  as the demo does, or store RFC 3339 text.
- A `Time` goes into MySQL as a date and time in UTC. A `datetime(6)`
  keeps its microseconds, where a plain `datetime` rounds to the second,
  and a `timestamp(6)` holds the same instant, since sqlx sets each
  session's time zone to UTC. A URL that sets another zone
  (`?timezone=%2B02:00`) makes MySQL give a `timestamp` column, and
  `now()`, as that zone's wall clock, which a `Time` takes as UTC, two
  hours off here, and stores a `Time` written into a `timestamp` two
  hours off the other way. Leave `timezone` out of the URL.
- A Postgres `timestamp`, with no zone, reads into a `Time` as UTC.

A field reads a column by one rule on every database. An integer
column goes into an integer field when the value is in the field's
range, and into a float field; a float column goes into a float field,
and into an integer field only when it is whole and in range. A `bool`
field takes a boolean column or an integer `0` or `1`. A `string` field
takes text, bytes that are UTF-8 text (MySQL names a text column with a
binary collation as bytes), and a time or uuid column as its written
form (on SQLite, the text stored). A `Time` field takes a time column,
or text in RFC 3339 form; a `Uuid` field a `uuid` column, text of 36
characters, or 16 bytes (a `binary(16)`, say); a `Bytes` field a binary
column, or text as base64. A value the field does not take is an
`Error` naming the column, never the value.

| Database | Read as they are | Cast in the query |
|---|---|---|
| Postgres | `boolean`, `smallint`, `integer`, `bigint`, `real`, `double precision`, `text`, `varchar`, `bytea`, `timestamptz`, `timestamp`, `uuid` | `numeric`, `json`, `date`, `time`, arrays, and `char(n)` to `text` or a number: `amount::text` |
| MySQL | `boolean`, `tinyint`, `smallint`, `mediumint`, `int`, and `bigint`, signed or unsigned, `float`, `double`, `char(n)`, `varchar(n)`, `text` of any size, `enum`, `binary(n)`, `varbinary(n)`, `blob` of any size, `datetime`, `timestamp` | `decimal`, `json`, `date`, `time` with `cast(x as char)`; `sum` over integers gives `decimal`, so `cast(sum(x) as signed)` reads an integer sum, and `cast(avg(x) as double)` an average |
| SQLite | every value, by what it holds (an integer, a float, text, or bytes), whatever type its column declares | nothing |

A column of a type the table does not list is an `Error` naming the
column when a field reads it. Postgres takes a `string` as `text`, so
one written into a `numeric`, `json`, or `date` column is cast at its
placeholder: `$1::date`.

## Transactions

```varyk
async fn rename(db: sql::Pool, id: i64, name: string) -> Result<bool, Error> {
    let mut tx = db.begin().await?;
    let changed = tx.run("update users set name = ? where id = ?", name, id).await?;
    if changed != 1 {
        return Err(Error::new("no such user"));
    }
    tx.run("insert into audit (user_id, action) values (?, 'rename')", id).await?;
    tx.commit().await
}
```

`db.begin()` starts a transaction on one connection of the pool; `one`,
`first`, `all`, and `run` work on it as on a pool and change it, so it
is `let mut`. `commit` commits and gives `true`. A transaction dropped
without `commit` rolls back, so an early `return` or a `?` undoes it;
there is no `rollback` call and no nested transaction. A second
`commit`, or a query after one, is an `Error`, "this transaction is
finished".

When a statement in a transaction fails in the database (a duplicate
key, a deadlock, a missing table), the transaction is over: every later
query on it is an `Error`, "a statement in this transaction failed; it
will roll back", without reaching the database, and `commit` rolls it
back and is an `Error`, "the transaction was rolled back: a statement in
it failed". To try again, start a new transaction. The rule is the same
on all three databases: Postgres aborts the transaction at the failure,
and a MySQL deadlock and some SQLite errors end it on the server. An
`Error` from reading the rows into `T` does not end the transaction,
nor do the package's own placeholder checks (the count, the numbering,
and the scanner's) on SQLite and MySQL; on Postgres too few values is a
database error and does. After `commit`
the transaction is finished, as after any other.

## Errors

Every failure is an `Error` whose message the package writes, and no
call panics.

- A database error keeps the database's own message, which names the
  constraint or column where there is one. It never includes Postgres's
  "detail" field, which can hold the row's values. Postgres and MySQL
  may still quote the offending value in the message itself (a
  duplicate key, say), so treat these messages as data: log them, and
  give clients a message of your own.
- A connection failure says which step failed and never contains the
  URL.
- A row that cannot be read names the field or column, never the value
  in it.

## Logging

sqlx reports each statement at debug level through `tracing`, which a
Varyk program that logs already sets up: the SQL text and the time it
took, never the values. `LOG=debug` turns these lines on; the demo's
insert gives

```text
2026-10-07T08:02:34.865Z DEBUG summary=insert into users (name, … db.statement=

insert into users (name, created_at) values ($1, $2)
 rows_affected=1 rows_returned=0 elapsed=14.334µs elapsed_secs=1.4334e-5
```

with the text as sent (on MySQL, `$1` and `$2` rewritten to `?`s) and the
values, `"Ada"` and the time, nowhere in the log.

## Health check

A readiness check takes a connection from the pool and asks the
database for an answer:

```varyk
let _ok = db.run("select 1").await?;
```

## Testing

```varyk
async fn count_users() -> Result<i64, Error> {
    let db = sql::connect_with("sqlite::memory:", 1).await?;
    db.migrate("migrations").await?;
    db.run("insert into users (name, created_at) values (?, ?)", "Ada", Time::now()).await?;
    db.one("select count(*) from users").await
}

#[test]
async fn adds_a_user() {
    let counted: Result<i64, Error> = count_users().await;
    match counted {
        Ok(n) => assert_eq(n, 1),
        Err(e) => assert_eq(e.message(), "no error"),
    }
}
```

A test returns nothing, so the work goes in a function that returns a
`Result`, and the test opens it with `match` and `assert_eq`. Each
`sqlite::memory:` pool is a database of its own, alive while the pool
is, so every test starts empty; its single connection has the limits
described under [Configuration](#configuration).

Run `varyk test` from the package root: `migrate("migrations")` and
`.env` are read relative to the working directory.

## Versions

A program and varyk-sql must resolve to one `varyk-std`, or the
generated Rust has two `Error` types, so each minor varyk release is
followed by a varyk-sql release.

| varyk-sql | varyk |
|---|---|
| 0.4 | 0.8 |
| 0.3 | 0.8 |
| 0.2 | 0.7 |
| 0.1 | 0.6 |

## Security

Report a vulnerability as described in the
[security policy](https://github.com/Varyk-Lang/varyk-sql/security/policy)
shared by every Varyk-Lang repository.

## Contributing

See [CONTRIBUTING.md](CONTRIBUTING.md); rules for AI coding agents are
in [AGENTS.md](AGENTS.md).

## License

Licensed under either of [Apache License, Version 2.0](LICENSE-APACHE)
or [MIT license](LICENSE-MIT) at your option. The name "Varyk" is a
trademark and is not covered by those licenses; see Varyk's
[TRADEMARKS.md](https://github.com/Varyk-Lang/varyk/blob/main/TRADEMARKS.md).
