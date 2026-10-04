// The facade of varyk-sql: the Rust that Varyk programs reach through
// `sql::connect`, `sql::connect_with`, `sql::Pool`, and `sql::Tx`. Every sqlx
// `Result` is mapped to a `varyk_std::Error` whose message names no URL
// and no database "detail" field, and nothing here panics.

use std::fmt;
use std::path::Path;
use std::pin::Pin;

use futures_core::Stream;

use serde::de::{self, DeserializeOwned, DeserializeSeed, IntoDeserializer, MapAccess, Visitor};
use sqlx::any::{
    Any, AnyArguments, AnyConnectOptions, AnyPoolOptions, AnyRow, AnyTypeInfoKind, AnyValueRef,
};
use sqlx::decode::Decode;
use sqlx::migrate::{MigrateError, Migrator};
use sqlx::query::Query;
use sqlx::{AnyConnection, Column, Either, Executor, Row, Statement, ValueRef};

/// sqlx's default for the most connections a pool opens.
const DEFAULT_MAX_CONNECTIONS: u32 = 10;

/// Opens a pool on the database `url` names, with at most
/// `max_connections` connections (at least 1). The scheme picks the
/// driver: `sqlite:`, `postgres:`, or `mysql:`. An in-memory SQLite
/// database gets a pool of one connection whatever the count.
pub async fn connect_with(url: &str, max_connections: u32) -> Result<Pool, varyk_std::Error> {
    if max_connections == 0 {
        return Err(varyk_std::Error::new(
            "a pool needs at least one connection; `max_connections` is 0".to_string(),
        ));
    }
    sqlx::any::install_default_drivers();
    let options: AnyConnectOptions = url
        .parse()
        .map_err(|_| varyk_std::Error::new("the database URL does not parse".to_string()))?;
    driver_built(options.database_url.scheme())?;
    let parsed = &options.database_url;
    let pool_options = if in_memory(parsed.scheme(), parsed.path(), parsed.query()) {
        // sqlx's `Any` driver parses the URL again for each connection,
        // and each parse of an in-memory SQLite URL names a new database,
        // so the pool keeps one connection and never closes it: the
        // database lives exactly as long as the pool. Should sqlx drop
        // that connection (after an I/O error, say), its replacement is
        // a new, empty database.
        AnyPoolOptions::new()
            .max_connections(1)
            .min_connections(1)
            .idle_timeout(None)
            .max_lifetime(None)
    } else {
        AnyPoolOptions::new().max_connections(max_connections)
    };
    let pool = pool_options
        .connect_with(options)
        .await
        .map_err(connect_error)?;
    Ok(Pool { pool })
}

/// Whether a URL of `scheme`, `path`, and `query` names an in-memory
/// SQLite database: `sqlite::memory:`, or a URL with `mode=memory`.
fn in_memory(scheme: &str, path: &str, query: Option<&str>) -> bool {
    scheme == "sqlite"
        && (path == ":memory:"
            || query.is_some_and(|q| q.split('&').any(|pair| pair == "mode=memory")))
}

/// Opens a pool on the database `url` names, with sqlx's defaults.
pub async fn connect(url: &str) -> Result<Pool, varyk_std::Error> {
    connect_with(url, DEFAULT_MAX_CONNECTIONS).await
}

/// An `Error` unless this build has the driver a URL of `scheme` needs.
fn driver_built(scheme: &str) -> Result<(), varyk_std::Error> {
    let (feature, built) = match scheme {
        "sqlite" => ("sqlite", cfg!(feature = "sqlite")),
        "postgres" | "postgresql" => ("postgres", cfg!(feature = "postgres")),
        "mysql" | "mariadb" => ("mysql", cfg!(feature = "mysql")),
        _ => {
            return Err(varyk_std::Error::new(
                "the database URL names no database varyk-sql knows; a database URL starts with `sqlite:`, `postgres:`, or `mysql:`"
                    .to_string(),
            ));
        }
    };
    if built {
        Ok(())
    } else {
        Err(varyk_std::Error::new(format!(
            "this program was built without the `{feature}` feature of varyk-sql, which the database URL needs; add it with `varyk add sql --features {feature}`"
        )))
    }
}

