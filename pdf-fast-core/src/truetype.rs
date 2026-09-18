//! Minimal TrueType `glyf` outline extraction.
//!
//! # Why this exists
//!
//! A glyph atlas recovers a character from an outline. Type 3 fonts reach their
//! outlines because those outlines are content streams the parser can execute.
//! Every other font keeps them in a *font program*, and until now this crate
//! only classified those streams — it never read one. So a document embedding
//! TrueType, which is the ordinary case for anything produced by a word
//! processor, gave the atlas nothing to work with.
//!
//! This module reads the four tables needed to turn a character code into an
//! outline, and nothing else:
//!
//! - `head` — `unitsPerEm`, which becomes the font matrix, and
//!   `indexToLocFormat`, which decides how `loca` is read.
//! - `maxp` — the glyph count, which bounds every other table read.
//! - `loca` + `glyf` — where each glyph's outline is, and its contours.
//! - `cmap` — character code to glyph index.
//!
//! # What it deliberately does not do
//!
//! No hinting (the `fpgm`/`prep`/`cvt` instructions are ignored: they adjust
//! rasterization, not the outline as designed), no kerning, no `CFF`/`CFF2`
//! charstrings, and no OpenType layout. A font whose outlines live in `CFF`
//! (an `OTTO` sfnt) returns `None` rather than a guess, because guessing at an
//! outline produces a *plausible* shape, and a plausible wrong shape is exactly
//! what the atlas compares against.
//!
//! # Coordinate space
//!
//! Outlines come back in font units with y pointing up, which is what the
//! caller's font matrix expects: the same convention Type 3 `d1` glyphs use, so
//! both sources flow through one transform path.

/// A glyph outline in font units, y-up.
#[derive(Debug, Clone, Default)]
pub struct GlyphOutline {
    /// Contours, each a closed point list.
    pub contours: Vec<Vec<OutlinePoint>>,
}

/// One point on a glyph contour.
#[derive(Debug, Clone, Copy)]
pub struct OutlinePoint {
    /// X in font units.
    pub x: f32,
    /// Y in font units.
    pub y: f32,
    /// True when the point lies on the curve.
    ///
    /// TrueType stores quadratic outlines as a stream of on-curve and off-curve
    /// points rather than as explicit segments, and a run of two or more
    /// off-curve points implies on-curve points midway between them that are
    /// not stored. Keeping the flag is what lets a caller reconstruct the path
    /// the way the format intends instead of inventing segments.
    pub on_curve: bool,
}

/// A parsed TrueType program.
#[derive(Debug, Clone)]
pub struct TrueTypeFont {
    units_per_em: f32,
    num_glyphs: u16,
    loca: Vec<u32>,
    glyf: (usize, usize),
    cmap4: Option<(usize, usize)>,
    /// `numberOfHMetrics`, and the `hmtx` byte range.
    hmtx: Option<(usize, usize, usize)>,
    data: Vec<u8>,
}

