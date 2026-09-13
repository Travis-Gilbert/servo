/* This Source Code Form is subject to the terms of the Mozilla Public
 * License, v. 2.0. If a copy of the MPL was not distributed with this
 * file, You can obtain one at https://mozilla.org/MPL/2.0/. */
use dom_struct::dom_struct;
use js::context::JSContext;
use js::conversions::ToJSValConvertible;
use js::gc::MutableHandleValue;
use js::jsapi::Heap;
use js::jsval::JSVal;
use js::rust::HandleValue;
use script_bindings::cell::DomRefCell;
use script_bindings::codegen::GenericBindings::IDBIndexBinding::IDBIndexMethods;
use script_bindings::codegen::GenericBindings::IDBObjectStoreBinding::IDBGetAllOptions;
use script_bindings::codegen::GenericBindings::IDBTransactionBinding::IDBTransactionMode;
use script_bindings::error::{Error, ErrorResult};
use script_bindings::reflector::{Reflector, reflect_dom_object_with_cx};
use script_bindings::str::DOMString;
use storage_traits::indexeddb::{
    AsyncOperation, AsyncReadOnlyOperation, KvsOperationContext, KvsOperationTarget, RecordsShape,
};

use crate::dom::bindings::codegen::Bindings::IDBCursorBinding::IDBCursorDirection;
use crate::dom::bindings::error::Fallible;
use crate::dom::bindings::trace::RootedTraceableBox;
use crate::dom::bindings::refcounted::Trusted;
use crate::dom::bindings::reflector::DomGlobal;
use crate::dom::bindings::root::{Dom, DomRoot};
use crate::dom::globalscope::GlobalScope;
use crate::dom::idbobjectstore::KeyPath;
use crate::dom::indexeddb::idbcursor::{IDBCursor, IterationParam, ObjectStoreOrIndex};
use crate::dom::indexeddb::idbcursorwithvalue::IDBCursorWithValue;
use crate::dom::indexeddb::idbobjectstore::IDBObjectStore;
use crate::dom::indexeddb::idbrequest::{
    GetAllKind, GetAllRequest, IDBRequest, RecordsParam, RequestSource,
};
use crate::indexeddb::convert_value_to_key_range;

#[dom_struct]
pub(crate) struct IDBIndex {
    reflector_: Reflector,
    object_store: Dom<IDBObjectStore>,
    name: DomRefCell<DOMString>,
    multi_entry: bool,
    unique: bool,
    key_path: KeyPath,
    /// <https://w3c.github.io/IndexedDB/#abort-an-upgrade-transaction>
    /// An index created during the upgrade transaction leaves the store's index set when
    /// the transaction aborts. One that existed before it gets its name back instead.
    newly_created_during_transaction: bool,
    /// The name this index carried when the upgrade transaction began, recorded on the
    /// first rename so the abort has something to restore.
    rollback_name: DomRefCell<Option<DOMString>>,
    /// `keyPath` converted to a value, kept so the attribute hands back the same object
    /// every time it is read. An index's key path never changes, so this is written once.
    #[ignore_malloc_size_of = "mozjs"]
    cached_key_path: DomRefCell<Option<Heap<JSVal>>>,
}

impl IDBIndex {
    pub fn new_inherited(
        object_store: &IDBObjectStore,
        name: DOMString,
        multi_entry: bool,
        unique: bool,
        key_path: KeyPath,
        newly_created_during_transaction: bool,
    ) -> IDBIndex {
        IDBIndex {
            reflector_: Reflector::new(),
            object_store: Dom::from_ref(object_store),
            name: DomRefCell::new(name),
            multi_entry,
            unique,
            key_path,
            newly_created_during_transaction,
            rollback_name: DomRefCell::new(None),
            cached_key_path: DomRefCell::new(None),
        }
    }

