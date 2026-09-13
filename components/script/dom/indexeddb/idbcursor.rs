/* This Source Code Form is subject to the terms of the Mozilla Public
 * License, v. 2.0. If a copy of the MPL was not distributed with this
 * file, You can obtain one at https://mozilla.org/MPL/2.0/. */

use std::cell::Cell;

use dom_struct::dom_struct;
use js::context::JSContext;
use js::jsapi::Heap;
use js::jsval::{JSVal, UndefinedValue};
use js::rust::{HandleValue, MutableHandleValue};
use script_bindings::cell::DomRefCell;
use script_bindings::reflector::{Reflector, reflect_dom_object_with_cx};
use storage_traits::indexeddb::{IndexedDBKeyRange, IndexedDBKeyType, IndexedDBRecord};

use storage_traits::indexeddb::{
    AsyncOperation, AsyncReadOnlyOperation, KvsOperationContext, KvsOperationTarget, RecordsShape,
};

use crate::dom::bindings::codegen::Bindings::IDBCursorBinding::{
    IDBCursorDirection, IDBCursorMethods,
};
use crate::dom::bindings::codegen::Bindings::IDBIndexBinding::IDBIndexMethods;
use crate::dom::bindings::codegen::Bindings::IDBTransactionBinding::{
    IDBTransactionMethods, IDBTransactionMode,
};
use crate::dom::bindings::codegen::UnionTypes::IDBObjectStoreOrIDBIndex;
use crate::dom::bindings::error::{Error, Fallible};
use crate::dom::bindings::refcounted::Trusted;
use crate::dom::bindings::root::{Dom, DomRoot, MutNullableDom};
use crate::dom::bindings::structuredclone;
use crate::dom::globalscope::GlobalScope;
use crate::dom::indexeddb::idbindex::IDBIndex;
use crate::dom::indexeddb::idbobjectstore::IDBObjectStore;
use crate::dom::indexeddb::idbrequest::{IDBRequest, RecordsParam, RequestSource};
use crate::dom::indexeddb::idbtransaction::IDBTransaction;
use crate::indexeddb::{convert_value_to_key, key_type_to_jsval};

#[derive(JSTraceable, MallocSizeOf)]
#[cfg_attr(crown, crown::unrooted_must_root_lint::must_root)]
pub(crate) enum ObjectStoreOrIndex {
    ObjectStore(Dom<IDBObjectStore>),
    Index(Dom<IDBIndex>),
}

#[dom_struct]
pub(crate) struct IDBCursor {
    reflector_: Reflector,

    /// <https://www.w3.org/TR/IndexedDB-3/#cursor-transaction>
    transaction: Dom<IDBTransaction>,
    /// <https://www.w3.org/TR/IndexedDB-3/#cursor-range>
    #[no_trace]
    range: IndexedDBKeyRange,
    /// <https://www.w3.org/TR/IndexedDB-3/#cursor-source>
    source: ObjectStoreOrIndex,
    /// <https://www.w3.org/TR/IndexedDB-3/#cursor-direction>
    direction: IDBCursorDirection,
    /// <https://www.w3.org/TR/IndexedDB-3/#cursor-position>
    #[no_trace]
    position: DomRefCell<Option<IndexedDBKeyType>>,
    /// <https://www.w3.org/TR/IndexedDB-3/#cursor-key>
    #[no_trace]
    key: DomRefCell<Option<IndexedDBKeyType>>,
    #[ignore_malloc_size_of = "mozjs"]
    cached_key: DomRefCell<Option<Heap<JSVal>>>,
    #[ignore_malloc_size_of = "mozjs"]
    cached_primary_key: DomRefCell<Option<Heap<JSVal>>>,
    /// <https://www.w3.org/TR/IndexedDB-3/#cursor-value>
    #[ignore_malloc_size_of = "mozjs"]
    value: Heap<JSVal>,
    /// <https://www.w3.org/TR/IndexedDB-3/#cursor-got-value-flag>
    got_value: Cell<bool>,
    /// <https://www.w3.org/TR/IndexedDB-3/#cursor-object-store-position>
    #[no_trace]
    object_store_position: DomRefCell<Option<IndexedDBKeyType>>,
    /// <https://www.w3.org/TR/IndexedDB-3/#cursor-key-only-flag>
    key_only: bool,

    /// <https://w3c.github.io/IndexedDB/#cursor-request>
    request: MutNullableDom<IDBRequest>,
}

