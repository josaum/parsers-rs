//! Page-level tessellation: from per-glyph Voronoi adjacency to semantic regions.
//!
//! Pipeline:
//! 1. [`build_adjacency`] turns glyphs into a k-NN Delaunay-like graph.
//! 2. [`converge`] merges glyphs → words → lines → blocks via energy-thresholded
//!    union-find, honoring drawn-line / cell barriers from [`PathConstraints`].
//! 3. [`assign_roles`] labels each [`AnnotatedRegion`] with a [`SemanticRole`]
//!    (Title, Heading, ListItem, Footer, …) based on relative font size, weight,
//!    and page position.
//! 4. [`PageExtraction::to_rdf_triples`] serialises the result as RDF triples
//!    for downstream KB ingestion.

#[cfg(feature = "serde")]
use serde::Serialize;
use std::collections::HashMap;

/// A glyph site for Voronoi construction.
#[derive(Debug, Clone)]
#[cfg_attr(feature = "serde", derive(Serialize))]
pub struct GlyphSite {
    /// Stable index into the parent glyph array.
    pub index: usize,
    /// Glyph left edge in page space.
    pub x: f64,
    /// Glyph baseline y in page space.
    pub y: f64,
    /// Glyph right edge in page space.
    pub x_end: f64,
    /// Glyph nominal font size in points.
    pub font_size: f64,
    /// Glyph cluster text (usually one grapheme).
    pub text: String,
    /// PostScript-style font name when known.
    pub font_name: Option<String>,
}

/// Edge in the Delaunay adjacency graph between two glyph sites.
#[derive(Debug, Clone)]
pub struct DelaunayEdge {
    /// Site index of one endpoint.
    pub a: usize,
    /// Site index of the other endpoint.
    pub b: usize,
    /// Euclidean distance between sites (page units).
    pub distance: f64,
    /// Signed x-delta from site `a` to site `b`.
    pub dx: f64,
    /// Signed y-delta from site `a` to site `b`.
    pub dy: f64,
    /// Classified edge geometry — drives the merge ordering.
    pub edge_type: EdgeType,
}

/// Geometric classification of a Delaunay edge between two glyph sites.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[cfg_attr(feature = "serde", derive(Serialize))]
pub enum EdgeType {
    /// Horizontal neighbor, small Y delta — same line candidate
    Inline,
    /// Vertical neighbor, small X delta — same column candidate
    Stack,
    /// Diagonal or large gap — likely cross-structure
    Diagonal,
}

/// A region: a merged set of glyph sites that converges toward
/// a semantic unit (word → line → cell → block).
#[derive(Debug, Clone)]
#[cfg_attr(feature = "serde", derive(Serialize))]
pub struct Region {
    /// Region identifier (stable for a single convergence pass).
    pub id: usize,
    /// Indices of the glyph sites that make up this region.
    pub site_indices: Vec<usize>,
    /// Bounding-box left edge in page space.
    pub x_min: f64,
    /// Bounding-box top edge in page space.
    pub y_min: f64,
    /// Bounding-box right edge in page space.
    pub x_max: f64,
    /// Bounding-box bottom edge in page space.
    pub y_max: f64,
    /// Centroid x (mean of site mid-points).
    pub centroid_x: f64,
    /// Centroid y (mean of site baselines).
    pub centroid_y: f64,
    /// Modal font size across the region's sites.
    pub dominant_font_size: f64,
    /// How far up the merge ladder this region has climbed.
    pub level: MergeLevel,
}

/// Granularity of a [`Region`] in the convergence hierarchy.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
#[cfg_attr(feature = "serde", derive(Serialize))]
pub enum MergeLevel {
    /// A single glyph site, not yet merged.
    Glyph,
    /// Glyphs merged into a word-shaped run.
    Word,
    /// Words merged into a horizontal text line.
    Line,
    /// Lines merged inside a single table cell.
    Cell,
    /// Higher-level grouping (paragraph, header block, …).
    Block,
}

/// Convergence energy for a candidate merge.
/// Lower energy = more likely to merge.
#[derive(Debug, Clone, Copy)]
struct MergeCandidate {
    region_a: usize,
    region_b: usize,
    energy: f64,
    merge_level: MergeLevel,
}

/// Configuration for tessellation convergence.
#[derive(Debug, Clone, Copy)]
pub struct TessellationOptions {
    /// Weight for horizontal distance in energy function.
    pub w_horizontal: f64,
    /// Weight for vertical distance.
    pub w_vertical: f64,
    /// Weight for font size mismatch.
    pub w_font_mismatch: f64,
    /// Weight for font name mismatch.
    pub w_font_name_mismatch: f64,
    /// Energy threshold for word merges (inline glyphs → word).
    pub word_threshold: f64,
    /// Energy threshold for line merges (words → line).
    pub line_threshold: f64,
    /// Energy threshold for cell/block merges (lines → block).
    pub block_threshold: f64,
}

impl Default for TessellationOptions {
    fn default() -> Self {
        Self {
            w_horizontal: 1.0,
            w_vertical: 3.0,
            w_font_mismatch: 5.0,
            w_font_name_mismatch: 2.0,
            word_threshold: 0.4,
            line_threshold: 0.6,
            block_threshold: 1.8,
        }
    }
}