impl TrueTypeFont {
    /// Parse the tables needed for outline extraction.
    ///
    /// Returns `None` for anything that is not a `glyf`-based TrueType program:
    /// an `OTTO` (CFF) sfnt, a bare CFF stream, or a truncated file. The caller
    /// treats `None` as "no outlines available", which is the honest answer —
    /// there is nothing here to approximate.
    pub fn parse(data: &[u8]) -> Option<Self> {
        if data.len() < 12 {
            return None;
        }
        // `true` is the older Apple tag; both mean glyf-based TrueType. `OTTO`
        // means CFF outlines, which this module does not read.
        let tag = &data[0..4];
        if tag != [0x00, 0x01, 0x00, 0x00] && tag != b"true" {
            return None;
        }
        let num_tables = u16::from_be_bytes([data[4], data[5]]) as usize;
        let mut head = None;
        let mut maxp = None;
        let mut loca = None;
        let mut glyf = None;
        let mut cmap = None;
        let mut hhea = None;
        let mut hmtx = None;
        for index in 0..num_tables {
            let record = 12 + index * 16;
            if record + 16 > data.len() {
                return None;
            }
            let offset = u32::from_be_bytes([
                data[record + 8],
                data[record + 9],
                data[record + 10],
                data[record + 11],
            ]) as usize;
            let length = u32::from_be_bytes([
                data[record + 12],
                data[record + 13],
                data[record + 14],
                data[record + 15],
            ]) as usize;
            let end = offset.saturating_add(length).min(data.len());
            if offset > data.len() {
                return None;
            }
            match &data[record..record + 4] {
                b"head" => head = Some(offset),
                b"maxp" => maxp = Some(offset),
                b"loca" => loca = Some(offset),
                b"glyf" => glyf = Some((offset, end)),
                b"cmap" => cmap = Some((offset, end)),
                b"hhea" => hhea = Some(offset),
                b"hmtx" => hmtx = Some((offset, end)),
                _ => {}
            }
        }
        let head = head?;
        let maxp = maxp?;
        let loca_offset = loca?;
        let glyf = glyf?;

        if head + 54 > data.len() || maxp + 6 > data.len() {
            return None;
        }
        let units_per_em = u16::from_be_bytes([data[head + 18], data[head + 19]]) as f32;
        if units_per_em <= 0.0 {
            return None;
        }
        let index_to_loc = i16::from_be_bytes([data[head + 50], data[head + 51]]);
        let num_glyphs = u16::from_be_bytes([data[maxp + 4], data[maxp + 5]]);
        if num_glyphs == 0 {
            return None;
        }

        // `loca` holds `num_glyphs + 1` offsets, short (halved) or long.
        let loca: Vec<u32> = if index_to_loc == 0 {
            let needed = (num_glyphs as usize + 1) * 2;
            if loca_offset + needed > data.len() {
                return None;
            }
            (0..=num_glyphs as usize)
                .map(|i| {
                    let at = loca_offset + i * 2;
                    u16::from_be_bytes([data[at], data[at + 1]]) as u32 * 2
                })
                .collect()
        } else {
            let needed = (num_glyphs as usize + 1) * 4;
            if loca_offset + needed > data.len() {
                return None;
            }
            (0..=num_glyphs as usize)
                .map(|i| {
                    let at = loca_offset + i * 4;
                    u32::from_be_bytes([data[at], data[at + 1], data[at + 2], data[at + 3]])
                })
                .collect()
        };
        if loca.len() != num_glyphs as usize + 1 {
            return None;
        }

        let cmap4 = cmap.and_then(|(start, end)| find_cmap4(&data[..end], start));
        // `hhea`'s numberOfHMetrics is at offset 34; without it the advance
        // table cannot be indexed.
        let hmtx = match (hhea, hmtx) {
            (Some(hhea), Some(hmtx)) if hhea + 36 <= data.len() => {
                let count = u16::from_be_bytes([data[hhea + 34], data[hhea + 35]]);
                Some((count as usize, hmtx.0, hmtx.1))
            }
            _ => None,
        };
        Some(Self {
            units_per_em,
            num_glyphs,
            loca,
            glyf,
            cmap4,
            hmtx,
            data: data.to_vec(),
        })
    }

    /// Font design units per em, the denominator of the font matrix.
    pub fn units_per_em(&self) -> f32 {
        self.units_per_em
    }

    /// Glyph index for a character code, or `None` when the format has no
    /// mapping for it.
    pub fn glyph_index(&self, code: u32) -> Option<u16> {
        let (start, end) = self.cmap4?;
        let table = self.data.get(start..end)?;
        lookup_cmap4(table, code).filter(|index| *index < self.num_glyphs)
    }

    /// Advance width for a character code, in font units.
    ///
    /// Without it text cannot advance, and text that does not advance collapses
    /// onto one position — the failure the atlas cannot see, because the shapes
    /// are all correct.
    ///
    /// `hmtx` stores one entry per glyph up to `numberOfHMetrics`, then repeats
    /// the last width for the remaining glyphs.
    pub fn advance_units(&self, code: u32) -> Option<f32> {
        let index = self.glyph_index(code)? as usize;
        let (metric_count, start, end) = self.hmtx?;
        if metric_count == 0 {
            return None;
        }
        let at = if index < metric_count {
            start + index * 4
        } else {
            start + (metric_count - 1) * 4
        };
        if at + 2 > end || at + 2 > self.data.len() {
            return None;
        }
        Some(u16::from_be_bytes([self.data[at], self.data[at + 1]]) as f32)
    }

    /// Outline for a glyph index, in font units, y-up.
    ///
    /// Composite glyphs are resolved into their components. A component cycle,
    /// which a malformed font can contain, is refused rather than followed.
    pub fn outline(&self, glyph_index: u16) -> Option<GlyphOutline> {
        let mut outline = GlyphOutline::default();
        let mut visiting = Vec::new();
        self.append_glyph(glyph_index, Affine::IDENTITY, &mut outline, &mut visiting)?;
        if outline.contours.is_empty() {
            // A glyph with no contours is legitimate — `space` is one — but it
            // carries no shape, so the caller gets `None` rather than an empty
            // outline it would have to special-case.
            return None;
        }
        Some(outline)
    }