/// A pool of connections to one database. `clone` gives another handle
/// to the same pool.
#[derive(Clone)]
pub struct Pool {
    pool: sqlx::AnyPool,
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
        let mut conn = self.pool.acquire().await.map_err(db_error)?;
        one_on(&mut conn, &mut false, query, values).await
    }

    /// As `one`, with `None` when there is no row.
    pub async fn first<T: varyk_std::serde::de::DeserializeOwned>(
        &self,
        query: &'static str,
        values: Vec<varyk_std::Value>,
    ) -> Result<Option<T>, varyk_std::Error> {
        let mut conn = self.pool.acquire().await.map_err(db_error)?;
        first_on(&mut conn, &mut false, query, values).await
    }

    /// Runs `query` with `values` and reads every row as a `T`.
    pub async fn all<T: varyk_std::serde::de::DeserializeOwned>(
        &self,
        query: &'static str,
        values: Vec<varyk_std::Value>,
    ) -> Result<Vec<T>, varyk_std::Error> {
        let mut conn = self.pool.acquire().await.map_err(db_error)?;
        all_on(&mut conn, &mut false, query, values).await
    }

    /// Runs the statement `query` with `values`; gives the number of
    /// rows it changed.
    pub async fn run(
        &self,
        query: &'static str,
        values: Vec<varyk_std::Value>,
    ) -> Result<u64, varyk_std::Error> {
        let mut conn = self.pool.acquire().await.map_err(db_error)?;
        run_on(&mut conn, &mut false, query, values).await
    }

    /// Applies, in version order, every migration in `folder` (a path
    /// relative to the working directory) that the database's
    /// `_sqlx_migrations` table does not record, and gives `true`. Files
    /// are sqlx's `<version>_<name>.sql`; `.down.sql` files are ignored.
    pub async fn migrate(&self, folder: &str) -> Result<bool, varyk_std::Error> {
        let migrator = Migrator::new(Path::new(folder))
            .await
            .map_err(|e| migrate_error(folder, e))?;
        let mut conn = self.pool.acquire().await.map_err(db_error)?;
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
                if conn.backend_name() != "SQLite" {
                    let _closed = conn.close().await;
                }
                Err(migrate_error(folder, e))
            }
        }
    }

    /// Starts a transaction on one connection of the pool. The
    /// connection is the transaction's until it commits or is dropped.
    pub async fn begin(&self) -> Result<Tx, varyk_std::Error> {
        let tx = self.pool.begin().await.map_err(db_error)?;
        Ok(Tx {
            tx: Some(tx),
            failed: false,
        })
    }
}

/// A transaction on one connection. A `Tx` dropped without `commit`
/// rolls back: sqlx rolls an open transaction back when it is dropped.
pub struct Tx {
    tx: Option<sqlx::Transaction<'static, Any>>,
    /// Set when a statement in the transaction failed in the database.
    /// Postgres then aborts the transaction and its `COMMIT` is a silent
    /// rollback; a MySQL deadlock, and some SQLite errors, end the
    /// transaction on the server, and later statements would each commit
    /// on their own. So a failed transaction runs no more statements, and
    /// `commit` rolls it back and says so.
    failed: bool,
}

impl Tx {
    /// As `Pool::one`, inside the transaction.
    pub async fn one<T: varyk_std::serde::de::DeserializeOwned>(
        &mut self,
        query: &'static str,
        values: Vec<varyk_std::Value>,
    ) -> Result<T, varyk_std::Error> {
        let (conn, failed) = self.open()?;
        one_on(conn, failed, query, values).await
    }

    /// As `Pool::first`, inside the transaction.
    pub async fn first<T: varyk_std::serde::de::DeserializeOwned>(
        &mut self,
        query: &'static str,
        values: Vec<varyk_std::Value>,
    ) -> Result<Option<T>, varyk_std::Error> {
        let (conn, failed) = self.open()?;
        first_on(conn, failed, query, values).await
    }

