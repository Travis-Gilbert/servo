/* This Source Code Form is subject to the terms of the Mozilla Public
 * License, v. 2.0. If a copy of the MPL was not distributed with this
 * file, You can obtain one at https://mozilla.org/MPL/2.0/. */

use std::cell::Cell;

use dom_struct::dom_struct;
use js::context::JSContext;
use js::conversions::{ConversionResult as JsConversionResult, ToJSValConvertible};
use js::jsapi::Heap;
use js::jsval::{DoubleValue, JSVal, NullValue, ObjectValue, UndefinedValue};
use js::rust::HandleValue;
use profile_traits::generic_callback::GenericCallback;
use script_bindings::cell::DomRefCell;
use script_bindings::reflector::{DomObject, reflect_dom_object_with_cx};
use serde::{Deserialize, Serialize};
use servo_base::generic_channel::GenericSend;
use storage_traits::indexeddb::{
    AsyncOperation, AsyncReadOnlyOperation, BackendError, BackendResult, BackfillIndexResult,
    IndexedDBKeyRange, IndexedDBKeyType, IndexedDBRecord, IndexedDBThreadMsg, IndexedDBTxnMode,
    KeyPath, KvsOperationContext, PutItemResult, RecordsShape, SyncOperation,
};
use stylo_atoms::Atom;

use crate::dom::bindings::codegen::Bindings::IDBCursorBinding::IDBCursorDirection;
use crate::dom::bindings::codegen::Bindings::IDBObjectStoreBinding::IDBGetAllOptions;
use crate::dom::bindings::codegen::Bindings::IDBRequestBinding::{
    IDBRequestMethods, IDBRequestReadyState,
};
use crate::dom::bindings::codegen::Bindings::IDBTransactionBinding::IDBTransactionMode;
use crate::dom::bindings::codegen::UnionTypes::IDBObjectStoreOrIDBIndexOrIDBCursor;
use crate::dom::bindings::error::{Error, Fallible, create_dom_exception};
use crate::dom::bindings::inheritance::Castable;
use crate::dom::bindings::refcounted::Trusted;
use crate::dom::bindings::reflector::DomGlobal;
use crate::dom::bindings::root::{Dom, DomRoot, MutNullableDom};
use crate::dom::bindings::structuredclone;
use crate::dom::domexception::DOMException;
use crate::dom::event::{Event, EventBubbles, EventCancelable};
use crate::dom::eventtarget::EventTarget;
use crate::dom::globalscope::GlobalScope;
use crate::dom::indexeddb::idbcursor::{IDBCursor, IterationParam, iterate_cursor};
use crate::dom::indexeddb::idbcursorwithvalue::IDBCursorWithValue;
use crate::dom::indexeddb::idbindex::IDBIndex;
use crate::dom::indexeddb::idbobjectstore::IDBObjectStore;
use crate::dom::indexeddb::idbrecord::IDBRecord;
use crate::dom::indexeddb::idbtransaction::IDBTransaction;
use crate::indexeddb::{
    convert_value_to_key_range, is_potentially_valid_key_range, key_type_to_jsval, reply_lost,
};
use crate::realms::enter_auto_realm;

/// <https://www.w3.org/TR/IndexedDB-3/#request-source>
///
/// Every request reaches the backend through an object store, because that is what names the
/// records on the wire, but the source the spec exposes is whichever surface the method was
/// called on. `IDBIndex`'s eight operations and `IDBCursor`'s `update` and `delete` all travel
/// over an object store while reporting the index or the cursor as their source.
#[derive(JSTraceable, MallocSizeOf)]
#[cfg_attr(crown, crown::unrooted_must_root_lint::must_root)]
pub(crate) enum RequestSource {
    ObjectStore(Dom<IDBObjectStore>),
    Index(Dom<IDBIndex>),
    Cursor(Dom<IDBCursor>),
}

/// Which projection of each record a `getAll`-family request answers with.
///
/// One backend read serves all three methods; only the shape of the answer differs.
#[derive(Clone, Copy)]
pub(crate) enum GetAllKind {
    /// `getAll`: the stored value of each record.
    Values,
    /// `getAllKeys`: the object store key of each record.
    PrimaryKeys,
    /// `getAllRecords`: an `IDBRecord` per record.
    Records,
}

impl GetAllKind {
    /// `getAllKeys` never reads a stored value, and over a store of large values that is the
    /// difference between shipping a list of keys and shipping a copy of the database.
    pub(crate) fn shape(self) -> RecordsShape {
        match self {
            GetAllKind::PrimaryKeys => RecordsShape::KeysOnly,
            GetAllKind::Values | GetAllKind::Records => RecordsShape::WithValues,
        }
    }
}

/// Whether a request may be held back by the transaction's outbound hold.
#[derive(Clone, Copy)]
enum OutboundHold {
    /// The ordinary case: the hold, when it is set, takes this request.
    Respect,
    /// The request the hold is waiting for. Holding it would stall the transaction on itself.
    Bypass,
}

/// Whether the answer to a request is reported to script.
#[derive(Clone, Copy, PartialEq)]
enum RequestVisibility {
    /// The ordinary case: the answer arrives as a `success` or an `error` event fired at the
    /// request, and an unhandled error event's default action aborts the transaction.
    Script,
    /// A half of a `create index` backfill.
    ///
    /// <https://w3c.github.io/IndexedDB/#dom-idbobjectstore-createindex> processes index
    /// creation as an asynchronous request inside the upgrade transaction but never hands
    /// script a request for it, so no event is fired at either half. A failure runs
    /// `abort a transaction` with the error directly, which is how a unique index over records
    /// that already share a key takes the upgrade transaction down.
    Internal(BackfillHalf),
}

/// Which half of a `create index` backfill a request carries.
///
/// The two halves sit on opposite sides of the transaction's outbound hold, so which half a
/// request is decides both where it goes in the queue and what its answer releases.
#[derive(Clone, Copy, PartialEq)]
pub(crate) enum BackfillHalf {
    /// The read that collects the records the new index has to cover. It takes its ordinary
    /// place in the outbound queue, because the records it has to see are the ones every
    /// message ahead of it leaves behind.
    Read,
    /// The write that stores the keys extracted from those records. It bypasses the hold,
    /// because the hold is what it is keeping the rest of the transaction waiting for, and its
    /// answer is the point at which creating the index has either succeeded or failed. Only
    /// then may the messages behind it go out: a request script placed after a `createIndex`
    /// that fails is one the abort answers, not one the backend should ever see.
    Write,
}

