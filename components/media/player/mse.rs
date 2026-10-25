/* This Source Code Form is subject to the terms of the Mozilla Public
 * License, v. 2.0. If a copy of the MPL was not distributed with this
 * file, You can obtain one at https://mozilla.org/MPL/2.0/. */
//! Backend-neutral MSE operations. Container parsing stays in the media backend.
use std::any::Any;
use std::sync::Arc;

#[derive(Debug, Clone)]
pub enum MseError {
    InvalidState,
    NotSupported,
    QuotaExceeded,
    Type,
    Backend(String),
}
pub type Result<T> = std::result::Result<T, MseError>;
pub type Completion = Box<dyn FnOnce(Result<()>) + Send>;

pub trait SourceBuffer: Send + Sync {
    fn as_any(&self) -> &dyn Any;
    fn buffered(&self) -> Result<Vec<(f64, f64)>>;
    fn append(&self, data: Vec<u8>, done: Completion) -> Result<()>;
    fn remove(&self, start: f64, end: f64, done: Completion) -> Result<()>;
    fn abort(&self) -> Result<()>;
    fn change_type(&self, mime: &str) -> Result<()>;
    fn set_sequence(&self, sequence: bool) -> Result<()>;
    fn set_timestamp_offset(&self, offset: f64) -> Result<()>;
    fn set_append_window(&self, start: f64, end: f64) -> Result<()>;
}
pub trait MediaSource: Send + Sync {
    fn as_any(&self) -> &dyn Any;
    fn add_source_buffer(&self, mime: &str) -> Result<Arc<dyn SourceBuffer>>;
    fn remove_source_buffer(&self, buffer: &dyn SourceBuffer) -> Result<()>;
    fn duration(&self) -> f64;
    fn set_duration(&self, duration: f64) -> Result<()>;
    fn end_of_stream(&self, error: Option<bool>) -> Result<()>;
    fn set_live_range(&self, range: Option<(f64, f64)>) -> Result<()>;
    fn detach(&self);
}