    /// As `Pool::all`, inside the transaction.
    pub async fn all<T: varyk_std::serde::de::DeserializeOwned>(
        &mut self,
        query: &'static str,
        values: Vec<varyk_std::Value>,
    ) -> Result<Vec<T>, varyk_std::Error> {
        let (conn, failed) = self.open()?;
        all_on(conn, failed, query, values).await
    }

    /// As `Pool::run`, inside the transaction.
    pub async fn run(
        &mut self,
        query: &'static str,
        values: Vec<varyk_std::Value>,
    ) -> Result<u64, varyk_std::Error> {
        let (conn, failed) = self.open()?;
        run_on(conn, failed, query, values).await
    }

    /// Commits the transaction and gives `true`. When a statement in it
    /// failed in the database, even one whose `Error` the program
    /// handled, it rolls the transaction back instead and is an `Error`
    /// saying so, on every database. The transaction is finished after
    /// it, even when the commit fails: then `commit` and every query are
    /// an `Error`.
    pub async fn commit(&mut self) -> Result<bool, varyk_std::Error> {
        match self.tx.take() {
            Some(tx) if self.failed => {
                let message = "the transaction was rolled back: a statement in it failed";
                match tx.rollback().await {
                    Ok(()) => Err(varyk_std::Error::new(message.to_string())),
                    Err(e) => Err(varyk_std::Error::new(format!(
                        "{message}, and the rollback failed: {}",
                        db_message(e)
                    ))),
                }
            }
            Some(tx) => {
                tx.commit().await.map_err(db_error)?;
                Ok(true)
            }
            None => Err(finished()),
        }
    }

    /// The transaction's connection and its `failed` flag, or an `Error`
    /// once it has committed or once a statement in it failed; the
    /// database is not touched then.
    fn open(&mut self) -> Result<(&mut AnyConnection, &mut bool), varyk_std::Error> {
        match self.tx.as_mut() {
            None => Err(finished()),
            Some(_) if self.failed => Err(varyk_std::Error::new(
                "a statement in this transaction failed; it will roll back".to_string(),
            )),
            Some(tx) => Ok((&mut **tx, &mut self.failed)),
        }
    }
}

/// The `Error` for a `Tx` used after `commit`.
fn finished() -> varyk_std::Error {
    varyk_std::Error::new("this transaction is finished".to_string())
}

/// `one` on `conn`, for a pool and a transaction alike. Each of these
/// sets `failed` when the statement fails in the database (see
/// `statement_error`); a pool passes a flag it does not read.
async fn one_on<T: DeserializeOwned>(
    conn: &mut AnyConnection,
    failed: &mut bool,
    query: &'static str,
    values: Vec<varyk_std::Value>,
) -> Result<T, varyk_std::Error> {
    match first_on(conn, failed, query, values).await? {
        Some(found) => Ok(found),
        None => Err(varyk_std::Error::new("the query gave no row".to_string())),
    }
}