/// What the DOM should make of a `Vec<IndexedDBRecord>` answer.
///
/// Cursor iteration and the `getAll` family read the same records and so share one wire
/// payload. This is what tells them apart, and it carries the part of each algorithm the
/// backend was not told about.
#[derive(Clone)]
pub(crate) enum RecordsParam {
    /// Move a cursor onto the next record the iteration selects.
    Cursor(IterationParam),
    /// Project the records the way `kind` names.
    GetAll {
        kind: GetAllKind,
        direction: IDBCursorDirection,
        count: Option<u32>,
    },
    /// Extract `index_name`'s keys from the records and send them back out as the write that
    /// populates a newly created index.
    ///
    /// This request is not script visible. `create index` needs the index's key path evaluated
    /// against every stored value, and only the script thread can do that, so the read comes
    /// here and the keys go out again.
    ///
    /// Everything the second half needs is carried across the round trip rather than read back
    /// off the object store when the records arrive, because the upgrade transaction goes on
    /// running while they travel. `store_name` is the name the backend knows the store by, and
    /// a rename placed after the `createIndex` is still held behind this round trip. The key
    /// path and the multiEntry flag belong to the index the `createIndex` created, and a
    /// `deleteIndex` placed after it has already taken that index off the store handle. The key
    /// path is the storage thread's own, because a `RequestListener` travels to the task queue
    /// and the DOM's `KeyPath` holds `DOMString`, which does not cross threads.
    IndexBackfill {
        store_name: String,
        index_name: String,
        key_path: KeyPath,
        multi_entry: bool,
    },
}

/// A resolved `getAll`, `getAllKeys` or `getAllRecords` request.
///
/// <https://w3c.github.io/IndexedDB/#create-request-to-retrieve-multiple-items>
pub(crate) struct GetAllRequest {
    /// The range the backend reads.
    pub(crate) key_range: IndexedDBKeyRange,
    /// The count the backend may apply during the read, which is not always the count the
    /// request asked for.
    pub(crate) count: Option<u32>,
    /// The projection the DOM applies to the answer.
    pub(crate) records_param: RecordsParam,
}

impl GetAllRequest {
    /// Splits a resolved request into what the DOM applies after the read and what the backend
    /// may apply during it.
    ///
    /// A count of zero is not a limit. The spec reads it as infinity, and a `LIMIT 0` would
    /// answer with nothing at all. Of the counts that do limit, only `next` may be pushed down:
    /// the backend answers in ascending key order with no duplicate filtering, so for every
    /// other direction a count applied during the read would already have truncated the wrong
    /// end of the range, or dropped records that the unique filter was going to remove anyway.
    fn new(
        kind: GetAllKind,
        key_range: IndexedDBKeyRange,
        direction: IDBCursorDirection,
        count: Option<u32>,
    ) -> Self {
        let count = count.filter(|count| *count > 0);
        let backend_count = match direction {
            IDBCursorDirection::Next => count,
            _ => None,
        };
        Self {
            key_range,
            count: backend_count,
            records_param: RecordsParam::GetAll {
                kind,
                direction,
                count,
            },
        }
    }

    /// `getAllRecords(options)`, whose single argument needs no disambiguation.
    pub(crate) fn from_options(
        cx: &mut JSContext,
        kind: GetAllKind,
        options: &IDBGetAllOptions,
    ) -> Fallible<Self> {
        let key_range = convert_value_to_key_range(cx, options.query.handle(), None)?;
        Ok(Self::new(kind, key_range, options.direction, options.count))
    }

    /// `getAll(queryOrOptions, count)` and `getAllKeys(queryOrOptions, count)`, whose first
    /// argument is either a query or an `IDBGetAllOptions`.
    pub(crate) fn resolve(
        cx: &mut JSContext,
        kind: GetAllKind,
        query_or_options: HandleValue,
        count: Option<u32>,
    ) -> Fallible<Self> {
        // Step 8. If running is a potentially valid key range with queryOrOptions is true, the
        // argument is the query and the direction is "next".
        if is_potentially_valid_key_range(cx, query_or_options)? {
            let key_range = convert_value_to_key_range(cx, query_or_options, None)?;
            return Ok(Self::new(kind, key_range, IDBCursorDirection::Next, count));
        }

        // Step 9. Otherwise the argument is an IDBGetAllOptions, and the dictionary replaces
        // the positional count whether or not it carries one of its own.
        let options = match IDBGetAllOptions::new(cx, query_or_options) {
            Ok(JsConversionResult::Success(options)) => options,
            Ok(JsConversionResult::Failure(error)) => {
                return Err(Error::Type(error.into_owned()));
            },
            Err(()) => return Err(Error::JSFailed),
        };
        Self::from_options(cx, kind, &options)
    }
}

/// Applies the direction and count that `getAllRecords` resolves after the read.
///
/// The backend answers in ascending index key order and then ascending primary key order, for
/// every direction. Unique filtering therefore keeps the first record of each key, which is the
/// one with the lowest primary key, and it has to run before the reversal rather than after.
/// Count is last, because it limits the records the direction chose and not the ones the range
/// matched.
fn project_records(
    direction: IDBCursorDirection,
    count: Option<u32>,
    mut records: Vec<IndexedDBRecord>,
) -> Vec<IndexedDBRecord> {
    if matches!(
        direction,
        IDBCursorDirection::Nextunique | IDBCursorDirection::Prevunique
    ) {
        let mut previous: Option<IndexedDBKeyType> = None;
        records.retain(|record| {
            if previous.as_ref() == Some(&record.key) {
                return false;
            }
            previous = Some(record.key.clone());
            true
        });
    }

    if matches!(
        direction,
        IDBCursorDirection::Prev | IDBCursorDirection::Prevunique
    ) {
        records.reverse();
    }

    if let Some(count) = count {
        records.truncate(count as usize);
    }

    records
}

#[derive(Clone)]
struct RequestListener {
    request: Trusted<IDBRequest>,
    records_param: Option<RecordsParam>,
    request_id: u64,
    visibility: RequestVisibility,
}

