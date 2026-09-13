/* This Source Code Form is subject to the terms of the Mozilla Public
 * License, v. 2.0. If a copy of the MPL was not distributed with this
 * file, You can obtain one at https://mozilla.org/MPL/2.0/. */

use dom_struct::dom_struct;
use js::context::JSContext;
use script_bindings::reflector::{Reflector, reflect_dom_object_with_cx};

use crate::dom::bindings::codegen::Bindings::LockManagerBinding::{LockMethods, LockMode};
use crate::dom::bindings::root::DomRoot;
use crate::dom::bindings::str::DOMString;
use crate::dom::globalscope::GlobalScope;

/// <https://w3c.github.io/web-locks/#api-lock>
#[dom_struct]
pub(crate) struct Lock {
    reflector_: Reflector,
    /// <https://w3c.github.io/web-locks/#lock-name>
    name: DOMString,
    /// <https://w3c.github.io/web-locks/#lock-mode>
    mode: LockMode,
}

impl Lock {
    fn new_inherited(name: DOMString, mode: LockMode) -> Lock {
        Lock {
            reflector_: Reflector::new(),
            name,
            mode,
        }
    }

    pub(crate) fn new(
        cx: &mut JSContext,
        global: &GlobalScope,
        name: DOMString,
        mode: LockMode,
    ) -> DomRoot<Lock> {
        reflect_dom_object_with_cx(Box::new(Lock::new_inherited(name, mode)), global, cx)
    }
}

impl LockMethods<crate::DomTypeHolder> for Lock {
    /// <https://w3c.github.io/web-locks/#dom-lock-name>
    fn Name(&self) -> DOMString {
        self.name.clone()
    }

    /// <https://w3c.github.io/web-locks/#dom-lock-mode>
    fn Mode(&self) -> LockMode {
        self.mode
    }
}
