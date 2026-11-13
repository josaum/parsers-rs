//! Bounds what the full interpreter is asked to render, so that it does not
//! run out of stack.
//!
//! The interpreter (`hayro`) recurses with no limit in many places, and a
//! stack overflow aborts the process. It therefore runs on a thread of its
//! own with a stack of [`RENDER_STACK_BYTES`], and [`vet_page`] runs there
//! first, on the very page object about to be rendered.
//!
//! The check uses the interpreter's own object model: the same page object,
//! the same `Resources` chain with its fallback to the enclosing stream's
//! resources, the same content tokenizer. Two details of that model decide
//! what recurses, and a second parser would get both wrong: a name missing
//! from a stream's own resources is looked up in every enclosing stream's,
//! and a reference back into an object the lookup is already inside comes
//! back empty. The second means the same stream can resolve, and even
//! decode, differently on different paths, so the walk follows contexts,
//! not streams.
//!
//! # Recursion in the interpreter, and what bounds it
//!
//! Every function in `hayro` 0.5.0, `hayro-interpret` 0.5.0, `hayro-syntax`
//! 0.5.0 and `hayro-font` 0.4.0 that calls itself, found by searching each
//! function body for a call to its own name, plus the cycles through one
//! other function known from reading (marked "one hop"). The image decoder
//! crates (`hayro-jpeg2000`, `hayro-jbig2`, `hayro-ccitt`) were not searched.
//!
//! Bounded by this module:
//!
//! | recursion | bound |
//! |---|---|
//! | `interpret` and `draw_form_xobject` (`Do`, annotation `/AP /N`; one hop) | [`STACK_BUDGET`]; a stream beneath itself is refused |
//! | `interpret` and `TilingPattern::interpret` (one hop) | the same |
//! | `interpret` and `SoftMask::interpret` (one hop) | the same |
//! | `interpret` and `Type3::render_glyph` (one hop) | glyph procedures are walked with the font's own resources only, and what they enter is charged to [`STACK_BUDGET`] |
//! | `Resources` clone, lookup and drop (derived, one frame per level) | the same budget: the chain is as long as the nesting |
//! | `page::resolve_pages` | `MAX_PAGE_TREE_DEPTH`, on this crate's own reading of the file |
//! | `Function::new` and stitching functions (one hop) | [`MAX_OBJECT_DEPTH`] |
//! | `ColorSpace::new` and `/Indexed`, `/Separation`, `/DeviceN`, `/ICCBased` (one hop) | [`MAX_OBJECT_DEPTH`] |
//! | `function::type4::parse_procedure_inner` | [`MAX_PROGRAM_NESTING`] |
//! | `function::type4::eval_inner` | the same: it recurses once per parsed procedure level |
//! | array and dictionary `skip`/`read` in a content stream (one hop) | [`MAX_CONTENT_NESTING`] |
//! | `font::cmap::CMapLexer::get_obj`, once per consecutive comment line in a font's `/Encoding` or `/ToUnicode` CMap | [`MAX_CMAP_COMMENT_RUN`] |
//! | `function::type0::interpolate_inner`, one level per input of a sampled function and twice the work per input | [`MAX_SAMPLED_FUNCTION_INPUTS`] |
//! | `function::type0::increment_index`, one level per input of a sampled function | the same |
//!
//! Bounded by the interpreter itself:
//!
//! | recursion | bound |
//! |---|---|
//! | `xref::fallback_xref_map_inner` | a flag allows one level |
//! | `hayro-font` `cff::_parse_char_string` | subroutine depth 10 |
//! | `hayro-font` `type1::charstring::_parse_char_string` | subroutine depth 10 |
//!
//! Bounded only by the size of the render stack:
//!
//! | recursion | what drives it |
//! |---|---|
//! | array and dictionary `skip`/`read` on objects in the file itself | nesting in any object's text; it runs while the document is opened, before this module sees anything |
//!
//! Matched by the search but not recursion: `hayro` `renderer::draw_image`
//! (calls a different method of the same name), `hayro-font`
//! `type1::decrypt::decrypt` (a closure of the same name), and
//! `hayro-interpret` `color::decode_l` (calls `decode_l_constant`).
//!
//! # What else is not bounded
//!
//! * **A page tree the two parsers read differently.** The page-tree bound
//!   uses this crate's parser, not the interpreter's.
//! * **The image decoders** (`hayro-jpeg2000`, `hayro-jbig2`, `hayro-ccitt`
//!   and JPEG) were not traced. Each may size its buffers from the
//!   codestream's own header rather than the dictionary.
//! * **Memory and time.** Every soft mask gets a renderer the size of the
//!   one it is drawn in, and how many soft masks a page uses is not bounded.
//!   Each saved graphics state copies the clip paths in force, and the
//!   number of those is not bounded.
//!
//! Bounded beyond recursion: the pixels an image XObject, its soft mask or
//! an inline image declares, and the pixels of the tiling cell renderers
//! held at once along one path of nested patterns, each by
//! [`image_pixel_budget`] (a cell is sized by `cell_pixels`, at the largest
//! scale the interpreter allows, since the transform is not known before
//! rendering); the interpreter's
//! saved-state stack, by [`MAX_SAVED_STATES`]; a sampled function's table, by
//! [`MAX_SAMPLED_FUNCTION_ENTRIES`], and its cost per evaluation, which
//! doubles with each input, by [`MAX_SAMPLED_FUNCTION_INPUTS`]. Every font a
//! stream selects, and every object it reaches, is held to
//! [`MAX_OBJECT_DEPTH`] like the resources are, although no recursion over
//! font objects is known in the interpreter.
//!
//! `/UseCMap` chains need no bound: `hayro-interpret` 0.5.0 logs the key
//! and treats the `usecmap` operator as a no-op, so it never loads a second
//! CMap, and a chain or a cycle costs it nothing.
//!
//! The search finds a function that names itself. A cycle through two or
//! more other functions that was not already known would not appear here.

use anyhow::{anyhow, Result};
use hayro::hayro_syntax::content::ops::TypedInstruction;
use hayro::hayro_syntax::content::TypedIter;
use hayro::hayro_syntax::object::{Array, Dict, MaybeRef, Name, Object, Stream};
use hayro::hayro_syntax::page::{Page, Resources};
use std::collections::{HashMap, HashSet};
use std::rc::Rc;

/// Stack given to the thread that opens, vets and renders a page.
///
/// The interpreter recurses and an overflow aborts the process, so it never
/// runs on the caller's stack, whose remaining size nobody knows. This is
/// reserved address space, not memory: the system commits a page only when
/// it is first touched, so a shallow page touches a few hundred kilobytes.
pub(crate) const RENDER_STACK_BYTES: usize = 64 * 1024 * 1024;

/// Deepest `/Kids` nesting accepted in the page tree.
///
/// The interpreter builds its page list by recursion with no limit, as soon
/// as it opens the document. A balanced tree over a million pages is about
/// twenty levels deep. The recursion costs roughly 1.3 KB a level, so this
/// is under 100 KB of [`RENDER_STACK_BYTES`].
pub(crate) const MAX_PAGE_TREE_DEPTH: usize = 64;