pub enum IdbResult {
    Key(IndexedDBKeyType),
    Keys(Vec<IndexedDBKeyType>),
    Value(Vec<u8>),
    Values(Vec<Vec<u8>>),
    Count(u64),
    /// The records a range covers. Which of the four algorithms that read records this answer
    /// belongs to is carried by the request's `RecordsParam`, not by the wire.
    Records(Vec<IndexedDBRecord>),
    Error(Error),
    None,
}

impl From<IndexedDBKeyType> for IdbResult {
    fn from(value: IndexedDBKeyType) -> Self {
        IdbResult::Key(value)
    }
}

impl From<Vec<IndexedDBKeyType>> for IdbResult {
    fn from(value: Vec<IndexedDBKeyType>) -> Self {
        IdbResult::Keys(value)
    }
}

impl From<Vec<u8>> for IdbResult {
    fn from(value: Vec<u8>) -> Self {
        IdbResult::Value(value)
    }
}

impl From<Vec<Vec<u8>>> for IdbResult {
    fn from(value: Vec<Vec<u8>>) -> Self {
        IdbResult::Values(value)
    }
}

impl From<PutItemResult> for IdbResult {
    fn from(value: PutItemResult) -> Self {
        match value {
            PutItemResult::Key(key) => Self::Key(key),
            PutItemResult::CannotOverwrite => Self::Error(Error::Constraint(None)),
            PutItemResult::IndexConstraintViolated(index_name) => {
                Self::Error(Error::Constraint(Some(format!(
                    "Unique index \"{index_name}\" already holds that key"
                ))))
            },
            PutItemResult::KeyGeneratorExhausted => Self::Error(Error::Constraint(Some(
                "The object store's key generator has reached its maximum value".into(),
            ))),
        }
    }
}

impl From<Vec<IndexedDBRecord>> for IdbResult {
    fn from(value: Vec<IndexedDBRecord>) -> Self {
        Self::Records(value)
    }
}

impl From<BackfillIndexResult> for IdbResult {
    fn from(value: BackfillIndexResult) -> Self {
        match value {
            BackfillIndexResult::Done => Self::None,
            // <https://w3c.github.io/IndexedDB/#dom-idbobjectstore-createindex>: a unique
            // index that cannot hold the store's records aborts the upgrade transaction with
            // this error. The request carrying it is not script visible, so the abort is run
            // directly rather than as an error event's default action.
            BackfillIndexResult::UniqueConstraintViolated => Self::Error(Error::Constraint(Some(
                "A unique index cannot be created over records that already share a key".into(),
            ))),
        }
    }
}

impl From<()> for IdbResult {
    fn from(_value: ()) -> Self {
        Self::None
    }
}

impl<T> From<Option<T>> for IdbResult
where
    T: Into<IdbResult>,
{
    fn from(value: Option<T>) -> Self {
        match value {
            Some(value) => value.into(),
            None => IdbResult::None,
        }
    }
}

impl From<u64> for IdbResult {
    fn from(value: u64) -> Self {
        IdbResult::Count(value)
    }
}

impl RequestListener {
    fn send_request_handled(cx: &mut JSContext, transaction: &IDBTransaction, request_id: u64) {
        let global = transaction.global();
        // https://w3c.github.io/IndexedDB/#transaction-lifecycle
        // A transaction is inactive after control returns to the event loop and
        // when events are not being dispatched. We call this after dispatching
        // the request event, so the backend can reevaluate commit eligibility.
        let send_result = global.storage_threads().send(IndexedDBThreadMsg::Sync(
            SyncOperation::RequestHandled {
                origin: global.origin().immutable().clone(),
                db_name: String::from(transaction.get_db_name()),
                txn: transaction.get_serial_number(),
                request_id,
            },
        ));
        if send_result.is_err() {
            error!("Failed to send SyncOperation::RequestHandled");
        }
        transaction.mark_request_handled(request_id);

        // This request's result has been handled by script, the
        // transaction might finally be ready to auto-commit.
        transaction.maybe_commit(cx);
    }