impl IDBCursor {
    #[cfg_attr(crown, expect(crown::unrooted_must_root))]
    pub(crate) fn new_inherited(
        transaction: &IDBTransaction,
        direction: IDBCursorDirection,
        got_value: bool,
        source: ObjectStoreOrIndex,
        range: IndexedDBKeyRange,
        key_only: bool,
    ) -> IDBCursor {
        IDBCursor {
            reflector_: Reflector::new(),
            transaction: Dom::from_ref(transaction),
            range,
            source,
            direction,
            position: DomRefCell::new(None),
            key: DomRefCell::new(None),
            cached_key: DomRefCell::new(None),
            cached_primary_key: DomRefCell::new(None),
            value: Heap::default(),
            got_value: Cell::new(got_value),
            object_store_position: DomRefCell::new(None),
            key_only,
            request: Default::default(),
        }
    }

    #[cfg_attr(crown, expect(crown::unrooted_must_root))]
    #[allow(clippy::too_many_arguments)]
    pub(crate) fn new(
        cx: &mut JSContext,
        global: &GlobalScope,
        transaction: &IDBTransaction,
        direction: IDBCursorDirection,
        got_value: bool,
        source: ObjectStoreOrIndex,
        range: IndexedDBKeyRange,
        key_only: bool,
    ) -> DomRoot<IDBCursor> {
        reflect_dom_object_with_cx(
            Box::new(IDBCursor::new_inherited(
                transaction,
                direction,
                got_value,
                source,
                range,
                key_only,
            )),
            global,
            cx,
        )
    }

    fn set_position(&self, position: Option<IndexedDBKeyType>) {
        let changed = *self.position.borrow() != position;
        *self.position.borrow_mut() = position;
        if changed {
            *self.cached_primary_key.borrow_mut() = None;
        }
    }

    fn set_key(&self, key: Option<IndexedDBKeyType>) {
        let key_changed = {
            let current_key = self.key.borrow();
            current_key.as_ref() != key.as_ref()
        };
        *self.key.borrow_mut() = key;
        if key_changed {
            *self.cached_key.borrow_mut() = None;
        }
    }

    fn set_object_store_position(&self, object_store_position: Option<IndexedDBKeyType>) {
        let changed = *self.object_store_position.borrow() != object_store_position;
        *self.object_store_position.borrow_mut() = object_store_position;
        if changed {
            *self.cached_primary_key.borrow_mut() = None;
        }
    }

    pub(crate) fn set_request(&self, request: &IDBRequest) {
        self.request.set(Some(request));
    }

    pub(crate) fn value(&self, mut out: MutableHandleValue) {
        out.set(self.value.get());
    }

    /// <https://www.w3.org/TR/IndexedDB-3/#cursor-effective-key>
    pub(crate) fn effective_key(&self) -> Option<IndexedDBKeyType> {
        match &self.source {
            ObjectStoreOrIndex::ObjectStore(_) => self.position.borrow().clone(),
            ObjectStoreOrIndex::Index(_) => self.object_store_position.borrow().clone(),
        }
    }

    /// <https://www.w3.org/TR/IndexedDB-3/#cursor-effective-object-store>
    fn effective_object_store(&self) -> DomRoot<IDBObjectStore> {
        match &self.source {
            ObjectStoreOrIndex::ObjectStore(store) => store.as_rooted(),
            ObjectStoreOrIndex::Index(index) => index.ObjectStore(),
        }
    }

    /// The backend must range over the same records the cursor was opened on, so an index
    /// cursor keeps naming its index on every subsequent iteration, not only on the first.
    fn operation_context(&self) -> KvsOperationContext {
        match &self.source {
            ObjectStoreOrIndex::ObjectStore(_) => KvsOperationContext::default(),
            ObjectStoreOrIndex::Index(index) => KvsOperationContext {
                target: KvsOperationTarget::Index {
                    name: index.Name().to_string(),
                },
                index_updates: Vec::new(),
            },
        }
    }

    /// The preconditions `advance`, `continue` and `continuePrimaryKey` share before any
    /// argument of their own is examined.
    ///
    /// Kept separate from the got value check because `continuePrimaryKey` interposes two
    /// "InvalidAccessError" checks of its own between them, and the order the exceptions are
    /// thrown in is observable.
    fn check_transaction_and_source(&self) -> Fallible<()> {
        // If this's transaction's state is not active, throw a "TransactionInactiveError"
        // DOMException.
        if !self.transaction.is_active() || !self.transaction.is_usable() {
            return Err(Error::TransactionInactive(None));
        }

        // If this's source or effective object store has been deleted, throw an
        // "InvalidStateError" DOMException.
        let store = self.effective_object_store();
        if !self.transaction.Db().object_store_exists(&store.get_name()) {
            return Err(Error::InvalidState(Some(
                "The cursor's effective object store has been deleted".to_owned(),
            )));
        }
        if let ObjectStoreOrIndex::Index(index) = &self.source {
            if !store.has_index(&index.Name()) {
                return Err(Error::InvalidState(Some(
                    "The cursor's source index has been deleted".to_owned(),
                )));
            }
        }
        Ok(())
    }