/// `first` on `conn`.
async fn first_on<T: DeserializeOwned>(
    conn: &mut AnyConnection,
    failed: &mut bool,
    query: &'static str,
    values: Vec<varyk_std::Value>,
) -> Result<Option<T>, varyk_std::Error> {
    let bound = bind(conn, failed, query, values).await?;
    let row = if conn.backend_name() != "SQLite" {
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
    row.as_ref().map(read_row).transpose()
}

/// The next row of `rows`, or `None` at the end.
async fn next_row<S>(rows: &mut S) -> Result<Option<AnyRow>, sqlx::Error>
where
    S: Stream<Item = Result<AnyRow, sqlx::Error>> + Unpin,
{
    std::future::poll_fn(|cx| Pin::new(&mut *rows).poll_next(cx))
        .await
        .transpose()
}

/// `all` on `conn`.
async fn all_on<T: DeserializeOwned>(
    conn: &mut AnyConnection,
    failed: &mut bool,
    query: &'static str,
    values: Vec<varyk_std::Value>,
) -> Result<Vec<T>, varyk_std::Error> {
    let bound = bind(conn, failed, query, values).await?;
    let rows = bound
        .fetch_all(&mut *conn)
        .await
        .map_err(|e| statement_error(e, failed))?;
    rows.iter().map(read_row).collect()
}

/// `run` on `conn`.
async fn run_on(
    conn: &mut AnyConnection,
    failed: &mut bool,
    query: &'static str,
    values: Vec<varyk_std::Value>,
) -> Result<u64, varyk_std::Error> {
    let bound = bind(conn, failed, query, values).await?;
    let done = bound
        .execute(&mut *conn)
        .await
        .map_err(|e| statement_error(e, failed))?;
    Ok(done.rows_affected())
}

/// `query` with `values` bound to its placeholders, in order. On SQLite
/// and MySQL the number of values is first checked against the number
/// of placeholders the prepared statement reports, since SQLite would
/// bind a missing value as `NULL` and ignore an extra one.
///
/// A Postgres statement is not prepared here: sqlx caches it by its text
/// with the parameter types the server inferred and would reuse it for
/// the values' own types (an `i64` sent to an `integer` parameter is
/// refused), and a parameter of a type the `Any` driver cannot map
/// (`$1::uuid`) would fail to prepare. The server rejects too few
/// values itself and ignores an extra one.
///
/// Nor is a Postgres statement kept prepared after it runs: sqlx's
/// statement cache keys a statement by its text with the parameter types
/// of its first run on the connection, and later runs send their values
/// in binary into those types unchecked. A `None` binds as an `i64`, so a
/// statement first run with `None` would take a later string's bytes as
/// an integer. Each Postgres run is therefore parsed again with its own
/// values' types.
async fn bind(
    conn: &mut AnyConnection,
    failed: &mut bool,
    query: &'static str,
    values: Vec<varyk_std::Value>,
) -> Result<Query<'static, Any, AnyArguments<'static>>, varyk_std::Error> {
    let placeholders = if conn.backend_name() == "PostgreSQL" {
        None
    } else {
        let statement = conn
            .prepare(query)
            .await
            .map_err(|e| statement_error(e, failed))?;
        match statement.parameters() {
            Some(Either::Left(types)) => Some(types.len()),
            Some(Either::Right(count)) => Some(count),
            None => None,
        }
    };
    if let Some(placeholders) = placeholders {
        if placeholders != values.len() {
            return Err(varyk_std::Error::new(format!(
                "the query has {} but was given {}",
                counted(placeholders, "placeholder", "placeholders"),
                counted(values.len(), "value", "values"),
            )));
        }
    }
    // `placeholders` is `None` only on Postgres (and on a driver that
    // reports no count), where the statement is not kept prepared.
    let mut bound = sqlx::query(query).persistent(placeholders.is_some());
    for value in values {
        bound = match value {
            varyk_std::Value::Null => bound.bind(None::<i64>),
            varyk_std::Value::Bool(b) => bound.bind(b),
            varyk_std::Value::Int(n) => bound.bind(n),
            varyk_std::Value::Float(x) => bound.bind(x),
            varyk_std::Value::Text(text) => bound.bind(text),
        };
    }
    Ok(bound)
}

/// `db_error` for a statement the database prepared or ran, setting
/// `failed` when the database refused it, on every database alike. A
/// row that cannot be read and the package's own placeholder check (on
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

/// Reads `row` as a `T` by column name (spec §2.5 and §2.6): a struct
/// takes each field from the column of its name, and a number, `bool`,
/// or `string`, or an `Option` of one, takes the row's only column.
fn read_row<T: DeserializeOwned>(row: &AnyRow) -> Result<T, varyk_std::Error> {
    T::deserialize(RowReader { row }).map_err(|e| varyk_std::Error::new(e.0))
}

/// A failure while reading a row, as the message `read_row` returns.
#[derive(Debug)]
struct ReadError(String);

impl fmt::Display for ReadError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

impl std::error::Error for ReadError {}

impl de::Error for ReadError {
    fn custom<M: fmt::Display>(message: M) -> Self {
        ReadError(message.to_string())
    }

    fn missing_field(field: &'static str) -> Self {
        ReadError(format!(
            "the row has no column `{field}`, which a field of the type needs"
        ))
    }

    fn duplicate_field(field: &'static str) -> Self {
        ReadError(format!(
            "the row has two columns named `{field}`; name them apart with `as`"
        ))
    }
}

/// A row read as a whole: a struct by column name, anything else from
/// its only column.
struct RowReader<'r> {
    row: &'r AnyRow,
}

