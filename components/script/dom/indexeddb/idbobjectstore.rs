/* This Source Code Form is subject to the terms of the Mozilla Public
 * License, v. 2.0. If a copy of the MPL was not distributed with this
 * file, You can obtain one at https://mozilla.org/MPL/2.0/. */
use std::collections::HashMap;

use dom_struct::dom_struct;
use js::context::JSContext;
use js::conversions::ToJSValConvertible;
use js::gc::MutableHandleValue;
use js::jsapi::Heap;
use js::jsval::{JSVal, NullValue};
use js::rust::HandleValue;
use js::rust::wrappers2::JS_ClearPendingException;
use script_bindings::cell::DomRefCell;
use script_bindings::codegen::GenericBindings::IDBObjectStoreBinding::IDBIndexParameters;
use script_bindings::codegen::GenericUnionTypes::StringOrStringSequence;
use script_bindings::error::ErrorResult;
use script_bindings::reflector::{Reflector, reflect_dom_object_with_cx};
use storage_traits::indexeddb::{
    self, AsyncOperation, AsyncReadOnlyOperation, AsyncReadWriteOperation, AsyncSchemaOperation,
    BackfillIndexResult, IndexBackfillEntry, IndexedDBKeyRange, IndexedDBKeyType,
    IndexedDBRecord, IndexedDBThreadMsg, KvsIndexUpdate, KvsOperationContext, KvsOperationTarget,
    RecordKeyPlacement,
    RecordsShape,
};

use crate::dom::bindings::codegen::Bindings::IDBCursorBinding::IDBCursorDirection;
use crate::dom::bindings::codegen::Bindings::IDBDatabaseBinding::IDBObjectStoreParameters;
use crate::dom::bindings::codegen::Bindings::IDBObjectStoreBinding::{
    IDBGetAllOptions, IDBObjectStoreMethods,
};
use crate::dom::bindings::codegen::Bindings::IDBTransactionBinding::{
    IDBTransactionMethods, IDBTransactionMode,
};
// We need to alias this name, otherwise test-tidy complains at &String reference.
use crate::dom::bindings::codegen::UnionTypes::StringOrStringSequence as StrOrStringSequence;
use crate::dom::bindings::error::{Error, Fallible};
use crate::dom::bindings::refcounted::Trusted;
use crate::dom::bindings::reflector::DomGlobal;
use crate::dom::bindings::root::{Dom, DomRoot};
use crate::dom::bindings::str::DOMString;
use crate::dom::bindings::structuredclone;
use crate::dom::bindings::trace::RootedTraceableBox;
use crate::dom::domstringlist::DOMStringList;
use crate::dom::globalscope::GlobalScope;
use crate::dom::indexeddb::idbcursor::{IDBCursor, IterationParam, ObjectStoreOrIndex};
use crate::dom::indexeddb::idbcursorwithvalue::IDBCursorWithValue;
use crate::dom::indexeddb::idbindex::IDBIndex;
use crate::dom::indexeddb::idbrequest::{
    BackfillHalf, GetAllKind, GetAllRequest, IDBRequest, RecordsParam, RequestSource,
};
use crate::dom::indexeddb::idbtransaction::IDBTransaction;
use crate::indexeddb::{
    ExtractionResult, can_inject_key_into_value, convert_value_to_key, convert_value_to_key_range,
    extract_key, inject_key_into_value, is_valid_key_path,
};

#[derive(Clone, JSTraceable, MallocSizeOf)]
pub enum KeyPath {
    String(DOMString),
    StringSequence(Vec<DOMString>),
}

impl From<StringOrStringSequence> for KeyPath {
    fn from(value: StringOrStringSequence) -> Self {
        match value {
            StringOrStringSequence::String(s) => KeyPath::String(s),
            StringOrStringSequence::StringSequence(ss) => KeyPath::StringSequence(ss),
        }
    }
}

impl From<indexeddb::KeyPath> for KeyPath {
    fn from(value: indexeddb::KeyPath) -> Self {
        match value {
            indexeddb::KeyPath::String(string) => KeyPath::String(string.into()),
            indexeddb::KeyPath::Sequence(ss) => {
                KeyPath::StringSequence(ss.into_iter().map(Into::into).collect())
            },
        }
    }
}

impl From<KeyPath> for indexeddb::KeyPath {
    fn from(item: KeyPath) -> Self {
        match item {
            KeyPath::String(s) => Self::String(String::from(s)),
            KeyPath::StringSequence(ss) => {
                Self::Sequence(ss.into_iter().map(String::from).collect())
            },
        }
    }
}

#[derive(Clone, JSTraceable, MallocSizeOf)]
struct IDBObjectStoreRollbackState {
    newly_created_during_transaction: bool,
    rollback_name: Option<DOMString>,
    #[no_trace]
    rollback_indexes: Vec<indexeddb::IndexedDBIndex>,
}

/// How far an index's key path reaches into the store's key path, for a record whose key the
/// engine has not generated yet.
///
/// `Untouched` is the ordinary case: the value carries everything the index needs. The other
/// three say the engine has to finish the key, or that the record earns no entry at all.
enum RecordKeyReach {
    /// The index's key path does not name the store's key path.
    Untouched,
    /// The index's key is the record's key.
    WholeKey,
    /// The index's key is a sequence, with the record's key at each `None`.
    InSequence(Vec<Option<IndexedDBKeyType>>),
    /// The index's key path reaches the store's key path, but another component of the sequence
    /// did not evaluate to a valid key, so the record earns no entry in this index.
    Dropped,
}

#[dom_struct]
pub struct IDBObjectStore {
    reflector_: Reflector,
    name: DomRefCell<DOMString>,
    key_path: Option<KeyPath>,
    index_set: DomRefCell<HashMap<DOMString, Dom<IDBIndex>>>,
    abort_state_on_abort: DomRefCell<Option<IDBObjectStoreRollbackState>>,
    transaction: Dom<IDBTransaction>,
    has_key_generator: bool,
    /// `keyPath` converted to a value, kept so the attribute hands back the same object
    /// every time it is read. A store's key path never changes, so this is written once.
    #[ignore_malloc_size_of = "mozjs"]
    cached_key_path: DomRefCell<Option<Heap<JSVal>>>,

    // We store the db name in the object store to address backend operations
    // that are keyed by (origin, database name, object store name).
    db_name: DOMString,
}

pub(crate) struct IDBObjectStoreAbortState {
    pub(crate) newly_created_during_transaction: bool,
    pub(crate) rollback_indexes_on_abort: Vec<indexeddb::IndexedDBIndex>,
}

impl IDBObjectStore {
    pub fn new_inherited(
        db_name: DOMString,
        name: DOMString,
        options: Option<&IDBObjectStoreParameters>,
        abort_state: IDBObjectStoreAbortState,
        transaction: &IDBTransaction,
    ) -> IDBObjectStore {
        let key_path: Option<KeyPath> = match options {
            Some(options) => options.keyPath.as_ref().map(|path| match path {
                StrOrStringSequence::String(inner) => KeyPath::String(inner.clone()),
                StrOrStringSequence::StringSequence(inner) => {
                    KeyPath::StringSequence(inner.clone())
                },
            }),
            None => None,
        };
        let has_key_generator = options.is_some_and(|options| options.autoIncrement);
        let IDBObjectStoreAbortState {
            newly_created_during_transaction,
            rollback_indexes_on_abort,
        } = abort_state;

        IDBObjectStore {
            reflector_: Reflector::new(),
            name: DomRefCell::new(name),
            key_path,
            index_set: DomRefCell::new(HashMap::new()),
            abort_state_on_abort: DomRefCell::new(Some(IDBObjectStoreRollbackState {
                newly_created_during_transaction,
                rollback_name: None,
                rollback_indexes: rollback_indexes_on_abort,
            })),
            transaction: Dom::from_ref(transaction),
            has_key_generator,
            cached_key_path: DomRefCell::new(None),
            db_name,
        }
    }

