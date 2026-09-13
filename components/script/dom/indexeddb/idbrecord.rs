/* This Source Code Form is subject to the terms of the Mozilla Public
 * License, v. 2.0. If a copy of the MPL was not distributed with this
 * file, You can obtain one at https://mozilla.org/MPL/2.0/. */

use dom_struct::dom_struct;
use js::context::JSContext;
use js::jsapi::Heap;
use js::jsval::{JSVal, UndefinedValue};
use js::rust::MutableHandleValue;
use script_bindings::reflector::{Reflector, reflect_dom_object_with_cx};
use storage_traits::indexeddb::IndexedDBRecord;

use crate::dom::bindings::codegen::Bindings::IDBRecordBinding::IDBRecordMethods;
use crate::dom::bindings::error::{Error, Fallible};
use crate::dom::bindings::root::DomRoot;
use crate::dom::bindings::structuredclone;
use crate::dom::globalscope::GlobalScope;
use crate::indexeddb::key_type_to_jsval;

/// <https://www.w3.org/TR/IndexedDB-3/#idbrecord>
///
/// One entry of a `getAllRecords` result. The three attributes are converted once, when the
/// record is built, rather than on each read: the spec says each getter returns the same object
/// every time it is inspected, and converting eagerly is also what lets the getters be
/// infallible, because the allocation that can fail has already happened where a failed request
/// can still report it.
#[dom_struct]
pub(crate) struct IDBRecord {
    reflector_: Reflector,
    /// <https://www.w3.org/TR/IndexedDB-3/#dom-idbrecord-key>
    #[ignore_malloc_size_of = "mozjs"]
    key: Heap<JSVal>,
    /// <https://www.w3.org/TR/IndexedDB-3/#dom-idbrecord-primarykey>
    #[ignore_malloc_size_of = "mozjs"]
    primary_key: Heap<JSVal>,
    /// <https://www.w3.org/TR/IndexedDB-3/#dom-idbrecord-value>
    #[ignore_malloc_size_of = "mozjs"]
    value: Heap<JSVal>,
}

impl IDBRecord {
    fn new_inherited() -> IDBRecord {
        IDBRecord {
            reflector_: Reflector::new(),
            key: Heap::default(),
            primary_key: Heap::default(),
            value: Heap::default(),
        }
    }

    /// Projects one stored record into the `IDBRecord` a `getAllRecords` result array holds.
    ///
    /// The reflector is allocated before any slot is written, because `Heap::set` registers the
    /// slot's own address with the GC store buffer: a value written into a `Heap` that is later
    /// moved leaves the buffer pointing at freed memory.
    pub(crate) fn new(
        cx: &mut JSContext,
        global: &GlobalScope,
        record: IndexedDBRecord,
    ) -> Fallible<DomRoot<IDBRecord>> {
        let this = reflect_dom_object_with_cx(Box::new(IDBRecord::new_inherited()), global, cx);

        rooted!(&in(cx) let mut key = UndefinedValue());
        key_type_to_jsval(cx, &record.key, key.handle_mut())?;
        this.key.set(key.get());

        rooted!(&in(cx) let mut primary_key = UndefinedValue());
        key_type_to_jsval(cx, &record.primary_key, primary_key.handle_mut())?;
        this.primary_key.set(primary_key.get());

        rooted!(&in(cx) let mut value = UndefinedValue());
        let data = postcard::from_bytes(&record.value).map_err(|_| Error::Data(None))?;
        structuredclone::read(cx, global, data, value.handle_mut())?;
        this.value.set(value.get());

        Ok(this)
    }
}

impl IDBRecordMethods<crate::DomTypeHolder> for IDBRecord {
    /// <https://www.w3.org/TR/IndexedDB-3/#dom-idbrecord-key>
    fn Key(&self, mut retval: MutableHandleValue) {
        retval.set(self.key.get());
    }

    /// <https://www.w3.org/TR/IndexedDB-3/#dom-idbrecord-primarykey>
    fn PrimaryKey(&self, mut retval: MutableHandleValue) {
        retval.set(self.primary_key.get());
    }

    /// <https://www.w3.org/TR/IndexedDB-3/#dom-idbrecord-value>
    fn Value(&self, mut retval: MutableHandleValue) {
        retval.set(self.value.get());
    }
}
