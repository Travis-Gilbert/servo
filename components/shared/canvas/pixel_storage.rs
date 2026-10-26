/* This Source Code Form is subject to the terms of the Mozilla Public
 * License, v. 2.0. If a copy of the MPL was not distributed with this
 * file, You can obtain one at https://mozilla.org/MPL/2.0/. */

/// The largest dimension at a mip level, including untrusted signed levels.
pub fn mip_size(size: u32, level: i32) -> Option<u32> {
    size.checked_shr(u32::try_from(level).ok()?)
}

/// Array layers do not contribute to the mip chain length.
pub fn mip_levels(width: u32, height: u32, depth: u32, is_3d: bool) -> u32 {
    let dimension = if is_3d {
        width.max(height).max(depth)
    } else {
        width.max(height)
    };
    dimension.checked_ilog2().map_or(0, |level| level + 1)
}

/// Storage read by GL with aligned rows and no padding after the final row.
pub fn required_bytes(
    width: u32,
    height: u32,
    depth: u32,
    cpp: u32,
    alignment: u32,
) -> Option<usize> {
    if width == 0 || height == 0 || depth == 0 {
        return Some(0);
    }
    let row = u64::from(width).checked_mul(u64::from(cpp))?;
    let stride = row.checked_next_multiple_of(u64::from(alignment.max(1)))?;
    let rows = u64::from(height).checked_mul(u64::from(depth))?;
    usize::try_from(stride.checked_mul(rows - 1)?.checked_add(row)?).ok()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn page_supplied_levels_cannot_overflow() {
        for level in [i32::MIN, -1, 32, 33, i32::MAX] {
            assert_eq!(mip_size(2048, level), None);
        }
        assert_eq!(mip_size(2048, 0), Some(2048));
        assert_eq!(mip_size(2048, 11), Some(1));
        assert_eq!(mip_size(2048, 31), Some(0));
    }

    #[test]
    fn volumes_larger_than_u32_do_not_wrap() {
        assert_eq!(
            required_bytes(2048, 2048, 2048, 4, 4),
            usize::try_from(34_359_738_368u64).ok()
        );
        assert_eq!(required_bytes(u32::MAX, u32::MAX, u32::MAX, 16, 8), None);
        assert_eq!(required_bytes(1, 1, 1, 1, 8), Some(1));
        assert_eq!(required_bytes(1, 2, 2, 1, 8), Some(25));
        assert_eq!(required_bytes(0, u32::MAX, u32::MAX, 16, 8), Some(0));
    }
    #[test]
    fn array_layers_do_not_add_mip_levels() {
        assert_eq!(mip_levels(2, 2, 256, false), 2);
        assert_eq!(mip_levels(2, 2, 256, true), 9);
        assert_eq!(mip_levels(3, 5, 1, false), 3);
    }
}