/// Build the Delaunay adjacency graph from glyph sites.
pub fn build_adjacency(sites: &[GlyphSite], k: usize) -> Vec<DelaunayEdge> {
    let mut edges: Vec<DelaunayEdge> = Vec::new();
    let mut seen: HashMap<(usize, usize), bool> = HashMap::new();

    let mut sorted_indices: Vec<usize> = (0..sites.len()).collect();
    sorted_indices.sort_by(|&a, &b| {
        sites[a]
            .y
            .total_cmp(&sites[b].y)
            .then(sites[a].x.total_cmp(&sites[b].x))
    });

    for (pos, &i) in sorted_indices.iter().enumerate() {
        let si = &sites[i];

        let window_start = pos.saturating_sub(k * 4);
        let window_end = (pos + k * 4).min(sorted_indices.len());

        let mut neighbors: Vec<(usize, f64)> = sorted_indices[window_start..window_end]
            .iter()
            .filter(|&&j| j != i)
            .map(|&j| {
                let sj = &sites[j];
                let dx = sj.x - si.x;
                let dy = sj.y - si.y;
                let dist = (dx * dx + dy * dy).sqrt();
                (j, dist)
            })
            .collect();

        neighbors.sort_by(|a, b| a.1.total_cmp(&b.1));
        neighbors.truncate(k);

        for (j, distance) in neighbors {
            let key = if i < j { (i, j) } else { (j, i) };
            if seen.contains_key(&key) {
                continue;
            }
            seen.insert(key, true);

            let dx = sites[j].x - sites[i].x;
            let dy = sites[j].y - sites[i].y;
            let fs = sites[i].font_size.max(sites[j].font_size).max(1.0);

            let edge_type = classify_edge(dx, dy, fs);

            edges.push(DelaunayEdge {
                a: i,
                b: j,
                distance,
                dx,
                dy,
                edge_type,
            });
        }
    }

    edges
}

fn classify_edge(dx: f64, dy: f64, font_size: f64) -> EdgeType {
    let abs_dx = dx.abs() / font_size;
    let abs_dy = dy.abs() / font_size;

    if abs_dy < 0.5 && abs_dx < 5.0 {
        EdgeType::Inline
    } else if abs_dx < 1.5 && abs_dy < 3.0 {
        EdgeType::Stack
    } else {
        EdgeType::Diagonal
    }
}

/// Compute merge energy between two regions connected by an edge.
fn merge_energy(
    ra: &Region,
    rb: &Region,
    edge: &DelaunayEdge,
    sites: &[GlyphSite],
    options: &TessellationOptions,
) -> f64 {
    let fs = ra.dominant_font_size.max(rb.dominant_font_size).max(1.0);

    let dx_norm = edge.dx.abs() / fs;
    let dy_norm = edge.dy.abs() / fs;

    let size_ratio = if ra.dominant_font_size > 0.0 && rb.dominant_font_size > 0.0 {
        let r = ra.dominant_font_size / rb.dominant_font_size;
        (r - 1.0).abs()
    } else {
        0.0
    };

    let name_mismatch = if edge.a < sites.len() && edge.b < sites.len() {
        let na = &sites[edge.a].font_name;
        let nb = &sites[edge.b].font_name;
        if na != nb {
            1.0
        } else {
            0.0
        }
    } else {
        0.0
    };

    options.w_horizontal * dx_norm
        + options.w_vertical * dy_norm
        + options.w_font_mismatch * size_ratio
        + options.w_font_name_mismatch * name_mismatch
}

/// Union-Find for efficient region merging.
struct UnionFind {
    parent: Vec<usize>,
    rank: Vec<usize>,
}

impl UnionFind {
    fn new(n: usize) -> Self {
        Self {
            parent: (0..n).collect(),
            rank: vec![0; n],
        }
    }

    fn find(&mut self, mut x: usize) -> usize {
        while self.parent[x] != x {
            self.parent[x] = self.parent[self.parent[x]];
            x = self.parent[x];
        }
        x
    }

    fn union(&mut self, a: usize, b: usize) -> usize {
        let ra = self.find(a);
        let rb = self.find(b);
        if ra == rb {
            return ra;
        }
        if self.rank[ra] < self.rank[rb] {
            self.parent[ra] = rb;
            rb
        } else if self.rank[ra] > self.rank[rb] {
            self.parent[rb] = ra;
            ra
        } else {
            self.parent[rb] = ra;
            self.rank[ra] += 1;
            ra
        }
    }
}

