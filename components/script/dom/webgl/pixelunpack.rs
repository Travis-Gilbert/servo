/* This Source Code Form is subject to the terms of the Mozilla Public
 * License, v. 2.0. If a copy of the MPL was not distributed with this
 * file, You can obtain one at https://mozilla.org/MPL/2.0/. */

//! The WebGL 2 pixel unpack parameters that place an image inside the array it is uploaded
//! from: `UNPACK_ROW_LENGTH`, `UNPACK_IMAGE_HEIGHT`, `UNPACK_SKIP_PIXELS`, `UNPACK_SKIP_ROWS`
//! and `UNPACK_SKIP_IMAGES`.
//!
//! A client-side upload copies its array to the GL thread, where the image is flipped or
//! premultiplied as tightly packed rows. The image is therefore cut out of the array here and
//! sent as rows that are padded to `UNPACK_ALIGNMENT` only, the same way an upload without
//! these parameters is sent.
//!
//! <https://registry.khronos.org/webgl/specs/latest/2.0/#PIXEL_STORE_PARAMETER_CONSTRAINTS>

use std::borrow::Cow;

/// The values of the pixel unpack parameters, which are all zero until they are set.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub(crate) struct PixelUnpack {
    pub(crate) row_length: u32,
    pub(crate) image_height: u32,
    pub(crate) skip_pixels: u32,
    pub(crate) skip_rows: u32,
    pub(crate) skip_images: u32,
}

/// The size of an image in pixels and of one of its pixels in bytes.
#[derive(Clone, Copy, Debug)]
pub(crate) struct ImageShape {
    pub(crate) width: u32,
    pub(crate) height: u32,
    /// The number of images, which is 1 for an upload to a 2D texture.
    pub(crate) depth: u32,
    /// Whether the upload is to a 3D texture or a 2D array texture, where the skipped rows have
    /// to lie inside the image height even when there is a single image.
    pub(crate) is_3d: bool,
    pub(crate) bytes_per_pixel: u32,
    /// `UNPACK_ALIGNMENT`.
    pub(crate) alignment: u32,
}

/// Where the pixels of an image are inside the memory it is uploaded from.
struct Layout {
    /// Bytes before the first pixel.
    skipped: usize,
    /// Bytes from the start of a row to the start of the next one.
    row_stride: usize,
    /// Bytes from the start of an image to the start of the next one.
    image_stride: usize,
    /// Bytes of memory the pixels need, counting from its start.
    required: usize,
}

#[derive(Debug, Eq, PartialEq)]
pub(crate) enum UnpackError {
    InvalidOperation,
    OutOfMemory,
}

impl PixelUnpack {
    /// Whether the image is stored the way it is when none of the parameters is set.
    pub(crate) fn is_default(&self) -> bool {
        *self == Self::default()
    }

    /// The bytes of the image that are needed in `data`, or `None` when the parameters do not
    /// fit the image, which is an `INVALID_OPERATION`.
    fn layout(&self, shape: ImageShape) -> Option<Layout> {
        let ImageShape {
            width,
            height,
            depth,
            is_3d,
            bytes_per_pixel,
            alignment,
        } = shape;

        // The data store is as wide and as high as the image unless the parameters say it is
        // larger, and the skipped pixels and rows have to lie inside of it.
        let store_width = if self.row_length > 0 {
            self.row_length
        } else {
            width
        };
        let store_height = if self.image_height > 0 {
            self.image_height
        } else {
            height
        };
        if u64::from(self.skip_pixels) + u64::from(width) > u64::from(store_width) {
            return None;
        }
        if is_3d && u64::from(self.skip_rows) + u64::from(height) > u64::from(store_height) {
            return None;
        }

        let alignment = alignment.max(1) as u64;
        let bytes_per_pixel = bytes_per_pixel as u64;
        let row_bytes = (store_width as u64).checked_mul(bytes_per_pixel)?;
        let row_stride = row_bytes.checked_next_multiple_of(alignment)?;
        let image_stride = row_stride.checked_mul(store_height as u64)?;
        let skipped = (self.skip_images as u64)
            .checked_mul(image_stride)?
            .checked_add((self.skip_rows as u64).checked_mul(row_stride)?)?
            .checked_add((self.skip_pixels as u64).checked_mul(bytes_per_pixel)?)?;

        // Padding after the last row is not needed, so that a single byte is enough for a
        // 1x1 image of one byte.
        let required = if width == 0 || height == 0 || depth == 0 {
            0
        } else {
            let last_row = (width as u64).checked_mul(bytes_per_pixel)?;
            skipped
                .checked_add(image_stride.checked_mul(depth as u64 - 1)?)?
                .checked_add(row_stride.checked_mul(height as u64 - 1)?)?
                .checked_add(last_row)?
        };

        Some(Layout {
            skipped: usize::try_from(skipped).ok()?,
            row_stride: usize::try_from(row_stride).ok()?,
            image_stride: usize::try_from(image_stride).ok()?,
            required: usize::try_from(required).ok()?,
        })
    }