    pub fn new(
        cx: &mut JSContext,
        global: &GlobalScope,
        db_name: DOMString,
        name: DOMString,
        options: Option<&IDBObjectStoreParameters>,
        abort_state: IDBObjectStoreAbortState,
        transaction: &IDBTransaction,
    ) -> DomRoot<IDBObjectStore> {
        reflect_dom_object_with_cx(
            Box::new(IDBObjectStore::new_inherited(
                db_name,
                name,
                options,
                abort_state,
                transaction,
            )),
            global,
            cx,
        )
    }

    pub fn get_name(&self) -> DOMString {
        self.name.borrow().clone()
    }

    /// <https://w3c.github.io/IndexedDB/#abort-an-upgrade-transaction>
    pub(crate) fn restore_metadata_after_abort(&self, cx: &mut JSContext) {
        let Some(abort_state) = self.abort_state_on_abort.borrow().as_ref().cloned() else {
            return;
        };

        // Step 5.1. If handle’s object store was not newly created during transaction,
        // set handle’s name to its object store’s name.
        if !abort_state.newly_created_during_transaction &&
            let Some(name) = abort_state.rollback_name
        {
            *self.name.borrow_mut() = name;
        }

        // Step 5.2. Set handle’s index set to the set of indexes that reference
        // its object store.
        // Step 6. For each index handle handle associated with transaction, if handle’s
        // index was not newly created during transaction, set handle’s name to its
        // index’s name.
        //
        // The handles script is already holding have to be the ones that come back, so a
        // surviving index keeps its `IDBIndex` object and only gets its name restored. An
        // index created during the transaction leaves the set, and one deleted during it
        // has no handle left, so it is rebuilt from the metadata the store started with.
        let handles = self
            .index_set
            .borrow()
            .values()
            .map(|index| index.as_rooted())
            .collect::<Vec<_>>();
        self.index_set.borrow_mut().clear();
        for handle in handles {
            if handle.was_newly_created_during_transaction() {
                continue;
            }
            let name = handle.restore_name_after_abort();
            self.index_set
                .borrow_mut()
                .insert(name, Dom::from_ref(&*handle));
        }
        for index in abort_state.rollback_indexes {
            let name: DOMString = index.name.clone().into();
            if self.index_set.borrow().contains_key(&name) {
                continue;
            }
            self.add_index(
                cx,
                name,
                &IDBIndexParameters {
                    multiEntry: index.multi_entry,
                    unique: index.unique,
                },
                index.key_path.clone().into(),
                false,
            );
        }

        // The key generator is not restored here. It is durable state that lives in the engine,
        // and `abort a transaction` rolls it back there.
    }

    pub(crate) fn transaction(&self) -> DomRoot<IDBTransaction> {
        self.transaction.as_rooted()
    }

    /// <https://www.w3.org/TR/IndexedDB-3/#clone>
    fn clone_value_in_target_realm(
        &self,
        cx: &mut JSContext,
        value: HandleValue,
        clone: MutableHandleValue<'_>,
    ) -> Fallible<()> {
        // Step 1. Assert: transaction's state is active.
        //
        // Step 2 below makes the transaction inactive and step 5 makes it active again, which
        // is what the clone needs; a transaction that was already inactive is simply left
        // active afterwards, and the release build has always run the clone either way.
        if !self.transaction.is_active() {
            warn!("Cloning a value for an object store whose transaction is not active.");
        }

        // Step 2. Set transaction's state to inactive.
        //
        // NOTE: The transaction is made inactive so that getters or other side
        // effects triggered by the cloning operation are unable to make
        // additional requests against the transaction.
        self.transaction.set_active_flag(false);

        let result = (|| {
            // Step 3. Let serialized be ? StructuredSerializeForStorage(value).
            let serialized = structuredclone::write(cx, value, None)?;

            // Step 4. Let clone be ? StructuredDeserialize(serialized, targetRealm).
            let _ = structuredclone::read(cx, &self.global(), serialized, clone)?;
            Ok(())
        })();

        // Step 5. Set transaction's state to active.
        self.transaction.set_active_flag(true);

        // Step 6. Return clone.
        result
    }

    fn has_key_generator(&self) -> bool {
        self.has_key_generator
    }

    /// Where this index's key path reaches the store's key path, for a record whose key the
    /// engine has not generated yet.
    ///
    /// Only a `String` key path can belong to a store with a key generator; `createObjectStore`
    /// refuses `autoIncrement` beside a sequence key path or an empty one. An index has no such
    /// restriction, so it reaches the store's key path either by naming it outright or by listing
    /// it among the components of a sequence, and `idbobjectstore_createIndex.any.js` builds both.
    #[expect(unsafe_code)]
    fn record_key_reach(
        &self,
        cx: &mut JSContext,
        value: HandleValue,
        index_key_path: &KeyPath,
    ) -> Fallible<RecordKeyReach> {
        let Some(KeyPath::String(store_path)) = self.key_path.as_ref() else {
            return Ok(RecordKeyReach::Untouched);
        };
        let KeyPath::StringSequence(components) = index_key_path else {
            return Ok(match index_key_path {
                KeyPath::String(index_path) if index_path == store_path => {
                    RecordKeyReach::WholeKey
                },
                _ => RecordKeyReach::Untouched,
            });
        };
        if !components.iter().any(|component| component == store_path) {
            return Ok(RecordKeyReach::Untouched);
        }
        // A sequence key path evaluates one component at a time and fails as a whole if any one
        // of them fails, so the components that are not the record's key are extracted here one
        // by one and a single failure takes the record out of this index. `createIndex` refuses
        // `multiEntry` beside a sequence key path, so every component is a plain key.
        let mut extracted = Vec::with_capacity(components.len());
        for component in components {
            if component == store_path {
                extracted.push(None);
                continue;
            }
            let result = extract_key(cx, value, &KeyPath::String(component.clone()), Some(false));
            match result {
                Ok(ExtractionResult::Key(key)) => extracted.push(Some(key)),
                Ok(ExtractionResult::Invalid | ExtractionResult::Failure) => {
                    return Ok(RecordKeyReach::Dropped);
                },
                // An exception thrown while evaluating one component takes the record out of the
                // index the same way a failure does, and the pending exception has to go with it.
                Err(_) => {
                    unsafe { JS_ClearPendingException(cx) };
                    return Ok(RecordKeyReach::Dropped);
                },
            }
        }
        Ok(RecordKeyReach::InSequence(extracted))
    }

    /// Put the record's key back into a value the store never wrote it into.
    ///
    /// A store with a key generator and an in-line key path has its keys generated in the engine,
    /// where there is no JavaScript context to inject one with, so the stored value does not carry
    /// its key. Every path that turns a stored value back into a JavaScript value runs this first.
    ///
    /// Records written before the generator moved to the engine do carry their key. The extraction
    /// below is what tells the two apart, so nothing has to be migrated: a value that answers its
    /// own key path is handed back exactly as it was stored.
    #[expect(unsafe_code)]
    pub(crate) fn inject_record_key_if_absent(
        &self,
        cx: &mut JSContext,
        value: HandleValue,
        key: &IndexedDBKeyType,
    ) -> Fallible<()> {
        let Some(KeyPath::String(key_path)) = self.key_path.as_ref() else {
            return Ok(());
        };
        let key_path = key_path.clone();
        match extract_key(cx, value, &KeyPath::String(key_path.clone()), None) {
            // The value has no key at its key path, so this record was stored without one.
            Ok(ExtractionResult::Failure) => {},
            // The value answers its key path already, or answers it with something that is not a
            // key. Either way the stored value is what script asked for.
            Ok(_) => return Ok(()),
            // A getter on the key path threw. The record is handed back unchanged rather than
            // failing the read, and the pending exception has to go with it or the next
            // JavaScript call on this context would inherit it.
            Err(_) => {
                unsafe { JS_ClearPendingException(cx) };
                return Ok(());
            },
        }
        // A false answer means the value cannot hold a key at this path, which leaves it as it
        // was stored. `put` has already refused the values that could not take one.
        inject_key_into_value(cx, value, key, &key_path)?;
        Ok(())
    }