/// Stack the interpreter may need for one page's nested content streams.
///
/// One eighth of [`RENDER_STACK_BYTES`]. The rest is margin for the
/// recursion this budget does not count: the rasterizer at the bottom of the
/// deepest stream and the interpreter's recursion over objects.
///
/// The costs below were measured with `probe_interpreter_stack` in
/// `tests/render_guard.rs`, which renders a chain of nested streams on a
/// thread of a chosen stack size with no guard in front of it. The deepest
/// chain that survives at 512 KiB and at 2 MiB gives the bytes per level.
/// Measured against hayro 0.5.0 on aarch64 macOS (deepest surviving chain at
/// 512 KiB / 2 MiB, then bytes per level):
///
/// | route        | unoptimized         | optimized           |
/// |--------------|---------------------|---------------------|
/// | form `Do`    | 21 / 106, 18,504    | 127 / 504, 4,172    |
/// | annotation   | 22 / 107, 18,504    | same code as a form |
/// | tiling cell  | 14 / 72, 27,118     | 73 / 292, 7,182     |
/// | soft mask    | 17 / 84, 23,476     | 70 / 280, 7,490     |
/// | Type 3 glyph | 13 / 67, 29,127     | 82 / 327, 6,420     |
///
/// The fixed cost left over is about 146,000 bytes unoptimized. Every
/// profile uses the unoptimized figures, rounded up: how this crate was
/// compiled says nothing reliable about how the interpreter was. With them
/// the budget accepts 433 nested forms ((8,388,608 - 150,000) / 19,000),
/// 294 tiling cells or 343 soft masks, each needing at most 8 MiB of the
/// 64 MiB stack.
///
/// Raising the budget or lowering a cost without re-measuring trades a
/// refused render for an aborted process. So does upgrading the interpreter.
const STACK_BUDGET: u32 = 8 * 1024 * 1024;

/// Stack held before the page's own content stream starts.
const PAGE_COST: u32 = 150_000;

/// Stack one nested Form XObject (or annotation appearance) costs.
const FORM_COST: u32 = 19_000;

/// Stack one tiling pattern cell costs. A cell gets a renderer of its own.
const PATTERN_COST: u32 = 28_000;

/// Stack one soft-mask group costs. A group gets a renderer of its own.
const SOFT_MASK_COST: u32 = 24_000;

/// Stack one Type 3 glyph procedure costs.
const GLYPH_COST: u32 = 30_000;

/// Most contexts one render will vet.
///
/// Resources are inherited, so a stream is vetted once per place it is drawn
/// from, and a small document can describe an enormous tree: twenty levels
/// that each draw the next level twice is a million draws. The interpreter
/// would execute every one of them, so refusing past this bound also refuses
/// a render that would not finish. Ordinary pages need tens to hundreds.
const MAX_VETTED_CONTEXTS: usize = 1_000_000;

/// Most objects (arrays, dictionaries, streams) one render will follow
/// while bounding the depth of colour spaces, functions and shadings. Each
/// object is followed once per render, so this is a bound on distinct
/// objects, not on uses.
const MAX_VETTED_OBJECTS: usize = 2_000_000;

/// Deepest array, dictionary and reference nesting accepted in a colour
/// space, function, shading, pattern, graphics state, image dictionary or
/// font (with its descendant fonts, descriptor, widths and encodings).
///
/// The interpreter builds these by recursion: about 2.7 KB of stack per
/// nested stitching function and 1.6 KB per nested colour space in the
/// measurements that found the fault. Real ones are under ten deep, a
/// `/DeviceN` space with process and colorant dictionaries being the
/// deepest common case.
const MAX_OBJECT_DEPTH: usize = 64;

/// Deepest nesting of arrays and dictionaries accepted in a content stream.
///
/// The tokenizer recurses once per level. It overflowed 8 MiB at about
/// 131,000 levels in an optimized build, some 64 bytes a level; at eight
/// times that for an unoptimized build this many levels cost about 2 MiB of
/// the 64 MiB render stack. Real content nests a handful deep.
const MAX_CONTENT_NESTING: usize = 4_096;

/// Deepest nesting of `{ }` procedures accepted in a PostScript calculator
/// function, whose parser recurses once per level at about 400 bytes
/// optimized: under 1 MiB unoptimized at this depth.
const MAX_PROGRAM_NESTING: usize = 256;

/// Most graphics states the interpreter may hold saved at once along one
/// path of nested content streams.
///
/// Every `q` clones the whole current state onto a stack that has no limit:
/// a page of a million `q` reached a peak of 616 MB in a release build
/// against 6 MB for a blank page, about 610 bytes a state, more when clip
/// paths are in force, since each saved state copies the list of them. Real
/// content nests `q` a few levels deep, rarely past a few dozen. This many
/// cost about 2.5 MB without clips. Glyph procedures are bounded per stream
/// only, so a glyph shown at the deepest point can hold as many again.
const MAX_SAVED_STATES: usize = 4_096;

/// Most samples a sampled (Type 0) function may hold: the product of its
/// `/Size` entries times its outputs, and the samples its stream holds.
///
/// The interpreter keeps every sample point in a hash map keyed by its
/// coordinates, about 80 bytes a point with four inputs or fewer, so this
/// many cost under 100 MiB. Real functions are far smaller: a shading or
/// tint transform over one input has 256 to 4,096 points with three or four
/// outputs; a three-input table of 33 points per input with three outputs
/// holds 107,811.
const MAX_SAMPLED_FUNCTION_ENTRIES: u64 = 1 << 20;

/// Most inputs a sampled (Type 0) function may take.
///
/// The interpreter interpolates between the two neighbouring samples on
/// every input, by recursion, so one evaluation reads up to 2^inputs
/// samples, and it evaluates once a pixel for shadings and images. Eight
/// inputs is 256 reads an evaluation; a `/DeviceN` tint transform is the
/// only common many-input function, and eight colorants is already rare.
const MAX_SAMPLED_FUNCTION_INPUTS: usize = 8;

/// Most consecutive comment lines accepted in a font's CMap.
///
/// The interpreter's CMap lexer calls itself once for every comment that
/// follows another with only white space between them; a million such lines
/// overflow the 64 MiB render stack. CMaps in real files open with a header
/// of ten to twenty comment lines. At 2 KB a level, more than an
/// unoptimized frame of that lexer needs, this many levels cost 8 MiB of
/// [`RENDER_STACK_BYTES`] beyond [`STACK_BUDGET`].
const MAX_CMAP_COMMENT_RUN: usize = 4_096;

/// Fewest pixels an image may have whatever the render size: 256 megapixels.
///
/// The interpreter converts an image to 8-bit RGB, three bytes a pixel, in
/// buffers sized from its `/Width` and `/Height`, plus the decoded samples.
/// A 600 dpi scan of an A3 sheet is about 70 megapixels, so real scans sit
/// well under this, even embedded in a page rendered small.
const IMAGE_PIXEL_FLOOR: u64 = 256_000_000;

/// How many times the page's own rendered pixel count an image may have,
/// when that is more than [`IMAGE_PIXEL_FLOOR`]: an image four times the
/// render's resolution on each axis.
const IMAGE_PIXELS_PER_PAGE_PIXEL: u64 = 16;

