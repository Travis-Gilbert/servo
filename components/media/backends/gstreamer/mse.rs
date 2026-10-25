/* This Source Code Form is subject to the terms of the Mozilla Public
 * License, v. 2.0. If a copy of the MPL was not distributed with this
 * file, You can obtain one at https://mozilla.org/MPL/2.0/. */
//! GStreamer's MSE implementation, adapted from servo/media#458.
use glib::prelude::*;
use glib::translate::{ToGlibPtr, from_glib_full};
use gstreamer::ClockTime;
use gstreamer_mse as gst_mse;
use servo_media_player::mse::{Completion, MediaSource, MseError, Result, SourceBuffer};
use std::any::Any;
use std::sync::{Arc, Mutex};

fn error(error: glib::Error) -> MseError {
    match error.kind::<gst_mse::MediaSourceError>() {
        Some(gst_mse::MediaSourceError::InvalidState) => MseError::InvalidState,
        Some(gst_mse::MediaSourceError::NotSupported) => MseError::NotSupported,
        Some(gst_mse::MediaSourceError::QuotaExceeded) => MseError::QuotaExceeded,
        Some(gst_mse::MediaSourceError::Type) => MseError::Type,
        _ => MseError::Backend(error.to_string()),
    }
}
fn time(seconds: f64) -> Result<ClockTime> {
    if !seconds.is_finite() || seconds < 0.0 || seconds >= (u64::MAX as f64 / 1e9) {
        return Err(MseError::Type);
    }
    Ok(ClockTime::from_nseconds((seconds * 1e9) as u64))
}

pub struct Source(pub gst_mse::MediaSource);
impl MediaSource for Source {
    fn as_any(&self) -> &dyn Any {
        self
    }
    fn add_source_buffer(&self, mime: &str) -> Result<Arc<dyn SourceBuffer>> {
        self.0
            .add_source_buffer(mime)
            .map(|b| Arc::new(Buffer(b)) as Arc<dyn SourceBuffer>)
            .map_err(error)
    }
    fn remove_source_buffer(&self, buffer: &dyn SourceBuffer) -> Result<()> {
        let buffer = buffer
            .as_any()
            .downcast_ref::<Buffer>()
            .ok_or(MseError::Type)?;
        self.0.remove_source_buffer(&buffer.0).map_err(error)
    }
    fn duration(&self) -> f64 {
        self.0
            .duration()
            .map(|d| d.seconds_f64())
            .unwrap_or(f64::NAN)
    }
    fn set_duration(&self, duration: f64) -> Result<()> {
        // GST_CLOCK_TIME_NONE represents the unbounded live duration.
        self.0
            .set_duration(if duration == f64::INFINITY {
                None
            } else {
                Some(time(duration)?)
            })
            .map_err(error)
    }
    fn end_of_stream(&self, error_kind: Option<bool>) -> Result<()> {
        self.0
            .end_of_stream(match error_kind {
                None => gst_mse::MediaSourceEOSError::None,
                Some(true) => gst_mse::MediaSourceEOSError::Network,
                Some(false) => gst_mse::MediaSourceEOSError::Decode,
            })
            .map_err(error)
    }
    fn set_live_range(&self, range: Option<(f64, f64)>) -> Result<()> {
        match range {
            Some((start, end)) => self
                .0
                .set_live_seekable_range(time(start)?, time(end)?)
                .map_err(error),
            None => self.0.clear_live_seekable_range().map_err(error),
        }
    }
    fn detach(&self) {
        self.0.detach();
    }
}