    /// The preconditions `update` and `delete` share: steps 2 through 6 of both algorithms,
    /// in the order their exceptions are observable in.
    ///
    /// Kept separate from `check_transaction_and_source` because the read-only check falls
    /// between the inactive check and the deleted check, and the three iteration methods have
    /// no read-only check at all.
    fn check_writable(&self) -> Fallible<()> {
        // Step 2. If transaction's state is not active, throw a "TransactionInactiveError"
        // DOMException.
        if !self.transaction.is_active() || !self.transaction.is_usable() {
            return Err(Error::TransactionInactive(None));
        }

        // Step 3. If transaction is a read-only transaction, throw a "ReadOnlyError"
        // DOMException.
        if let IDBTransactionMode::Readonly = self.transaction.get_mode() {
            return Err(Error::ReadOnly(None));
        }

        // Step 4. If this's source or effective object store has been deleted, throw an
        // "InvalidStateError" DOMException.
        let store = self.effective_object_store();
        if !self.transaction.Db().object_store_exists(&store.get_name()) {
            return Err(Error::InvalidState(Some(
                "The cursor's effective object store has been deleted".to_owned(),
            )));
        }
        if let ObjectStoreOrIndex::Index(index) = &self.source &&
            !store.has_index(&index.Name())
        {
            return Err(Error::InvalidState(Some(
                "The cursor's source index has been deleted".to_owned(),
            )));
        }

        // Step 5. If this's got value flag is false, throw an "InvalidStateError" DOMException.
        self.check_got_value()?;

        // Step 6. If this's key only flag is true, throw an "InvalidStateError" DOMException.
        if self.key_only {
            return Err(Error::InvalidState(Some(
                "A key-only cursor has no value to write".to_owned(),
            )));
        }

        Ok(())
    }

    /// If this's got value flag is false, throw an "InvalidStateError" DOMException.
    ///
    /// The flag is unset while a previous iteration is outstanding, so this is what refuses a
    /// second `continue` before the first one's success event has fired.
    fn check_got_value(&self) -> Fallible<()> {
        if !self.got_value.get() {
            return Err(Error::InvalidState(Some(
                "The cursor is already iterating".to_owned(),
            )));
        }
        Ok(())
    }

    /// The tail the three iteration methods share: unset the got value flag, reopen the
    /// cursor's one request, and run another iterate operation against it.
    fn run_iteration(
        &self,
        cx: &mut JSContext,
        key: Option<IndexedDBKeyType>,
        primary_key: Option<IndexedDBKeyType>,
        count: Option<u32>,
    ) -> Fallible<()> {
        // Unset this's got value flag.
        self.got_value.set(false);

        // Let request be this's request. Set request's done flag to false.
        let request = self.request.get().ok_or(Error::InvalidState(Some(
            "The cursor has no request".to_owned(),
        )))?;
        request.set_ready_state_pending();

        let iteration_param = IterationParam {
            cursor: Trusted::new(self),
            key,
            primary_key,
            count,
        };
        let key_range = self.range.clone();

        // Run the steps to asynchronously execute a request with this as source and the steps
        // to iterate a cursor as operation, reusing request.
        IDBRequest::execute_async_with_context(
            cx,
            &self.effective_object_store(),
            self.operation_context(),
            |callback| {
                AsyncOperation::ReadOnly(AsyncReadOnlyOperation::Iterate {
                    callback,
                    key_range,
                    count: None,
                    shape: RecordsShape::WithValues,
                })
            },
            Some(request),
            Some(RecordsParam::Cursor(iteration_param)),
        )
        .map(|_| ())
    }
}

impl IDBCursorMethods<crate::DomTypeHolder> for IDBCursor {
    /// <https://www.w3.org/TR/IndexedDB-3/#dom-idbcursor-source>
    fn Source(&self) -> IDBObjectStoreOrIDBIndex {
        match &self.source {
            ObjectStoreOrIndex::ObjectStore(source) => {
                IDBObjectStoreOrIDBIndex::IDBObjectStore(source.as_rooted())
            },
            ObjectStoreOrIndex::Index(source) => {
                IDBObjectStoreOrIDBIndex::IDBIndex(source.as_rooted())
            },
        }
    }