    // https://www.w3.org/TR/IndexedDB-3/#async-execute-request
    // Implements Step 5.4
    fn handle_async_request_finished(&self, cx: &mut JSContext, result: BackendResult<IdbResult>) {
        let request = self.request.root();
        let global = request.global();

        let transaction = request
            .transaction
            .get()
            .expect("Request unexpectedly has no transaction");

        // <https://w3c.github.io/IndexedDB/#abort-a-transaction> step 5 already answered this
        // request with an `AbortError`, which is what "abort the steps to asynchronously
        // execute a request" leaves behind: the answer that has just arrived is the one those
        // steps were told to stop producing, so it is dropped rather than fired at a request
        // that is already done. The transaction still counts the request as gone.
        if request.is_settled_by_abort() {
            transaction.request_finished();
            return;
        }

        // Substep 1: Set the result of request to result.
        request.set_ready_state_done();

        let mut realm = enter_auto_realm(cx, &*request);
        let cx: &mut JSContext = &mut realm;
        rooted!(&in(cx) let mut answer = UndefinedValue());

        if let Ok(data) = result {
            match data {
                IdbResult::Key(key) => {
                    // A failed key conversion is the same kind of event as a failed
                    // structured clone below: the request rejects rather than the
                    // process dying.
                    if let Err(e) = key_type_to_jsval(cx, &key, answer.handle_mut()) {
                        warn!("Error converting an IndexedDB key to a value");
                        self.handle_async_request_error(&global, cx, request, e);
                        return;
                    }
                },
                IdbResult::Keys(keys) => {
                    rooted!(&in(cx) let mut array = vec![JSVal::default(); keys.len()]);
                    for (i, key) in keys.into_iter().enumerate() {
                        if let Err(e) = key_type_to_jsval(cx, &key, array.handle_mut_at(i)) {
                            warn!("Error converting an IndexedDB key to a value");
                            self.handle_async_request_error(&global, cx, request, e);
                            return;
                        }
                    }
                    array.safe_to_jsval(cx, answer.handle_mut());
                },
                IdbResult::Value(serialized_data) => {
                    let result = postcard::from_bytes(&serialized_data)
                        .map_err(|_| Error::Data(None))
                        .and_then(|data| {
                            structuredclone::read(cx, &global, data, answer.handle_mut())
                        });
                    if let Err(e) = result {
                        warn!("Error reading structuredclone data");
                        self.handle_async_request_error(&global, cx, request, e);
                        return;
                    };
                },
                IdbResult::Values(serialized_values) => {
                    rooted!(&in(cx) let mut values = vec![JSVal::default(); serialized_values.len()]);
                    for (i, serialized_data) in serialized_values.into_iter().enumerate() {
                        let result = postcard::from_bytes(&serialized_data)
                            .map_err(|_| Error::Data(None))
                            .and_then(|data| {
                                structuredclone::read(cx, &global, data, values.handle_mut_at(i))
                            });
                        if let Err(e) = result {
                            warn!("Error reading structuredclone data");
                            self.handle_async_request_error(&global, cx, request, e);
                            return;
                        };
                    }
                    values.safe_to_jsval(cx, answer.handle_mut());
                },
                IdbResult::Count(count) => {
                    answer.handle_mut().set(DoubleValue(count as f64));
                },
                IdbResult::Records(records) => match self.records_param.as_ref() {
                    Some(RecordsParam::Cursor(param)) => {
                        let cursor = match iterate_cursor(&global, cx, param, records) {
                            Ok(cursor) => cursor,
                            Err(e) => {
                                warn!("Error reading structuredclone data");
                                self.handle_async_request_error(&global, cx, request, e);
                                return;
                            },
                        };
                        match cursor {
                            Some(cursor) => match cursor.downcast::<IDBCursorWithValue>() {
                                Some(cursor_with_value) => {
                                    answer.handle_mut().set(ObjectValue(
                                        *cursor_with_value.reflector().get_jsobject(),
                                    ));
                                },
                                None => {
                                    answer
                                        .handle_mut()
                                        .set(ObjectValue(*cursor.reflector().get_jsobject()));
                                },
                            },
                            // <https://w3c.github.io/IndexedDB/#iterate-a-cursor>
                            // Step 6: no record was found, so the cursor is exhausted and
                            // the request's result is null rather than undefined.
                            None => answer.handle_mut().set(NullValue()),
                        }
                    },
                    Some(RecordsParam::GetAll {
                        kind,
                        direction,
                        count,
                    }) => {
                        let records = project_records(*direction, *count, records);
                        rooted!(&in(cx) let mut array = vec![JSVal::default(); records.len()]);
                        for (i, record) in records.into_iter().enumerate() {
                            let element = match kind {
                                GetAllKind::Values => postcard::from_bytes(&record.value)
                                    .map_err(|_| Error::Data(None))
                                    .and_then(|data| {
                                        structuredclone::read(
                                            cx,
                                            &global,
                                            data,
                                            array.handle_mut_at(i),
                                        )
                                    })
                                    // The deserialized message ports belong to the value, which
                                    // is now rooted in the array. Nothing here owns them.
                                    .map(|_| ()),
                                GetAllKind::PrimaryKeys => key_type_to_jsval(
                                    cx,
                                    &record.primary_key,
                                    array.handle_mut_at(i),
                                ),
                                GetAllKind::Records => {
                                    IDBRecord::new(cx, &global, record).map(|idb_record| {
                                        array.handle_mut_at(i).set(ObjectValue(
                                            *idb_record.reflector().get_jsobject(),
                                        ));
                                    })
                                },
                            };
                            if let Err(e) = element {
                                warn!("Error building a getAll result");
                                self.handle_async_request_error(&global, cx, request, e);
                                return;
                            }
                        }
                        array.safe_to_jsval(cx, answer.handle_mut());
                    },
                    Some(RecordsParam::IndexBackfill {
                        store_name,
                        index_name,
                        key_path,
                        multi_entry,
                    }) => {
                        // A backfill request is not script visible, so no event is fired at it
                        // and nothing below opens the activity window `fire a success event`
                        // step 6 opens. The continuation places the backfill write against the
                        // transaction, so it opens that window here; the internal tail closes
                        // it again.
                        if transaction.is_inactive() {
                            transaction.set_active_flag(true);
                        }
                        let store = match &*request.source.borrow() {
                            Some(RequestSource::ObjectStore(store)) => Some(store.as_rooted()),
                            _ => None,
                        };
                        // A backfill read is always issued from the object store that owns the
                        // index, so anything else here is a protocol error. The transaction
                        // aborts, and the messages the hold is carrying go with it.
                        let Some(store) = store else {
                            warn!("An index backfill answered a request with no object store");
                            self.handle_async_request_error(
                                &global,
                                cx,
                                request,
                                Error::InvalidState(None),
                            );
                            return;
                        };
                        if let Err(e) = store.finish_index_backfill(
                            cx,
                            store_name,
                            index_name,
                            key_path,
                            *multi_entry,
                            records,
                        ) {
                            warn!("Error populating a new index from the store's records");
                            self.handle_async_request_error(&global, cx, request, e);
                            return;
                        }
                    },
                    // The pairing is asserted where the operation is sent, so reaching here
                    // means the backend answered with records for a request that reads none.
                    // The request rejects; it is not a reason to end the content process.
                    None => {
                        warn!("IndexedDB answered with records for a request that reads none");
                        self.handle_async_request_error(
                            &global,
                            cx,
                            request,
                            Error::InvalidState(None),
                        );
                        return;
                    },
                },
                IdbResult::None => {
                    // no-op
                },
                IdbResult::Error(error) => {
                    // Substep 2
                    self.handle_async_request_error(&global, cx, request, error);
                    return;
                },
            }

            if let RequestVisibility::Internal(half) = self.visibility {
                // <https://w3c.github.io/IndexedDB/#dom-idbobjectstore-createindex> keeps the
                // backfill inside the upgrade transaction without exposing a request for it,
                // so there is no result to set and no event to fire. The activity window the
                // continuation above opened is closed here, the way step 8 of
                // `fire a success event` would have closed it.
                if half == BackfillHalf::Write {
                    // Creating the index has landed, so everything script placed behind it can
                    // follow, up to the next `createIndex` that queued itself here.
                    transaction.resume_after_backfill();
                }
                if transaction.is_active() {
                    transaction.set_active_flag(false);
                }
                transaction.request_finished();
                Self::send_request_handled(cx, &transaction, self.request_id);
                return;
            }

            // Substep 3.1: Set the result of request to answer.
            request.set_result(answer.handle());

            // Substep 3.2: Set the error of request to undefined
            request.set_error(cx, None);

            // https://w3c.github.io/IndexedDB/#fire-success-event
            // Step 1: Let event be the result of creating an event using Event.
            // Step 2: Set event’s type attribute to "success".
            // Step 3: Set event’s bubbles and cancelable attributes to false.
            let event = Event::new(
                cx,
                &global,
                Atom::from("success"),
                EventBubbles::DoesNotBubble,
                EventCancelable::NotCancelable,
            );

            // Step 5: Let legacyOutputDidListenersThrowFlag be initially false.
            let did_listeners_throw = Cell::new(false);
            // Step 6: If transaction’s state is inactive, then set transaction’s state to active.
            if transaction.is_inactive() {
                transaction.set_active_flag(true);
            }
            // Step 7: Dispatch event at request with legacyOutputDidListenersThrowFlag.
            event
                .upcast::<Event>()
                .fire_with_legacy_output_did_listeners_throw(
                    cx,
                    request.upcast(),
                    &did_listeners_throw,
                );
            // Step 8: If transaction’s state is active, then:
            if transaction.is_active() {
                // Step 8.1: Set transaction’s state to inactive.
                transaction.set_active_flag(false);
                // Step 8.2: If legacyOutputDidListenersThrowFlag is true, then run abort a
                // transaction with transaction and a newly created "AbortError" DOMException.
                if did_listeners_throw.get() {
                    transaction.initiate_abort(cx, Error::Abort(None));
                    transaction.request_backend_abort();
                }
            }
            transaction.request_finished();

            Self::send_request_handled(cx, &transaction, self.request_id);
        } else {
            // FIXME:(arihant2math) dispatch correct error
            // Substep 2
            self.handle_async_request_error(&global, cx, request, Error::Data(None));
        }
    }

