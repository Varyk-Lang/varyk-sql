// The facade of varyk-sql: the Rust that Varyk programs reach through
// `sql::connect`, `sql::connect_with`, `sql::Pool`, and `sql::Tx`. Every sqlx
// `Result` is mapped to a `varyk_std::Error` whose message names no URL
// and no database "detail" field, and nothing here panics.

use std::borrow::Cow;
use std::fmt;
use std::ops::Range;
use std::path::Path;
use std::pin::Pin;

use futures_core::Stream;

use serde::de::{self, DeserializeOwned, DeserializeSeed, IntoDeserializer, MapAccess, Visitor};
use sqlx::database::HasStatementCache;
use sqlx::decode::Decode;
use sqlx::migrate::{Migrate, MigrateError, Migrator};
use sqlx::pool::PoolOptions;
use sqlx::query::Query;
use sqlx::{
    Column, ColumnIndex, Database, Either, Executor, IntoArguments, Row, Statement, TypeInfo,
    ValueRef,
};

/// sqlx's default for the most connections a pool opens.
const DEFAULT_MAX_CONNECTIONS: u32 = 10;

/// Opens a pool on the database `url` names, with at most
/// `max_connections` connections (at least 1). The scheme picks the
/// driver: `sqlite:`, `postgres:`, or `mysql:`. An in-memory SQLite
/// database gets a pool of one connection whatever the count.
pub async fn connect_with(url: &str, max_connections: u32) -> Result<Pool, varyk_std::Error> {
    // Inside a body: Varyk's importer refuses a macro call among a facade's
    // items.
    #[cfg(not(any(feature = "sqlite", feature = "postgres", feature = "mysql")))]
    compile_error!(
        "varyk-sql needs at least one of its features `sqlite`, `postgres`, or `mysql`, one per database it connects to"
    );
    if max_connections == 0 {
        return Err(varyk_std::Error::new(
            "a pool needs at least one connection; `max_connections` is 0".to_string(),
        ));
    }
    let Some((scheme, rest)) = url.split_once(':') else {
        return Err(does_not_parse());
    };
    // A URL's scheme is read in any case, as a URL parser reads it; sqlx's
    // SQLite parser strips only a lower-case one, so the URL goes on with
    // its scheme in lower case.
    let scheme = scheme.to_ascii_lowercase();
    let url = format!("{scheme}:{rest}");
    driver_built(&scheme)?;
    // False for every scheme but `sqlite`.
    let memory = in_memory(&url);
    let pool = match scheme.as_str() {
        #[cfg(feature = "sqlite")]
        "sqlite" => DriverPool::Sqlite(open(&url, max_connections, memory).await?),
        #[cfg(feature = "postgres")]
        "postgres" | "postgresql" => {
            DriverPool::Postgres(open(&url, max_connections, memory).await?)
        }
        #[cfg(feature = "mysql")]
        "mysql" | "mariadb" => DriverPool::MySql(open(&url, max_connections, memory).await?),
        // `driver_built` has refused every other scheme.
        _ => return Err(unknown_scheme()),
    };
    Ok(Pool { pool })
}

/// A pool of `DB` on `url`, whose options are parsed once here.
async fn open<DB: Database>(
    url: &str,
    max_connections: u32,
    in_memory: bool,
) -> Result<sqlx::Pool<DB>, varyk_std::Error> {
    let options: <DB::Connection as sqlx::Connection>::Options =
        url.parse().map_err(|_| does_not_parse())?;
    let pool_options = if in_memory {
        // An in-memory SQLite database lives only while a connection to
        // it is open, so the pool keeps one connection and never closes
        // it: the database lives exactly as long as the pool. Should sqlx
        // drop that connection (after an I/O error, say), its replacement
        // is a new, empty database.
        PoolOptions::<DB>::new()
            .max_connections(1)
            .min_connections(1)
            .idle_timeout(None)
            .max_lifetime(None)
    } else {
        PoolOptions::<DB>::new().max_connections(max_connections)
    };
    pool_options
        .connect_with(options)
        .await
        .map_err(connect_error)
}

/// The `Error` for a URL that does not parse.
fn does_not_parse() -> varyk_std::Error {
    varyk_std::Error::new("the database URL does not parse".to_string())
}

/// Whether `url` names an in-memory SQLite database, by sqlx's own rule:
/// with `sqlite://`, then `sqlite:`, stripped from the front, the
/// database before any `?` is `:memory:`, or a parameter after it is
/// `mode=memory`. So `sqlite::memory:`, `sqlite://:memory:`, and
/// `sqlite://?mode=memory` all do.
fn in_memory(url: &str) -> bool {
    if !url.starts_with("sqlite:") {
        return false;
    }
    let rest = url
        .trim_start_matches("sqlite://")
        .trim_start_matches("sqlite:");
    let (database, params) = match rest.split_once('?') {
        Some((database, params)) => (database, Some(params)),
        None => (rest, None),
    };
    database == ":memory:" || params.is_some_and(|p| p.split('&').any(|pair| pair == "mode=memory"))
}

/// Opens a pool on the database `url` names, with sqlx's defaults.
pub async fn connect(url: &str) -> Result<Pool, varyk_std::Error> {
    connect_with(url, DEFAULT_MAX_CONNECTIONS).await
}

/// The `Error` for a URL whose scheme names no database varyk-sql knows.
fn unknown_scheme() -> varyk_std::Error {
    varyk_std::Error::new(
        "the database URL names no database varyk-sql knows; a database URL starts with `sqlite:`, `postgres:`, or `mysql:`"
            .to_string(),
    )
}

/// An `Error` unless this build has the driver a URL of `scheme` needs.
fn driver_built(scheme: &str) -> Result<(), varyk_std::Error> {
    let (feature, built) = match scheme {
        "sqlite" => ("sqlite", cfg!(feature = "sqlite")),
        "postgres" | "postgresql" => ("postgres", cfg!(feature = "postgres")),
        "mysql" | "mariadb" => ("mysql", cfg!(feature = "mysql")),
        _ => return Err(unknown_scheme()),
    };
    if built {
        Ok(())
    } else {
        Err(varyk_std::Error::new(format!(
            "this program was built without the `{feature}` feature of varyk-sql, which the database URL needs; add it with `varyk add sql --features {feature}`"
        )))
    }
}

/// Runs `$body` with `$inner` bound to the value inside `$value`, a
/// `DriverPool` or a `DriverTx` (or a reference to one), whichever driver
/// it holds, and with `$db` naming that driver's sqlx `Database`. Each
/// arm exists only with its driver's feature.
macro_rules! per_driver {
    ($value:expr, $kind:ident($inner:ident), $db:ident => $body:expr) => {
        match $value {
            #[cfg(feature = "sqlite")]
            $kind::Sqlite($inner) => {
                type $db = sqlx::Sqlite;
                $body
            }
            #[cfg(feature = "postgres")]
            $kind::Postgres($inner) => {
                type $db = sqlx::Postgres;
                $body
            }
            #[cfg(feature = "mysql")]
            $kind::MySql($inner) => {
                type $db = sqlx::MySql;
                $body
            }
        }
    };
}

/// A pool of connections to one database. `clone` gives another handle
/// to the same pool.
#[derive(Clone)]
pub struct Pool {
    pool: DriverPool,
}

/// The pool of the driver the URL named.
#[derive(Clone)]
enum DriverPool {
    #[cfg(feature = "sqlite")]
    Sqlite(sqlx::SqlitePool),
    #[cfg(feature = "postgres")]
    Postgres(sqlx::PgPool),
    #[cfg(feature = "mysql")]
    MySql(sqlx::MySqlPool),
}

impl Pool {
    /// Runs `query` with `values` bound to its placeholders and reads
    /// the first row as a `T`; an `Error` when there is no row. No
    /// `limit` is added: a query that can match many rows says so.
    pub async fn one<T: varyk_std::serde::de::DeserializeOwned>(
        &self,
        query: &'static str,
        values: Vec<varyk_std::Value>,
    ) -> Result<T, varyk_std::Error> {
        per_driver!(&self.pool, DriverPool(pool), DB => {
            let mut conn = pool.acquire().await.map_err(db_error)?;
            one_on::<DB, T>(&mut *conn, &mut false, query, values).await
        })
    }

    /// As `one`, with `None` when there is no row.
    pub async fn first<T: varyk_std::serde::de::DeserializeOwned>(
        &self,
        query: &'static str,
        values: Vec<varyk_std::Value>,
    ) -> Result<Option<T>, varyk_std::Error> {
        per_driver!(&self.pool, DriverPool(pool), DB => {
            let mut conn = pool.acquire().await.map_err(db_error)?;
            first_on::<DB, T>(&mut *conn, &mut false, query, values).await
        })
    }

    /// Runs `query` with `values` and reads every row as a `T`.
    pub async fn all<T: varyk_std::serde::de::DeserializeOwned>(
        &self,
        query: &'static str,
        values: Vec<varyk_std::Value>,
    ) -> Result<Vec<T>, varyk_std::Error> {
        per_driver!(&self.pool, DriverPool(pool), DB => {
            let mut conn = pool.acquire().await.map_err(db_error)?;
            all_on::<DB, T>(&mut *conn, &mut false, query, values).await
        })
    }

    /// Runs the statement `query` with `values`; gives the number of
    /// rows it changed.
    pub async fn run(
        &self,
        query: &'static str,
        values: Vec<varyk_std::Value>,
    ) -> Result<u64, varyk_std::Error> {
        per_driver!(&self.pool, DriverPool(pool), DB => {
            let mut conn = pool.acquire().await.map_err(db_error)?;
            run_on::<DB>(&mut *conn, &mut false, query, values).await
        })
    }

    /// Applies, in version order, every migration in `folder` (a path
    /// relative to the working directory) that the database's
    /// `_sqlx_migrations` table does not record, and gives `true`. Files
    /// are sqlx's `<version>_<name>.sql`; `.down.sql` files are ignored.
    pub async fn migrate(&self, folder: &str) -> Result<bool, varyk_std::Error> {
        per_driver!(&self.pool, DriverPool(pool), DB => migrate_on::<DB>(pool, folder).await)
    }

    /// Starts a transaction on one connection of the pool. The
    /// connection is the transaction's until it commits or is dropped.
    pub async fn begin(&self) -> Result<Tx, varyk_std::Error> {
        let tx = match &self.pool {
            #[cfg(feature = "sqlite")]
            DriverPool::Sqlite(pool) => DriverTx::Sqlite(pool.begin().await.map_err(db_error)?),
            #[cfg(feature = "postgres")]
            DriverPool::Postgres(pool) => DriverTx::Postgres(pool.begin().await.map_err(db_error)?),
            #[cfg(feature = "mysql")]
            DriverPool::MySql(pool) => DriverTx::MySql(pool.begin().await.map_err(db_error)?),
        };
        Ok(Tx {
            tx: Some(tx),
            failed: false,
        })
    }
}

/// `migrate` on `pool`.
async fn migrate_on<DB: Database>(
    pool: &sqlx::Pool<DB>,
    folder: &str,
) -> Result<bool, varyk_std::Error>
where
    DB::Connection: Migrate,
{
    let migrator = Migrator::new(Path::new(folder))
        .await
        .map_err(|e| migrate_error(folder, e))?;
    let mut conn = pool.acquire().await.map_err(db_error)?;
    match migrator.run_direct(&mut *conn).await {
        Ok(()) => Ok(true),
        Err(e) => {
            // sqlx takes a session lock on Postgres and MySQL
            // (`pg_advisory_lock`, `GET_LOCK`) and returns on a failure
            // without releasing it, so the connection would go back to
            // the pool still holding it and another replica's `migrate`
            // would wait forever. Closing the connection ends the
            // session and its lock; the migration's `Error` is what
            // the caller needs, so a failure to close adds nothing.
            // SQLite takes no lock, and closing an in-memory
            // database's only connection would drop the database.
            if DB::NAME != "SQLite" {
                let _closed = conn.close().await;
            }
            Err(migrate_error(folder, e))
        }
    }
}

/// A transaction on one connection. A `Tx` dropped without `commit`
/// rolls back: sqlx rolls an open transaction back when it is dropped.
pub struct Tx {
    tx: Option<DriverTx>,
    /// Set when a statement in the transaction failed in the database.
    /// Postgres then aborts the transaction and its `COMMIT` is a silent
    /// rollback; a MySQL deadlock, and some SQLite errors, end the
    /// transaction on the server, and later statements would each commit
    /// on their own. So a failed transaction runs no more statements, and
    /// `commit` rolls it back and says so.
    failed: bool,
}

