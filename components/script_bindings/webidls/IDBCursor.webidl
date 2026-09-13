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
  // Converting a key to a value allocates, so both getters can report an allocation
  // failure instead of taking the content process down with them.
  [Throws] readonly attribute any key;
  [Throws] readonly attribute any primaryKey;
  // The request is stored on the cursor by the open cursor steps rather than at
  // construction, so the getter reports a cursor that has none instead of taking the
  // content process down with it.
  [Throws, SameObject] readonly attribute IDBRequest request;

  [Throws] undefined advance([EnforceRange] unsigned long count);
  [Throws] undefined continue(optional any key);
  [Throws] undefined continuePrimaryKey(any key, any primaryKey);

  // The cursor's write path. Both run against the cursor's effective key through
  // IDBObjectStore, which owns the key path, clone and index extraction they need.
  [NewObject, Throws] IDBRequest update(any value);
  [NewObject, Throws] IDBRequest delete();
};

enum IDBCursorDirection {
  "next",
  "nextunique",
  "prev",
  "prevunique"
};
