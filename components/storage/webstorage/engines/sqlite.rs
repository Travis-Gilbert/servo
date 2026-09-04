/* This Source Code Form is subject to the terms of the Mozilla Public
 * License, v. 2.0. If a copy of the MPL was not distributed with this
 * file, You can obtain one at https://mozilla.org/MPL/2.0/. */

use std::path::PathBuf;
use std::sync::Arc;

use rusqlite::{Connection, OptionalExtension};
use servo_base::threadpool::ThreadPool;

use crate::shared::{DB_IN_MEMORY_INIT_PRAGMAS, DB_IN_MEMORY_PRAGMAS, DB_INIT_PRAGMAS, DB_PRAGMAS};
use storage_traits::webstorage_thread::WebStorageEngine;

pub struct SqliteEngine {
    connection: Connection,
}

impl SqliteEngine {
    pub fn new(db_dir: &Option<PathBuf>, _pool: Arc<ThreadPool>) -> rusqlite::Result<Self> {
        let connection = match db_dir {
            Some(path) => {
                let path = path.join("webstorage.sqlite");
                Self::init_db(Some(&path))?
            },
            None => Self::init_db(None)?,
        };
        Ok(SqliteEngine { connection })
    }

    pub fn init_db(db_path: Option<&PathBuf>) -> rusqlite::Result<Connection> {
        let connection = if let Some(path) = db_path {
            if let Some(parent) = path.parent() {
                let _ = std::fs::create_dir_all(parent);
            }
            let conn = Connection::open(path)?;
            for pragma in DB_INIT_PRAGMAS.iter() {
                let _ = conn.execute(pragma, []);
            }
            for pragma in DB_PRAGMAS.iter() {
                let _ = conn.execute(pragma, []);
            }
            conn
        } else {
            // TODO We probably don't need an in memory implementation at all.
            // WebStorageEnvironment already keeps all key value pairs in memory via its data field.
            // A future refactoring could avoid creating a WebStorageEngine entirely when config_dir is None.
            let conn = Connection::open_in_memory()?;
            for pragma in DB_IN_MEMORY_INIT_PRAGMAS.iter() {
                let _ = conn.execute(pragma, []);
            }
            for pragma in DB_IN_MEMORY_PRAGMAS.iter() {
                let _ = conn.execute(pragma, []);
            }
            conn
        };
        connection.execute("CREATE TABLE IF NOT EXISTS data (id INTEGER PRIMARY KEY AUTOINCREMENT, key TEXT, value TEXT);", [])?;
        Ok(connection)
    }
}

impl WebStorageEngine for SqliteEngine {
    fn len(&self) -> Result<usize, String> {
        self.connection
            .query_row("SELECT COUNT(*) FROM data", [], |row| row.get::<_, i64>(0))
            .map(|length| length as usize)
            .map_err(|error| error.to_string())
    }

    fn key(&self, index: usize) -> Result<Option<String>, String> {
        self.connection
            .query_row(
                "SELECT key FROM data ORDER BY key LIMIT 1 OFFSET ?",
                [index as i64],
                |row| row.get(0),
            )
            .optional()
            .map_err(|error| error.to_string())
    }

    fn keys(&self) -> Result<Vec<String>, String> {
        let load = || -> rusqlite::Result<Vec<String>> {
            let mut statement = self
                .connection
                .prepare("SELECT key FROM data ORDER BY key")?;
            statement
                .query_map([], |row| row.get(0))?
                .collect::<Result<Vec<_>, _>>()
        };
        load().map_err(|error| error.to_string())
    }

    fn get(&self, key: &str) -> Result<Option<String>, String> {
        self.connection
            .query_row("SELECT value FROM data WHERE key = ?", [key], |row| {
                row.get(0)
            })
            .optional()
            .map_err(|error| error.to_string())
    }

    fn set(&mut self, key: &str, value: &str) -> Result<Option<String>, String> {
        let old_value = self.get(key)?;
        let mut set = || -> rusqlite::Result<()> {
            // TODO: Replace this with an UPSERT once the schema guarantees a
            // UNIQUE/PRIMARY KEY constraint on `key`.
            let tx = self.connection.transaction()?;
            let rows = tx.execute("UPDATE data SET value = ? WHERE key = ?", [value, key])?;
            if rows == 0 {
                tx.execute("INSERT INTO data (key, value) VALUES (?, ?)", [key, value])?;
            }
            tx.commit()?;
            Ok(())
        };
        set().map(|()| old_value).map_err(|error| error.to_string())
    }

    fn delete(&mut self, key: &str) -> Result<Option<String>, String> {
        let old_value = self.get(key)?;
        self.connection
            .execute("DELETE FROM data WHERE key = ?", [key])
            .map(|_| old_value)
            .map_err(|error| error.to_string())
    }

    fn clear(&mut self) -> Result<bool, String> {
        let changed = self.len()? != 0;
        self.connection
            .execute("DELETE FROM data", [])
            .map(|_| changed)
            .map_err(|error| error.to_string())
    }

    fn size(&self) -> Result<usize, String> {
        self.connection
            .query_row(
                "SELECT COALESCE(SUM(LENGTH(key) + LENGTH(value)), 0) FROM data",
                [],
                |row| row.get::<_, i64>(0),
            )
            .map(|size| size as usize)
            .map_err(|error| error.to_string())
    }
}