    // https://www.w3.org/TR/IndexedDB-3/#async-execute-request
    // Implements Step 5.4.2
    fn handle_async_request_error(
        &self,
        global: &GlobalScope,
        cx: &mut JSContext,
        request: DomRoot<IDBRequest>,
        error: Error,
    ) {
        let request_id = self.request_id;
        let transaction = request
            .transaction
            .get()
            .expect("Request has no transaction");
        // Substep 1: Set the result of request to undefined.
        rooted!(&in(cx) let undefined = UndefinedValue());
        request.set_result(undefined.handle());

        // Substep 2: Set the error of request to result.
        request.set_error(cx, Some(error.clone()));

        if matches!(self.visibility, RequestVisibility::Internal(_)) {
            // <https://w3c.github.io/IndexedDB/#dom-idbobjectstore-createindex>: creating an
            // index can only fail after the method has returned, and the algorithm answers that
            // by running `abort a transaction` with the error. Script holds no request for the
            // backfill, so firing an error event here would instead surface the failure at the
            // transaction and the connection, where the algorithm never puts it.
            if transaction.is_active() {
                transaction.set_active_flag(false);
            }
            transaction.initiate_abort(cx, error);
            transaction.request_backend_abort();
            transaction.request_finished();
            Self::send_request_handled(cx, &transaction, request_id);
            return;
        }

        // https://w3c.github.io/IndexedDB/#fire-error-event
        // Step 1: Let event be the result of creating an event using Event.
        // Step 2: Set event’s type attribute to "error".
        // Step 3: Set event’s bubbles and cancelable attributes to true.
        let event = Event::new(
            cx,
            global,
            Atom::from("error"),
            EventBubbles::Bubbles,
            EventCancelable::Cancelable,
        );

        // If result is an error and transaction’s state is committing, then run abort a
        // transaction with transaction and result, and terminate these steps.
        if transaction.is_committing() {
            transaction.initiate_abort(cx, error.clone());
            transaction.request_backend_abort();
        }
        // Step 5: Let legacyOutputDidListenersThrowFlag be initially false.
        let did_listeners_throw = Cell::new(false);
        // Step 6: If transaction’s state is inactive, then set transaction’s state to active.
        if transaction.is_inactive() {
            transaction.set_active_flag(true);
        }
        // Step 7: Dispatch event at request with legacyOutputDidListenersThrowFlag.
        let default_not_prevented = event
            .upcast::<Event>()
            .fire_with_legacy_output_did_listeners_throw(
                cx,
                request.upcast(),
                &did_listeners_throw,
            );
        // Step 8: If transaction’s state is active, then:
        if transaction.is_active() {
            // Step 8.1: Set transaction’s state to inactive.
            transaction.set_active_flag(false);
            // Step 8.2: If legacyOutputDidListenersThrowFlag is true, then run abort a transaction
            // with transaction and a newly created "AbortError" DOMException and terminate these steps.
            // NOTE: This is done even if event’s canceled flag is false.
            // NOTE: This means that if an error event is fired and any of the event handlers throw an
            // exception, transaction’s error property is set to an AbortError rather than request’s
            // error, even if preventDefault() is never called.
            if did_listeners_throw.get() {
                transaction.initiate_abort(cx, Error::Abort(None));
                transaction.request_backend_abort();
            } else if default_not_prevented {
                // Step 8.3: If event’s canceled flag is false, then run abort a transaction
                // using transaction and request’s error, and terminate these steps.
                transaction.initiate_abort(cx, error);
                transaction.request_backend_abort();
            }
        }
        transaction.request_finished();
        Self::send_request_handled(cx, &transaction, request_id);
    }
}

#[dom_struct]
pub struct IDBRequest {
    eventtarget: EventTarget,
    #[ignore_malloc_size_of = "mozjs"]
    result: Heap<JSVal>,
    error: MutNullableDom<DOMException>,
    source: DomRefCell<Option<RequestSource>>,
    transaction: MutNullableDom<IDBTransaction>,
    ready_state: Cell<IDBRequestReadyState>,
    /// Whether `abort a transaction` has taken this request over.
    ///
    /// <https://w3c.github.io/IndexedDB/#abort-a-transaction> step 5 answers every request the
    /// transaction still owes an answer to, so the answer the backend is still going to send
    /// for it stops being the answer. The flag is set while the abort runs, which is before
    /// either answer can be delivered, and it is what tells the late one apart.
    settled_by_abort: Cell<bool>,
    /// Whether script holds this request.
    ///
    /// <https://w3c.github.io/IndexedDB/#dom-idbobjectstore-createindex> runs its backfill as
    /// requests the algorithm never exposes, so an abort has nobody to fire their error events
    /// at.
    script_visible: Cell<bool>,
}

