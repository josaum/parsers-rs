//! Minimal CFF (Compact Font Format) outline extraction.
//!
//! # Why this exists
//!
//! The glyph atlas recovers a character from an outline. A Type 3 font reaches
//! its outlines because they are content streams; a `glyf`-based TrueType font
//! reaches them through a table; an OpenType font with **CFF outlines** keeps
//! them as *charstrings*, a bytecode of its own. Without an interpreter those
//! outlines are unreachable, and a document embedding an OpenType font — which
//! is what LaTeX and most design tools produce — gives the atlas nothing.
//!
//! # What is read
//!
//! - The `CFF ` table of an `OTTO` sfnt, or a bare CFF stream.
//! - `INDEX` structures, `DICT` operands, `charset` for glyph names, `Private`
//!   for `Subrs`/`defaultWidthX`/`nominalWidthX`.
//! - Type 2 charstrings: the moveto/lineto/curve operators, the hint operators,
//!   and the ten `callsubr`/`callgsubr` families including bias.
//!
//! # What is deliberately not read
//!
//! No hinting beyond skipping it (hints adjust rasterization, not the designed
//! outline), no `seac` accent composition, no `FDArray`/`FDSelect` per-glyph
//! private dicts, and no CID-keyed fonts. Each of those returns `None` rather
//! than an approximation: a plausible wrong outline is exactly what the atlas
//! compares against, so a refusal is safer than a guess.

use std::collections::HashMap;

/// CFF draws the same outlines TrueType does, so it reuses that type rather
/// than defining a parallel one: the atlas compares point sequences, and two
/// representations of one shape would only invite them to drift.
pub use crate::truetype::{GlyphOutline, OutlinePoint};

/// A parsed CFF font.
#[derive(Debug, Clone)]
pub struct CffFont {
    charstrings: Vec<(usize, usize)>,
    global_subrs: Vec<(usize, usize)>,
    local_subrs: Vec<(usize, usize)>,
    default_width_x: f32,
    nominal_width_x: f32,
    units_per_em: f32,
    /// Glyph name to glyph id, from the charset.
    names: HashMap<String, u16>,
    /// Glyph id to glyph name, for diagnostics and callers that key by name.
    order: Vec<String>,
    data: Vec<u8>,
}

impl CffFont {
    /// Parse a CFF table or bare CFF stream.
    ///
    /// Returns `None` for a CID-keyed font, a truncated file, or anything whose
    /// structure does not match the format. `units_per_em` comes from the
    /// caller's `head` table when the font is an `OTTO` sfnt; a bare CFF stream
    /// carries its own `FontMatrix`, and passes `None` to use that.
    pub fn parse(data: &[u8], units_per_em: Option<f32>) -> Option<Self> {
        let header_size = *data.get(2)? as usize;
        if !(4..=data.len()).contains(&header_size) {
            return None;
        }

        // Name INDEX, Top DICT INDEX, String INDEX, Global Subrs, in that order.
        let mut cursor = header_size;
        let (names, next) = read_index(data, cursor)?;
        cursor = next;
        let (top_dicts, next) = read_index(data, cursor)?;
        cursor = next;
        let (strings, next) = read_index(data, cursor)?;
        cursor = next;
        let (global_subrs, _) = read_index(data, cursor)?;
        if names.is_empty() {
            return None;
        }

        let (dict_start, dict_end) = *top_dicts.first()?;
        let dict = parse_dict(data, dict_start, dict_end)?;

        // A CID-keyed font has `ROS`; its charstrings are indexed by CID and its
        // charset is a CID map, neither of which this reader handles.
        if dict.contains_key(&(12, 30)) {
            return None;
        }
        // charstringType 2 is the default and the only language implemented
        // here; Type 1 charstrings are a different one.
        if let Some(kind) = dict_number(data, &dict, (0, 6)) {
            if kind != 2.0 {
                return None;
            }
        }

        let charstrings_index = dict_offset(data, &dict, (0, 17))?;
        let (charstrings, _) = read_index(data, charstrings_index)?;

        // `Private` is a pair: size then offset. Parsing to the end of the file
        // instead walks past the private DICT into whatever follows.
        let (private_start, private_end) = match dict.get(&(0, 18)) {
            // `/Private` is `<size> <offset>`, in that order. Reading them the
            // other way round silently skips the dict: every Private-derived
            // value, including `nominalWidthX`, comes back as its default.
            Some(operands) if operands.len() >= 2 => {
                let size = single_number(data, operands[0].0, operands[0].1)? as usize;
                let offset = single_number(data, operands[1].0, operands[1].1)? as usize;
                (offset, (offset + size).min(data.len()))
            }
            _ => (0, 0),
        };
        let private = if private_end > private_start {
            parse_dict(data, private_start, private_end)?
        } else {
            Dict::new()
        };
        let local_subrs = match dict_offset(data, &private, (0, 19)) {
            // A Subrs offset is relative to the Private DICT start.
            Some(offset) => read_index(data, private_start + offset)
                .map(|(entries, _)| entries)
                .unwrap_or_default(),
            None => Vec::new(),
        };

        // The font matrix gives the em size for a bare CFF stream; an sfnt's
        // `head` table is authoritative when present.
        let matrix_em = dict
            .get(&(12, 7))
            .and_then(|operands| single_number(data, operands[0].0, operands[0].1))
            .filter(|scale| *scale > 0.0)
            .map(|scale| 1.0 / scale);
        let units_per_em = units_per_em.or(matrix_em).unwrap_or(1000.0);
        if !(units_per_em.is_finite() && units_per_em > 0.0) {
            return None;
        }

        // 0, 1, and 2 name predefined charsets whose tables this crate does not
        // ship; a wrong glyph name would mislabel every outline, so only an
        // explicit charset is accepted.
        let charset_offset = dict_offset(data, &dict, (0, 15)).unwrap_or(0);
        if charset_offset <= 2 {
            return None;
        }
        let order = read_charset(data, charset_offset, charstrings.len(), &strings)?;
        let names = order
            .iter()
            .enumerate()
            .map(|(index, name)| (name.clone(), index as u16))
            .collect();

        Some(Self {
            charstrings,
            global_subrs,
            local_subrs,
            default_width_x: dict_number(data, &private, (0, 20)).unwrap_or(0.0),
            nominal_width_x: dict_number(data, &private, (0, 21)).unwrap_or(0.0),
            units_per_em,
            names,
            order,
            data: data.to_vec(),
        })
    }

