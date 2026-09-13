/* This Source Code Form is subject to the terms of the Mozilla Public
 * License, v. 2.0. If a copy of the MPL was not distributed with this
 * file, You can obtain one at https://mozilla.org/MPL/2.0/. */
use std::path::{Path, PathBuf};
use std::sync::Arc;

use log::{info, warn};
use malloc_size_of::{MallocSizeOf, MallocSizeOfOps};
use rusqlite::types::Value;
use rusqlite::{Connection, Error, OptionalExtension, params, params_from_iter};
use sea_query::{Condition, Expr, ExprTrait, IntoCondition, SqliteQueryBuilder};
use sea_query_rusqlite::RusqliteBinder;
use servo_base::threadpool::ThreadPool;
use storage_traits::indexeddb::{
    AsyncOperation, AsyncReadOnlyOperation, AsyncReadWriteOperation, AsyncSchemaOperation,
    BackendError, BackendResult, BackfillIndexResult, CreateObjectResult, IndexBackfillEntry,
    IndexedDBDescription, IndexedDBIndex, IndexedDBKeyRange, IndexedDBKeyType, IndexedDBRecord,
    IndexedDBTxnMode, KeyPath, KvsEngine, KvsIndexUpdate, KvsOperationTarget, KvsTransaction,
    PutItemResult, RecordsShape,
};

use crate::shared::{DB_INIT_PRAGMAS, DB_PRAGMAS, is_sqlite_disk_full_error};

mod create;
mod database_model;
mod encoding;
mod object_data_model;
mod object_store_index_model;
mod object_store_model;

/// Bytes already in the database that do not decode are corrupt storage, not a
/// programming error. Reporting them as a `rusqlite::Error` lets the request reject
/// through the path every other SQL failure already takes, instead of killing the
/// storage thread and every other database it is serving.
fn corrupt_storage(what: &str) -> Error {
    Error::FromSqlConversionFailure(
        0,
        rusqlite::types::Type::Blob,
        Box::new(std::io::Error::new(
            std::io::ErrorKind::InvalidData,
            format!("stored IndexedDB {what} is not decodable"),
        )),
    )
}

fn decode_key(bytes: &[u8]) -> Result<IndexedDBKeyType, Error> {
    encoding::deserialize(bytes).ok_or_else(|| corrupt_storage("key"))
}

fn decode_key_path(bytes: &[u8]) -> Result<KeyPath, Error> {
    postcard::from_bytes(bytes).map_err(|_| corrupt_storage("key path"))
}

fn encode_key_path(key_path: &KeyPath) -> Result<Vec<u8>, Error> {
    postcard::to_stdvec(key_path).map_err(|error| Error::ToSqlConversionFailure(Box::new(error)))
}

fn backend_error_from_sqlite_error(error: Error) -> BackendError {
    if is_sqlite_disk_full_error(&error) {
        BackendError::QuotaExceeded
    } else {
        BackendError::DbErr(format!("{error:?}"))
    }
}

fn range_to_query(range: IndexedDBKeyRange) -> Condition {
    // Special case for optimization
    if let Some(singleton) = range.as_singleton() {
        let encoded = encoding::serialize(singleton);
        return Expr::column(object_data_model::Column::Key)
            .eq(encoded)
            .into_condition();
    }
    let mut parts = vec![];
    if let Some(upper) = range.upper.as_ref() {
        let upper_bytes = encoding::serialize(upper);
        let query = if range.upper_open {
            Expr::column(object_data_model::Column::Key).lt(upper_bytes)
        } else {
            Expr::column(object_data_model::Column::Key).lte(upper_bytes)
        };
        parts.push(query);
    }
    if let Some(lower) = range.lower.as_ref() {
        let lower_bytes = encoding::serialize(lower);
        let query = if range.lower_open {
            Expr::column(object_data_model::Column::Key).gt(lower_bytes)
        } else {
            Expr::column(object_data_model::Column::Key).gte(lower_bytes)
        };
        parts.push(query);
    }
    let mut condition = Condition::all();
    for part in parts {
        condition = condition.add(part);
    }
    condition
}

/// One record from whichever surface a request addressed.
struct SourceRecord {
    /// The key the request's range applied to, which is the index key for an index request.
    key: Vec<u8>,
    /// The object store key, which an index cursor reports as its `primaryKey`.
    primary_key: Vec<u8>,
    data: Vec<u8>,
}

/// Index records live in `unique_index_data` when the index is unique and in `index_data`
/// otherwise. The two tables differ only in their primary key, which is what makes the unique
/// one reject a second primary key for the same index key.
fn index_table(index: &object_store_index_model::Model) -> &'static str {
    if index.unique_index {
        "unique_index_data"
    } else {
        "index_data"
    }
}

/// Append `range` to `sql` as a comparison over `column`, pushing its bound values onto `values`.
///
/// [`range_to_query`] does the same thing through sea_query, but only ever over `object_data.key`.
/// The index tables need the identical comparison over their `value` column, and the join those
/// statements carry reads more clearly hand written than through a query builder.
fn append_range_predicate(
    sql: &mut String,
    values: &mut Vec<Value>,
    column: &str,
    range: &IndexedDBKeyRange,
) {
    if let Some(singleton) = range.as_singleton() {
        sql.push_str(&format!(" AND {column} = ?"));
        values.push(Value::Blob(encoding::serialize(singleton)));
        return;
    }
    if let Some(lower) = range.lower.as_ref() {
        let operator = if range.lower_open { ">" } else { ">=" };
        sql.push_str(&format!(" AND {column} {operator} ?"));
        values.push(Value::Blob(encoding::serialize(lower)));
    }
    if let Some(upper) = range.upper.as_ref() {
        let operator = if range.upper_open { "<" } else { "<=" };
        sql.push_str(&format!(" AND {column} {operator} ?"));
        values.push(Value::Blob(encoding::serialize(upper)));
    }
}

/// One table a readwrite transaction may change, described well enough to undo a change to it.
///
/// The three record tables are all `WITHOUT ROWID`, so an undo statement has to name a row by
/// its declared primary key rather than by a rowid the table does not have.
struct UndoLoggedTable {
    name: &'static str,
    /// Every column, in the order an `INSERT` lists them.
    columns: &'static [&'static str],
    /// The primary key columns, which are what a row is found by afterwards.
    key_columns: &'static [&'static str],
}

/// The tables whose contents belong to a readwrite transaction.
///
/// `object_store` is deliberately absent. Its `auto_increment` column carries the key
/// generator's current number, and the scheduler already snapshots that when the transaction is
/// registered and writes it back when the transaction aborts; logging it here as well would
/// revert it twice.
const UNDO_LOGGED_TABLES: [UndoLoggedTable; 3] = [
    UndoLoggedTable {
        name: "object_data",
        columns: &["object_store_id", "key", "data"],
        key_columns: &["object_store_id", "key"],
    },
    UndoLoggedTable {
        name: "index_data",
        columns: &[
            "index_id",
            "value",
            "object_data_key",
            "object_store_id",
            "value_locale",
        ],
        key_columns: &["index_id", "value", "object_data_key"],
    },
    UndoLoggedTable {
        name: "unique_index_data",
        columns: &[
            "index_id",
            "value",
            "object_store_id",
            "object_data_key",
            "value_locale",
        ],
        key_columns: &["index_id", "value"],
    },
];

/// Build the SQL expression a trigger body concatenates to name one row of `table`.
///
/// `row` is `new` or `old`, whichever alias holds the row the undo statement has to find.
fn undo_where_clause(table: &UndoLoggedTable, row: &str) -> String {
    table
        .key_columns
        .iter()
        .enumerate()
        .map(|(position, column)| {
            let separator = if position == 0 { " WHERE " } else { " AND " };
            format!("'{separator}{column}=' || quote({row}.{column})")
        })
        .collect::<Vec<_>>()
        .join(" || ")
}

pub struct SqliteEngine {
    db_path: PathBuf,
    connection: Connection,
    read_pool: Arc<ThreadPool>,
    write_pool: Arc<ThreadPool>,
}

impl SqliteEngine {
    fn object_store_by_name(
        connection: &Connection,
        store_name: &str,
    ) -> Result<object_store_model::Model, Error> {
        connection.query_row(
            "SELECT * FROM object_store WHERE name = ?",
            params![store_name.to_string()],
            |row| object_store_model::Model::try_from(row),
        )
    }