struct Buffer(gst_mse::SourceBuffer);
// The signal closure retains a DOM completion only while an operation is pending.
// Connections use a weak native reference and are removed on completion/error.
fn operation(
    buffer: &gst_mse::SourceBuffer,
    done: Completion,
    start: impl FnOnce() -> std::result::Result<(), glib::Error>,
) -> Result<()> {
    let callback = Arc::new(Mutex::new(Some(done)));
    let handlers = Arc::new(Mutex::new(Vec::new()));
    let failed = Arc::new(std::sync::atomic::AtomicBool::new(false));
    let failed_clone = failed.clone();
    let error_id = buffer.connect_on_error(move |_| {
        failed_clone.store(true, std::sync::atomic::Ordering::Relaxed);
    });
    let weak = buffer.downgrade();
    let ids = handlers.clone();
    let completion = callback.clone();
    let end_id = buffer.connect_on_update_end(move |_| {
        if let Some(buffer) = weak.upgrade() {
            for id in ids.lock().unwrap().drain(..) {
                buffer.disconnect(id);
            }
        }
        let done = completion.lock().unwrap().take();
        if let Some(done) = done {
            done(if failed.load(std::sync::atomic::Ordering::Relaxed) {
                Err(MseError::Backend("MSE append failed".into()))
            } else {
                Ok(())
            });
        }
    });
    handlers.lock().unwrap().extend([error_id, end_id]);
    if let Err(e) = start() {
        for id in handlers.lock().unwrap().drain(..) {
            buffer.disconnect(id);
        }
        callback.lock().unwrap().take();
        return Err(error(e));
    }
    Ok(())
}
impl SourceBuffer for Buffer {
    fn as_any(&self) -> &dyn Any {
        self
    }
    fn buffered(&self) -> Result<Vec<(f64, f64)>> {
        // gstreamer-mse 0.25 omits this GArray-returning API. Keep the small
        // ownership adapter here; range computation remains in GStreamer.
        unsafe {
            let mut failure = std::ptr::null_mut();
            let ranges =
                gst_mse::ffi::gst_source_buffer_get_buffered(self.0.to_glib_none().0, &mut failure);
            if !failure.is_null() {
                return Err(error(from_glib_full(failure)));
            }
            if ranges.is_null() {
                return Err(MseError::Backend("No MSE ranges".into()));
            }
            let mut result = Vec::with_capacity((*ranges).len as usize);
            let data = (*ranges).data.cast::<gst_mse::ffi::GstMediaSourceRange>();
            for i in 0..(*ranges).len as usize {
                let range = &*data.add(i);
                result.push((range.start as f64 / 1e9, range.end as f64 / 1e9));
            }
            glib::ffi::g_array_unref(ranges);
            Ok(result)
        }
    }
    fn append(&self, data: Vec<u8>, done: Completion) -> Result<()> {
        operation(&self.0, done, || {
            self.0
                .append_buffer(gstreamer::Buffer::from_mut_slice(data))
        })
    }
    fn remove(&self, start: f64, end: f64, done: Completion) -> Result<()> {
        let start = time(start)?;
        let end = if end == f64::INFINITY {
            ClockTime::MAX
        } else {
            time(end)?
        };
        operation(&self.0, done, || self.0.remove(start, end))
    }
    fn abort(&self) -> Result<()> {
        self.0.abort().map_err(error)
    }
    fn change_type(&self, mime: &str) -> Result<()> {
        self.0.change_content_type(mime).map_err(error)
    }
    fn set_sequence(&self, sequence: bool) -> Result<()> {
        self.0
            .set_append_mode(if sequence {
                gst_mse::SourceBufferAppendMode::Sequence
            } else {
                gst_mse::SourceBufferAppendMode::Segments
            })
            .map_err(error)
    }
    fn set_timestamp_offset(&self, offset: f64) -> Result<()> {
        self.0.set_timestamp_offset(time(offset)?).map_err(error)
    }
    fn set_append_window(&self, start: f64, end: f64) -> Result<()> {
        self.0
            .set_append_window_start(time(start)?)
            .map_err(error)?;
        self.0
            .set_append_window_end(if end == f64::INFINITY {
                None
            } else {
                Some(time(end)?)
            })
            .map_err(error)
    }
}