    /// Design units per em, the denominator of the font matrix.
    pub fn units_per_em(&self) -> f32 {
        self.units_per_em
    }

    /// Glyph id for a glyph name, as the PDF encoding names it.
    ///
    /// CFF charstrings are reached by *name* through the charset, not by code:
    /// the PDF content stream supplies a code, the PDF encoding turns that into
    /// a name, and the charset turns the name into an index.
    pub fn glyph_id(&self, name: &str) -> Option<u16> {
        self.names.get(name).copied()
    }

    /// Glyph name for an index, when a caller needs to report one.
    pub fn glyph_name(&self, index: u16) -> Option<&str> {
        self.order.get(index as usize).map(String::as_str)
    }

    /// Advance width for a glyph id, in font units.
    ///
    /// The width is not a table in CFF: it is the first operand of a charstring,
    /// present only when the charstring declares one. Absent, it is
    /// `defaultWidthX`; present, it is relative to `nominalWidthX`.
    pub fn advance_units(&self, index: u16) -> Option<f32> {
        let width = self.charstring_width(index)?;
        Some(width)
    }

    /// Outline for a glyph id, in font units, y-up.
    pub fn outline(&self, index: u16) -> Option<GlyphOutline> {
        let (start, end) = *self.charstrings.get(index as usize)?;
        let mut interpreter = Interpreter {
            font: self,
            stack: Vec::new(),
            outline: GlyphOutline::default(),
            contour: Vec::new(),
            x: 0.0,
            y: 0.0,
            width: None,
            width_known: false,
            depth: 0,
            open: false,
            hint_count: 0,
        };
        interpreter.run(start, end, false)?;
        interpreter.close();
        if interpreter.outline.contours.is_empty() {
            return None;
        }
        Some(interpreter.outline)
    }

    fn charstring_width(&self, index: u16) -> Option<f32> {
        let (start, end) = *self.charstrings.get(index as usize)?;
        let mut interpreter = Interpreter {
            font: self,
            stack: Vec::new(),
            outline: GlyphOutline::default(),
            contour: Vec::new(),
            x: 0.0,
            y: 0.0,
            width: None,
            width_known: false,
            depth: 0,
            open: false,
            hint_count: 0,
        };
        // Only the width is wanted; running the whole charstring is the only way
        // to know whether a width operand is present, because the first
        // operator's argument count is what decides.
        interpreter.run(start, end, true)?;
        match interpreter.width {
            // A width operand is relative to `nominalWidthX`.
            Some(w) => Some(self.nominal_width_x + w),
            // No operand: the font's `defaultWidthX`.
            None => Some(self.default_width_x),
        }
    }
}

/// One charstring being interpreted.
struct Interpreter<'a> {
    font: &'a CffFont,
    stack: Vec<f32>,
    outline: GlyphOutline,
    contour: Vec<OutlinePoint>,
    x: f32,
    y: f32,
    /// The width operand, once the first stack-clearing operator has settled
    /// whether one is present — `None` means "not yet known".
    width: Option<f32>,
    /// Whether the width has been settled.
    ///
    /// A separate flag rather than a sentinel inside `width`: `Some(NaN)` is
    /// still `Some`, so using the value to mean "absent" makes the very next
    /// operator think the width is already known and skip its own detection.
    width_known: bool,
    depth: usize,
    open: bool,
    /// Hints declared so far, which decides a hintmask's byte length.
    ///
    /// Held on the interpreter rather than derived from the stack because the
    /// hint operators consume their operands before the mask length can be
    /// computed, and the count only ever grows.
    hint_count: usize,
}