/// Most pixels one image may declare on a page rendered to `page_pixels`.
pub(crate) fn image_pixel_budget(page_pixels: u64) -> u64 {
    page_pixels
        .saturating_mul(IMAGE_PIXELS_PER_PAGE_PIXEL)
        .max(IMAGE_PIXEL_FLOOR)
}

/// Pixels an image dictionary declares, read as the interpreter reads them:
/// `/W` or `/Width` times `/H` or `/Height`; zero when either is missing.
fn declared_pixels(dict: &Dict<'_>) -> u64 {
    let side = |short: &[u8], long: &[u8]| {
        dict.get::<u32>(short)
            .or_else(|| dict.get::<u32>(long))
            .map_or(0, u64::from)
    };
    side(&b"W"[..], &b"Width"[..]).saturating_mul(side(&b"H"[..], &b"Height"[..]))
}

/// Largest renderer, in pixels, the interpreter can make for one tiling
/// cell, whatever the transform it is drawn with.
///
/// Mirrors the cell sizing in `hayro` 0.5.0's renderer: the cell's scale on
/// each axis is clamped to between 1 and 3,000 pixels across its `/BBox`,
/// and the renderer is `/XStep` and `/YStep` at that scale, cast to `u16`.
/// The transform in force is not known before rendering, so the largest
/// scale is assumed: a cell whose step is its box size counts as 3,000 by
/// 3,000 pixels however small it is drawn, and one whose step is k times
/// its box as 3,000·k a side, up to 65,535.
fn cell_pixels(dict: &Dict<'_>) -> u64 {
    let side = |step: &[u8], low: usize, high: usize| -> u64 {
        let step = f64::from(dict.get::<f32>(step).unwrap_or(0.0)).abs();
        let extent = dict
            .get::<[f32; 4]>(&b"BBox"[..])
            .and_then(|bbox| Some(f64::from(*bbox.get(high)?) - f64::from(*bbox.get(low)?)))
            .map_or(0.0, f64::abs);
        let pixels = (step * 3000.0 / extent).round();
        if pixels.is_nan() {
            0
        } else {
            // A saturating cast, as the interpreter's own.
            pixels.min(f64::from(u16::MAX)) as u64
        }
    };
    side(&b"XStep"[..], 0, 2).saturating_mul(side(&b"YStep"[..], 1, 3))
}

/// A dictionary's keys in byte order.
///
/// `Dict::keys` follows a hash map whose order changes from process to
/// process. Walking in a fixed order makes the walk, and so the reason given
/// for a page that breaks two bounds, the same in every run.
fn sorted_keys<'a>(dict: &Dict<'a>) -> Vec<Name<'a>> {
    let mut keys: Vec<Name<'a>> = dict.keys().collect();
    keys.sort_unstable();
    keys
}

/// What `vet_object` names in a refusal for objects reached from resources.
const RESOURCE_OBJECTS: &str = "a colour space, function or shading";

/// Identity of a stream: where its dictionary sits in the file.
type StreamId = (usize, usize);

/// How the interpreter enters a content stream.
#[derive(Clone, Copy)]
enum Route {
    /// `Do` on a Form XObject, or an annotation appearance.
    Form,
    /// A fill or stroke with a tiling pattern.
    Pattern,
    /// A paint through a soft mask.
    SoftMask,
}

impl Route {
    fn cost(self) -> u32 {
        match self {
            Self::Form => FORM_COST,
            Self::Pattern => PATTERN_COST,
            Self::SoftMask => SOFT_MASK_COST,
        }
    }
}

/// The resource names one content stream uses, by the route they can open.
#[derive(Default)]
struct Scan {
    /// Names already recorded, by route, so a repeat costs one lookup.
    seen: HashSet<(u8, Vec<u8>)>,
    /// Operands of `Do`.
    x_objects: Vec<Vec<u8>>,
    /// Pattern operands of `scn` and `SCN`.
    patterns: Vec<Vec<u8>>,
    /// Operands of `gs`.
    ext_g_states: Vec<Vec<u8>>,
    /// Font operands of `Tf`.
    fonts: Vec<Vec<u8>>,
    /// Most pixels any inline image in this stream declares.
    inline_pixels: u64,
    /// Most graphics states this stream holds saved at once: the deepest
    /// `q` nesting, with a `Q` past the first ignored.
    saves: usize,
}

impl Scan {
    fn of(content: &[u8]) -> Self {
        let mut scan = Self::default();
        let mut depth = 0usize;
        for instruction in TypedIter::new(content) {
            match instruction {
                TypedInstruction::XObject(op) => {
                    remember(&mut scan.seen, 0, &mut scan.x_objects, &op.0);
                }
                TypedInstruction::NonStrokeColorNamed(op) => {
                    if let Some(name) = &op.1 {
                        remember(&mut scan.seen, 1, &mut scan.patterns, name);
                    }
                }
                TypedInstruction::StrokeColorNamed(op) => {
                    if let Some(name) = &op.1 {
                        remember(&mut scan.seen, 1, &mut scan.patterns, name);
                    }
                }
                TypedInstruction::SetGraphicsState(op) => {
                    remember(&mut scan.seen, 2, &mut scan.ext_g_states, &op.0);
                }
                TypedInstruction::TextFont(op) => {
                    remember(&mut scan.seen, 3, &mut scan.fonts, &op.0);
                }
                TypedInstruction::InlineImage(op) => {
                    scan.inline_pixels = scan.inline_pixels.max(declared_pixels(op.0.dict()));
                }
                TypedInstruction::SaveState(_) => {
                    depth += 1;
                    scan.saves = scan.saves.max(depth);
                }
                TypedInstruction::RestoreState(_) => depth = depth.saturating_sub(1),
                _ => {}
            }
        }
        scan.seen = HashSet::new();
        scan
    }

    fn is_empty(&self) -> bool {
        self.x_objects.is_empty()
            && self.patterns.is_empty()
            && self.ext_g_states.is_empty()
            && self.fonts.is_empty()
    }
}

/// Records `name` once, in the escaped form [`Name::new`] reads back.
fn remember(
    seen: &mut HashSet<(u8, Vec<u8>)>,
    route: u8,
    names: &mut Vec<Vec<u8>>,
    name: &Name<'_>,
) {
    // `Name::new` decodes `#xx` and assumes two hex digits follow every `#`,
    // so a literal `#` has to go back in as `#23`.
    let mut escaped = Vec::with_capacity(name.len());
    for &byte in name.iter() {
        if byte == b'#' {
            escaped.extend_from_slice(b"#23");
        } else {
            escaped.push(byte);
        }
    }
    if seen.insert((route, escaped.clone())) {
        names.push(escaped);
    }
}

/// Identity of an array, dictionary or stream whose height may be
/// remembered.
fn object_id(object: &Object<'_>) -> Option<StreamId> {
    let data = match object {
        Object::Array(array) => array.data(),
        Object::Dict(dict) => dict.data(),
        Object::Stream(stream) => stream.dict().data(),
        _ => return None,
    };
    // Only an object written without any indirect reference is followed the
    // same way from everywhere: the object model refuses a reference back
    // into an object the lookup is already inside, so what a reference
    // yields depends on the path. `R` is the reference keyword; a name that
    // merely contains the letter costs a second traversal, nothing else.
    (!data.is_empty() && !data.contains(&b'R')).then_some((data.as_ptr() as usize, data.len()))
}