    /// The table holding the statements that would undo the writes of each live transaction.
    ///
    /// It is created outside [`Self::init_db`] because that returns early for a database that
    /// already exists, and a database written by an earlier build has every other table but not
    /// this one.
    fn create_undo_log(connection: &Connection) -> Result<(), Error> {
        connection.execute(
            "CREATE TABLE IF NOT EXISTS undo_log (
                seq                INTEGER PRIMARY KEY AUTOINCREMENT,
                transaction_serial INTEGER NOT NULL,
                statement          TEXT    NOT NULL
            )",
            [],
        )?;
        // A transaction that was live when the process died left its undo statements behind.
        // They describe a state the database no longer has any transaction waiting to return
        // to, and replaying them later against whatever a new transaction reuses the number for
        // would corrupt it, so the log starts empty.
        connection.execute("DELETE FROM undo_log", [])?;
        Ok(())
    }

    /// Record, for the duration of this connection, how to undo every row `serial_number`
    /// writes.
    ///
    /// This is SQLite's own undo/redo recipe: a trigger per table per statement kind writes the
    /// SQL text that would put the row back, and [`Self::rollback_transaction`] runs those
    /// statements in reverse. `quote()` renders a blob, a NULL and a string the way SQL reads
    /// them back, which is what lets the undo travel as text.
    ///
    /// The triggers are `TEMP`, so they belong to this connection alone and end with it. The
    /// scheduler's own connection therefore never has them, and the writes it makes while
    /// reverting are not themselves logged.
    fn install_undo_log_triggers(connection: &Connection, serial_number: u64) -> Result<(), Error> {
        // Conflict resolution inside `INSERT OR REPLACE` deletes the row it replaces, and that
        // deletion only reaches a delete trigger when recursive triggers are on. The index
        // tables are written that way, so without this an index record could be replaced with
        // no record of what it held.
        connection.execute_batch("PRAGMA recursive_triggers = ON;")?;

        let serial = i64::from_ne_bytes(serial_number.to_ne_bytes());
        for table in &UNDO_LOGGED_TABLES {
            let name = table.name;
            let columns = table.columns.join(",");
            let restored_values = table
                .columns
                .iter()
                .enumerate()
                .map(|(position, column)| {
                    let separator = if position == 0 { "" } else { "," };
                    format!("'{separator}' || quote(old.{column})")
                })
                .collect::<Vec<_>>()
                .join(" || ");
            let restored_assignments = table
                .columns
                .iter()
                .enumerate()
                .map(|(position, column)| {
                    let separator = if position == 0 { " SET " } else { "," };
                    format!("'{separator}{column}=' || quote(old.{column})")
                })
                .collect::<Vec<_>>()
                .join(" || ");
            // An insert and an update are both undone against the row as it now stands, so
            // both find it by the `new` alias. A delete has no row left to find.
            let find_row = undo_where_clause(table, "new");

            connection.execute_batch(&format!(
                "CREATE TEMP TRIGGER undo_{name}_insert AFTER INSERT ON {name} BEGIN
                     INSERT INTO undo_log (transaction_serial, statement)
                     VALUES ({serial}, 'DELETE FROM {name}' || {find_row});
                 END;
                 CREATE TEMP TRIGGER undo_{name}_delete AFTER DELETE ON {name} BEGIN
                     INSERT INTO undo_log (transaction_serial, statement)
                     VALUES ({serial}, 'INSERT INTO {name} ({columns}) VALUES (' || {restored_values} || ')');
                 END;
                 CREATE TEMP TRIGGER undo_{name}_update AFTER UPDATE ON {name} BEGIN
                     INSERT INTO undo_log (transaction_serial, statement)
                     VALUES ({serial}, 'UPDATE {name}' || {restored_assignments} || {find_row});
                 END;"
            ))?;
        }
        Ok(())
    }

    // TODO: intake dual pools
    pub fn new(
        path: PathBuf,
        _created: bool,
        db_info: &IndexedDBDescription,
        pool: Arc<ThreadPool>,
    ) -> Result<Self, Error> {
        let db_path = path.join("indexeddb.sqlite");
        let connection = Self::init_db(&db_path, db_info)?;
        Self::create_undo_log(&connection)?;

        for stmt in DB_PRAGMAS {
            // TODO: Handle errors properly
            let _ = connection.execute(stmt, ());
        }

        Ok(Self {
            connection,
            db_path,
            read_pool: pool.clone(),
            write_pool: pool,
        })
    }

    fn init_db(path: &Path, db_info: &IndexedDBDescription) -> Result<Connection, Error> {
        let connection = Connection::open(path)?;
        if connection.table_exists(None, "database")? {
            // Database already exists, no need to initialize
            return Ok(connection);
        }
        info!("Initializing indexeddb database at {:?}", path);
        for stmt in DB_INIT_PRAGMAS {
            // FIXME(arihant2math): this fails occasionally
            let _ = connection.execute(stmt, ());
        }
        create::create_tables(&connection)?;
        // From https://w3c.github.io/IndexedDB/#database-version:
        // "When a database is first created, its version is 0 (zero)."
        connection.execute(
            "INSERT INTO database (name, origin, version) VALUES (?, ?, ?)",
            params![
                db_info.name.to_owned(),
                db_info.origin.to_owned().ascii_serialization(),
                i64::from_ne_bytes(0_u64.to_ne_bytes())
            ],
        )?;
        Ok(connection)
    }

    fn get(
        connection: &Connection,
        store: object_store_model::Model,
        key_range: IndexedDBKeyRange,
    ) -> Result<Option<object_data_model::Model>, Error> {
        let query = range_to_query(key_range);
        let (sql, values) = sea_query::Query::select()
            .from(object_data_model::Column::Table)
            .columns(vec![
                object_data_model::Column::ObjectStoreId,
                object_data_model::Column::Key,
                object_data_model::Column::Data,
            ])
            .and_where(query.and(Expr::col(object_data_model::Column::ObjectStoreId).is(store.id)))
            .limit(1)
            .build_rusqlite(SqliteQueryBuilder);
        connection
            .prepare(&sql)?
            .query_one(&*values.as_params(), |row| {
                object_data_model::Model::try_from(row)
            })
            .optional()
    }

    fn get_key(
        connection: &Connection,
        store: object_store_model::Model,
        key_range: IndexedDBKeyRange,
    ) -> Result<Option<Vec<u8>>, Error> {
        Self::get(connection, store, key_range).map(|opt| opt.map(|model| model.key))
    }

    fn get_item(
        connection: &Connection,
        store: object_store_model::Model,
        key_range: IndexedDBKeyRange,
    ) -> Result<Option<Vec<u8>>, Error> {
        Self::get(connection, store, key_range).map(|opt| opt.map(|model| model.data))
    }

    /// The records of an object store that a key range covers, in ascending key order.
    ///
    /// An object store record's key and primary key are the same key, which is what makes the
    /// answer the same shape as an index request's.
    fn object_store_records(
        connection: &Connection,
        store: object_store_model::Model,
        key_range: IndexedDBKeyRange,
        count: Option<u32>,
        shape: RecordsShape,
    ) -> Result<Vec<SourceRecord>, Error> {
        let query = range_to_query(key_range);
        let mut sql_query = sea_query::Query::select();
        sql_query
            .from(object_data_model::Column::Table)
            .column(object_data_model::Column::Key);
        // A key-only request pays for every stored byte it selects and then discards.
        if shape == RecordsShape::WithValues {
            sql_query.column(object_data_model::Column::Data);
        }
        sql_query
            .and_where(query.and(Expr::col(object_data_model::Column::ObjectStoreId).is(store.id)))
            // Every operation reaching here (getAll, getAllKeys, getAllRecords, cursor
            // iteration) is defined in key order, and a LIMIT without an ORDER BY truncates
            // an unspecified subset rather than the first `count` records.
            .order_by(object_data_model::Column::Key, sea_query::Order::Asc);
        // "If count is not given or is 0 (zero), let count be infinity."
        // <https://w3c.github.io/IndexedDB/#retrieve-multiple-values-from-an-object-store>
        // A `LIMIT 0` answers with nothing, which is the opposite of what a zero count asks for.
        if let Some(count) = count.filter(|count| *count > 0) {
            sql_query.limit(count as u64);
        }
        let (sql, values) = sql_query.build_rusqlite(SqliteQueryBuilder);
        let mut stmt = connection.prepare(&sql)?;
        let records = stmt
            .query_and_then(&*values.as_params(), |row| {
                let key: Vec<u8> = row.get(0)?;
                Ok::<SourceRecord, Error>(SourceRecord {
                    key: key.clone(),
                    primary_key: key,
                    data: match shape {
                        RecordsShape::WithValues => row.get(1)?,
                        RecordsShape::KeysOnly => Vec::new(),
                    },
                })
            })?
            .collect::<Result<Vec<_>, _>>()?;
        Ok(records)
    }

    /// Look up one declared index of a store by name.
    fn index_by_name(
        connection: &Connection,
        store_id: i32,
        index_name: &str,
    ) -> Result<Option<object_store_index_model::Model>, Error> {
        connection
            .prepare("SELECT * FROM object_store_index WHERE object_store_id = ? AND name = ?")
            .and_then(|mut stmt| {
                stmt.query_row(params![store_id, index_name], |row| {
                    object_store_index_model::Model::try_from(row)
                })
                .optional()
            })
    }

    /// The records an index request ranges over, in index key order and then primary key order.
    ///
    /// The index key is what the request's key range applies to and the object store key rides
    /// along beside it. Keeping the pair distinct is the whole point: an index cursor's `key` is
    /// the index key and its `primaryKey` is the object store key, and the two coincide only for
    /// an object store request. An index that no longer exists yields no records rather than an
    /// error, because the transaction that deleted it has already invalidated the request.
    fn index_records(
        connection: &Connection,
        store: &object_store_model::Model,
        index_name: &str,
        key_range: IndexedDBKeyRange,
        count: Option<u32>,
        shape: RecordsShape,
    ) -> Result<Vec<SourceRecord>, Error> {
        let Some(index) = Self::index_by_name(connection, store.id, index_name)? else {
            return Ok(Vec::new());
        };
        let table = index_table(&index);
        // The join stays even for a key-only request: it is what drops an index record whose
        // object store row is already gone. Only the selected value changes.
        let value_column = match shape {
            RecordsShape::WithValues => ", o.data",
            RecordsShape::KeysOnly => "",
        };
        let mut sql = format!(
            "SELECT i.value, i.object_data_key{value_column} FROM {table} i \
             JOIN object_data o \
             ON o.object_store_id = i.object_store_id AND o.key = i.object_data_key \
             WHERE i.index_id = ? AND i.object_store_id = ?"
        );
        let mut values = vec![
            Value::Integer(index.id as i64),
            Value::Integer(store.id as i64),
        ];
        append_range_predicate(&mut sql, &mut values, "i.value", &key_range);
        sql.push_str(" ORDER BY i.value ASC, i.object_data_key ASC");
        // A zero count is infinity, not an empty answer. See `get_all`.
        if let Some(count) = count.filter(|count| *count > 0) {
            sql.push_str(" LIMIT ?");
            values.push(Value::Integer(count as i64));
        }
        let mut stmt = connection.prepare(&sql)?;
        let records = stmt
            .query_and_then(params_from_iter(values), |row| {
                Ok::<SourceRecord, Error>(SourceRecord {
                    key: row.get(0)?,
                    primary_key: row.get(1)?,
                    data: match shape {
                        RecordsShape::WithValues => row.get(2)?,
                        RecordsShape::KeysOnly => Vec::new(),
                    },
                })
            })?
            .collect::<Result<Vec<_>, _>>()?;
        Ok(records)
    }

    /// How many index records the range covers, counted without loading any stored value.
    fn index_count(
        connection: &Connection,
        store: &object_store_model::Model,
        index_name: &str,
        key_range: IndexedDBKeyRange,
    ) -> Result<u64, Error> {
        let Some(index) = Self::index_by_name(connection, store.id, index_name)? else {
            return Ok(0);
        };
        let table = index_table(&index);
        let mut sql = format!(
            "SELECT COUNT(*) FROM {table} i WHERE i.index_id = ? AND i.object_store_id = ?"
        );
        let mut values = vec![
            Value::Integer(index.id as i64),
            Value::Integer(store.id as i64),
        ];
        append_range_predicate(&mut sql, &mut values, "i.value", &key_range);
        let count: i64 =
            connection
                .prepare(&sql)?
                .query_row(params_from_iter(values), |row| row.get(0))?;
        Ok(count as u64)
    }

    /// Drop every index record pointing at any of `primary_keys`.
    fn delete_index_records(
        connection: &Connection,
        store_id: i32,
        primary_keys: &[Vec<u8>],
    ) -> Result<(), Error> {
        for key in primary_keys {
            for table in ["index_data", "unique_index_data"] {
                connection.execute(
                    &format!(
                        "DELETE FROM {table} WHERE object_store_id = ? AND object_data_key = ?"
                    ),
                    params![store_id, key],
                )?;
            }
        }
        Ok(())
    }

    /// The name of the first unique index `index_updates` would collide on, if any.
    ///
    /// This runs before the value is stored. A unique index rejects the whole put, so finding the
    /// collision after writing `object_data` would leave the store holding a record the
    /// transaction is about to be told never landed. A key already held by this same primary key
    /// is not a collision: that is the record being overwritten.
    fn unique_index_conflict(
        connection: &Connection,
        store_id: i32,
        primary_key: &[u8],
        index_updates: &[KvsIndexUpdate],
    ) -> Result<Option<String>, Error> {
        for update in index_updates {
            let Some(index) = Self::index_by_name(connection, store_id, &update.index_name)? else {
                continue;
            };
            if !index.unique_index {
                continue;
            }
            for key in &update.keys {
                let value = encoding::serialize(key);
                let holder: Option<Vec<u8>> = connection
                    .prepare(
                        "SELECT object_data_key FROM unique_index_data WHERE index_id = ? AND value = ?",
                    )
                    .and_then(|mut stmt| {
                        stmt.query_row(params![index.id, value], |row| row.get(0))
                            .optional()
                    })?;
                if holder.is_some_and(|held| held != primary_key) {
                    return Ok(Some(update.index_name.clone()));
                }
            }
        }
        Ok(None)
    }

    /// Replace the index records that point at `primary_key`.
    ///
    /// A put rewrites the whole record, so every index key extracted from the old value stops
    /// being true the moment the new one lands. Deleting this primary key's records first and
    /// inserting the freshly extracted keys afterwards is what keeps the index tables agreeing
    /// with `object_data`. The keys themselves come from the script thread, which owns the
    /// JavaScript value the key path is evaluated against; a multiEntry index arrives here as one
    /// update carrying several keys.
    fn replace_index_records(
        connection: &Connection,
        store_id: i32,
        primary_key: &[u8],
        index_updates: &[KvsIndexUpdate],
    ) -> Result<(), Error> {
        Self::delete_index_records(connection, store_id, &[primary_key.to_vec()])?;
        for update in index_updates {
            let Some(index) = Self::index_by_name(connection, store_id, &update.index_name)? else {
                continue;
            };
            let table = index_table(&index);
            for key in &update.keys {
                let value = encoding::serialize(key);
                connection.execute(
                    &format!(
                        "INSERT OR REPLACE INTO {table} \
                         (index_id, value, object_store_id, object_data_key) VALUES (?, ?, ?, ?)"
                    ),
                    params![index.id, value, store_id, primary_key],
                )?;
            }
        }
        Ok(())
    }

    /// Write the index records a newly created index needs for the records the store already
    /// holds.
    ///
    /// <https://w3c.github.io/IndexedDB/#dom-idbobjectstore-createindex> step 12. The keys are
    /// extracted on the script thread, so all this does is insert them and answer whether a
    /// unique index found two records under one key. The index row was inserted by the schema
    /// operation that ran ahead of this one, so it holds no records yet and the only collisions
    /// possible are between entries in this call; the read below still goes to the table, so a
    /// row that arrived some other way is caught too.
    fn backfill_index(
        connection: &Connection,
        store: object_store_model::Model,
        index_name: &str,
        entries: &[IndexBackfillEntry],
    ) -> Result<BackfillIndexResult, Error> {
        let Some(index) = Self::index_by_name(connection, store.id, index_name)? else {
            // The transaction that created the index has already deleted it again. There is
            // nothing to populate and nothing to refuse.
            return Ok(BackfillIndexResult::Done);
        };
        let table = index_table(&index);
        for entry in entries {
            let primary_key = encoding::serialize(&entry.primary_key);
            for key in &entry.keys {
                let value = encoding::serialize(key);
                if index.unique_index {
                    let holder: Option<Vec<u8>> = connection
                        .prepare(
                            "SELECT object_data_key FROM unique_index_data \
                             WHERE index_id = ? AND value = ?",
                        )
                        .and_then(|mut stmt| {
                            stmt.query_row(params![index.id, value], |row| row.get(0))
                                .optional()
                        })?;
                    if holder.is_some_and(|held| held != primary_key) {
                        return Ok(BackfillIndexResult::UniqueConstraintViolated);
                    }
                }
                connection.execute(
                    &format!(
                        "INSERT OR REPLACE INTO {table} \
                         (index_id, value, object_store_id, object_data_key) VALUES (?, ?, ?, ?)"
                    ),
                    params![index.id, value, store.id, primary_key],
                )?;
            }
        }
        Ok(BackfillIndexResult::Done)
    }

    fn put_item(
        connection: &Connection,
        store: object_store_model::Model,
        key: IndexedDBKeyType,
        value: Vec<u8>,
        should_overwrite: bool,
        key_generator_current_number: Option<i64>,
        index_updates: &[KvsIndexUpdate],
    ) -> Result<PutItemResult, Error> {
        let no_overwrite = !should_overwrite;
        let serialized_key: Vec<u8> = encoding::serialize(&key);
        let existing_item = connection
            .prepare("SELECT * FROM object_data WHERE key = ? AND object_store_id = ?")
            .and_then(|mut stmt| {
                stmt.query_row(params![serialized_key, store.id], |row| {
                    object_data_model::Model::try_from(row)
                })
                .optional()
            })?;
        if let Some(index_name) =
            Self::unique_index_conflict(connection, store.id, &serialized_key, index_updates)?
        {
            return Ok(PutItemResult::IndexConstraintViolated(index_name));
        }
        if existing_item.is_some() {
            if no_overwrite {
                return Ok(PutItemResult::CannotOverwrite);
            }
            // Preserve `put()` semantics by replacing the stored value when the primary
            // key already exists.
            connection.execute(
                "UPDATE object_data SET data = ? WHERE object_store_id = ? AND key = ?",
                params![value, store.id, serialized_key],
            )?;
        } else {
            connection.execute(
                "INSERT INTO object_data (object_store_id, key, data) VALUES (?, ?, ?)",
                params![store.id, serialized_key, value],
            )?;
        }
        Self::replace_index_records(connection, store.id, &serialized_key, index_updates)?;
        if let Some(next_key_generator_current_number) = key_generator_current_number {
            connection.execute(
                "UPDATE object_store SET auto_increment = ? WHERE id = ?",
                params![next_key_generator_current_number, store.id],
            )?;
        }
        Ok(PutItemResult::Key(key))
    }

    fn delete_item(
        connection: &Connection,
        store: object_store_model::Model,
        key_range: IndexedDBKeyRange,
    ) -> Result<(), Error> {
        // The index records name their primary keys, so the set has to be read before the rows
        // that define it are gone.
        let removed = Self::object_store_records(
            connection,
            store.clone(),
            key_range.clone(),
            None,
            RecordsShape::KeysOnly,
        )?
        .into_iter()
        .map(|record| record.key)
        .collect::<Vec<_>>();
        Self::delete_index_records(connection, store.id, &removed)?;
        let query = range_to_query(key_range);
        let (sql, values) = sea_query::Query::delete()
            .from_table(object_data_model::Column::Table)
            .and_where(query.and(Expr::col(object_data_model::Column::ObjectStoreId).is(store.id)))
            .build_rusqlite(SqliteQueryBuilder);
        connection.prepare(&sql)?.execute(&*values.as_params())?;
        Ok(())
    }

    fn clear(connection: &Connection, store: object_store_model::Model) -> Result<(), Error> {
        for table in ["index_data", "unique_index_data"] {
            connection.execute(
                &format!("DELETE FROM {table} WHERE object_store_id = ?"),
                params![store.id],
            )?;
        }
        connection.execute(
            "DELETE FROM object_data WHERE object_store_id = ?",
            params![store.id],
        )?;
        Ok(())
    }

    fn count(
        connection: &Connection,
        store: object_store_model::Model,
        key_range: IndexedDBKeyRange,
    ) -> Result<usize, Error> {
        let query = range_to_query(key_range);
        let (sql, values) = sea_query::Query::select()
            .expr(Expr::col(object_data_model::Column::Key).count())
            .from(object_data_model::Column::Table)
            .and_where(query.and(Expr::col(object_data_model::Column::ObjectStoreId).is(store.id)))
            .build_rusqlite(SqliteQueryBuilder);
        connection
            .prepare(&sql)?
            .query_row(&*values.as_params(), |row| row.get(0))
            .map(|count: i64| count as usize)
    }

    fn create_store(
        connection: &Connection,
        store_name: &str,
        key_path: Option<KeyPath>,
        auto_increment: bool,
    ) -> Result<CreateObjectResult, Error> {
        let mut stmt = connection.prepare("SELECT * FROM object_store WHERE name = ?")?;
        if stmt.exists(params![store_name.to_string()])? {
            // Store already exists
            return Ok(CreateObjectResult::AlreadyExists);
        }
        connection.execute(
            "INSERT INTO object_store (name, key_path, auto_increment) VALUES (?, ?, ?)",
            params![
                store_name.to_string(),
                key_path.as_ref().map(encode_key_path).transpose()?,
                auto_increment as i32
            ],
        )?;
        Ok(CreateObjectResult::Created)
    }

    fn delete_store(connection: &Connection, store_name: &str) -> Result<(), Error> {
        // https://www.w3.org/TR/IndexedDB-3/#dom-idbdatabase-deleteobjectstore
        // Step 7. Destroy store.
        let object_store = Self::object_store_by_name(connection, store_name)?;

        connection.execute(
            "DELETE FROM index_data WHERE object_store_id = ?",
            params![object_store.id],
        )?;
        connection.execute(
            "DELETE FROM unique_index_data WHERE object_store_id = ?",
            params![object_store.id],
        )?;
        connection.execute(
            "DELETE FROM object_store_index WHERE object_store_id = ?",
            params![object_store.id],
        )?;
        connection.execute(
            "DELETE FROM object_data WHERE object_store_id = ?",
            params![object_store.id],
        )?;
        let result = connection.execute(
            "DELETE FROM object_store WHERE id = ?",
            params![object_store.id],
        )?;
        if result == 0 {
            Err(Error::QueryReturnedNoRows)
        } else if result > 1 {
            Err(Error::QueryReturnedMoreThanOneRow)
        } else {
            Ok(())
        }
    }

    /// <https://www.w3.org/TR/IndexedDB/#dom-idbobjectstore-name>
    /// Step 9. Set store's name to name.
    /// Every row that belongs to the store keys off the store's id, so nothing below the
    /// `object_store` row moves with it.
    fn rename_store(
        connection: &Connection,
        store_name: &str,
        new_name: &str,
    ) -> Result<(), Error> {
        let object_store = Self::object_store_by_name(connection, store_name)?;
        let rows_affected = connection.execute(
            "UPDATE object_store SET name = ? WHERE id = ?",
            params![new_name, object_store.id],
        )?;
        if rows_affected == 0 {
            return Err(Error::QueryReturnedNoRows);
        }
        Ok(())
    }

    fn create_index(
        connection: &Connection,
        store_name: &str,
        index_name: String,
        key_path: KeyPath,
        unique: bool,
        multi_entry: bool,
    ) -> Result<CreateObjectResult, Error> {
        let object_store = connection.query_row(
            "SELECT * FROM object_store WHERE name = ?",
            params![store_name.to_string()],
            |row| object_store_model::Model::try_from(row),
        )?;

        let index_exists: bool = connection.query_row(
            "SELECT EXISTS(SELECT * FROM object_store_index WHERE name = ? AND object_store_id = ?)",
            params![index_name, object_store.id],
            |row| row.get(0),
        )?;
        if index_exists {
            return Ok(CreateObjectResult::AlreadyExists);
        }

        connection.execute(
            "INSERT INTO object_store_index (object_store_id, name, key_path, unique_index, multi_entry_index)\
            VALUES (?, ?, ?, ?, ?)",
            params![
                object_store.id,
                index_name,
                encode_key_path(&key_path)?,
                unique,
                multi_entry,
            ],
        )?;
        Ok(CreateObjectResult::Created)
    }

    fn rename_index(
        connection: &Connection,
        store_name: &str,
        index_name: &str,
        new_name: &str,
    ) -> Result<(), Error> {
        let object_store = connection.query_row(
            "SELECT * FROM object_store WHERE name = ?",
            params![store_name],
            |row| object_store_model::Model::try_from(row),
        )?;

        // Rename the index if it exists
        let _ = connection.execute(
            "UPDATE object_store_index SET name = ? WHERE name = ? AND object_store_id = ?",
            params![new_name, index_name, object_store.id],
        )?;
        Ok(())
    }

    fn delete_index(
        connection: &Connection,
        store_name: &str,
        index_name: String,
    ) -> Result<(), Error> {
        let object_store = connection.query_row(
            "SELECT * FROM object_store WHERE name = ?",
            params![store_name.to_string()],
            |row| object_store_model::Model::try_from(row),
        )?;

        // Delete the index's records before the row that gives them their index_id.
        if let Some(index) = Self::index_by_name(connection, object_store.id, &index_name)? {
            let table = index_table(&index);
            connection.execute(
                &format!("DELETE FROM {table} WHERE index_id = ?"),
                params![index.id],
            )?;
        }
        let _ = connection.execute(
            "DELETE FROM object_store_index WHERE name = ? AND object_store_id = ?",
            params![index_name, object_store.id],
        )?;
        Ok(())
    }
}