impl Interpreter<'_> {
    fn run(&mut self, start: usize, end: usize, width_only: bool) -> Option<()> {
        if self.depth > 10 {
            return None;
        }
        let data = &self.font.data;
        let mut i = start;
        while i < end {
            let byte = *data.get(i)?;
            match byte {
                // Operands. The widths are chosen so the value lands in
                // `b0 - 139` (short) or the two-byte forms below.
                32..=246 => {
                    self.stack.push(byte as f32 - 139.0);
                    i += 1;
                }
                247..=250 => {
                    let b1 = *data.get(i + 1)? as f32;
                    self.stack.push((byte as f32 - 247.0) * 256.0 + b1 + 108.0);
                    i += 2;
                }
                251..=254 => {
                    let b1 = *data.get(i + 1)? as f32;
                    self.stack.push(-(byte as f32 - 251.0) * 256.0 - b1 - 108.0);
                    i += 2;
                }
                28 => {
                    let value = i16::from_be_bytes([*data.get(i + 1)?, *data.get(i + 2)?]);
                    self.stack.push(value as f32);
                    i += 3;
                }
                255 => {
                    // 16.16 fixed point.
                    let raw = i32::from_be_bytes([
                        *data.get(i + 1)?,
                        *data.get(i + 2)?,
                        *data.get(i + 3)?,
                        *data.get(i + 4)?,
                    ]);
                    self.stack.push(raw as f32 / 65536.0);
                    i += 5;
                }
                operator => {
                    let consumed = match operator {
                        1 | 3 | 18 | 23 => {
                            // hstem / vstem / hstemhm / vstemhm: hints, which
                            // adjust rasterization rather than the designed
                            // outline, so the operands are dropped.
                            self.maybe_width(None);
                            if width_only { return Some(()); }
                            // The stem operators consume their operands. Not
                            // clearing leaves them on the stack, where the next
                            // operator — `endchar`, which takes none — reads one
                            // back as its own optional width.
                            self.stack.clear();
                            i += 1;
                            false
                        }
                        19 | 20 => {
                            // hintmask / cntrmask: the mask is
                            // ceil(hint_count / 8) bytes, and the hint count
                            // includes any operands on the stack.
                            self.maybe_width(None);
                            if width_only { return Some(()); }
                            let hints = self.stack.len() / 2 + self.hint_count;
                            self.hint_count = hints;
                            self.stack.clear();
                            i += 1 + (hints + 7) / 8;
                            false
                        }
                        21 => {
                            // rmoveto
                            self.maybe_width(Some(2));
                            if width_only { return Some(()); }
                            if self.stack.len() < 2 {
                                return None;
                            }
                            let dy = self.stack.pop()?;
                            let dx = self.stack.pop()?;
                            self.start_contour(dx, dy);
                            self.stack.clear();
                            i += 1;
                            false
                        }
                        22 => {
                            // hmoveto
                            self.maybe_width(Some(1));
                            if width_only { return Some(()); }
                            let dx = self.stack.pop()?;
                            self.start_contour(dx, 0.0);
                            self.stack.clear();
                            i += 1;
                            false
                        }
                        4 => {
                            // vmoveto
                            self.maybe_width(Some(1));
                            if width_only { return Some(()); }
                            let dy = self.stack.pop()?;
                            self.start_contour(0.0, dy);
                            self.stack.clear();
                            i += 1;
                            false
                        }
                        5 => {
                            // rlineto: pairs
                            self.line_to_pairs();
                            self.stack.clear();
                            i += 1;
                            false
                        }
                        6 | 7 => {
                            // hlineto / vlineto: alternating, starting on the
                            // axis the operator names.
                            let mut horizontal = operator == 6;
                            for value in std::mem::take(&mut self.stack) {
                                if horizontal {
                                    self.line(value, 0.0);
                                } else {
                                    self.line(0.0, value);
                                }
                                horizontal = !horizontal;
                            }
                            i += 1;
                            false
                        }
                        8 => {
                            // rrcurveto: six operands per curve
                            let values = std::mem::take(&mut self.stack);
                            if values.len() % 6 != 0 {
                                return None;
                            }
                            for chunk in values.chunks(6) {
                                self.curve(
                                    chunk[0], chunk[1], chunk[2], chunk[3], chunk[4], chunk[5],
                                );
                            }
                            i += 1;
                            false
                        }
                        24 => {
                            // rcurveline: curves then one line
                            let values = std::mem::take(&mut self.stack);
                            if values.len() < 8 || (values.len() - 2) % 6 != 0 {
                                return None;
                            }
                            let curves = (values.len() - 2) / 6;
                            for index in 0..curves {
                                let c = &values[index * 6..index * 6 + 6];
                                self.curve(c[0], c[1], c[2], c[3], c[4], c[5]);
                            }
                            self.line(values[curves * 6], values[curves * 6 + 1]);
                            i += 1;
                            false
                        }
                        25 => {
                            // rlinecurve: one line then curves
                            let values = std::mem::take(&mut self.stack);
                            if values.len() < 8 || (values.len() - 2) % 6 != 0 {
                                return None;
                            }
                            self.line(values[0], values[1]);
                            let mut index = 2;
                            while index + 5 < values.len() {
                                self.curve(
                                    values[index],
                                    values[index + 1],
                                    values[index + 2],
                                    values[index + 3],
                                    values[index + 4],
                                    values[index + 5],
                                );
                                index += 6;
                            }
                            i += 1;
                            false
                        }
                        26 | 27 => {
                            // vvcurveto / hhcurveto: one optional leading
                            // coordinate, then four per curve.
                            let values = std::mem::take(&mut self.stack);
                            let odd = values.len() % 4 == 1;
                            let mut index = 0;
                            let mut first = 0.0;
                            if odd {
                                first = values[0];
                                index = 1;
                            }
                            if (values.len() - index) % 4 != 0 {
                                return None;
                            }
                            while index + 3 < values.len() {
                                let (a, b, c, d) = (
                                    values[index],
                                    values[index + 1],
                                    values[index + 2],
                                    values[index + 3],
                                );
                                if operator == 26 {
                                    // vvcurveto: dx1 c d dx2
                                    self.curve(first, a, b, c, d, 0.0);
                                    first = 0.0;
                                } else {
                                    // hhcurveto: dy1 c d dy2
                                    self.curve(a, first, b, c, 0.0, d);
                                    first = 0.0;
                                }
                                index += 4;
                            }
                            i += 1;
                            false
                        }
                        30 | 31 => {
                            // vhcurveto / hvcurveto: alternating, with an
                            // optional final coordinate when the count is odd.
                            let values = std::mem::take(&mut self.stack);
                            let mut horizontal = operator == 31;
                            let mut index = 0;
                            while index + 3 < values.len() {
                                let (a, b, c, d) = (
                                    values[index],
                                    values[index + 1],
                                    values[index + 2],
                                    values[index + 3],
                                );
                                // A trailing fifth value is the last curve's
                                // other axis, and only when it is the last one.
                                let extra = if index + 4 == values.len() - 1 {
                                    values[index + 4]
                                } else {
                                    0.0
                                };
                                if horizontal {
                                    self.curve(a, 0.0, b, c, extra, d);
                                } else {
                                    self.curve(0.0, a, b, c, d, extra);
                                }
                                horizontal = !horizontal;
                                index += 4;
                            }
                            i += 1;
                            false
                        }
                        10 => {
                            // callsubr
                            let index = self.stack.pop()?;
                            let local = self.font.local_subrs.clone();
                            self.call(&local, index, start, end)?;
                            i += 1;
                            false
                        }
                        29 => {
                            // callgsubr
                            let index = self.stack.pop()?;
                            let global = self.font.global_subrs.clone();
                            self.call(&global, index, start, end)?;
                            i += 1;
                            false
                        }
                        11 => {
                            // return: the caller resumes after its callsubr.
                            self.close();
                            return Some(());
                        }
                        14 => {
                            // endchar
                            // `endchar` takes no operands, so a width here is
                            // always the single optional one.
                            self.maybe_width(Some(0));
                            if width_only { return Some(()); }
                            self.close();
                            return Some(());
                        }
                        12 => {
                            // Escape: two-byte operators.
                            let second = *data.get(i + 1)?;
                            match second {
                                35 => {
                                    // flex: two curves, 13 operands
                                    let values = std::mem::take(&mut self.stack);
                                    if values.len() < 13 {
                                        return None;
                                    }
                                    self.curve(
                                        values[0], values[1], values[2], values[3], values[4],
                                        values[5],
                                    );
                                    self.curve(
                                        values[6], values[7], values[8], values[9], values[10],
                                        values[11],
                                    );
                                    i += 2;
                                }
                                34 => {
                                    // hflex: 7 operands
                                    let values = std::mem::take(&mut self.stack);
                                    if values.len() < 7 {
                                        return None;
                                    }
                                    self.curve(values[0], 0.0, values[1], values[2], values[3], 0.0);
                                    self.curve(values[4], 0.0, values[5], -values[2], values[6], 0.0);
                                    i += 2;
                                }
                                36 => {
                                    // hflex1: 9 operands
                                    let values = std::mem::take(&mut self.stack);
                                    if values.len() < 9 {
                                        return None;
                                    }
                                    self.curve(values[0], values[1], values[2], values[3], values[4], 0.0);
                                    self.curve(values[5], 0.0, values[6], values[7], values[8], -(values[1] + values[3] + 0.0));
                                    i += 2;
                                }
                                37 => {
                                    // flex1: 11 operands
                                    let values = std::mem::take(&mut self.stack);
                                    if values.len() < 11 {
                                        return None;
                                    }
                                    let dx = values[0] + values[2] + values[4] + values[6] + values[8];
                                    let dy = values[1] + values[3] + values[5] + values[7] + values[9];
                                    self.curve(values[0], values[1], values[2], values[3], values[4], values[5]);
                                    let (last_x, last_y) = if dx.abs() > dy.abs() {
                                        (values[10], -dy)
                                    } else {
                                        (-dx, values[10])
                                    };
                                    self.curve(values[6], values[7], values[8], values[9], last_x, last_y);
                                    i += 2;
                                }
                                _ => {
                                    // arithmetic, storage, and the rest of the
                                    // escaped set: not needed to draw an
                                    // outline, and skipping the operands would
                                    // corrupt the stack, so refuse.
                                    return None;
                                }
                            }
                            false
                        }
                        _ => return None,
                    };
                    let _ = consumed;
                }
            }
            if width_only && self.width.is_some() && !self.open {
                // The width was resolved by the first moveto; nothing further
                // is needed to report it.
                return Some(());
            }
        }
        Some(())
    }

    /// Resolve a subroutine call and run it under this interpreter.
    fn call(
        &mut self,
        subs: &[(usize, usize)],
        raw_index: f32,
        _start: usize,
        _end: usize,
    ) -> Option<()> {
        if subs.is_empty() {
            return None;
        }
        // Subroutine indices are biased so that the most-used subroutines get
        // the short operand encodings.
        let bias = if subs.len() < 1240 {
            107
        } else if subs.len() < 33900 {
            1131
        } else {
            32768
        };
        let index = raw_index as i32 + bias;
        if index < 0 {
            return None;
        }
        let (start, end) = *subs.get(index as usize)?;
        self.depth += 1;
        let result = self.run(start, end, false);
        self.depth -= 1;
        result
    }

    /// Width detection for the stem and hintmask operators.
    ///
    /// Their operand count is a multiple of two, and the spec states their
    /// `nargs` as the next odd number, so the count is `2k + 1` whether the
    /// width stage is still pending or has already passed.
    /// Establish the glyph width from the first stack-clearing operator.
    ///
    /// The width is an **optional leading operand**, and whether it is present
    /// has to be read off the operator itself:
    ///
    /// * A stem or hintmask operator consumes `(coordinate, delta)` pairs, so
    ///   its operand count is `2k` without a width and `2k + 1` with one — odd
    ///   exactly when a width is present, whatever the number of hints. The
    ///   width sits at the bottom of the stack, ahead of the pairs.
    /// * Every other operator has a fixed arity `n`, so the count is `n + 1`
    ///   exactly when a width is present.
    ///
    /// `nargs` is that parity, not an operand count: `Some(n)` for the fixed
    /// arities, `None` for the stem family. Encoding it as a count and comparing
    /// to the stack length — which is what an earlier revision did — is wrong for
    /// the stem family, because the same count is both "no width, one hint" and
    /// "width present, no hints".
    ///
    /// It must act **once**: running it on every hint operator lets a later one
    /// consume a real coordinate as a width, silently dropping the last operand
    /// of the following moveto.
    fn maybe_width(&mut self, nargs: Option<usize>) -> bool {
        if self.width_known {
            return false;
        }
        let present = match nargs {
            Some(n) => self.stack.len() == n + 1,
            None => self.stack.len() % 2 == 1,
        };
        self.width_known = true;
        if present {
            self.width = Some(self.stack.remove(0));
            true
        } else {
            false
        }
    }


    fn close(&mut self) {
        if self.contour.is_empty() {
            return;
        }
        let contour = std::mem::take(&mut self.contour);
        // CFF contours are implicitly closed; making it explicit keeps two
        // renderings of one shape comparable point for point.
        let mut closed = contour;
        if let Some(first) = closed.first().copied() {
            closed.push(first);
        }
        self.outline.contours.push(closed);
    }

    fn start_contour(&mut self, dx: f32, dy: f32) {
        self.close();
        self.x += dx;
        self.y += dy;
        self.contour.push(OutlinePoint {
            x: self.x,
            y: self.y,
            on_curve: true,
        });
        self.open = true;
    }

    fn line_to_pairs(&mut self) {
        let values = std::mem::take(&mut self.stack);
        for chunk in values.chunks(2) {
            if chunk.len() == 2 {
                self.line(chunk[0], chunk[1]);
            }
        }
    }

    fn line(&mut self, dx: f32, dy: f32) {
        if !self.open {
            // A line before any moveto is malformed; refusing keeps a stray
            // point out of the outline.
            return;
        }
        self.x += dx;
        self.y += dy;
        self.contour.push(OutlinePoint {
            x: self.x,
            y: self.y,
            on_curve: true,
        });
    }

    fn curve(&mut self, dx1: f32, dy1: f32, dx2: f32, dy2: f32, dx3: f32, dy3: f32) {
        if !self.open {
            return;
        }
        // Two cubic control points, then the endpoint. `kind` 2 marks a control
        // point, matching what the outline representation already carries.
        let c1 = (self.x + dx1, self.y + dy1);
        let c2 = (c1.0 + dx2, c1.1 + dy2);
        let end = (c2.0 + dx3, c2.1 + dy3);
        self.contour.push(OutlinePoint {
            x: c1.0,
            y: c1.1,
            on_curve: false,
        });
        self.contour.push(OutlinePoint {
            x: c2.0,
            y: c2.1,
            on_curve: false,
        });
        self.contour.push(OutlinePoint {
            x: end.0,
            y: end.1,
            on_curve: true,
        });
        self.x = end.0;
        self.y = end.1;
    }
}