/// Merge glyph sites into [`Region`]s using energy-thresholded union-find.
///
/// Runs three passes (Word → Line → Block), each filtering [`DelaunayEdge`]s
/// by the preferred [`EdgeType`] for that level and accepting merges whose
/// energy is at or below the corresponding threshold in `options`. Returns
/// the surviving regions (one per union-find root).
pub fn converge(
    sites: &[GlyphSite],
    edges: &[DelaunayEdge],
    options: TessellationOptions,
    constraints: &PathConstraints,
) -> Vec<Region> {
    let n = sites.len();
    if n == 0 {
        return Vec::new();
    }

    let mut uf = UnionFind::new(n);
    let mut regions: Vec<Region> = sites
        .iter()
        .enumerate()
        .map(|(i, s)| Region {
            id: i,
            site_indices: vec![i],
            x_min: s.x,
            y_min: s.y,
            x_max: s.x_end,
            y_max: s.y,
            centroid_x: (s.x + s.x_end) / 2.0,
            centroid_y: s.y,
            dominant_font_size: s.font_size,
            level: MergeLevel::Glyph,
        })
        .collect();

    let levels = [
        (MergeLevel::Word, options.word_threshold, EdgeType::Inline),
        (MergeLevel::Line, options.line_threshold, EdgeType::Inline),
        (MergeLevel::Block, options.block_threshold, EdgeType::Stack),
    ];

    for (level, threshold, preferred_edge_type) in &levels {
        let mut candidates: Vec<MergeCandidate> = edges
            .iter()
            .filter(|e| e.edge_type == *preferred_edge_type || *level == MergeLevel::Block)
            .filter_map(|e| {
                let ra = uf.find(e.a);
                let rb = uf.find(e.b);
                if ra == rb {
                    return None;
                }

                let base = merge_energy(&regions[ra], &regions[rb], e, sites, &options);
                let penalty = barrier_penalty(&regions[ra], &regions[rb], constraints);
                let energy = base + penalty;

                if energy <= *threshold {
                    Some(MergeCandidate {
                        region_a: ra,
                        region_b: rb,
                        energy,
                        merge_level: *level,
                    })
                } else {
                    None
                }
            })
            .collect();

        candidates.sort_by(|a, b| a.energy.total_cmp(&b.energy));

        for candidate in candidates {
            let ra = uf.find(candidate.region_a);
            let rb = uf.find(candidate.region_b);
            if ra == rb {
                continue;
            }

            let root = uf.union(ra, rb);
            let other = if root == ra { rb } else { ra };

            let other_sites = std::mem::take(&mut regions[other].site_indices);
            regions[root].site_indices.extend(other_sites);
            regions[root].x_min = regions[root].x_min.min(regions[other].x_min);
            regions[root].y_min = regions[root].y_min.min(regions[other].y_min);
            regions[root].x_max = regions[root].x_max.max(regions[other].x_max);
            regions[root].y_max = regions[root].y_max.max(regions[other].y_max);
            regions[root].level = candidate.merge_level;

            let count = regions[root].site_indices.len() as f64;
            regions[root].centroid_x = regions[root]
                .site_indices
                .iter()
                .map(|&i| (sites[i].x + sites[i].x_end) / 2.0)
                .sum::<f64>()
                / count;
            regions[root].centroid_y = regions[root]
                .site_indices
                .iter()
                .map(|&i| sites[i].y)
                .sum::<f64>()
                / count;

            let mut size_counts: HashMap<u32, usize> = HashMap::new();
            for &si in &regions[root].site_indices {
                let key = (sites[si].font_size * 10.0) as u32;
                *size_counts.entry(key).or_default() += 1;
            }
            regions[root].dominant_font_size = size_counts
                .into_iter()
                .max_by_key(|(_, c)| *c)
                .map(|(k, _)| k as f64 / 10.0)
                .unwrap_or(12.0);
        }
    }

    let mut final_regions: HashMap<usize, &Region> = HashMap::new();
    for i in 0..n {
        let root = uf.find(i);
        final_regions.entry(root).or_insert(&regions[root]);
    }

    final_regions
        .into_values()
        .filter(|r| !r.site_indices.is_empty())
        .cloned()
        .collect()
}

// --- Integration with tessellation convergence ---

/// Drawn-line and cell constraints that shape merge energies during
/// convergence. Lines act as hard barriers; cell rectangles bias merges
/// inward and forbid cross-cell merges.
#[derive(Debug, Clone)]
pub struct PathConstraints {
    /// Horizontal rule lines that prevent vertical merges across them.
    pub h_barriers: Vec<HBarrier>,
    /// Vertical rule lines that prevent horizontal merges across them.
    pub v_barriers: Vec<VBarrier>,
    /// Cell rectangles for explicit-border table cells.
    pub cells: Vec<CellConstraint>,
}

/// Horizontal drawn line acting as a vertical-merge barrier.
#[derive(Debug, Clone)]
pub struct HBarrier {
    /// Y coordinate of the line in page space.
    pub y: f64,
    /// Left endpoint x coordinate.
    pub x_start: f64,
    /// Right endpoint x coordinate.
    pub x_end: f64,
}

/// Vertical drawn line acting as a horizontal-merge barrier.
#[derive(Debug, Clone)]
pub struct VBarrier {
    /// X coordinate of the line in page space.
    pub x: f64,
    /// Top endpoint y coordinate.
    pub y_start: f64,
    /// Bottom endpoint y coordinate.
    pub y_end: f64,
}

/// Axis-aligned cell rectangle for explicit-border table inference.
#[derive(Debug, Clone)]
pub struct CellConstraint {
    /// Left edge of the cell in page space.
    pub x: f64,
    /// Top edge of the cell in page space.
    pub y: f64,
    /// Right edge of the cell in page space.
    pub x_end: f64,
    /// Bottom edge of the cell in page space.
    pub y_end: f64,
}

fn ranges_overlap(a0: f64, a1: f64, b0: f64, b1: f64, tolerance: f64) -> bool {
    a0 - tolerance <= b1 && b0 - tolerance <= a1
}