    /// The bytes of memory that an image of this shape needs, counting from the start of the
    /// array, or `None` when the parameters do not fit the image.
    pub(crate) fn required_len(&self, shape: ImageShape) -> Option<usize> {
        self.layout(shape).map(|layout| layout.required)
    }

    /// The image cut out of `data` as rows padded to `UNPACK_ALIGNMENT`, one image after the
    /// other. Invalid layouts and allocation failures remain distinct WebGL errors.
    pub(crate) fn unpack<'a>(
        &self,
        data: &'a [u8],
        shape: ImageShape,
    ) -> Result<Cow<'a, [u8]>, UnpackError> {
        let layout = self.layout(shape).ok_or(UnpackError::InvalidOperation)?;
        if data.len() < layout.required {
            return Err(UnpackError::InvalidOperation);
        }
        if self.is_default() {
            return Ok(Cow::Borrowed(data));
        }

        let ImageShape {
            width,
            height,
            depth,
            bytes_per_pixel,
            alignment,
            ..
        } = shape;
        if width == 0 || height == 0 || depth == 0 {
            return Ok(Cow::Borrowed(&[]));
        }
        let row_bytes = (width as usize)
            .checked_mul(bytes_per_pixel as usize)
            .ok_or(UnpackError::OutOfMemory)?;
        let packed_row_stride = row_bytes
            .checked_next_multiple_of(alignment.max(1) as usize)
            .ok_or(UnpackError::OutOfMemory)?;
        let packed_len = packed_row_stride
            .checked_mul(height as usize)
            .ok_or(UnpackError::OutOfMemory)?
            .checked_mul(depth as usize)
            .ok_or(UnpackError::OutOfMemory)?;
        let mut packed = zeroed_pixels(packed_len).ok_or(UnpackError::OutOfMemory)?;
        if row_bytes == 0 {
            return Ok(Cow::Owned(packed));
        }

        for image in 0..depth as usize {
            for row in 0..height as usize {
                let from = layout.skipped + image * layout.image_stride + row * layout.row_stride;
                let to = (image * height as usize + row) * packed_row_stride;
                packed[to..to + row_bytes].copy_from_slice(&data[from..from + row_bytes]);
            }
        }
        Ok(Cow::Owned(packed))
    }
}