// ── Container primitives ────────────────────────────────────────────────────

/// Read a CFF `INDEX`, returning the byte range of each entry and the offset
/// just past the structure.
///
/// An INDEX is a count, an offset array, then the concatenated data. An empty
/// INDEX is the single byte `0` — there is no offset array to read — which is a
/// case that returns successfully rather than as an error, because a font with
/// no local subroutines is ordinary.
fn read_index(data: &[u8], start: usize) -> Option<(Vec<(usize, usize)>, usize)> {
    let count = u16::from_be_bytes([*data.get(start)?, *data.get(start + 1)?]) as usize;
    if count == 0 {
        return Some((Vec::new(), start + 2));
    }
    // A one-byte count of 0 means "empty" in the DICT-INDEX form used by CFF2;
    // CFF 1 uses a two-byte count, and 0 there means the same thing.
    let offset_size = *data.get(start + 2)? as usize;
    if !(1..=4).contains(&offset_size) {
        return None;
    }
    let base = start + 3;
    let offsets_at = base;
    let data_at = base + (count + 1) * offset_size;
    if data_at > data.len() {
        return None;
    }
    let read_offset = |index: usize| -> Option<usize> {
        let at = offsets_at + index * offset_size;
        let mut value = 0usize;
        for byte in 0..offset_size {
            value = (value << 8) | *data.get(at + byte)? as usize;
        }
        Some(value)
    };
    let mut entries = Vec::with_capacity(count);
    for index in 0..count {
        let from = read_offset(index)?;
        let to = read_offset(index + 1)?;
        if to < from {
            return None;
        }
        // INDEX offsets are one-based relative to the data, so the first
        // object begins at `data_at + 1 - 1`. Reading them as zero-based shifts
        // every object one byte into its own contents — which for a Name INDEX
        // yields `abin-Regular` from a font actually called `Cabin-Regular`,
        // and for a CharStrings INDEX silently misaligns every outline.
        let range = (data_at + from - 1, data_at + to - 1);
        if range.1 > data.len() {
            return None;
        }
        entries.push(range);
    }
    let end = data_at + read_offset(count)?.saturating_sub(1);
    Some((entries, end.min(data.len())))
}