/// The transaction of the driver the pool holds.
enum DriverTx {
    #[cfg(feature = "sqlite")]
    Sqlite(sqlx::Transaction<'static, sqlx::Sqlite>),
    #[cfg(feature = "postgres")]
    Postgres(sqlx::Transaction<'static, sqlx::Postgres>),
    #[cfg(feature = "mysql")]
    MySql(sqlx::Transaction<'static, sqlx::MySql>),
}

impl Tx {
    /// As `Pool::one`, inside the transaction.
    pub async fn one<T: varyk_std::serde::de::DeserializeOwned>(
        &mut self,
        query: &'static str,
        values: Vec<varyk_std::Value>,
    ) -> Result<T, varyk_std::Error> {
        let (tx, failed) = self.open()?;
        per_driver!(tx, DriverTx(tx), DB => {
            one_on::<DB, T>(&mut **tx, failed, query, values).await
        })
    }

    /// As `Pool::first`, inside the transaction.
    pub async fn first<T: varyk_std::serde::de::DeserializeOwned>(
        &mut self,
        query: &'static str,
        values: Vec<varyk_std::Value>,
    ) -> Result<Option<T>, varyk_std::Error> {
        let (tx, failed) = self.open()?;
        per_driver!(tx, DriverTx(tx), DB => {
            first_on::<DB, T>(&mut **tx, failed, query, values).await
        })
    }

    /// As `Pool::all`, inside the transaction.
    pub async fn all<T: varyk_std::serde::de::DeserializeOwned>(
        &mut self,
        query: &'static str,
        values: Vec<varyk_std::Value>,
    ) -> Result<Vec<T>, varyk_std::Error> {
        let (tx, failed) = self.open()?;
        per_driver!(tx, DriverTx(tx), DB => {
            all_on::<DB, T>(&mut **tx, failed, query, values).await
        })
    }

    /// As `Pool::run`, inside the transaction.
    pub async fn run(
        &mut self,
        query: &'static str,
        values: Vec<varyk_std::Value>,
    ) -> Result<u64, varyk_std::Error> {
        let (tx, failed) = self.open()?;
        per_driver!(tx, DriverTx(tx), DB => {
            run_on::<DB>(&mut **tx, failed, query, values).await
        })
    }

    /// Commits the transaction and gives `true`. When a statement in it
    /// failed in the database, even one whose `Error` the program
    /// handled, it rolls the transaction back instead and is an `Error`
    /// saying so, on every database. The transaction is finished after
    /// it, even when the commit fails: then `commit` and every query are
    /// an `Error`.
    pub async fn commit(&mut self) -> Result<bool, varyk_std::Error> {
        match self.tx.take() {
            Some(tx) => per_driver!(tx, DriverTx(tx), DB => {
                finish::<DB>(tx, self.failed).await
            }),
            None => Err(finished()),
        }
    }

    /// The transaction and its `failed` flag, or an `Error` once it has
    /// committed or once a statement in it failed; the database is not
    /// touched then.
    fn open(&mut self) -> Result<(&mut DriverTx, &mut bool), varyk_std::Error> {
        match self.tx.as_mut() {
            None => Err(finished()),
            Some(_) if self.failed => Err(varyk_std::Error::new(
                "a statement in this transaction failed; it will roll back".to_string(),
            )),
            Some(tx) => Ok((tx, &mut self.failed)),
        }
    }
}

/// `commit` on `tx`: commits it, or rolls it back when `failed`.
async fn finish<DB: Database>(
    tx: sqlx::Transaction<'static, DB>,
    failed: bool,
) -> Result<bool, varyk_std::Error> {
    if failed {
        let message = "the transaction was rolled back: a statement in it failed";
        match tx.rollback().await {
            Ok(()) => Err(varyk_std::Error::new(message.to_string())),
            Err(e) => Err(varyk_std::Error::new(format!(
                "{message}, and the rollback failed: {}",
                db_message(e)
            ))),
        }
    } else {
        tx.commit().await.map_err(db_error)?;
        Ok(true)
    }
}

/// The `Error` for a `Tx` used after `commit`.
fn finished() -> varyk_std::Error {
    varyk_std::Error::new("this transaction is finished".to_string())
}

/// A query on `DB` with its values bound.
type Bound<'q, DB> = Query<'q, DB, <DB as Database>::Arguments<'q>>;

/// `one` on `conn`, for a pool and a transaction alike. Each of these
/// sets `failed` when the statement fails in the database (see
/// `statement_error`); a pool passes a flag it does not read.
async fn one_on<DB: Driver, T: DeserializeOwned>(
    conn: &mut DB::Connection,
    failed: &mut bool,
    query: &'static str,
    values: Vec<varyk_std::Value>,
) -> Result<T, varyk_std::Error>
where
    for<'c> &'c mut DB::Connection: Executor<'c, Database = DB>,
    for<'q> DB::Arguments<'q>: IntoArguments<'q, DB>,
{
    match first_on::<DB, T>(conn, failed, query, values).await? {
        Some(found) => Ok(found),
        None => Err(varyk_std::Error::new("the query gave no row".to_string())),
    }
}

/// `first` on `conn`.
async fn first_on<DB: Driver, T: DeserializeOwned>(
    conn: &mut DB::Connection,
    failed: &mut bool,
    query: &'static str,
    values: Vec<varyk_std::Value>,
) -> Result<Option<T>, varyk_std::Error>
where
    for<'c> &'c mut DB::Connection: Executor<'c, Database = DB>,
    for<'q> DB::Arguments<'q>: IntoArguments<'q, DB>,
{
    let query = to_send::<DB>(query, values)?;
    let bound = bind(
        conn,
        failed,
        &query.text,
        query.values,
        query.given,
        query.dollars,
    )
    .await?;
    let row = if DB::READS_TO_END {
        // sqlx's `fetch_optional` on Postgres and MySQL stops at the first
        // row and leaves the rest unread, so a statement that fails on a
        // later row would give `Ok` here: on Postgres its error is lost,
        // or surfaces on the transaction's next statement, and on MySQL it
        // is returned as the next statement's error on the connection,
        // which on a pool may be another request's. The rest is read to
        // the end. SQLite computes rows as they are read, so a row never
        // read never fails, and `fetch_optional` is right there.
        let mut rows = bound.fetch(&mut *conn);
        let first = next_row(&mut rows)
            .await
            .map_err(|e| statement_error(e, failed))?;
        while next_row(&mut rows)
            .await
            .map_err(|e| statement_error(e, failed))?
            .is_some()
        {}
        first
    } else {
        bound
            .fetch_optional(&mut *conn)
            .await
            .map_err(|e| statement_error(e, failed))?
    };
    row.as_ref().map(read_row::<DB, T>).transpose()
}

/// The next row of `rows`, or `None` at the end.
async fn next_row<S, R>(rows: &mut S) -> Result<Option<R>, sqlx::Error>
where
    S: Stream<Item = Result<R, sqlx::Error>> + Unpin,
{
    std::future::poll_fn(|cx| Pin::new(&mut *rows).poll_next(cx))
        .await
        .transpose()
}

/// `all` on `conn`.
async fn all_on<DB: Driver, T: DeserializeOwned>(
    conn: &mut DB::Connection,
    failed: &mut bool,
    query: &'static str,
    values: Vec<varyk_std::Value>,
) -> Result<Vec<T>, varyk_std::Error>
where
    for<'c> &'c mut DB::Connection: Executor<'c, Database = DB>,
    for<'q> DB::Arguments<'q>: IntoArguments<'q, DB>,
{
    let query = to_send::<DB>(query, values)?;
    let bound = bind(
        conn,
        failed,
        &query.text,
        query.values,
        query.given,
        query.dollars,
    )
    .await?;
    let rows = bound
        .fetch_all(&mut *conn)
        .await
        .map_err(|e| statement_error(e, failed))?;
    rows.iter().map(read_row::<DB, T>).collect()
}

/// `run` on `conn`.
async fn run_on<DB: Driver>(
    conn: &mut DB::Connection,
    failed: &mut bool,
    query: &'static str,
    values: Vec<varyk_std::Value>,
) -> Result<u64, varyk_std::Error>
where
    for<'c> &'c mut DB::Connection: Executor<'c, Database = DB>,
    for<'q> DB::Arguments<'q>: IntoArguments<'q, DB>,
{
    let query = to_send::<DB>(query, values)?;
    let bound = bind(
        conn,
        failed,
        &query.text,
        query.values,
        query.given,
        query.dollars,
    )
    .await?;
    let done = bound
        .execute(&mut *conn)
        .await
        .map_err(|e| statement_error(e, failed))?;
    Ok(DB::rows_affected(&done))
}

/// A query as it is sent: its text, the values bound to it in order,
/// the number of values the caller gave, and whether the text held a
/// `$n`.
struct Sendable {
    text: Cow<'static, str>,
    values: Vec<varyk_std::Value>,
    given: usize,
    dollars: bool,
}

/// `query` and `values` as they are sent. On SQLite and MySQL a query
/// whose text holds a `$` is first scanned and its placeholders checked,
/// an `Error` before anything is sent; on MySQL each `$n` then becomes a
/// `?`, and the values go in the order the placeholders appear, a value
/// used twice sent twice. A query with no `$`, and every query on
/// Postgres, goes as given.
fn to_send<DB: Driver>(
    query: &'static str,
    values: Vec<varyk_std::Value>,
) -> Result<Sendable, varyk_std::Error> {
    let given = values.len();
    if !DB::COUNTS_PLACEHOLDERS || !query.contains('$') {
        return Ok(Sendable {
            text: Cow::Borrowed(query),
            values,
            given,
            dollars: false,
        });
    }
    let found = scan_placeholders(query, DB::REWRITES_DOLLARS)?;
    check_placeholders(&found, given)?;
    let dollars = !found.dollars.is_empty();
    if !DB::REWRITES_DOLLARS || !dollars {
        return Ok(Sendable {
            text: Cow::Borrowed(query),
            values,
            given,
            dollars,
        });
    }
    // The checks have made every number name one of the values, and each
    // `$n` is ASCII, so its bytes are a slice of the text: an `Error`
    // here would be a bug in the scanner.
    let unplaced =
        || varyk_std::Error::new("varyk-sql could not place the query's values".to_string());
    let mut slots: Vec<Option<varyk_std::Value>> = values.into_iter().map(Some).collect();
    let mut text = String::with_capacity(query.len());
    let mut sent = Vec::with_capacity(found.dollars.len());
    let mut from = 0;
    for (k, dollar) in found.dollars.iter().enumerate() {
        // A value used again later is copied; its last use takes it.
        let again = found
            .dollars
            .iter()
            .skip(k + 1)
            .any(|later| later.number == dollar.number);
        let slot = dollar
            .number
            .and_then(|n| n.checked_sub(1))
            .and_then(|index| slots.get_mut(index));
        let value = match slot {
            Some(slot) if again => slot.clone(),
            Some(slot) => slot.take(),
            None => None,
        };
        match (query.get(from..dollar.at.start), value) {
            (Some(before), Some(value)) => {
                text.push_str(before);
                text.push('?');
                sent.push(value);
            }
            _ => return Err(unplaced()),
        }
        from = dollar.at.end;
    }
    text.push_str(query.get(from..).ok_or_else(unplaced)?);
    Ok(Sendable {
        text: Cow::Owned(text),
        values: sent,
        given,
        dollars,
    })
}

/// `text` with `values` bound to its placeholders, in order. On SQLite
/// and MySQL the number of values is first checked against the number
/// of placeholders the prepared statement reports, since SQLite would
/// bind a missing value as `NULL` and ignore an extra one. The `Error`
/// names `given`, the number of values the caller gave, which on MySQL
/// can differ from the number bound; when the text held a `$n`, a
/// difference means the database read its placeholders differently from
/// `scan_placeholders`, and the message says where to look. Postgres
/// rejects too few values itself and ignores an extra one, so a
/// Postgres statement is not prepared here.
///
/// Nor is a Postgres statement kept prepared after it runs: sqlx's
/// statement cache keys a statement by its text with the parameter types
/// of its first run on the connection, and later runs send their values
/// in binary into those types unchecked. A `None` is an untyped `NULL`,
/// whose type the server infers, and an integer is an `i64` whatever the
/// column, so a statement first run with one value would take a later
/// value's bytes as that first type. Each Postgres run is therefore
/// parsed again with its own values' types.
async fn bind<'q, DB: Driver>(
    conn: &mut DB::Connection,
    failed: &mut bool,
    text: &'q str,
    values: Vec<varyk_std::Value>,
    given: usize,
    dollars: bool,
) -> Result<Bound<'q, DB>, varyk_std::Error>
where
    for<'c> &'c mut DB::Connection: Executor<'c, Database = DB>,
{
    if DB::COUNTS_PLACEHOLDERS {
        let placeholders = (&mut *conn)
            .prepare(text)
            .await
            .map(|statement| placeholder_count(&statement))
            .map_err(|e| statement_error(e, failed))?;
        if let Some(placeholders) = placeholders {
            if placeholders != values.len() {
                let note = if dollars {
                    "; the database read the placeholders differently from varyk-sql; look for `$1::text` or one `$n` in two statements (SQLite), or a `/*! */` comment (MySQL)"
                } else {
                    ""
                };
                return Err(varyk_std::Error::new(format!(
                    "the query has {} but was given {}{note}",
                    counted(placeholders, "placeholder", "placeholders"),
                    counted(given, "value", "values"),
                )));
            }
        }
    }
    let mut bound = sqlx::query::<DB>(text).persistent(DB::COUNTS_PLACEHOLDERS);
    for value in values {
        bound = DB::bind_value(bound, value)?;
    }
    Ok(bound)
}

/// The number of placeholders `statement` reports, if it reports one.
fn placeholder_count<'q, S: Statement<'q>>(statement: &S) -> Option<usize> {
    match statement.parameters() {
        Some(Either::Left(types)) => Some(types.len()),
        Some(Either::Right(count)) => Some(count),
        None => None,
    }
}

/// `db_error` for a statement the database prepared or ran, setting
/// `failed` when the database refused it, on every database alike. A
/// row that cannot be read and the package's own placeholder checks (on
/// SQLite and MySQL) leave the transaction as it was, so they do not.
fn statement_error(e: sqlx::Error, failed: &mut bool) -> varyk_std::Error {
    if matches!(e, sqlx::Error::Database(_)) {
        *failed = true;
    }
    db_error(e)
}

/// `n` and the word for it: "1 value", "3 values".
fn counted(n: usize, one: &str, many: &str) -> String {
    if n == 1 {
        format!("1 {one}")
    } else {
        format!("{n} {many}")
    }
}

/// The placeholders `scan_placeholders` finds in a query's text: each
/// `$n`, in the order they appear, and whether a `?` appears.
struct Placeholders {
    dollars: Vec<Dollar>,
    question: bool,
}

/// One `$n` in a query's text: the bytes it covers, and its number, or
/// `None` when the number is too large to hold.
struct Dollar {
    at: Range<usize>,
    number: Option<usize>,
}

/// The `$n` and `?` placeholders of `text`, read by MySQL's rules when
/// `mysql` is true and SQLite's when it is false. What the database does
/// not read as SQL is skipped: `'…'` strings and `"…"` quoted text, where
/// a doubled quote stays inside and, on MySQL only, a backslash escapes
/// the next character; `` `…` `` quoted names, where a doubled backtick
/// stays inside, and on SQLite `[…]`; and comments: `--` to the end of
/// the line (on MySQL only when a space, a control character, or the end
/// of the text follows it), on MySQL `#` the same way, and `/* … */`.
/// A `$n` is a `$` and digits with no letter (any non-ASCII character
/// counts as one), digit, `_`, or `$` on either side, since both
/// databases allow `$` inside a name. A quote or a `/*` that never closes
/// is an `Error`, not a guess.
fn scan_placeholders(text: &str, mysql: bool) -> Result<Placeholders, varyk_std::Error> {
    let bytes = text.as_bytes();
    let mut found = Placeholders {
        dollars: Vec::new(),
        question: false,
    };
    let mut i = 0;
    while let Some(&b) = bytes.get(i) {
        let next = bytes.get(i + 1).copied();
        i = match b {
            b'\'' | b'"' => quoted(bytes, i, b, mysql)?,
            b'`' => quoted(bytes, i, b, false)?,
            b'[' if !mysql => match bytes.iter().skip(i + 1).position(|&c| c == b']') {
                Some(n) => i + n + 2,
                None => return Err(never_closes("quote")),
            },
            b'-' if next == Some(b'-') && (!mysql || opens_mysql_comment(bytes.get(i + 2))) => {
                line_end(bytes, i)
            }
            b'#' if mysql => line_end(bytes, i),
            b'/' if next == Some(b'*') => {
                match bytes.windows(2).skip(i + 2).position(|w| w == b"*/") {
                    Some(n) => i + n + 4,
                    None => return Err(never_closes("comment")),
                }
            }
            b'?' => {
                found.question = true;
                i + 1
            }
            b'$' => {
                let digits = bytes
                    .iter()
                    .skip(i + 1)
                    .take_while(|c| c.is_ascii_digit())
                    .count();
                let end = i + 1 + digits;
                let before = i.checked_sub(1).and_then(|j| bytes.get(j));
                if digits > 0 && !in_name(before) && !in_name(bytes.get(end)) {
                    let number = text.get(i + 1..end).and_then(|n| n.parse::<usize>().ok());
                    found.dollars.push(Dollar { at: i..end, number });
                    end
                } else {
                    i + 1
                }
            }
            _ => i + 1,
        };
    }
    Ok(found)
}

/// The index just past the quote `q` that `bytes[start]` opens, where a
/// doubled `q` stays inside and, when `backslash`, a backslash escapes
/// the next byte.
fn quoted(bytes: &[u8], start: usize, q: u8, backslash: bool) -> Result<usize, varyk_std::Error> {
    let mut i = start + 1;
    loop {
        match bytes.get(i) {
            None => return Err(never_closes("quote")),
            Some(&b'\\') if backslash => i += 2,
            Some(&c) if c == q => {
                if bytes.get(i + 1) == Some(&q) {
                    i += 2;
                } else {
                    return Ok(i + 1);
                }
            }
            Some(_) => i += 1,
        }
    }
}

/// Whether `--` followed by `after` starts a comment on MySQL: a space, a
/// control character, or the end of the text.
fn opens_mysql_comment(after: Option<&u8>) -> bool {
    after.is_none_or(|&c| c == b' ' || c.is_ascii_control())
}

/// The index of the end of the line `bytes[start]` is on: its newline, or
/// the end of the text.
fn line_end(bytes: &[u8], start: usize) -> usize {
    match bytes.iter().skip(start).position(|&c| c == b'\n') {
        Some(n) => start + n,
        None => bytes.len(),
    }
}

/// Whether `c` can be part of a name: a letter (any byte of a non-ASCII
/// character counts as one), a digit, `_`, or `$`.
fn in_name(c: Option<&u8>) -> bool {
    c.is_some_and(|&c| c.is_ascii_alphanumeric() || c == b'_' || c == b'$' || !c.is_ascii())
}

/// The `Error` for a quote or a comment that never closes.
fn never_closes(what: &str) -> varyk_std::Error {
    varyk_std::Error::new(format!("the query has a {what} that never closes"))
}

/// Whether `found`'s placeholders suit `values` values: a query with no
/// `$n` passes, left to the count of placeholders the database reports;
/// one with a `$n` holds no `?` and numbers exactly `$1` to `$k`, each
/// used at least once, where `k` is `values`. The first that fails gives
/// the message: mixing, `$0`, a number too large, the highest number,
/// then a number skipped.
fn check_placeholders(found: &Placeholders, values: usize) -> Result<(), varyk_std::Error> {
    if found.dollars.is_empty() {
        return Ok(());
    }
    let fail = |message: String| Err(varyk_std::Error::new(message));
    if found.question {
        return fail("use `$1`, `$2`, … or `?`, not both".to_string());
    }
    let numbers: Vec<Option<usize>> = found.dollars.iter().map(|d| d.number).collect();
    if numbers.contains(&Some(0)) {
        return fail("`$0` is not a placeholder; they start at `$1`".to_string());
    }
    if numbers.contains(&None) {
        return fail("`$` followed by a number too large to be a placeholder".to_string());
    }
    let mut numbers: Vec<usize> = numbers.into_iter().flatten().collect();
    numbers.sort_unstable();
    numbers.dedup();
    let highest = numbers.last().copied().unwrap_or(0);
    if highest != values {
        let uses = if highest == 1 {
            "`$1`".to_string()
        } else {
            format!("`$1` to `${highest}`")
        };
        return fail(format!(
            "the query uses {uses} but was given {}",
            counted(values, "value", "values")
        ));
    }
    match (1..).zip(&numbers).find(|&(n, &number)| number != n) {
        Some((skipped, _)) => fail(format!("the query skips `${skipped}`")),
        None => Ok(()),
    }
}

/// What differs by database: how a value is bound, how a cell is read,
/// and two facts about the server. Its rows are read by position, and its
/// statements can be kept prepared.
trait Driver: Database<Row: ByPosition> + HasStatementCache {
    /// Whether a statement is prepared first to count its placeholders
    /// and is kept prepared after it runs, and whether a query holding a
    /// `$` is scanned for its placeholders and checked before it is sent:
    /// true but on Postgres (see `to_send` and `bind`).
    const COUNTS_PLACEHOLDERS: bool;

    /// Whether each `$n` is rewritten to a `?`, with the values put in the
    /// order the placeholders appear: true on MySQL only (see `to_send`).
    const REWRITES_DOLLARS: bool;

    /// Whether `first` reads a result to its end: true but on SQLite (see
    /// `first_on`).
    const READS_TO_END: bool;

    /// `query` with `value` bound to its next placeholder.
    fn bind_value<'q>(
        query: Bound<'q, Self>,
        value: varyk_std::Value,
    ) -> Result<Bound<'q, Self>, varyk_std::Error>;

    /// The value of column `index` of `row`, by the driver's name for the
    /// value's type.
    fn datum(row: &Self::Row, index: usize) -> Result<Datum, ReadError>;

    /// Whether column `index` of `row` reads as `NULL`, as `datum` gives
    /// it.
    fn is_null(row: &Self::Row, index: usize) -> Result<bool, ReadError> {
        raw_value(row, index).map(|raw| raw.is_null())
    }

    /// The number of rows a statement changed.
    fn rows_affected(result: &Self::QueryResult) -> u64;
}

#[cfg(feature = "sqlite")]
impl Driver for sqlx::Sqlite {
    const COUNTS_PLACEHOLDERS: bool = true;
    const REWRITES_DOLLARS: bool = false;
    const READS_TO_END: bool = false;