impl<'r> RowReader<'r> {
    /// The row's only column; an `Error` naming the count otherwise.
    fn only(self) -> Result<Cell<'r>, ReadError> {
        self.only_or("")
    }

    /// `only`, with `hint` after the count in the `Error`.
    fn only_or(self, hint: &str) -> Result<Cell<'r>, ReadError> {
        let count = self.row.columns().len();
        if count == 1 {
            Ok(Cell {
                row: self.row,
                index: 0,
            })
        } else {
            Err(ReadError(format!(
                "a row read as one value must have one column; this one has {count}{hint}"
            )))
        }
    }

    fn not_a_list() -> ReadError {
        ReadError(
            "a row cannot be read as a list or a map; read it into a struct, or its one column into a number, `bool`, or `string`"
                .to_string(),
        )
    }
}

impl<'de, 'r> de::Deserializer<'de> for RowReader<'r> {
    type Error = ReadError;

    fn deserialize_struct<V: Visitor<'de>>(
        self,
        _name: &'static str,
        _fields: &'static [&'static str],
        visitor: V,
    ) -> Result<V::Value, ReadError> {
        visitor.visit_map(Columns {
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
        self.only()?.deserialize_any(visitor)
    }

    fn deserialize_bool<V: Visitor<'de>>(self, visitor: V) -> Result<V::Value, ReadError> {
        self.only()?.deserialize_bool(visitor)
    }

    fn deserialize_i8<V: Visitor<'de>>(self, visitor: V) -> Result<V::Value, ReadError> {
        self.only()?.deserialize_i8(visitor)
    }

    fn deserialize_i16<V: Visitor<'de>>(self, visitor: V) -> Result<V::Value, ReadError> {
        self.only()?.deserialize_i16(visitor)
    }

    fn deserialize_i32<V: Visitor<'de>>(self, visitor: V) -> Result<V::Value, ReadError> {
        self.only()?.deserialize_i32(visitor)
    }

    fn deserialize_i64<V: Visitor<'de>>(self, visitor: V) -> Result<V::Value, ReadError> {
        self.only()?.deserialize_i64(visitor)
    }

    fn deserialize_u8<V: Visitor<'de>>(self, visitor: V) -> Result<V::Value, ReadError> {
        self.only()?.deserialize_u8(visitor)
    }

    fn deserialize_u16<V: Visitor<'de>>(self, visitor: V) -> Result<V::Value, ReadError> {
        self.only()?.deserialize_u16(visitor)
    }

    fn deserialize_u32<V: Visitor<'de>>(self, visitor: V) -> Result<V::Value, ReadError> {
        self.only()?.deserialize_u32(visitor)
    }

    fn deserialize_u64<V: Visitor<'de>>(self, visitor: V) -> Result<V::Value, ReadError> {
        self.only()?.deserialize_u64(visitor)
    }

    fn deserialize_f32<V: Visitor<'de>>(self, visitor: V) -> Result<V::Value, ReadError> {
        self.only()?.deserialize_f32(visitor)
    }

    fn deserialize_f64<V: Visitor<'de>>(self, visitor: V) -> Result<V::Value, ReadError> {
        self.only()?.deserialize_f64(visitor)
    }

    fn deserialize_char<V: Visitor<'de>>(self, visitor: V) -> Result<V::Value, ReadError> {
        self.only()?.deserialize_char(visitor)
    }

    fn deserialize_str<V: Visitor<'de>>(self, visitor: V) -> Result<V::Value, ReadError> {
        self.only()?.deserialize_str(visitor)
    }

    fn deserialize_string<V: Visitor<'de>>(self, visitor: V) -> Result<V::Value, ReadError> {
        self.only()?.deserialize_string(visitor)
    }

    fn deserialize_bytes<V: Visitor<'de>>(self, visitor: V) -> Result<V::Value, ReadError> {
        self.only()?.deserialize_bytes(visitor)
    }

    fn deserialize_byte_buf<V: Visitor<'de>>(self, visitor: V) -> Result<V::Value, ReadError> {
        self.only()?.deserialize_byte_buf(visitor)
    }

    fn deserialize_option<V: Visitor<'de>>(self, visitor: V) -> Result<V::Value, ReadError> {
        // `let u: Option<User> = db.one(..)` lands here: a row that may be
        // missing is `first`'s, which reads the row into `User`.
        self.only_or("; to read a row that may be missing, use `first`")?
            .deserialize_option(visitor)
    }

    fn deserialize_unit<V: Visitor<'de>>(self, visitor: V) -> Result<V::Value, ReadError> {
        self.only()?.deserialize_unit(visitor)
    }

    fn deserialize_unit_struct<V: Visitor<'de>>(
        self,
        name: &'static str,
        visitor: V,
    ) -> Result<V::Value, ReadError> {
        self.only()?.deserialize_unit_struct(name, visitor)
    }

    fn deserialize_newtype_struct<V: Visitor<'de>>(
        self,
        name: &'static str,
        visitor: V,
    ) -> Result<V::Value, ReadError> {
        self.only()?.deserialize_newtype_struct(name, visitor)
    }

    fn deserialize_enum<V: Visitor<'de>>(
        self,
        name: &'static str,
        variants: &'static [&'static str],
        visitor: V,
    ) -> Result<V::Value, ReadError> {
        self.only()?.deserialize_enum(name, variants, visitor)
    }

    fn deserialize_identifier<V: Visitor<'de>>(self, visitor: V) -> Result<V::Value, ReadError> {
        self.only()?.deserialize_identifier(visitor)
    }

    fn deserialize_ignored_any<V: Visitor<'de>>(self, visitor: V) -> Result<V::Value, ReadError> {
        visitor.visit_unit()
    }
}