    /// <https://www.w3.org/TR/IndexedDB-3/#dom-idbcursor-direction>
    fn Direction(&self) -> IDBCursorDirection {
        self.direction
    }

    /// <https://www.w3.org/TR/IndexedDB-3/#dom-idbcursor-key>
    fn GetKey(&self, cx: &mut JSContext, mut value: MutableHandleValue) -> Fallible<()> {
        // The key getter steps are to return the result of converting a key to a value with the cursor’s current key.
        //
        // NOTE: If key returns an object (e.g. a Date or Array), it returns the
        // same object instance every time it is inspected, until the cursor’s key is changed.
        // This means that if the object is modified, those modifications will be seen by
        // anyone inspecting the value of the cursor. However modifying such an object does not
        // modify the contents of the database.
        if let Some(cached) = &*self.cached_key.borrow() {
            value.set(cached.get());
            return Ok(());
        }

        match self.key.borrow().as_ref() {
            Some(key) => key_type_to_jsval(cx, key, value.reborrow())?,
            None => value.set(UndefinedValue()),
        }

        // The `Heap` is stored before it is set: `Heap::set` registers the slot's own
        // address with the GC store buffer, so the value has to be written where it
        // will live rather than moved in afterwards.
        *self.cached_key.borrow_mut() = Some(Heap::default());
        if let Some(cached) = self.cached_key.borrow().as_ref() {
            cached.set(value.get());
        }
        Ok(())
    }

    /// <https://www.w3.org/TR/IndexedDB-3/#dom-idbcursor-primarykey>
    fn GetPrimaryKey(
        &self,
        cx: &mut JSContext,
        mut value: MutableHandleValue,
    ) -> Fallible<()> {
        // NOTE: If primaryKey returns an object (e.g. a Date or Array),
        // it returns the same object instance every time it is inspected,
        // until the cursor’s effective key is changed. This means that if the object is modified,
        // those modifications will be seen by anyone inspecting the value of the cursor.
        // However modifying such an object does not modify the contents of the database.
        if let Some(cached) = &*self.cached_primary_key.borrow() {
            value.set(cached.get());
            return Ok(());
        }

        match self.effective_key() {
            Some(effective_key) => key_type_to_jsval(cx, &effective_key, value.reborrow())?,
            None => value.set(UndefinedValue()),
        }

        *self.cached_primary_key.borrow_mut() = Some(Heap::default());
        if let Some(cached) = self.cached_primary_key.borrow().as_ref() {
            cached.set(value.get());
        }
        Ok(())
    }

    /// <https://w3c.github.io/IndexedDB/#dom-idbcursor-request>
    fn Request(&self) -> Fallible<DomRoot<IDBRequest>> {
        // A cursor reaches script only as the result of the request that opened it, and that
        // request is what `set_request` stores, so the getter normally has one. The invariant
        // is established by IDBObjectStore::OpenCursor and IDBIndex::OpenCursor rather than
        // here, so report a cursor without one the way `run_iteration` already reports it.
        self.request.get().ok_or(Error::InvalidState(Some(
            "The cursor has no request".to_owned(),
        )))
    }

    /// <https://www.w3.org/TR/IndexedDB-3/#dom-idbcursor-advance>
    fn Advance(&self, cx: &mut JSContext, count: u32) -> Fallible<()> {
        // Step 1. If count is 0 (zero), throw a TypeError.
        if count == 0 {
            return Err(Error::Type(c"count must not be zero".to_owned()));
        }

        // Step 2. Let transaction be this's transaction.
        // Step 3. If transaction's state is not active, throw a "TransactionInactiveError"
        // DOMException.
        // Step 4. If this's source or effective object store has been deleted, throw an
        // "InvalidStateError" DOMException.
        self.check_transaction_and_source()?;

        // Step 5. If this's got value flag is false, throw an "InvalidStateError" DOMException.
        self.check_got_value()?;

        // Step 6. Unset this's got value flag.
        // Step 7. Let request be this's request.
        // Step 8. Set request's done flag to false.
        // Step 9. Let operation be an algorithm to run iterate a cursor with the current Realm
        // record, this, and count.
        // Step 10. Run asynchronously execute a request with this's source as source, operation
        // as operation and request as request.
        self.run_iteration(cx, None, None, Some(count))
    }