/// Parse a DICT into operator to operand.
///
/// Operators are one byte, or two when the first is 12. Offsets inside a DICT
/// are relative to the DICT's own start, so the raw byte range is kept for
/// values that must be resolved later — that resolution needs the start, which
/// this function does not have.
/// A DICT entry: the byte ranges of its operands, in order.
///
/// Every operand is kept, not just the first, because several entries are a
/// pair — `Private` is `size offset`, and reading only the offset leaves the
/// parser walking past the end of the private dict into whatever follows.
type Dict = HashMap<(u8, u8), Vec<(usize, usize)>>;

fn parse_dict(data: &[u8], start: usize, end: usize) -> Option<Dict> {
    let mut result: Dict = HashMap::new();
    let mut operands: Vec<(usize, usize)> = Vec::new();
    let mut i = start;
    while i < end {
        let byte = *data.get(i)?;
        match byte {
            28 => {
                operands.push((i, i + 3));
                i += 3;
            }
            29 => {
                operands.push((i, i + 5));
                i += 5;
            }
            30 => {
                // Real number, packed as nibbles until 0xf. The byte range is
                // all the caller needs; parsing the decimal is not.
                let mut cursor = i + 1;
                loop {
                    let b = *data.get(cursor)?;
                    cursor += 1;
                    if b & 0x0f == 0x0f || b >> 4 == 0x0f {
                        break;
                    }
                }
                operands.push((i, cursor));
                i = cursor;
            }
            32..=246 => {
                operands.push((i, i + 1));
                i += 1;
            }
            247..=254 => {
                operands.push((i, i + 2));
                i += 2;
            }
            12 => {
                let second = *data.get(i + 1)?;
                result.insert((12, second), std::mem::take(&mut operands));
                i += 2;
            }
            op => {
                // An operator with no operands is legal and means "the default";
                // recording it as empty keeps that distinction instead of
                // treating it as a parse failure.
                result.insert((0, op), std::mem::take(&mut operands));
                i += 1;
            }
        }
    }
    Some(result)
}

/// The single operand of a DICT entry, as a number.
fn dict_number(data: &[u8], dict: &Dict, key: (u8, u8)) -> Option<f32> {
    let operands = dict.get(&key)?;
    let range = *operands.first()?;
    single_number(data, range.0, range.1)
}

/// The single operand of a DICT entry, as an offset.
///
/// Resolves the operand's *value*, not its position: a DICT operand is stored
/// as a byte range, and returning the range's start would hand back an offset
/// into the DICT itself rather than the object the entry names.
fn dict_offset(data: &[u8], dict: &Dict, key: (u8, u8)) -> Option<usize> {
    let range = *dict.get(&key)?.first()?;
    single_number(data, range.0, range.1).map(|value| value.max(0.0) as usize)
}


/// A DICT operand as an integer.
///
/// The value is read from the byte range the DICT recorded, because a DICT that
/// points at a `CharStrings` INDEX stores the offset rather than the value.
fn single_number(data: &[u8], start: usize, end: usize) -> Option<f32> {
    let byte = *data.get(start)?;
    match byte {
        28 if start + 2 < end || start + 2 < data.len() => {
            Some(i16::from_be_bytes([*data.get(start + 1)?, *data.get(start + 2)?]) as f32)
        }
        29 => Some(i32::from_be_bytes([
            *data.get(start + 1)?,
            *data.get(start + 2)?,
            *data.get(start + 3)?,
            *data.get(start + 4)?,
        ]) as f32),
        32..=246 => Some(byte as f32 - 139.0),
        247..=250 => Some((byte as f32 - 247.0) * 256.0 + *data.get(start + 1)? as f32 + 108.0),
        251..=254 => Some(-(byte as f32 - 251.0) * 256.0 - *data.get(start + 1)? as f32 - 108.0),
        // A real number: read the digits. Only the integer part is needed for
        // the values that use it, which are offsets and counts.
        30 => {
            let mut text = String::new();
            let mut cursor = start + 1;
            'outer: loop {
                let b = *data.get(cursor)?;
                cursor += 1;
                for nibble in [b >> 4, b & 0x0f] {
                    match nibble {
                        0..=9 => text.push((b'0' + nibble) as char),
                        0x0a => text.push('.'),
                        0x0b => text.push('E'),
                        0x0c => text.push_str("E-"),
                        0x0e => text.push('-'),
                        0x0f => break 'outer,
                        _ => {}
                    }
                }
            }
            text.parse::<f32>().ok()
        }
        _ => None,
    }
}


// ── Charset ────────────────────────────────────────────────────────────────