    fn bind_value<'q>(
        query: Bound<'q, Self>,
        value: varyk_std::Value,
    ) -> Result<Bound<'q, Self>, varyk_std::Error> {
        bind_plain(query, value, |at| Ok(sqlite_time(at)))
    }

    fn datum(row: &sqlx::sqlite::SqliteRow, index: usize) -> Result<Datum, ReadError> {
        let raw = raw_value(row, index)?;
        if raw.is_null() {
            return Ok(Datum::Null);
        }
        // A SQLite value's type is its storage class, not the column's
        // declared type.
        let type_info = raw.type_info().into_owned();
        match type_info.name() {
            "INTEGER" => decode::<Self, i64>(row, index, raw).map(Datum::Int),
            "REAL" => decode::<Self, f64>(row, index, raw).map(Datum::Float),
            "TEXT" => decode::<Self, String>(row, index, raw).map(Datum::Text),
            "BLOB" => decode::<Self, Vec<u8>>(row, index, raw).map(Datum::Bytes),
            _ => Err(unlisted(row, index)),
        }
    }

    fn rows_affected(result: &sqlx::sqlite::SqliteQueryResult) -> u64 {
        result.rows_affected()
    }
}

#[cfg(feature = "postgres")]
impl Driver for sqlx::Postgres {
    const COUNTS_PLACEHOLDERS: bool = false;
    const REWRITES_DOLLARS: bool = false;
    const READS_TO_END: bool = true;