    /// Whether an aborting upgrade transaction should drop this handle rather than
    /// restore it.
    pub(crate) fn was_newly_created_during_transaction(&self) -> bool {
        self.newly_created_during_transaction
    }

    /// <https://w3c.github.io/IndexedDB/#abort-an-upgrade-transaction>
    /// Step 6: if the index was not newly created during the transaction, set the
    /// handle's name back to the index's name. Returns the name the handle now carries,
    /// which is the key the store's index set has to file it under.
    pub(crate) fn restore_name_after_abort(&self) -> DOMString {
        if let Some(name) = self.rollback_name.borrow_mut().take() {
            *self.name.borrow_mut() = name;
        }
        self.name.borrow().clone()
    }

    #[allow(clippy::too_many_arguments)]
    pub fn new(
        cx: &mut JSContext,
        global: &GlobalScope,
        object_store: &IDBObjectStore,
        name: DOMString,
        multi_entry: bool,
        unique: bool,
        key_path: KeyPath,
        newly_created_during_transaction: bool,
    ) -> DomRoot<IDBIndex> {
        reflect_dom_object_with_cx(
            Box::new(IDBIndex::new_inherited(
                object_store,
                name,
                multi_entry,
                unique,
                key_path,
                newly_created_during_transaction,
            )),
            global,
            cx,
        )
    }

    /// The object store this index belongs to.
    pub(crate) fn object_store(&self) -> DomRoot<IDBObjectStore> {
        self.object_store.as_rooted()
    }

    /// The index's name, as the object store knows it when it builds index records.
    pub(crate) fn index_name(&self) -> String {
        self.name.borrow().to_string()
    }

    /// The key path index keys are extracted with.
    pub(crate) fn index_key_path(&self) -> &KeyPath {
        &self.key_path
    }

    /// Whether each element of an extracted array key becomes its own index record.
    pub(crate) fn is_multi_entry(&self) -> bool {
        self.multi_entry
    }

    /// An index request addresses the same object store as its handle, so the transaction, the
    /// store name on the wire and the request's source all come from the object store. What the
    /// context adds is the index name, which is how the backend knows to range over index
    /// records rather than object store records.
    fn operation_context(&self) -> KvsOperationContext {
        KvsOperationContext {
            target: KvsOperationTarget::Index {
                name: self.name.borrow().to_string(),
            },
            index_updates: Vec::new(),
        }
    }

    /// <https://www.w3.org/TR/IndexedDB-3/#index-deleted>
    ///
    /// Every request method begins by rejecting a deleted index or a deleted owning object
    /// store with an "InvalidStateError" DOMException.
    fn verify_not_deleted(&self) -> ErrorResult {
        let transaction = self.object_store.transaction();
        if !self.object_store.has_index(&self.name.borrow()) ||
            !transaction
                .get_db()
                .object_store_exists(&self.object_store.get_name())
        {
            return Err(Error::InvalidState(Some(
                "Index or its object store has been deleted".to_owned(),
            )));
        }
        Ok(())
    }

    /// Rejects a request made against an inactive transaction with a
    /// "TransactionInactiveError" DOMException.
    fn check_transaction_active(&self) -> Fallible<()> {
        let transaction = self.object_store.transaction();
        if !transaction.is_active() || !transaction.is_usable() {
            return Err(Error::TransactionInactive(None));
        }
        Ok(())
    }

