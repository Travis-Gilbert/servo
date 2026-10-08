/* This Source Code Form is subject to the terms of the Mozilla Public
 * License, v. 2.0. If a copy of the MPL was not distributed with this
 * file, You can obtain one at https://mozilla.org/MPL/2.0/. */

//! Native-only Theorem World texture grants. No value is exposed through WebIDL.
//! Shared here because embedder, script, and WebGL must authenticate the same grant.

use crate::id::{PipelineId, WebViewId};
use serde::{Deserialize, Serialize};

/// Producer identity qualified by the GPUI surface registry and allocation generation.
#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct TheoremWorldSource {
    pub registry: u64,
    pub surface: u64,
    pub resource_generation: u64,
    pub width: u32,
    pub height: u32,
}

/// A grant issued by the owning document after resolving actual DOM GPU objects.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct TheoremWorldTextureBinding {
    pub webview: WebViewId,
    pub document: PipelineId,
    pub presentation_epoch: u64,
    pub challenge: String,
    pub context: u64,
    pub texture: u32,
    pub source: TheoremWorldSource,
}

/// Metadata for an IOSurface retained by the native caller until completion.
/// `surface_address` is in-process engine-private data, never accepted from page JSON.
/// The embedding API refuses multiprocess mode before dispatching this metadata.
#[derive(Clone, Debug, Deserialize, Serialize)]
pub struct TheoremWorldFrame {
    pub surface_address: u64,
    pub source: TheoremWorldSource,
    pub frame_generation: u64,
    pub transport_resource_generation: u64,
    pub bytes_per_row: u64,
    pub pixel_format: u32,
    pub premultiplied_alpha: bool,
}

/// Private operations dispatched only by the embedding WebView API.
#[derive(Clone, Debug, Deserialize, Serialize)]
pub enum TheoremWorldTextureRequest {
    Register {
        origin: String,
        challenge: String,
        presentation_epoch: u64,
        source: TheoremWorldSource,
    },
    Import {
        binding: TheoremWorldTextureBinding,
        frame: TheoremWorldFrame,
    },
    /// Check exact active document/context/texture and completed frame without GPU work.
    Validate {
        binding: TheoremWorldTextureBinding,
    },
    Revoke {
        binding: TheoremWorldTextureBinding,
    },
}

/// The callback is delivered only after the GPU has finished consuming the frame.
#[derive(Clone, Debug, Deserialize, Serialize)]
pub struct TheoremWorldTextureReceipt {
    pub binding: TheoremWorldTextureBinding,
    pub completed_frame: Option<u64>,
}

/// Refusals preserve the producer lease and never authorize a partial binding.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub enum TheoremWorldTextureError {
    Unsupported,
    DocumentUnavailable,
    OriginMismatch,
    InvalidBootstrap,
    StaleBinding,
    InvalidTexture,
    InvalidFrame,
    Graphics(String),
}

/// Validate monotonic producer stamps before dereferencing an IOSurface.
pub fn validate_frame(
    binding: &TheoremWorldTextureBinding,
    frame: &TheoremWorldFrame,
    previous_frame: u64,
) -> Result<(), TheoremWorldTextureError> {
    if frame.source != binding.source
        || frame.surface_address == 0
        || frame.frame_generation <= previous_frame
        || frame.transport_resource_generation == 0
        || frame.pixel_format != u32::from_be_bytes(*b"BGRA")
        || !frame.premultiplied_alpha
        || frame.source.width == 0
        || frame.source.height == 0
        || frame.bytes_per_row < u64::from(frame.source.width) * 4
        || frame.source.width > i32::MAX as u32
        || frame.source.height > i32::MAX as u32
    {
        return Err(TheoremWorldTextureError::InvalidFrame);
    }
    Ok(())
}