    /// <https://www.w3.org/TR/IndexedDB-3/#object-store-in-line-keys>
    fn uses_inline_keys(&self) -> bool {
        self.key_path.is_some()
    }

    fn verify_not_deleted(&self) -> ErrorResult {
        let db = self.transaction.Db();
        if !db.object_store_exists(&self.name.borrow()) {
            return Err(Error::InvalidState(None));
        }
        Ok(())
    }

    /// Checks if the transaction is active, throwing a "TransactionInactiveError" DOMException if not.
    fn check_transaction_active(&self) -> Fallible<()> {
        // Let transaction be this object store handle's transaction.
        let transaction = &self.transaction;

        // If transaction is not active, throw a "TransactionInactiveError" DOMException.
        // https://w3c.github.io/IndexedDB/#transaction-inactive
        // A transaction is in this state after control returns to the event loop after its creation, and when events are not being dispatched.
        // No requests can be made against the transaction when it is in this state.
        if !transaction.is_active() || !transaction.is_usable() {
            return Err(Error::TransactionInactive(None));
        }

        Ok(())
    }

    /// Checks if the transaction is active, throwing a "TransactionInactiveError" DOMException if not.
    /// it then checks if the transaction is a read-only transaction, throwing a "ReadOnlyError" DOMException if so.
    fn check_readwrite_transaction_active(&self) -> Fallible<()> {
        // Let transaction be this object store handle's transaction.
        let transaction = &self.transaction;

        // If transaction is not active, throw a "TransactionInactiveError" DOMException.
        self.check_transaction_active()?;

        if let IDBTransactionMode::Readonly = transaction.get_mode() {
            return Err(Error::ReadOnly(None));
        }
        Ok(())
    }

    /// <https://w3c.github.io/IndexedDB/#dom-idbobjectstore-createindex>
    /// Step 12's operation, first half: read back every record the store already holds.
    ///
    /// A new index has to hold a record for each of them, and the index key comes out of the
    /// JavaScript value the key path is evaluated against. Only the script thread holds that
    /// value, so the records make a round trip: out through this read, back through
    /// [`Self::finish_index_backfill`] as index keys.
    ///
    /// `store_name` is the name the backend knows the store by. It is passed in rather than
    /// read from the handle, because the round trip outlives the call that started it and a
    /// rename placed in the same upgrade transaction changes the handle's name while the
    /// backend is still holding the old one.
    pub(crate) fn start_index_backfill(
        &self,
        cx: &mut JSContext,
        store_name: &str,
        index_name: &str,
        key_path: &indexeddb::KeyPath,
        multi_entry: bool,
    ) -> Fallible<()> {
        IDBRequest::execute_backfill_operation::<Vec<IndexedDBRecord>, _>(
            cx,
            self,
            store_name,
            BackfillHalf::Read,
            |callback| {
                AsyncOperation::ReadOnly(AsyncReadOnlyOperation::Iterate {
                    callback,
                    key_range: IndexedDBKeyRange::default(),
                    count: None,
                    shape: RecordsShape::WithValues,
                })
            },
            Some(RecordsParam::IndexBackfill {
                store_name: store_name.to_owned(),
                index_name: index_name.to_owned(),
                key_path: key_path.clone(),
                multi_entry,
            }),
        )?;
        Ok(())
    }

    /// <https://w3c.github.io/IndexedDB/#dom-idbobjectstore-createindex>
    /// Step 12's operation, second half: evaluate the index's key path against each record and
    /// send the extracted keys out as the write that populates the index.
    ///
    /// The uniqueness check stays in the backend, which is where the index records land and the
    /// only place that can see a collision with a row that arrived some other way.
    #[expect(unsafe_code)]
    pub(crate) fn finish_index_backfill(
        &self,
        cx: &mut JSContext,
        store_name: &str,
        index_name: &str,
        key_path: &indexeddb::KeyPath,
        multi_entry: bool,
        records: Vec<IndexedDBRecord>,
    ) -> Fallible<()> {
        // The key path and the multiEntry flag arrive with the records rather than being read
        // back off the index set, because a `deleteIndex` placed after the `createIndex` has
        // already taken the index off this handle by the time they come back. The write still
        // belongs ahead of that delete on the wire, and the backend drops the records with the
        // index when the delete reaches it.
        let key_path = KeyPath::from(key_path.clone());
        let global = self.global();
        let mut entries = Vec::with_capacity(records.len());
        for record in records {
            rooted!(&in(cx) let mut value = NullValue());
            let data = postcard::from_bytes(&record.value).map_err(|_| Error::Data(None))?;
            structuredclone::read(cx, &global, data, value.handle_mut())?;
            // A record of a store that generates keys into an in-line key path was stored
            // without its key. The index is built over the value script would be handed, so the
            // key goes back in first, and an index on the store's own key path finds it.
            self.inject_record_key_if_absent(cx, value.handle(), &record.primary_key)?;
            // Step 6 of `store a record into an object store`, run here against a value the
            // store is already holding rather than one being written.
            let extracted = match extract_key(cx, value.handle(), &key_path, Some(multi_entry)) {
                Ok(extracted) => extracted,
                // An exception thrown while extracting an index key takes the record out of
                // the index rather than failing the operation. The pending exception has to go
                // with it, or the next JavaScript call on this context would inherit it.
                Err(_) => {
                    unsafe { JS_ClearPendingException(cx) };
                    continue;
                },
            };
            let keys = match extracted {
                ExtractionResult::Key(IndexedDBKeyType::Array(elements)) if multi_entry => elements,
                ExtractionResult::Key(key) => vec![key],
                ExtractionResult::Invalid | ExtractionResult::Failure => continue,
            };
            entries.push(IndexBackfillEntry {
                primary_key: record.primary_key,
                keys,
            });
        }

        IDBRequest::execute_backfill_operation::<BackfillIndexResult, _>(
            cx,
            self,
            store_name,
            BackfillHalf::Write,
            |callback| {
                AsyncOperation::ReadWrite(AsyncReadWriteOperation::BackfillIndex {
                    callback,
                    index_name: index_name.to_owned(),
                    entries,
                })
            },
            None,
        )?;
        Ok(())
    }