/// Compute the constraint-induced energy penalty for merging two regions.
///
/// Returns `f64::INFINITY` when the merge would cross a drawn barrier or
/// straddle two distinct cells, a small bonus when both regions live inside
/// the same cell, a fixed penalty when only one region is in a cell, and
/// zero otherwise.
pub fn barrier_penalty(ra: &Region, rb: &Region, constraints: &PathConstraints) -> f64 {
    for barrier in &constraints.h_barriers {
        let ya = ra.centroid_y;
        let yb = rb.centroid_y;
        let (y_lo, y_hi) = if ya < yb { (ya, yb) } else { (yb, ya) };

        if barrier.y > y_lo && barrier.y < y_hi {
            let x_lo = ra.x_min.min(rb.x_min);
            let x_hi = ra.x_max.max(rb.x_max);
            if ranges_overlap(barrier.x_start, barrier.x_end, x_lo, x_hi, 1.0) {
                return f64::INFINITY;
            }
        }
    }

    for barrier in &constraints.v_barriers {
        let xa = ra.centroid_x;
        let xb = rb.centroid_x;
        let (x_lo, x_hi) = if xa < xb { (xa, xb) } else { (xb, xa) };

        if barrier.x > x_lo && barrier.x < x_hi {
            let y_lo = ra.y_min.min(rb.y_min);
            let y_hi = ra.y_max.max(rb.y_max);
            if ranges_overlap(barrier.y_start, barrier.y_end, y_lo, y_hi, 1.0) {
                return f64::INFINITY;
            }
        }
    }

    let cell_a = find_containing_cell(ra, constraints);
    let cell_b = find_containing_cell(rb, constraints);

    match (cell_a, cell_b) {
        (Some(a), Some(b)) if a == b => -0.5,
        (Some(_), Some(_)) => f64::INFINITY,
        (Some(_), None) | (None, Some(_)) => 2.0,
        (None, None) => 0.0,
    }
}

/// Returns the index of the first `constraints.cells` rectangle that fully
/// contains the region's centroid, or `None` if no cell does.
pub fn find_containing_cell(region: &Region, constraints: &PathConstraints) -> Option<usize> {
    constraints.cells.iter().position(|cell| {
        region.centroid_x >= cell.x
            && region.centroid_x <= cell.x_end
            && region.centroid_y >= cell.y
            && region.centroid_y <= cell.y_end
    })
}

// ── Semantic annotation ──────────────────────────────────────────────

/// Every region gets a semantic role after convergence.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[cfg_attr(feature = "serde", derive(Serialize))]
pub enum SemanticRole {
    /// Document or section title (largest, often bold).
    Title,
    /// Section heading (h1/h2-style).
    Heading,
    /// Sub-heading nested under a heading.
    Subheading,
    /// Body paragraph at the dominant font size.
    Paragraph,
    /// Item in a bulleted or numbered list.
    ListItem,
    /// Caption for a figure, table, or image.
    Caption,
    /// Footnote, end-note, or similar marginalia.
    Footnote,
    /// Page-number marker on header/footer band.
    PageNumber,
    /// Repeating header band at the top of the page.
    Header,
    /// Repeating footer band at the bottom of the page.
    Footer,
    /// Cell in a tabular grid.
    TableCell,
    /// Header cell in a tabular grid.
    TableHeader,
    /// Monospace code block.
    CodeBlock,
    /// Block-level quotation.
    Quote,
    /// Form-style label paired with a value.
    Label,
    /// Form-style value paired with a label.
    Value,
    /// Could not be classified.
    Unknown,
}

/// Font classification beyond just the name string.
#[derive(Debug, Clone)]
#[cfg_attr(feature = "serde", derive(Serialize))]
pub struct FontInfo {
    /// Full font name as encountered in the PDF.
    pub name: String,
    /// Font name with the subset-tag prefix stripped (e.g. `ABCDEF+Helvetica` → `Helvetica`).
    pub base_name: String,
    /// Heuristic: name implies a bold weight.
    pub is_bold: bool,
    /// Heuristic: name implies an italic / oblique style.
    pub is_italic: bool,
    /// Heuristic: name implies a monospace face.
    pub is_monospace: bool,
    /// Heuristic: name implies a serif face.
    pub is_serif: bool,
    /// Quantised weight derived from the name.
    pub weight: FontWeight,
    /// Style (italic / oblique / normal) derived from the name.
    pub style: FontStyle,
}

/// CSS-style font weight, mapped from a PDF font name.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[cfg_attr(feature = "serde", derive(Serialize))]
pub enum FontWeight {
    /// Thin / Hairline.
    Thin,
    /// Light.
    Light,
    /// Regular / Book.
    Regular,
    /// Medium.
    Medium,
    /// SemiBold / Demi.
    SemiBold,
    /// Bold.
    Bold,
    /// ExtraBold / Ultra.
    ExtraBold,
    /// Black / Heavy.
    Black,
}

/// CSS-style font style, mapped from a PDF font name.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[cfg_attr(feature = "serde", derive(Serialize))]
pub enum FontStyle {
    /// Upright.
    Normal,
    /// Italic (typically a hand-drawn cursive variant).
    Italic,
    /// Oblique (slanted upright glyphs).
    Oblique,
}