    /// <https://www.w3.org/TR/IndexedDB-3/#dom-idbcursor-continue>
    fn Continue(&self, cx: &mut JSContext, key: HandleValue) -> Fallible<()> {
        // Step 1. Let transaction be this's transaction.
        // Step 2. If transaction's state is not active, throw a "TransactionInactiveError"
        // DOMException.
        // Step 3. If this's source or effective object store has been deleted, throw an
        // "InvalidStateError" DOMException.
        self.check_transaction_and_source()?;

        // Step 4. If this's got value flag is false, throw an "InvalidStateError" DOMException.
        self.check_got_value()?;

        // Step 5. If key is given, then:
        let key = if key.is_undefined() {
            None
        } else {
            // Step 5.1. Let r be the result of running the steps to convert a value to a key
            // with key. Rethrow any exceptions.
            // Step 5.2. If r is "invalid value" or "invalid type", throw a "DataError"
            // DOMException.
            // Step 5.3. Let key be r.
            let key = convert_value_to_key(cx, key, None)?.into_result()?;

            // Step 5.4. If key is less than or equal to this's position and this's direction is
            // "next" or "nextunique", or if key is greater than or equal to this's position and
            // this's direction is "prev" or "prevunique", throw a "DataError" DOMException.
            if let Some(position) = self.position.borrow().as_ref() {
                let moves_backwards = match self.direction {
                    IDBCursorDirection::Next | IDBCursorDirection::Nextunique => &key <= position,
                    IDBCursorDirection::Prev | IDBCursorDirection::Prevunique => &key >= position,
                };
                if moves_backwards {
                    return Err(Error::Data(Some(
                        "continue() must move the cursor in its own direction".to_owned(),
                    )));
                }
            }
            Some(key)
        };

        // Step 6. Unset this's got value flag.
        // Step 7. Let request be this's request.
        // Step 8. Set request's done flag to false.
        // Step 9. Let operation be an algorithm to run iterate a cursor with the current Realm
        // record, this, and key (if given).
        // Step 10. Run asynchronously execute a request with this's source as source, operation
        // as operation and request as request.
        self.run_iteration(cx, key, None, None)
    }

    /// <https://www.w3.org/TR/IndexedDB-3/#dom-idbcursor-continueprimarykey>
    fn ContinuePrimaryKey(
        &self,
        cx: &mut JSContext,
        key: HandleValue,
        primary_key: HandleValue,
    ) -> Fallible<()> {
        // Step 1. Let transaction be this's transaction.
        // Step 2. If transaction's state is not active, throw a "TransactionInactiveError"
        // DOMException.
        // Step 3. If this's source or effective object store has been deleted, throw an
        // "InvalidStateError" DOMException.
        self.check_transaction_and_source()?;

        // Step 4. If this's source is not an index, throw an "InvalidAccessError" DOMException.
        if !matches!(self.source, ObjectStoreOrIndex::Index(_)) {
            return Err(Error::InvalidAccess(Some(
                "continuePrimaryKey() requires an index cursor".to_owned(),
            )));
        }

        // Step 5. If this's direction is not "next" or "prev", throw an "InvalidAccessError"
        // DOMException.
        if !matches!(
            self.direction,
            IDBCursorDirection::Next | IDBCursorDirection::Prev
        ) {
            return Err(Error::InvalidAccess(Some(
                "continuePrimaryKey() requires direction next or prev".to_owned(),
            )));
        }

        // Step 6. If this's got value flag is false, throw an "InvalidStateError" DOMException.
        self.check_got_value()?;

        // Step 7. Let r be the result of running the steps to convert a value to a key with key.
        // Rethrow any exceptions.
        // Step 8. If r is "invalid value" or "invalid type", throw a "DataError" DOMException.
        // Step 9. Let key be r.
        let key = convert_value_to_key(cx, key, None)?.into_result()?;

        // Step 10. Let r be the result of running the steps to convert a value to a key with
        // primaryKey. Rethrow any exceptions.
        // Step 11. If r is "invalid value" or "invalid type", throw a "DataError" DOMException.
        // Step 12. Let primaryKey be r.
        let primary_key = convert_value_to_key(cx, primary_key, None)?.into_result()?;

        // Step 13. If key is less than this's position and this's direction is "next", or if key
        // is greater than this's position and this's direction is "prev", throw a "DataError"
        // DOMException.
        //
        // Step 14. If key is equal to this's position and primaryKey is less than or equal to
        // this's object store position and this's direction is "next", or if key is equal to
        // this's position and primaryKey is greater than or equal to this's object store
        // position and this's direction is "prev", throw a "DataError" DOMException.
        //
        // Step 14 is the reason the object store position has to survive iterate_cursor: it is
        // the only record of where within a run of equal index keys the cursor stopped.
        if let Some(position) = self.position.borrow().as_ref() {
            let object_store_position = self.object_store_position.borrow();
            let refuses = match self.direction {
                IDBCursorDirection::Next => {
                    &key < position ||
                        (&key == position &&
                            object_store_position
                                .as_ref()
                                .is_some_and(|current| &primary_key <= current))
                },
                IDBCursorDirection::Prev => {
                    &key > position ||
                        (&key == position &&
                            object_store_position
                                .as_ref()
                                .is_some_and(|current| &primary_key >= current))
                },
                // Refused at step 5 above.
                IDBCursorDirection::Nextunique | IDBCursorDirection::Prevunique => false,
            };
            if refuses {
                return Err(Error::Data(Some(
                    "continuePrimaryKey() must move the cursor in its own direction".to_owned(),
                )));
            }
        }

        // Step 15. Unset this's got value flag.
        // Step 16. Let request be this's request.
        // Step 17. Set request's done flag to false.
        // Step 18. Let operation be an algorithm to run iterate a cursor with the current Realm
        // record, this, key and primaryKey.
        // Step 19. Run asynchronously execute a request with this's source as source, operation
        // as operation and request as request.
        self.run_iteration(cx, Some(key), Some(primary_key), None)
    }