    /// The index records this store's declared indexes produce for `value`, one update per index.
    ///
    /// This is steps 6.1 through 6.4 of "store a record into an object store". Only the script
    /// thread can run them: an index key is extracted by evaluating a key path against the
    /// JavaScript value, which never crosses to the storage thread. The backend receives the
    /// extracted keys and writes the index records from those alone.
    ///
    /// An index whose key path does not evaluate against the value contributes nothing, which is
    /// what makes an index sparse. A multiEntry index contributes one record per distinct element
    /// of its extracted array key; every other index contributes one record. Uniqueness is left
    /// to the backend, which is the only place that can see the records already stored.
    ///
    /// `key_comes_from_the_generator` says the engine has not chosen this record's key yet, so
    /// the value does not carry it. An index that reaches the store's key path indexes the record
    /// under that key, so extracting it from this value fails. Those indexes carry a
    /// `RecordKeyPlacement` rather than a key, and the engine fills the hole once it has
    /// generated one. `record_key_reach` is what tells the cases apart.
    #[expect(unsafe_code)]
    fn extract_index_updates(
        &self,
        cx: &mut JSContext,
        value: HandleValue,
        key_comes_from_the_generator: bool,
    ) -> Fallible<Vec<KvsIndexUpdate>> {
        // Extraction runs JavaScript getters, which can reenter this object store, so the index
        // set is snapshotted here rather than held borrowed across any of it.
        let indexes = self
            .index_set
            .borrow()
            .values()
            .map(|index| {
                (
                    index.index_name(),
                    index.index_key_path().clone(),
                    index.is_multi_entry(),
                )
            })
            .collect::<Vec<_>>();
        let mut updates = Vec::with_capacity(indexes.len());
        for (index_name, key_path, multi_entry) in indexes {
            // An index that reaches the store's key path indexes the record under a key that is
            // not in this value, so it is answered before extraction rather than by it.
            let reach = if key_comes_from_the_generator {
                self.record_key_reach(cx, value, &key_path)?
            } else {
                RecordKeyReach::Untouched
            };
            let placement = match reach {
                // Step 6.2. A component that did not evaluate to a valid key takes the record out
                // of this index, the same as any other extraction failure.
                RecordKeyReach::Dropped => continue,
                RecordKeyReach::WholeKey => Some(RecordKeyPlacement::WholeKey),
                RecordKeyReach::InSequence(components) => {
                    Some(RecordKeyPlacement::InSequence(components))
                },
                RecordKeyReach::Untouched => None,
            };
            if let Some(placement) = placement {
                updates.push(KvsIndexUpdate {
                    index_name,
                    keys: Vec::new(),
                    record_key_placement: Some(placement),
                });
                continue;
            }
            // Step 6.1. Let index key be the result of extracting a key from a value using a key
            // path with value, index's key path, and index's multiEntry flag.
            let extracted = match extract_key(cx, value, &key_path, Some(multi_entry)) {
                Ok(extracted) => extracted,
                // Step 6.2. An exception thrown while extracting an index key takes the record
                // out of that index rather than failing the put, so it is discarded here. The
                // pending exception has to go with it, or the next JavaScript call on this
                // context would inherit it.
                Err(_) => {
                    unsafe { JS_ClearPendingException(cx) };
                    continue;
                },
            };
            let keys = match extracted {
                // Step 6.4. A multiEntry index whose key is an array key stores one record per
                // element; `convert_value_to_multientry_key` has already dropped the duplicates.
                ExtractionResult::Key(IndexedDBKeyType::Array(elements)) if multi_entry => elements,
                // Step 6.3. Otherwise the whole extracted key is the one index key.
                ExtractionResult::Key(key) => vec![key],
                // Step 6.2. Invalid or failure leaves the record out of this index.
                ExtractionResult::Invalid | ExtractionResult::Failure => continue,
            };
            updates.push(KvsIndexUpdate {
                index_name,
                keys,
                record_key_placement: None,
            });
        }
        Ok(updates)
    }

    /// <https://w3c.github.io/IndexedDB/#store-a-record-into-an-object-store>
    ///
    /// The cursor write path. `IDBCursor.update` has already decided the key: it is the
    /// cursor's effective key, and a record is already stored under it. None of the key
    /// generator, out-of-line key, or key injection machinery in `put` applies, which is why
    /// this is a sibling of `put` rather than another flag through it. What does apply is the
    /// clone, the in-line key path equality check, and index record extraction.
    pub(crate) fn store_record_with_known_key(
        &self,
        cx: &mut JSContext,
        source: RequestSource,
        value: HandleValue,
        key: &IndexedDBKeyType,
    ) -> Fallible<DomRoot<IDBRequest>> {
        // update() Step 8. Let clone be a clone of value in targetRealm during transaction.
        rooted!(&in(cx) let mut cloned_js_value = NullValue());
        self.clone_value_in_target_realm(cx, value, cloned_js_value.handle_mut())?;

        // update() Step 9. If the effective object store uses in-line keys, then the key
        // extracted from the clone has to equal the key the record is stored under. A record
        // cannot be moved by rewriting its own key.
        if let Some(key_path) = self.key_path.as_ref() {
            match extract_key(cx, cloned_js_value.handle(), key_path, None)? {
                ExtractionResult::Key(extracted_key) if &extracted_key == key => {},
                _ => return Err(Error::Data(None)),
            }
        }

        let cloned_value = structuredclone::write(cx, cloned_js_value.handle(), None)?;
        let Ok(serialized_value) = postcard::to_stdvec(&cloned_value) else {
            return Err(Error::InvalidState(None));
        };

        // Storing a record also rebuilds its index records, so they are extracted from the
        // finished clone and travel with the operation, exactly as they do for `put`.
        // The cursor is positioned on a record that already has a key, so no index key waits on
        // the generator here.
        let index_updates = self.extract_index_updates(cx, cloned_js_value.handle(), false)?;
        IDBRequest::execute_async_from_source(
            cx,
            self,
            source,
            KvsOperationContext {
                target: KvsOperationTarget::ObjectStore,
                index_updates,
            },
            |callback| {
                AsyncOperation::ReadWrite(AsyncReadWriteOperation::PutItem {
                    callback,
                    key: Some(key.clone()),
                    value: serialized_value,
                    should_overwrite: true,
                })
            },
            None,
            None,
        )
    }

    /// <https://w3c.github.io/IndexedDB/#delete-records-from-an-object-store>
    ///
    /// The cursor delete path. The range is the one effective key the cursor is positioned on,
    /// so there is no query value to convert and nothing left to reject.
    pub(crate) fn delete_record_with_known_key(
        &self,
        cx: &mut JSContext,
        source: RequestSource,
        key: &IndexedDBKeyType,
    ) -> Fallible<DomRoot<IDBRequest>> {
        IDBRequest::execute_async_from_source(
            cx,
            self,
            source,
            KvsOperationContext::default(),
            |callback| {
                AsyncOperation::ReadWrite(AsyncReadWriteOperation::RemoveItem {
                    callback,
                    key_range: IndexedDBKeyRange::only(key.clone()),
                })
            },
            None,
            None,
        )
    }

