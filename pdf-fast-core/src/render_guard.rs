//! Decides whether the full interpreter can render a page without running
//! out of stack.
//!
//! The interpreter (`hayro`) enters another content stream in five places and
//! guards none of them: a Form XObject drawn with `Do`, an annotation's normal
//! appearance, the cell of a tiling pattern, a Type 3 glyph procedure, and the
//! group of a soft mask. A stream that reaches itself recurses until the stack
//! overflows, and so does a long enough chain of distinct streams. Either one
//! aborts the process, so [`vet_page`] walks everything a page can reach
//! before the interpreter is allowed near it.
//!
//! The walk uses the interpreter's own object model: the same page object,
//! the same `Resources` chain with its fallback to the enclosing stream's
//! resources, the same content tokenizer. Two details of that model decide
//! what recurses, and a second parser would get both wrong: a name missing
//! from a stream's own resources is looked up in every enclosing stream's,
//! and a reference back into an object the lookup is already inside comes
//! back empty.
//!
//! What a stream reaches therefore depends on the streams around it, so the
//! walk follows contexts, not streams: one visit per distinct name a stream
//! uses, however many times it uses it. Each stream is decoded and tokenized
//! once per render. A stream that reappears beneath itself is refused, and so
//! is anything that would need more stack than [`STACK_BUDGET`].

use anyhow::{anyhow, Result};
use hayro::hayro_syntax::content::ops::TypedInstruction;
use hayro::hayro_syntax::content::TypedIter;
use hayro::hayro_syntax::object::{Array, Dict, Name, Object, Stream};
use hayro::hayro_syntax::page::{Page, Resources};
use std::collections::HashMap;
use std::rc::Rc;

/// Stack the interpreter may need for one page, in bytes.
///
/// Sized so that a render refused here would not have fit on a 512 KiB thread
/// stack, the smallest this crate is expected to render on, with a quarter
/// left for the caller's own frames and for whatever the rasterizer does at
/// the bottom of the deepest stream.
///
/// The costs below were measured with `probe_interpreter_stack` in
/// `tests/render_guard.rs`, which renders a chain of nested streams on a
/// thread of a chosen stack size with no guard in front of it. The deepest
/// chain that survives at 512 KiB and at 2 MiB gives the bytes per level.
/// Measured against hayro 0.5.0 on aarch64 macOS (deepest surviving chain at
/// 512 KiB / 2 MiB, then bytes per level):
///
/// | route        | debug               | release             |
/// |--------------|---------------------|---------------------|
/// | form `Do`    | 21 / 106, 18,504    | 127 / 504, 4,172    |
/// | annotation   | 22 / 107, 18,504    | same code as a form |
/// | tiling cell  | 14 / 72, 27,118     | 73 / 292, 7,182     |
/// | soft mask    | 17 / 84, 23,476     | 70 / 280, 7,490     |
/// | Type 3 glyph | 13 / 67, 29,127     | 82 / 327, 6,420     |
///
/// What is left of 512 KiB after the levels is the fixed cost: about 146,000
/// bytes in a debug build and too little to measure in a release build.
/// Each constant below is its figure rounded up, chosen by build profile
/// because an unoptimized interpreter uses over four times the stack. With
/// these numbers a debug build accepts 12 nested forms (21 survive at
/// 512 KiB) and a release build 87 (127 survive).
///
/// Raising the budget or lowering a cost without re-measuring trades a
/// refused render for an aborted process. So does upgrading the interpreter.
const STACK_BUDGET: u32 = 384 * 1024;

/// Stack held before the page's own content stream starts.
const PAGE_COST: u32 = if cfg!(debug_assertions) {
    150_000
} else {
    16_000
};

/// Stack one nested Form XObject (or annotation appearance) costs.
const FORM_COST: u32 = if cfg!(debug_assertions) {
    19_000
} else {
    4_300
};

/// Stack one tiling pattern cell costs. A cell gets a renderer of its own.
const PATTERN_COST: u32 = if cfg!(debug_assertions) {
    28_000
} else {
    7_400
};

/// Stack one soft-mask group costs. A group gets a renderer of its own.
const SOFT_MASK_COST: u32 = if cfg!(debug_assertions) {
    24_000
} else {
    7_700
};

/// Stack one Type 3 glyph procedure costs.
const GLYPH_COST: u32 = if cfg!(debug_assertions) {
    30_000
} else {
    6_600
};

