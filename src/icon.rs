//! Procedurally drawn app icon (circular "restart" arrow on a blue tile).
//! Generated in code so the portable EXE needs no external asset files.

/// RGBA pixels of a `size`×`size` icon.
pub fn rgba(size: u32) -> Vec<u8> {
    const SS: u32 = 4; // supersampling per axis, for anti-aliased edges
    let s = size as f32;
    let mut out = Vec::with_capacity((size * size * 4) as usize);

    for py in 0..size {
        for px in 0..size {
            let (mut tile, mut glyph) = (0u32, 0u32);
            for sy in 0..SS {
                for sx in 0..SS {
                    // Normalised coordinates in [0, 1], y pointing up.
                    let x = (px as f32 + (sx as f32 + 0.5) / SS as f32) / s;
                    let y = 1.0 - (py as f32 + (sy as f32 + 0.5) / SS as f32) / s;
                    if in_rounded_square(x, y) {
                        tile += 1;
                        if in_arrow(x, y) {
                            glyph += 1;
                        }
                    }
                }
            }
            let n = (SS * SS) as f32;
            let a = tile as f32 / n;
            let g = if tile > 0 { glyph as f32 / tile as f32 } else { 0.0 };
            // Blend white glyph over #2563EB.
            let mix = |bg: f32| (bg + (255.0 - bg) * g).round() as u8;
            out.extend_from_slice(&[mix(37.0), mix(99.0), mix(235.0), (a * 255.0).round() as u8]);
        }
    }
    out
}

fn in_rounded_square(x: f32, y: f32) -> bool {
    let (r, m) = (0.22, 0.02); // corner radius, outer margin
    let cx = x.clamp(m + r, 1.0 - m - r);
    let cy = y.clamp(m + r, 1.0 - m - r);
    (x - cx).powi(2) + (y - cy).powi(2) <= r * r && x >= m && x <= 1.0 - m && y >= m && y <= 1.0 - m
}

fn in_arrow(x: f32, y: f32) -> bool {
    let (cx, cy) = (0.5, 0.5);
    let (dx, dy) = (x - cx, y - cy);
    let dist = (dx * dx + dy * dy).sqrt();
    let angle = dy.atan2(dx).to_degrees(); // 0° = east, 90° = north

    // Ring with a gap between 0° and 70° (upper right).
    let (r_in, r_out) = (0.21, 0.31);
    let ring = dist >= r_in && dist <= r_out && !(0.0..70.0).contains(&angle);

    // Arrowhead at the 70° end of the ring, pointing clockwise into the gap.
    let t = 70f32.to_radians();
    let (ux, uy) = (t.cos(), t.sin());
    let base_in = (cx + ux * 0.13, cy + uy * 0.13);
    let base_out = (cx + ux * 0.39, cy + uy * 0.39);
    let mid = (r_in + r_out) / 2.0;
    let tip = (cx + ux * mid + t.sin() * 0.16, cy + uy * mid - t.cos() * 0.16);
    ring || in_triangle((x, y), base_in, base_out, tip)
}

fn in_triangle(p: (f32, f32), a: (f32, f32), b: (f32, f32), c: (f32, f32)) -> bool {
    let sign = |p1: (f32, f32), p2: (f32, f32), p3: (f32, f32)| {
        (p1.0 - p3.0) * (p2.1 - p3.1) - (p2.0 - p3.0) * (p1.1 - p3.1)
    };
    let (d1, d2, d3) = (sign(p, a, b), sign(p, b, c), sign(p, c, a));
    let neg = d1 < 0.0 || d2 < 0.0 || d3 < 0.0;
    let pos = d1 > 0.0 || d2 > 0.0 || d3 > 0.0;
    !(neg && pos)
}

#[cfg(test)]
mod tests {
    /// Writes target/icon_preview_{32,256}.png for visual inspection.
    #[test]
    fn preview() {
        let _ = std::fs::create_dir_all("target");
        for size in [32u32, 256] {
            let img = image::RgbaImage::from_raw(size, size, super::rgba(size)).unwrap();
            img.save(format!("target/icon_preview_{size}.png")).unwrap();
        }
    }
}
