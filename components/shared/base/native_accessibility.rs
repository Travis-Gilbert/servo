/* This Source Code Form is subject to the terms of the Mozilla Public
 * License, v. 2.0. If a copy of the MPL was not distributed with this
 * file, You can obtain one at https://mozilla.org/MPL/2.0/. */
//! Native current-document control semantics. These are observations of actual
//! DOM/layout owners, never selectors or script-provided action capabilities.
use crate::id::{PipelineId, WebViewId};
use crate::native_text::NativeTextError;
use serde::{Deserialize, Serialize};
#[derive(Clone, Debug, Deserialize, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct NativeAccessibilityNode {
    pub id: String,
    pub parent: Option<String>,
    pub revision: String,
    pub text_digest: Option<String>,
    pub text_utf16_length: Option<u32>,
    pub offscreen: bool,
    pub protected: bool,
    pub role: String,
    pub label: String,
    pub value: Option<String>,
    pub bounds: [f64; 4],
    pub action_point: Option<[f64; 2]>,
    pub disabled: bool,
    pub readonly: bool,
    pub focused: bool,
    pub selected: Option<bool>,
    pub expanded: Option<bool>,
    pub selection: Option<(u32, u32)>,
    pub actions: Vec<NativeAccessibilityActionKind>,
}
#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub enum NativeAccessibilityActionKind {
    Focus,
    Reveal,
    ReadText,
    Click,
    SetValue,
    SelectText,
}
#[derive(Clone, Debug, Deserialize, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct NativeAccessibilitySnapshot {
    pub webview: WebViewId,
    pub document: PipelineId,
    pub viewport: [f64; 2],
    pub nodes: Vec<NativeAccessibilityNode>,
    pub truncated: bool,
    pub text: Option<NativeAccessibilityText>,
}
#[derive(Clone, Debug, Deserialize, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct NativeAccessibilityText {
    pub node: String,
    pub revision: String,
    pub digest: String,
    pub range: (u32, u32),
    pub total_utf16_length: u32,
    pub value: String,
}
#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub enum NativeAccessibilityAction {
    Focus,
    Reveal,
    ReadText { range: (u32, u32) },
    Click,
    SetValue { value: String },
    SelectText { range: (u32, u32) },
}
#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub enum NativeAccessibilityRequest {
    /// Actual current document identity without walking its DOM.
    Identity,
    Snapshot,
    Action {
        webview: WebViewId,
        document: PipelineId,
        expected: NativeAccessibilityNode,
        action: NativeAccessibilityAction,
    },
}
pub type NativeAccessibilityResult = Result<NativeAccessibilitySnapshot, NativeTextError>;