impl KvsEngine for SqliteEngine {
    fn create_store(
        &self,
        store_name: &str,
        key_path: Option<KeyPath>,
        auto_increment: bool,
    ) -> BackendResult<CreateObjectResult> {
        Self::create_store(&self.connection, store_name, key_path, auto_increment)
            .map_err(backend_error_from_sqlite_error)
    }
    fn create_index(
        &self,
        store_name: &str,
        index_name: String,
        key_path: KeyPath,
        unique: bool,
        multi_entry: bool,
    ) -> BackendResult<CreateObjectResult> {
        Self::create_index(
            &self.connection,
            store_name,
            index_name,
            key_path,
            unique,
            multi_entry,
        )
        .map_err(backend_error_from_sqlite_error)
    }

    fn delete_store(&self, store_name: &str) -> BackendResult<()> {
        Self::delete_store(&self.connection, store_name).map_err(backend_error_from_sqlite_error)
    }

    fn close_store(&self, _store_name: &str) -> BackendResult<()> {
        // TODO: do something
        Ok(())
    }

    fn process_transaction(
        &self,
        transaction: KvsTransaction,
        on_complete: Box<dyn FnOnce() + Send + 'static>,
    ) {
        let spawning_pool = if transaction.mode == IndexedDBTxnMode::Readonly {
            self.read_pool.clone()
        } else {
            self.write_pool.clone()
        };
        let path = self.db_path.clone();
        let serial_number = transaction.serial_number;
        let undo_logged = transaction.mode == IndexedDBTxnMode::Readwrite;
        spawning_pool.spawn(move || {
            let connection = match Connection::open(path) {
                Ok(connection) => connection,
                Err(error) => {
                    for request in transaction.requests {
                        request
                            .operation
                            .notify_error(BackendError::DbErr(format!("{error:?}")));
                    }
                    on_complete();
                    return;
                },
            };
            // Only a readwrite transaction is undone row by row. An upgrade transaction is
            // reverted by rebuilding the schema it started from, which gives stores it
            // recreates new identifiers, so rows carrying the old ones have nothing to go back
            // to; reverting the records an upgrade wrote is a separate piece of work. A
            // readonly transaction writes nothing to undo.
            if undo_logged {
                if let Err(error) = Self::install_undo_log_triggers(&connection, serial_number) {
                    // Without the triggers the transaction would look like it could be aborted
                    // and then silently keep its writes, so it is refused instead.
                    for request in transaction.requests {
                        request
                            .operation
                            .notify_error(BackendError::DbErr(format!("{error:?}")));
                    }
                    on_complete();
                    return;
                }
            }
            for request in transaction.requests {
                // The pinned SQLite implementation has schema support for indexes but no index
                // request methods or index-record maintenance. Preserve its behavior by handling
                // the operation against the object store while still carrying `request.context`
                // intact to custom engines through the public KvsEngine contract.
                if let AsyncOperation::Schema(AsyncSchemaOperation::CreateObjectStore {
                    callback,
                    key_path,
                    auto_increment
                }) = &request.operation {
                    if let Err(error) =
                        Self::create_store(&connection, &request.store_name, key_path.clone(), *auto_increment)
                    {
                        let _ = callback.send(BackendError::DbErr(format!("{error:?}")));
                    }
                    continue;
                }

                let object_store = connection
                    .prepare("SELECT * FROM object_store WHERE name = ?")
                    .and_then(|mut stmt| {
                        stmt.query_row(params![request.store_name.to_string()], |row| {
                            object_store_model::Model::try_from(row)
                        })
                        .optional()
                    });
                let object_store = match object_store {
                    Ok(Some(store)) => store,
                    Ok(None) => {
                        request.operation.notify_error(BackendError::StoreNotFound);
                        continue;
                    },
                    Err(error) => {
                        request
                            .operation
                            .notify_error(BackendError::DbErr(format!("{error:?}")));
                        continue;
                    },
                };

                // The target says which of the store's surfaces the request addressed. The six
                // read operations are deliberately target agnostic on the wire, so an index needs
                // no operation variants of its own, only this context to select which records
                // they range over.
                let context = request.context;
                match request.operation {
                    AsyncOperation::ReadWrite(AsyncReadWriteOperation::PutItem {
                        callback,
                        key,
                        value,
                        should_overwrite,
                        key_generator_current_number,
                    }) => {
                        let (key, key_generator_current_number) = match key {
                            Some(key) => (key, key_generator_current_number),
                            // <https://w3c.github.io/IndexedDB/#generate-a-key>. A store
                            // that has no key generator holds 0 here; every generator starts at
                            // 1 and only ever grows, so the one column carries both the flag and
                            // the generator's current number.
                            None => {
                                if object_store.auto_increment == 0 {
                                    if let Err(error) = callback.send(Err(BackendError::DbErr(
                                        "Missing key for PutItem request".to_string(),
                                    ))) {
                                        warn!("Failed to send PutItem missing key error: {error:?}");
                                    }
                                    continue;
                                }
                                // Step 3. If key is greater than 2^53 (9007199254740992), then
                                // return failure. An explicit key is allowed to push the
                                // generator one past that maximum, and this is what makes the
                                // next generated key fail instead of repeating 2^53 forever.
                                if object_store.auto_increment > 9_007_199_254_740_992 {
                                    let _ =
                                        callback.send(Ok(PutItemResult::KeyGeneratorExhausted));
                                    continue;
                                }
                                (
                                    IndexedDBKeyType::Number(object_store.auto_increment as f64),
                                    // Step 4. Increase the generator's current number by 1. The
                                    // check above leaves it at 2^53 or below, so this cannot
                                    // overflow an i64.
                                    Some(object_store.auto_increment + 1),
                                )
                            },
                        };
                        let _ = callback.send(
                            Self::put_item(
                                &connection,
                                object_store,
                                key,
                                value,
                                should_overwrite,
                                key_generator_current_number,
                                &context.index_updates,
                            )
                            .map_err(|e| BackendError::DbErr(format!("{:?}", e))),
                        );
                    },
                    AsyncOperation::ReadOnly(AsyncReadOnlyOperation::GetItem {
                        callback,
                        key_range,
                    }) => {
                        let result = match &context.target {
                            KvsOperationTarget::Index { name } => Self::index_records(
                                &connection,
                                &object_store,
                                name,
                                key_range,
                                Some(1),
                                RecordsShape::WithValues,
                            )
                            .map(|records| records.into_iter().next().map(|record| record.data)),
                            KvsOperationTarget::ObjectStore => {
                                Self::get_item(&connection, object_store, key_range)
                            },
                        };
                        let _ = callback
                            .send(result.map_err(|e| BackendError::DbErr(format!("{:?}", e))));
                    },
                    AsyncOperation::ReadWrite(AsyncReadWriteOperation::BackfillIndex {
                        callback,
                        index_name,
                        entries,
                    }) => {
                        let _ = callback.send(
                            Self::backfill_index(&connection, object_store, &index_name, &entries)
                                .map_err(|e| BackendError::DbErr(format!("{:?}", e))),
                        );
                    },
                    AsyncOperation::ReadWrite(AsyncReadWriteOperation::RemoveItem {
                        callback,
                        key_range,
                    }) => {
                        let _ = callback.send(
                            Self::delete_item(&connection, object_store, key_range)
                                .map_err(|e| BackendError::DbErr(format!("{:?}", e))),
                        );
                    },
                    AsyncOperation::ReadOnly(AsyncReadOnlyOperation::Count {
                        callback,
                        key_range,
                    }) => {
                        let result = match &context.target {
                            KvsOperationTarget::Index { name } => {
                                Self::index_count(&connection, &object_store, name, key_range)
                            },
                            KvsOperationTarget::ObjectStore => {
                                Self::count(&connection, object_store, key_range).map(|r| r as u64)
                            },
                        };
                        let _ = callback
                            .send(result.map_err(|e| BackendError::DbErr(format!("{:?}", e))));
                    },
                    AsyncOperation::ReadOnly(AsyncReadOnlyOperation::Iterate {
                        callback,
                        key_range,
                        count,
                        shape,
                    }) => {
                        // An object store record's key and primary key are the same key. An
                        // index record's are not, and keeping them apart here is what lets a
                        // cursor report the index key while continuing from the object store
                        // position, and `getAllKeys` on an index answer with primary keys.
                        //
                        // Direction is not applied here: the DOM applies it, the way IDBCursor
                        // already does. See ADR12.
                        let result = match &context.target {
                            KvsOperationTarget::Index { name } => Self::index_records(
                                &connection,
                                &object_store,
                                name,
                                key_range,
                                count,
                                shape,
                            ),
                            KvsOperationTarget::ObjectStore => Self::object_store_records(
                                &connection,
                                object_store,
                                key_range,
                                count,
                                shape,
                            ),
                        };
                        let _ = callback.send(
                            result
                                .and_then(|records: Vec<SourceRecord>| {
                                    records
                                        .into_iter()
                                        .map(|record| {
                                            Ok(IndexedDBRecord {
                                                key: decode_key(&record.key)?,
                                                primary_key: decode_key(&record.primary_key)?,
                                                value: record.data,
                                            })
                                        })
                                        .collect::<Result<Vec<_>, Error>>()
                                })
                                .map_err(|e| BackendError::DbErr(format!("{:?}", e))),
                        );
                    },
                    AsyncOperation::ReadWrite(AsyncReadWriteOperation::Clear(sender)) => {
                        let _ = sender.send(
                            Self::clear(&connection, object_store)
                                .map_err(|e| BackendError::DbErr(format!("{:?}", e))),
                        );
                    },
                    AsyncOperation::ReadOnly(AsyncReadOnlyOperation::GetKey {
                        callback,
                        key_range,
                    }) => {
                        // An index request answers with the primary key the index record points
                        // at, which is what `IDBIndex.getKey` is defined to return.
                        let result = match &context.target {
                            KvsOperationTarget::Index { name } => Self::index_records(
                                &connection,
                                &object_store,
                                name,
                                key_range,
                                Some(1),
                                RecordsShape::KeysOnly,
                            )
                            .map(|records| {
                                records.into_iter().next().map(|record| record.primary_key)
                            }),
                            KvsOperationTarget::ObjectStore => {
                                Self::get_key(&connection, object_store, key_range)
                            },
                        };
                        let _ = callback.send(
                            result
                                .and_then(|key| key.as_deref().map(decode_key).transpose())
                                .map_err(|e| BackendError::DbErr(format!("{:?}", e))),
                        );
                    },
                    AsyncOperation::Schema(AsyncSchemaOperation::CreateIndex {
                        callback,
                        index_name,
                        key_path,
                        unique,
                        multi_entry
                    }) => {
                        if let Err(error) = Self::create_index(
                            &connection,
                            &request.store_name,
                            index_name,
                            key_path,
                            unique,
                            multi_entry
                        ) {
                            let _ = callback.send(BackendError::DbErr(format!("{error:?}")));
                        }
                    },
                    AsyncOperation::Schema(AsyncSchemaOperation::CreateObjectStore {
                        callback, ..
                    }) => {
                        // The pre-pass above handles this and continues, because the store
                        // does not exist yet and so cannot survive the lookup every other
                        // operation needs. Reaching here would mean the two patterns have
                        // drifted apart. Report it rather than killing a storage thread that
                        // is serving every other database in the process.
                        let _ = callback.send(BackendError::DbErr(
                            "CreateObjectStore reached the main dispatch; the pre-pass above \
                             should have handled it"
                                .to_owned(),
                        ));
                    },
                    AsyncOperation::Schema(AsyncSchemaOperation::DeleteIndex { index_name, callback }) => {
                        if let Err(error) = Self::delete_index(&connection, &request.store_name, index_name) {
                            let _ = callback.send(BackendError::DbErr(format!("{error:?}")));
                        }
                    },
                    AsyncOperation::Schema(AsyncSchemaOperation::DeleteObjectStore { callback }) => {
                        if let Err(error) = Self::delete_store(&connection, &request.store_name) {
                            let _ = callback.send(BackendError::DbErr(format!("{error:?}")));
                        }
                    },
                    AsyncOperation::Schema(AsyncSchemaOperation::RenameObjectStore { new_name, callback }) => {
                        if let Err(error) = Self::rename_store(&connection, &request.store_name, &new_name) {
                            let _ = callback.send(BackendError::DbErr(format!("{error:?}")));
                        }
                    },
                    AsyncOperation::Schema(AsyncSchemaOperation::RenameIndex { index_name, new_name, callback }) =>  {
                        if let Err(error) = Self::rename_index(
                            &connection,
                            &request.store_name,
                            &index_name,
                            &new_name
                        ) {
                            let _ = callback.send(BackendError::DbErr(format!("{error:?}")));
                        }
                    },
                }
            }
            on_complete();
        });
    }

