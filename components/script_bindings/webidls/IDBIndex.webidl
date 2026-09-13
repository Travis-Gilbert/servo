/* This Source Code Form is subject to the terms of the Mozilla Public
 * License, v. 2.0. If a copy of the MPL was not distributed with this
 * file, You can obtain one at https://mozilla.org/MPL/2.0/. */
/*
 * The origin of this IDL file is
 * https://w3c.github.io/IndexedDB/#idbindex
 *
 */

[Pref="dom_indexeddb_enabled", Exposed=(Window,Worker)]
interface IDBIndex {
  [SetterThrows] attribute DOMString name;
  [SameObject] readonly attribute IDBObjectStore objectStore;
  readonly attribute any keyPath;
  readonly attribute boolean multiEntry;
  readonly attribute boolean unique;

  [NewObject, Throws] IDBRequest get(any query);
  [NewObject, Throws] IDBRequest getKey(any query);
  [NewObject, Throws] IDBRequest getAll(optional any query,
                                optional [EnforceRange] unsigned long count);
  [NewObject, Throws] IDBRequest getAllKeys(optional any query,
                                    optional [EnforceRange] unsigned long count);
  [NewObject, Throws] IDBRequest count(optional any query);

  [NewObject, Throws] IDBRequest openCursor(optional any query,
                                    optional IDBCursorDirection direction = "next");
  [NewObject, Throws] IDBRequest openKeyCursor(optional any query,
                                       optional IDBCursorDirection direction = "next");

  // getAllRecords needs the IDBGetAllOptions dictionary and the IDBRecord interface, neither of
  // which exists in this tree yet, and IDBObjectStore does not declare it either. The newer
  // queryOrOptions overload of getAll and getAllKeys arrives with the same spec revision; both
  // signatures above match IDBObjectStore's live ones so the two interfaces stay consistent.
  // [NewObject, Throws] IDBRequest getAllRecords(optional IDBGetAllOptions options = {});
};
