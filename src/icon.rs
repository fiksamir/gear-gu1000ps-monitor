//! The app icon, drawn in code: a rounded square in the state colour with a
//! white lightning bolt. Used for the tray, the window and (as an .ico file)
//! the desktop shortcut.

/// The "all good" green, used for the shortcut icon.
pub const GREEN: [u8; 3] = [46, 160, 67];

/// Lightning bolt outline in unit coordinates (y down).
const BOLT: [(f32, f32); 7] = [
    (0.60, 0.10),
    (0.28, 0.55),
    (0.48, 0.55),
    (0.40, 0.90),
    (0.74, 0.43),
    (0.54, 0.43),
    (0.68, 0.10),
];
/// Samples per pixel side for anti-aliasing.
const AA: u32 = 4;

fn in_bolt(x: f32, y: f32) -> bool {
    let mut inside = false;
    let mut j = BOLT.len() - 1;
    for i in 0..BOLT.len() {
        let (xi, yi) = BOLT[i];
        let (xj, yj) = BOLT[j];
        if (yi > y) != (yj > y) && x < (xj - xi) * (y - yi) / (yj - yi) + xi {
            inside = !inside;
        }
        j = i;
    }
    inside
}

/// Inside a rounded square covering the unit square, corner radius `r`.
fn in_square(x: f32, y: f32, r: f32) -> bool {
    let dx = (r - x).max(x - (1.0 - r)).max(0.0);
    let dy = (r - y).max(y - (1.0 - r)).max(0.0);
    (0.0..=1.0).contains(&x) && (0.0..=1.0).contains(&y) && dx * dx + dy * dy <= r * r
}

/// `size`×`size` RGBA pixels, rows top to bottom.
pub fn rgba(rgb: [u8; 3], size: u32) -> Vec<u8> {
    let mut out = Vec::with_capacity((size * size * 4) as usize);
    for py in 0..size {
        for px in 0..size {
            let (mut bg, mut bolt) = (0u32, 0u32);
            for sy in 0..AA {
                for sx in 0..AA {
                    let x = (px as f32 + (sx as f32 + 0.5) / AA as f32) / size as f32;
                    let y = (py as f32 + (sy as f32 + 0.5) / AA as f32) / size as f32;
                    if in_square(x, y, 0.24) {
                        bg += 1;
                        if in_bolt(x, y) {
                            bolt += 1;
                        }
                    }
                }
            }
            let n = (AA * AA) as f32;
            // Lighter at the top, darker at the bottom.
            let shade = 1.15 - 0.3 * (py as f32 / size as f32);
            let w = bolt as f32 / bg.max(1) as f32;
            let mix = |c: u8| ((c as f32 * shade).min(255.0) * (1.0 - w) + 255.0 * w) as u8;
            out.extend_from_slice(&[
                mix(rgb[0]),
                mix(rgb[1]),
                mix(rgb[2]),
                (bg as f32 / n * 255.0) as u8,
            ]);
        }
    }
    out
}

/// A Windows .ico with 32-bit images of the given sizes (each at most 256).
pub fn ico(rgb: [u8; 3], sizes: &[u32]) -> Vec<u8> {
    let mut images = Vec::new();
    for &size in sizes {
        let pixels = rgba(rgb, size);
        let mut bmp = Vec::new();
        // BITMAPINFOHEADER; height counts the colour and the mask halves.
        for v in [40u32, size, size * 2] {
            bmp.extend_from_slice(&v.to_le_bytes());
        }
        bmp.extend_from_slice(&1u16.to_le_bytes());
        bmp.extend_from_slice(&32u16.to_le_bytes());
        bmp.extend_from_slice(&[0; 24]);
        for row in (0..size).rev() {
            for col in 0..size {
                let i = ((row * size + col) * 4) as usize;
                bmp.extend_from_slice(&[pixels[i + 2], pixels[i + 1], pixels[i], pixels[i + 3]]);
            }
        }
        // AND mask: all zero (alpha does the work), rows padded to 4 bytes.
        bmp.extend(std::iter::repeat_n(
            0u8,
            (size.div_ceil(32) * 4 * size) as usize,
        ));
        images.push((size, bmp));
    }
    let mut out = Vec::new();
    out.extend_from_slice(&[0, 0, 1, 0]);
    out.extend_from_slice(&(images.len() as u16).to_le_bytes());
    let mut offset = 6 + 16 * images.len() as u32;
    for (size, bmp) in &images {
        let dim = if *size >= 256 { 0 } else { *size as u8 };
        out.extend_from_slice(&[dim, dim, 0, 0]);
        out.extend_from_slice(&1u16.to_le_bytes());
        out.extend_from_slice(&32u16.to_le_bytes());
        out.extend_from_slice(&(bmp.len() as u32).to_le_bytes());
        out.extend_from_slice(&offset.to_le_bytes());
        offset += bmp.len() as u32;
    }
    for (_, bmp) in images {
        out.extend(bmp);
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn bolt_and_corners() {
        let p = rgba([46, 160, 67], 64);
        let at = |x: usize, y: usize| &p[(y * 64 + x) * 4..(y * 64 + x) * 4 + 4];
        assert_eq!(at(0, 0)[3], 0, "corner is transparent");
        assert_eq!(at(32, 40), &[255, 255, 255, 255], "bolt is white");
        assert_eq!(at(8, 32)[3], 255, "background is opaque");
    }

    #[test]
    fn ico_layout() {
        let data = ico([46, 160, 67], &[16, 32]);
        assert_eq!(&data[..6], &[0, 0, 1, 0, 2, 0]);
        let first = 6 + 32;
        assert_eq!(u32::from_le_bytes(data[18..22].try_into().unwrap()), first);
        assert_eq!(
            data.len(),
            first as usize + (40 + 16 * 16 * 4 + 64) + (40 + 32 * 32 * 4 + 128)
        );
    }
}