    /// <https://www.w3.org/TR/IndexedDB-3/#dom-idbindex-opencursor>
    /// <https://www.w3.org/TR/IndexedDB-3/#dom-idbindex-openkeycursor>
    fn open_cursor(
        &self,
        cx: &mut JSContext,
        query: HandleValue,
        direction: IDBCursorDirection,
        key_only: bool,
    ) -> Fallible<DomRoot<IDBRequest>> {
        // Step 1. Let transaction be this's transaction.
        // Step 2. Let index be this's index.
        let transaction = self.object_store.transaction();

        // Step 3. If index or index's object store has been deleted, throw an
        // "InvalidStateError" DOMException.
        self.verify_not_deleted()?;

        // Step 4. If transaction is not active, throw a "TransactionInactiveError" DOMException.
        self.check_transaction_active()?;

        // Step 5. Let range be the result of running the steps to convert a value to a key range
        // with query. Rethrow any exceptions.
        let range = convert_value_to_key_range(cx, query, Some(false))?;

        // Step 6. Let cursor be a new cursor with transaction set to transaction, an undefined
        // position, direction set to direction, got value flag unset, undefined key and value,
        // source set to index, range set to range, and key only flag set to key only.
        //
        // The cursor's source being the index is what makes its effective key the object store
        // position rather than the position, which is the distinction iterate_cursor turns on.
        let cursor = if key_only {
            IDBCursor::new(
                cx,
                &self.global(),
                &transaction,
                direction,
                false,
                ObjectStoreOrIndex::Index(Dom::from_ref(self)),
                range.clone(),
                key_only,
            )
        } else {
            DomRoot::upcast(IDBCursorWithValue::new(
                cx,
                &self.global(),
                &transaction,
                direction,
                false,
                ObjectStoreOrIndex::Index(Dom::from_ref(self)),
                range.clone(),
                key_only,
            ))
        };

        // Step 7. Run the steps to asynchronously execute a request and return the IDBRequest
        // created by these steps, with this as source and the steps to iterate a cursor as
        // operation.
        let iteration_param = IterationParam {
            cursor: Trusted::new(&cursor),
            key: None,
            primary_key: None,
            count: None,
        };

        IDBRequest::execute_async_from_source(
            cx,
            &self.object_store,
            RequestSource::Index(Dom::from_ref(self)),
            self.operation_context(),
            |callback| {
                AsyncOperation::ReadOnly(AsyncReadOnlyOperation::Iterate {
                    callback,
                    key_range: range,
                    count: None,
                    shape: RecordsShape::WithValues,
                })
            },
            None,
            Some(RecordsParam::Cursor(iteration_param)),
        )
        .inspect(|request| cursor.set_request(request))
    }
}

impl IDBIndexMethods<crate::DomTypeHolder> for IDBIndex {
    /// <https://www.w3.org/TR/IndexedDB/#dom-idbindex-name>
    fn Name(&self) -> DOMString {
        self.name.borrow().clone()
    }

    /// <https://www.w3.org/TR/IndexedDB/#ref-for-dom-idbindex-name%E2%91%A2>
    fn SetName(&self, name: DOMString) -> ErrorResult {
        // Step 1: Let name be the given value.
        // Step 2: Let transaction be this’s transaction.
        let transaction = self.object_store.transaction();

        // Step 3: Let index be this’s index.
        // We do not have an explicit object representing the underlying index.

        // Step 4: If transaction is not an upgrade transaction, throw an "InvalidStateError" DOMException.
        if transaction.get_mode() != IDBTransactionMode::Versionchange {
            return Err(Error::InvalidState(Some(
                "Transaction is not an upgrade transaction".to_owned(),
            )));
        }

        // Step 5: If transaction’s state is not active, then throw a "TransactionInactiveError" DOMException.
        if !transaction.is_active() {
            return Err(Error::TransactionInactive(Some(
                "Transaction is not active while updating index name".to_owned(),
            )));
        }

        // Step 6: If index or index’s object store has been deleted, throw an "InvalidStateError" DOMException.
        let mut stored_name = self.name.borrow_mut();
        if !self.object_store.has_index(&stored_name) ||
            !transaction
                .get_db()
                .object_store_exists(&self.object_store.get_name())
        {
            return Err(Error::InvalidState(Some(
                "Index or its object store has been deleted".to_owned(),
            )));
        }

        // Step 7: If index’s name is equal to name, terminate these steps.
        if *stored_name == name {
            return Ok(());
        }

        // Step 8: If an index named name already exists in index’s object store, throw a "ConstraintError" DOMException.
        if self.object_store.has_index(&name) {
            return Err(Error::Constraint(Some(
                "An index with the given name already exists".to_owned(),
            )));
        }

        // Step 9: Set index’s name to name.
        // An aborting upgrade transaction has to put the first name back, not the name of
        // whatever rename happened to be last.
        {
            let mut rollback_name = self.rollback_name.borrow_mut();
            if rollback_name.is_none() {
                *rollback_name = Some(stored_name.clone());
            }
        }
        self.object_store.rename_index(&stored_name, &name);

        // Step 10: Set this’s name to name.
        *stored_name = name;
        Ok(())
    }