    fn key_generator_current_number(&self, store_name: &str) -> BackendResult<Option<i64>> {
        let load = || -> Result<Option<i64>, Error> {
            let mut stmt = self
                .connection
                .prepare("SELECT * FROM object_store WHERE name = ?")?;
            stmt.query_row(params![store_name.to_string()], |row| {
                Ok(object_store_model::Model::try_from(row)?.auto_increment)
            })
            .optional()
        };
        // Zero is the stored representation of "this store has no key generator". A failed
        // read is a different answer and now reaches the caller as one.
        Ok(load()
            .map_err(backend_error_from_sqlite_error)?
            .and_then(|current_number| (current_number != 0).then_some(current_number)))
    }

    fn set_key_generator_current_number(
        &self,
        store_name: &str,
        current_number: i64,
    ) -> BackendResult<()> {
        let update = || -> Result<(), Error> {
            // Ensure missing store is reported as QueryReturnedNoRows even when an
            // UPDATE might affect zero rows due to no-op assignment.
            let store_exists: bool = self.connection.query_row(
                "SELECT EXISTS(SELECT 1 FROM object_store WHERE name = ?)",
                params![store_name],
                |row| row.get(0),
            )?;
            if !store_exists {
                return Err(Error::QueryReturnedNoRows);
            }

            let rows_affected = self.connection.execute(
                "UPDATE object_store SET auto_increment = ? WHERE name = ?",
                params![current_number, store_name],
            )?;
            if rows_affected > 1 {
                return Err(Error::QueryReturnedMoreThanOneRow);
            }
            Ok(())
        };
        update().map_err(backend_error_from_sqlite_error)
    }

