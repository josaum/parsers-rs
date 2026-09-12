//! Connected-component extraction for bitonal scanned PDF page images.

use anyhow::{anyhow, Result};

/// Tuning knobs for bitmap glyph candidate extraction.
#[derive(Debug, Clone, Copy)]
#[cfg_attr(feature = "serde", derive(serde::Serialize))]
pub struct BitmapComponentOptions {
    /// Pixels at or below this luma value are treated as ink.
    pub black_threshold: u8,
    /// Minimum component width in pixels.
    pub min_width: u32,
    /// Minimum component height in pixels.
    pub min_height: u32,
    /// Maximum component width in pixels.
    pub max_width: u32,
    /// Maximum component height in pixels.
    pub max_height: u32,
    /// Maximum width/height or height/width ratio before rejecting a component.
    pub max_aspect_ratio: f32,
    /// Minimum number of ink pixels in a component.
    pub min_pixels: u32,
    /// Vertical tolerance used when grouping glyph candidates into text lines.
    pub line_y_tolerance: u32,
}

impl Default for BitmapComponentOptions {
    fn default() -> Self {
        Self {
            black_threshold: 128,
            min_width: 1,
            min_height: 2,
            max_width: 256,
            max_height: 256,
            max_aspect_ratio: 16.0,
            min_pixels: 2,
            line_y_tolerance: 6,
        }
    }
}

/// One connected ink component that may represent a glyph or glyph fragment.
#[derive(Debug, Clone, PartialEq, Eq)]
#[cfg_attr(feature = "serde", derive(serde::Serialize))]
pub struct BitmapGlyphCandidate {
    /// Left edge in page pixel coordinates.
    pub x: u32,
    /// Top edge in page pixel coordinates.
    pub y: u32,
    /// Bounding-box width in pixels.
    pub width: u32,
    /// Bounding-box height in pixels.
    pub height: u32,
    /// Number of ink pixels in the connected component.
    pub pixel_count: u32,
}

impl BitmapGlyphCandidate {
    fn x_end(&self) -> u32 {
        self.x.saturating_add(self.width)
    }

    fn y_end(&self) -> u32 {
        self.y.saturating_add(self.height)
    }
}

/// A rough text-line grouping over bitmap glyph candidates.
#[derive(Debug, Clone, PartialEq, Eq)]
#[cfg_attr(feature = "serde", derive(serde::Serialize))]
pub struct BitmapTextLine {
    /// Top edge across all candidates in the line.
    pub y_min: u32,
    /// Bottom edge across all candidates in the line.
    pub y_max: u32,
    /// Indices into [`BitmapComponentPage::candidates`], sorted left-to-right.
    pub candidate_indices: Vec<usize>,
}

/// Connected-component summary for a scanned page image.
#[derive(Debug, Clone, PartialEq, Eq)]
#[cfg_attr(feature = "serde", derive(serde::Serialize))]
pub struct BitmapComponentPage {
    /// Page image width in pixels.
    pub width: u32,
    /// Page image height in pixels.
    pub height: u32,
    /// Filtered glyph-like connected components, sorted top-to-bottom then left-to-right.
    pub candidates: Vec<BitmapGlyphCandidate>,
    /// Rough line groups over the filtered candidates.
    pub lines: Vec<BitmapTextLine>,
}

/// Extract glyph-like connected components from an 8-bit luma image.
pub fn components_from_luma(
    width: u32,
    height: u32,
    pixels: &[u8],
    options: BitmapComponentOptions,
) -> Result<BitmapComponentPage> {
    let expected_len = usize::try_from(u64::from(width) * u64::from(height))
        .map_err(|_| anyhow!("bitmap dimensions overflow"))?;
    if pixels.len() != expected_len {
        return Err(anyhow!(
            "bitmap luma buffer has {} bytes, expected {expected_len}",
            pixels.len()
        ));
    }
    if width == 0 || height == 0 {
        return Ok(BitmapComponentPage {
            width,
            height,
            candidates: Vec::new(),
            lines: Vec::new(),
        });
    }

    let mut visited = vec![false; expected_len];
    let mut stack = Vec::with_capacity(256);
    let mut candidates = Vec::new();
    let stride = usize::try_from(width).map_err(|_| anyhow!("bitmap width overflow"))?;

    for start in 0..expected_len {
        if visited[start] || !is_ink(pixels[start], options.black_threshold) {
            continue;
        }

        visited[start] = true;
        stack.push(start);
        let mut min_x = width;
        let mut min_y = height;
        let mut max_x = 0u32;
        let mut max_y = 0u32;
        let mut pixel_count = 0u32;

        while let Some(index) = stack.pop() {
            let x = u32::try_from(index % stride).map_err(|_| anyhow!("bitmap x overflow"))?;
            let y = u32::try_from(index / stride).map_err(|_| anyhow!("bitmap y overflow"))?;
            min_x = min_x.min(x);
            min_y = min_y.min(y);
            max_x = max_x.max(x);
            max_y = max_y.max(y);
            pixel_count = pixel_count.saturating_add(1);

            let y0 = y.saturating_sub(1);
            let y1 = y.saturating_add(1).min(height - 1);
            let x0 = x.saturating_sub(1);
            let x1 = x.saturating_add(1).min(width - 1);
            for ny in y0..=y1 {
                let row = usize::try_from(ny).map_err(|_| anyhow!("bitmap row overflow"))? * stride;
                for nx in x0..=x1 {
                    if nx == x && ny == y {
                        continue;
                    }
                    let neighbor =
                        row + usize::try_from(nx).map_err(|_| anyhow!("bitmap col overflow"))?;
                    if !visited[neighbor] && is_ink(pixels[neighbor], options.black_threshold) {
                        visited[neighbor] = true;
                        stack.push(neighbor);
                    }
                }
            }
        }

        let candidate = BitmapGlyphCandidate {
            x: min_x,
            y: min_y,
            width: max_x.saturating_sub(min_x).saturating_add(1),
            height: max_y.saturating_sub(min_y).saturating_add(1),
            pixel_count,
        };
        if accepts_candidate(&candidate, options) {
            candidates.push(candidate);
        }
    }

    candidates.sort_by_key(|candidate| {
        (
            candidate.y,
            candidate.x,
            candidate.y_end(),
            candidate.x_end(),
        )
    });
    let lines = group_lines(&candidates, options.line_y_tolerance);
    Ok(BitmapComponentPage {
        width,
        height,
        candidates,
        lines,
    })
}