/// A row's columns as a map from column name to value, for a struct.
struct Columns<'r> {
    row: &'r AnyRow,
    next: usize,
}

impl<'de, 'r> MapAccess<'de> for Columns<'r> {
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
        let cell = Cell {
            row: self.row,
            index: self.next,
        };
        self.next += 1;
        seed.deserialize(cell)
    }
}

/// One column of a row, read into one field.
struct Cell<'r> {
    row: &'r AnyRow,
    index: usize,
}

/// A column's value, as the kinds the `Any` driver decodes.
enum Datum {
    Null,
    Bool(bool),
    Int(i64),
    Float(f64),
    Text(String),
}

impl Datum {
    fn kind(&self) -> &'static str {
        match self {
            Datum::Null => "NULL",
            Datum::Bool(_) => "a boolean",
            Datum::Int(_) => "an integer",
            Datum::Float(_) => "a float",
            Datum::Text(_) => "text",
        }
    }
}

impl<'r> Cell<'r> {
    fn name(&self) -> &'r str {
        self.row
            .columns()
            .get(self.index)
            .map_or("?", |column| column.name())
    }

    /// An error about this column: "column `age` ...".
    fn error(&self, what: &str) -> ReadError {
        ReadError(format!("column `{}` {what}", self.name()))
    }

    fn is_null(&self) -> Result<bool, ReadError> {
        self.row
            .try_get_raw(self.index)
            .map(|raw| raw.is_null())
            .map_err(|_| self.error("is not in the row"))
    }

    /// The column's value. A value is never put in a message, so no
    /// row's data reaches a log through an error.
    fn datum(&self) -> Result<Datum, ReadError> {
        let raw = self
            .row
            .try_get_raw(self.index)
            .map_err(|_| self.error("is not in the row"))?;
        if raw.is_null() {
            return Ok(Datum::Null);
        }
        let kind = raw.type_info().kind();
        match kind {
            AnyTypeInfoKind::Null => Ok(Datum::Null),
            AnyTypeInfoKind::Bool => self.decode(raw).map(Datum::Bool),
            AnyTypeInfoKind::SmallInt | AnyTypeInfoKind::Integer | AnyTypeInfoKind::BigInt => {
                self.decode(raw).map(Datum::Int)
            }
            AnyTypeInfoKind::Real | AnyTypeInfoKind::Double => self.decode(raw).map(Datum::Float),
            AnyTypeInfoKind::Text => self.decode(raw).map(Datum::Text),
            // MySQL reports a `text` column as bytes; bytes that are UTF-8
            // text read as text.
            AnyTypeInfoKind::Blob => {
                let bytes: Vec<u8> = self.decode(raw)?;
                String::from_utf8(bytes).map(Datum::Text).map_err(|_| {
                    self.error("holds bytes that are not UTF-8 text; cast the column in the query")
                })
            }
        }
    }

    fn decode<T: Decode<'r, Any>>(&self, raw: AnyValueRef<'r>) -> Result<T, ReadError> {
        T::decode(raw).map_err(|_| self.error("cannot be decoded"))
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
            other => Err(self.mismatch(&other, field)),
        }
    }

    fn not_one_value(&self, what: &str) -> ReadError {
        self.error(&format!(
            "cannot be read into {what}; a column holds one number, `bool`, or `string`"
        ))
    }
}