    fn bind_value<'q>(
        query: Bound<'q, Self>,
        value: varyk_std::Value,
    ) -> Result<Bound<'q, Self>, varyk_std::Error> {
        Ok(match value {
            varyk_std::Value::Null => query.bind(UntypedNull),
            varyk_std::Value::Bool(b) => query.bind(b),
            varyk_std::Value::Int(n) => query.bind(n),
            varyk_std::Value::Float(x) => query.bind(x),
            varyk_std::Value::Text(text) => query.bind(text),
            varyk_std::Value::Time(at) => query.bind(utc(at)?),
            varyk_std::Value::Uuid(id) => query.bind(uuid::Uuid::from_bytes(*id.as_bytes())),
            varyk_std::Value::Bytes(bytes) => query.bind(bytes.as_ref().to_vec()),
        })
    }

    fn datum(row: &sqlx::postgres::PgRow, index: usize) -> Result<Datum, ReadError> {
        let raw = raw_value(row, index)?;
        if raw.is_null() {
            return Ok(Datum::Null);
        }
        let type_info = raw.type_info().into_owned();
        match type_info.name() {
            // A function that returns `void`, `select pg_sleep(0)`, as 0.1
            // read it.
            "VOID" => Ok(Datum::Null),
            "BOOL" => decode::<Self, bool>(row, index, raw).map(Datum::Bool),
            "INT2" | "INT4" | "INT8" => decode::<Self, i64>(row, index, raw).map(Datum::Int),
            "FLOAT4" => decode::<Self, f32>(row, index, raw).map(|x| Datum::Float(f64::from(x))),
            "FLOAT8" => decode::<Self, f64>(row, index, raw).map(Datum::Float),
            "TEXT" | "VARCHAR" => decode::<Self, String>(row, index, raw).map(Datum::Text),
            "BYTEA" => decode::<Self, Vec<u8>>(row, index, raw).map(Datum::Bytes),
            // A `timestamp` with no zone is taken as UTC.
            "TIMESTAMPTZ" | "TIMESTAMP" => pg_time(row, index, raw),
            "UUID" => decode::<Self, uuid::Uuid>(row, index, raw)
                .map(|id| Datum::Text(varyk_std::Uuid::from_bytes(id.into_bytes()).to_string())),
            _ => Err(unlisted(row, index)),
        }
    }

    fn is_null(row: &sqlx::postgres::PgRow, index: usize) -> Result<bool, ReadError> {
        let raw = raw_value(row, index)?;
        Ok(raw.is_null() || raw.type_info().name() == "VOID")
    }

    fn rows_affected(result: &sqlx::postgres::PgQueryResult) -> u64 {
        result.rows_affected()
    }
}

/// The value of a Postgres `timestamptz` or `timestamp`, read from its
/// binary form, 8 big-endian bytes of microseconds since
/// 2000-01-01T00:00:00, rather than through sqlx's decoder, which panics
/// on `infinity` and past the year 9999 (spec §1.1). Any other shape is an
/// `Error` naming the column, and `infinity` (`i64::MAX`), `-infinity`
/// (`i64::MIN`), and every time outside `Time`'s range one that says so.
#[cfg(feature = "postgres")]
fn pg_time(
    row: &sqlx::postgres::PgRow,
    index: usize,
    raw: sqlx::postgres::PgValueRef<'_>,
) -> Result<Datum, ReadError> {
    /// 2000-01-01T00:00:00Z in microseconds since 1970.
    const FROM_1970: i64 = 946_684_800_000_000;
    let binary = match raw.format() {
        sqlx::postgres::PgValueFormat::Binary => raw.as_bytes().ok(),
        sqlx::postgres::PgValueFormat::Text => None,
    };
    match binary.and_then(|bytes| <[u8; 8]>::try_from(bytes).ok()) {
        Some(bytes) => time_datum(row, index, i64::from_be_bytes(bytes).checked_add(FROM_1970)),
        None => Err(column_error(row, index, "cannot be decoded")),
    }
}

/// A time column's value, `micros` since 1970 in UTC, as the written form
/// of a `Time`, which a `Time` field reads back; `None`, or a time outside
/// `Time`'s range, is an `Error` naming the column and not the value.
#[cfg(any(feature = "postgres", feature = "mysql"))]
fn time_datum<R: Row>(row: &R, index: usize, micros: Option<i64>) -> Result<Datum, ReadError> {
    micros
        .and_then(|micros| varyk_std::Time::from_unix_micros(micros).ok())
        .map(|at| Datum::Text(at.to_iso()))
        .ok_or_else(|| column_error(row, index, "holds a time out of the range of a `Time`"))
}

/// A Postgres `NULL` of type OID 0, "unspecified": the server infers the
/// parameter's type from where it stands in the query, so `None` needs no
/// cast where the query gives it a type (spec §2.2).
#[cfg(feature = "postgres")]
struct UntypedNull;

#[cfg(feature = "postgres")]
impl sqlx::Type<sqlx::Postgres> for UntypedNull {
    fn type_info() -> sqlx::postgres::PgTypeInfo {
        sqlx::postgres::PgTypeInfo::with_oid(sqlx::postgres::types::Oid(0))
    }
}

#[cfg(feature = "postgres")]
impl sqlx::Encode<'_, sqlx::Postgres> for UntypedNull {
    fn encode_by_ref(
        &self,
        _buf: &mut sqlx::postgres::PgArgumentBuffer,
    ) -> Result<sqlx::encode::IsNull, sqlx::error::BoxDynError> {
        Ok(sqlx::encode::IsNull::Yes)
    }
}

#[cfg(feature = "mysql")]
impl Driver for sqlx::MySql {
    const COUNTS_PLACEHOLDERS: bool = true;
    const REWRITES_DOLLARS: bool = true;
    const READS_TO_END: bool = true;

    fn bind_value<'q>(
        query: Bound<'q, Self>,
        value: varyk_std::Value,
    ) -> Result<Bound<'q, Self>, varyk_std::Error> {
        // A `DATETIME(6)` value with no zone, in UTC: sqlx sets the
        // session's time zone to UTC, so a `TIMESTAMP` column holds the
        // same instant.
        bind_plain(query, value, |at| {
            utc(at).map(|utc| time::PrimitiveDateTime::new(utc.date(), utc.time()))
        })
    }

    fn datum(row: &sqlx::mysql::MySqlRow, index: usize) -> Result<Datum, ReadError> {
        if Self::is_null(row, index)? {
            return Ok(Datum::Null);
        }
        let raw = raw_value(row, index)?;
        // A column with a binary collation carries the binary flag, so
        // MySQL names it as bytes (`VARBINARY`, `BLOB`) though it holds
        // text; a `string` field reads bytes that are UTF-8 as text.
        let type_info = raw.type_info().into_owned();
        match type_info.name() {
            // sqlx names every `tinyint(1)` `BOOLEAN`, an unsigned one too,
            // whose value would read as signed: 200 as -56.
            "BOOLEAN" if <u8 as sqlx::Type<Self>>::compatible(&type_info) => {
                decode::<Self, u64>(row, index, raw).map(Datum::Unsigned)
            }
            "BOOLEAN" | "TINYINT" | "SMALLINT" | "MEDIUMINT" | "INT" | "BIGINT" => {
                decode::<Self, i64>(row, index, raw).map(Datum::Int)
            }
            "TINYINT UNSIGNED" | "SMALLINT UNSIGNED" | "MEDIUMINT UNSIGNED" | "INT UNSIGNED"
            | "BIGINT UNSIGNED" => decode::<Self, u64>(row, index, raw).map(Datum::Unsigned),
            // A value with no zone, in UTC, the session's time zone. A zero
            // date does not decode, an `Error` naming the column.
            "DATETIME" | "TIMESTAMP" => {
                let at = decode::<Self, time::PrimitiveDateTime>(row, index, raw)?.assume_utc();
                let micros = i64::try_from(at.unix_timestamp_nanos().div_euclid(1000)).ok();
                time_datum(row, index, micros)
            }
            "FLOAT" => decode::<Self, f32>(row, index, raw).map(|x| Datum::Float(f64::from(x))),
            "DOUBLE" => decode::<Self, f64>(row, index, raw).map(Datum::Float),
            "CHAR" | "VARCHAR" | "ENUM" | "TINYTEXT" | "TEXT" | "MEDIUMTEXT" | "LONGTEXT" => {
                decode::<Self, String>(row, index, raw).map(Datum::Text)
            }
            "BINARY" | "VARBINARY" | "TINYBLOB" | "BLOB" | "MEDIUMBLOB" | "LONGBLOB" => {
                decode::<Self, Vec<u8>>(row, index, raw).map(Datum::Bytes)
            }
            _ => Err(unlisted(row, index)),
        }
    }

    /// Only a value the server sent as `NULL`: sqlx's `is_null` also says
    /// so of a zero date (`0000-00-00`), which would then read as `None`;
    /// here it is a value, which no `Time` reads, so an `Option<Time>`
    /// field is an `Error` naming the column for it, as a `Time` field is.
    /// The raw bytes are there exactly when the server sent a value.
    fn is_null(row: &sqlx::mysql::MySqlRow, index: usize) -> Result<bool, ReadError> {
        let raw = raw_value(row, index)?;
        Ok(<&[u8] as Decode<'_, Self>>::decode(raw).is_err())
    }

    fn rows_affected(result: &sqlx::mysql::MySqlQueryResult) -> u64 {
        result.rows_affected()
    }
}

/// `bind_value` on SQLite and MySQL, where `None` is an integer `NULL`, a
/// `Uuid` is its written form, 36 characters in lower case, and a `Time`
/// is what `write_time` makes of it.
#[cfg(any(feature = "sqlite", feature = "mysql"))]
fn bind_plain<'q, DB: Database, T>(
    query: Bound<'q, DB>,
    value: varyk_std::Value,
    write_time: impl FnOnce(varyk_std::Time) -> Result<T, varyk_std::Error>,
) -> Result<Bound<'q, DB>, varyk_std::Error>
where
    Option<i64>: sqlx::Encode<'q, DB> + sqlx::Type<DB>,
    bool: sqlx::Encode<'q, DB> + sqlx::Type<DB>,
    i64: sqlx::Encode<'q, DB> + sqlx::Type<DB>,
    f64: sqlx::Encode<'q, DB> + sqlx::Type<DB>,
    String: sqlx::Encode<'q, DB> + sqlx::Type<DB>,
    Vec<u8>: sqlx::Encode<'q, DB> + sqlx::Type<DB>,
    T: sqlx::Encode<'q, DB> + sqlx::Type<DB> + Send + 'q,
{
    Ok(match value {
        varyk_std::Value::Null => query.bind(None::<i64>),
        varyk_std::Value::Bool(b) => query.bind(b),
        varyk_std::Value::Int(n) => query.bind(n),
        varyk_std::Value::Float(x) => query.bind(x),
        varyk_std::Value::Text(text) => query.bind(text),
        varyk_std::Value::Time(at) => query.bind(write_time(at)?),
        varyk_std::Value::Uuid(id) => query.bind(id.to_string()),
        varyk_std::Value::Bytes(bytes) => query.bind(bytes.as_ref().to_vec()),
    })
}

/// `at` as the `time` crate's date and time in UTC, which sqlx encodes.
/// Every `Time` is inside the crate's range; one it refused would be an
/// `Error` that names no value.
#[cfg(any(feature = "postgres", feature = "mysql"))]
fn utc(at: varyk_std::Time) -> Result<time::OffsetDateTime, varyk_std::Error> {
    time::OffsetDateTime::from_unix_timestamp_nanos(i128::from(at.to_unix_micros()) * 1000).map_err(
        |_| {
            varyk_std::Error::new(
                "a `Time` is out of the range the database driver takes".to_string(),
            )
        },
    )
}