    /// `Ok(None)` still conflates "no such store" with "this store has no key path".
    /// That conflation is the one the old `TODO: Wrong, same issues as has_key_generator`
    /// named and it is unchanged here; what changed is that a failed read is no longer a
    /// third thing hiding inside it.
    fn key_path(&self, store_name: &str) -> BackendResult<Option<KeyPath>> {
        let load = || -> Result<Option<KeyPath>, Error> {
            let mut stmt = self
                .connection
                .prepare("SELECT * FROM object_store WHERE name = ?")?;
            stmt.query_row(params![store_name.to_string()], |row| {
                object_store_model::Model::try_from(row)?
                    .key_path
                    .map(|key_path| decode_key_path(&key_path))
                    .transpose()
            })
            .optional()
            .map(Option::flatten)
        };
        load().map_err(backend_error_from_sqlite_error)
    }

    fn object_store_names(&self) -> BackendResult<Vec<String>> {
        let load = || -> Result<Vec<String>, Error> {
            let mut stmt = self.connection.prepare("SELECT name FROM object_store")?;
            stmt.query_map([], |row| row.get(0))?
                .collect::<Result<Vec<_>, _>>()
        };
        load().map_err(backend_error_from_sqlite_error)
    }

    fn indexes(&self, store_name: &str) -> BackendResult<Vec<IndexedDBIndex>> {
        let load = || -> Result<Vec<IndexedDBIndex>, Error> {
            let object_store = self.connection.query_row(
                "SELECT * FROM object_store WHERE name = ?",
                params![store_name.to_string()],
                |row| object_store_model::Model::try_from(row),
            )?;

            let mut stmt = self
                .connection
                .prepare("SELECT * FROM object_store_index WHERE object_store_id = ?")?;
            let indexes = stmt
                .query_map(params![object_store.id], |row| {
                    let model = object_store_index_model::Model::try_from(row)?;
                    Ok(IndexedDBIndex {
                        name: model.name,
                        key_path: decode_key_path(&model.key_path)?,
                        unique: model.unique_index,
                        multi_entry: model.multi_entry_index,
                    })
                })?
                .collect::<Result<Vec<_>, _>>()?;
            Ok(indexes)
        };
        load().map_err(backend_error_from_sqlite_error)
    }