impl IDBRequest {
    pub fn new_inherited() -> IDBRequest {
        IDBRequest {
            eventtarget: EventTarget::new_inherited(),

            result: Heap::default(),
            error: Default::default(),
            source: Default::default(),
            transaction: Default::default(),
            ready_state: Cell::new(IDBRequestReadyState::Pending),
            settled_by_abort: Cell::new(false),
            script_visible: Cell::new(true),
        }
    }

    pub fn new(cx: &mut JSContext, global: &GlobalScope) -> DomRoot<IDBRequest> {
        reflect_dom_object_with_cx(Box::new(IDBRequest::new_inherited()), global, cx)
    }

    pub(crate) fn set_source(&self, source: RequestSource) {
        *self.source.borrow_mut() = Some(source);
    }

    pub fn set_ready_state_done(&self) {
        self.ready_state.set(IDBRequestReadyState::Done);
    }

    /// Reopens a completed request so it can carry the result of another operation.
    ///
    /// Cursor iteration is the only caller: `advance`, `continue` and `continuePrimaryKey` all
    /// say "set request's done flag to false" and then run a fresh iterate operation against the
    /// same `IDBRequest` object, because the cursor holds exactly one request for its lifetime.
    pub fn set_ready_state_pending(&self) {
        self.ready_state.set(IDBRequestReadyState::Pending);
    }

    pub fn set_result(&self, result: HandleValue) {
        self.result.set(result.get());
    }

    pub fn set_error(&self, cx: &mut JSContext, error: Option<Error>) {
        if let Some(error) = error {
            if let Ok(exception) = create_dom_exception(cx, &self.global(), error) {
                self.error.set(Some(&exception));
            }
        } else {
            self.error.set(None);
        }
    }

    pub fn set_transaction(&self, transaction: &IDBTransaction) {
        self.transaction.set(Some(transaction));
    }

    pub fn clear_transaction(&self) {
        self.transaction.set(None);
    }

    fn is_done(&self) -> bool {
        self.ready_state.get() == IDBRequestReadyState::Done
    }

    /// See [`Self::script_visible`].
    pub(crate) fn hide_from_script(&self) {
        self.script_visible.set(false);
    }

    /// See [`Self::settled_by_abort`].
    pub(crate) fn is_settled_by_abort(&self) -> bool {
        self.settled_by_abort.get()
    }

    /// Whether an aborting transaction still owes this request an answer.
    pub(crate) fn is_awaiting_answer(&self) -> bool {
        self.script_visible.get() && !self.is_done() && !self.settled_by_abort.get()
    }

    /// <https://w3c.github.io/IndexedDB/#abort-a-transaction> step 5, for one request.
    ///
    /// The transaction is aborting, so the answer to this request is that the transaction took
    /// it down, whether the backend was about to answer it or, for a request the outbound hold
    /// was still carrying, was never going to hear about it at all. The request is settled
    /// synchronously so a reply already on its way is recognised as stale when it lands, and the
    /// event the abort owes script is queued as a database task.
    ///
    /// Step 5.4 is `fire an event`, not `fire an error event`: the transaction is already
    /// finished, so neither the activity window nor the unhandled-error abort that the latter
    /// carries has anything left to act on.
    pub(crate) fn settle_by_abort(&self) {
        if self.settled_by_abort.get() {
            return;
        }
        self.settled_by_abort.set(true);
        let this = Trusted::new(self);
        self.global()
            .task_manager()
            .database_access_task_source()
            .queue(task!(idb_request_aborted: move |cx| {
                let request = this.root();
                let global = request.global();
                let mut realm = enter_auto_realm(cx, &*request);
                let cx: &mut JSContext = &mut realm;
                // Step 5.1. Set request's done flag to true.
                request.set_ready_state_done();
                // Step 5.2. Set request's result to undefined.
                rooted!(&in(cx) let undefined = UndefinedValue());
                request.set_result(undefined.handle());
                // Step 5.3. Set request's error to a newly created "AbortError" DOMException.
                request.set_error(cx, Some(Error::Abort(None)));
                // Step 5.4. Fire an event named error at request with its bubbles and
                // cancelable attributes initialized to true.
                let event = Event::new(
                    cx,
                    &global,
                    Atom::from("error"),
                    EventBubbles::Bubbles,
                    EventCancelable::Cancelable,
                );
                event.fire(cx, request.upcast());
            }));
    }

    pub(crate) fn transaction(&self) -> Option<DomRoot<IDBTransaction>> {
        self.transaction.get()
    }

    // https://www.w3.org/TR/IndexedDB-3/#asynchronously-execute-a-request
    pub fn execute_async<T, F>(
        cx: &mut JSContext,
        store: &IDBObjectStore,
        operation_fn: F,
        request: Option<DomRoot<IDBRequest>>,
        records_param: Option<RecordsParam>,
    ) -> Fallible<DomRoot<IDBRequest>>
    where
        T: Into<IdbResult> + for<'a> Deserialize<'a> + Serialize + Send + Sync + 'static,
        F: FnOnce(GenericCallback<BackendResult<T>>) -> AsyncOperation,
    {
        Self::execute_async_with_context(
            cx,
            store,
            KvsOperationContext::default(),
            operation_fn,
            request,
            records_param,
        )
    }

    /// `asynchronously execute a request` where the request's source is not the object store
    /// that carries the transaction and the store name.
    ///
    /// Only `IDBIndex` and `IDBCursor` need this. Everything the backend is told still derives
    /// from `store`; `source` is the DOM surface the method was called on, and the two are not
    /// the same thing.
    pub(crate) fn execute_async_from_source<T, F>(
        cx: &mut JSContext,
        store: &IDBObjectStore,
        source: RequestSource,
        context: KvsOperationContext,
        operation_fn: F,
        request: Option<DomRoot<IDBRequest>>,
        records_param: Option<RecordsParam>,
    ) -> Fallible<DomRoot<IDBRequest>>
    where
        T: Into<IdbResult> + for<'a> Deserialize<'a> + Serialize + Send + Sync + 'static,
        F: FnOnce(GenericCallback<BackendResult<T>>) -> AsyncOperation,
    {
        Self::execute_async_inner(
            cx,
            store,
            String::from(store.get_name()),
            Some(source),
            context,
            operation_fn,
            request,
            records_param,
            OutboundHold::Respect,
            RequestVisibility::Script,
        )
    }