    /// <https://www.w3.org/TR/IndexedDB/#dom-idbindex-objectstore>
    fn ObjectStore(&self) -> DomRoot<IDBObjectStore> {
        self.object_store.as_rooted()
    }

    /// <https://www.w3.org/TR/IndexedDB/#dom-idbindex-multientry>
    fn MultiEntry(&self) -> bool {
        self.multi_entry
    }

    /// <https://www.w3.org/TR/IndexedDB/#dom-idbindex-unique>
    fn Unique(&self) -> bool {
        self.unique
    }

    /// <https://www.w3.org/TR/IndexedDB/#dom-idbindex-keypath>
    fn KeyPath(&self, cx: &mut JSContext, mut retval: MutableHandleValue) {
        // A sequence key path converts to a fresh Array on every call, so converting on
        // each read would hand script a different object each time it looked. The value is
        // converted once and kept; `idbindex_keyPath.any.js` asserts both halves of that,
        // that one index answers with the same object and that two index handles onto the
        // same index answer with different ones.
        if let Some(cached) = self.cached_key_path.borrow().as_ref() {
            retval.set(cached.get());
            return;
        }

        match &self.key_path {
            KeyPath::String(string) => {
                string.safe_to_jsval(cx, retval.reborrow());
            },
            KeyPath::StringSequence(sequence) => {
                sequence.safe_to_jsval(cx, retval.reborrow());
            },
        }

        // The `Heap` is stored before it is set: `Heap::set` registers the slot's own
        // address with the GC store buffer, so the value has to be written where it will
        // live rather than moved in afterwards.
        *self.cached_key_path.borrow_mut() = Some(Heap::default());
        if let Some(cached) = self.cached_key_path.borrow().as_ref() {
            cached.set(retval.get());
        }
    }

    /// <https://www.w3.org/TR/IndexedDB-3/#dom-idbindex-get>
    fn Get(&self, cx: &mut JSContext, query: HandleValue) -> Fallible<DomRoot<IDBRequest>> {
        // Step 1. Let transaction be this's transaction.
        // Step 2. Let index be this's index.
        // Step 3. If index or index's object store has been deleted, throw an
        // "InvalidStateError" DOMException.
        self.verify_not_deleted()?;

        // Step 4. If transaction's state is not active, then throw a
        // "TransactionInactiveError" DOMException.
        self.check_transaction_active()?;

        // Step 5. Let range be the result of converting a value to a key range with query and
        // true. Rethrow any exceptions.
        let serialized_query = convert_value_to_key_range(cx, query, Some(true));

        // Step 6. Let operation be an algorithm to run retrieve a referenced value from an index
        // with the current Realm record, index, and range.
        // Step 7. Return the result (an IDBRequest) of running asynchronously execute a request
        // with this and operation.
        serialized_query.and_then(|q| {
            IDBRequest::execute_async_from_source(
                cx,
                &self.object_store,
                RequestSource::Index(Dom::from_ref(self)),
                self.operation_context(),
                |callback| {
                    AsyncOperation::ReadOnly(AsyncReadOnlyOperation::GetItem {
                        callback,
                        key_range: q,
                    })
                },
                None,
                None,
            )
        })
    }