/// `t` as SQLite stores it: text of a fixed width,
/// `2026-10-07T12:00:00.000000Z`, so `order by` and `<` in SQL agree with
/// `<` in Varyk, which text with trailing zeros dropped does not. The
/// date is worked out from the microseconds with Euclidean division, so a
/// time before 1970 gives the day and second it falls in, and every
/// `Time`, from the year 0000 to 9999, has one.
#[cfg_attr(not(feature = "sqlite"), allow(dead_code))]
fn sqlite_time(t: varyk_std::Time) -> String {
    const MICROS: i64 = 1_000_000;
    const DAY: i64 = 86_400;
    let micros = t.to_unix_micros();
    let (seconds, fraction) = (micros.div_euclid(MICROS), micros.rem_euclid(MICROS));
    let (days, second) = (seconds.div_euclid(DAY), seconds.rem_euclid(DAY));
    // The civil date of a day count, by Howard Hinnant's `civil_from_days`,
    // in 400-year eras that start on 0000-03-01.
    let shifted = days + 719_468;
    let (era, day_of_era) = (shifted.div_euclid(146_097), shifted.rem_euclid(146_097));
    let year_of_era =
        (day_of_era - day_of_era / 1_460 + day_of_era / 36_524 - day_of_era / 146_096) / 365;
    let day_of_year = day_of_era - (365 * year_of_era + year_of_era / 4 - year_of_era / 100);
    let month_from_march = (5 * day_of_year + 2) / 153;
    let day = day_of_year - (153 * month_from_march + 2) / 5 + 1;
    let month = if month_from_march < 10 {
        month_from_march + 3
    } else {
        month_from_march - 9
    };
    let year = era * 400 + year_of_era + i64::from(month <= 2);
    format!(
        "{year:04}-{month:02}-{day:02}T{:02}:{:02}:{:02}.{fraction:06}Z",
        second / 3_600,
        second / 60 % 60,
        second % 60,
    )
}

/// The raw value of column `index` of `row`.
fn raw_value<R: ByPosition>(
    row: &R,
    index: usize,
) -> Result<<R::Database as Database>::ValueRef<'_>, ReadError> {
    row.raw_at(index)
        .map_err(|_| column_error(row, index, "is not in the row"))
}

/// A row whose columns are found by position, as every driver's are.
/// `Driver` carries it, so code generic over the driver can read a cell.
trait ByPosition: Row {
    fn raw_at(
        &self,
        index: usize,
    ) -> Result<<Self::Database as Database>::ValueRef<'_>, sqlx::Error>;
}

impl<R: Row> ByPosition for R
where
    usize: ColumnIndex<R>,
{
    fn raw_at(
        &self,
        index: usize,
    ) -> Result<<Self::Database as Database>::ValueRef<'_>, sqlx::Error> {
        self.try_get_raw(index)
    }
}

/// `raw`, the value of column `index` of `row`, decoded as a `T`.
fn decode<'r, DB: Database, T: Decode<'r, DB>>(
    row: &DB::Row,
    index: usize,
    raw: DB::ValueRef<'r>,
) -> Result<T, ReadError> {
    T::decode(raw).map_err(|_| column_error(row, index, "cannot be decoded"))
}

/// The `Error` for a column of a type the driver's reader does not list.
fn unlisted<R: Row>(row: &R, index: usize) -> ReadError {
    column_error(
        row,
        index,
        "is of a type varyk-sql cannot read, such as `numeric`, `json`, `date`, `time`, or an array; cast it in the query",
    )
}

/// An error about column `index` of `row`: "column `age` ...".
fn column_error<R: Row>(row: &R, index: usize, what: &str) -> ReadError {
    let name = row.columns().get(index).map_or("?", |column| column.name());
    ReadError::Own(format!("column `{name}` {what}"))
}

/// Reads `row` as a `T` by column name (spec §2.5 and §2.6): a struct
/// takes each field from the column of its name, and a number, `bool`,
/// `string`, `Time`, `Uuid`, or `Bytes`, or an `Option` of one, takes the
/// row's only column.
fn read_row<DB: Driver, T: DeserializeOwned>(row: &DB::Row) -> Result<T, varyk_std::Error> {
    T::deserialize(RowReader::<DB> { row }).map_err(|e| varyk_std::Error::new(e.to_string()))
}

/// A failure while reading a row, as the message `read_row` returns.
#[derive(Debug)]
enum ReadError {
    /// varyk-sql's own message, which names at most a column, a type, or
    /// a count.
    Own(String),
    /// A field's deserializer refused the value it was handed. Its text
    /// can hold the value (a `Uuid`'s names the text it read), so it is
    /// dropped; where a cell meets a field, `refused_in` names the column
    /// instead.
    Refused,
}

impl ReadError {
    /// This error of reading column `index` of `row` into a field: a
    /// field's refusal becomes "column `x` cannot be read into the field",
    /// and the cell's own messages (`NULL`, out of range, a type to cast)
    /// stay as they are.
    fn refused_in<R: Row>(self, row: &R, index: usize) -> ReadError {
        match self {
            ReadError::Refused => column_error(row, index, "cannot be read into the field"),
            own => own,
        }
    }
}

impl fmt::Display for ReadError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            ReadError::Own(message) => f.write_str(message),
            ReadError::Refused => f.write_str("a column cannot be read into its field"),
        }
    }
}

impl std::error::Error for ReadError {}

impl de::Error for ReadError {
    /// A field's refusal: serde's other constructors (`invalid_type`,
    /// `invalid_length`, ...) come here too.
    fn custom<M: fmt::Display>(_message: M) -> Self {
        ReadError::Refused
    }

    fn missing_field(field: &'static str) -> Self {
        ReadError::Own(format!(
            "the row has no column `{field}`, which a field of the type needs"
        ))
    }

    fn duplicate_field(field: &'static str) -> Self {
        ReadError::Own(format!(
            "the row has two columns named `{field}`; name them apart with `as`"
        ))
    }
}

/// A row read as a whole: a struct by column name, anything else from
/// its only column.
struct RowReader<'r, DB: Driver> {
    row: &'r DB::Row,
}

impl<'r, DB: Driver> RowReader<'r, DB> {
    /// `read` of the row's only column, with a field's refusal naming
    /// the column; an `Error` naming the count when the row has another
    /// number of columns.
    fn only<T>(
        self,
        read: impl FnOnce(Cell<'r, DB>) -> Result<T, ReadError>,
    ) -> Result<T, ReadError> {
        self.only_or("", read)
    }

    /// `only`, with `hint` after the count in the `Error`.
    fn only_or<T>(
        self,
        hint: &str,
        read: impl FnOnce(Cell<'r, DB>) -> Result<T, ReadError>,
    ) -> Result<T, ReadError> {
        let count = self.row.columns().len();
        if count == 1 {
            let cell = Cell {
                row: self.row,
                index: 0,
            };
            read(cell).map_err(|e| e.refused_in(self.row, 0))
        } else {
            Err(ReadError::Own(format!(
                "a row read as one value must have one column; this one has {count}{hint}"
            )))
        }
    }

    fn not_a_list() -> ReadError {
        ReadError::Own(
            "a row cannot be read as a list or a map; read it into a struct, or its one column into a number, `bool`, `string`, `Time`, `Uuid`, or `Bytes`"
                .to_string(),
        )
    }
}

impl<'de, 'r, DB: Driver> de::Deserializer<'de> for RowReader<'r, DB> {
    type Error = ReadError;

    fn deserialize_struct<V: Visitor<'de>>(
        self,
        _name: &'static str,
        _fields: &'static [&'static str],
        visitor: V,
    ) -> Result<V::Value, ReadError> {
        visitor.visit_map(Columns::<DB> {
            row: self.row,
            next: 0,
        })
    }

    fn deserialize_seq<V: Visitor<'de>>(self, _visitor: V) -> Result<V::Value, ReadError> {
        Err(Self::not_a_list())
    }

    fn deserialize_tuple<V: Visitor<'de>>(
        self,
        _len: usize,
        _visitor: V,
    ) -> Result<V::Value, ReadError> {
        Err(Self::not_a_list())
    }

    fn deserialize_tuple_struct<V: Visitor<'de>>(
        self,
        _name: &'static str,
        _len: usize,
        _visitor: V,
    ) -> Result<V::Value, ReadError> {
        Err(Self::not_a_list())
    }

    fn deserialize_map<V: Visitor<'de>>(self, _visitor: V) -> Result<V::Value, ReadError> {
        Err(Self::not_a_list())
    }

    fn deserialize_any<V: Visitor<'de>>(self, visitor: V) -> Result<V::Value, ReadError> {
        self.only(|cell| cell.deserialize_any(visitor))
    }

    fn deserialize_bool<V: Visitor<'de>>(self, visitor: V) -> Result<V::Value, ReadError> {
        self.only(|cell| cell.deserialize_bool(visitor))
    }

    fn deserialize_i8<V: Visitor<'de>>(self, visitor: V) -> Result<V::Value, ReadError> {
        self.only(|cell| cell.deserialize_i8(visitor))
    }

    fn deserialize_i16<V: Visitor<'de>>(self, visitor: V) -> Result<V::Value, ReadError> {
        self.only(|cell| cell.deserialize_i16(visitor))
    }

    fn deserialize_i32<V: Visitor<'de>>(self, visitor: V) -> Result<V::Value, ReadError> {
        self.only(|cell| cell.deserialize_i32(visitor))
    }

    fn deserialize_i64<V: Visitor<'de>>(self, visitor: V) -> Result<V::Value, ReadError> {
        self.only(|cell| cell.deserialize_i64(visitor))
    }

    fn deserialize_u8<V: Visitor<'de>>(self, visitor: V) -> Result<V::Value, ReadError> {
        self.only(|cell| cell.deserialize_u8(visitor))
    }

    fn deserialize_u16<V: Visitor<'de>>(self, visitor: V) -> Result<V::Value, ReadError> {
        self.only(|cell| cell.deserialize_u16(visitor))
    }

    fn deserialize_u32<V: Visitor<'de>>(self, visitor: V) -> Result<V::Value, ReadError> {
        self.only(|cell| cell.deserialize_u32(visitor))
    }

    fn deserialize_u64<V: Visitor<'de>>(self, visitor: V) -> Result<V::Value, ReadError> {
        self.only(|cell| cell.deserialize_u64(visitor))
    }

    fn deserialize_f32<V: Visitor<'de>>(self, visitor: V) -> Result<V::Value, ReadError> {
        self.only(|cell| cell.deserialize_f32(visitor))
    }

    fn deserialize_f64<V: Visitor<'de>>(self, visitor: V) -> Result<V::Value, ReadError> {
        self.only(|cell| cell.deserialize_f64(visitor))
    }

    fn deserialize_char<V: Visitor<'de>>(self, visitor: V) -> Result<V::Value, ReadError> {
        self.only(|cell| cell.deserialize_char(visitor))
    }

    fn deserialize_str<V: Visitor<'de>>(self, visitor: V) -> Result<V::Value, ReadError> {
        self.only(|cell| cell.deserialize_str(visitor))
    }

    fn deserialize_string<V: Visitor<'de>>(self, visitor: V) -> Result<V::Value, ReadError> {
        self.only(|cell| cell.deserialize_string(visitor))
    }

    fn deserialize_bytes<V: Visitor<'de>>(self, visitor: V) -> Result<V::Value, ReadError> {
        self.only(|cell| cell.deserialize_bytes(visitor))
    }

    fn deserialize_byte_buf<V: Visitor<'de>>(self, visitor: V) -> Result<V::Value, ReadError> {
        self.only(|cell| cell.deserialize_byte_buf(visitor))
    }

    fn deserialize_option<V: Visitor<'de>>(self, visitor: V) -> Result<V::Value, ReadError> {
        // `let u: Option<User> = db.one(..)` lands here: a row that may be
        // missing is `first`'s, which reads the row into `User`.
        self.only_or("; to read a row that may be missing, use `first`", |cell| {
            cell.deserialize_option(visitor)
        })
    }

    fn deserialize_unit<V: Visitor<'de>>(self, visitor: V) -> Result<V::Value, ReadError> {
        self.only(|cell| cell.deserialize_unit(visitor))
    }

    fn deserialize_unit_struct<V: Visitor<'de>>(
        self,
        name: &'static str,
        visitor: V,
    ) -> Result<V::Value, ReadError> {
        self.only(|cell| cell.deserialize_unit_struct(name, visitor))
    }

    fn deserialize_newtype_struct<V: Visitor<'de>>(
        self,
        name: &'static str,
        visitor: V,
    ) -> Result<V::Value, ReadError> {
        self.only(|cell| cell.deserialize_newtype_struct(name, visitor))
    }

    fn deserialize_enum<V: Visitor<'de>>(
        self,
        name: &'static str,
        variants: &'static [&'static str],
        visitor: V,
    ) -> Result<V::Value, ReadError> {
        self.only(|cell| cell.deserialize_enum(name, variants, visitor))
    }

    fn deserialize_identifier<V: Visitor<'de>>(self, visitor: V) -> Result<V::Value, ReadError> {
        self.only(|cell| cell.deserialize_identifier(visitor))
    }

    fn deserialize_ignored_any<V: Visitor<'de>>(self, visitor: V) -> Result<V::Value, ReadError> {
        visitor.visit_unit()
    }
}