fn stream_id(stream: &Stream<'_>) -> StreamId {
    let data = stream.dict().data();
    (data.as_ptr() as usize, data.len())
}

fn own_resources<'a>(dict: &Dict<'a>) -> Dict<'a> {
    dict.get::<Dict<'_>>(&b"Resources"[..]).unwrap_or_default()
}

fn has_subtype(dict: &Dict<'_>, subtype: &[u8]) -> bool {
    dict.get::<Name<'_>>(&b"Subtype"[..])
        .is_some_and(|name| &*name == subtype)
}

/// A content stream the walk still has to look inside.
struct Child<'a> {
    route: Route,
    stream: StreamId,
    scan: Rc<Scan>,
    /// The stream's own `/Resources`. The chain above it is only built when
    /// the child is entered, so a stream naming many children holds one
    /// chain, not one per child.
    own: Dict<'a>,
    /// Pixels of every tiling cell renderer held while this stream runs,
    /// its own included.
    cells: u64,
}

/// One content stream in one context, part-way through being vetted.
struct Frame<'a> {
    id: u32,
    route: Route,
    stream: StreamId,
    resources: Resources<'a>,
    /// Least stack already in use when this stream starts.
    floor: u32,
    children: Vec<Child<'a>>,
    /// Deepest run of nested forms beneath this stream, in bytes.
    form_chain: u32,
    /// Most stack needed beneath this stream through its forms.
    through_forms: u32,
    /// Most stack a pattern or soft mask set here needs once entered.
    target: Option<u32>,
    /// Stack the deepest glyph procedure of a Type 3 font selected here
    /// needs, when one is selected.
    glyphs: Option<u32>,
    /// Pixels of every tiling cell renderer held while this stream runs.
    cells: u64,
    /// Graphics states the interpreter may hold saved inside this stream:
    /// its own deepest nesting on top of every stream's above it.
    saves: usize,
}

impl Frame<'_> {
    /// Most stack needed beneath this stream.
    fn height(&self) -> u32 {
        // A pattern, soft mask or font set here stays in force inside every
        // form drawn afterwards, so it is charged at the bottom of the
        // deepest run of forms, under a glyph procedure that may be the
        // thing painting with it.
        let mut height = self.through_forms;
        let glyphs = self.glyphs.unwrap_or(GLYPH_COST);
        if let Some(target) = self.target {
            height = height.max(
                self.form_chain
                    .saturating_add(glyphs)
                    .saturating_add(target),
            );
        }
        if self.glyphs.is_some() {
            height = height.max(self.form_chain.saturating_add(glyphs));
        }
        height
    }
}

/// State of one page's vetting.
struct Walk<'a> {
    scans: HashMap<StreamId, Option<Rc<Scan>>>,
    /// Type 3 fonts already checked, by the context whose resources name them.
    fonts: HashMap<(u32, u32, Vec<u8>), u32>,
    stack: Vec<Frame<'a>>,
    visited: usize,
    objects: usize,
    /// Height of every object already followed by `vet_object`.
    heights: HashMap<StreamId, usize>,
    /// The page's own resources, the chain every stream ends in.
    root: Resources<'a>,
    /// Decoded bytes scanned so far, and the most this render may scan.
    decoded: usize,
    decode_allowance: usize,
    /// Most pixels one image may declare, from [`image_pixel_budget`].
    image_pixels: u64,
    next_id: u32,
}

fn refused(reason: &str) -> anyhow::Error {
    anyhow!("interpreted render refused: {reason}")
}

impl<'a> Walk<'a> {
    fn count(&mut self) -> Result<()> {
        self.visited += 1;
        if self.visited > MAX_VETTED_CONTEXTS {
            return Err(refused(
                "the page draws more nested content than can be checked",
            ));
        }
        Ok(())
    }

    /// The names a stream uses, or `None` when the interpreter cannot decode
    /// it and so will not enter it.
    fn scan(&mut self, stream: &Stream<'a>) -> Result<Option<Rc<Scan>>> {
        // What a stream decodes to can depend on how it was reached: the
        // object model will not follow a reference back into an object the
        // lookup is already inside, so an indirect `/DecodeParms` or
        // `/Filter` can be present on one path and missing on another. Only
        // a stream whose filters are written out in its own dictionary is
        // scanned once and remembered; any other is decoded on every path.
        let key = decode_is_fixed(stream).then(|| stream_id(stream));
        if let Some(known) = key.and_then(|key| self.scans.get(&key)) {
            return Ok(known.clone());
        }
        let scan = match stream.decoded() {
            Ok(content) => Some(Rc::new(self.scan_content(&content)?)),
            Err(_) => None,
        };
        if let Some(key) = key {
            self.scans.insert(key, scan.clone());
        }
        Ok(scan)
    }

    /// Scans decoded content after counting its size against this render's
    /// allowance.
    fn scan_content(&mut self, content: &[u8]) -> Result<Scan> {
        self.spend(content.len())?;
        check_nesting(content, MAX_CONTENT_NESTING)?;
        let scan = Scan::of(content);
        check_saves(scan.saves)?;
        self.check_image_pixels(scan.inline_pixels)?;
        Ok(scan)
    }

    /// Bounds the depth of every object the interpreter may build from one
    /// level of resources: colour spaces, shadings, graphics states,
    /// patterns and the dictionaries of XObjects.
    fn vet_resource_objects(&mut self, resources: &Resources<'a>) -> Result<()> {
        for dict in [
            &resources.color_spaces,
            &resources.shadings,
            &resources.ext_g_states,
            &resources.patterns,
            &resources.x_objects,
        ] {
            self.vet_object(Object::Dict(dict.clone()), RESOURCE_OBJECTS)?;
        }
        Ok(())
    }