    /// <https://www.w3.org/TR/IndexedDB-3/#dom-idbindex-getkey>
    fn GetKey(&self, cx: &mut JSContext, query: HandleValue) -> Fallible<DomRoot<IDBRequest>> {
        // Step 1. Let transaction be this's transaction.
        // Step 2. Let index be this's index.
        // Step 3. If index or index's object store has been deleted, throw an
        // "InvalidStateError" DOMException.
        self.verify_not_deleted()?;

        // Step 4. If transaction's state is not active, then throw a
        // "TransactionInactiveError" DOMException.
        self.check_transaction_active()?;

        // Step 5. Let range be the result of converting a value to a key range with query and
        // true. Rethrow any exceptions.
        let serialized_query = convert_value_to_key_range(cx, query, Some(true));

        // Step 6. Let operation be an algorithm to run retrieve a value from an index with
        // index and range.
        // Step 7. Return the result (an IDBRequest) of running asynchronously execute a request
        // with this and operation.
        serialized_query.and_then(|q| {
            IDBRequest::execute_async_from_source(
                cx,
                &self.object_store,
                RequestSource::Index(Dom::from_ref(self)),
                self.operation_context(),
                |callback| {
                    AsyncOperation::ReadOnly(AsyncReadOnlyOperation::GetKey {
                        callback,
                        key_range: q,
                    })
                },
                None,
                None,
            )
        })
    }

    /// <https://www.w3.org/TR/IndexedDB-3/#dom-idbindex-getall>
    fn GetAll(
        &self,
        cx: &mut JSContext,
        query_or_options: HandleValue,
        count: Option<u32>,
    ) -> Fallible<DomRoot<IDBRequest>> {
        // Step 1. Let transaction be this's transaction.
        // Step 2. Let index be this's index.
        // Step 3. If index or index's object store has been deleted, throw an
        // "InvalidStateError" DOMException.
        self.verify_not_deleted()?;

        // Step 4. If transaction's state is not active, then throw a
        // "TransactionInactiveError" DOMException.
        self.check_transaction_active()?;

        // Steps 6 to 9. Resolve the range, the direction and the count the read may apply.
        let request = GetAllRequest::resolve(cx, GetAllKind::Values, query_or_options, count)?;

        // Steps 10 to 13. Run retrieve multiple records from an index as the operation of an
        // asynchronously executed request.
        IDBRequest::execute_async_from_source(
            cx,
            &self.object_store,
            RequestSource::Index(Dom::from_ref(self)),
            self.operation_context(),
            |callback| {
                AsyncOperation::ReadOnly(AsyncReadOnlyOperation::Iterate {
                    callback,
                    key_range: request.key_range,
                    count: request.count,
                    shape: GetAllKind::Values.shape(),
                })
            },
            None,
            Some(request.records_param),
        )
    }

    /// <https://www.w3.org/TR/IndexedDB-3/#dom-idbindex-getallrecords>
    fn GetAllRecords(
        &self,
        cx: &mut JSContext,
        options: RootedTraceableBox<IDBGetAllOptions>,
    ) -> Fallible<DomRoot<IDBRequest>> {
        // Step 1. Let transaction be this's transaction.
        // Step 2. Let index be this's index.
        // Step 3. If index or index's object store has been deleted, throw an
        // "InvalidStateError" DOMException.
        self.verify_not_deleted()?;

        // Step 4. If transaction's state is not active, then throw a
        // "TransactionInactiveError" DOMException.
        self.check_transaction_active()?;

        // Steps 6 to 9. Resolve the range, the direction and the count the read may apply.
        let request = GetAllRequest::from_options(cx, GetAllKind::Records, &options)?;

        // Steps 10 to 13. Run retrieve multiple records from an index as the operation of an
        // asynchronously executed request.
        IDBRequest::execute_async_from_source(
            cx,
            &self.object_store,
            RequestSource::Index(Dom::from_ref(self)),
            self.operation_context(),
            |callback| {
                AsyncOperation::ReadOnly(AsyncReadOnlyOperation::Iterate {
                    callback,
                    key_range: request.key_range,
                    count: request.count,
                    shape: GetAllKind::Records.shape(),
                })
            },
            None,
            Some(request.records_param),
        )
    }