/// A row's columns as a map from column name to value, for a struct.
struct Columns<'r, DB: Driver> {
    row: &'r DB::Row,
    next: usize,
}

impl<'de, 'r, DB: Driver> MapAccess<'de> for Columns<'r, DB> {
    type Error = ReadError;

    fn next_key_seed<K: DeserializeSeed<'de>>(
        &mut self,
        seed: K,
    ) -> Result<Option<K::Value>, ReadError> {
        match self.row.columns().get(self.next) {
            Some(column) => seed
                .deserialize(column.name().into_deserializer())
                .map(Some),
            None => Ok(None),
        }
    }

    fn next_value_seed<V: DeserializeSeed<'de>>(&mut self, seed: V) -> Result<V::Value, ReadError> {
        let index = self.next;
        self.next += 1;
        seed.deserialize(Cell::<DB> {
            row: self.row,
            index,
        })
        .map_err(|e| e.refused_in(self.row, index))
    }
}

/// One column of a row, read into one field.
struct Cell<'r, DB: Driver> {
    row: &'r DB::Row,
    index: usize,
}

/// A column's value, as the deserializer reads it.
enum Datum {
    Null,
    /// A Postgres boolean.
    #[cfg_attr(not(feature = "postgres"), allow(dead_code))]
    Bool(bool),
    Int(i64),
    /// A MySQL `... UNSIGNED` integer.
    #[cfg_attr(not(feature = "mysql"), allow(dead_code))]
    Unsigned(u64),
    Float(f64),
    Text(String),
    Bytes(Vec<u8>),
}

impl Datum {
    fn kind(&self) -> &'static str {
        match self {
            Datum::Null => "NULL",
            Datum::Bool(_) => "a boolean",
            Datum::Int(_) | Datum::Unsigned(_) => "an integer",
            Datum::Float(_) => "a float",
            Datum::Text(_) => "text",
            Datum::Bytes(_) => "bytes",
        }
    }
}

impl<'r, DB: Driver> Cell<'r, DB> {
    /// An error about this column: "column `age` ...".
    fn error(&self, what: &str) -> ReadError {
        column_error(self.row, self.index, what)
    }

    fn is_null(&self) -> Result<bool, ReadError> {
        DB::is_null(self.row, self.index)
    }

    /// The column's value. A value is never put in a message, so no
    /// row's data reaches a log through an error.
    fn datum(&self) -> Result<Datum, ReadError> {
        DB::datum(self.row, self.index)
    }

    /// The column's value, which must not be `NULL`.
    fn present(&self) -> Result<Datum, ReadError> {
        match self.datum()? {
            Datum::Null => Err(self.error("is NULL; read it into an `Option` field")),
            datum => Ok(datum),
        }
    }

    fn mismatch(&self, datum: &Datum, field: &str) -> ReadError {
        self.error(&format!(
            "holds {}, which cannot be read into {field}",
            datum.kind()
        ))
    }

    /// An integer field of type `T`, named `field` in messages: an
    /// integer column in range, or a float column with no fractional
    /// part in range.
    fn integer<T: TryFrom<i128>>(&self, field: &str) -> Result<T, ReadError> {
        match self.present()? {
            Datum::Int(n) => T::try_from(i128::from(n))
                .map_err(|_| self.error(&format!("holds an integer out of the range of {field}"))),
            Datum::Unsigned(n) => T::try_from(i128::from(n))
                .map_err(|_| self.error(&format!("holds an integer out of the range of {field}"))),
            Datum::Float(x) if x.is_finite() && x.fract() == 0.0 => {
                // `as` saturates, and a saturated value fails `try_from`.
                T::try_from(x as i128)
                    .map_err(|_| self.error(&format!("holds a number out of the range of {field}")))
            }
            Datum::Float(_) => Err(self.error(&format!(
                "holds a float with a fractional part, which cannot be read into {field}"
            ))),
            other => Err(self.mismatch(&other, field)),
        }
    }

    fn float(&self, field: &str) -> Result<f64, ReadError> {
        match self.present()? {
            Datum::Float(x) => Ok(x),
            Datum::Int(n) => Ok(n as f64),
            Datum::Unsigned(n) => Ok(n as f64),
            other => Err(self.mismatch(&other, field)),
        }
    }

    /// A field that asks for a string, `field` in messages: a text
    /// column, a time or an id as its written form, handed over as text,
    /// and a binary column handed over raw. The field takes or refuses
    /// what it gets: a `string` takes UTF-8 bytes as their text (MySQL
    /// names a text column with a binary collation as bytes), a `Uuid`
    /// takes 16 bytes, and a `Bytes` takes any.
    fn text<'de, V: Visitor<'de>>(self, visitor: V, field: &str) -> Result<V::Value, ReadError> {
        match self.present()? {
            Datum::Text(text) => visitor.visit_string(text),
            Datum::Bytes(bytes) => visitor.visit_byte_buf(bytes),
            other => Err(self.mismatch(&other, field)),
        }
    }

    fn not_one_value(&self, what: &str) -> ReadError {
        self.error(&format!(
            "cannot be read into {what}; a column holds one number, `bool`, `string`, `Time`, `Uuid`, or `Bytes`"
        ))
    }
}

impl<'de, 'r, DB: Driver> de::Deserializer<'de> for Cell<'r, DB> {
    type Error = ReadError;

    fn deserialize_any<V: Visitor<'de>>(self, visitor: V) -> Result<V::Value, ReadError> {
        match self.datum()? {
            Datum::Null => visitor.visit_none(),
            Datum::Bool(b) => visitor.visit_bool(b),
            Datum::Int(n) => visitor.visit_i64(n),
            Datum::Unsigned(n) => visitor.visit_u64(n),
            Datum::Float(x) => visitor.visit_f64(x),
            Datum::Text(text) => visitor.visit_string(text),
            Datum::Bytes(bytes) => visitor.visit_byte_buf(bytes),
        }
    }

    fn deserialize_bool<V: Visitor<'de>>(self, visitor: V) -> Result<V::Value, ReadError> {
        match self.present()? {
            Datum::Bool(b) => visitor.visit_bool(b),
            Datum::Int(0) | Datum::Unsigned(0) => visitor.visit_bool(false),
            Datum::Int(1) | Datum::Unsigned(1) => visitor.visit_bool(true),
            Datum::Int(_) | Datum::Unsigned(_) => Err(self
                .error("holds an integer other than 0 or 1, which cannot be read into a `bool`")),
            other => Err(self.mismatch(&other, "a `bool`")),
        }
    }

    fn deserialize_i8<V: Visitor<'de>>(self, visitor: V) -> Result<V::Value, ReadError> {
        visitor.visit_i8(self.integer("an `i8`")?)
    }

    fn deserialize_i16<V: Visitor<'de>>(self, visitor: V) -> Result<V::Value, ReadError> {
        visitor.visit_i16(self.integer("an `i16`")?)
    }

    fn deserialize_i32<V: Visitor<'de>>(self, visitor: V) -> Result<V::Value, ReadError> {
        visitor.visit_i32(self.integer("an `i32`")?)
    }

    fn deserialize_i64<V: Visitor<'de>>(self, visitor: V) -> Result<V::Value, ReadError> {
        visitor.visit_i64(self.integer("an `i64`")?)
    }

    fn deserialize_u8<V: Visitor<'de>>(self, visitor: V) -> Result<V::Value, ReadError> {
        visitor.visit_u8(self.integer("a `u8`")?)
    }

    fn deserialize_u16<V: Visitor<'de>>(self, visitor: V) -> Result<V::Value, ReadError> {
        visitor.visit_u16(self.integer("a `u16`")?)
    }

    fn deserialize_u32<V: Visitor<'de>>(self, visitor: V) -> Result<V::Value, ReadError> {
        visitor.visit_u32(self.integer("a `u32`")?)
    }

    fn deserialize_u64<V: Visitor<'de>>(self, visitor: V) -> Result<V::Value, ReadError> {
        visitor.visit_u64(self.integer("a `u64`")?)
    }

    fn deserialize_f32<V: Visitor<'de>>(self, visitor: V) -> Result<V::Value, ReadError> {
        visitor.visit_f32(self.float("an `f32`")? as f32)
    }

    fn deserialize_f64<V: Visitor<'de>>(self, visitor: V) -> Result<V::Value, ReadError> {
        visitor.visit_f64(self.float("an `f64`")?)
    }

    /// A `Time`, a `Uuid`, or a `Bytes` asks for this (M5c §5).
    fn deserialize_str<V: Visitor<'de>>(self, visitor: V) -> Result<V::Value, ReadError> {
        self.text(visitor, "a `Time`, `Uuid`, `Bytes`, or `string`")
    }

    fn deserialize_string<V: Visitor<'de>>(self, visitor: V) -> Result<V::Value, ReadError> {
        self.text(visitor, "a `string`")
    }

    fn deserialize_option<V: Visitor<'de>>(self, visitor: V) -> Result<V::Value, ReadError> {
        if self.is_null()? {
            visitor.visit_none()
        } else {
            visitor.visit_some(self)
        }
    }

    fn deserialize_newtype_struct<V: Visitor<'de>>(
        self,
        _name: &'static str,
        visitor: V,
    ) -> Result<V::Value, ReadError> {
        visitor.visit_newtype_struct(self)
    }

    fn deserialize_identifier<V: Visitor<'de>>(self, visitor: V) -> Result<V::Value, ReadError> {
        self.deserialize_string(visitor)
    }

    fn deserialize_ignored_any<V: Visitor<'de>>(self, visitor: V) -> Result<V::Value, ReadError> {
        visitor.visit_unit()
    }

    fn deserialize_char<V: Visitor<'de>>(self, _visitor: V) -> Result<V::Value, ReadError> {
        Err(self.not_one_value("a character"))
    }

    fn deserialize_bytes<V: Visitor<'de>>(self, _visitor: V) -> Result<V::Value, ReadError> {
        Err(self.not_one_value("bytes"))
    }

    fn deserialize_byte_buf<V: Visitor<'de>>(self, _visitor: V) -> Result<V::Value, ReadError> {
        Err(self.not_one_value("bytes"))
    }

    fn deserialize_unit<V: Visitor<'de>>(self, _visitor: V) -> Result<V::Value, ReadError> {
        Err(self.not_one_value("nothing"))
    }

    fn deserialize_unit_struct<V: Visitor<'de>>(
        self,
        _name: &'static str,
        _visitor: V,
    ) -> Result<V::Value, ReadError> {
        Err(self.not_one_value("a struct"))
    }

    fn deserialize_seq<V: Visitor<'de>>(self, _visitor: V) -> Result<V::Value, ReadError> {
        Err(self.not_one_value("a list"))
    }

    fn deserialize_tuple<V: Visitor<'de>>(
        self,
        _len: usize,
        _visitor: V,
    ) -> Result<V::Value, ReadError> {
        Err(self.not_one_value("a list"))
    }

    fn deserialize_tuple_struct<V: Visitor<'de>>(
        self,
        _name: &'static str,
        _len: usize,
        _visitor: V,
    ) -> Result<V::Value, ReadError> {
        Err(self.not_one_value("a list"))
    }

    fn deserialize_map<V: Visitor<'de>>(self, _visitor: V) -> Result<V::Value, ReadError> {
        Err(self.not_one_value("a map"))
    }

    fn deserialize_struct<V: Visitor<'de>>(
        self,
        _name: &'static str,
        _fields: &'static [&'static str],
        _visitor: V,
    ) -> Result<V::Value, ReadError> {
        Err(self.not_one_value("a struct"))
    }

    fn deserialize_enum<V: Visitor<'de>>(
        self,
        _name: &'static str,
        _variants: &'static [&'static str],
        _visitor: V,
    ) -> Result<V::Value, ReadError> {
        Err(self.not_one_value("an enum"))
    }
}