/// Allocate pixel storage without aborting on a recoverable allocation failure.
pub(crate) fn zeroed_pixels(length: usize) -> Option<Vec<u8>> {
    let mut pixels = Vec::new();
    pixels.try_reserve_exact(length).ok()?;
    pixels.resize(length, 0);
    Some(pixels)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn shape_2d(width: u32, height: u32, bytes_per_pixel: u32, alignment: u32) -> ImageShape {
        ImageShape {
            width,
            height,
            depth: 1,
            is_3d: false,
            bytes_per_pixel,
            alignment,
        }
    }

    fn shape_3d(width: u32, height: u32, depth: u32, bytes_per_pixel: u32, alignment: u32) -> ImageShape {
        ImageShape {
            depth,
            is_3d: true,
            ..shape_2d(width, height, bytes_per_pixel, alignment)
        }
    }

    #[test]
    fn default_parameters_keep_the_data() {
        let data = [1, 2, 3, 4, 5, 6, 7, 8];
        let unpacked = PixelUnpack::default()
            .unpack(&data, shape_2d(2, 2, 2, 1))
            .unwrap();
        assert!(matches!(unpacked, Cow::Borrowed(_)));
        assert_eq!(&*unpacked, &data);
    }

    #[test]
    fn data_that_is_too_small_is_refused() {
        // Two rows of two one-byte pixels, rows aligned to 4: 4 + 2 bytes are needed.
        let unpack = PixelUnpack::default();
        assert_eq!(unpack.required_len(shape_2d(2, 2, 1, 4)), Some(6));
        assert!(unpack.unpack(&[0; 5], shape_2d(2, 2, 1, 4)).is_err());
        assert!(unpack.unpack(&[0; 6], shape_2d(2, 2, 1, 4)).is_ok());
    }

    #[test]
    fn row_length_and_skips_cut_out_a_sub_image() {
        // A 4x3 store of one-byte pixels holding 0..12, of which the 2x2 image at (1, 1).
        let data: Vec<u8> = (0..12).collect();
        let unpack = PixelUnpack {
            row_length: 4,
            skip_pixels: 1,
            skip_rows: 1,
            ..Default::default()
        };
        let unpacked = unpack.unpack(&data, shape_2d(2, 2, 1, 1)).unwrap();
        assert_eq!(&*unpacked, &[5, 6, 9, 10]);

        // Padded to the alignment of 4 the rows are four bytes each.
        let unpacked = unpack.unpack(&data, shape_2d(2, 2, 1, 4)).unwrap();
        assert_eq!(&*unpacked, &[5, 6, 0, 0, 9, 10, 0, 0]);
    }

    #[test]
    fn skipped_pixels_must_fit_the_row() {
        let unpack = PixelUnpack {
            row_length: 4,
            skip_pixels: 3,
            ..Default::default()
        };
        assert!(unpack.required_len(shape_2d(2, 1, 1, 1)).is_none());
        let unpack = PixelUnpack {
            skip_pixels: 1,
            ..Default::default()
        };
        // Without a row length the store is as wide as the image.
        assert!(unpack.required_len(shape_2d(2, 1, 1, 1)).is_none());
    }

    #[test]
    fn images_are_taken_from_the_image_height() {
        // Two 2x1 images of one-byte pixels, three rows apart: 0 1 | - - | - - | 6 7 ...
        let data: Vec<u8> = (0..16).collect();
        let unpack = PixelUnpack {
            image_height: 3,
            skip_images: 1,
            ..Default::default()
        };
        let unpacked = unpack.unpack(&data, shape_3d(2, 1, 2, 1, 1)).unwrap();
        // Skipping one image skips 3 rows of 2 bytes; the second image is 6 bytes further.
        assert_eq!(&*unpacked, &[6, 7, 12, 13]);
    }

    #[test]
    fn skipped_rows_must_fit_the_image_height_of_a_3d_upload() {
        let unpack = PixelUnpack {
            skip_rows: 1,
            ..Default::default()
        };
        assert!(unpack.required_len(shape_3d(2, 2, 1, 1, 1)).is_none());
        assert!(unpack.required_len(shape_3d(2, 2, 2, 1, 1)).is_none());
        assert!(unpack.required_len(shape_2d(2, 2, 1, 1)).is_some());
    }
    #[test]
    fn volume_length_does_not_wrap_at_four_gibibytes() {
        let expected = usize::try_from(34_359_738_368u64).ok();
        assert_eq!(
            PixelUnpack::default().required_len(shape_3d(2048, 2048, 2048, 4, 4)),
            expected
        );
        assert!(
            PixelUnpack::default()
                .unpack(&[], shape_3d(2048, 2048, 2048, 4, 4))
                .is_err()
        );
    }

    #[test]
    fn unrepresentable_volume_and_allocation_fail() {
        assert!(
            PixelUnpack::default()
                .required_len(shape_3d(u32::MAX, u32::MAX, u32::MAX, 16, 8))
                .is_none()
        );
        assert!(zeroed_pixels(usize::MAX).is_none());
    }

    #[test]
    fn empty_volume_does_not_allocate_or_copy_skipped_rows() {
        let unpack = PixelUnpack {
            skip_images: 1,
            ..Default::default()
        };
        assert!(
            unpack
                .unpack(&[], shape_3d(2, 0, 2, 4, 4))
                .unwrap()
                .is_empty()
        );
    }
}