    fn delete_index(&self, store_name: &str, index_name: String) -> BackendResult<()> {
        Self::delete_index(&self.connection, store_name, index_name)
            .map_err(backend_error_from_sqlite_error)
    }

    fn rename_store(&self, store_name: &str, new_name: &str) -> BackendResult<()> {
        Self::rename_store(&self.connection, store_name, new_name)
            .map_err(backend_error_from_sqlite_error)
    }

    fn rename_index(
        &self,
        store_name: &str,
        index_name: &str,
        new_name: &str,
    ) -> BackendResult<()> {
        Self::rename_index(&self.connection, store_name, index_name, new_name)
            .map_err(backend_error_from_sqlite_error)
    }

    fn version(&self) -> BackendResult<u64> {
        self.connection
            .query_row("SELECT version FROM database LIMIT 1", [], |row| row.get(0))
            .map(|version: i64| u64::from_ne_bytes(version.to_ne_bytes()))
            .map_err(backend_error_from_sqlite_error)
    }

    fn set_version(&self, version: u64) -> BackendResult<()> {
        let update = || -> Result<(), Error> {
            let rows_affected = self.connection.execute(
                "UPDATE database SET version = ?",
                params![i64::from_ne_bytes(version.to_ne_bytes())],
            )?;
            if rows_affected == 0 {
                return Err(Error::QueryReturnedNoRows);
            }
            Ok(())
        };
        update().map_err(backend_error_from_sqlite_error)
    }

    fn rollback_transaction(&self, serial_number: u64) -> BackendResult<()> {
        let serial = i64::from_ne_bytes(serial_number.to_ne_bytes());
        let replay = || -> Result<(), Error> {
            // The statements undo one write each, so they have to run against the database the
            // write after them has already been taken back out of: newest first.
            let statements = self
                .connection
                .prepare(
                    "SELECT statement FROM undo_log \
                     WHERE transaction_serial = ? ORDER BY seq DESC",
                )
                .and_then(|mut stmt| {
                    stmt.query_map(params![serial], |row| row.get::<_, String>(0))?
                        .collect::<Result<Vec<String>, Error>>()
                })?;
            for statement in statements {
                self.connection.execute_batch(&statement)?;
            }
            self.connection.execute(
                "DELETE FROM undo_log WHERE transaction_serial = ?",
                params![serial],
            )?;
            Ok(())
        };

        // A half-applied undo is a state no transaction ever wrote, so the replay either lands
        // whole or leaves the database as the abort found it and says why.
        self.connection
            .execute_batch("BEGIN IMMEDIATE")
            .map_err(backend_error_from_sqlite_error)?;
        match replay() {
            Ok(()) => self
                .connection
                .execute_batch("COMMIT")
                .map_err(backend_error_from_sqlite_error),
            Err(error) => {
                if let Err(rollback_error) = self.connection.execute_batch("ROLLBACK") {
                    warn!("Failed to roll back a failed undo replay: {rollback_error:?}");
                }
                Err(backend_error_from_sqlite_error(error))
            },
        }
    }

    fn commit_transaction(&self, serial_number: u64) -> BackendResult<()> {
        let serial = i64::from_ne_bytes(serial_number.to_ne_bytes());
        self.connection
            .execute(
                "DELETE FROM undo_log WHERE transaction_serial = ?",
                params![serial],
            )
            .map(|_| ())
            .map_err(backend_error_from_sqlite_error)
    }
}

fn get_db_status(connection: &Connection, op: i32) -> Result<i32, i32> {
    let mut p_curr = 0;
    let mut p_hiwater = 0;
    let res = unsafe {
        rusqlite::ffi::sqlite3_db_status(connection.handle(), op, &mut p_curr, &mut p_hiwater, 0)
    };
    if res != 0 { Err(res) } else { Ok(p_curr) }
}

impl MallocSizeOf for SqliteEngine {
    fn size_of(&self, _ops: &mut MallocSizeOfOps) -> usize {
        // 48 KB (3.3.1 at https://sqlite.org/malloc.html)
        const DEFAULT_LOOKASIDE_SIZE: usize = 48 * 1024;
        DEFAULT_LOOKASIDE_SIZE +
            get_db_status(
                &self.connection,
                rusqlite::ffi::SQLITE_DBSTATUS_CACHE_USED_SHARED,
            )
            .unwrap_or_default() as usize +
            get_db_status(&self.connection, rusqlite::ffi::SQLITE_DBSTATUS_SCHEMA_USED)
                .unwrap_or_default() as usize +
            get_db_status(&self.connection, rusqlite::ffi::SQLITE_DBSTATUS_STMT_USED)
                .unwrap_or_default() as usize
    }
}

#[cfg(test)]
mod tests {
    use std::collections::VecDeque;
    use std::path::PathBuf;
    use std::sync::Arc;

    use profile_traits::generic_callback::GenericCallback;
    use profile_traits::time::ProfilerChan;
    use serde::{Deserialize, Serialize};
    use servo_base::generic_channel::{self, GenericReceiver, GenericSender};
    use servo_base::id::{PIPELINE_NAMESPACE, PipelineNamespace, PipelineNamespaceId, WebViewId};
    use servo_base::threadpool::ThreadPool;
    use servo_url::ImmutableOrigin;
    use storage_traits::client_storage::{
        ClientStorageThreadHandle, StorageIdentifier, StorageProxyMap, StorageType,
    };
    use storage_traits::indexeddb::{
        AsyncOperation, AsyncReadOnlyOperation, AsyncReadWriteOperation, CreateObjectResult,
        IndexedDBDescription, IndexedDBKeyRange, IndexedDBKeyType, IndexedDBTxnMode, KeyPath,
        KvsEngine, KvsOperation, KvsTransaction, PutItemResult, RecordsShape,
    };
    use url::Host;

    use crate::ClientStorageThreadFactory;
    use crate::indexeddb::engines::sqlite::encoding;
    use crate::indexeddb::engines::SqliteEngine;

    fn install_test_namespace() {
        PipelineNamespace::install(PipelineNamespaceId(1));
    }

    fn test_origin() -> ImmutableOrigin {
        ImmutableOrigin::Tuple(
            "test_origin".to_string(),
            Host::Domain("localhost".to_string()),
            80,
        )
    }

    fn get_pool() -> Arc<ThreadPool> {
        ThreadPool::global()
    }

    fn create_db(
        db_name: String,
    ) -> (
        tempfile::TempDir,
        PathBuf,
        bool,
        StorageProxyMap,
        ClientStorageThreadHandle,
    ) {
        if PIPELINE_NAMESPACE.get().is_none() {
            install_test_namespace();
        }
        let tmp_dir = tempfile::tempdir().unwrap();
        let handle: ClientStorageThreadHandle =
            ClientStorageThreadFactory::new(Some(tmp_dir.path().to_path_buf()), true, None);

        let storage_proxy_map = handle
            .obtain_a_storage_bottle_map(
                StorageType::Local,
                Some(WebViewId::new(servo_base::id::TEST_PAINTER_ID)),
                StorageIdentifier::IndexedDB,
                test_origin(),
            )
            .recv()
            .unwrap()
            .unwrap();
        let (path, created) = handle
            .create_database(storage_proxy_map.bottle_id, db_name)
            .recv()
            .unwrap()
            .unwrap();
        (tmp_dir, path, created, storage_proxy_map, handle)
    }

    #[test]
    fn test_cycle() {
        let (_temp_dir, path, created, proxy_map, handle) = create_db("test_db".to_string());
        let thread_pool = get_pool();
        // Test create
        let db = SqliteEngine::new(
            path.clone(),
            created,
            &IndexedDBDescription {
                name: "test_db".to_string(),
                origin: test_origin(),
            },
            thread_pool.clone(),
        )
        .unwrap();
        drop(db);

        // Test open
        let db = SqliteEngine::new(
            path,
            created,
            &IndexedDBDescription {
                name: "test_db".to_string(),
                origin: test_origin(),
            },
            thread_pool.clone(),
        )
        .unwrap();
        let version = db.version().expect("Failed to get version");
        assert_eq!(version, 0);
        db.set_version(5).unwrap();
        let new_version = db.version().expect("Failed to get new version");
        assert_eq!(new_version, 5);
        drop(db);
        handle
            .delete_database(proxy_map.bottle_id, "test_db".to_string())
            .recv()
            .unwrap()
            .expect("Failed to delete database");
    }