    /// <https://www.w3.org/TR/IndexedDB-3/#add-or-put>
    fn put(
        &self,
        cx: &mut JSContext,
        value: HandleValue,
        key: HandleValue,
        no_overwrite: bool,
    ) -> Fallible<DomRoot<IDBRequest>> {
        // Step 1. Let transaction be handle’s transaction.
        // Step 2. Let store be handle’s object store.
        // Step 3. If store has been deleted, throw an "InvalidStateError" DOMException.
        self.verify_not_deleted()?;

        // Step 4. If transaction’s state is not active, then throw a "TransactionInactiveError" DOMException.
        // Step 5. If transaction is a read-only transaction, throw a "ReadOnlyError" DOMException.
        self.check_readwrite_transaction_active()?;

        // Step 6. If store uses in-line keys and key was given, throw a "DataError"
        // DOMException.
        if !key.is_undefined() && self.uses_inline_keys() {
            return Err(Error::Data(None));
        }

        // Step 7. If store uses out-of-line keys and has no key generator and key
        // was not given, throw a "DataError" DOMException.
        if !self.uses_inline_keys() && !self.has_key_generator() && key.is_undefined() {
            return Err(Error::Data(None));
        }

        // Step 8. If key was given, then:
        let mut serialized_key = None;
        // Whether the engine is the one that decides this record's key.
        let mut key_comes_from_the_generator = false;

        if !key.is_undefined() {
            // Step 8.1. Let r be the result of converting a value to a key with key.
            // Rethrow any exceptions.
            let key = convert_value_to_key(cx, key, None)?.into_result()?;
            // Step 8.2. If r is "invalid value" or "invalid type", throw a
            // "DataError" DOMException.
            // Handled by `into_result()` above.
            // Step 8.3. Let key be r.
            //
            // `possibly update the key generator` is not run here. It runs in the engine when
            // this key arrives, against the durable generator rather than against a copy of it.
            serialized_key = Some(key);
        }

        // Step 9. Let targetRealm be a user-agent defined Realm.
        // Step 10. Let clone be a clone of value in targetRealm during transaction.
        // Rethrow any exceptions.
        rooted!(&in(cx) let mut cloned_js_value = NullValue());
        self.clone_value_in_target_realm(cx, value, cloned_js_value.handle_mut())?;

        // Step 11. If store uses in-line keys, then:
        let cloned_value = match self.key_path.as_ref() {
            Some(key_path) => {
                // Step 11.1. Let kpk be the result of extracting a key from a value using a key
                // path with clone and store’s key path. Rethrow any exceptions.
                match extract_key(cx, cloned_js_value.handle(), key_path, None)? {
                    // Step 11.2. If kpk is invalid, throw a "DataError" DOMException.
                    ExtractionResult::Invalid => return Err(Error::Data(None)),
                    // Step 11.3. If kpk is not failure, let key be kpk.
                    ExtractionResult::Key(kpk) => {
                        serialized_key = Some(kpk);
                    },
                    // Step 11.4. Otherwise (kpk is failure):
                    ExtractionResult::Failure => {
                        // Step 11.4.1. If store does not have a key generator, throw a
                        // "DataError" DOMException.
                        if !self.has_key_generator() {
                            return Err(Error::Data(None));
                        }
                        let KeyPath::String(key_path) = key_path else {
                            return Err(Error::Data(None));
                        };
                        // Step 11.4.2. If check that a key could be injected into a value with
                        // clone and store’s key path return false, throw a "DataError"
                        // DOMException.
                        if !can_inject_key_into_value(cx, cloned_js_value.handle(), key_path)? {
                            return Err(Error::Data(None));
                        }

                        // `generate a key` runs in the engine, and the key is therefore not
                        // injected into the clone here.
                        //
                        // It has to run there. The generator is durable state, and
                        // <https://w3c.github.io/IndexedDB/#object-store-key-generator> requires
                        // that an insertion refused by a constraint leave it alone. Only the
                        // engine knows whether the requests queued ahead of this one kept their
                        // keys, and it knows it too late to help here: in
                        // `request-event-ordering-small-values` the refusal is answered six
                        // requests after this one is queued.
                        //
                        // The key is injected on the way back out instead, by
                        // `inject_record_key_if_absent`, which every path that turns a stored
                        // value back into a JavaScript value runs.
                        serialized_key = None;
                        key_comes_from_the_generator = true;
                    },
                }

                structuredclone::write(cx, cloned_js_value.handle(), None)?
            },
            None => structuredclone::write(cx, cloned_js_value.handle(), None)?,
        };
        let Ok(serialized_value) = postcard::to_stdvec(&cloned_value) else {
            return Err(Error::InvalidState(None));
        };
        // Step 12. Let operation be an algorithm to run store a record into an object store with
        // store, clone, key, and no-overwrite flag.
        //
        // Storing a record also builds its index records, so the index keys are extracted from
        // the finished clone, after any generated key has been injected into it, and travel with
        // the operation.
        let index_updates =
            self.extract_index_updates(cx, cloned_js_value.handle(), key_comes_from_the_generator)?;
        let request = IDBRequest::execute_async_with_context(
            cx,
            self,
            KvsOperationContext {
                target: KvsOperationTarget::ObjectStore,
                index_updates,
            },
            |callback| {
                AsyncOperation::ReadWrite(AsyncReadWriteOperation::PutItem {
                    callback,
                    key: serialized_key,
                    value: serialized_value,
                    should_overwrite: !no_overwrite,
                })
            },
            None,
            None,
        )?;
        // Step 13. Return the result (an IDBRequest) of running asynchronously execute a request
        // with handle and operation.
        Ok(request)
    }

