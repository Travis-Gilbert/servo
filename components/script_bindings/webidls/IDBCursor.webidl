/* This Source Code Form is subject to the terms of the Mozilla Public
 * License, v. 2.0. If a copy of the MPL was not distributed with this
 * file, You can obtain one at https://mozilla.org/MPL/2.0/. */
/*
 * The origin of this IDL file is
 * https://w3c.github.io/IndexedDB/#idbcursor
 *
 */

// https://w3c.github.io/IndexedDB/#idbcursor
[Pref="dom_indexeddb_enabled", Exposed=(Window,Worker)]
interface IDBCursor {
  readonly attribute (IDBObjectStore or IDBIndex) source;
  readonly attribute IDBCursorDirection direction;
  readonly attribute any key;
  readonly attribute any primaryKey;
  [SameObject] readonly attribute IDBRequest request;

  [Throws] undefined advance([EnforceRange] unsigned long count);
  [Throws] undefined continue(optional any key);
  [Throws] undefined continuePrimaryKey(any key, any primaryKey);

  // update and delete are the cursor's write path. They run "store a record into an object
  // store" and "delete records from an object store" against the cursor's effective key, which
  // needs IDBObjectStore's key path, clone and extraction helpers; those are private to that
  // type today. The three iteration methods above share iterate_cursor and need none of it.
  // [NewObject, Throws] IDBRequest update(any value);
  // [NewObject, Throws] IDBRequest delete();
};

enum IDBCursorDirection {
  "next",
  "nextunique",
  "prev",
  "prevunique"
};