/// Glyph names in glyph-id order.
///
/// Three formats exist and all three are read, because a subsetter picks
/// whichever is smallest: format 0 lists one SID per glyph, format 1 lists
/// ranges of consecutive SIDs, and format 2 does the same for CIDs. All are a
/// mapping from glyph id to a name, and only the encoding differs — reading
/// only format 0 rejects a font whose subsetter chose ranges, which is what
/// `fontTools` does for a font as small as this one.
///
/// A format 2 entry is a CID, not a name, and a CID names nothing without the
/// registry that issued it, so those glyphs get no name and the PDF encoding
/// that reaches them by name finds nothing. That is a partial result rather
/// than a refusal: the glyphs whose CIDs happen to fall in the standard-string
/// range keep working, and the rest do not mislabel anything.
fn read_charset(
    data: &[u8],
    start: usize,
    glyph_count: usize,
    strings: &[(usize, usize)],
) -> Option<Vec<String>> {
    let format = *data.get(start)?;
    let cid_keyed = format == 2;
    if format > 2 {
        return None;
    }
    let mut order = Vec::with_capacity(glyph_count);
    order.push(".notdef".to_string());
    let mut cursor = start + 1;
    if format == 0 {
        while order.len() < glyph_count {
            let sid = u16::from_be_bytes([*data.get(cursor)?, *data.get(cursor + 1)?]);
            cursor += 2;
            order.push(name_for(data, sid, cid_keyed, strings));
        }
        return Some(order);
    }
    // Formats 1 and 2: repeated (first, nLeft) pairs covering consecutive
    // values. The loop is bounded by the glyph count, so a malformed font
    // cannot make it run away.
    while order.len() < glyph_count {
        let first = u16::from_be_bytes([*data.get(cursor)?, *data.get(cursor + 1)?]);
        let left = *data.get(cursor + 2)? as usize;
        cursor += 3;
        for step in 0..=left {
            if order.len() >= glyph_count {
                break;
            }
            let value = first.saturating_add(step as u16);
            order.push(name_for(data, value, cid_keyed, strings));
        }
    }
    Some(order)
}

/// A charset entry as a glyph name.
fn name_for(data: &[u8], value: u16, cid_keyed: bool, strings: &[(usize, usize)]) -> String {
    if cid_keyed {
        // A CID is not a SID. Only the predefined range can be named without
        // the issuing registry, and guessing further would mislabel outlines.
        return if (value as usize) < STANDARD_STRINGS.len() {
            STANDARD_STRINGS[value as usize].to_string()
        } else {
            format!("cid{value}")
        };
    }
    sid_to_name(data, value, strings)
}