    /// <https://www.w3.org/TR/IndexedDB-3/#dom-idbobjectstore-opencursor>
    /// <https://www.w3.org/TR/IndexedDB-3/#dom-idbobjectstore-openkeycursor>
    fn open_cursor(
        &self,
        cx: &mut JSContext,
        query: HandleValue,
        direction: IDBCursorDirection,
        key_only: bool,
    ) -> Fallible<DomRoot<IDBRequest>> {
        // Step 1. Let transaction be this object store handle's transaction.
        // Step 2. Let store be this object store handle's object store.

        // Step 3. If store has been deleted, throw an "InvalidStateError" DOMException.
        self.verify_not_deleted()?;

        // Step 4. If transaction is not active, throw a "TransactionInactiveError" DOMException.
        self.check_transaction_active()?;

        // Step 5. Let range be the result of running the steps to convert a value to a key range
        // with query. Rethrow any exceptions.
        //
        // The query parameter may be a key or an IDBKeyRange to use as the cursor's range. If null
        // or not given, an unbounded key range is used.
        let range = convert_value_to_key_range(cx, query, Some(false))?;

        // Step 6. Let cursor be a new cursor with transaction set to transaction, an undefined
        // position, direction set to direction, got value flag unset, and undefined key and value.
        // The source of cursor is store. The range of cursor is range.
        //
        // NOTE: A cursor that has the key only flag unset implements the IDBCursorWithValue
        // interface as well.
        let cursor = if key_only {
            IDBCursor::new(
                cx,
                &self.global(),
                &self.transaction,
                direction,
                false,
                ObjectStoreOrIndex::ObjectStore(Dom::from_ref(self)),
                range.clone(),
                key_only,
            )
        } else {
            DomRoot::upcast(IDBCursorWithValue::new(
                cx,
                &self.global(),
                &self.transaction,
                direction,
                false,
                ObjectStoreOrIndex::ObjectStore(Dom::from_ref(self)),
                range.clone(),
                key_only,
            ))
        };

        // Step 7. Run the steps to asynchronously execute a request and return the IDBRequest
        // created by these steps. The steps are run with this object store handle as source and
        // the steps to iterate a cursor as operation, using the current Realm as targetRealm, and
        // cursor.
        let iteration_param = IterationParam {
            cursor: Trusted::new(&cursor),
            key: None,
            primary_key: None,
            count: None,
        };

        IDBRequest::execute_async(
            cx,
            self,
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

    pub(crate) fn add_index(
        &self,
        cx: &mut JSContext,
        name: DOMString,
        options: &IDBIndexParameters,
        key_path: KeyPath,
        newly_created_during_transaction: bool,
    ) -> DomRoot<IDBIndex> {
        let index = IDBIndex::new(
            cx,
            &self.global(),
            self,
            name.clone(),
            options.multiEntry,
            options.unique,
            key_path,
            newly_created_during_transaction,
        );
        self.index_set
            .borrow_mut()
            .insert(name, Dom::from_ref(&index));
        index
    }

    /// <https://w3c.github.io/IndexedDB/#dom-idbdatabase-deleteobjectstore>
    /// Step 6: remove every entry from the handle's index set. An aborted upgrade puts
    /// them back through `restore_metadata_after_abort`.
    pub(crate) fn clear_index_set(&self) {
        self.index_set.borrow_mut().clear();
    }

    pub(crate) fn has_index(&self, name: &DOMString) -> bool {
        self.index_set.borrow().contains_key(name)
    }

    /// The caller must ensure that the original index exists.
    pub(crate) fn rename_index(&self, name: &DOMString, new_name: &DOMString) -> ErrorResult {
        let index = self
            .index_set
            .borrow()
            .get(name)
            .map(|index| index.as_rooted())
            .ok_or_else(|| {
                warn!("rename_index called for an index that is not in the index set");
                Error::InvalidState(Some(
                    "The index to rename is no longer in its object store".to_owned(),
                ))
            })?;
        let operation = AsyncSchemaOperation::RenameIndex {
            callback: self.transaction.create_abort_callback()?,
            index_name: name.to_string(),
            new_name: new_name.to_string(),
        };
        self.transaction
            .send_or_hold(IndexedDBThreadMsg::AsyncSchemaOperation {
                origin: self.global().origin().immutable().clone(),
                database_name: self.db_name.to_string(),
                store_name: self.name.borrow().clone().into(),
                operation,
                transaction_serial_number: self.transaction.get_serial_number(),
            })
            .map_err(|()| {
                warn!("Could not send RenameIndex to the IndexedDB backend");
                Error::Operation(Some("Could not send the rename index operation".to_owned()))
            })?;

        // We also need to update the key in the index set.
        if self.index_set.borrow_mut().remove(name).is_none() {
            warn!("The index disappeared while its backend rename was being queued");
        }
        self.index_set
            .borrow_mut()
            .insert(new_name.clone(), Dom::from_ref(&index));
        Ok(())
    }
}

impl IDBObjectStoreMethods<crate::DomTypeHolder> for IDBObjectStore {
    /// <https://www.w3.org/TR/IndexedDB-3/#dom-idbobjectstore-put>
    fn Put(
        &self,
        cx: &mut JSContext,
        value: HandleValue,
        key: HandleValue,
    ) -> Fallible<DomRoot<IDBRequest>> {
        // Step 1. Return the result of running add or put with this, value, key and the
        // no-overwrite flag false.
        self.put(cx, value, key, false)
    }

    /// <https://www.w3.org/TR/IndexedDB-3/#dom-idbobjectstore-add>
    fn Add(
        &self,
        cx: &mut JSContext,
        value: HandleValue,
        key: HandleValue,
    ) -> Fallible<DomRoot<IDBRequest>> {
        // Step 1. Return the result of running add or put with this, value, key and the
        // no-overwrite flag true.
        self.put(cx, value, key, true)
    }

    /// <https://www.w3.org/TR/IndexedDB-3/#dom-idbobjectstore-delete>
    fn Delete(&self, cx: &mut JSContext, query: HandleValue) -> Fallible<DomRoot<IDBRequest>> {
        // Step 1. Let transaction be this’s transaction.
        // Step 2. Let store be this's object store.
        // Step 3. If store has been deleted, throw an "InvalidStateError" DOMException.
        self.verify_not_deleted()?;

        // Step 4. If transaction’s state is not active, then throw a "TransactionInactiveError" DOMException.
        // Step 5. If transaction is a read-only transaction, throw a "ReadOnlyError" DOMException.
        self.check_readwrite_transaction_active()?;

        // Step 6. Let range be the result of running the steps to convert a value to a key range with query and null disallowed flag set. Rethrow any exceptions.
        let serialized_query = convert_value_to_key_range(cx, query, Some(true));
        // Step 7. Let operation be an algorithm to run delete records from an object store with store and range.
        // Step 8. Return the result (an IDBRequest) of running asynchronously execute a request with this and operation.
        serialized_query.and_then(|key_range| {
            IDBRequest::execute_async(
                cx,
                self,
                |callback| {
                    AsyncOperation::ReadWrite(AsyncReadWriteOperation::RemoveItem {
                        callback,
                        key_range,
                    })
                },
                None,
                None,
            )
        })
    }

    /// <https://www.w3.org/TR/IndexedDB-3/#dom-idbobjectstore-clear>
    fn Clear(&self, cx: &mut JSContext) -> Fallible<DomRoot<IDBRequest>> {
        // Step 1. Let transaction be this’s transaction.
        // Step 2. Let store be this's object store.
        // Step 3. If store has been deleted, throw an "InvalidStateError" DOMException.
        self.verify_not_deleted()?;

        // Step 4. If transaction’s state is not active, then throw a "TransactionInactiveError" DOMException.
        // Step 5. If transaction is a read-only transaction, throw a "ReadOnlyError" DOMException.
        self.check_readwrite_transaction_active()?;

        // Step 6. Let operation be an algorithm to run clear an object store with store.
        // Step 7. Return the result (an IDBRequest) of running asynchronously execute a request with this and operation.
        IDBRequest::execute_async(
            cx,
            self,
            |callback| AsyncOperation::ReadWrite(AsyncReadWriteOperation::Clear(callback)),
            None,
            None,
        )
    }

    /// <https://www.w3.org/TR/IndexedDB-3/#dom-idbobjectstore-get>
    fn Get(&self, cx: &mut JSContext, query: HandleValue) -> Fallible<DomRoot<IDBRequest>> {
        // Step 1. Let transaction be this’s transaction.
        // Step 2. Let store be this's object store.
        // Step 3. If store has been deleted, throw an "InvalidStateError" DOMException.
        self.verify_not_deleted()?;

        // Step 4. If transaction’s state is not active, then throw a "TransactionInactiveError" DOMException.
        self.check_transaction_active()?;

        // Step 5. Let range be the result of converting a value to a key range with query and true. Rethrow any exceptions.
        let serialized_query = convert_value_to_key_range(cx, query, Some(true));

        // Step 6. Let operation be an algorithm to run retrieve a value from an object store with the current Realm record, store, and range.
        // Step 7. Return the result (an IDBRequest) of running asynchronously execute a request with this and operation.
        serialized_query.and_then(|q| {
            IDBRequest::execute_async(
                cx,
                self,
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

    /// <https://www.w3.org/TR/IndexedDB-3/#dom-idbobjectstore-getkey>
    fn GetKey(&self, cx: &mut JSContext, query: HandleValue) -> Result<DomRoot<IDBRequest>, Error> {
        // Step 1. Let transaction be this’s transaction.
        // Step 2. Let store be this's object store.
        // Step 3. If store has been deleted, throw an "InvalidStateError" DOMException.
        self.verify_not_deleted()?;

        // Step 4. If transaction’s state is not active, then throw a "TransactionInactiveError" DOMException.
        self.check_transaction_active()?;

        // Step 5. Let range be the result of converting a value to a key range with query and true. Rethrow any exceptions.
        let serialized_query = convert_value_to_key_range(cx, query, Some(true));

        // Step 6. Run the steps to asynchronously execute a request and return the IDBRequest created by these steps.
        // The steps are run with this object store handle as source and the steps to retrieve a key from an object
        // store as operation, using store and range.
        serialized_query.and_then(|q| {
            IDBRequest::execute_async(
                cx,
                self,
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

    /// <https://www.w3.org/TR/IndexedDB-3/#dom-idbobjectstore-getall>
    fn GetAll(
        &self,
        cx: &mut JSContext,
        query_or_options: HandleValue,
        count: Option<u32>,
    ) -> Fallible<DomRoot<IDBRequest>> {
        // Step 1. Let transaction be this's transaction.
        // Step 2. Let store be this's object store.
        // Step 3. If store has been deleted, throw an "InvalidStateError" DOMException.
        self.verify_not_deleted()?;

        // Step 4. If transaction's state is not active, then throw a
        // "TransactionInactiveError" DOMException.
        self.check_transaction_active()?;

        // Steps 6 to 9. Resolve the range, the direction and the count the read may apply.
        let request = GetAllRequest::resolve(cx, GetAllKind::Values, query_or_options, count)?;

        // Step 10 to 13. Run retrieve multiple records from an object store as the operation of
        // an asynchronously executed request.
        IDBRequest::execute_async(
            cx,
            self,
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

    /// <https://www.w3.org/TR/IndexedDB-3/#dom-idbobjectstore-getallrecords>
    fn GetAllRecords(
        &self,
        cx: &mut JSContext,
        options: RootedTraceableBox<IDBGetAllOptions>,
    ) -> Fallible<DomRoot<IDBRequest>> {
        // Step 1. Let transaction be this's transaction.
        // Step 2. Let store be this's object store.
        // Step 3. If store has been deleted, throw an "InvalidStateError" DOMException.
        self.verify_not_deleted()?;

        // Step 4. If transaction's state is not active, then throw a
        // "TransactionInactiveError" DOMException.
        self.check_transaction_active()?;

        // Steps 6 to 9. Resolve the range, the direction and the count the read may apply.
        let request = GetAllRequest::from_options(cx, GetAllKind::Records, &options)?;

        // Step 10 to 13. Run retrieve multiple records from an object store as the operation of
        // an asynchronously executed request.
        IDBRequest::execute_async(
            cx,
            self,
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

    /// <https://www.w3.org/TR/IndexedDB-3/#dom-idbobjectstore-getallkeys>
    fn GetAllKeys(
        &self,
        cx: &mut JSContext,
        query_or_options: HandleValue,
        count: Option<u32>,
    ) -> Fallible<DomRoot<IDBRequest>> {
        // Step 1. Let transaction be this's transaction.
        // Step 2. Let store be this's object store.
        // Step 3. If store has been deleted, throw an "InvalidStateError" DOMException.
        self.verify_not_deleted()?;

        // Step 4. If transaction's state is not active, then throw a
        // "TransactionInactiveError" DOMException.
        self.check_transaction_active()?;

        // Steps 6 to 9. Resolve the range, the direction and the count the read may apply.
        let request = GetAllRequest::resolve(cx, GetAllKind::PrimaryKeys, query_or_options, count)?;

        // Step 10 to 13. Run retrieve multiple records from an object store as the operation of
        // an asynchronously executed request.
        IDBRequest::execute_async(
            cx,
            self,
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

    /// <https://www.w3.org/TR/IndexedDB-3/#dom-idbobjectstore-count>
    fn Count(&self, cx: &mut JSContext, query: HandleValue) -> Fallible<DomRoot<IDBRequest>> {
        // Step 1. Let transaction be this’s transaction.
        // Step 2. Let store be this's object store.
        // Step 3. If store has been deleted, throw an "InvalidStateError" DOMException.
        self.verify_not_deleted()?;

        // Step 4. If transaction’s state is not active, then throw a "TransactionInactiveError" DOMException.
        self.check_transaction_active()?;

        // Step 5. Let range be the result of converting a value to a key range with query. Rethrow any exceptions.
        let serialized_query = convert_value_to_key_range(cx, query, None);

        // Step 6. Let operation be an algorithm to run count the records in a range with store and range.
        // Step 7. Return the result (an IDBRequest) of running asynchronously execute a request with this and operation.
        serialized_query.and_then(|q| {
            IDBRequest::execute_async(
                cx,
                self,
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

    /// <https://www.w3.org/TR/IndexedDB-3/#dom-idbobjectstore-opencursor>
    fn OpenCursor(
        &self,
        cx: &mut JSContext,
        query: HandleValue,
        direction: IDBCursorDirection,
    ) -> Fallible<DomRoot<IDBRequest>> {
        self.open_cursor(cx, query, direction, false)
    }

    /// <https://www.w3.org/TR/IndexedDB-3/#dom-idbobjectstore-openkeycursor>
    fn OpenKeyCursor(
        &self,
        cx: &mut JSContext,
        query: HandleValue,
        direction: IDBCursorDirection,
    ) -> Fallible<DomRoot<IDBRequest>> {
        self.open_cursor(cx, query, direction, true)
    }

    /// <https://www.w3.org/TR/IndexedDB-3/#dom-idbobjectstore-name>
    fn Name(&self) -> DOMString {
        self.name.borrow().clone()
    }

    /// <https://www.w3.org/TR/IndexedDB-3/#dom-idbobjectstore-name>
    fn SetName(&self, value: DOMString) -> ErrorResult {
        // Step 1. Let name be the given value.
        let name = value;

        // Step 2. Let transaction be this’s transaction.
        let transaction = &self.transaction;

        // Step 3. Let store be this’s object store.
        // Step 4. If store has been deleted, throw an "InvalidStateError" DOMException.
        self.verify_not_deleted()?;

        // Step 5. If transaction is not an upgrade transaction, throw an "InvalidStateError" DOMException.
        if transaction.Mode() != IDBTransactionMode::Versionchange {
            return Err(Error::InvalidState(None));
        }
        // Step 6. If transaction’s state is not active, throw a "TransactionInactiveError" DOMException.
        self.check_transaction_active()?;

        // Step 7. If store’s name is equal to name, terminate these steps.
        if *self.name.borrow() == name {
            return Ok(());
        }

        // Step 8. If an object store named name already exists in store’s database,
        // throw a "ConstraintError" DOMException.
        if transaction.Db().object_store_exists(&name) {
            return Err(Error::Constraint(None));
        }

        let old_name = self.name.borrow().clone();

        // Step 9. Set store’s name to name.
        // The store also has to be renamed in the backend, which keys a store's rows and
        // every later request against it by name. Without this the rename lived only on
        // the handle, and the first request issued after the upgrade transaction
        // committed failed against a store the backend still held under the old name.
        let operation = AsyncSchemaOperation::RenameObjectStore {
            callback: self.transaction.create_abort_callback()?,
            new_name: name.to_string(),
        };
        self.transaction
            .send_or_hold(IndexedDBThreadMsg::AsyncSchemaOperation {
                origin: self.global().origin().immutable().clone(),
                database_name: self.db_name.to_string(),
                store_name: old_name.to_string(),
                operation,
                transaction_serial_number: self.transaction.get_serial_number(),
            })
            .map_err(|()| {
                warn!("Could not send RenameObjectStore to the IndexedDB backend");
                Error::Operation(Some(
                    "Could not send the rename object store operation".to_owned(),
                ))
            })?;

        if let Some(abort_state) = self.abort_state_on_abort.borrow_mut().as_mut() &&
            abort_state.rollback_name.is_none()
        {
            abort_state.rollback_name = Some(old_name.clone());
        }

        transaction
            .Db()
            .rename_object_store_name(&old_name, name.clone());
        // Step 10. Set this’s name to name.
        *self.name.borrow_mut() = name.clone();
        transaction.rename_object_store_handle_cache(&old_name, &name, self);
        Ok(())
    }

    /// <https://www.w3.org/TR/IndexedDB-3/#dom-idbobjectstore-keypath>
    fn KeyPath(&self, cx: &mut JSContext, mut ret_val: MutableHandleValue) {
        // A sequence key path converts to a fresh Array on every call, so converting on
        // each read would hand script a different object each time it looked. The value is
        // converted once and kept; `idbobjectstore_keyPath.any.js` asserts both halves of
        // that, that one store answers with the same object and that two store handles onto
        // the same store answer with different ones.
        if let Some(cached) = self.cached_key_path.borrow().as_ref() {
            ret_val.set(cached.get());
            return;
        }

        match &self.key_path {
            Some(KeyPath::String(path)) => path.safe_to_jsval(cx, ret_val.reborrow()),
            Some(KeyPath::StringSequence(paths)) => paths.safe_to_jsval(cx, ret_val.reborrow()),
            None => ret_val.set(NullValue()),
        }

        // The `Heap` is stored before it is set: `Heap::set` registers the slot's own
        // address with the GC store buffer, so the value has to be written where it will
        // live rather than moved in afterwards.
        *self.cached_key_path.borrow_mut() = Some(Heap::default());
        if let Some(cached) = self.cached_key_path.borrow().as_ref() {
            cached.set(ret_val.get());
        }
    }

    /// <https://www.w3.org/TR/IndexedDB-3/#dom-idbobjectstore-indexnames>
    fn IndexNames(&self, cx: &mut JSContext) -> DomRoot<DOMStringList> {
        DOMStringList::new_sorted(cx, &self.global(), self.index_set.borrow().keys())
    }

    /// <https://www.w3.org/TR/IndexedDB-3/#dom-idbobjectstore-transaction>
    fn Transaction(&self) -> DomRoot<IDBTransaction> {
        self.transaction()
    }

    /// <https://www.w3.org/TR/IndexedDB-3/#dom-idbobjectstore-autoincrement>
    fn AutoIncrement(&self) -> bool {
        self.has_key_generator()
    }

    /// <https://www.w3.org/TR/IndexedDB-3/#dom-idbobjectstore-createindex>
    fn CreateIndex(
        &self,
        cx: &mut JSContext,
        name: DOMString,
        key_path: StringOrStringSequence,
        options: &IDBIndexParameters,
    ) -> Fallible<DomRoot<IDBIndex>> {
        let key_path: KeyPath = key_path.into();
        // Step 3. If transaction is not an upgrade transaction, throw an "InvalidStateError" DOMException.
        if self.transaction.Mode() != IDBTransactionMode::Versionchange {
            return Err(Error::InvalidState(None));
        }

        // Step 4. If store has been deleted, throw an "InvalidStateError" DOMException.
        self.verify_not_deleted()?;
        // Step 5. If transaction is not active, throw a "TransactionInactiveError" DOMException.
        self.check_transaction_active()?;

        // Step 6. If an index named name already exists in store, throw a "ConstraintError" DOMException.
        if self.has_index(&name) {
            return Err(Error::Constraint(None));
        }

        let js_key_path = match key_path.clone() {
            KeyPath::String(s) => StringOrStringSequence::String(s),
            KeyPath::StringSequence(s) => StringOrStringSequence::StringSequence(s),
        };

        // Step 7. If keyPath is not a valid key path, throw a "SyntaxError" DOMException.
        if !is_valid_key_path(cx, &js_key_path)? {
            return Err(Error::Syntax(None));
        }
        // Step 8. Let unique be set if options’s unique member is true, and unset otherwise.
        // Step 9. Let multiEntry be set if options’s multiEntry member is true, and unset otherwise.
        // Step 10. If keyPath is a sequence and multiEntry is set, throw an "InvalidAccessError" DOMException.
        if matches!(key_path, KeyPath::StringSequence(_)) && options.multiEntry {
            return Err(Error::InvalidAccess(None));
        }

        // Step 11. Let index be a new index in store.
        // Set index’s name to name and key path to keyPath. If unique is set, set index’s unique flag.
        // If multiEntry is set, set index’s multiEntry flag.
        let stored_key_path: indexeddb::KeyPath = key_path.clone().into();
        let operation = AsyncSchemaOperation::CreateIndex {
            callback: self.transaction.create_abort_callback()?,
            index_name: name.to_string(),
            key_path: stored_key_path.clone(),
            unique: options.unique,
            multi_entry: options.multiEntry,
        };

        self.transaction
            .send_or_hold(IndexedDBThreadMsg::AsyncSchemaOperation {
                origin: self.global().origin().immutable().clone(),
                database_name: self.db_name.to_string(),
                store_name: self.name.borrow().clone().into(),
                operation,
                transaction_serial_number: self.transaction.get_serial_number(),
            })
            .map_err(|()| {
                warn!("Could not send CreateIndex to the IndexedDB backend");
                Error::Operation(Some("Could not send the create index operation".to_owned()))
            })?;

        // Step 12. Add index to this object store handle's index set.
        let index = self.add_index(cx, name.clone(), options, key_path, true);

        // Step 11's operation: the index has to hold a record for every record the store
        // already has, and the index keys come out of the stored JavaScript values, so the
        // records make a round trip through the script thread. The read goes out here, in the
        // place in the outbound queue this `createIndex` occupies, and the transaction then
        // holds everything script places after it: without the hold a later request would
        // reach the backend first and be ordered ahead of the index records, so the index
        // would read as empty right after it was made.
        //
        // The store name is captured now rather than when the round trip finishes. It names
        // the store the backend has, and a rename placed later in this same transaction is
        // held behind the round trip, so the backend still knows the store by this name when
        // the backfill's write lands.
        let store_name = self.name.borrow().to_string();
        let index_name = name.to_string();
        self.start_index_backfill(
            cx,
            &store_name,
            &index_name,
            &stored_key_path,
            index.is_multi_entry(),
        )?;
        self.transaction.hold_outbound_after_backfill();

        // Step 13. Return a new index handle associated with index and this object store handle.
        Ok(index)
    }

    /// <https://www.w3.org/TR/IndexedDB-3/#dom-idbobjectstore-deleteindex>
    fn DeleteIndex(&self, name: DOMString) -> Fallible<()> {
        // Step 3. If transaction is not an upgrade transaction, throw an "InvalidStateError" DOMException.
        if self.transaction.Mode() != IDBTransactionMode::Versionchange {
            return Err(Error::InvalidState(None));
        }
        // Step 4. If store has been deleted, throw an "InvalidStateError" DOMException.
        self.verify_not_deleted()?;
        // Step 5. If transaction is not active, throw a "TransactionInactiveError" DOMException.
        self.check_transaction_active()?;
        // Step 6. Let index be the index named name in store if one exists,
        // or throw a "NotFoundError" DOMException otherwise.
        if !self.index_set.borrow().contains_key(&name) {
            return Err(Error::NotFound(None));
        }
        // Step 8. Destroy index.
        let operation = AsyncSchemaOperation::DeleteIndex {
            callback: self.transaction.create_abort_callback()?,
            index_name: name.to_string(),
        };
        self.transaction
            .send_or_hold(IndexedDBThreadMsg::AsyncSchemaOperation {
                origin: self.global().origin().immutable().clone(),
                database_name: self.db_name.to_string(),
                store_name: self.name.borrow().clone().into(),
                operation,
                transaction_serial_number: self.transaction.get_serial_number(),
            })
            .map_err(|()| {
                warn!("Could not send DeleteIndex to the IndexedDB backend");
                Error::Operation(Some("Could not send the delete index operation".to_owned()))
            })?;

        // Step 7. Remove index from this object store handle's index set only once the backend
        // operation is guaranteed to be queued.
        self.index_set.borrow_mut().retain(|n, _| n != &name);
        Ok(())
    }

    /// <https://w3c.github.io/IndexedDB/#dom-idbobjectstore-index>
    fn Index(&self, name: DOMString) -> Fallible<DomRoot<IDBIndex>> {
        // Step 3. If store has been deleted, throw an "InvalidStateError" DOMException.
        self.verify_not_deleted()?;

        // Step 4. If the transaction's state is finished, then throw an "InvalidStateError" DOMException.
        if self.transaction.is_finished() {
            return Err(Error::InvalidState(None));
        }

        // Step 5. Let index be the index named name in this’s index set if one exists, or throw a "NotFoundError" DOMException otherwise.
        let index_set = self.index_set.borrow();
        let index = index_set.get(&name).ok_or(Error::NotFound(None))?;

        // Step 6. Return an index handle associated with index and this.
        Ok(index.as_rooted())
    }
}