/// The error of a pool that could not open its first connection. Only
/// the variants whose message is fixed or the database's own go through
/// `db_error`; any other gives a fixed message, since a driver's text on
/// this path could hold the URL.
fn connect_error(e: sqlx::Error) -> varyk_std::Error {
    match e {
        sqlx::Error::Io(err) => varyk_std::Error::new(format!("cannot reach the database: {err}")),
        sqlx::Error::Database(db) => {
            varyk_std::Error::new(format!("cannot connect to the database: {}", db.message()))
        }
        e @ (sqlx::Error::Configuration(_)
        | sqlx::Error::Tls(_)
        | sqlx::Error::PoolTimedOut
        | sqlx::Error::PoolClosed) => db_error(e),
        _ => varyk_std::Error::new("cannot connect to the database".to_string()),
    }
}

/// The `varyk_std::Error` of a sqlx error. A database error keeps the
/// database's own message and drops its "detail" field, which on
/// Postgres may hold the row's values; every other message is written
/// here, naming at most a column or a count, so no sqlx text that could
/// carry the URL, a password, or a value passes through.
fn db_error(e: sqlx::Error) -> varyk_std::Error {
    varyk_std::Error::new(db_message(e))
}

/// The message of `db_error`.
fn db_message(e: sqlx::Error) -> String {
    match e {
        sqlx::Error::Database(db) => db.message().to_string(),
        sqlx::Error::Configuration(_) => "the database URL is not valid".to_string(),
        sqlx::Error::Io(err) => format!("the connection to the database failed: {err}"),
        sqlx::Error::Tls(_) => "the TLS connection to the database failed".to_string(),
        sqlx::Error::PoolTimedOut => {
            "timed out waiting for a connection to the database".to_string()
        }
        sqlx::Error::PoolClosed => "the connection pool is closed".to_string(),
        sqlx::Error::RowNotFound => "the query gave no row".to_string(),
        sqlx::Error::ColumnNotFound(name) => format!("the row has no column `{name}`"),
        sqlx::Error::ColumnIndexOutOfBounds { index, len } => {
            format!("the row has {len} columns, so no column {index}")
        }
        sqlx::Error::ColumnDecode { index, .. } => format!(
            "column {index} cannot be read; a `numeric`, `json`, `date`, `time`, or array column is cast in the query"
        ),
        sqlx::Error::Encode(_) => "a value cannot be sent to the database".to_string(),
        sqlx::Error::Decode(_) => "a value from the database cannot be decoded".to_string(),
        sqlx::Error::Protocol(_) => {
            "the database sent a reply the driver does not understand".to_string()
        }
        sqlx::Error::WorkerCrashed => "the database connection stopped".to_string(),
        _ => "the database call failed".to_string(),
    }
}

/// Maps a failure of `migrate` on `folder`. sqlx's migration errors name
/// no URL; each message names the folder or the migration's version.
fn migrate_error(folder: &str, e: MigrateError) -> varyk_std::Error {
    let message = match e {
        MigrateError::Source(source) => {
            // sqlx's message repeats the absolute path; the cause, when
            // there is one, says what went wrong.
            let reason = match std::error::Error::source(&*source) {
                Some(cause) => cause.to_string(),
                None => source.to_string(),
            };
            format!("cannot read the migrations in `{folder}`: {reason}")
        }
        MigrateError::ExecuteMigration(err, version) => format!(
            "migration {version} in `{folder}` failed: {}",
            db_message(err)
        ),
        MigrateError::Execute(err) => {
            format!("the migrations in `{folder}` failed: {}", db_message(err))
        }
        MigrateError::VersionMissing(version) => {
            format!("migration {version} was applied to the database but is not in `{folder}`")
        }
        MigrateError::VersionMismatch(version) => {
            format!("migration {version} in `{folder}` was changed after it was applied")
        }
        MigrateError::Dirty(version) => format!(
            "migration {version} is partly applied; fix the database by hand and delete its row from `_sqlx_migrations`"
        ),
        other => format!("the migrations in `{folder}` failed: {other}"),
    };
    varyk_std::Error::new(message)
}

#[cfg(test)]
mod tests {
    use super::{
        Pool, ReadError, Tx, check_placeholders, connect, connect_error, connect_with, db_error,
        in_memory, migrate_error, scan_placeholders, sqlite_time,
    };
    use serde::de;
    use sqlx::migrate::MigrateError;
    use std::borrow::Cow;
    use std::error::Error as StdError;
    use std::fmt;

    const URL: &str = "postgres://ada:hunter2@db.internal/users";

    #[test]
    fn in_memory_names_only_an_in_memory_sqlite_database() {
        for (url, memory) in [
            ("sqlite::memory:", true),
            ("sqlite://:memory:", true),
            ("sqlite://?mode=memory", true),
            ("sqlite:///x.db?cache=shared&mode=memory", true),
            ("sqlite://memory", false),
            ("sqlite:memory", false),
            ("sqlite:///x.db", false),
            ("sqlite:///x.db?mode=rwc", false),
            ("sqlite://data/x.db?mode=rwc", false),
            ("postgres::memory:", false),
        ] {
            assert_eq!(in_memory(url), memory, "{url}");
        }
    }

    /// The fixed-width SQLite text of each time, and each text read back
    /// through `Time`'s parser is the same time: the bounds of the range,
    /// no fraction, six digits, half a second before 1970, whose negative
    /// microseconds need Euclidean division, and leap days and month ends,
    /// before 1970 among them, where the calendar is worked out backwards.
    #[test]
    fn sqlite_time_writes_six_fraction_digits_both_ways() {
        for (micros, text) in [
            (-62_167_219_200_000_000, "0000-01-01T00:00:00.000000Z"),
            (253_402_300_799_999_999, "9999-12-31T23:59:59.999999Z"),
            (1_791_374_400_000_000, "2026-10-07T12:00:00.000000Z"),
            (1_791_374_400_123_456, "2026-10-07T12:00:00.123456Z"),
            (-500_000, "1969-12-31T23:59:59.500000Z"),
            (-86_400_000_000, "1969-12-31T00:00:00.000000Z"),
            (-2_203_891_200_000_000, "1900-03-01T00:00:00.000000Z"),
            (-2_203_891_200_000_001, "1900-02-28T23:59:59.999999Z"),
            (-62_162_121_600_000_000, "0000-02-29T00:00:00.000000Z"),
            (951_868_799_000_000, "2000-02-29T23:59:59.000000Z"),
        ] {
            let time = varyk_std::Time::from_unix_micros(micros);
            assert!(time.is_ok(), "{micros}");
            if let Ok(time) = time {
                assert_eq!(sqlite_time(time), text);
                let read = text.parse::<varyk_std::Time>();
                assert!(matches!(read, Ok(read) if read == time), "{text}");
            }
        }
    }

    /// varyk-http runs each handler on a task of its own, so every call a
    /// handler awaits must give a future that can move between threads,
    /// with any set of drivers built in. Checked by the compiler; nothing
    /// runs.
    #[test]
    fn every_call_gives_a_future_that_can_move_between_threads() {
        fn send<F: Send>(future: F) -> F {
            future
        }
        fn pool_calls(pool: &Pool) {
            drop(send(connect("sqlite::memory:")));
            drop(send(connect_with("sqlite::memory:", 1)));
            drop(send(pool.one::<i64>("select 1", Vec::new())));
            drop(send(pool.first::<i64>("select 1", Vec::new())));
            drop(send(pool.all::<i64>("select 1", Vec::new())));
            drop(send(pool.run("select 1", Vec::new())));
            drop(send(pool.migrate("migrations")));
            drop(send(pool.begin()));
        }
        fn tx_calls(tx: &mut Tx) {
            drop(send(tx.one::<i64>("select 1", Vec::new())));
            drop(send(tx.first::<i64>("select 1", Vec::new())));
            drop(send(tx.all::<i64>("select 1", Vec::new())));
            drop(send(tx.run("select 1", Vec::new())));
            drop(send(tx.commit()));
        }
        let _: fn(&Pool) = pool_calls;
        let _: fn(&mut Tx) = tx_calls;
    }

    /// A database error shaped like Postgres's: a message, and a detail
    /// holding a row's values that `Display` shows as well.
    #[derive(Debug)]
    struct PgShaped;

