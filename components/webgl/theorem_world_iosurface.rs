/* This Source Code Form is subject to the terms of the Mozilla Public
 * License, v. 2.0. If a copy of the MPL was not distributed with this
 * file, You can obtain one at https://mozilla.org/MPL/2.0/. */
#![expect(unsafe_code)]

//! Runs only on the WebGL owner thread after DOM destination admission.
//! CGL imports BGRA IOSurface storage into a rectangle texture. A GPU framebuffer
//! blit converts that texture to the document-owned sampler2D texture; no bytes
//! are mapped, encoded, or read back. Completion is fenced with glFinish before
//! the native producer may recycle its lease.
use glow::{Context, HasContext};
use servo_base::theorem_world_gpu::{
    TheoremWorldFrame, TheoremWorldTextureBinding, TheoremWorldTextureError as Error,
};
use std::ffi::c_void;
use std::num::NonZeroU32;

#[link(name = "OpenGL", kind = "framework")]
unsafe extern "C" {
    fn CGLGetCurrentContext() -> *mut c_void;
    fn CGLTexImageIOSurface2D(
        context: *mut c_void,
        target: u32,
        internal_format: u32,
        width: i32,
        height: i32,
        format: u32,
        data_type: u32,
        surface: *const c_void,
        plane: u32,
    ) -> i32;
}
#[link(name = "IOSurface", kind = "framework")]
unsafe extern "C" {
    fn IOSurfaceGetWidth(surface: *const c_void) -> usize;
    fn IOSurfaceGetHeight(surface: *const c_void) -> usize;
    fn IOSurfaceGetBytesPerRow(surface: *const c_void) -> usize;
    fn IOSurfaceGetPixelFormat(surface: *const c_void) -> u32;
    fn IOSurfaceGetPlaneCount(surface: *const c_void) -> usize;
}

/// Restore state even when admission or framebuffer validation fails.
struct SavedState<'a> {
    gl: &'a Context,
    texture_2d: Option<glow::NativeTexture>,
    rectangle: Option<glow::NativeTexture>,
    read: Option<glow::NativeFramebuffer>,
    draw: Option<glow::NativeFramebuffer>,
    scissor: bool,
    temporary_texture: Option<glow::NativeTexture>,
    temporary_read: Option<glow::NativeFramebuffer>,
    temporary_draw: Option<glow::NativeFramebuffer>,
}
impl Drop for SavedState<'_> {
    fn drop(&mut self) {
        unsafe {
            self.gl.bind_texture(glow::TEXTURE_2D, self.texture_2d);
            self.gl
                .bind_texture(glow::TEXTURE_RECTANGLE, self.rectangle);
            self.gl.bind_framebuffer(glow::READ_FRAMEBUFFER, self.read);
            self.gl.bind_framebuffer(glow::DRAW_FRAMEBUFFER, self.draw);
            if self.scissor {
                self.gl.enable(glow::SCISSOR_TEST);
            }
            if let Some(fbo) = self.temporary_read {
                self.gl.delete_framebuffer(fbo);
            }
            if let Some(fbo) = self.temporary_draw {
                self.gl.delete_framebuffer(fbo);
            }
            if let Some(texture) = self.temporary_texture {
                self.gl.delete_texture(texture);
            }
        }
    }
}