/// Parse a PDF font name into structured info.
pub fn classify_font(name: &str) -> FontInfo {
    let lower = name.to_lowercase();

    let base_name = if name.len() > 7 && name.as_bytes()[6] == b'+' {
        &name[7..]
    } else {
        name
    };

    let is_bold = lower.contains("bold")
        || lower.contains("-b")
        || lower.contains("black")
        || lower.contains("heavy")
        || lower.contains("demi");

    let is_italic = lower.contains("italic")
        || lower.contains("oblique")
        || lower.contains("-it")
        || lower.contains(",italic");

    let is_monospace = lower.contains("mono")
        || lower.contains("courier")
        || lower.contains("consolas")
        || lower.contains("menlo")
        || lower.contains("inconsolata")
        || lower.contains("firacode")
        || lower.contains("source code")
        || lower.contains("jetbrains");

    let is_serif = lower.contains("serif") && !lower.contains("sans")
        || lower.contains("times")
        || lower.contains("garamond")
        || lower.contains("georgia")
        || lower.contains("palatino")
        || lower.contains("cambria")
        || lower.contains("minion");

    let weight = if lower.contains("black") || lower.contains("heavy") {
        FontWeight::Black
    } else if lower.contains("extrabold") || lower.contains("ultra") {
        FontWeight::ExtraBold
    } else if lower.contains("bold") {
        FontWeight::Bold
    } else if lower.contains("semibold") || lower.contains("demi") {
        FontWeight::SemiBold
    } else if lower.contains("medium") {
        FontWeight::Medium
    } else if lower.contains("light") || lower.contains("thin") {
        FontWeight::Light
    } else {
        FontWeight::Regular
    };

    let style = if lower.contains("italic") {
        FontStyle::Italic
    } else if lower.contains("oblique") {
        FontStyle::Oblique
    } else {
        FontStyle::Normal
    };

    FontInfo {
        name: name.to_string(),
        base_name: base_name.to_string(),
        is_bold,
        is_italic,
        is_monospace,
        is_serif,
        weight,
        style,
    }
}

// ── Color extraction ─────────────────────────────────────────────────

/// Color in the content stream.
#[derive(Debug, Clone)]
#[cfg_attr(feature = "serde", derive(Serialize))]
pub struct PdfColor {
    /// Colour space the components are expressed in.
    pub space: ColorSpace,
    /// Raw components in the order PDF specifies them for `space`.
    pub components: Vec<f64>,
}

impl PdfColor {
    /// Convert the colour to RGB in `[0, 1]`. Returns black when the colour
    /// space is unsupported (e.g. `Pattern` or arbitrary `Other`).
    pub fn to_rgb(&self) -> (f64, f64, f64) {
        match self.space {
            ColorSpace::DeviceRGB => {
                let r = self.components.first().copied().unwrap_or(0.0);
                let g = self.components.get(1).copied().unwrap_or(0.0);
                let b = self.components.get(2).copied().unwrap_or(0.0);
                (r, g, b)
            }
            ColorSpace::DeviceGray => {
                let g = self.components.first().copied().unwrap_or(0.0);
                (g, g, g)
            }
            ColorSpace::DeviceCMYK => {
                let c = self.components.first().copied().unwrap_or(0.0);
                let m = self.components.get(1).copied().unwrap_or(0.0);
                let y = self.components.get(2).copied().unwrap_or(0.0);
                let k = self.components.get(3).copied().unwrap_or(0.0);
                (
                    (1.0 - c) * (1.0 - k),
                    (1.0 - m) * (1.0 - k),
                    (1.0 - y) * (1.0 - k),
                )
            }
            _ => (0.0, 0.0, 0.0),
        }
    }

    /// Render the colour as a `#RRGGBB` hex string after going through [`Self::to_rgb`].
    pub fn to_hex(&self) -> String {
        let (r, g, b) = self.to_rgb();
        format!(
            "#{:02X}{:02X}{:02X}",
            (r * 255.0).round() as u8,
            (g * 255.0).round() as u8,
            (b * 255.0).round() as u8,
        )
    }

    /// True when all RGB components are below 10% — a coarse near-black check.
    pub fn is_black(&self) -> bool {
        let (r, g, b) = self.to_rgb();
        r < 0.1 && g < 0.1 && b < 0.1
    }

    /// Relative luminance using the standard NTSC coefficients (0.299/0.587/0.114).
    pub fn luminance(&self) -> f64 {
        let (r, g, b) = self.to_rgb();
        0.299 * r + 0.587 * g + 0.114 * b
    }
}

/// PDF colour space the [`PdfColor::components`] are expressed in.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[cfg_attr(feature = "serde", derive(Serialize))]
pub enum ColorSpace {
    /// `DeviceGray` — single greyscale component.
    DeviceGray,
    /// `DeviceRGB` — red/green/blue components.
    DeviceRGB,
    /// `DeviceCMYK` — cyan/magenta/yellow/black components.
    DeviceCMYK,
    /// `Pattern` colour space (uncoloured / coloured pattern).
    Pattern,
    /// Any other named or ICC-based colour space.
    Other,
}

// ── Extended glyph with full metadata ────────────────────────────────

