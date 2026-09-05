use std::{ffi::c_void, ptr::null_mut, slice};

use windows::Win32::Graphics::Gdi::{
    BI_RGB, BITMAPINFO, BITMAPINFOHEADER, BitBlt, CAPTUREBLT, CreateCompatibleDC, CreateDIBSection,
    DIB_RGB_COLORS, DeleteDC, DeleteObject, GdiFlush, HBITMAP, HDC, HGDIOBJ, ReleaseDC, SRCCOPY,
    SelectObject,
};

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct ReadbackBounds {
    min_x: i32,
    min_y: i32,
    width: i32,
    height: i32,
    byte_len: usize,
}

#[derive(Default)]
pub(super) struct CaptionReadback {
    memory_dc: HDC,
    bitmap: HBITMAP,
    previous_object: HGDIOBJ,
    bits: *mut c_void,
    width: i32,
    height: i32,
}

impl CaptionReadback {
    pub(super) fn read(&mut self, points: &[(i32, i32)]) -> Option<Vec<u32>> {
        let bounds = bounds(points)?;

        let screen_dc = unsafe { windows::Win32::Graphics::Gdi::GetDC(None) };
        if screen_dc.0.is_null() {
            return None;
        }
        let result = self.read_from_dc(screen_dc, points, bounds);
        unsafe {
            let _ = ReleaseDC(None, screen_dc);
        }
        if result.is_none() {
            // A display/device change can invalidate cached GDI resources.
            // Recreate them on the next read instead of retaining a failed DC.
            *self = Self::default();
        }
        result
    }

    fn read_from_dc(
        &mut self,
        screen_dc: HDC,
        points: &[(i32, i32)],
        bounds: ReadbackBounds,
    ) -> Option<Vec<u32>> {
        if self.memory_dc.0.is_null() {
            self.memory_dc = unsafe { CreateCompatibleDC(Some(screen_dc)) };
            if self.memory_dc.0.is_null() {
                return None;
            }
        }
        if self.width != bounds.width || self.height != bounds.height || self.bitmap.0.is_null() {
            self.replace_bitmap(bounds.width, bounds.height)?;
        }

        if unsafe {
            BitBlt(
                self.memory_dc,
                0,
                0,
                bounds.width,
                bounds.height,
                Some(screen_dc),
                bounds.min_x,
                bounds.min_y,
                SRCCOPY | CAPTUREBLT,
            )
        }
        .is_err()
            || !unsafe { GdiFlush().as_bool() }
        {
            return None;
        }

        let raw = unsafe { slice::from_raw_parts(self.bits.cast::<u8>(), bounds.byte_len) };
        read_colors(bounds, raw, points)
    }

    fn replace_bitmap(&mut self, width: i32, height: i32) -> Option<()> {
        if !self.bitmap.0.is_null() {
            unsafe {
                let _ = SelectObject(self.memory_dc, self.previous_object);
                let _ = DeleteObject(self.bitmap.into());
            }
            self.bitmap = HBITMAP::default();
            self.bits = null_mut();
        }

        let info = BITMAPINFO {
            bmiHeader: BITMAPINFOHEADER {
                biSize: std::mem::size_of::<BITMAPINFOHEADER>() as u32,
                biWidth: width,
                biHeight: height.checked_neg()?,
                biPlanes: 1,
                biBitCount: 32,
                biCompression: BI_RGB.0,
                ..Default::default()
            },
            ..Default::default()
        };
        let mut bits = null_mut();
        let bitmap = unsafe {
            CreateDIBSection(
                Some(self.memory_dc),
                &info,
                DIB_RGB_COLORS,
                &mut bits,
                None,
                0,
            )
        }
        .ok()?;
        let previous = unsafe { SelectObject(self.memory_dc, bitmap.into()) };
        if invalid_gdi_object(previous) || bits.is_null() {
            unsafe {
                if !invalid_gdi_object(previous) {
                    let _ = SelectObject(self.memory_dc, previous);
                }
                let _ = DeleteObject(bitmap.into());
            }
            return None;
        }
        self.bitmap = bitmap;
        self.previous_object = previous;
        self.bits = bits;
        self.width = width;
        self.height = height;
        Some(())
    }
}

