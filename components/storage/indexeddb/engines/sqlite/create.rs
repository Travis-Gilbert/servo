/* This Source Code Form is subject to the terms of the Mozilla Public
 * License, v. 2.0. If a copy of the MPL was not distributed with this
 * file, You can obtain one at https://mozilla.org/MPL/2.0/. */

// Adapted from:
// https://github.com/mozilla-firefox/firefox/blob/ee102e926521b3e460293b0aea6b54b1a03f6f74/dom/indexedDB/DBSchema.cpp#L78

/// The name the `object_store_index` table has in a database that is done migrating.
pub(crate) const OBJECT_STORE_INDEX: &str = "object_store_index";

/// The columns copied when the table is rebuilt, in the order this schema declares them.
pub(crate) const OBJECT_STORE_INDEX_COLUMNS: &str =
    "id, object_store_id, name, key_path, unique_index, multi_entry_index";

/// The `object_store_index` table, under whichever name the caller needs.
///
/// A rebuild has to create the replacement under a temporary name before it can take the real
/// one, so the shape is written once here rather than once per caller.
pub(crate) fn object_store_index_table(table: &str) -> String {
    format!(
        r#"
create table {table} (
    id                integer        not null
    primary key autoincrement,
    object_store_id   integer        not null
    references object_store,
    name              varchar        not null,
    key_path          varbinary_blob not null,
    unique_index      boolean        not null,
    multi_entry_index boolean        not null,
    -- An index name is scoped to its object store, so two stores in one database may each have
    -- an index of the same name. The Firefox schema this file is adapted from spells that as a
    -- table-level `UNIQUE (object_store_id, name)`; writing `unique` on the column alone makes
    -- the name unique across the whole database, so the second store's index is refused with a
    -- constraint violation the specification has no error for.
    unique (object_store_id, name)
);"#
    )
}

pub(crate) fn create_tables(conn: &rusqlite::Connection) -> Result<(), rusqlite::Error> {
    const DATABASE: &str = r#"
create table database (
    name    varchar          not null
        primary key,
    origin  varchar          not null,
    version bigint default 0 not null
) WITHOUT ROWID;"#;
    conn.execute(DATABASE, [])?;

    const OBJECT_STORE: &str = r#"
create table object_store (
    id             integer               not null
        primary key autoincrement,
    name           varchar               not null
        unique,
    key_path       varbinary_blob,
    auto_increment integer default FALSE not null
);"#;
    conn.execute(OBJECT_STORE, [])?;

    const OBJECT_DATA: &str = r#"
create table object_data (
    object_store_id integer not null
        references object_store,
    key             blob    not null,
    data            blob    not null,
    constraint "pk-object_data"
        primary key (object_store_id, key)
) WITHOUT ROWID;"#;
    conn.execute(OBJECT_DATA, [])?;

    conn.execute(&object_store_index_table(OBJECT_STORE_INDEX), [])?;

    const INDEX_DATA: &str = r#"
CREATE TABLE index_data (
    index_id INTEGER NOT NULL,
    value BLOB NOT NULL,
    object_data_key BLOB NOT NULL,
    object_store_id INTEGER NOT NULL,
    value_locale BLOB,
    PRIMARY KEY (index_id, value, object_data_key)
    FOREIGN KEY (index_id) REFERENCES object_store_index(id),
    FOREIGN KEY (object_store_id, object_data_key)
    REFERENCES object_data(object_store_id, key)
) WITHOUT ROWID;"#;
    conn.execute(INDEX_DATA, [])?;

    const UNIQUE_INDEX_DATA: &str = r#"
CREATE TABLE unique_index_data (
    index_id INTEGER NOT NULL,
    value BLOB NOT NULL,
    object_store_id INTEGER NOT NULL,
    object_data_key BLOB NOT NULL,
    value_locale BLOB,
    PRIMARY KEY (index_id, value),
    FOREIGN KEY (index_id) REFERENCES object_store_index(id),
    FOREIGN KEY (object_store_id, object_data_key)
    REFERENCES object_data(object_store_id, key)
) WITHOUT ROWID;"#;
    conn.execute(UNIQUE_INDEX_DATA, [])?;
    Ok(())
}