pub(crate) fn import(
    gl: &Context,
    binding: &TheoremWorldTextureBinding,
    frame: &TheoremWorldFrame,
) -> Result<(), Error> {
    if gl.version().is_embedded {
        return Err(Error::Unsupported);
    }
    servo_base::theorem_world_gpu::validate_frame(binding, frame, 0)?;
    let context = unsafe { CGLGetCurrentContext() };
    if context.is_null() {
        return Err(Error::Unsupported);
    }
    // This pointer comes only from the admitted native producer lease. Never from JS or JSON.
    let surface = frame.surface_address as usize as *const c_void;
    let width = binding.source.width as i32;
    let height = binding.source.height as i32;
    unsafe {
        if IOSurfaceGetWidth(surface) != width as usize
            || IOSurfaceGetHeight(surface) != height as usize
            || IOSurfaceGetBytesPerRow(surface) as u64 != frame.bytes_per_row
            || IOSurfaceGetPixelFormat(surface) != frame.pixel_format
            || IOSurfaceGetPlaneCount(surface) != 0
        {
            return Err(Error::InvalidFrame);
        }
        let mut saved = SavedState {
            gl,
            texture_2d: gl.get_parameter_texture(glow::TEXTURE_BINDING_2D),
            rectangle: gl.get_parameter_texture(glow::TEXTURE_BINDING_RECTANGLE),
            read: gl.get_parameter_framebuffer(glow::READ_FRAMEBUFFER_BINDING),
            draw: gl.get_parameter_framebuffer(glow::DRAW_FRAMEBUFFER_BINDING),
            scissor: gl.is_enabled(glow::SCISSOR_TEST),
            temporary_texture: None,
            temporary_read: None,
            temporary_draw: None,
        };
        let texture =
            glow::NativeTexture(NonZeroU32::new(binding.texture).ok_or(Error::InvalidTexture)?);
        if !gl.is_texture(texture) {
            return Err(Error::InvalidTexture);
        }
        gl.bind_texture(glow::TEXTURE_2D, Some(texture));
        if gl.get_tex_level_parameter_i32(glow::TEXTURE_2D, 0, glow::TEXTURE_WIDTH) != width
            || gl.get_tex_level_parameter_i32(glow::TEXTURE_2D, 0, glow::TEXTURE_HEIGHT) != height
            || gl.get_tex_level_parameter_i32(glow::TEXTURE_2D, 0, glow::TEXTURE_INTERNAL_FORMAT)
                != glow::RGBA8 as i32
        {
            return Err(Error::InvalidTexture);
        }
        let imported = gl.create_texture().map_err(Error::Graphics)?;
        saved.temporary_texture = Some(imported);
        gl.bind_texture(glow::TEXTURE_RECTANGLE, Some(imported));
        let cgl_result = CGLTexImageIOSurface2D(
            context,
            glow::TEXTURE_RECTANGLE,
            glow::RGBA,
            width,
            height,
            glow::BGRA,
            glow::UNSIGNED_INT_8_8_8_8_REV,
            surface,
            0,
        );
        if cgl_result != 0 {
            return Err(Error::Graphics(format!(
                "CGL IOSurface import failed: {cgl_result}"
            )));
        }
        let read = gl.create_framebuffer().map_err(Error::Graphics)?;
        saved.temporary_read = Some(read);
        gl.bind_framebuffer(glow::READ_FRAMEBUFFER, Some(read));
        gl.framebuffer_texture_2d(
            glow::READ_FRAMEBUFFER,
            glow::COLOR_ATTACHMENT0,
            glow::TEXTURE_RECTANGLE,
            Some(imported),
            0,
        );
        gl.read_buffer(glow::COLOR_ATTACHMENT0);
        if gl.check_framebuffer_status(glow::READ_FRAMEBUFFER) != glow::FRAMEBUFFER_COMPLETE {
            return Err(Error::Graphics(
                "IOSurface read framebuffer incomplete".into(),
            ));
        }
        let draw = gl.create_framebuffer().map_err(Error::Graphics)?;
        saved.temporary_draw = Some(draw);
        gl.bind_framebuffer(glow::DRAW_FRAMEBUFFER, Some(draw));
        gl.framebuffer_texture_2d(
            glow::DRAW_FRAMEBUFFER,
            glow::COLOR_ATTACHMENT0,
            glow::TEXTURE_2D,
            Some(texture),
            0,
        );
        gl.draw_buffer(glow::COLOR_ATTACHMENT0);
        if gl.check_framebuffer_status(glow::DRAW_FRAMEBUFFER) != glow::FRAMEBUFFER_COMPLETE {
            return Err(Error::Graphics(
                "World destination framebuffer incomplete".into(),
            ));
        }
        gl.disable(glow::SCISSOR_TEST);
        // IOSurface rows begin at the top. The sampler2D texture uses OpenGL's bottom origin.
        gl.blit_framebuffer(
            0,
            height,
            width,
            0,
            0,
            0,
            width,
            height,
            glow::COLOR_BUFFER_BIT,
            glow::NEAREST,
        );
        // GPU synchronization only: no pixel readback. A later asynchronous fence can improve throughput.
        gl.finish();
        let error = gl.get_error();
        if error != glow::NO_ERROR {
            return Err(Error::Graphics(format!(
                "World GPU blit GL error: {error:#x}"
            )));
        }
        Ok(())
    }
}