    /// <https://www.w3.org/TR/IndexedDB-3/#dom-idbcursor-update>
    fn Update(&self, cx: &mut JSContext, value: HandleValue) -> Fallible<DomRoot<IDBRequest>> {
        // Steps 1 through 6.
        self.check_writable()?;

        // Step 7. Let targetRealm be a user-agent defined Realm.
        // Steps 8 through 11 are the effective object store's, because the clone, the key path
        // check and the index records all need state that belongs to it.
        let Some(effective_key) = self.effective_key() else {
            return Err(Error::InvalidState(Some(
                "The cursor has no effective key to update".to_owned(),
            )));
        };
        self.effective_object_store().store_record_with_known_key(
            cx,
            RequestSource::Cursor(Dom::from_ref(self)),
            value,
            &effective_key,
        )
    }

    /// <https://www.w3.org/TR/IndexedDB-3/#dom-idbcursor-delete>
    fn Delete(&self, cx: &mut JSContext) -> Fallible<DomRoot<IDBRequest>> {
        // Steps 1 through 6, the same preconditions update() checks and in the same order.
        self.check_writable()?;

        // Step 7. Let operation be an algorithm to run delete records from an object store with
        // this's effective object store and this's effective key.
        // Step 8. Return the result of running asynchronously execute a request.
        let Some(effective_key) = self.effective_key() else {
            return Err(Error::InvalidState(Some(
                "The cursor has no effective key to delete".to_owned(),
            )));
        };
        self.effective_object_store().delete_record_with_known_key(
            cx,
            RequestSource::Cursor(Dom::from_ref(self)),
            &effective_key,
        )
    }
}

/// A struct containing parameters for
/// <https://www.w3.org/TR/IndexedDB-3/#iterate-a-cursor>
#[derive(Clone)]
pub(crate) struct IterationParam {
    pub(crate) cursor: Trusted<IDBCursor>,
    pub(crate) key: Option<IndexedDBKeyType>,
    pub(crate) primary_key: Option<IndexedDBKeyType>,
    pub(crate) count: Option<u32>,
}