impl<'de, 'r> de::Deserializer<'de> for Cell<'r> {
    type Error = ReadError;

    fn deserialize_any<V: Visitor<'de>>(self, visitor: V) -> Result<V::Value, ReadError> {
        match self.datum()? {
            Datum::Null => visitor.visit_none(),
            Datum::Bool(b) => visitor.visit_bool(b),
            Datum::Int(n) => visitor.visit_i64(n),
            Datum::Float(x) => visitor.visit_f64(x),
            Datum::Text(text) => visitor.visit_string(text),
        }
    }

    fn deserialize_bool<V: Visitor<'de>>(self, visitor: V) -> Result<V::Value, ReadError> {
        match self.present()? {
            Datum::Bool(b) => visitor.visit_bool(b),
            Datum::Int(0) => visitor.visit_bool(false),
            Datum::Int(1) => visitor.visit_bool(true),
            Datum::Int(_) => Err(self
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

    fn deserialize_str<V: Visitor<'de>>(self, visitor: V) -> Result<V::Value, ReadError> {
        self.deserialize_string(visitor)
    }

    fn deserialize_string<V: Visitor<'de>>(self, visitor: V) -> Result<V::Value, ReadError> {
        match self.present()? {
            Datum::Text(text) => visitor.visit_string(text),
            other => Err(self.mismatch(&other, "a `string`")),
        }
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
            "column {index} cannot be read; a column of a type other than boolean, integer, float, or text is cast in the query"
        ),
        sqlx::Error::AnyDriverError(_) => {
            "the query uses a type varyk-sql cannot read or write; cast it in the query".to_string()
        }
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
    use super::{connect_error, db_error, in_memory, migrate_error};
    use sqlx::migrate::MigrateError;
    use std::borrow::Cow;
    use std::error::Error as StdError;
    use std::fmt;

    const URL: &str = "postgres://ada:hunter2@db.internal/users";

    #[test]
    fn in_memory_names_only_an_in_memory_sqlite_database() {
        assert!(in_memory("sqlite", ":memory:", None));
        assert!(in_memory("sqlite", "", Some("mode=memory")));
        assert!(in_memory(
            "sqlite",
            "/x.db",
            Some("cache=shared&mode=memory")
        ));
        assert!(!in_memory("sqlite", "/x.db", None));
        assert!(!in_memory("sqlite", "/x.db", Some("mode=rwc")));
        assert!(!in_memory("postgres", ":memory:", None));
        for (url, memory) in [
            ("sqlite::memory:", true),
            ("sqlite://?mode=memory", true),
            ("sqlite://memory", false),
            ("sqlite:memory", false),
            ("sqlite://data/x.db?mode=rwc", false),
        ] {
            let parsed = url.parse::<sqlx::any::AnyConnectOptions>();
            assert!(parsed.is_ok(), "{url} parses");
            if let Ok(options) = parsed {
                let u = &options.database_url;
                assert_eq!(in_memory(u.scheme(), u.path(), u.query()), memory, "{url}");
            }
        }
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
}