impl Drop for CaptionReadback {
    fn drop(&mut self) {
        unsafe {
            if !self.memory_dc.0.is_null() {
                if !self.bitmap.0.is_null() {
                    let _ = SelectObject(self.memory_dc, self.previous_object);
                    let _ = DeleteObject(self.bitmap.into());
                }
                let _ = DeleteDC(self.memory_dc);
            }
        }
    }
}

fn invalid_gdi_object(object: HGDIOBJ) -> bool {
    object.0.is_null() || object.0 as isize == -1
}

fn bounds(points: &[(i32, i32)]) -> Option<ReadbackBounds> {
    let &(first_x, first_y) = points.first()?;
    let mut result = (first_x, first_x, first_y, first_y);
    for &(x, y) in &points[1..] {
        result.0 = result.0.min(x);
        result.1 = result.1.max(x);
        result.2 = result.2.min(y);
        result.3 = result.3.max(y);
    }
    let width = result.1.checked_sub(result.0)?.checked_add(1)?;
    let height = result.3.checked_sub(result.2)?.checked_add(1)?;
    let pixel_count = usize::try_from(width)
        .ok()?
        .checked_mul(usize::try_from(height).ok()?)?;
    let byte_len = pixel_count.checked_mul(4)?;
    if byte_len > isize::MAX as usize {
        return None;
    }
    Some(ReadbackBounds {
        min_x: result.0,
        min_y: result.2,
        width,
        height,
        byte_len,
    })
}

fn read_colors(bounds: ReadbackBounds, raw: &[u8], points: &[(i32, i32)]) -> Option<Vec<u32>> {
    if raw.len() < bounds.byte_len {
        return None;
    }
    let width = usize::try_from(bounds.width).ok()?;
    points
        .iter()
        .map(|&(x, y)| {
            let dx = usize::try_from(x.checked_sub(bounds.min_x)?).ok()?;
            let dy = usize::try_from(y.checked_sub(bounds.min_y)?).ok()?;
            if dx >= width || dy >= usize::try_from(bounds.height).ok()? {
                return None;
            }
            let index = dy.checked_mul(width)?.checked_add(dx)?;
            let offset = index.checked_mul(4)?;
            let pixel = raw.get(offset..offset.checked_add(4)?)?;
            Some(u32::from(pixel[2]) << 16 | u32::from(pixel[1]) << 8 | u32::from(pixel[0]))
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::{ReadbackBounds, bounds, read_colors};

    #[test]
    fn read_colors_handles_negative_coordinates_and_bgra_order() {
        let points = [(-2, -1), (0, 0)];
        let bounds = bounds(&points).unwrap();
        let mut raw = vec![0_u8; bounds.byte_len];
        raw[0..4].copy_from_slice(&[0x33, 0x22, 0x11, 0]);
        let second = (bounds.width as usize + 2) * 4;
        raw[second..second + 4].copy_from_slice(&[0xcc, 0xbb, 0xaa, 0]);
        assert_eq!(
            read_colors(bounds, &raw, &points),
            Some(vec![0x112233, 0xaabbcc])
        );
    }

    #[test]
    fn read_colors_rejects_out_of_bounds_points_and_short_buffers() {
        let bounds = ReadbackBounds {
            min_x: -4,
            min_y: 8,
            width: 2,
            height: 2,
            byte_len: 16,
        };
        assert!(read_colors(bounds, &[0; 15], &[(-4, 8)]).is_none());
        assert!(read_colors(bounds, &[0; 16], &[(-2, 8)]).is_none());
    }

    #[test]
    fn bounds_rejects_empty_and_integer_overflow() {
        assert!(bounds(&[]).is_none());
        assert!(bounds(&[(i32::MIN, 0), (i32::MAX, 0)]).is_none());
        assert!(bounds(&[(0, i32::MIN), (0, i32::MAX)]).is_none());
    }
}