    #[test]
    fn test_create_store() {
        let (_temp_dir, path, created, _proxy_map, _handle) = create_db("test_db".to_string());
        let thread_pool = get_pool();
        let db = SqliteEngine::new(
            path,
            created,
            &IndexedDBDescription {
                name: "test_db".to_string(),
                origin: test_origin(),
            },
            thread_pool,
        )
        .unwrap();
        let store_name = "test_store";
        let result = db.create_store(store_name, None, true);
        assert!(result.is_ok());
        let create_result = result.unwrap();
        assert_eq!(create_result, CreateObjectResult::Created);
        // Try to create the same store again
        let result = db.create_store(store_name, None, false);
        assert!(result.is_ok());
        let create_result = result.unwrap();
        assert_eq!(create_result, CreateObjectResult::AlreadyExists);
        // Ensure store was not overwritten
        assert_eq!(db.key_generator_current_number(store_name), Ok(Some(1)));
    }

    #[test]
    fn test_create_store_empty_name() {
        let (_temp_dir, path, created, _proxy_map, _handle) = create_db("test_db".to_string());
        let thread_pool = get_pool();
        let db = SqliteEngine::new(
            path,
            created,
            &IndexedDBDescription {
                name: "test_db".to_string(),
                origin: test_origin(),
            },
            thread_pool,
        )
        .unwrap();
        let store_name = "";
        let result = db.create_store(store_name, None, true);
        assert!(result.is_ok());
        let create_result = result.unwrap();
        assert_eq!(create_result, CreateObjectResult::Created);
    }

    #[test]
    fn test_injection() {
        let (_temp_dir, path, created, _proxy_map, _handle) = create_db("test_db".to_string());
        let thread_pool = get_pool();
        let db = SqliteEngine::new(
            path,
            created,
            &IndexedDBDescription {
                name: "test_db".to_string(),
                origin: test_origin(),
            },
            thread_pool,
        )
        .unwrap();
        // Create a normal store
        let store_name1 = "test_store";
        let result = db.create_store(store_name1, None, true);
        assert!(result.is_ok());
        let create_result = result.unwrap();
        assert_eq!(create_result, CreateObjectResult::Created);
        // Injection
        let store_name2 = "' OR 1=1 -- -";
        let result = db.create_store(store_name2, None, false);
        assert!(result.is_ok());
        let create_result = result.unwrap();
        assert_eq!(create_result, CreateObjectResult::Created);
    }

    #[test]
    fn test_key_path() {
        let (_temp_dir, path, created, _proxy_map, _handle) = create_db("test_db".to_string());
        let thread_pool = get_pool();
        let db = SqliteEngine::new(
            path,
            created,
            &IndexedDBDescription {
                name: "test_db".to_string(),
                origin: test_origin(),
            },
            thread_pool,
        )
        .unwrap();
        let store_name = "test_store";
        let result = db.create_store(store_name, Some(KeyPath::String("test".to_string())), true);
        assert!(result.is_ok());
        assert_eq!(
            db.key_path(store_name),
            Ok(Some(KeyPath::String("test".to_string())))
        );
    }

    #[test]
    fn test_delete_store() {
        let (_temp_dir, path, created, _proxy_map, _handle) = create_db("test_db".to_string());
        let thread_pool = get_pool();
        let db = SqliteEngine::new(
            path,
            created,
            &IndexedDBDescription {
                name: "test_db".to_string(),
                origin: test_origin(),
            },
            thread_pool,
        )
        .unwrap();
        db.create_store("test_store", None, false)
            .expect("Failed to create store");
        // Delete the store
        db.delete_store("test_store")
            .expect("Failed to delete store");
        // Try to delete the same store again
        let result = db.delete_store("test_store");
        assert!(result.is_err());
        // Try to delete a non-existing store
        let result = db.delete_store("test_store");
        // Should work as per spec
        assert!(result.is_err());
    }

    #[test]
    fn test_delete_store_removes_store_records() {
        let (_temp_dir, path, created, _proxy_map, _handle) = create_db("test_db".to_string());
        let thread_pool = get_pool();
        let db = SqliteEngine::new(
            path,
            created,
            &IndexedDBDescription {
                name: "test_db".to_string(),
                origin: test_origin(),
            },
            thread_pool,
        )
        .unwrap();

        db.create_store("test_store", None, false)
            .expect("Failed to create store");
        let object_store = SqliteEngine::object_store_by_name(&db.connection, "test_store")
            .expect("Failed to fetch store metadata");
        SqliteEngine::put_item(
            &db.connection,
            object_store.clone(),
            IndexedDBKeyType::Number(1.0),
            vec![1, 2, 3],
            true,
            None,
            &[],
        )
        .expect("Failed to insert item");

        let row_count_before: i64 = db
            .connection
            .query_row(
                "SELECT COUNT(*) FROM object_data WHERE object_store_id = ?",
                rusqlite::params![object_store.id],
                |row| row.get(0),
            )
            .expect("Failed to count rows before delete");
        assert_eq!(row_count_before, 1);

        db.delete_store("test_store")
            .expect("Failed to delete store");

        let row_count_after: i64 = db
            .connection
            .query_row(
                "SELECT COUNT(*) FROM object_data WHERE object_store_id = ?",
                rusqlite::params![object_store.id],
                |row| row.get(0),
            )
            .expect("Failed to count rows after delete");
        assert_eq!(row_count_after, 0);
    }

    #[test]
    fn test_async_operations() {
        fn get_channel<T>() -> (GenericSender<T>, GenericReceiver<T>)
        where
            T: for<'de> Deserialize<'de> + Serialize,
        {
            generic_channel::channel().unwrap()
        }

        fn get_callback<T>(chan: GenericSender<T>) -> GenericCallback<T>
        where
            T: for<'de> Deserialize<'de> + Serialize + Send + Sync,
        {
            GenericCallback::new(ProfilerChan(None), move |r| {
                assert!(chan.send(r.unwrap()).is_ok());
            })
            .expect("Could not construct callback")
        }

        let (_temp_dir, path, created, _proxy_map, _handle) = create_db("test_db".to_string());
        let thread_pool = get_pool();
        let db = SqliteEngine::new(
            path,
            created,
            &IndexedDBDescription {
                name: "test_db".to_string(),
                origin: test_origin(),
            },
            thread_pool,
        )
        .unwrap();
        let store_name = "test_store";
        db.create_store(store_name, None, false)
            .expect("Failed to create store");
        let put = get_channel();
        let put2 = get_channel();
        let put3 = get_channel();
        let put_dup = get_channel();
        let put_overwrite = get_channel();
        let get_item_some = get_channel();
        let get_item_none = get_channel();
        let get_all_items = get_channel();
        let count = get_channel();
        let remove = get_channel();
        let clear = get_channel();
        let (done_tx, done_rx) = std::sync::mpsc::channel();
        db.process_transaction(
            KvsTransaction {
                mode: IndexedDBTxnMode::Readwrite,
                serial_number: 0,
                requests: VecDeque::from(vec![
                    KvsOperation {
                        store_name: store_name.to_owned(),
                        context: Default::default(),
                        operation: AsyncOperation::ReadWrite(AsyncReadWriteOperation::PutItem {
                            callback: get_callback(put.0),
                            key: Some(IndexedDBKeyType::Number(1.0)),
                            value: vec![1, 2, 3],
                            should_overwrite: false,
                            key_generator_current_number: None,
                        }),
                    },
                    KvsOperation {
                        store_name: store_name.to_owned(),
                        context: Default::default(),
                        operation: AsyncOperation::ReadWrite(AsyncReadWriteOperation::PutItem {
                            callback: get_callback(put2.0),
                            key: Some(IndexedDBKeyType::String("2.0".to_string())),
                            value: vec![4, 5, 6],
                            should_overwrite: false,
                            key_generator_current_number: None,
                        }),
                    },
                    KvsOperation {
                        store_name: store_name.to_owned(),
                        context: Default::default(),
                        operation: AsyncOperation::ReadWrite(AsyncReadWriteOperation::PutItem {
                            callback: get_callback(put3.0),
                            key: Some(IndexedDBKeyType::Array(vec![
                                IndexedDBKeyType::String("3".to_string()),
                                IndexedDBKeyType::Number(0.0),
                            ])),
                            value: vec![7, 8, 9],
                            should_overwrite: false,
                            key_generator_current_number: None,
                        }),
                    },
                    // Try to put a duplicate key without overwrite
                    KvsOperation {
                        store_name: store_name.to_owned(),
                        context: Default::default(),
                        operation: AsyncOperation::ReadWrite(AsyncReadWriteOperation::PutItem {
                            callback: get_callback(put_dup.0),
                            key: Some(IndexedDBKeyType::Number(1.0)),
                            value: vec![10, 11, 12],
                            should_overwrite: false,
                            key_generator_current_number: None,
                        }),
                    },
                    KvsOperation {
                        store_name: store_name.to_owned(),
                        context: Default::default(),
                        operation: AsyncOperation::ReadWrite(AsyncReadWriteOperation::PutItem {
                            callback: get_callback(put_overwrite.0),
                            key: Some(IndexedDBKeyType::Number(1.0)),
                            value: vec![13, 14, 15],
                            should_overwrite: true,
                            key_generator_current_number: None,
                        }),
                    },
                    KvsOperation {
                        store_name: store_name.to_owned(),
                        context: Default::default(),
                        operation: AsyncOperation::ReadOnly(AsyncReadOnlyOperation::GetItem {
                            callback: get_callback(get_item_some.0),
                            key_range: IndexedDBKeyRange::only(IndexedDBKeyType::Number(1.0)),
                        }),
                    },
                    KvsOperation {
                        store_name: store_name.to_owned(),
                        context: Default::default(),
                        operation: AsyncOperation::ReadOnly(AsyncReadOnlyOperation::GetItem {
                            callback: get_callback(get_item_none.0),
                            key_range: IndexedDBKeyRange::only(IndexedDBKeyType::Number(5.0)),
                        }),
                    },
                    KvsOperation {
                        store_name: store_name.to_owned(),
                        context: Default::default(),
                        operation: AsyncOperation::ReadOnly(AsyncReadOnlyOperation::Iterate {
                            callback: get_callback(get_all_items.0),
                            key_range: IndexedDBKeyRange::lower_bound(
                                IndexedDBKeyType::Number(0.0),
                                false,
                            ),
                            count: None,
                            shape: RecordsShape::WithValues,
                        }),
                    },
                    KvsOperation {
                        store_name: store_name.to_owned(),
                        context: Default::default(),
                        operation: AsyncOperation::ReadOnly(AsyncReadOnlyOperation::Count {
                            callback: get_callback(count.0),
                            key_range: IndexedDBKeyRange::only(IndexedDBKeyType::Number(1.0)),
                        }),
                    },
                    KvsOperation {
                        store_name: store_name.to_owned(),
                        context: Default::default(),
                        operation: AsyncOperation::ReadWrite(AsyncReadWriteOperation::RemoveItem {
                            callback: get_callback(remove.0),
                            key_range: IndexedDBKeyRange::only(IndexedDBKeyType::Number(1.0)),
                        }),
                    },
                    KvsOperation {
                        store_name: store_name.to_owned(),
                        context: Default::default(),
                        operation: AsyncOperation::ReadWrite(AsyncReadWriteOperation::Clear(
                            get_callback(clear.0),
                        )),
                    },
                ]),
            },
            Box::new(move || {
                let _ = done_tx.send(());
            }),
        );
        let _ = done_rx.recv().unwrap();
        put.1.recv().unwrap().unwrap();
        put2.1.recv().unwrap().unwrap();
        put3.1.recv().unwrap().unwrap();
        let err = put_dup.1.recv().unwrap().unwrap();
        assert_eq!(err, PutItemResult::CannotOverwrite);
        let overwritten = put_overwrite.1.recv().unwrap().unwrap();
        assert_eq!(
            overwritten,
            PutItemResult::Key(IndexedDBKeyType::Number(1.0))
        );
        let get_result = get_item_some.1.recv().unwrap();
        let value = get_result.unwrap();
        assert_eq!(value, Some(vec![13, 14, 15]));
        let get_result = get_item_none.1.recv().unwrap();
        let value = get_result.unwrap();
        assert_eq!(value, None);
        let all_items: Vec<Vec<u8>> = get_all_items
            .1
            .recv()
            .unwrap()
            .unwrap()
            .into_iter()
            .map(|record| record.value)
            .collect();
        assert_eq!(all_items.len(), 3);
        // Check that all three items are present
        assert!(all_items.contains(&vec![13, 14, 15]));
        assert!(all_items.contains(&vec![4, 5, 6]));
        assert!(all_items.contains(&vec![7, 8, 9]));
        let amount = count.1.recv().unwrap().unwrap();
        assert_eq!(amount, 1);
        remove.1.recv().unwrap().unwrap();
        clear.1.recv().unwrap().unwrap();
    }