    fn glyph_range(&self, glyph_index: u16) -> Option<(usize, usize)> {
        let index = glyph_index as usize;
        let start = *self.loca.get(index)? as usize;
        let end = *self.loca.get(index + 1)? as usize;
        if end <= start {
            // Equal offsets mean "no outline", the encoding for a blank glyph.
            return None;
        }
        let base = self.glyf.0;
        let range = (base + start, (base + end).min(self.glyf.1));
        if range.0 >= range.1 || range.1 > self.data.len() {
            return None;
        }
        Some(range)
    }

    fn append_glyph(
        &self,
        glyph_index: u16,
        transform: Affine,
        out: &mut GlyphOutline,
        visiting: &mut Vec<u16>,
    ) -> Option<()> {
        if visiting.contains(&glyph_index) || visiting.len() > 8 {
            return None;
        }
        let (start, end) = self.glyph_range(glyph_index)?;
        let glyph = self.data.get(start..end)?;
        if glyph.len() < 10 {
            return None;
        }
        let number_of_contours = i16::from_be_bytes([glyph[0], glyph[1]]);
        if number_of_contours >= 0 {
            self.append_simple(glyph, number_of_contours as usize, transform, out)
        } else {
            visiting.push(glyph_index);
            let result = self.append_composite(glyph, transform, out, visiting);
            visiting.pop();
            result
        }
    }

    fn append_simple(
        &self,
        glyph: &[u8],
        contour_count: usize,
        transform: Affine,
        out: &mut GlyphOutline,
    ) -> Option<()> {
        // Ten bytes of header, then the contour end points.
        let ends_at = 10 + contour_count * 2;
        if contour_count == 0 || glyph.len() < ends_at + 2 {
            return None;
        }
        let mut ends = Vec::with_capacity(contour_count);
        for i in 0..contour_count {
            let at = 10 + i * 2;
            ends.push(u16::from_be_bytes([glyph[at], glyph[at + 1]]) as usize);
        }
        let point_count = *ends.last()? + 1;
        let instruction_length = u16::from_be_bytes([glyph[ends_at], glyph[ends_at + 1]]) as usize;
        let flags_at = ends_at + 2 + instruction_length;
        if flags_at > glyph.len() {
            return None;
        }

        // Flags are run-length encoded: bit 3 repeats the flag.
        let mut flags = Vec::with_capacity(point_count);
        let mut cursor = flags_at;
        while flags.len() < point_count {
            let flag = *glyph.get(cursor)?;
            cursor += 1;
            flags.push(flag);
            if flag & 0x08 != 0 {
                let repeat = *glyph.get(cursor)? as usize;
                cursor += 1;
                for _ in 0..repeat {
                    if flags.len() >= point_count {
                        break;
                    }
                    flags.push(flag);
                }
            }
        }

        // Coordinates are deltas, with per-axis flag-selected widths.
        let mut xs = Vec::with_capacity(point_count);
        let mut x = 0i32;
        for flag in &flags {
            let delta = if flag & 0x02 != 0 {
                let value = *glyph.get(cursor)? as i32;
                cursor += 1;
                if flag & 0x10 != 0 {
                    value
                } else {
                    -value
                }
            } else if flag & 0x10 != 0 {
                0
            } else {
                let value =
                    i16::from_be_bytes([*glyph.get(cursor)?, *glyph.get(cursor + 1)?]) as i32;
                cursor += 2;
                value
            };
            x += delta;
            xs.push(x);
        }
        let mut ys = Vec::with_capacity(point_count);
        let mut y = 0i32;
        for flag in &flags {
            let delta = if flag & 0x04 != 0 {
                let value = *glyph.get(cursor)? as i32;
                cursor += 1;
                if flag & 0x20 != 0 {
                    value
                } else {
                    -value
                }
            } else if flag & 0x20 != 0 {
                0
            } else {
                let value =
                    i16::from_be_bytes([*glyph.get(cursor)?, *glyph.get(cursor + 1)?]) as i32;
                cursor += 2;
                value
            };
            y += delta;
            ys.push(y);
        }

        let mut first = 0usize;
        for end in ends {
            if end < first || end >= point_count {
                return None;
            }
            let mut contour = Vec::with_capacity(end - first + 1);
            for i in first..=end {
                let (px, py) = transform.apply(xs[i] as f32, ys[i] as f32);
                contour.push(OutlinePoint {
                    x: px,
                    y: py,
                    on_curve: flags[i] & 0x01 != 0,
                });
            }
            // Close the contour explicitly. TrueType implies the closing
            // segment, and an atlas comparing point sequences needs the same
            // point count from two renderings of one shape.
            if let Some(first_point) = contour.first().copied() {
                contour.push(first_point);
            }
            out.contours.push(contour);
            first = end + 1;
        }
        Some(())
    }