    /// Asynchronously execute a request, naming which of the object store's surfaces the
    /// request addresses.
    ///
    /// `store` is always the owning object store, because the transaction, the store name on
    /// the wire and the request's source all derive from it. `context` is what distinguishes an
    /// `IDBIndex` request from an `IDBObjectStore` one: the backend reads index records when
    /// `context.target` is `KvsOperationTarget::Index` and object store records otherwise. The
    /// six read operations are deliberately target agnostic, so an index needs no new operation
    /// variants, only the context that selects which records they range over.
    pub fn execute_async_with_context<T, F>(
        cx: &mut JSContext,
        store: &IDBObjectStore,
        context: KvsOperationContext,
        operation_fn: F,
        request: Option<DomRoot<IDBRequest>>,
        records_param: Option<RecordsParam>,
    ) -> Fallible<DomRoot<IDBRequest>>
    where
        T: Into<IdbResult> + for<'a> Deserialize<'a> + Serialize + Send + Sync + 'static,
        F: FnOnce(GenericCallback<BackendResult<T>>) -> AsyncOperation,
    {
        Self::execute_async_inner(
            cx,
            store,
            String::from(store.get_name()),
            None,
            context,
            operation_fn,
            request,
            records_param,
            OutboundHold::Respect,
            RequestVisibility::Script,
        )
    }

    /// `asynchronously execute a request` for one half of a `create index` backfill.
    ///
    /// Neither half is script visible, so neither fires an event and a failure aborts the
    /// upgrade transaction outright. `store_name` is the name the backend knows the store by,
    /// which is the name it had when `createIndex` ran: a rename placed afterwards is still
    /// waiting behind this round trip.
    ///
    /// `half` decides where the request sits relative to the transaction's outbound hold, so
    /// the two travel together rather than being chosen separately at each call site.
    pub(crate) fn execute_backfill_operation<T, F>(
        cx: &mut JSContext,
        store: &IDBObjectStore,
        store_name: &str,
        half: BackfillHalf,
        operation_fn: F,
        records_param: Option<RecordsParam>,
    ) -> Fallible<DomRoot<IDBRequest>>
    where
        T: Into<IdbResult> + for<'a> Deserialize<'a> + Serialize + Send + Sync + 'static,
        F: FnOnce(GenericCallback<BackendResult<T>>) -> AsyncOperation,
    {
        let hold = match half {
            BackfillHalf::Read => OutboundHold::Respect,
            BackfillHalf::Write => OutboundHold::Bypass,
        };
        Self::execute_async_inner(
            cx,
            store,
            store_name.to_owned(),
            None,
            KvsOperationContext::default(),
            operation_fn,
            None,
            records_param,
            hold,
            RequestVisibility::Internal(half),
        )
    }

    /// `asynchronously execute a request` for an operation that has already failed.
    ///
    /// <https://w3c.github.io/IndexedDB/#asynchronously-execute-a-request> runs the operation in
    /// parallel and reports its failure by firing an error event at the request, so an operation
    /// that cannot even start still answers asynchronously rather than throwing out of the method
    /// that created the request. `store a record into an object store` fails this way when the
    /// store's key generator can no longer produce a key.
    pub(crate) fn execute_async_failure(
        cx: &mut JSContext,
        store: &IDBObjectStore,
        error: Error,
    ) -> Fallible<DomRoot<IDBRequest>> {
        // Step 1. Let transaction be the transaction associated with source.
        let transaction = store.transaction();
        let global = transaction.global();
        // Step 2. Assert: transaction is active.
        if !transaction.is_active() || !transaction.is_usable() {
            return Err(Error::TransactionInactive(None));
        }

        let request_id = transaction.allocate_request_id();
        // Step 3. Let request be a new request with source as source.
        let request = IDBRequest::new(cx, &global);
        request.set_source(RequestSource::ObjectStore(Dom::from_ref(store)));
        request.set_transaction(&transaction);
        // Step 4. Add request to the end of transaction's request list.
        transaction.add_request(&request);

        // Step 5. The answer is already known, so the returning task is queued here instead of
        // by a backend reply. It still reports the request id as handled, which is what keeps
        // the transaction's commit bookkeeping in step with the requests script placed.
        let listener = RequestListener {
            request: Trusted::new(&request),
            records_param: None,
            request_id,
            visibility: RequestVisibility::Script,
        };
        global.task_manager().database_access_task_source().queue(
            task!(idb_request_failed: move |cx| {
                listener.handle_async_request_finished(cx, Ok(IdbResult::Error(error)));
            }),
        );

        // Step 6. Return request.
        Ok(request)
    }

