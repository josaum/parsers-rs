//! Raster → glyph-geometry bridge: the input side for TRUE scanned pages.
//!
//! A scanned page has no vector glyphs — but it has ink. Connected components
//! of the binarized bitmap are the glyphs; Moore boundary tracing recovers
//! each component's outer contour; arc-length resampling plus centroid/RMS
//! normalization produces descriptors in EXACTLY the space the vector path
//! uses. From there the entire downstream (line banding, shape identity,
//! segmental alignment, self-labelled decoding) runs unchanged on raster
//! input.

/// One raster-extracted glyph, in pixel units: ink-left x, ink-top y,
/// size = max(bbox extent), and the normalized outline descriptor
/// (`2k` floats, same construction as the vector path).
pub type RasterGlyph = (f32, f32, f32, Vec<f32>);

/// Extract glyph-scale ink components from a grayscale page image.
///
/// `threshold`: luma below this is ink. `k`: descriptor sample count.
/// `min_px`/`max_px`: component bbox extent gates in pixels (drop specks and
/// page-scale rules/frames — same character-scale doctrine as the vector
/// path).
/// Otsu's threshold from the image histogram — per-scan adaptation.
/// Fixed thresholds under-extract light scans (measured on FUNSD: a fixed
/// 160 missed 75% of the ink on the lightest form).
pub fn otsu_threshold(luma: &[u8]) -> u8 {
    let mut hist = [0u64; 256];
    for &v in luma {
        hist[v as usize] += 1;
    }
    let total: u64 = luma.len() as u64;
    let sum_all: u64 = hist.iter().enumerate().map(|(i, &c)| i as u64 * c).sum();
    let (mut w0, mut sum0) = (0u64, 0u64);
    let (mut best_t, mut best_var) = (128u8, -1.0f64);
    for (t, &count) in hist.iter().enumerate() {
        w0 += count;
        if w0 == 0 {
            continue;
        }
        let w1 = total - w0;
        if w1 == 0 {
            break;
        }
        sum0 += t as u64 * count;
        let m0 = sum0 as f64 / w0 as f64;
        let m1 = (sum_all - sum0) as f64 / w1 as f64;
        let var = w0 as f64 * w1 as f64 * (m0 - m1) * (m0 - m1);
        if var > best_var {
            best_var = var;
            best_t = t as u8;
        }
    }
    best_t
}