/// Most contexts one render will vet.
///
/// Resources are inherited, so a stream is vetted once per place it is drawn
/// from, and a small document can describe an enormous tree: twenty levels
/// that each draw the next level twice is a million draws. The interpreter
/// would execute every one of them, so refusing past this bound also refuses
/// a render that would not finish. Ordinary pages need tens to hundreds.
const MAX_VETTED_CONTEXTS: usize = 100_000;

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
    /// Operands of `Do`.
    x_objects: Vec<Vec<u8>>,
    /// Pattern operands of `scn` and `SCN`.
    patterns: Vec<Vec<u8>>,
    /// Operands of `gs`.
    ext_g_states: Vec<Vec<u8>>,
    /// Font operands of `Tf`.
    fonts: Vec<Vec<u8>>,
}

impl Scan {
    fn of(content: &[u8]) -> Self {
        let mut scan = Self::default();
        for instruction in TypedIter::new(content) {
            match instruction {
                TypedInstruction::XObject(op) => remember(&mut scan.x_objects, &op.0),
                TypedInstruction::NonStrokeColorNamed(op) => {
                    if let Some(name) = &op.1 {
                        remember(&mut scan.patterns, name);
                    }
                }
                TypedInstruction::StrokeColorNamed(op) => {
                    if let Some(name) = &op.1 {
                        remember(&mut scan.patterns, name);
                    }
                }
                TypedInstruction::SetGraphicsState(op) => remember(&mut scan.ext_g_states, &op.0),
                TypedInstruction::TextFont(op) => remember(&mut scan.fonts, &op.0),
                _ => {}
            }
        }
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
fn remember(names: &mut Vec<Vec<u8>>, name: &Name<'_>) {
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
    if !names.contains(&escaped) {
        names.push(escaped);
    }
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
    resources: Resources<'a>,
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
    /// Whether this stream selects a Type 3 font.
    shows_glyphs: bool,
}

impl Frame<'_> {
    /// Most stack needed beneath this stream.
    fn height(&self) -> u32 {
        // A pattern, soft mask or font set here stays in force inside every
        // form drawn afterwards, so it is charged at the bottom of the
        // deepest run of forms, under a glyph procedure that may be the
        // thing painting with it.
        let mut height = self.through_forms;
        if let Some(target) = self.target {
            height = height.max(
                self.form_chain
                    .saturating_add(GLYPH_COST)
                    .saturating_add(target),
            );
        }
        if self.shows_glyphs {
            height = height.max(self.form_chain.saturating_add(GLYPH_COST));
        }
        height
    }
}

/// State of one page's vetting.
struct Walk<'a> {
    scans: HashMap<StreamId, Option<Rc<Scan>>>,
    /// Type 3 fonts already checked, by the context whose resources name them.
    fonts: HashMap<(u32, u32, Vec<u8>), ()>,
    stack: Vec<Frame<'a>>,
    visited: usize,
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
    fn scan(&mut self, stream: &Stream<'a>) -> Option<Rc<Scan>> {
        self.scans
            .entry(stream_id(stream))
            .or_insert_with(|| {
                stream
                    .decoded()
                    .ok()
                    .map(|content| Rc::new(Scan::of(&content)))
            })
            .clone()
    }