/// A glyph carrying everything the content stream told us.
#[derive(Debug, Clone)]
#[cfg_attr(feature = "serde", derive(Serialize))]
pub struct RichGlyph {
    /// Decoded text for this glyph cluster.
    pub text: String,
    /// X coordinate in page space.
    pub x: f64,
    /// Y coordinate in page space.
    pub y: f64,
    /// Horizontal advance after this glyph (effective character width).
    pub advance: f64,
    /// Glyph nominal font size in points.
    pub font_size: f64,
    /// Resolved font info for this glyph.
    pub font: FontInfo,
    /// Non-stroking fill colour.
    pub fill_color: PdfColor,
    /// Stroking colour.
    pub stroke_color: PdfColor,
    /// PDF text-rendering mode (`Tr` operand).
    pub render_mode: TextRenderMode,
    /// `Tc` character spacing operand.
    pub char_spacing: f64,
    /// `Tw` word spacing operand.
    pub word_spacing: f64,
    /// `Tz` horizontal scaling operand (percent).
    pub horizontal_scaling: f64,
    /// `Ts` text rise operand.
    pub text_rise: f64,
    /// PDF object number that hosts the source content stream.
    pub object_number: u32,
    /// Index of the content stream within the page's stream list.
    pub stream_index: usize,
}

/// PDF text-rendering mode (`Tr` operand: fill, stroke, clip, …).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[cfg_attr(feature = "serde", derive(Serialize))]
pub enum TextRenderMode {
    /// Fill text (mode 0).
    Fill,
    /// Stroke text (mode 1).
    Stroke,
    /// Fill, then stroke (mode 2).
    FillStroke,
    /// Invisible: neither fill nor stroke (mode 3).
    Invisible,
    /// Fill text and add it to the clipping path (mode 4).
    FillClip,
    /// Stroke text and add it to the clipping path (mode 5).
    StrokeClip,
    /// Fill, stroke, and add to clipping path (mode 6).
    FillStrokeClip,
    /// Add to clipping path only (mode 7).
    Clip,
}

impl From<i64> for TextRenderMode {
    fn from(v: i64) -> Self {
        match v {
            0 => Self::Fill,
            1 => Self::Stroke,
            2 => Self::FillStroke,
            3 => Self::Invisible,
            4 => Self::FillClip,
            5 => Self::StrokeClip,
            6 => Self::FillStrokeClip,
            7 => Self::Clip,
            _ => Self::Fill,
        }
    }
}

// ── Shape and chart detection ────────────────────────────────────────

/// A drawn shape recovered from path operators (line, rectangle, circle, …).
#[derive(Debug, Clone)]
#[cfg_attr(feature = "serde", derive(Serialize))]
pub struct DetectedShape {
    /// Classified shape kind.
    pub kind: ShapeKind,
    /// Axis-aligned bounding box for the shape.
    pub bbox: BBox,
    /// Fill colour when the shape was filled.
    pub fill_color: Option<PdfColor>,
    /// Stroke colour when the shape had a visible stroke.
    pub stroke_color: Option<PdfColor>,
    /// Stroke width in page units.
    pub line_width: f64,
}

/// Coarse classification of a [`DetectedShape`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[cfg_attr(feature = "serde", derive(Serialize))]
pub enum ShapeKind {
    /// Long, thin horizontal line.
    HorizontalLine,
    /// Long, thin vertical line.
    VerticalLine,
    /// Hollow axis-aligned rectangle.
    Rectangle,
    /// Filled axis-aligned rectangle.
    FilledRectangle,
    /// Closed curve resembling a circle / ellipse.
    Circle,
    /// Heuristic match for an arrowhead.
    Arrow,
    /// Could not be classified.
    Unknown,
}

/// Axis-aligned bounding box in page coordinates.
#[derive(Debug, Clone, Copy)]
#[cfg_attr(feature = "serde", derive(Serialize))]
pub struct BBox {
    /// Left edge in page space.
    pub x: f64,
    /// Top edge in page space.
    pub y: f64,
    /// Width in page units.
    pub width: f64,
    /// Height in page units.
    pub height: f64,
}

/// A region recognised as a chart, with its shapes and labels.
#[derive(Debug, Clone)]
#[cfg_attr(feature = "serde", derive(Serialize))]
pub struct DetectedChart {
    /// Classified chart kind.
    pub kind: ChartKind,
    /// Axis-aligned bounding box for the chart.
    pub bbox: BBox,
    /// Geometric shapes that make up the chart (bars, lines, slices, …).
    pub shapes: Vec<DetectedShape>,
    /// Text labels grouped with the chart.
    pub labels: Vec<ChartLabel>,
}

/// Coarse classification of a [`DetectedChart`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[cfg_attr(feature = "serde", derive(Serialize))]
pub enum ChartKind {
    /// Bar chart (single-series rectangles).
    Bar,
    /// Stacked bar chart.
    StackedBar,
    /// Line chart.
    Line,
    /// Pie / donut chart.
    Pie,
    /// Scatter plot.
    Scatter,
    /// Could not be classified.
    Unknown,
}

/// A text label associated with a chart (title, axis tick, legend, …).
#[derive(Debug, Clone)]
#[cfg_attr(feature = "serde", derive(Serialize))]
pub struct ChartLabel {
    /// Label text content.
    pub text: String,
    /// X coordinate of the label baseline in page space.
    pub x: f64,
    /// Y coordinate of the label baseline in page space.
    pub y: f64,
    /// Inferred role of the label inside the chart.
    pub role: ChartLabelRole,
}