    /// Refuses an object graph nested deeper than [`MAX_OBJECT_DEPTH`].
    ///
    /// The interpreter builds functions and colour spaces by plain
    /// recursion over arrays, dictionaries and references. This follows the
    /// same graph with a stack of its own and remembers the height of every
    /// object that holds no indirect reference (see `object_id`), so the
    /// self-contained parts of shared resources are followed once per
    /// render. An object that does hold a reference is followed again each
    /// time it is reached. It does not follow `/Resources`, `/Parent`
    /// or a soft mask's `/G`, which lead to content streams and are bounded
    /// by the walk over those, nor `/UseCMap`, which the interpreter ignores.
    /// `what` names the kind of object in the refusal.
    fn vet_object(&mut self, root: Object<'a>, what: &str) -> Result<()> {
        let too_deep = || refused(&format!("{what} is nested deeper than can be built"));
        // Each entry: the object's identity, the children still to visit,
        // and the tallest child seen so far.
        let mut path: Vec<(Option<StreamId>, Vec<Object<'a>>, usize)> = Vec::new();
        let mut next = Some(root);
        loop {
            if let Some(object) = next.take() {
                let key = object_id(&object);
                if let Some(height) = key.and_then(|key| self.heights.get(&key)).copied() {
                    if path.len() + height > MAX_OBJECT_DEPTH {
                        return Err(too_deep());
                    }
                    if let Some((_, _, tallest)) = path.last_mut() {
                        *tallest = (*tallest).max(height);
                    }
                } else {
                    self.count_object()?;
                    let children = self.children_of(object)?;
                    path.push((key, children, 0));
                    if path.len() > MAX_OBJECT_DEPTH {
                        return Err(too_deep());
                    }
                }
            }
            let Some((key, children, tallest)) = path.last_mut() else {
                return Ok(());
            };
            if let Some(child) = children.pop() {
                next = Some(child);
                continue;
            }
            let height = *tallest + 1;
            if let Some(key) = *key {
                self.heights.insert(key, height);
            }
            path.pop();
            if let Some((_, _, tallest)) = path.last_mut() {
                *tallest = (*tallest).max(height);
            }
        }
    }

    /// The arrays, dictionaries and streams directly inside `object`.
    fn children_of(&mut self, object: Object<'a>) -> Result<Vec<Object<'a>>> {
        let nested = |object: &Object<'a>| {
            matches!(
                object,
                Object::Array(_) | Object::Dict(_) | Object::Stream(_)
            )
        };
        let dict = match object {
            Object::Array(array) => return Ok(array.iter::<Object<'a>>().filter(nested).collect()),
            Object::Dict(dict) => dict,
            Object::Stream(stream) => {
                // An image XObject, or the soft mask of one.
                if has_subtype(stream.dict(), b"Image") {
                    self.check_image_pixels(declared_pixels(stream.dict()))?;
                }
                match stream.dict().get::<u8>(&b"FunctionType"[..]) {
                    Some(4) => {
                        // The calculator parser recurses once per `{`.
                        if let Ok(program) = stream.decoded() {
                            self.spend(program.len())?;
                            check_nesting(&program, MAX_PROGRAM_NESTING)?;
                        }
                    }
                    Some(0) => self.vet_sampled_function(&stream)?,
                    _ => {}
                }
                stream.dict().clone()
            }
            _ => return Ok(Vec::new()),
        };
        Ok(sorted_keys(&dict)
            .into_iter()
            .filter(|key| !matches!(&**key, b"Resources" | b"Parent" | b"G" | b"UseCMap"))
            .filter_map(|key| dict.get::<Object<'a>>(key))
            .filter(nested)
            .collect())
    }

    /// Refuses a sampled (Type 0) function that would cost the interpreter
    /// more than [`MAX_SAMPLED_FUNCTION_ENTRIES`] samples or more than
    /// [`MAX_SAMPLED_FUNCTION_INPUTS`] inputs, or whose `/Size` overflows or
    /// holds a zero.
    ///
    /// Mirrors `Type0::new`: it builds nothing without a valid
    /// `/BitsPerSample`, a `/Size` or a non-empty `/Range`, so neither is
    /// checked here. It reads every sample in the stream before cutting the
    /// list to `/Size`, so the samples the stream holds are counted too.
    fn vet_sampled_function(&mut self, stream: &Stream<'a>) -> Result<()> {
        let dict = stream.dict();
        let Some(bits) = dict.get::<u8>(&b"BitsPerSample"[..]) else {
            return Ok(());
        };
        if !matches!(bits, 1 | 2 | 4 | 8 | 16 | 24 | 32) {
            return Ok(());
        }
        let outputs = dict
            .get::<Array<'_>>(&b"Range"[..])
            .map_or(0, |range| range.iter::<f32>().count() / 2);
        let Some(sizes) = dict.get::<Array<'_>>(&b"Size"[..]) else {
            return Ok(());
        };
        if outputs == 0 {
            return Ok(());
        }
        let over_budget =
            || refused("a sampled function holds more samples than the interpreter may build");
        let mut inputs = 0usize;
        let mut entries = u64::try_from(outputs).map_err(|_| over_budget())?;
        for size in sizes.iter::<u32>() {
            inputs += 1;
            if size == 0 {
                return Err(refused("a sampled function has a /Size entry of zero"));
            }
            entries = entries
                .checked_mul(u64::from(size))
                .ok_or_else(|| refused("a sampled function's /Size overflows"))?;
        }
        if inputs > MAX_SAMPLED_FUNCTION_INPUTS {
            return Err(refused(
                "a sampled function has more inputs than the interpreter can evaluate in time",
            ));
        }
        if entries > MAX_SAMPLED_FUNCTION_ENTRIES {
            return Err(over_budget());
        }
        if let Ok(data) = stream.decoded() {
            self.spend(data.len())?;
            let held = u64::try_from(data.len())
                .unwrap_or(u64::MAX)
                .saturating_mul(8)
                / u64::from(bits);
            if held > MAX_SAMPLED_FUNCTION_ENTRIES {
                return Err(over_budget());
            }
        }
        Ok(())
    }

    /// Refuses an image that declares more than [`image_pixel_budget`]
    /// pixels for this render.
    fn check_image_pixels(&self, pixels: u64) -> Result<()> {
        if pixels > self.image_pixels {
            return Err(refused(
                "an image declares more pixels than this render may decode",
            ));
        }
        Ok(())
    }

    /// Refuses tiling cell renderers, held at once along one path, that
    /// together have more than [`image_pixel_budget`] pixels.
    fn check_cell_pixels(&self, pixels: u64) -> Result<()> {
        if pixels > self.image_pixels {
            return Err(refused(
                "a tiling pattern's cells need more pixels than this render may hold",
            ));
        }
        Ok(())
    }

    fn count_object(&mut self) -> Result<()> {
        self.objects += 1;
        if self.objects > MAX_VETTED_OBJECTS {
            return Err(refused(
                "the page's resources hold more nested objects than can be checked",
            ));
        }
        Ok(())
    }

    /// Counts `bytes` of decoded data against the allowance this check was
    /// given: the caller's configured decode limit (see [`vet_page`]).
    fn spend(&mut self, bytes: usize) -> Result<()> {
        self.decoded = self.decoded.saturating_add(bytes);
        if self.decoded > self.decode_allowance {
            return Err(refused(&format!(
                "checking the page would decode more than the configured decode limit of {} \
                 bytes (stopped after decoding {} bytes)",
                self.decode_allowance, self.decoded
            )));
        }
        Ok(())
    }

    fn push(
        &mut self,
        child: Child<'a>,
        resources: Resources<'a>,
        floor: u32,
        saves_above: usize,
    ) -> Result<()> {
        if floor > STACK_BUDGET {
            return Err(refused(
                "content streams nest deeper than the interpreter's stack allows",
            ));
        }
        if self.stack.iter().any(|frame| frame.stream == child.stream) {
            return Err(refused("a content stream draws itself"));
        }
        let id = self.next_id;
        self.next_id = self.next_id.saturating_add(1);
        let mut frame = Frame {
            id,
            route: child.route,
            stream: child.stream,
            resources,
            floor,
            children: Vec::new(),
            form_chain: 0,
            through_forms: 0,
            target: None,
            glyphs: None,
            saves: saves_above.saturating_add(child.scan.saves),
            cells: child.cells,
        };
        check_saves(frame.saves)?;
        self.vet_resource_objects(&frame.resources)?;
        self.expand(&mut frame, &child.scan)?;
        self.stack.push(frame);
        Ok(())
    }

    /// Resolves every name `scan` uses the way the interpreter will inside
    /// `frame`, queueing the content streams that turn up.
    fn expand(&mut self, frame: &mut Frame<'a>, scan: &Scan) -> Result<()> {
        for name in &scan.x_objects {
            self.count()?;
            // Mirrors `XObject::new`: only `/Subtype /Form` is interpreted.
            let Some(stream) = frame.resources.get_x_object(Name::new(name)) else {
                continue;
            };
            if has_subtype(stream.dict(), b"Form") {
                self.queue(frame, Route::Form, &stream)?;
            }
        }
        for name in &scan.patterns {
            self.count()?;
            // Mirrors `Pattern::new`: a stream is a tiling pattern.
            let Some(stream) = frame
                .resources
                .get_pattern(Name::new(name))
                .and_then(Object::into_stream)
            else {
                continue;
            };
            self.queue(frame, Route::Pattern, &stream)?;
        }
        for name in &scan.ext_g_states {
            self.count()?;
            // Mirrors `handle_gs_single` and `SoftMask::new`.
            let Some(stream) = frame
                .resources
                .get_ext_g_state(Name::new(name))
                .and_then(|state| state.get::<Dict<'_>>(&b"SMask"[..]))
                .and_then(|mask| mask.get::<Stream<'_>>(&b"G"[..]))
            else {
                continue;
            };
            self.queue(frame, Route::SoftMask, &stream)?;
        }
        for name in &scan.fonts {
            self.count()?;
            self.vet_font(frame, name)?;
        }
        Ok(())
    }

    /// Queues `stream` as a child of `frame`, or settles it on the spot when
    /// it uses no resources and so cannot reach anything.
    fn queue(&mut self, frame: &mut Frame<'a>, route: Route, stream: &Stream<'a>) -> Result<()> {
        // The stream's dictionary carries objects of its own, such as a
        // transparency group's colour space.
        self.vet_object(Object::Dict(stream.dict().clone()), RESOURCE_OBJECTS)?;
        // A tiling cell renders into a renderer of its own, held while the
        // cell's content runs, beneath every cell it is nested in.
        let cells = match route {
            Route::Pattern => frame.cells.saturating_add(cell_pixels(stream.dict())),
            Route::Form | Route::SoftMask => frame.cells,
        };
        self.check_cell_pixels(cells)?;
        let Some(scan) = self.scan(stream)? else {
            return Ok(());
        };
        // Its saved states sit on top of those of every stream above it.
        check_saves(frame.saves.saturating_add(scan.saves))?;
        if scan.is_empty() {
            // This walk never enters it, so its own resources are checked
            // here: the interpreter still builds whatever they hold if the
            // stream's operators use a colour space or shading by name.
            let own = Resources::from_parent(own_resources(stream.dict()), self.root.clone());
            self.vet_resource_objects(&own)?;
            settle(frame, route, 0, 0);
            return Ok(());
        }
        frame.children.push(Child {
            route,
            stream: stream_id(stream),
            scan,
            own: own_resources(stream.dict()),
            cells,
        });
        Ok(())
    }

    /// Checks the font `name` selects inside `frame`, if it is a Type 3 font.
    ///
    /// A glyph procedure runs with the resources of whichever stream shows
    /// the text, which may be any form drawn after the font was selected, so
    /// what its names mean there cannot be known from here. It is accepted
    /// only when it cannot enter another content stream at all: every name
    /// it uses must resolve, inside the procedure's or the font's own
    /// resources, to something that is not a content stream.
    fn vet_font(&mut self, frame: &mut Frame<'a>, name: &[u8]) -> Result<()> {
        // Mirrors `Resources::get_font`, keeping track of whose resources
        // answered, so a font inherited by many forms is checked once.
        let mut level = Some(&frame.resources);
        let mut above = 0usize;
        let font = loop {
            let Some(resources) = level else {
                return Ok(());
            };
            if let Some(font) = resources.fonts.get::<Dict<'a>>(Name::new(name)) {
                break font;
            }
            level = resources.parent();
            above += 1;
        };
        let type3 = has_subtype(&font, b"Type3");
        // The stack holds this frame's ancestors; `above` levels up is one of
        // them, or one of the page tree's own resource levels past the root.
        let ancestors = self.stack.len();
        let owner = match ancestors.checked_sub(above) {
            Some(index) if above > 0 => self.stack.get(index).map_or(0, |owner| owner.id),
            _ if above == 0 => frame.id,
            _ => 0,
        };
        let beyond_root = u32::try_from(above.saturating_sub(ancestors)).unwrap_or(u32::MAX);
        let memo = (owner, beyond_root, name.to_vec());
        if let Some(known) = self.fonts.get(&memo) {
            if type3 {
                frame.glyphs = Some(frame.glyphs.unwrap_or(0).max(*known));
            }
            return Ok(());
        }
        self.vet_font_dict(&font)?;
        if !type3 {
            self.fonts.insert(memo, 0);
            return Ok(());
        }
        frame.glyphs = Some(frame.glyphs.unwrap_or(GLYPH_COST));

        let font_resources = own_resources(&font);
        let procedures = font.get::<Dict<'_>>(&b"CharProcs"[..]).unwrap_or_default();
        let mut cost = GLYPH_COST;
        for glyph in sorted_keys(&procedures) {
            self.count()?;
            let Some(procedure) = procedures.get::<Stream<'a>>(glyph) else {
                continue;
            };
            // Mirrors `Type3::render_glyph`: the procedure's own resources
            // come first, then the font's.
            let local = vec![own_resources(procedure.dict()), font_resources.clone()];
            cost = cost.max(GLYPH_COST.saturating_add(self.vet_glyph(&procedure, local)?));
        }
        self.fonts.insert(memo, cost);
        frame.glyphs = Some(frame.glyphs.unwrap_or(0).max(cost));
        Ok(())
    }

    /// Checks what the interpreter builds from a font dictionary as soon as
    /// the font is selected: the objects it reaches, to [`MAX_OBJECT_DEPTH`],
    /// and the CMaps of its `/Encoding` and `/ToUnicode`.
    ///
    /// `/UseCMap` and the `usecmap` operator are not followed: the
    /// interpreter ignores both.
    fn vet_font_dict(&mut self, font: &Dict<'a>) -> Result<()> {
        // Descendant fonts, the descriptor, widths, encodings and glyph
        // procedure dictionaries are held to the same depth as resources.
        self.vet_object(Object::Dict(font.clone()), "a font")?;
        for key in [&b"Encoding"[..], b"ToUnicode"] {
            let Some(cmap) = font.get::<Stream<'a>>(key) else {
                continue;
            };
            if let Ok(text) = cmap.decoded() {
                self.spend(text.len())?;
                check_cmap_comments(&text)?;
            }
        }
        Ok(())
    }

    /// Walks one glyph procedure and every content stream it can enter,
    /// using only the resources that travel with the font. Returns the stack
    /// those streams may need beneath the procedure.
    ///
    /// A name that none of those resources define would be looked up in the
    /// stream showing the text, which is not known here, so it is refused.
    /// A form, tiling pattern, soft-mask group or nested Type 3 font that
    /// they do define is entered. The cost returned is the sum over every
    /// stream entered, not the deepest path: a pattern or mask set in one
    /// stream stays in force in the forms drawn after it, and the sum
    /// covers every order they can stack in without modelling it.
    fn vet_glyph(&mut self, procedure: &Stream<'a>, local: Vec<Dict<'a>>) -> Result<u32> {
        let outside = || refused("a Type 3 glyph procedure uses resources from outside its font");
        let find = |chain: &[Dict<'a>], category: &[u8], name: &[u8]| -> Option<Object<'a>> {
            chain.iter().find_map(|resources| {
                resources
                    .get::<Dict<'_>>(category)
                    .and_then(|dict| dict.get::<Object<'a>>(Name::new(name)))
            })
        };
        let mut total = 0u32;
        // Streams still to look inside, with their resources and the
        // streams above them.
        let mut pending = vec![(procedure.clone(), local, vec![stream_id(procedure)])];
        while let Some((stream, chain, path)) = pending.pop() {
            self.count()?;
            for resources in &chain {
                for category in [
                    &b"ColorSpace"[..],
                    b"Shading",
                    b"ExtGState",
                    b"Pattern",
                    b"XObject",
                ] {
                    if let Some(dict) = resources.get::<Dict<'a>>(category) {
                        self.vet_object(Object::Dict(dict), RESOURCE_OBJECTS)?;
                    }
                }
            }
            let Some(scan) = self.scan(&stream)? else {
                continue;
            };
            // Each target found: the stream, the resources placed in front
            // of this stream's chain, and what entering it costs.
            let mut targets: Vec<(Stream<'a>, Vec<Dict<'a>>, u32)> = Vec::new();
            for name in &scan.x_objects {
                let target = find(&chain, b"XObject", name)
                    .and_then(Object::into_stream)
                    .ok_or_else(outside)?;
                if has_subtype(target.dict(), b"Form") {
                    let own = vec![own_resources(target.dict())];
                    targets.push((target, own, FORM_COST));
                }
            }
            for name in &scan.patterns {
                let pattern = find(&chain, b"Pattern", name).ok_or_else(outside)?;
                if let Some(cell) = pattern.into_stream() {
                    self.check_cell_pixels(cell_pixels(cell.dict()))?;
                    let own = vec![own_resources(cell.dict())];
                    targets.push((cell, own, PATTERN_COST));
                }
            }
            for name in &scan.ext_g_states {
                let state = find(&chain, b"ExtGState", name)
                    .and_then(Object::into_dict)
                    .ok_or_else(outside)?;
                if let Some(group) = state
                    .get::<Dict<'_>>(&b"SMask"[..])
                    .and_then(|mask| mask.get::<Stream<'a>>(&b"G"[..]))
                {
                    let own = vec![own_resources(group.dict())];
                    targets.push((group, own, SOFT_MASK_COST));
                }
            }
            for name in &scan.fonts {
                let font = find(&chain, b"Font", name)
                    .and_then(Object::into_dict)
                    .ok_or_else(outside)?;
                self.vet_font_dict(&font)?;
                if !has_subtype(&font, b"Type3") {
                    continue;
                }
                let font_resources = own_resources(&font);
                let procedures = font.get::<Dict<'_>>(&b"CharProcs"[..]).unwrap_or_default();
                for glyph in sorted_keys(&procedures) {
                    self.count()?;
                    if let Some(inner) = procedures.get::<Stream<'a>>(glyph) {
                        let own = vec![own_resources(inner.dict()), font_resources.clone()];
                        targets.push((inner, own, GLYPH_COST));
                    }
                }
            }
            for (target, mut own, cost) in targets {
                let id = stream_id(&target);
                if path.contains(&id) {
                    return Err(refused("a content stream draws itself"));
                }
                total = total.saturating_add(cost);
                if total > STACK_BUDGET {
                    return Err(refused(
                        "content streams nest deeper than the interpreter's stack allows",
                    ));
                }
                own.extend(chain.iter().cloned());
                let mut inner_path = path.clone();
                inner_path.push(id);
                pending.push((target, own, inner_path));
            }
        }
        Ok(total)
    }

    /// Vets `root` and everything beneath it; `floor` is the stack already in
    /// use when `root` starts. Returns the stack needed beneath `root`.
    fn run(&mut self, root: Child<'a>, resources: Resources<'a>, floor: u32) -> Result<u32> {
        self.push(root, resources, floor, 0)?;
        loop {
            let Some(frame) = self.stack.last_mut() else {
                return Ok(0);
            };
            if let Some(child) = frame.children.pop() {
                let floor = frame.floor.saturating_add(child.route.cost());
                let saves = frame.saves;
                let resources = Resources::from_parent(child.own.clone(), frame.resources.clone());
                self.count()?;
                self.push(child, resources, floor, saves)?;
                continue;
            }
            let Some(done) = self.stack.pop() else {
                return Ok(0);
            };
            let height = done.height();
            if done.floor.saturating_add(height) > STACK_BUDGET {
                return Err(refused(
                    "content streams nest deeper than the interpreter's stack allows",
                ));
            }
            match self.stack.last_mut() {
                Some(parent) => settle(parent, done.route, done.form_chain, height),
                None => return Ok(height),
            }
        }
    }
}

/// Folds a finished child, entered by `route`, into its parent.
fn settle(parent: &mut Frame<'_>, route: Route, form_chain: u32, height: u32) {
    let cost = route.cost();
    match route {
        Route::Form => {
            parent.form_chain = parent.form_chain.max(cost.saturating_add(form_chain));
            parent.through_forms = parent.through_forms.max(cost.saturating_add(height));
        }
        Route::Pattern | Route::SoftMask => {
            let needed = cost.saturating_add(height);
            parent.target = Some(parent.target.map_or(needed, |known| known.max(needed)));
        }
    }
}

/// Refuses content whose arrays, dictionaries or procedures nest deeper
/// than `limit`.
///
/// The tokenizer recurses once per nested `[`, `<<` or `{`. This is a
/// linear scan with no recursion that tracks the same nesting. Brackets
/// inside literal strings (with their escapes and balanced parentheses),
/// hex strings, comments and inline image data do not count.
///
/// Inline image data is skipped from `ID` to the next `EI` that stands as a
/// token of its own. If the interpreter ends an image somewhere else the
/// two readings of what follows can differ; that is the one place this
/// scan is an approximation of the tokenizer and not a mirror of it.
fn check_nesting(content: &[u8], limit: usize) -> Result<()> {
    let blank = |byte: u8| matches!(byte, b' ' | b'\t' | b'\r' | b'\n' | 0x0c | 0);
    let mut depth = 0usize;
    let mut index = 0usize;
    let mut in_image_dict = false;
    while let Some(&byte) = content.get(index) {
        index += 1;
        match byte {
            b'%' => {
                while content
                    .get(index)
                    .is_some_and(|&b| b != b'\n' && b != b'\r')
                {
                    index += 1;
                }
            }
            b'(' => {
                let mut open = 1usize;
                while let Some(&inner) = content.get(index) {
                    index += 1;
                    match inner {
                        b'\\' => index += 1,
                        b'(' => open += 1,
                        b')' => {
                            open -= 1;
                            if open == 0 {
                                break;
                            }
                        }
                        _ => {}
                    }
                }
            }
            b'<' => {
                if content.get(index) == Some(&b'<') {
                    index += 1;
                    depth += 1;
                } else {
                    while content.get(index).is_some_and(|&b| b != b'>') {
                        index += 1;
                    }
                    index += 1;
                }
            }
            b'[' | b'{' => depth += 1,
            b']' | b'}' => depth = depth.saturating_sub(1),
            b'>' => {
                if content.get(index) == Some(&b'>') {
                    index += 1;
                    depth = depth.saturating_sub(1);
                }
            }
            b'B' if content.get(index) == Some(&b'I')
                && content
                    .get(index + 1)
                    .is_none_or(|&b| blank(b) || b == b'/') =>
            {
                index += 1;
                in_image_dict = true;
            }
            b'I' if in_image_dict
                && content.get(index) == Some(&b'D')
                && index >= 2
                && content.get(index - 2).is_some_and(|&b| blank(b))
                && content.get(index + 1).is_none_or(|&b| blank(b)) =>
            {
                in_image_dict = false;
                index += 2;
                // Skip the data: up to `EI` standing alone.
                while index < content.len() {
                    if content.get(index) == Some(&b'E')
                        && content.get(index + 1) == Some(&b'I')
                        && content
                            .get(index.wrapping_sub(1))
                            .is_some_and(|&b| blank(b))
                        && content.get(index + 2).is_none_or(|&b| blank(b))
                    {
                        index += 2;
                        break;
                    }
                    index += 1;
                }
            }
            _ => {}
        }
        if depth > limit {
            return Err(refused(
                "a stream nests arrays or dictionaries deeper than the tokenizer can safely follow",
            ));
        }
    }
    Ok(())
}

/// Refuses a CMap with more consecutive comment lines than
/// [`MAX_CMAP_COMMENT_RUN`].
///
/// The interpreter parses a CMap only when it is all ASCII, and its lexer
/// calls itself once for each comment that follows another with only white
/// space (as `char::is_whitespace` sees it) between them. This counts the
/// same runs with no recursion. A `%` inside a string is counted as a
/// comment too, which can only make a run look longer.
fn check_cmap_comments(text: &[u8]) -> Result<()> {
    if !text.is_ascii() {
        return Ok(());
    }
    let mut run = 0usize;
    let mut index = 0usize;
    while let Some(&byte) = text.get(index) {
        index += 1;
        match byte {
            b' ' | b'\t' | b'\n' | 0x0b | 0x0c | b'\r' => {}
            b'%' => {
                run += 1;
                if run > MAX_CMAP_COMMENT_RUN {
                    return Err(refused(
                        "a font's CMap holds more consecutive comment lines than its parser can safely follow",
                    ));
                }
                while text.get(index).is_some_and(|&b| b != b'\n' && b != b'\r') {
                    index += 1;
                }
            }
            _ => run = 0,
        }
    }
    Ok(())
}

/// Refuses more saved graphics states than [`MAX_SAVED_STATES`].
fn check_saves(saves: usize) -> Result<()> {
    if saves > MAX_SAVED_STATES {
        return Err(refused(
            "the page saves more graphics states at once than the interpreter may hold",
        ));
    }
    Ok(())
}

/// Whether a stream decodes to the same bytes however it was reached.
fn decode_is_fixed(stream: &Stream<'_>) -> bool {
    if !matches!(stream.raw_data(), std::borrow::Cow::Borrowed(_)) {
        return false;
    }
    let dict = stream.dict();
    [&b"Filter"[..], b"F", b"DecodeParms", b"DP", b"Length"]
        .into_iter()
        .all(|key| match dict.get_raw::<Object<'_>>(key) {
            None => !dict.contains_key(key),
            Some(MaybeRef::Ref(_)) => false,
            Some(MaybeRef::NotRef(Object::Array(array))) => !array.data().contains(&b'R'),
            Some(MaybeRef::NotRef(Object::Dict(dict))) => !dict.data().contains(&b'R'),
            Some(MaybeRef::NotRef(_)) => true,
        })
}

/// Refuses `page` unless the interpreter can render it within
/// [`STACK_BUDGET`].
///
/// `page` must be the very object handed to the interpreter afterwards, and
/// this must run on the thread that will render it. `decode_allowance` is
/// how many decoded bytes the check may look at: the caller's configured
/// decode limit, not what is left of it, so the check never competes with
/// the render for the budget and a caller who lowers the limit lowers the
/// check's work too. Nothing is taken from the document's budget.
/// `image_pixels` is the most pixels one image may declare, from
/// [`image_pixel_budget`].
///
/// # Errors
///
/// Returns an error when a content stream reachable from the page reaches
/// itself, when the streams nest deeper than the budget allows, when a Type 3
/// glyph procedure could enter another content stream, or when checking
/// would take more contexts or decoded bytes than its bounds allow.
pub(crate) fn vet_page(page: &Page<'_>, decode_allowance: usize, image_pixels: u64) -> Result<()> {
    let mut walk = Walk {
        scans: HashMap::new(),
        fonts: HashMap::new(),
        stack: Vec::new(),
        visited: 0,
        objects: 0,
        heights: HashMap::new(),
        root: page.resources().clone(),
        decoded: 0,
        decode_allowance,
        image_pixels,
        next_id: 1,
    };

    // Every level of resources the page inherits from the page tree. The
    // page's own level is checked again when its stream is entered; the
    // levels above it only here.
    let mut level = Some(page.resources());
    while let Some(resources) = level {
        walk.count()?;
        walk.vet_resource_objects(resources)?;
        level = resources.parent();
    }

    // The page's own stream. The interpreter decodes it through the same
    // cache, so scanning it here costs no second decode.
    let page_scan = Rc::new(walk.scan_content(page.page_stream().unwrap_or_default())?);
    let page_id = {
        let data = page.raw().data();
        (data.as_ptr() as usize, data.len())
    };
    if !page_scan.is_empty() {
        walk.run(
            Child {
                route: Route::Form,
                stream: page_id,
                scan: page_scan,
                own: Dict::empty(),
                cells: 0,
            },
            page.resources().clone(),
            PAGE_COST,
        )?;
    }

    // Mirrors `interpret_page`: each visible annotation's normal appearance
    // is drawn as a form under the page's resources, after the page content
    // and outside its frame.
    let Some(annotations) = page.raw().get::<Array<'_>>(&b"Annots"[..]) else {
        return Ok(());
    };
    for annotation in annotations.iter::<Dict<'_>>() {
        walk.count()?;
        if annotation.get::<u32>(&b"F"[..]).unwrap_or(0) & 2 != 0 {
            continue;
        }
        let Some(appearance) = annotation
            .get::<Dict<'_>>(&b"AP"[..])
            .and_then(|appearances| appearances.get::<Stream<'_>>(&b"N"[..]))
        else {
            continue;
        };
        walk.vet_object(Object::Dict(appearance.dict().clone()), RESOURCE_OBJECTS)?;
        let own =
            Resources::from_parent(own_resources(appearance.dict()), page.resources().clone());
        let Some(scan) = walk.scan(&appearance)? else {
            continue;
        };
        if scan.is_empty() {
            walk.vet_resource_objects(&own)?;
            continue;
        }
        walk.run(
            Child {
                route: Route::Form,
                stream: stream_id(&appearance),
                scan,
                own: Dict::empty(),
                cells: 0,
            },
            own,
            PAGE_COST.saturating_add(FORM_COST),
        )?;
    }
    Ok(())
}