    impl fmt::Display for PgShaped {
        fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
            write!(
                f,
                "duplicate key value violates unique constraint \"users_pkey\" DETAIL: Key (email)=(ada@example.com) already exists."
            )
        }
    }

    impl StdError for PgShaped {}

    impl sqlx::error::DatabaseError for PgShaped {
        fn message(&self) -> &str {
            "duplicate key value violates unique constraint \"users_pkey\""
        }

        fn code(&self) -> Option<Cow<'_, str>> {
            Some(Cow::Borrowed("23505"))
        }

        fn as_error(&self) -> &(dyn StdError + Send + Sync + 'static) {
            self
        }

        fn as_error_mut(&mut self) -> &mut (dyn StdError + Send + Sync + 'static) {
            self
        }

        fn into_error(self: Box<Self>) -> Box<dyn StdError + Send + Sync + 'static> {
            self
        }

        fn kind(&self) -> sqlx::error::ErrorKind {
            sqlx::error::ErrorKind::UniqueViolation
        }
    }

    fn assert_no_secret(message: &str) {
        assert!(!message.contains(URL), "{message}");
        assert!(!message.contains("hunter2"), "{message}");
        assert!(!message.contains("db.internal"), "{message}");
    }

    /// A field's refusal keeps none of the deserializer's text, which can
    /// hold the value it read.
    #[test]
    fn a_field_refusal_drops_the_field_text() {
        let custom = <ReadError as de::Error>::custom(
            "`0192f0c4-7a3e-7b5c-9d1e-2f3a4b5cqqqq` is not a Uuid like 01890a5d-ac96-774b-bcce-b302099a8057",
        );
        let length = <ReadError as de::Error>::invalid_length(15, &"16 bytes");
        for e in [custom, length] {
            assert!(matches!(e, ReadError::Refused), "{e}");
            let message = e.to_string();
            assert!(!message.contains("0192f0c4"), "{message}");
            assert!(!message.contains("15"), "{message}");
        }
    }

    #[test]
    fn a_bad_url_names_neither_the_url_nor_the_password() {
        let e = sqlx::Error::Configuration(format!("cannot parse {URL}").into());
        let message = db_error(e).message().to_string();
        assert_no_secret(&message);
        assert_eq!(message, "the database URL is not valid");
    }

    #[test]
    fn an_unreachable_database_names_neither_the_url_nor_the_password() {
        let io = std::io::Error::new(std::io::ErrorKind::ConnectionRefused, "connection refused");
        let message = connect_error(sqlx::Error::Io(io)).message().to_string();
        assert_no_secret(&message);
        assert_eq!(message, "cannot reach the database: connection refused");
    }

    #[test]
    fn a_failed_tls_handshake_names_neither_the_url_nor_the_password() {
        let e = sqlx::Error::Tls(format!("certificate not valid for {URL}").into());
        let message = connect_error(e).message().to_string();
        assert_no_secret(&message);
    }

    #[test]
    fn a_protocol_error_while_connecting_gives_a_fixed_message() {
        let e = sqlx::Error::Protocol(format!("unexpected reply from {URL}"));
        let message = connect_error(e).message().to_string();
        assert_eq!(message, "cannot connect to the database");
    }

    #[test]
    fn no_row_gives_a_fixed_message() {
        let message = db_error(sqlx::Error::RowNotFound).message().to_string();
        assert_eq!(message, "the query gave no row");
    }

    #[test]
    fn a_missing_column_is_named() {
        let e = sqlx::Error::ColumnNotFound("age".to_string());
        assert_eq!(db_error(e).message(), "the row has no column `age`");
    }

    #[test]
    fn a_column_out_of_bounds_names_both_counts() {
        let e = sqlx::Error::ColumnIndexOutOfBounds { index: 3, len: 2 };
        assert_eq!(
            db_error(e).message(),
            "the row has 2 columns, so no column 3"
        );
    }

    #[test]
    fn a_column_that_cannot_be_decoded_is_named_without_the_driver_text() {
        let e = sqlx::Error::ColumnDecode {
            index: "\"born\"".to_string(),
            source: format!("value ada@example.com from {URL}").into(),
        };
        let message = db_error(e).message().to_string();
        assert!(
            message.starts_with("column \"born\" cannot be read"),
            "{message}"
        );
        assert!(!message.contains("ada@example.com"), "{message}");
        assert_no_secret(&message);
    }

    #[test]
    fn every_other_error_gives_a_fixed_message() {
        let errors = vec![
            sqlx::Error::Protocol(format!("unexpected reply from {URL}")),
            sqlx::Error::Decode(format!("bad value from {URL}").into()),
            sqlx::Error::Encode(format!("bad value for {URL}").into()),
            sqlx::Error::AnyDriverError(format!("unsupported type at {URL}").into()),
            sqlx::Error::InvalidArgument(format!("bad argument for {URL}")),
            sqlx::Error::TypeNotFound {
                type_name: URL.to_string(),
            },
        ];
        for e in errors {
            let message = db_error(e).message().to_string();
            assert_no_secret(&message);
        }
    }

    #[test]
    fn a_database_error_keeps_the_message_and_drops_the_detail() {
        let message = db_error(sqlx::Error::Database(Box::new(PgShaped)))
            .message()
            .to_string();
        assert_eq!(
            message,
            "duplicate key value violates unique constraint \"users_pkey\""
        );
        assert!(!message.contains("ada@example.com"), "{message}");
        assert!(!message.contains("DETAIL"), "{message}");
    }

    #[test]
    fn a_database_error_while_connecting_says_so_and_drops_the_detail() {
        let message = connect_error(sqlx::Error::Database(Box::new(PgShaped)))
            .message()
            .to_string();
        assert!(
            message.starts_with("cannot connect to the database: "),
            "{message}"
        );
        assert!(!message.contains("ada@example.com"), "{message}");
    }

    #[test]
    fn a_missing_folder_is_named_with_the_cause() {
        let cause = std::io::Error::new(std::io::ErrorKind::NotFound, "no such directory");
        let message = migrate_error("db/migrations", MigrateError::Source(Box::new(cause)))
            .message()
            .to_string();
        assert_eq!(
            message,
            "cannot read the migrations in `db/migrations`: no such directory"
        );
    }

    #[test]
    fn a_failed_migration_names_its_version_and_drops_the_detail() {
        let message = migrate_error(
            "migrations",
            MigrateError::ExecuteMigration(sqlx::Error::Database(Box::new(PgShaped)), 3),
        )
        .message()
        .to_string();
        assert_eq!(
            message,
            "migration 3 in `migrations` failed: duplicate key value violates unique constraint \"users_pkey\""
        );
        assert!(!message.contains("ada@example.com"), "{message}");
    }

    #[test]
    fn a_migration_history_error_names_the_version() {
        let missing = migrate_error("migrations", MigrateError::VersionMissing(4));
        assert!(
            missing.message().contains("migration 4"),
            "{}",
            missing.message()
        );
        let dirty = migrate_error("migrations", MigrateError::Dirty(5));
        assert!(
            dirty.message().contains("migration 5"),
            "{}",
            dirty.message()
        );
        let changed = migrate_error("migrations", MigrateError::VersionMismatch(6));
        assert!(
            changed.message().contains("migration 6"),
            "{}",
            changed.message()
        );
    }

    #[test]
    fn a_connection_failure_while_migrating_names_no_secret() {
        let io = std::io::Error::new(std::io::ErrorKind::ConnectionRefused, "refused");
        let message = migrate_error("migrations", MigrateError::Execute(sqlx::Error::Io(io)))
            .message()
            .to_string();
        assert_no_secret(&message);
        assert!(message.contains("`migrations`"), "{message}");
    }

    /// The numbers of the `$n` placeholders `text` holds, in the order
    /// they appear, or the scanner's message.
    fn numbers(text: &str, mysql: bool) -> Result<Vec<Option<usize>>, String> {
        match scan_placeholders(text, mysql) {
            Ok(found) => Ok(found.dollars.iter().map(|d| d.number).collect()),
            Err(e) => Err(e.message().to_string()),
        }
    }

    /// The message the checks give `text` (read by SQLite's rules) with
    /// `values` values, or `None` when they pass.
    fn checked(text: &str, values: usize) -> Option<String> {
        match scan_placeholders(text, false).and_then(|found| check_placeholders(&found, values)) {
            Ok(()) => None,
            Err(e) => Some(e.message().to_string()),
        }
    }

    #[test]
    fn dollar_placeholders_are_found_with_their_places() {
        for mysql in [false, true] {
            let text = "select $1, $12 from t where a = ($1)";
            let found = scan_placeholders(text, mysql);
            assert!(found.is_ok(), "{text}");
            if let Ok(found) = found {
                assert!(!found.question, "{text}");
                let places: Vec<(Option<&str>, Option<usize>)> = found
                    .dollars
                    .iter()
                    .map(|d| (text.get(d.at.clone()), d.number))
                    .collect();
                assert_eq!(
                    places,
                    [
                        (Some("$1"), Some(1)),
                        (Some("$12"), Some(12)),
                        (Some("$1"), Some(1))
                    ]
                );
            }
        }
    }

    /// A `$n`'s range is in bytes, so after text outside ASCII it still
    /// slices the `$n` out of the text, as the MySQL rewrite needs.
    #[test]
    fn a_dollar_after_non_ascii_text_keeps_its_place() {
        for mysql in [false, true] {
            let text = "select 'é', $1";
            let found = scan_placeholders(text, mysql);
            assert!(found.is_ok(), "{text}");
            if let Ok(found) = found {
                let places: Vec<Option<&str>> = found
                    .dollars
                    .iter()
                    .map(|d| text.get(d.at.clone()))
                    .collect();
                assert_eq!(places, [Some("$1")]);
            }
        }
    }

    #[test]
    fn text_with_nothing_to_find_scans_clean() {
        for mysql in [false, true] {
            for text in [
                "",
                "$",
                "select $",
                "select 1 -",
                "select 1 /",
                "select $x",
                "-",
                "/",
            ] {
                assert_eq!(numbers(text, mysql), Ok(vec![]), "{text}");
            }
        }
    }

    /// A `$1` inside each kind of quote, quoted name, and comment is text,
    /// and so is a `?` there.
    #[test]
    fn quotes_names_and_comments_hold_text_on_sqlite() {
        for text in [
            "select '$1'",
            "select \"$1\"",
            "select `$1`",
            "select [$1]",
            "select 'it''s $1'",
            "select \"a\"\"$1\"",
            "select `a``$1`",
            "select 1 -- $1",
            "select 1 --$1",
            "select 5--$1",
            "select 1 /* $1 */",
            "select 1 /* /* $1 */",
            "select 1 /*$1*/",
            "select '?'",
            "select 1 -- ?",
            "select 1 /* ? */",
            "select [?]",
        ] {
            let found = scan_placeholders(text, false);
            assert!(
                matches!(&found, Ok(found) if found.dollars.is_empty() && !found.question),
                "{text}"
            );
        }
    }

    #[test]
    fn quotes_names_and_comments_hold_text_on_mysql() {
        for text in [
            "select '$1'",
            "select \"$1\"",
            "select `$1`",
            "select 'it''s $1'",
            "select 'it\\'s $1'",
            "select \"a\\\"$1\"",
            "select \"a\"\"$1\"",
            "select `a``$1`",
            "select 1 -- $1",
            "select 1 --\t$1",
            "select 1 --\r$1",
            "select 1 --",
            "select 1 #$1",
            "select 1 /* $1 */",
            "select 1 /* /* $1 */",
            "select '?'",
            "select 1 # ?",
            "select 1 /* ? */",
        ] {
            let found = scan_placeholders(text, true);
            assert!(
                matches!(&found, Ok(found) if found.dollars.is_empty() && !found.question),
                "{text}"
            );
        }
    }

    /// What one database reads as quoted text or a comment, the other
    /// reads as SQL.
    #[test]
    fn each_database_keeps_its_own_rules() {
        // A backslash escapes nothing on SQLite, and in backticks on MySQL.
        assert_eq!(numbers("select 'a\\', $1", false), Ok(vec![Some(1)]));
        assert_eq!(numbers("select `a\\`, $1", true), Ok(vec![Some(1)]));
        // `--` needs a space or a control character after it on MySQL.
        assert_eq!(numbers("select 5--$1", true), Ok(vec![Some(1)]));
        assert_eq!(numbers("select 5--$1", false), Ok(vec![]));
        // `#` and `[` are not quoting on SQLite and MySQL respectively.
        assert_eq!(numbers("select 1 #$1", false), Ok(vec![Some(1)]));
        assert_eq!(numbers("select [$1]", true), Ok(vec![Some(1)]));
        // A line comment ends at the end of its line.
        assert_eq!(numbers("select 1 -- a\n, $1", false), Ok(vec![Some(1)]));
        assert_eq!(numbers("select 1 # a\n, $1", true), Ok(vec![Some(1)]));
        assert_eq!(numbers("select 1 /* a */ $1", true), Ok(vec![Some(1)]));
        assert_eq!(numbers("select 'a'$1", false), Ok(vec![Some(1)]));
    }

    /// A `$` inside a name, or digits running into a name, is no
    /// placeholder.
    #[test]
    fn a_dollar_inside_a_name_is_no_placeholder() {
        for mysql in [false, true] {
            for text in [
                "select price$1",
                "select $1$x",
                "select é$1",
                "select $1a",
                "select $1_",
                "select _$1",
                "select 1$1",
                "select $$1",
                "select $1é",
            ] {
                assert_eq!(numbers(text, mysql), Ok(vec![]), "{text}");
            }
        }
    }

    #[test]
    fn a_quote_or_comment_that_never_closes_is_an_error() {
        let quote = "the query has a quote that never closes";
        let comment = "the query has a comment that never closes";
        for mysql in [false, true] {
            for (text, message) in [
                ("select 'a", quote),
                ("select 'a''", quote),
                ("select \"a", quote),
                ("select `a", quote),
                ("select `a``", quote),
                ("select '", quote),
                ("select 1 /* a", comment),
                ("select 1 /*/", comment),
                ("select 1 /* $1 *", comment),
                ("/*", comment),
            ] {
                assert_eq!(numbers(text, mysql), Err(message.to_string()), "{text}");
            }
        }
        assert_eq!(numbers("select [a", false), Err(quote.to_string()));
        assert_eq!(numbers("select 'a\\'", true), Err(quote.to_string()));
        assert_eq!(numbers("select 'a\\", true), Err(quote.to_string()));
        assert_eq!(numbers("select 'a\\', $1", true), Err(quote.to_string()));
    }

    #[test]
    fn a_number_too_large_is_found_without_one() {
        assert_eq!(
            numbers("select $99999999999999999999", false),
            Ok(vec![None])
        );
    }

    #[test]
    fn a_question_mark_outside_quotes_is_found() {
        for mysql in [false, true] {
            let found = scan_placeholders("select ?, '?', $1", mysql);
            assert!(matches!(&found, Ok(found) if found.question), "{mysql}");
            let found = scan_placeholders("select '?', $1", mysql);
            assert!(matches!(&found, Ok(found) if !found.question), "{mysql}");
        }
    }

    #[test]
    fn the_checks_give_their_messages_in_order() {
        let mixed = "use `$1`, `$2`, … or `?`, not both";
        let zero = "`$0` is not a placeholder; they start at `$1`";
        let large = "`$` followed by a number too large to be a placeholder";
        for (text, values, message) in [
            ("select ?, $1", 1, Some(mixed)),
            ("select ?, $0", 1, Some(mixed)),
            ("select ?, $99999999999999999999", 1, Some(mixed)),
            ("select $0", 0, Some(zero)),
            ("select $99999999999999999999, $0", 1, Some(zero)),
            ("select $99999999999999999999", 1, Some(large)),
            (
                "select $1, $3",
                2,
                Some("the query uses `$1` to `$3` but was given 2 values"),
            ),
            (
                "select $2",
                1,
                Some("the query uses `$1` to `$2` but was given 1 value"),
            ),
            (
                "select $1",
                2,
                Some("the query uses `$1` but was given 2 values"),
            ),
            (
                "select $1",
                0,
                Some("the query uses `$1` but was given 0 values"),
            ),
            ("select $1, $3", 3, Some("the query skips `$2`")),
            ("select $3, $3, $2", 3, Some("the query skips `$1`")),
            ("select $1, $4, $2", 4, Some("the query skips `$3`")),
            ("select $1", 1, None),
            ("select $2, $1, $2", 2, None),
            ("select '?', $1", 1, None),
            ("select 1", 0, None),
            ("select ?", 5, None),
            ("select '$1'", 0, None),
        ] {
            assert_eq!(checked(text, values).as_deref(), message, "{text}");
        }
    }
}