/// <https://www.w3.org/TR/IndexedDB-3/#iterate-a-cursor>
///
/// NOTE: Be cautious: this part of the specification seems to assume the cursor’s source is an
/// index. Therefore,
///   "record’s key" means the key of the record,
///   "record’s value" means the primary key of the record, and
///   "record’s referenced value" means the value of the record.
pub(crate) fn iterate_cursor(
    global: &GlobalScope,
    cx: &mut JSContext,
    param: &IterationParam,
    records: Vec<IndexedDBRecord>,
) -> Result<Option<DomRoot<IDBCursor>>, Error> {
    // Unpack IterationParam
    let cursor = param.cursor.root();
    let key = param.key.clone();
    let primary_key = param.primary_key.clone();
    let count = param.count;

    // Step 1. Let source be cursor’s source.
    let source = &cursor.source;

    // Step 2. Let direction be cursor’s direction.
    let direction = cursor.direction;

    // Step 3. Assert: if primaryKey is given, source is an index and direction is "next" or "prev".
    if primary_key.is_some() {
        assert!(matches!(source, ObjectStoreOrIndex::Index(..)));
        assert!(matches!(
            direction,
            IDBCursorDirection::Next | IDBCursorDirection::Prev
        ));
    }

    // Step 4. Let records be the list of records in source.
    // NOTE: It is given as a function parameter.

    // Step 5. Let range be cursor’s range.
    let range = &cursor.range;

    // Step 6. Let position be cursor’s position.
    let mut position = cursor.position.borrow().clone();

    // Step 7. Let object store position be cursor’s object store position.
    //
    // NOTE: This is a local, exactly like position above. Steps 9.4, 10 and 11 rebind it and
    // only step 11 writes it back to the cursor. Writing the cursor field inside the loop and
    // then restoring this local at step 11 would discard every iteration's object store
    // position, which is the cursor's effective key when the source is an index.
    let mut object_store_position = cursor.object_store_position.borrow().clone();

    // Step 8. If count is not given, let count be 1.
    let mut count = count.unwrap_or(1);

    let mut found_record: Option<&IndexedDBRecord> = None;

    // Step 9. While count is greater than 0:
    while count > 0 {
        // Step 9.1. Switch on direction:
        found_record = match direction {
            // "next"
            IDBCursorDirection::Next => records.iter().find(|record| {
                // Let found record be the first record in records which satisfy all of the
                // following requirements:

                // If key is defined, the record’s key is greater than or equal to key.
                let requirement1 = || match &key {
                    Some(key) => &record.key >= key,
                    None => true,
                };

                // If primaryKey is defined, the record’s key is equal to key and the record’s
                // value is greater than or equal to primaryKey, or the record’s key is greater
                // than key.
                let requirement2 = || match &primary_key {
                    Some(primary_key) => key.as_ref().is_some_and(|key| {
                        (&record.key == key && &record.primary_key >= primary_key) ||
                            &record.key > key
                    }),
                    _ => true,
                };

                // If position is defined, and source is an object store, the record’s key is
                // greater than position.
                let requirement3 = || match (&position, source) {
                    (Some(position), ObjectStoreOrIndex::ObjectStore(_)) => &record.key > position,
                    _ => true,
                };

                // If position is defined, and source is an index, the record’s key is equal to
                // position and the record’s value is greater than object store position or the
                // record’s key is greater than position.
                let requirement4 = || match (&position, source) {
                    (Some(position), ObjectStoreOrIndex::Index(_)) => {
                        (&record.key == position &&
                            object_store_position.as_ref().is_some_and(
                                |object_store_position| &record.primary_key > object_store_position,
                            )) ||
                            &record.key > position
                    },
                    _ => true,
                };

                // The record’s key is in range.
                let requirement5 = || range.contains(&record.key);

                // NOTE: Use closures here for lazy computation on requirements.
                requirement1() &&
                    requirement2() &&
                    requirement3() &&
                    requirement4() &&
                    requirement5()
            }),
            // "nextunique"
            IDBCursorDirection::Nextunique => records.iter().find(|record| {
                // Let found record be the first record in records which satisfy all of the
                // following requirements:

                // If key is defined, the record’s key is greater than or equal to key.
                let requirement1 = || match &key {
                    Some(key) => &record.key >= key,
                    None => true,
                };

                // If position is defined, the record’s key is greater than position.
                let requirement2 = || match &position {
                    Some(position) => &record.key > position,
                    None => true,
                };

                // The record’s key is in range.
                let requirement3 = || range.contains(&record.key);

                // NOTE: Use closures here for lazy computation on requirements.
                requirement1() && requirement2() && requirement3()
            }),
            // "prev"
            IDBCursorDirection::Prev => {
                records.iter().rev().find(|&record| {
                    // Let found record be the last record in records which satisfy all of the
                    // following requirements:

                    // If key is defined, the record’s key is less than or equal to key.
                    let requirement1 = || match &key {
                        Some(key) => &record.key <= key,
                        None => true,
                    };

                    // If primaryKey is defined, the record’s key is equal to key and the record’s
                    // value is less than or equal to primaryKey, or the record’s key is less than
                    // key.
                    let requirement2 = || match &primary_key {
                        Some(primary_key) => key.as_ref().is_some_and(|key| {
                            (&record.key == key && &record.primary_key <= primary_key) ||
                                &record.key < key
                        }),
                        _ => true,
                    };

                    // If position is defined, and source is an object store, the record’s key is
                    // less than position.
                    let requirement3 = || match (&position, source) {
                        (Some(position), ObjectStoreOrIndex::ObjectStore(_)) => {
                            &record.key < position
                        },
                        _ => true,
                    };

                    // If position is defined, and source is an index, the record’s key is equal to
                    // position and the record’s value is less than object store position or the
                    // record’s key is less than position.
                    let requirement4 = || match (&position, source) {
                        (Some(position), ObjectStoreOrIndex::Index(_)) => {
                            (&record.key == position &&
                                object_store_position.as_ref().is_some_and(
                                    |object_store_position| {
                                        &record.primary_key < object_store_position
                                    },
                                )) ||
                                &record.key < position
                        },
                        _ => true,
                    };

                    // The record’s key is in range.
                    let requirement5 = || range.contains(&record.key);

                    // NOTE: Use closures here for lazy computation on requirements.
                    requirement1() &&
                        requirement2() &&
                        requirement3() &&
                        requirement4() &&
                        requirement5()
                })
            },
            // "prevunique"
            IDBCursorDirection::Prevunique => records
                .iter()
                .rev()
                .find(|&record| {
                    // Let temp record be the last record in records which satisfy all of the
                    // following requirements:

                    // If key is defined, the record’s key is less than or equal to key.
                    let requirement1 = || match &key {
                        Some(key) => &record.key <= key,
                        None => true,
                    };

                    // If position is defined, the record’s key is less than position.
                    let requirement2 = || match &position {
                        Some(position) => &record.key < position,
                        None => true,
                    };

                    // The record’s key is in range.
                    let requirement3 = || range.contains(&record.key);

                    // NOTE: Use closures here for lazy computation on requirements.
                    requirement1() && requirement2() && requirement3()
                })
                // If temp record is defined, let found record be the first record in records
                // whose key is equal to temp record’s key.
                .map(|temp_record| {
                    records
                        .iter()
                        .find(|&record| record.key == temp_record.key)
                        // The search starts from a record that is already in `records`, so a
                        // reflexive comparison always finds at least that one. Key equality is
                        // not reflexive for a NaN number key, which script cannot produce but
                        // stored bytes can, so a corrupt record falls back to itself instead of
                        // killing the content process.
                        .unwrap_or(temp_record)
                }),
        };

        match found_record {
            // Step 9.2. If found record is not defined, then:
            None => {
                // Step 9.2.1. Set cursor’s key to undefined.
                cursor.set_key(None);

                // Step 9.2.2. If source is an index, set cursor’s object store position to undefined.
                if matches!(source, ObjectStoreOrIndex::Index(_)) {
                    cursor.set_object_store_position(None);
                }

                // Step 9.2.3. If cursor’s key only flag is unset, set cursor’s value to undefined.
                if !cursor.key_only {
                    cursor.value.set(UndefinedValue());
                }

                // Step 9.2.4. Return null.
                return Ok(None);
            },
            Some(found_record) => {
                // Step 9.3. Let position be found record’s key.
                position = Some(found_record.key.clone());

                // Step 9.4. If source is an index, let object store position be found record’s value.
                if matches!(source, ObjectStoreOrIndex::Index(_)) {
                    object_store_position = Some(found_record.primary_key.clone());
                }

                // Step 9.5. Decrease count by 1.
                count -= 1;
            },
        }
    }
    // Step 9 runs at least once: `count` is never `Some(0)`, because `advance()` rejects a
    // zero count and the other two iteration methods pass `None`. An iteration that finds
    // nothing has already returned at step 9.2.4, so reaching here means the loop bound a
    // record. A cursor that somehow did not is a request that failed, not a crash.
    let Some(found_record) = found_record else {
        warn!("iterate_cursor reached step 10 without a found record.");
        return Err(Error::Operation(None));
    };

    // Step 10. Set cursor’s position to position.
    cursor.set_position(position);

    // Step 11. If source is an index, set cursor’s object store position to object store position.
    if let ObjectStoreOrIndex::Index(_) = source {
        cursor.set_object_store_position(object_store_position);
    }

    // Step 12. Set cursor’s key to found record’s key.
    cursor.set_key(Some(found_record.key.clone()));

    // Step 13. If cursor’s key only flag is unset, then:
    if !cursor.key_only {
        // Step 13.1. Let serialized be found record’s referenced value.
        // Step 13.2. Set cursor’s value to ! StructuredDeserialize(serialized, targetRealm)
        rooted!(&in(cx) let mut new_cursor_value = UndefinedValue());
        postcard::from_bytes(&found_record.value)
            .map_err(|_| Error::Data(None))
            .and_then(|data| {
                structuredclone::read(cx, global, data, new_cursor_value.handle_mut())
            })?;
        cursor.value.set(new_cursor_value.get());
    }

    // Step 14. Set cursor’s got value flag.
    cursor.got_value.set(true);

    // Step 15. Return cursor.
    Ok(Some(cursor))
}