    fn append_composite(
        &self,
        glyph: &[u8],
        transform: Affine,
        out: &mut GlyphOutline,
        visiting: &mut Vec<u16>,
    ) -> Option<()> {
        let mut cursor = 10usize;
        loop {
            let flags = u16::from_be_bytes([*glyph.get(cursor)?, *glyph.get(cursor + 1)?]);
            let component_index =
                u16::from_be_bytes([*glyph.get(cursor + 2)?, *glyph.get(cursor + 3)?]);
            cursor += 4;

            let (dx, dy) = if flags & 0x0001 != 0 {
                // ARG_1_AND_2_ARE_WORDS
                let dx = i16::from_be_bytes([*glyph.get(cursor)?, *glyph.get(cursor + 1)?]) as f32;
                let dy =
                    i16::from_be_bytes([*glyph.get(cursor + 2)?, *glyph.get(cursor + 3)?]) as f32;
                cursor += 4;
                (dx, dy)
            } else {
                let dx = *glyph.get(cursor)? as i8 as f32;
                let dy = *glyph.get(cursor + 1)? as i8 as f32;
                cursor += 2;
                (dx, dy)
            };

            // Arguments are either offsets or point numbers to align to. Only
            // offsets are handled: point-matching needs the already-placed
            // points of the parent, and approximating it would shift the
            // component by an unknown amount.
            if flags & 0x0002 == 0 {
                return None;
            }

            let mut component = Affine {
                dx,
                dy,
                ..Affine::IDENTITY
            };
            if flags & 0x0008 != 0 {
                // WE_HAVE_A_SCALE
                let scale = f2dot14(i16::from_be_bytes([
                    *glyph.get(cursor)?,
                    *glyph.get(cursor + 1)?,
                ]));
                cursor += 2;
                component.a = scale;
                component.d = scale;
            } else if flags & 0x0040 != 0 {
                // WE_HAVE_AN_X_AND_Y_SCALE
                component.a = f2dot14(i16::from_be_bytes([
                    *glyph.get(cursor)?,
                    *glyph.get(cursor + 1)?,
                ]));
                component.d = f2dot14(i16::from_be_bytes([
                    *glyph.get(cursor + 2)?,
                    *glyph.get(cursor + 3)?,
                ]));
                cursor += 4;
            } else if flags & 0x0080 != 0 {
                // WE_HAVE_A_TWO_BY_TWO
                component.a = f2dot14(i16::from_be_bytes([
                    *glyph.get(cursor)?,
                    *glyph.get(cursor + 1)?,
                ]));
                component.b = f2dot14(i16::from_be_bytes([
                    *glyph.get(cursor + 2)?,
                    *glyph.get(cursor + 3)?,
                ]));
                component.c = f2dot14(i16::from_be_bytes([
                    *glyph.get(cursor + 4)?,
                    *glyph.get(cursor + 5)?,
                ]));
                component.d = f2dot14(i16::from_be_bytes([
                    *glyph.get(cursor + 6)?,
                    *glyph.get(cursor + 7)?,
                ]));
                cursor += 8;
            }

            self.append_glyph(component_index, transform.then(component), out, visiting)?;

            if flags & 0x0020 == 0 {
                // MORE_COMPONENTS clear: this was the last one.
                return Some(());
            }
        }
    }
}

/// A 2x3 affine transform, matching the matrix layout used elsewhere.
#[derive(Debug, Clone, Copy)]
struct Affine {
    a: f32,
    b: f32,
    c: f32,
    d: f32,
    dx: f32,
    dy: f32,
}

impl Affine {
    const IDENTITY: Self = Self {
        a: 1.0,
        b: 0.0,
        c: 0.0,
        d: 1.0,
        dx: 0.0,
        dy: 0.0,
    };

