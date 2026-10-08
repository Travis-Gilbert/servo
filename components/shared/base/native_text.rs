/* This Source Code Form is subject to the terms of the Mozilla Public
 * License, v. 2.0. If a copy of the MPL was not distributed with this
 * file, You can obtain one at https://mozilla.org/MPL/2.0/. */
//! Native-only current-document text input. No WebIDL or JavaScript route.
use crate::id::{PipelineId, WebViewId};
use serde::{Deserialize, Serialize};
#[derive(Clone, Debug, Deserialize, PartialEq, Serialize)]
pub struct NativeTextContext {
    pub webview: WebViewId,
    pub document: PipelineId,
    pub element: String,
    pub focus_sequence: u64,
    pub text: String,
    pub selection: (u32, u32),
    pub marked: Option<(u32, u32)>,
    /// Actual focused element bounds in CSS viewport coordinates.
    pub rect: [f64; 4],
    /// Viewport CSS pixels from actual glyph-selection/caret layout; never element bounds.
    pub selection_rect: Option<[f64; 4]>,
}
impl NativeTextContext {
    /// Geometry is a layout observation, not an editor identity or edit precondition.
    /// Native edit chains retain exact document/focus/buffer state while caret paint moves.
    pub fn same_editor_state(&self, other: &Self) -> bool {
        self.webview == other.webview
            && self.document == other.document
            && self.element == other.element
            && self.focus_sequence == other.focus_sequence
            && self.text == other.text
            && self.selection == other.selection
            && self.marked == other.marked
    }
}
#[derive(Clone, Debug, Deserialize, Serialize)]
pub enum NativeTextAction {
    Replace {
        range: Option<(u32, u32)>,
        text: String,
    },
    Preedit {
        range: Option<(u32, u32)>,
        text: String,
        selection: Option<(u32, u32)>,
    },
    Unmark,
    Select {
        range: (u32, u32),
    },
}
#[derive(Clone, Debug, Deserialize, Serialize)]
pub enum NativeTextRequest {
    Snapshot,
    Edit {
        expected: NativeTextContext,
        action: NativeTextAction,
    },
}
#[derive(Clone, Debug, Deserialize, PartialEq, Serialize)]
pub enum NativeTextError {
    DocumentUnavailable,
    StaleContext,
    Unsupported,
    InvalidRange,
    TooLarge,
    EditRefused,
}
pub type NativeTextResult = Result<Option<NativeTextContext>, NativeTextError>;