    fn push(&mut self, child: Child<'a>, floor: u32) -> Result<()> {
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
            resources: child.resources,
            floor,
            children: Vec::new(),
            form_chain: 0,
            through_forms: 0,
            target: None,
            shows_glyphs: false,
        };
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
                self.queue(frame, Route::Form, &stream);
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
            self.queue(frame, Route::Pattern, &stream);
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
            self.queue(frame, Route::SoftMask, &stream);
        }
        for name in &scan.fonts {
            self.count()?;
            self.vet_font(frame, name)?;
        }
        Ok(())
    }

    /// Queues `stream` as a child of `frame`, or settles it on the spot when
    /// it uses no resources and so cannot reach anything.
    fn queue(&mut self, frame: &mut Frame<'a>, route: Route, stream: &Stream<'a>) {
        let Some(scan) = self.scan(stream) else {
            return;
        };
        if scan.is_empty() {
            settle(frame, route, 0, 0);
            return;
        }
        frame.children.push(Child {
            route,
            stream: stream_id(stream),
            scan,
            resources: Resources::from_parent(
                own_resources(stream.dict()),
                frame.resources.clone(),
            ),
        });
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
        if !has_subtype(&font, b"Type3") {
            return Ok(());
        }
        frame.shows_glyphs = true;
        // The stack holds this frame's ancestors; `above` levels up is one of
        // them, or one of the page tree's own resource levels past the root.
        let ancestors = self.stack.len();
        let owner = match ancestors.checked_sub(above) {
            Some(index) if above > 0 => self.stack.get(index).map_or(0, |owner| owner.id),
            _ if above == 0 => frame.id,
            _ => 0,
        };
        let beyond_root = u32::try_from(above.saturating_sub(ancestors)).unwrap_or(u32::MAX);
        if self
            .fonts
            .insert((owner, beyond_root, name.to_vec()), ())
            .is_some()
        {
            return Ok(());
        }

        let font_resources = own_resources(&font);
        let procedures = font.get::<Dict<'_>>(&b"CharProcs"[..]).unwrap_or_default();
        for glyph in procedures.keys() {
            self.count()?;
            let Some(procedure) = procedures.get::<Stream<'a>>(glyph) else {
                continue;
            };
            let Some(scan) = self.scan(&procedure) else {
                continue;
            };
            if scan.is_empty() {
                continue;
            }
            // Mirrors `Type3::render_glyph`: the procedure's own resources
            // come first, then the font's.
            let local = [
                procedure
                    .dict()
                    .get::<Dict<'_>>(&b"Resources"[..])
                    .unwrap_or_default(),
                font_resources.clone(),
            ];
            let category = |key: &[u8]| {
                local
                    .iter()
                    .map(|resources| resources.get::<Dict<'_>>(key).unwrap_or_default())
                    .collect::<Vec<_>>()
            };
            let x_objects = category(b"XObject");
            let harmless = scan.x_objects.iter().all(|name| {
                x_objects
                    .iter()
                    .find_map(|dict| dict.get::<Stream<'_>>(Name::new(name)))
                    .is_some_and(|stream| !has_subtype(stream.dict(), b"Form"))
            });
            let patterns = category(b"Pattern");
            let harmless = harmless
                && scan.patterns.iter().all(|name| {
                    patterns
                        .iter()
                        .find_map(|dict| dict.get::<Object<'_>>(Name::new(name)))
                        .is_some_and(|pattern| pattern.into_stream().is_none())
                });
            let states = category(b"ExtGState");
            let harmless = harmless
                && scan.ext_g_states.iter().all(|name| {
                    states
                        .iter()
                        .find_map(|dict| dict.get::<Dict<'_>>(Name::new(name)))
                        .is_some_and(|state| state.get::<Dict<'_>>(&b"SMask"[..]).is_none())
                });
            let fonts = category(b"Font");
            let harmless = harmless
                && scan.fonts.iter().all(|name| {
                    fonts
                        .iter()
                        .find_map(|dict| dict.get::<Dict<'_>>(Name::new(name)))
                        .is_some_and(|font| !has_subtype(&font, b"Type3"))
                });
            if !harmless {
                return Err(refused(
                    "a Type 3 glyph procedure can reach another content stream",
                ));
            }
        }
        Ok(())
    }

    /// Vets `root` and everything beneath it; `floor` is the stack already in
    /// use when `root` starts. Returns the stack needed beneath `root`.
    fn run(&mut self, root: Child<'a>, floor: u32) -> Result<u32> {
        self.push(root, floor)?;
        loop {
            let Some(frame) = self.stack.last_mut() else {
                return Ok(0);
            };
            if let Some(child) = frame.children.pop() {
                let floor = frame.floor.saturating_add(child.route.cost());
                self.count()?;
                self.push(child, floor)?;
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

/// Refuses `page` unless the interpreter can render it within
/// [`STACK_BUDGET`].
///
/// `page` must be the very object handed to the interpreter afterwards.
///
/// # Errors
///
/// Returns an error when a content stream reachable from the page reaches
/// itself, when the streams nest deeper than the budget allows, when a Type 3
/// glyph procedure could enter another content stream, or when the page
/// describes more nested content than [`MAX_VETTED_CONTEXTS`].
pub(crate) fn vet_page(page: &Page<'_>) -> Result<()> {
    let mut walk = Walk {
        scans: HashMap::new(),
        fonts: HashMap::new(),
        stack: Vec::new(),
        visited: 0,
        next_id: 1,
    };

    // The page's own stream. The interpreter decodes it through the same
    // cache, so scanning it here costs no second decode.
    let page_scan = Rc::new(Scan::of(page.page_stream().unwrap_or_default()));
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
                resources: page.resources().clone(),
            },
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
        let Some(scan) = walk.scan(&appearance) else {
            continue;
        };
        if scan.is_empty() {
            continue;
        }
        walk.run(
            Child {
                route: Route::Form,
                stream: stream_id(&appearance),
                scan,
                resources: Resources::from_parent(
                    own_resources(appearance.dict()),
                    page.resources().clone(),
                ),
            },
            PAGE_COST.saturating_add(FORM_COST),
        )?;
    }
    Ok(())
}