/// Resolve a string id to a glyph name.
///
/// The first 391 SIDs are the predefined Standard Strings; anything at or above
/// that is an offset into the font's String INDEX, which is why the INDEX is
/// passed in rather than a resolved name table.
fn sid_to_name(data: &[u8], sid: u16, strings: &[(usize, usize)]) -> String {
    if (sid as usize) < STANDARD_STRINGS.len() {
        return STANDARD_STRINGS[sid as usize].to_string();
    }
    let index = sid as usize - STANDARD_STRINGS.len();
    match strings.get(index) {
        Some((start, end)) => data
            .get(*start..*end)
            .map(|bytes| String::from_utf8_lossy(bytes).into_owned())
            .unwrap_or_default(),
        None => String::new(),
    }
}
/// The 391 predefined CFF Standard Strings.
///
/// A charset stores a *string id* per glyph; ids below this table's length name
/// a standard string and anything at or above it indexes the font's String INDEX.
/// An embedded font almost always uses the standard names — `space`, `A`, `period` —
/// so this table is not optional: without it those glyphs have no name, and the PDF
/// encoding that reaches them by name finds nothing.
const STANDARD_STRINGS: [&str; 391] = [
    ".notdef",
    "space",
    "exclam",
    "quotedbl",
    "numbersign",
    "dollar",
    "percent",
    "ampersand",
    "quoteright",
    "parenleft",
    "parenright",
    "asterisk",
    "plus",
    "comma",
    "hyphen",
    "period",
    "slash",
    "zero",
    "one",
    "two",
    "three",
    "four",
    "five",
    "six",
    "seven",
    "eight",
    "nine",
    "colon",
    "semicolon",
    "less",
    "equal",
    "greater",
    "question",
    "at",
    "A",
    "B",
    "C",
    "D",
    "E",
    "F",
    "G",
    "H",
    "I",
    "J",
    "K",
    "L",
    "M",
    "N",
    "O",
    "P",
    "Q",
    "R",
    "S",
    "T",
    "U",
    "V",
    "W",
    "X",
    "Y",
    "Z",
    "bracketleft",
    "backslash",
    "bracketright",
    "asciicircum",
    "underscore",
    "quoteleft",
    "a",
    "b",
    "c",
    "d",
    "e",
    "f",
    "g",
    "h",
    "i",
    "j",
    "k",
    "l",
    "m",
    "n",
    "o",
    "p",
    "q",
    "r",
    "s",
    "t",
    "u",
    "v",
    "w",
    "x",
    "y",
    "z",
    "braceleft",
    "bar",
    "braceright",
    "asciitilde",
    "exclamdown",
    "cent",
    "sterling",
    "fraction",
    "yen",
    "florin",
    "section",
    "currency",
    "quotesingle",
    "quotedblleft",
    "guillemotleft",
    "guilsinglleft",
    "guilsinglright",
    "fi",
    "fl",
    "endash",
    "dagger",
    "daggerdbl",
    "periodcentered",
    "paragraph",
    "bullet",
    "quotesinglbase",
    "quotedblbase",
    "quotedblright",
    "guillemotright",
    "ellipsis",
    "perthousand",
    "questiondown",
    "grave",
    "acute",
    "circumflex",
    "tilde",
    "macron",
    "breve",
    "dotaccent",
    "dieresis",
    "ring",
    "cedilla",
    "hungarumlaut",
    "ogonek",
    "caron",
    "emdash",
    "AE",
    "ordfeminine",
    "Lslash",
    "Oslash",
    "OE",
    "ordmasculine",
    "ae",
    "dotlessi",
    "lslash",
    "oslash",
    "oe",
    "germandbls",
    "onesuperior",
    "logicalnot",
    "mu",
    "trademark",
    "Eth",
    "onehalf",
    "plusminus",
    "Thorn",
    "onequarter",
    "divide",
    "brokenbar",
    "degree",
    "thorn",
    "threequarters",
    "twosuperior",
    "registered",
    "minus",
    "eth",
    "multiply",
    "threesuperior",
    "copyright",
    "Aacute",
    "Acircumflex",
    "Adieresis",
    "Agrave",
    "Aring",
    "Atilde",
    "Ccedilla",
    "Eacute",
    "Ecircumflex",
    "Edieresis",
    "Egrave",
    "Iacute",
    "Icircumflex",
    "Idieresis",
    "Igrave",
    "Ntilde",
    "Oacute",
    "Ocircumflex",
    "Odieresis",
    "Ograve",
    "Otilde",
    "Scaron",
    "Uacute",
    "Ucircumflex",
    "Udieresis",
    "Ugrave",
    "Yacute",
    "Ydieresis",
    "Zcaron",
    "aacute",
    "acircumflex",
    "adieresis",
    "agrave",
    "aring",
    "atilde",
    "ccedilla",
    "eacute",
    "ecircumflex",
    "edieresis",
    "egrave",
    "iacute",
    "icircumflex",
    "idieresis",
    "igrave",
    "ntilde",
    "oacute",
    "ocircumflex",
    "odieresis",
    "ograve",
    "otilde",
    "scaron",
    "uacute",
    "ucircumflex",
    "udieresis",
    "ugrave",
    "yacute",
    "ydieresis",
    "zcaron",
    "exclamsmall",
    "Hungarumlautsmall",
    "dollaroldstyle",
    "dollarsuperior",
    "ampersandsmall",
    "Acutesmall",
    "parenleftsuperior",
    "parenrightsuperior",
    "twodotenleader",
    "onedotenleader",
    "zerooldstyle",
    "oneoldstyle",
    "twooldstyle",
    "threeoldstyle",
    "fouroldstyle",
    "fiveoldstyle",
    "sixoldstyle",
    "sevenoldstyle",
    "eightoldstyle",
    "nineoldstyle",
    "commasuperior",
    "threequartersemdash",
    "periodsuperior",
    "questionsmall",
    "asuperior",
    "bsuperior",
    "centsuperior",
    "dsuperior",
    "esuperior",
    "isuperior",
    "lsuperior",
    "msuperior",
    "nsuperior",
    "osuperior",
    "rsuperior",
    "ssuperior",
    "tsuperior",
    "ff",
    "ffi",
    "ffl",
    "parenleftinferior",
    "parenrightinferior",
    "Circumflexsmall",
    "hyphensuperior",
    "Gravesmall",
    "Asmall",
    "Bsmall",
    "Csmall",
    "Dsmall",
    "Esmall",
    "Fsmall",
    "Gsmall",
    "Hsmall",
    "Ismall",
    "Jsmall",
    "Ksmall",
    "Lsmall",
    "Msmall",
    "Nsmall",
    "Osmall",
    "Psmall",
    "Qsmall",
    "Rsmall",
    "Ssmall",
    "Tsmall",
    "Usmall",
    "Vsmall",
    "Wsmall",
    "Xsmall",
    "Ysmall",
    "Zsmall",
    "colonmonetary",
    "onefitted",
    "rupiah",
    "Tildesmall",
    "exclamdownsmall",
    "centoldstyle",
    "Lslashsmall",
    "Scaronsmall",
    "Zcaronsmall",
    "Dieresissmall",
    "Brevesmall",
    "Caronsmall",
    "Dotaccentsmall",
    "Macronsmall",
    "figuredash",
    "hypheninferior",
    "Ogoneksmall",
    "Ringsmall",
    "Cedillasmall",
    "questiondownsmall",
    "oneeighth",
    "threeeighths",
    "fiveeighths",
    "seveneighths",
    "onethird",
    "twothirds",
    "zerosuperior",
    "foursuperior",
    "fivesuperior",
    "sixsuperior",
    "sevensuperior",
    "eightsuperior",
    "ninesuperior",
    "zeroinferior",
    "oneinferior",
    "twoinferior",
    "threeinferior",
    "fourinferior",
    "fiveinferior",
    "sixinferior",
    "seveninferior",
    "eightinferior",
    "nineinferior",
    "centinferior",
    "dollarinferior",
    "periodinferior",
    "commainferior",
    "Agravesmall",
    "Aacutesmall",
    "Acircumflexsmall",
    "Atildesmall",
    "Adieresissmall",
    "Aringsmall",
    "AEsmall",
    "Ccedillasmall",
    "Egravesmall",
    "Eacutesmall",
    "Ecircumflexsmall",
    "Edieresissmall",
    "Igravesmall",
    "Iacutesmall",
    "Icircumflexsmall",
    "Idieresissmall",
    "Ethsmall",
    "Ntildesmall",
    "Ogravesmall",
    "Oacutesmall",
    "Ocircumflexsmall",
    "Otildesmall",
    "Odieresissmall",
    "OEsmall",
    "Oslashsmall",
    "Ugravesmall",
    "Uacutesmall",
    "Ucircumflexsmall",
    "Udieresissmall",
    "Yacutesmall",
    "Thornsmall",
    "Ydieresissmall",
    "001.000",
    "001.001",
    "001.002",
    "001.003",
    "Black",
    "Bold",
    "Book",
    "Light",
    "Medium",
    "Regular",
    "Roman",
    "Semibold",
];


#[cfg(test)]
mod charstring_tests {
    use super::*;

    /// A font shell whose only job is to let the interpreter run.
    ///
    /// `data` is the charstring, and nothing else is referenced, so a test can
    /// hand the interpreter an exact byte sequence and assert what it drew.
    /// Driving the real fixture instead would make a failure ambiguous between
    /// the byte flow and the outline arithmetic.
    fn run(bytes: &[u8]) -> Option<GlyphOutline> {
        let font = CffFont {
            charstrings: vec![(0, bytes.len())],
            global_subrs: Vec::new(),
            local_subrs: Vec::new(),
            default_width_x: 500.0,
            nominal_width_x: 0.0,
            units_per_em: 1000.0,
            names: HashMap::new(),
            order: vec![".notdef".to_string()],
            data: bytes.to_vec(),
        };
        font.outline(0)
    }