    #[test]
    fn test_delete_item_range_respects_open_bounds() {
        fn remaining_keys_after_delete(
            lower: i32,
            upper: i32,
            lower_open: bool,
            upper_open: bool,
        ) -> Vec<i32> {
            let (_temp_dir, path, created, _proxy_map, _handle) = create_db("test_db".to_string());
            let thread_pool = get_pool();
            let db = SqliteEngine::new(
                path,
                created,
                &IndexedDBDescription {
                    name: "test_db".to_string(),
                    origin: test_origin(),
                },
                thread_pool,
            )
            .unwrap();
            let store_name = "test_store";
            db.create_store(store_name, None, false)
                .expect("Failed to create store");
            let store = SqliteEngine::object_store_by_name(&db.connection, store_name)
                .expect("Failed to get object store");

            for key in 1..=10 {
                SqliteEngine::put_item(
                    &db.connection,
                    store.clone(),
                    IndexedDBKeyType::Number(key as f64),
                    vec![key as u8],
                    false,
                    None,
                    &[],
                )
                .expect("Failed to seed object store");
            }

            SqliteEngine::delete_item(
                &db.connection,
                store.clone(),
                IndexedDBKeyRange::new(
                    Some(IndexedDBKeyType::Number(lower as f64)),
                    Some(IndexedDBKeyType::Number(upper as f64)),
                    lower_open,
                    upper_open,
                ),
            )
            .expect("Failed to delete key range");

            SqliteEngine::object_store_records(
                &db.connection,
                store,
                IndexedDBKeyRange::default(),
                None,
                RecordsShape::KeysOnly,
            )
            .expect("Failed to read remaining keys")
            .into_iter()
            .map(|record| match encoding::deserialize(&record.key).unwrap() {
                IndexedDBKeyType::Number(number) => number as i32,
                other => panic!("Expected numeric key, got {other:?}"),
            })
            .collect()
        }

        assert_eq!(
            remaining_keys_after_delete(3, 8, false, false),
            vec![1, 2, 9, 10]
        );
        assert_eq!(
            remaining_keys_after_delete(3, 8, true, false),
            vec![1, 2, 3, 9, 10]
        );
        assert_eq!(
            remaining_keys_after_delete(3, 8, false, true),
            vec![1, 2, 8, 9, 10]
        );
        assert_eq!(
            remaining_keys_after_delete(3, 8, true, true),
            vec![1, 2, 3, 8, 9, 10]
        );
    }

    /// <https://w3c.github.io/IndexedDB/#abort-a-transaction>
    ///
    /// > When a transaction is aborted the implementation must undo (roll back) any changes
    /// > that were made to the database during that transaction.
    ///
    /// One transaction lays down two records and commits. A second one overwrites one of them,
    /// adds a record of its own and removes the other, then aborts. All three of those are
    /// taken back, which is the difference between `count()` reading 1 and reading 0 in
    /// `idb-explicit-commit`.
    #[test]
    fn test_rollback_transaction_undoes_a_readwrite_transaction() {
        fn ignored_callback<T>() -> GenericCallback<T>
        where
            T: for<'de> Deserialize<'de> + Serialize + Send + Sync,
        {
            GenericCallback::new(ProfilerChan(None), |_| {}).expect("Could not construct callback")
        }

        fn run(db: &SqliteEngine, serial_number: u64, requests: Vec<KvsOperation>) {
            let (done_tx, done_rx) = std::sync::mpsc::channel();
            db.process_transaction(
                KvsTransaction {
                    mode: IndexedDBTxnMode::Readwrite,
                    serial_number,
                    requests: VecDeque::from(requests),
                },
                Box::new(move || {
                    let _ = done_tx.send(());
                }),
            );
            done_rx.recv().unwrap();
        }

        fn put(store_name: &str, key: f64, value: Vec<u8>) -> KvsOperation {
            KvsOperation {
                store_name: store_name.to_owned(),
                context: Default::default(),
                operation: AsyncOperation::ReadWrite(AsyncReadWriteOperation::PutItem {
                    callback: ignored_callback(),
                    key: Some(IndexedDBKeyType::Number(key)),
                    value,
                    should_overwrite: true,
                    key_generator_current_number: None,
                }),
            }
        }

        let (_temp_dir, path, created, _proxy_map, _handle) = create_db("test_db".to_string());
        let db = SqliteEngine::new(
            path,
            created,
            &IndexedDBDescription {
                name: "test_db".to_string(),
                origin: test_origin(),
            },
            get_pool(),
        )
        .unwrap();
        let store_name = "test_store";
        db.create_store(store_name, None, false)
            .expect("Failed to create store");

        run(
            &db,
            1,
            vec![
                put(store_name, 1.0, vec![1, 2, 3]),
                put(store_name, 3.0, vec![7, 8, 9]),
            ],
        );
        db.commit_transaction(1).expect("Failed to commit");

        run(
            &db,
            2,
            vec![
                put(store_name, 1.0, vec![9, 9, 9]),
                put(store_name, 2.0, vec![4, 5, 6]),
                KvsOperation {
                    store_name: store_name.to_owned(),
                    context: Default::default(),
                    operation: AsyncOperation::ReadWrite(AsyncReadWriteOperation::RemoveItem {
                        callback: ignored_callback(),
                        key_range: IndexedDBKeyRange::only(IndexedDBKeyType::Number(3.0)),
                    }),
                },
            ],
        );
        db.rollback_transaction(2).expect("Failed to roll back");

        let read = |key: f64| {
            let (tx, rx) = generic_channel::channel().unwrap();
            let callback = GenericCallback::new(ProfilerChan(None), move |result| {
                assert!(tx.send(result.unwrap()).is_ok());
            })
            .expect("Could not construct callback");
            run(
                &db,
                3,
                vec![KvsOperation {
                    store_name: store_name.to_owned(),
                    context: Default::default(),
                    operation: AsyncOperation::ReadOnly(AsyncReadOnlyOperation::GetItem {
                        callback,
                        key_range: IndexedDBKeyRange::only(IndexedDBKeyType::Number(key)),
                    }),
                }],
            );
            rx.recv().unwrap().unwrap()
        };

        assert_eq!(read(1.0), Some(vec![1, 2, 3]), "an overwrite was undone");
        assert_eq!(read(2.0), None, "an added record was undone");
        assert_eq!(read(3.0), Some(vec![7, 8, 9]), "a removal was undone");
    }
}