fn is_ink(pixel: u8, black_threshold: u8) -> bool {
    pixel <= black_threshold
}

fn accepts_candidate(candidate: &BitmapGlyphCandidate, options: BitmapComponentOptions) -> bool {
    if candidate.width < options.min_width
        || candidate.height < options.min_height
        || candidate.width > options.max_width
        || candidate.height > options.max_height
        || candidate.pixel_count < options.min_pixels
    {
        return false;
    }
    if options.max_aspect_ratio.is_finite() && options.max_aspect_ratio > 0.0 {
        let width = candidate.width as f32;
        let height = candidate.height as f32;
        let aspect = width.max(height) / width.min(height).max(1.0);
        if aspect > options.max_aspect_ratio {
            return false;
        }
    }
    true
}

fn group_lines(candidates: &[BitmapGlyphCandidate], tolerance: u32) -> Vec<BitmapTextLine> {
    if candidates.is_empty() {
        return Vec::new();
    }

    let mut heights = candidates
        .iter()
        .map(|candidate| candidate.height)
        .collect::<Vec<_>>();
    heights.sort_unstable();
    let median_height = heights[heights.len() / 2].max(1);
    let max_line_height = median_height
        .saturating_mul(3)
        .max(tolerance.saturating_mul(2).saturating_add(1))
        .min(96);

    let mut order = (0..candidates.len()).collect::<Vec<_>>();
    order.sort_by_key(|index| {
        let candidate = &candidates[*index];
        (candidate.y, candidate.y_end(), candidate.x)
    });

    let mut lines: Vec<BitmapTextLine> = Vec::new();
    for index in order {
        let candidate = &candidates[index];
        let mut best_line = None;
        let mut best_growth = u32::MAX;
        for (line_index, line) in lines.iter().enumerate() {
            let proposed_y_min = line.y_min.min(candidate.y);
            let proposed_y_max = line.y_max.max(candidate.y_end());
            let proposed_height = proposed_y_max.saturating_sub(proposed_y_min);
            let overlaps = candidate.y <= line.y_max.saturating_add(tolerance)
                && candidate.y_end().saturating_add(tolerance) >= line.y_min;
            if overlaps && proposed_height <= max_line_height {
                let growth = proposed_height.saturating_sub(line.y_max.saturating_sub(line.y_min));
                if growth < best_growth {
                    best_line = Some(line_index);
                    best_growth = growth;
                }
            }
        }

        if best_line.is_none() {
            for (line_index, line) in lines.iter().enumerate() {
                let distance = if candidate.y > line.y_max {
                    candidate.y - line.y_max
                } else {
                    line.y_min.saturating_sub(candidate.y_end())
                };
                if distance <= tolerance {
                    let proposed_y_min = line.y_min.min(candidate.y);
                    let proposed_y_max = line.y_max.max(candidate.y_end());
                    let proposed_height = proposed_y_max.saturating_sub(proposed_y_min);
                    if proposed_height <= max_line_height && distance < best_growth {
                        best_line = Some(line_index);
                        best_growth = distance;
                    }
                }
            }
        }

        if let Some(line_index) = best_line {
            let line = &mut lines[line_index];
            line.y_min = line.y_min.min(candidate.y);
            line.y_max = line.y_max.max(candidate.y_end());
            line.candidate_indices.push(index);
        } else {
            lines.push(BitmapTextLine {
                y_min: candidate.y,
                y_max: candidate.y_end(),
                candidate_indices: vec![index],
            });
        }
    }

    loop {
        let mut merged_any = false;
        'outer: for i in 0..lines.len() {
            for j in (i + 1)..lines.len() {
                let proposed_y_min = lines[i].y_min.min(lines[j].y_min);
                let proposed_y_max = lines[i].y_max.max(lines[j].y_max);
                let proposed_height = proposed_y_max.saturating_sub(proposed_y_min);
                let gap = if lines[j].y_min > lines[i].y_max {
                    lines[j].y_min - lines[i].y_max
                } else {
                    lines[i].y_min.saturating_sub(lines[j].y_max)
                };
                if gap <= tolerance && proposed_height <= max_line_height {
                    let mut other = lines.remove(j);
                    lines[i].y_min = proposed_y_min;
                    lines[i].y_max = proposed_y_max;
                    lines[i]
                        .candidate_indices
                        .append(&mut other.candidate_indices);
                    merged_any = true;
                    break 'outer;
                }
            }
        }
        if !merged_any {
            break;
        }
    }

    for line in &mut lines {
        line.candidate_indices.sort_by_key(|index| {
            let candidate = &candidates[*index];
            (candidate.x, candidate.y, candidate.x_end())
        });
    }
    lines.sort_by_key(|line| (line.y_min, line.y_max));
    lines
}
