# varyk-sql

The official SQL package for [Varyk](https://varyk.com), a language
for backend services that compiles to Rust: SQLite, Postgres, and MySQL
through sqlx.

varyk-sql 0.1.0 is on [crates.io](https://crates.io/crates/varyk-sql)
and works with varyk 0.6 (see [Versions](#versions)). Varyk is
experimental and pre-1.0: anything here may change.

The package covers what an ordinary service does with a database and
nothing more: connect, over TLS when the URL asks for it, run
migrations, query, write inside transactions, log, and test against an
in-memory SQLite database.

## Install

```sh
cargo install varyk --version '^0.6' --locked
varyk init users
cd users
varyk add sql
```

`varyk add sql` runs `cargo add varyk-sql --rename sql`, so the
manifest gets `sql = { version = "0.1.0", package = "varyk-sql" }` and
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

`migrations/0001_users.sql`:

```sql
create table users (
    id integer primary key,
    name text not null
);
```

`integer primary key` numbers new rows by itself only on SQLite; on
Postgres write `id integer primary key generated always as identity`,
and on MySQL `id integer primary key auto_increment`. To read the id a
new row got, see [Generated ids](#generated-ids).

`.env`:

```sh
DATABASE_URL=sqlite::memory:
```

`varyk run` prints `1 Ada`. `main` returns nothing in Varyk, so the work
is in `load`, and `main` matches on its result; `all` takes its row type
from `load`'s return type. The same program is in
[`demo/users`](demo/users). The query's `?` is SQLite's and MySQL's
placeholder; Postgres's is `$1` (see [Queries](#queries)).

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
RUN cargo install varyk --version '^0.6' --locked
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
of it:

```text
target
.env
```

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
`i64`, and an `Option` of each, where `None` is `NULL`.

```varyk
let user: Option<User> = db.first("select id, name from users where id = ?", id).await?;
```

The placeholders are the database's own: `?` on SQLite and MySQL, `$1`,
`$2` on Postgres. A program that runs on two databases keeps two
queries and picks one with an `if`.

- On SQLite and MySQL the number of values is checked against the
  number of placeholders before the query runs, and a mismatch is an
  `Error` naming both. On Postgres the server rejects too few values
  and ignores an extra one.
- On Postgres a value goes as it is into an integer, float, or text
  column. A parameter for a column of another type is written
  `$1::text::<type>`: `$1::text::boolean` for a boolean when the value
  may be `None` (a `None` is sent as an integer `NULL`, which Postgres
  will not cast to `boolean`, not even with `$1::boolean`), and
  `$1::text::uuid` or `$1::text::timestamptz` with the value as a
  string. A value that may be `None` compared with a column that is not
  an integer is written `$1::text` (or `$1::text::<type>`), as in the
  optional filter `where ($1::text is null or name = $1::text)`; a
  plain `nick = $1` with `None` fails with "operator does not exist:
  character varying = bigint".

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
// Postgres; SQLite takes `$1` as well as `?`
let id: i64 = db.one("insert into users (name) values ($1) returning id", name).await?;
```

MySQL has no `returning`, and `select last_insert_id()` on a pool may
run on another connection than the insert and give 0 or another
request's id. Run both in one transaction, which holds one connection:

```varyk
let mut tx = db.begin().await?;
let _added = tx.run("insert into users (name) values (?)", name).await?;
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
has no field for are ignored; a field with no column is an `Error`
naming it, unless it is an `Option` (then `None`) or has `#[default]`.
A `NULL` goes into an `Option` field as `None` and into any other field
as an `Error` naming the column.

When `T` is a number, `bool`, or `string`, or an `Option` of one, the
row has exactly one column:

```varyk
let n: i64 = db.one("select count(*) from users").await?;
let last: Option<i64> = db.one("select max(id) from users").await?;
```

A struct holding a struct, a `Vec`, and a `HashMap` cannot be read from
a row.

## Column types

sqlx's `Any` driver, which serves all three databases through one pool,
reads booleans, integers, floats, and text. One rule applies on each:
an integer column goes into an integer field when the value is in the
field's range, and into a float field; a float column goes into a float
field, and into an integer field only when it is whole and in range; a
`bool` field takes a boolean column or an integer `0` or `1`; a
`string` field takes text, and bytes that are UTF-8 text (MySQL
reports a `text` column as bytes).

| Database | Read as they are | Cast in the query |
|---|---|---|
| Postgres | `boolean`, `smallint`, `integer`, `bigint`, `real`, `double precision`, `text`, `varchar` | anything else to `text` (or a number): `id::text` for `uuid`, `timestamptz`, `numeric`, `json`, `char(n)` |
| MySQL | `smallint`, `int`, `bigint` (signed), `float`, `double`, `char(n)`, `varchar(n)`, `text` | `datetime`, `decimal`, `json` with `cast(x as char)`; `boolean`, `tinyint`, `mediumint`, `smallint unsigned`, `int unsigned` with `cast(x as signed)`; `bigint unsigned` with `cast(x as char)` into a `string`; `sum` over integers gives `decimal`, so `cast(sum(x) as signed)` reads an integer sum, and `cast(avg(x) as double)` an average |
| SQLite | `integer`, `real`, `text`, `varchar` | a column declared `boolean` with `cast(x as integer)`; `datetime`, `date`, `timestamp`, `time` with `cast(x as text)` |

Every selected column must be one the driver reads, whether or not `T`
has a field for it: `select *` on a table with a `timestamptz` column
fails even when the struct leaves it out. Name the columns. Varyk has
no date, uuid, or bytes type yet; when it does, this list grows.

MySQL's unsigned integers read wrong without an `Error`: the driver
reads `smallint unsigned`, `int unsigned`, and `bigint unsigned` as
signed, so a value at or above 2^15, 2^31, or 2^63 wraps to a negative
number (40000 in a `smallint unsigned` reads as -25536, 3000000000 in
an `int unsigned` as -1294967296). Read a `smallint unsigned` or an
`int unsigned` with `cast(x as signed)` into an `i64`, and a `bigint
unsigned` that may reach 2^63 with `cast(x as char)` into a `string`.
The cast goes around the selected expression, `cast(max(x) as
signed)`, since `max(x)`, `min(x)`, and a subquery keep the column's
unsigned type.

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
nor does the package's own count of values on SQLite and MySQL; on
Postgres too few values is a database error and does. After `commit`
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
- A row that cannot be read names the field or column, with one
  exception: on MySQL a column of a type the driver cannot read is
  "the query uses a type varyk-sql cannot read or write; cast it in the
  query", naming no column.

## Logging

sqlx reports each statement at debug level through `tracing`, which a
Varyk program that logs already sets up: the SQL text and the time it
took, never the values. `LOG=debug` turns these lines on; the demo's
insert gives

```text
2026-10-03T23:59:25.268Z DEBUG summary=insert into users (name) … db.statement=

insert into users (name) values (?)
 rows_affected=1 rows_returned=0 elapsed=18.583µs elapsed_secs=1.8583e-5
```

with the `?` as written and the value `"Ada"` nowhere in the log.

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
    db.run("insert into users (name) values (?)", "Ada").await?;
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