    /// <https://www.w3.org/TR/IndexedDB-3/#dom-idbindex-getallkeys>
    fn GetAllKeys(
        &self,
        cx: &mut JSContext,
        query_or_options: HandleValue,
        count: Option<u32>,
    ) -> Fallible<DomRoot<IDBRequest>> {
        // Step 1. Let transaction be this's transaction.
        // Step 2. Let index be this's index.
        // Step 3. If index or index's object store has been deleted, throw an
        // "InvalidStateError" DOMException.
        self.verify_not_deleted()?;

        // Step 4. If transaction's state is not active, then throw a
        // "TransactionInactiveError" DOMException.
        self.check_transaction_active()?;

        // Steps 6 to 9. Resolve the range, the direction and the count the read may apply.
        let request = GetAllRequest::resolve(cx, GetAllKind::PrimaryKeys, query_or_options, count)?;

        // Steps 10 to 13. Run retrieve multiple records from an index as the operation of an
        // asynchronously executed request.
        IDBRequest::execute_async_from_source(
            cx,
            &self.object_store,
            RequestSource::Index(Dom::from_ref(self)),
            self.operation_context(),
            |callback| {
                AsyncOperation::ReadOnly(AsyncReadOnlyOperation::Iterate {
                    callback,
                    key_range: request.key_range,
                    count: request.count,
                    shape: GetAllKind::PrimaryKeys.shape(),
                })
            },
            None,
            Some(request.records_param),
        )
    }

    /// <https://www.w3.org/TR/IndexedDB-3/#dom-idbindex-count>
    fn Count(&self, cx: &mut JSContext, query: HandleValue) -> Fallible<DomRoot<IDBRequest>> {
        // Step 1. Let transaction be this's transaction.
        // Step 2. Let index be this's index.
        // Step 3. If index or index's object store has been deleted, throw an
        // "InvalidStateError" DOMException.
        self.verify_not_deleted()?;

        // Step 4. If transaction's state is not active, then throw a
        // "TransactionInactiveError" DOMException.
        self.check_transaction_active()?;

        // Step 5. Let range be the result of converting a value to a key range with query.
        // Rethrow any exceptions.
        let serialized_query = convert_value_to_key_range(cx, query, None);

        // Step 6. Let operation be an algorithm to run count the records in a range with index
        // and range.
        // Step 7. Return the result (an IDBRequest) of running asynchronously execute a request
        // with this and operation.
        serialized_query.and_then(|q| {
            IDBRequest::execute_async_from_source(
                cx,
                &self.object_store,
                RequestSource::Index(Dom::from_ref(self)),
                self.operation_context(),
                |callback| {
                    AsyncOperation::ReadOnly(AsyncReadOnlyOperation::Count {
                        callback,
                        key_range: q,
                    })
                },
                None,
                None,
            )
        })
    }

    /// <https://www.w3.org/TR/IndexedDB-3/#dom-idbindex-opencursor>
    fn OpenCursor(
        &self,
        cx: &mut JSContext,
        query: HandleValue,
        direction: IDBCursorDirection,
    ) -> Fallible<DomRoot<IDBRequest>> {
        self.open_cursor(cx, query, direction, false)
    }

    /// <https://www.w3.org/TR/IndexedDB-3/#dom-idbindex-openkeycursor>
    fn OpenKeyCursor(
        &self,
        cx: &mut JSContext,
        query: HandleValue,
        direction: IDBCursorDirection,
    ) -> Fallible<DomRoot<IDBRequest>> {
        self.open_cursor(cx, query, direction, true)
    }
}