/// Functional role of a [`ChartLabel`] within a chart.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[cfg_attr(feature = "serde", derive(Serialize))]
pub enum ChartLabelRole {
    /// Chart title.
    Title,
    /// Axis-name label.
    AxisLabel,
    /// Tick-mark label.
    TickLabel,
    /// Data-point label.
    DataLabel,
    /// Legend entry.
    Legend,
    /// Could not be classified.
    Unknown,
}

// ── Page-level extraction: everything at once ────────────────────────

/// Everything the tessellator recovered from a single PDF page.
#[derive(Debug, Clone)]
#[cfg_attr(feature = "serde", derive(Serialize))]
pub struct PageExtraction {
    /// 1-based page number in the source document.
    pub page_number: usize,
    /// Page width in PDF points.
    pub width: f64,
    /// Page height in PDF points.
    pub height: f64,
    /// Text regions with semantic roles assigned.
    pub regions: Vec<AnnotatedRegion>,
    /// Reconstructed tables.
    pub tables: Vec<ExtractedTable>,
    /// Recognised charts.
    pub charts: Vec<DetectedChart>,
    /// Embedded raster images.
    pub images: Vec<PageImage>,
    /// Drawn shapes detected on the page.
    pub shapes: Vec<DetectedShape>,
    /// Aggregate page metadata.
    pub metadata: PageMetadata,
}

/// Simple RDF triple in `subject predicate object` form.
#[derive(Debug, Clone)]
#[cfg_attr(feature = "serde", derive(Serialize))]
pub struct SemanticTriple {
    /// Subject IRI / literal.
    pub subject: String,
    /// Predicate IRI.
    pub predicate: String,
    /// Object IRI / literal.
    pub object: String,
}

impl SemanticTriple {
    /// Construct a triple from any string-like values.
    pub fn new(
        subject: impl Into<String>,
        predicate: impl Into<String>,
        object: impl Into<String>,
    ) -> Self {
        Self {
            subject: subject.into(),
            predicate: predicate.into(),
            object: object.into(),
        }
    }
}

impl PageExtraction {
    /// Serialise the extraction to RDF triples under `base_iri`.
    ///
    /// Emits `DisplaySurface` / `Morphable` / `hasRole` / `text` / geometry /
    /// `readingOrder` triples per region, plus pairwise `owl:adjacentTo`
    /// triples for regions whose bounding boxes are within 10 pt of each
    /// other on both axes.
    pub fn to_rdf_triples(&self, base_iri: &str) -> Vec<SemanticTriple> {
        let mut triples = Vec::new();
        let ui_ns = "urn:pdf:tessellation:ui:";
        let layout_ns = "urn:pdf:tessellation:layout:";
        let rdf_type = "http://www.w3.org/1999/02/22-rdf-syntax-ns#type";
        let owl_adjacent = "http://www.w3.org/2002/07/owl#adjacentTo";

        let surface_iri = format!("{}page/{}", base_iri, self.page_number);

        triples.push(SemanticTriple::new(
            &surface_iri,
            rdf_type,
            format!("{}DisplaySurface", ui_ns),
        ));

        // Regions
        for (idx, region) in self.regions.iter().enumerate() {
            let region_iri = format!("{}region/{}_{}", base_iri, self.page_number, idx);

            triples.push(SemanticTriple::new(
                &region_iri,
                rdf_type,
                format!("{}Morphable", layout_ns),
            ));

            triples.push(SemanticTriple::new(
                &region_iri,
                format!("{}partOf", layout_ns),
                &surface_iri,
            ));

            let role_fragment = format!("{:?}", region.role);
            triples.push(SemanticTriple::new(
                &region_iri,
                format!("{}hasRole", ui_ns),
                format!("{}{}", ui_ns, role_fragment),
            ));

            triples.push(SemanticTriple::new(
                &region_iri,
                format!("{}text", layout_ns),
                format!(
                    "\"{}\"",
                    region.text.replace('\"', "\\\"").replace('\n', "\\n")
                ),
            ));

            // cx, cy, w, h
            triples.push(SemanticTriple::new(
                &region_iri,
                format!("{}cx", layout_ns),
                format!("{:.6}", region.bbox.x + region.bbox.width / 2.0),
            ));
            triples.push(SemanticTriple::new(
                &region_iri,
                format!("{}cy", layout_ns),
                format!("{:.6}", region.bbox.y + region.bbox.height / 2.0),
            ));
            triples.push(SemanticTriple::new(
                &region_iri,
                format!("{}width", layout_ns),
                format!("{:.6}", region.bbox.width),
            ));
            triples.push(SemanticTriple::new(
                &region_iri,
                format!("{}height", layout_ns),
                format!("{:.6}", region.bbox.height),
            ));
            triples.push(SemanticTriple::new(
                &region_iri,
                format!("{}readingOrder", layout_ns),
                format!("{}", idx),
            ));
        }

        // Compute adjacencies based on bounding box gaps
        let epsilon = 10.0; // 10 points adjacency threshold
        for (i, ri) in self.regions.iter().enumerate() {
            for (j, rj) in self.regions.iter().enumerate().skip(i + 1) {
                let gap_x = (ri.bbox.x.max(rj.bbox.x)
                    - (ri.bbox.x + ri.bbox.width).min(rj.bbox.x + rj.bbox.width))
                .max(0.0);
                let gap_y = (ri.bbox.y.max(rj.bbox.y)
                    - (ri.bbox.y + ri.bbox.height).min(rj.bbox.y + rj.bbox.height))
                .max(0.0);
                if gap_x <= epsilon && gap_y <= epsilon {
                    let iri_i = format!("{}region/{}_{}", base_iri, self.page_number, i);
                    let iri_j = format!("{}region/{}_{}", base_iri, self.page_number, j);
                    triples.push(SemanticTriple::new(&iri_i, owl_adjacent, &iri_j));
                    triples.push(SemanticTriple::new(&iri_j, owl_adjacent, &iri_i));
                }
            }
        }

        triples
    }
}