    #[allow(clippy::too_many_arguments)]
    fn execute_async_inner<T, F>(
        cx: &mut JSContext,
        store: &IDBObjectStore,
        store_name: String,
        source: Option<RequestSource>,
        context: KvsOperationContext,
        operation_fn: F,
        request: Option<DomRoot<IDBRequest>>,
        records_param: Option<RecordsParam>,
        hold: OutboundHold,
        visibility: RequestVisibility,
    ) -> Fallible<DomRoot<IDBRequest>>
    where
        T: Into<IdbResult> + for<'a> Deserialize<'a> + Serialize + Send + Sync + 'static,
        F: FnOnce(GenericCallback<BackendResult<T>>) -> AsyncOperation,
    {
        // Step 1: Let transaction be the transaction associated with source.
        let transaction = store.transaction();
        let global = transaction.global();
        // Step 2: Assert: transaction is active.
        if !transaction.is_active() || !transaction.is_usable() {
            return Err(Error::TransactionInactive(None));
        }

        let request_id = transaction.allocate_request_id();

        // Step 3: If request was not given, let request be a new request with source as source.
        let request = request.unwrap_or_else(|| {
            let new_request = IDBRequest::new(cx, &global);
            new_request.set_source(
                source.unwrap_or_else(|| RequestSource::ObjectStore(Dom::from_ref(store))),
            );
            new_request.set_transaction(&transaction);
            new_request
        });
        if matches!(visibility, RequestVisibility::Internal(_)) {
            request.hide_from_script();
        }

        // Step 4: Add request to the end of transaction’s request list.
        transaction.add_request(&request);

        // Step 5: Run the operation, and queue a returning task in parallel
        // the result will be put into `receiver`
        let transaction_mode = match transaction.get_mode() {
            IDBTransactionMode::Readonly => IndexedDBTxnMode::Readonly,
            IDBTransactionMode::Readwrite => IndexedDBTxnMode::Readwrite,
            IDBTransactionMode::Versionchange => IndexedDBTxnMode::Versionchange,
        };

        let response_listener = RequestListener {
            request: Trusted::new(&request),
            records_param: records_param.clone(),
            request_id,
            visibility,
        };

        let task_source = global
            .task_manager()
            .database_access_task_source()
            .to_sendable();

        let closure = move |message: Result<BackendResult<T>, ipc_channel::IpcError>| {
            let response_listener = response_listener.clone();
            // In multiprocess mode this runs on the router thread with a reply that crossed a
            // process boundary, so it can arrive as a transport error. That is one request's
            // failure, and it has a request to fail; panicking here would instead take down
            // every page in the content process.
            let result = message.unwrap_or_else(|error| Err(reply_lost(error)));
            task_source.queue(task!(request_callback: move |cx| {
                response_listener.handle_async_request_finished(
                    cx,
                    result.inspect_err(|e| {
                        if let BackendError::DbErr(e) = e {
                            error!("Error in IndexedDB operation: {}", e);
                        }
                    }).map(|t| t.into()),
                );
            }));
        };
        let callback = GenericCallback::new(global.time_profiler_chan().clone(), closure)
            .expect("Could not create callback");
        let operation = operation_fn(callback);

        // Every record-reading request answers with `Vec<IndexedDBRecord>`, so the parameter
        // is the only thing that says which algorithm the answer belongs to. Pairing it with
        // the operation here is what lets the result handler treat a missing one as a protocol
        // error rather than guess.
        match &operation {
            AsyncOperation::ReadOnly(AsyncReadOnlyOperation::Iterate { .. }) => assert!(
                records_param.is_some(),
                "Iterate must carry the RecordsParam that names the algorithm reading it"
            ),
            _ => assert!(
                records_param.is_none(),
                "records_param should not be provided for an operation that reads no records"
            ),
        }

        // Start is a backend database task (spec). Script does not model it with a
        // separate queued task, backend scheduling decides when requests begin.
        let message = IndexedDBThreadMsg::Async(
            global.origin().immutable().clone(),
            String::from(transaction.get_db_name()),
            store_name,
            context,
            transaction.get_serial_number(),
            request_id,
            transaction_mode,
            operation,
        );
        let sent = match hold {
            OutboundHold::Respect => transaction.send_or_hold(message),
            OutboundHold::Bypass => transaction
                .global()
                .storage_threads()
                .send(message)
                .map_err(|_| ()),
        };
        if sent.is_err() {
            warn!("Could not send an IndexedDB request to the storage backend");
        }

        // Step 6
        Ok(request)
    }
}

impl IDBRequestMethods<crate::DomTypeHolder> for IDBRequest {
    /// <https://www.w3.org/TR/IndexedDB-3/#dom-idbrequest-result>
    fn GetResult(
        &self,
        _cx: &mut JSContext,
        mut val: js::rust::MutableHandle<'_, js::jsapi::Value>,
    ) -> Fallible<()> {
        // Step 1. If this's done flag is false, then throw an "InvalidStateError" DOMException.
        if !self.is_done() {
            return Err(Error::InvalidState(Some(
                "Cannot get result on a request that is still pending.".into(),
            )));
        }

        // Step 2. Return this's result, or undefined if the request resulted in an error.
        val.set(self.result.get());
        Ok(())
    }

    /// <https://www.w3.org/TR/IndexedDB-3/#dom-idbrequest-error>
    fn GetError(&self) -> Fallible<Option<DomRoot<DOMException>>> {
        // Step 1. If this's done flag is false, then throw an "InvalidStateError" DOMException.
        if !self.is_done() {
            return Err(Error::InvalidState(Some(
                "Cannot get error on a request that is still pending.".into(),
            )));
        }

        // Step 2. Return this's error, or null if no error occurred.
        Ok(self.error.get())
    }

    /// <https://www.w3.org/TR/IndexedDB-3/#dom-idbrequest-source>
    fn GetSource(&self) -> Option<IDBObjectStoreOrIDBIndexOrIDBCursor> {
        // A cursor's reflector is the `IDBCursorWithValue` object when the cursor has one, so
        // rooting it as an `IDBCursor` still hands script back the object it already holds.
        self.source.borrow().as_ref().map(|source| match source {
            RequestSource::ObjectStore(store) => {
                IDBObjectStoreOrIDBIndexOrIDBCursor::IDBObjectStore(store.as_rooted())
            },
            RequestSource::Index(index) => {
                IDBObjectStoreOrIDBIndexOrIDBCursor::IDBIndex(index.as_rooted())
            },
            RequestSource::Cursor(cursor) => {
                IDBObjectStoreOrIDBIndexOrIDBCursor::IDBCursor(cursor.as_rooted())
            },
        })
    }

    /// <https://www.w3.org/TR/IndexedDB-3/#dom-idbrequest-transaction>
    fn GetTransaction(&self) -> Option<DomRoot<IDBTransaction>> {
        self.transaction.get()
    }

    /// <https://www.w3.org/TR/IndexedDB-3/#dom-idbrequest-readystate>
    fn ReadyState(&self) -> IDBRequestReadyState {
        self.ready_state.get()
    }

    // https://www.w3.org/TR/IndexedDB-3/#dom-idbrequest-onsuccess
    event_handler!(success, GetOnsuccess, SetOnsuccess);

    // https://www.w3.org/TR/IndexedDB-3/#dom-idbrequest-onerror
    event_handler!(error, GetOnerror, SetOnerror);
}