    #[inline]
    fn apply(&self, x: f32, y: f32) -> (f32, f32) {
        (
            self.a * x + self.c * y + self.dx,
            self.b * x + self.d * y + self.dy,
        )
    }

    /// This transform followed by `next`.
    #[inline]
    fn then(&self, next: Affine) -> Affine {
        Affine {
            a: self.a * next.a + self.b * next.c,
            b: self.a * next.b + self.b * next.d,
            c: self.c * next.a + self.d * next.c,
            d: self.c * next.b + self.d * next.d,
            dx: self.dx * next.a + self.dy * next.c + next.dx,
            dy: self.dx * next.b + self.dy * next.d + next.dy,
        }
    }
}

/// A 2.14 fixed-point value, the format composite transforms use.
#[inline]
fn f2dot14(value: i16) -> f32 {
    value as f32 / 16384.0
}

/// Find the best `cmap` format-4 subtable, preferring a Unicode platform.
fn find_cmap4(data: &[u8], start: usize) -> Option<(usize, usize)> {
    if start + 4 > data.len() {
        return None;
    }
    let table_count = u16::from_be_bytes([data[start + 2], data[start + 3]]) as usize;
    let mut best: Option<(usize, usize, u8)> = None;
    for index in 0..table_count {
        let record = start + 4 + index * 8;
        if record + 8 > data.len() {
            break;
        }
        let platform = u16::from_be_bytes([data[record], data[record + 1]]);
        let offset = u32::from_be_bytes([
            data[record + 4],
            data[record + 5],
            data[record + 6],
            data[record + 7],
        ]) as usize;
        let sub = start + offset;
        if sub + 2 > data.len() {
            continue;
        }
        let format = u16::from_be_bytes([data[sub], data[sub + 1]]);
        if format != 4 {
            continue;
        }
        // Length is at offset 2 of the subtable header.
        if sub + 4 > data.len() {
            continue;
        }
        let length = u16::from_be_bytes([data[sub + 2], data[sub + 3]]) as usize;
        let end = (sub + length).min(data.len());
        // Prefer a Unicode platform; Windows Unicode and Unicode BMP are both
        // fine, and a symbol-encoded subtable is a last resort.
        let rank = match platform {
            0 => 3, // Unicode
            3 => 2, // Windows, and format 4 is the BMP encoding
            _ => 1,
        };
        if best.map(|(_, _, r)| rank > r).unwrap_or(true) {
            best = Some((sub, end, rank));
        }
    }
    best.map(|(s, e, _)| (s, e))
}

/// Look a character code up in a format-4 `cmap` subtable.
fn lookup_cmap4(table: &[u8], code: u32) -> Option<u16> {
    if code > 0xFFFF || table.len() < 14 {
        return None;
    }
    let code = code as u16;
    let seg_count_x2 = u16::from_be_bytes([table[6], table[7]]) as usize;
    let seg_count = seg_count_x2 / 2;
    let end_codes = 14;
    let start_codes = end_codes + seg_count_x2 + 2; // +2 for reservedPad
    let id_deltas = start_codes + seg_count_x2;
    let id_range_offsets = id_deltas + seg_count_x2;

    for segment in 0..seg_count {
        let at = end_codes + segment * 2;
        let end = u16::from_be_bytes([*table.get(at)?, *table.get(at + 1)?]);
        if code > end {
            continue;
        }
        let at = start_codes + segment * 2;
        let start = u16::from_be_bytes([*table.get(at)?, *table.get(at + 1)?]);
        if code < start {
            // Segments are sorted, so this code is absent.
            return None;
        }
        let at = id_deltas + segment * 2;
        let delta = i16::from_be_bytes([*table.get(at)?, *table.get(at + 1)?]);
        let at = id_range_offsets + segment * 2;
        let range_offset = u16::from_be_bytes([*table.get(at)?, *table.get(at + 1)?]);
        if range_offset == 0 {
            return Some(code.wrapping_add(delta as u16));
        }
        // The glyph is reached through a shared range table.
        let glyph_at =
            id_range_offsets + segment * 2 + range_offset as usize + (code - start) as usize * 2;
        let glyph = u16::from_be_bytes([*table.get(glyph_at)?, *table.get(glyph_at + 1)?]);
        if glyph == 0 {
            return None;
        }
        return Some(glyph.wrapping_add(delta as u16));
    }
    None
}
