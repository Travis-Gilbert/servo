/* This Source Code Form is subject to the terms of the Mozilla Public
 * License, v. 2.0. If a copy of the MPL was not distributed with this
 * file, You can obtain one at https://mozilla.org/MPL/2.0/. */
/*
 * The origin of this IDL file is
 * https://w3c.github.io/IndexedDB/#keyrange
 *
 */

// https://w3c.github.io/IndexedDB/#keyrange
[Pref="dom_indexeddb_enabled", Exposed=(Window,Worker)]
interface IDBKeyRange {
  // Converting a key to a value allocates, so both getters can report an allocation
  // failure instead of taking the content process down with them.
  [Throws] readonly attribute any lower;
  [Throws] readonly attribute any upper;
  readonly attribute boolean lowerOpen;
  readonly attribute boolean upperOpen;

  // Static construction methods:
  [Throws, NewObject] static IDBKeyRange only(any value);
  [Throws, NewObject] static IDBKeyRange lowerBound(any lower, optional boolean open = false);
  [Throws, NewObject] static IDBKeyRange upperBound(any upper, optional boolean open = false);
  [Throws, NewObject] static IDBKeyRange bound(any lower,
                                       any upper,
                                       optional boolean lowerOpen = false,
                                       optional boolean upperOpen = false);

  [Throws] boolean _includes(any key);
};