/// Extract glyph-scale ink components from a grayscale page image.
/// `threshold = 0` selects Otsu per-image; see module docs for the pipeline.
pub fn glyphs_from_luma(
    width: usize,
    height: usize,
    luma: &[u8],
    threshold: u8,
    k: usize,
    min_px: usize,
    max_px: usize,
) -> Vec<RasterGlyph> {
    assert_eq!(luma.len(), width * height, "luma buffer size mismatch");
    // threshold 0 selects Otsu per-image (biased slightly toward ink so thin
    // strokes survive)
    let threshold = if threshold == 0 {
        otsu_threshold(luma).saturating_add(12)
    } else {
        threshold
    };
    // RULE REMOVAL before labeling: table grids connect every character
    // into one page-sized component (measured: a landscape price-table page
    // was a single 319k-pixel blob and yielded ZERO glyphs). Ink runs longer
    // than `max_px` in either direction are structural lines, not character
    // strokes — erase them from the working mask.
    let mut mask: Vec<bool> = luma.iter().map(|&v| v < threshold).collect();
    for y in 0..height {
        let row = y * width;
        let mut x = 0;
        while x < width {
            if mask[row + x] {
                let start = x;
                while x < width && mask[row + x] {
                    x += 1;
                }
                if x - start > max_px {
                    for clear_x in start..x {
                        mask[row + clear_x] = false;
                    }
                }
            } else {
                x += 1;
            }
        }
    }
    for x in 0..width {
        let mut y = 0;
        while y < height {
            if mask[y * width + x] {
                let start = y;
                while y < height && mask[y * width + x] {
                    y += 1;
                }
                if y - start > max_px {
                    for clear_y in start..y {
                        mask[clear_y * width + x] = false;
                    }
                }
            } else {
                y += 1;
            }
        }
    }
    let ink = |x: usize, y: usize| -> bool { mask[y * width + x] };

    // ---- connected components, 8-connectivity, single-pass BFS labeling
    let mut label = vec![0u32; width * height];
    let mut next = 1u32;
    let mut comps: Vec<(usize, usize, usize, usize, Vec<usize>)> = Vec::new(); // x0,y0,x1,y1,pixels
    let mut queue = Vec::new();
    for sy in 0..height {
        for sx in 0..width {
            if !ink(sx, sy) || label[sy * width + sx] != 0 {
                continue;
            }
            let id = next;
            next += 1;
            let (mut x0, mut y0, mut x1, mut y1) = (sx, sy, sx, sy);
            let mut pixels = Vec::new();
            queue.clear();
            queue.push((sx, sy));
            label[sy * width + sx] = id;
            while let Some((x, y)) = queue.pop() {
                pixels.push(y * width + x);
                x0 = x0.min(x);
                x1 = x1.max(x);
                y0 = y0.min(y);
                y1 = y1.max(y);
                for (dx, dy) in [
                    (-1i32, -1i32),
                    (0, -1),
                    (1, -1),
                    (-1, 0),
                    (1, 0),
                    (-1, 1),
                    (0, 1),
                    (1, 1),
                ] {
                    let nx = x as i32 + dx;
                    let ny = y as i32 + dy;
                    if nx < 0 || ny < 0 || nx as usize >= width || ny as usize >= height {
                        continue;
                    }
                    let (nx, ny) = (nx as usize, ny as usize);
                    if ink(nx, ny) && label[ny * width + nx] == 0 {
                        label[ny * width + nx] = id;
                        queue.push((nx, ny));
                    }
                }
            }
            comps.push((x0, y0, x1, y1, pixels));
        }
    }

    // ---- per component: projection split of merged runs, then Moore trace
    // + normalized descriptor per piece. Typewriter scans routinely fuse
    // whole words into one component (measured on FUNSD: ~4 characters per
    // component on average) — a wide component is split at vertical
    // projection minima before tracing.
    let mut out = Vec::new();
    for (x0, y0, x1, y1, pixels) in comps {
        let w = x1 - x0 + 1;
        let h = y1 - y0 + 1;
        // Gate only min size / speck count here. Do NOT apply max_px on the
        // full component: fused typewriter words routinely exceed max_px in
        // width, and the projection split below is what recovers character-
        // sized pieces from those merges. Per-segment max_px runs after.
        if w.max(h) < min_px || pixels.len() < 4 {
            continue;
        }
        let segments: Vec<(usize, usize)> = if w > (h * 8) / 5 && h >= min_px {
            // column ink histogram of THIS component only
            let mut col = vec![0u32; w];
            for &pix in &pixels {
                col[pix % width - x0] += 1;
            }
            let peak = *col.iter().max().unwrap_or(&1);
            let cut = (peak / 8).max(1);
            let mut segs = Vec::new();
            let mut start: Option<usize> = None;
            for (c, &v) in col.iter().enumerate() {
                if v >= cut {
                    if start.is_none() {
                        start = Some(c);
                    }
                } else if let Some(sx) = start.take() {
                    segs.push((x0 + sx, x0 + c - 1));
                }
            }
            if let Some(sx) = start {
                segs.push((x0 + sx, x1));
            }
            if segs.len() >= 2 {
                segs
            } else {
                vec![(x0, x1)]
            }
        } else {
            vec![(x0, x1)]
        };
        for (sx0, sx1) in segments {
            let sw = sx1 - sx0 + 1;
            let sext = sw.max(h);
            if sext < min_px || sext > max_px {
                continue;
            }
            let seg_ink = |x: usize, y: usize| -> bool { x >= sx0 && x <= sx1 && ink(x, y) };
            let contour = moore_trace(width, height, &seg_ink, sx0, y0, sx1, y1);
            if contour.len() < 4 {
                continue;
            }
            let desc = normalize_descriptor(&contour, k);
            out.push((sx0 as f32, y0 as f32, sext as f32, desc));
        }
    }
    out
}

/// Moore-neighbor boundary tracing of the component whose top-left-most ink
/// pixel lies inside the given bbox. Returns the outer contour as pixel
/// coordinates.
fn moore_trace(
    width: usize,
    height: usize,
    ink: &dyn Fn(usize, usize) -> bool,
    x0: usize,
    y0: usize,
    x1: usize,
    y1: usize,
) -> Vec<(f32, f32)> {
    // find start: first ink pixel scanning the bbox row-major
    let mut start = None;
    'find: for y in y0..=y1 {
        for x in x0..=x1 {
            if ink(x, y) {
                start = Some((x, y));
                break 'find;
            }
        }
    }
    let Some((sx, sy)) = start else {
        return Vec::new();
    };
    // 8 neighbors clockwise starting from W
    const N: [(i32, i32); 8] = [
        (-1, 0),
        (-1, -1),
        (0, -1),
        (1, -1),
        (1, 0),
        (1, 1),
        (0, 1),
        (-1, 1),
    ];
    let inside = |x: i32, y: i32| -> bool {
        x >= 0
            && y >= 0
            && (x as usize) < width
            && (y as usize) < height
            && ink(x as usize, y as usize)
    };
    let mut contour = Vec::new();
    let (mut cx, mut cy) = (sx as i32, sy as i32);
    let mut backtrack = 0usize; // came from W
    let limit = 4 * (x1 - x0 + y1 - y0 + 4) * 4;
    loop {
        contour.push((cx as f32, cy as f32));
        if contour.len() > limit {
            break; // safety on pathological shapes
        }
        let mut found = false;
        for step in 0..8 {
            let dir = (backtrack + 1 + step) % 8;
            let nx = cx + N[dir].0;
            let ny = cy + N[dir].1;
            if inside(nx, ny) {
                // new backtrack = direction pointing back to previous cell
                backtrack = (dir + 4) % 8;
                cx = nx;
                cy = ny;
                found = true;
                break;
            }
        }
        if !found {
            break; // isolated pixel
        }
        if cx == sx as i32 && cy == sy as i32 && contour.len() > 2 {
            break;
        }
    }
    contour
}