/// A converged text region paired with a semantic role and styling info.
#[derive(Debug, Clone)]
#[cfg_attr(feature = "serde", derive(Serialize))]
pub struct AnnotatedRegion {
    /// Assigned semantic role (Title, Heading, Paragraph, …).
    pub role: SemanticRole,
    /// Region text content.
    pub text: String,
    /// Axis-aligned bounding box in page space.
    pub bbox: BBox,
    /// Dominant font size for the region.
    pub font_size: f64,
    /// Resolved font info, when known.
    pub font: Option<FontInfo>,
    /// Hex colour string (`#RRGGBB`) when a uniform colour is known.
    pub color: Option<String>,
    /// Heuristic confidence in the assigned role (`0.0..=1.0`).
    pub confidence: f64,
}

/// Assign [`SemanticRole`]s to a slice of regions in place.
///
/// Uses character-weighted dominant font size, font weight, and page position
/// (top 8% → `Header`, bottom 8% → `Footer`) to bucket each region.
pub fn assign_roles(regions: &mut [AnnotatedRegion], _page_width: f64, page_height: f64) {
    if regions.is_empty() {
        return;
    }

    let mut size_counts: std::collections::HashMap<u32, usize> = std::collections::HashMap::new();
    for r in regions.iter() {
        let key = (r.font_size * 10.0) as u32;
        *size_counts.entry(key).or_default() += r.text.len();
    }

    let dominant_size = size_counts
        .into_iter()
        .max_by_key(|(_, c)| *c)
        .map(|(k, _)| k as f64 / 10.0)
        .unwrap_or(12.0);

    for r in regions.iter_mut() {
        let is_bold = r.font.as_ref().map(|f| f.is_bold).unwrap_or(false);
        let size_ratio = r.font_size / dominant_size;

        r.role = if size_ratio > 1.5 && is_bold {
            SemanticRole::Title
        } else if size_ratio > 1.2 || (size_ratio >= 1.0 && is_bold) {
            SemanticRole::Heading
        } else if r.font_size < dominant_size * 0.8 {
            SemanticRole::Footnote
        } else if r.text.starts_with("•") || r.text.starts_with("- ") {
            SemanticRole::ListItem
        } else {
            SemanticRole::Paragraph
        };

        if r.bbox.y < page_height * 0.08 && size_ratio <= 1.0 {
            r.role = SemanticRole::Header;
        } else if r.bbox.y > page_height * 0.92 && size_ratio <= 1.0 {
            r.role = SemanticRole::Footer;
        }
    }
}

/// A table reconstructed from converged cells / explicit borders.
#[derive(Debug, Clone)]
#[cfg_attr(feature = "serde", derive(Serialize))]
pub struct ExtractedTable {
    /// Header row text values, left-to-right.
    pub headers: Vec<String>,
    /// Body rows (`rows[i][j]` is the cell at row `i`, column `j`).
    pub rows: Vec<Vec<String>>,
    /// Axis-aligned bounding box for the whole table.
    pub bbox: BBox,
    /// `true` when the table was reconstructed from drawn cell borders rather
    /// than purely from text layout.
    pub has_explicit_borders: bool,
}

/// An embedded raster image observed on a page.
#[derive(Debug, Clone)]
#[cfg_attr(feature = "serde", derive(Serialize))]
pub struct PageImage {
    /// PDF object number that hosts the image XObject.
    pub object_number: u32,
    /// Index of the content stream where the image was referenced.
    pub stream_index: usize,
    /// Bounding box in page space.
    pub bbox: BBox,
    /// Image width in pixels.
    pub width: i64,
    /// Image height in pixels.
    pub height: i64,
    /// Source encoding (e.g. `DCTDecode`, `FlateDecode`) when known.
    pub format: Option<String>,
    /// Source colour space name when known.
    pub color_space: Option<String>,
}

/// Aggregate page-level counts and language hints.
#[derive(Debug, Clone)]
#[cfg_attr(feature = "serde", derive(Serialize))]
pub struct PageMetadata {
    /// Number of text-block regions detected.
    pub text_block_count: usize,
    /// Number of tables detected.
    pub table_count: usize,
    /// Number of charts detected.
    pub chart_count: usize,
    /// Number of embedded images.
    pub image_count: usize,
    /// Total visible-character count across regions.
    pub total_characters: usize,
    /// Modal font name across the page.
    pub dominant_font: Option<String>,
    /// Modal font size across the page.
    pub dominant_font_size: f64,
    /// Heuristic ISO-639 language codes detected on the page.
    pub languages_detected: Vec<String>,
    /// `true` when the page exposes a selectable text layer (not just raster).
    pub has_selectable_text: bool,
    /// Non-fatal warnings produced by the extractor for this page.
    pub extraction_warnings: Vec<String>,
}