    /// moveto then lineto, the simplest path, with a leading width operand.
    #[test]
    fn a_width_operand_does_not_eat_a_coordinate() {
        // 100 w, 0 0 rmoveto, 10 20 rlineto, endchar.
        // 100 -> 239, 0 -> 139, 10 -> 149, 20 -> 159.
        let bytes = [239u8, 139, 139, 21, 149, 159, 5, 14];
        let outline = run(&bytes).expect("a moveto and a lineto must draw");
        assert_eq!(outline.contours.len(), 1);
        let points: Vec<(f32, f32)> = outline.contours[0].iter().map(|p| (p.x, p.y)).collect();
        assert_eq!(
            points,
            vec![(0.0, 0.0), (10.0, 20.0), (0.0, 0.0)],
            "the moveto must land at the origin and the lineto at (10, 20); a \
             coordinate eaten as a width would show here"
        );
    }

    /// The same path with no width operand.
    #[test]
    fn a_path_without_a_width_operand_still_draws() {
        let bytes = [139u8, 139, 21, 149, 159, 5, 14];
        let outline = run(&bytes).expect("must draw");
        let points: Vec<(f32, f32)> = outline.contours[0].iter().map(|p| (p.x, p.y)).collect();
        assert_eq!(points, vec![(0.0, 0.0), (10.0, 20.0), (0.0, 0.0)]);
    }

    /// Width detection must happen once, on the first stack-clearing operator.
    ///
    /// This is the regression: running it on every hint operator lets a later
    /// one consume a real coordinate as if it were a width, silently dropping
    /// the last operand of the following moveto.
    #[test]
    fn a_hint_operand_does_not_eat_a_later_coordinate() {
        // 100 w, 20 vstem, 300 vstem, 0 0 rmoveto, 10 20 rlineto, endchar.
        let bytes = [
            239u8, // 100: width
            159, 1, // 20 vstem (2 operands, no width needed)
            248, 53, 1, // 300 vstem (two-byte form)
            139, 139, 21, // 0 0 rmoveto
            149, 159, 5,  // 10 20 rlineto
            14,
        ];
        let outline = run(&bytes).expect("must draw");
        let points: Vec<(f32, f32)> = outline.contours[0].iter().map(|p| (p.x, p.y)).collect();
        assert_eq!(
            points,
            vec![(0.0, 0.0), (10.0, 20.0), (0.0, 0.0)],
            "the second stem must not consume a moveto coordinate"
        );
    }

    /// The width is read from the first stack-clearing operator, or comes from
    /// the font default when absent.
    #[test]
    fn the_width_is_read_once_or_defaulted() {
        let width_of = |bytes: &[u8]| {
            let font = CffFont {
                charstrings: vec![(0, bytes.len())],
                global_subrs: Vec::new(),
                local_subrs: Vec::new(),
                default_width_x: 500.0,
                nominal_width_x: 0.0,
                units_per_em: 1000.0,
                names: HashMap::new(),
                order: vec![".notdef".to_string()],
                data: bytes.to_vec(),
            };
            font.advance_units(0)
        };
        // Width 100 present, one stem pair: three operands, so odd.
        assert_eq!(
            width_of(&[239u8, 159, 179, 1, 139, 139, 21, 14]),
            Some(100.0)
        );
        // No width, one stem pair: two operands, so even, and the glyph uses
        // the default.
        assert_eq!(width_of(&[159u8, 179, 1, 139, 139, 21, 14]), Some(500.0));
        // Width 100 present, three stem pairs: seven operands, still odd.
        assert_eq!(
            width_of(&[
                239u8, 179, 179, 179, 179, 179, 179, 1, 139, 139, 21, 14
            ]),
            Some(100.0)
        );
        // No width, three stem pairs: six operands, still even.
        assert_eq!(
            width_of(&[179u8, 179, 179, 179, 179, 179, 1, 139, 139, 21, 14]),
            Some(500.0)
        );
    }

    /// A subroutine call resolves through the bias, which is what lets a
    /// charstring share repeated geometry.
    #[test]
    fn a_subroutine_call_is_followed() {
        // Local subr 0 draws a lineto; the caller does moveto, callsubr, endchar.
        // With one subr the bias is 107, so `callsubr` takes -107.
        let subr = [149u8, 159, 5, 11]; // 10 20 rlineto, return
        let main = [139u8, 139, 21, /* -107 */ 32, 10, 14];
        let mut data = subr.to_vec();
        let offset = data.len();
        data.extend_from_slice(&main);
        let font = CffFont {
            charstrings: vec![(offset, data.len())],
            global_subrs: Vec::new(),
            local_subrs: vec![(0, 4)],
            default_width_x: 0.0,
            nominal_width_x: 0.0,
            units_per_em: 1000.0,
            names: HashMap::new(),
            order: vec![".notdef".to_string()],
            data,
        };
        let outline = font.outline(0).expect("the caller must draw");
        let points: Vec<(f32, f32)> = outline.contours[0].iter().map(|p| (p.x, p.y)).collect();
        assert_eq!(points, vec![(0.0, 0.0), (10.0, 20.0), (0.0, 0.0)]);
    }

    /// A cubic contributes two controls and an endpoint, marked so a consumer
    /// can rebuild the curve.
    #[test]
    fn a_curve_marks_its_control_points() {
        // 0 0 rmoveto, then 1 2 3 4 5 6 rrcurveto, endchar.
        let bytes = [139u8, 139, 21, 140, 141, 142, 143, 144, 145, 8, 14];
        let outline = run(&bytes).expect("must draw");
        let contour = &outline.contours[0];
        // moveto + two controls + endpoint + the closing repeat.
        assert_eq!(contour.len(), 5, "moveto, two controls, endpoint, close");
        assert!(contour[0].on_curve, "the moveto is on the curve");
        assert!(!contour[1].on_curve, "first control");
        assert!(!contour[2].on_curve, "second control");
        assert!(contour[3].on_curve, "the endpoint is on the curve");
        assert_eq!((contour[1].x, contour[1].y), (1.0, 2.0));
        assert_eq!((contour[2].x, contour[2].y), (4.0, 6.0));
        assert_eq!((contour[3].x, contour[3].y), (9.0, 12.0));
    }

    /// An unknown operator is refused rather than skipped, because skipping it
    /// would leave its operands on the stack and corrupt every later operator.
    #[test]
    fn an_unknown_operator_is_refused() {
        // A byte the format leaves unassigned. The assigned operators are
        // 1-11, 12 (escape), 13-14, 18-31 and 255 is the fixed-point operand
        // prefix, so 17 is genuinely unassigned.
        assert!(run(&[139u8, 139, 21, 17, 14]).is_none());
    }
}