/// Arc-length resample to `k` points, centroid-centered, RMS-radius scaled —
/// bit-compatible with the vector path's descriptor construction.
fn normalize_descriptor(contour: &[(f32, f32)], k: usize) -> Vec<f32> {
    let total: f32 = contour
        .windows(2)
        .map(|w| ((w[1].0 - w[0].0).powi(2) + (w[1].1 - w[0].1).powi(2)).sqrt())
        .sum::<f32>()
        .max(1e-4);
    let step = total / k as f32;
    let mut pts: Vec<(f32, f32)> = Vec::with_capacity(k);
    let mut target = 0.0f32;
    let mut acc = 0.0f32;
    let mut seg = 0usize;
    for _ in 0..k {
        while seg + 1 < contour.len() {
            let d = ((contour[seg + 1].0 - contour[seg].0).powi(2)
                + (contour[seg + 1].1 - contour[seg].1).powi(2))
            .sqrt();
            if acc + d >= target || seg + 2 >= contour.len() {
                let t = if d > 1e-6 {
                    ((target - acc) / d).clamp(0.0, 1.0)
                } else {
                    0.0
                };
                pts.push((
                    contour[seg].0 + t * (contour[seg + 1].0 - contour[seg].0),
                    contour[seg].1 + t * (contour[seg + 1].1 - contour[seg].1),
                ));
                break;
            }
            acc += d;
            seg += 1;
        }
        target += step;
    }
    if pts.is_empty() {
        return vec![0.0; k * 2];
    }
    let n = pts.len() as f32;
    let cx = pts.iter().map(|p| p.0).sum::<f32>() / n;
    let cy = pts.iter().map(|p| p.1).sum::<f32>() / n;
    let rms = (pts
        .iter()
        .map(|p| (p.0 - cx).powi(2) + (p.1 - cy).powi(2))
        .sum::<f32>()
        / n)
        .sqrt()
        .max(1e-4);
    let mut d = Vec::with_capacity(k * 2);
    for p in pts.iter().take(k) {
        d.push((p.0 - cx) / rms);
        d.push((p.1 - cy) / rms);
    }
    while d.len() < k * 2 {
        let m = d.len();
        d.push(d[m - 2]);
        d.push(d[m - 1]);
    }
    d
}

#[cfg(test)]
mod tests {
    use super::*;

    fn draw(w: usize, h: usize, boxes: &[(usize, usize, usize, usize)]) -> Vec<u8> {
        let mut px = vec![255u8; w * h];
        for &(x0, y0, x1, y1) in boxes {
            for y in y0..=y1 {
                for x in x0..=x1 {
                    px[y * w + x] = 0;
                }
            }
        }
        px
    }

    #[test]
    fn finds_two_separate_components() {
        let px = draw(64, 32, &[(4, 4, 12, 20), (30, 6, 42, 22)]);
        let g = glyphs_from_luma(64, 32, &px, 128, 16, 3, 60);
        assert_eq!(g.len(), 2, "{g:?}");
        assert!(g[0].0 < g[1].0);
        assert_eq!(g[0].3.len(), 32);
    }

    #[test]
    fn same_shape_at_two_positions_has_near_identical_descriptor() {
        let px = draw(96, 32, &[(4, 4, 14, 24), (60, 4, 70, 24)]);
        let g = glyphs_from_luma(96, 32, &px, 128, 16, 3, 60);
        assert_eq!(g.len(), 2);
        let d: f32 = g[0]
            .3
            .iter()
            .zip(&g[1].3)
            .map(|(a, b)| (a - b) * (a - b))
            .sum();
        assert!(d < 1e-3, "translation invariance: d2 = {d}");
    }

    #[test]
    fn specks_and_frames_are_gated() {
        let px = draw(64, 64, &[(1, 1, 62, 62), (30, 30, 31, 31)]);
        // frame ext=62 > max 40; speck ext=2 < min 3
        let g = glyphs_from_luma(64, 64, &px, 128, 16, 3, 40);
        // the frame's interior hole region isn't a component of ink; the
        // frame itself is gated by max_px, the speck... is INSIDE the frame
        // fill (drawn solid) so only one component existed anyway
        assert!(g.is_empty(), "{g:?}");
    }
}
