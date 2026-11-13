//! XSD file parser.
//!
//! Parses one or more XSD files (following `xs:include` / `xs:import`) into a
//! [`SchemaModel`]. Uses `quick-xml` for event parsing and `memmap2` for cheap
//! file reads.
//!
//! Each recursive parsing function allocates its own `Vec<u8>` read buffer so
//! that borrow lifetimes never conflict across nested calls, and reads element
//! boundaries as owned `Tag` values so its stack frame stays small.

use std::collections::{HashMap, HashSet};
use std::path::{Component, Path, PathBuf};
use std::sync::Arc;

use anyhow::{Context, Result};
use memmap2::Mmap;
use ooxml_archive::utf8::LossyUtf8;
use quick_xml::events::attributes::Attribute;
use quick_xml::events::Event;
use quick_xml::{Reader, XmlVersion};

use crate::schema::{
    AttributeDecl, ComplexType, ContentModel, Derivation, ElementDecl, ElementRef, EnumValue,
    OccurrenceConstraint, Particle, QName, SchemaModel, SimpleFacets, SimpleTypeDef,
    SimpleTypeKind, Wildcard, XsdType,
};

// ── XS namespace URIs accepted as the XML Schema namespace ──────────────────

const XS_URIS: &[&str] = &[
    "http://www.w3.org/2001/XMLSchema",
    "http://www.w3.org/1999/XMLSchema",
];

/// Default aggregate byte budget for a root schema and all followed includes.
pub const DEFAULT_MAX_SCHEMA_BYTES: usize = 32 * 1024 * 1024;
/// Default byte budget for one raw `schemaLocation` attribute.
pub const DEFAULT_MAX_SCHEMA_LOCATION_BYTES: usize = 4 * 1024;

/// Deepest nesting of type-bearing constructs accepted in one schema file.
///
/// One level is one recursive parser call: a model group inside a model
/// group, or an inline type inside an element. An element with an inline
/// complex type and a sequence costs three levels, so this admits schemas
/// nested about twenty elements deep, well beyond hand-written or generated
/// schemas. The parser, and every later walk over the model (validation,
/// Arrow derivation, `Clone`, `Drop`, serialization), recurses once per
/// level, so the value is sized to keep all of them inside a 256 KiB stack in
/// an unoptimized build. Raising it needs the deep-nesting smoke tests rerun.
const MAX_NESTING_DEPTH: usize = 64;

/// Deepest chain of `xs:include` / `xs:import` files followed from the root.
///
/// The root is not counted: a chain of the root and 32 files, each included
/// by the one before, loads, and a 33rd is refused. Each followed file is
/// parsed while its includer is still on the stack, so the chain length,
/// unlike the file count in [`ParseOptions::max_includes`], costs stack. The
/// deepest chains in published schema sets (GML, NIEM) are around fifteen
/// files long.
const MAX_INCLUDE_NESTING: usize = 32;

// ── Public API ───────────────────────────────────────────────────────────────

/// Options controlling the parse phase.
#[derive(Debug, Clone)]
pub struct ParseOptions {
    /// Maximum number of schema files to follow via `xs:include` / `xs:import`.
    /// Prevents runaway cycles. Default: 64.
    pub max_includes: usize,
    /// Maximum aggregate bytes across the root schema and all unique followed
    /// `xs:include` / `xs:import` files. Default: 32 MiB.
    pub max_schema_bytes: usize,
    /// Maximum raw UTF-8 bytes in one `schemaLocation` value. The bound is
    /// checked before XML unescaping can allocate. Default: 4 KiB.
    pub max_schema_location_bytes: usize,
    /// Whether to map `xs:date` / `xs:dateTime` to Arrow `Date32` / `Timestamp`
    /// in the arrow feature. Default: true.
    pub map_date_types: bool,
}

impl Default for ParseOptions {
    fn default() -> Self {
        Self {
            max_includes: 64,
            max_schema_bytes: DEFAULT_MAX_SCHEMA_BYTES,
            max_schema_location_bytes: DEFAULT_MAX_SCHEMA_LOCATION_BYTES,
            map_date_types: true,
        }
    }
}

/// A fully loaded, multi-file XSD schema set.
pub struct SchemaSet {
    pub(crate) model: SchemaModel,
    pub(crate) options: ParseOptions,
}

impl SchemaSet {
    /// Load a schema from disk, following `xs:include` / `xs:import` relative paths.
    ///
    /// Each schema file is memory-mapped read-only while it is parsed and
    /// unmapped before this returns. No schema file may be modified or
    /// truncated, by this process or any other, while the call runs. Nothing
    /// here can enforce that: a truncated file makes reads of the mapping
    /// fault (`SIGBUS` on Unix), and a modified one changes the bytes being
    /// parsed. Read the files yourself and use [`Self::load_bytes`] when
    /// another writer may touch them.
    pub fn load(path: impl AsRef<Path>, options: ParseOptions) -> Result<Self> {
        let path = path
            .as_ref()
            .canonicalize()
            .with_context(|| format!("schema file not found: {}", path.as_ref().display()))?;
        let dir = path.parent().unwrap_or(Path::new(".")).to_path_buf();
        let mut loader = Loader::new(Some(dir), &options);
        loader.load_root_file(&path)?;
        let mut model = loader.finish();
        // Fold complexContent/extension base types into derived types now that
        // every named type is loaded across all included files.
        model.try_resolve_inheritance()?;
        Ok(Self { model, options })
    }

    /// Load a schema from an in-memory XSD byte slice (no `xs:include` resolution).
    pub fn load_bytes(bytes: &[u8], options: ParseOptions) -> Result<Self> {
        let mut loader = Loader::new(None, &options);
        loader.charge_schema_bytes(bytes.len())?;
        loader.parse_bytes(bytes)?;
        let mut model = loader.finish();
        model.try_resolve_inheritance()?;
        Ok(Self { model, options })
    }

    /// The merged schema model.
    pub fn model(&self) -> &SchemaModel {
        &self.model
    }

    /// Options used when this schema set was loaded.
    pub fn options(&self) -> &ParseOptions {
        &self.options
    }
}

// ── Loader ──────────────────────────────────────────────────────────────────

/// Per-file parse state: what one schema document has declared so far.
struct FileState {
    /// Namespace prefix to URI map for this file.
    ns_map: HashMap<String, String>,
    /// Whether the parser is inside `<xs:schema>`.
    in_schema: bool,
}

/// A schema document as the XML reader reads it.
///
/// The reader takes UTF-8 only, so a byte that is not UTF-8 is read as
/// U+FFFD. Where a name or an attribute value was once read from raw bytes
/// and refused when they were not UTF-8, it is still refused: the input
/// remembers where it put each U+FFFD, and [`Repairs`] tells a value holding
/// one from a value holding a U+FFFD the document really has. Text that was
/// never read stays ignored.
pub(crate) type SchemaInput<'a> = LossyUtf8<&'a [u8]>;

/// Reads `bytes` as a schema document, refusing to repair it past `limit`.
pub(crate) fn schema_input(bytes: &[u8], limit: usize) -> SchemaInput<'_> {
    LossyUtf8::with_limit(bytes, limit.max(bytes.len()) as u64)
}

/// Makes the reader for one schema document, on the heap.
///
/// No reader of a schema document collects element text: declarations are
/// read from attributes alone, and annotations are skipped. So trimming,
/// which works on each piece of text between references, cannot change a
/// value here. For the same reason a bare `&` in that text, which is not
/// well-formed, is passed over and does not fail the load: published
/// schemas carry them in documentation.
///
/// The reader is built here, not in the caller, so that neither it nor the
/// temporary it is moved from takes room in [`Loader::parse_bytes`]' frame,
/// which recursion through includes repeats.
#[inline(never)]
fn schema_reader(bytes: &[u8], limit: usize) -> Box<Reader<SchemaInput<'_>>> {
    let mut reader = Box::new(Reader::from_reader(schema_input(bytes, limit)));
    reader.config_mut().trim_text(true);
    reader.config_mut().allow_dangling_amp = true;
    reader
}

/// Whether the end tag `e` closes `xs:schema`.
///
/// Kept out of [`Loader::parse_bytes`] so that its temporaries do not take
/// room in a frame that recursion through includes repeats.
#[inline(never)]
fn closes_schema(e: &quick_xml::events::BytesEnd<'_>, input: &SchemaInput<'_>) -> bool {
    local_name(e.name().as_ref(), Repairs::end(input, e)) == "schema"
}

/// Tells the bytes the reader replaced in the event it read last from text
/// the document holds.
///
/// Built from the reader's input and the event's content right after the
/// event is read, before the next one is.
#[derive(Clone, Copy)]
pub(crate) struct Repairs<'a> {
    input: &'a SchemaInput<'a>,
    event: &'a str,
    /// Bytes the reader consumed after the event's content, at least and at
    /// most; see [`LossyUtf8::repaired_in`].
    after: (u64, u64),
}

impl<'a> Repairs<'a> {
    /// For a start or empty tag.
    pub(crate) fn tag(input: &'a SchemaInput<'a>, tag: &'a str) -> Self {
        Self {
            input,
            event: tag,
            after: (1, 2),
        }
    }

    /// For an end tag or a `<!DOCTYPE>`, which end with `>`.
    pub(crate) fn end(input: &'a SchemaInput<'a>, tag: &'a str) -> Self {
        Self {
            input,
            event: tag,
            after: (1, 1),
        }
    }

    /// For text the reader does not trim.
    pub(crate) fn text(input: &'a SchemaInput<'a>, text: &'a str) -> Self {
        Self {
            input,
            event: text,
            after: (0, 0),
        }
    }

    /// Whether `value`, part of the event, stands for bytes that are not
    /// UTF-8.
    pub(crate) fn not_utf8(&self, value: &str) -> bool {
        self.input.repaired_in(
            self.event.as_bytes(),
            self.after.0..=self.after.1,
            value.as_bytes(),
        )
    }
}

struct Loader {
    model: SchemaModel,
    base_dir: PathBuf,
    root_dir: Option<PathBuf>,
    visited: HashSet<PathBuf>,
    max_includes: usize,
    followed_includes: usize,
    max_schema_bytes: usize,
    schema_bytes: usize,
    max_schema_location_bytes: usize,
    /// Files currently being parsed, the root included.
    include_nesting: usize,
    /// The named attribute groups, by local name, as written: references
    /// to other groups are still placeholders.
    attribute_groups: HashMap<String, Vec<AttributeDecl>>,
    /// The global attribute declarations, by local name.
    global_attributes: HashMap<String, AttributeDecl>,
}

impl Loader {
    fn new(root_dir: Option<PathBuf>, options: &ParseOptions) -> Self {
        Self {
            model: SchemaModel::default(),
            base_dir: root_dir.clone().unwrap_or_default(),
            root_dir,
            visited: HashSet::new(),
            max_includes: options.max_includes,
            followed_includes: 0,
            max_schema_bytes: options.max_schema_bytes,
            schema_bytes: 0,
            max_schema_location_bytes: options.max_schema_location_bytes,
            include_nesting: 0,
            attribute_groups: HashMap::new(),
            global_attributes: HashMap::new(),
        }
    }

    fn load_root_file(&mut self, path: &Path) -> Result<()> {
        self.load_file(path, false)
    }

    fn load_include_file(&mut self, path: &Path) -> Result<()> {
        self.load_file(path, true)
    }

    fn load_file(&mut self, path: &Path, is_include: bool) -> Result<()> {
        let Some((mmap, parent)) = self.open_schema_file(path, is_include)? else {
            return Ok(());
        };
        let prev_dir = std::mem::replace(&mut self.base_dir, parent);
        self.include_nesting += 1;
        let result = self.parse_bytes(&mmap);
        self.include_nesting -= 1;
        self.base_dir = prev_dir;
        result
    }

    /// Checks one schema file against every limit and maps it.
    ///
    /// Returns the mapping and the file's directory, or `None` when the file
    /// was already loaded. Kept apart from [`Loader::load_file`], which stays
    /// on the stack while the file's own includes are followed.
    fn open_schema_file(
        &mut self,
        path: &Path,
        is_include: bool,
    ) -> Result<Option<(Mmap, PathBuf)>> {
        let canon = path
            .canonicalize()
            .with_context(|| format!("include not found: {}", path.display()))?;
        if let Some(root_dir) = &self.root_dir {
            if !canon.starts_with(root_dir) {
                return Err(anyhow::anyhow!(
                    "rejected schemaLocation outside uploaded schema root: {}",
                    canon.display()
                ));
            }
        } else {
            return Err(anyhow::anyhow!(
                "rejected schemaLocation for an in-memory schema set"
            ));
        }
        if self.visited.contains(&canon) {
            return Ok(None);
        }
        if self.include_nesting > MAX_INCLUDE_NESTING {
            return Err(anyhow::anyhow!(
                "resource_limit: schema_include_nesting: include chain is deeper than {MAX_INCLUDE_NESTING} files below the root schema"
            ));
        }
        let next_include_count = if is_include {
            let observed = self.followed_includes.saturating_add(1);
            if observed > self.max_includes {
                return Err(anyhow::anyhow!(
                    "resource_limit: schema_includes: observed {observed} exceeds limit {}",
                    self.max_includes
                ));
            }
            observed
        } else {
            self.followed_includes
        };
        let file =
            std::fs::File::open(&canon).with_context(|| format!("open: {}", canon.display()))?;
        // SAFETY: `Mmap::map` is unsafe because the mapping is only sound
        // while no one modifies or truncates the file, and no code here can
        // enforce that: a truncated schema makes reads of the mapping fault
        // (`SIGBUS` on Unix), a modified one changes the bytes being parsed.
        // `SchemaSet::load`, the one public entry point that reads schemas
        // from disk, states the contract that the caller must not let those
        // files change while they are parsed. Within this process the mapping
        // is read-only and never written through; `load_file` parses it and
        // drops it before `SchemaSet::load` returns.
        let mmap =
            unsafe { Mmap::map(&file) }.with_context(|| format!("mmap: {}", canon.display()))?;
        self.charge_schema_bytes(mmap.len())?;
        self.followed_includes = next_include_count;
        let parent = canon.parent().unwrap_or(Path::new(".")).to_path_buf();
        self.visited.insert(canon);
        Ok(Some((mmap, parent)))
    }

    fn charge_schema_bytes(&mut self, bytes: usize) -> Result<()> {
        let observed = self.schema_bytes.saturating_add(bytes);
        if observed > self.max_schema_bytes {
            return Err(anyhow::anyhow!(
                "resource_limit: schema_bytes: observed {observed} exceeds limit {}",
                self.max_schema_bytes
            ));
        }
        self.schema_bytes = observed;
        Ok(())
    }

    /// The file a `schemaLocation` names, relative to the including file.
    ///
    /// A location may go up with `..` as long as it stays inside the root
    /// schema's directory at every step. The path is folded by its text, not
    /// by the filesystem, so a location that would leave the root is refused
    /// without anything outside the root being looked at, and a `..` cannot
    /// be carried elsewhere by a symbolic link. Links are settled afterwards,
    /// when the file is opened: its canonical path must be inside the root.
    ///
    /// # Errors
    ///
    /// Refuses a URL, an absolute path, a location that steps outside the
    /// root schema's directory, and any location of an in-memory schema.
    fn resolve_schema_location(&self, location: &str) -> Result<PathBuf> {
        let rejected = || anyhow::anyhow!("rejected schemaLocation: {location}");
        let Some(root_dir) = &self.root_dir else {
            return Err(rejected());
        };
        let path = Path::new(location);
        if location.contains("://") || path.is_absolute() {
            return Err(rejected());
        }
        let mut resolved = self.base_dir.clone();
        for component in path.components() {
            match component {
                Component::CurDir => {}
                Component::Normal(part) => resolved.push(part),
                Component::ParentDir => {
                    if !resolved.pop() || !resolved.starts_with(root_dir) {
                        return Err(rejected());
                    }
                }
                Component::RootDir | Component::Prefix(_) => return Err(rejected()),
            }
        }
        Ok(resolved)
    }

    fn parse_bytes(&mut self, bytes: &[u8]) -> Result<()> {
        // This frame stays on the stack while an include is followed, once
        // per file in the chain, so the reader and its repair state live on
        // the heap rather than here.
        let mut reader = schema_reader(bytes, self.max_schema_bytes);

        let mut file = FileState {
            ns_map: HashMap::new(),
            in_schema: false,
        };
        let mut buf = Vec::with_capacity(4096);

        loop {
            // Each handler returns the file to follow, if any, so the include
            // is followed from this frame and not from the handler's.
            let include = match reader.read_event_into(&mut buf) {
                Ok(Event::Start(ref e)) => self.top_level_start(e, &mut reader, &mut file)?,
                Ok(Event::Empty(ref e)) => self.top_level_empty(e, reader.get_ref(), &mut file)?,
                Ok(Event::End(ref e)) => {
                    if closes_schema(e, reader.get_ref()) {
                        file.in_schema = false;
                    }
                    None
                }
                Ok(Event::Eof) => break,
                Err(e) => return Err(anyhow::anyhow!("XML parse error: {e}")),
                _ => None,
            };
            if let Some(path) = include {
                self.load_include_file(&path)?;
            }
            buf.clear();
        }
        Ok(())
    }

    /// Handles one top-level start tag; returns the file an include names.
    fn top_level_start(
        &mut self,
        e: &quick_xml::events::BytesStart<'_>,
        reader: &mut Reader<SchemaInput<'_>>,
        file: &mut FileState,
    ) -> Result<Option<PathBuf>> {
        // Collect xmlns declarations.
        collect_xmlns(e, &mut file.ns_map, Repairs::tag(reader.get_ref(), e));
        let local = local_name(e.name().as_ref(), Repairs::tag(reader.get_ref(), e));

        match local.as_str() {
            "schema" => {
                // Extract targetNamespace.
                let attrs = collect_attrs(e, Repairs::tag(reader.get_ref(), e));
                if let Some(tns) = attrs.get("targetNamespace") {
                    self.model.target_namespace = Some(tns.clone());
                    // Kept with the prefixes, which every parser of this
                    // document's declarations is handed, for wildcards.
                    file.ns_map
                        .insert(TARGET_NAMESPACE_KEY.to_owned(), tns.clone());
                }
                file.in_schema = true;
            }
            "element" if file.in_schema => {
                let attrs = collect_attrs(e, Repairs::tag(reader.get_ref(), e));
                // Top-level element: parse its children.
                let decl = parse_element_start(&attrs, reader, &file.ns_map, 0)?;
                if let Some(decl) = decl {
                    self.model
                        .global_elements
                        .insert(decl.name.local.clone(), decl);
                }
            }
            "complexType" if file.in_schema => {
                let attrs = collect_attrs(e, Repairs::tag(reader.get_ref(), e));
                let name = attrs.get("name").cloned();
                let ct = parse_complex_type(reader, &file.ns_map, 0)?;
                if let Some(n) = name {
                    self.model.complex_types.insert(n, ct);
                }
            }
            "simpleType" if file.in_schema => {
                let attrs = collect_attrs(e, Repairs::tag(reader.get_ref(), e));
                let name = attrs.get("name").cloned();
                let mut st = parse_simple_type(reader, &file.ns_map)?;
                if let Some(n) = &name {
                    st.name = Some(QName::local(n));
                    self.model.simple_types.insert(n.clone(), st);
                }
            }
            "attribute" if file.in_schema => {
                let attrs = collect_attrs(e, Repairs::tag(reader.get_ref(), e));
                let inline_type = parse_attribute_children(reader, &file.ns_map)?;
                if let Some(attribute) = parse_attribute_decl(&attrs, inline_type, &file.ns_map) {
                    self.global_attributes
                        .insert(attribute.name.clone(), attribute);
                }
            }
            "attributeGroup" if file.in_schema => {
                let attrs = collect_attrs(e, Repairs::tag(reader.get_ref(), e));
                let attributes = parse_attribute_group(reader, &file.ns_map)?;
                if let Some(name) = attrs.get("name") {
                    self.attribute_groups.insert(name.clone(), attributes);
                }
            }
            "include" | "import" => {
                return self.include_target(e, Repairs::tag(reader.get_ref(), e))
            }
            _ => {}
        }
        Ok(None)
    }

    /// Handles one top-level self-closing tag; returns the file an include names.
    fn top_level_empty(
        &mut self,
        e: &quick_xml::events::BytesStart<'_>,
        input: &SchemaInput<'_>,
        file: &mut FileState,
    ) -> Result<Option<PathBuf>> {
        collect_xmlns(e, &mut file.ns_map, Repairs::tag(input, e));
        let local = local_name(e.name().as_ref(), Repairs::tag(input, e));

        match local.as_str() {
            "include" | "import" => return self.include_target(e, Repairs::tag(input, e)),
            // A self-closing named type declares a type with nothing in it:
            // no content and no attributes, or no restriction.
            "complexType" if file.in_schema => {
                if let Some(name) = collect_attrs(e, Repairs::tag(input, e)).remove("name") {
                    self.model.complex_types.insert(name, empty_complex_type());
                }
            }
            "attributeGroup" if file.in_schema => {
                if let Some(name) = collect_attrs(e, Repairs::tag(input, e)).remove("name") {
                    self.attribute_groups.insert(name, Vec::new());
                }
            }
            "attribute" if file.in_schema => {
                if let Some(attribute) = parse_attribute_decl(
                    &collect_attrs(e, Repairs::tag(input, e)),
                    None,
                    &file.ns_map,
                ) {
                    self.global_attributes
                        .insert(attribute.name.clone(), attribute);
                }
            }
            "simpleType" if file.in_schema => {
                if let Some(name) = collect_attrs(e, Repairs::tag(input, e)).remove("name") {
                    let mut simple = SimpleTypeDef::string_unbounded();
                    simple.name = Some(QName::local(name.as_str()));
                    self.model.simple_types.insert(name, simple);
                }
            }
            "element" if file.in_schema => {
                let attrs = collect_attrs(e, Repairs::tag(input, e));
                // Self-closing top-level element with type= attribute.
                if let Ok(Some(decl)) = parse_element_start(
                    &attrs,
                    // No children to parse since it's empty — pass a stub reader.
                    &mut Reader::from_reader(LossyUtf8::new(&b""[..])),
                    &file.ns_map,
                    0,
                ) {
                    self.model
                        .global_elements
                        .insert(decl.name.local.clone(), decl);
                }
            }
            _ => {}
        }
        Ok(None)
    }

    /// Path of the file an `xs:include` / `xs:import` names, if it names one.
    fn include_target(
        &self,
        e: &quick_xml::events::BytesStart<'_>,
        repairs: Repairs<'_>,
    ) -> Result<Option<PathBuf>> {
        match schema_location(e, self.max_schema_location_bytes, repairs)? {
            Some(location) => self.resolve_schema_location(&location).map(Some),
            None => Ok(None),
        }
    }

    /// The loaded model, with every type reference settled.
    fn finish(mut self) -> SchemaModel {
        let mut expansion = Expansion {
            attribute_groups: &self.attribute_groups,
            global_attributes: &self.global_attributes,
            remaining: MAX_EXPANDED_NODES,
            text_remaining: MAX_EXPANDED_TEXT_BYTES,
            walks: 0,
        };
        expansion.expand_model(&mut self.model);
        settle_unprefixed_builtin_names(&mut self.model);
        self.model
    }
}

// ── Element declaration parser ───────────────────────────────────────────────

/// Refuses a construct nested deeper than [`MAX_NESTING_DEPTH`].
///
/// Every recursive parse function calls this on entry with its own depth, so
/// the recursion, and the nesting of the model it builds, are both bounded.
fn check_nesting(depth: usize) -> Result<()> {
    if depth > MAX_NESTING_DEPTH {
        return Err(anyhow::anyhow!(
            "resource_limit: schema_nesting_depth: observed {depth} exceeds limit {MAX_NESTING_DEPTH}"
        ));
    }
    Ok(())
}

fn parse_element_start(
    attrs: &HashMap<String, String>,
    reader: &mut Reader<SchemaInput<'_>>,
    ns_map: &HashMap<String, String>,
    depth: usize,
) -> Result<Option<ElementDecl>> {
    let name = match attrs.get("name") {
        Some(n) => n.clone(),
        None => return Ok(None),
    };

    let occurrence = parse_occurrence(attrs);
    let nillable = attrs.get("nillable").is_some_and(|v| v == "true");

    let type_def = match declared_type(attrs, ns_map) {
        Some(declared) => declared,
        // Inline type in children.
        None => parse_inline_type(reader, ns_map, depth + 1)?,
    };

    Ok(Some(ElementDecl {
        name: QName::local(name),
        type_def: *type_def,
        occurrence,
        nillable,
        documentation: None,
    }))
}

/// One element boundary of a schema document, detached from the read buffer.
///
/// The recursive parse functions match on this instead of on the reader's
/// borrowed event. Their stack frames then hold one small owned value per
/// level instead of every temporary of every match arm, which is what keeps
/// [`MAX_NESTING_DEPTH`] levels inside a small stack.
enum Tag {
    /// `<local ...>`.
    Start {
        local: String,
        attrs: HashMap<String, String>,
    },
    /// `<local .../>`.
    Empty {
        local: String,
        attrs: HashMap<String, String>,
    },
    /// `</local>`.
    End { local: String },
    /// End of input.
    Eof,
}

/// Reads up to the next element boundary, skipping text, comments and the like.
///
/// `context` names the construct being parsed, for the error message.
fn next_tag(reader: &mut Reader<SchemaInput<'_>>, buf: &mut Vec<u8>, context: &str) -> Result<Tag> {
    loop {
        buf.clear();
        match reader.read_event_into(buf) {
            Ok(Event::Start(ref e)) => return Ok(start_tag(e, reader.get_ref(), false)),
            Ok(Event::Empty(ref e)) => return Ok(start_tag(e, reader.get_ref(), true)),
            Ok(Event::End(ref e)) => return Ok(end_tag(e, reader.get_ref())),
            Ok(Event::Eof) => return Ok(Tag::Eof),
            Err(e) => return Err(anyhow::anyhow!("{context} parse error: {e}")),
            _ => {}
        }
    }
}

/// The [`Tag`] for a start or (`empty`) empty tag.
///
/// Built outside [`next_tag`], whose frame sits under the reader's own while
/// an event is read, so these temporaries never add to that deepest point.
#[inline(never)]
fn start_tag(e: &quick_xml::events::BytesStart<'_>, input: &SchemaInput<'_>, empty: bool) -> Tag {
    let repairs = Repairs::tag(input, e);
    let local = local_name(e.name().as_ref(), repairs);
    let attrs = collect_attrs(e, repairs);
    if empty {
        Tag::Empty { local, attrs }
    } else {
        Tag::Start { local, attrs }
    }
}

/// The [`Tag`] for an end tag; see [`start_tag`].
#[inline(never)]
fn end_tag(e: &quick_xml::events::BytesEnd<'_>, input: &SchemaInput<'_>) -> Tag {
    Tag::End {
        local: local_name(e.name().as_ref(), Repairs::end(input, e)),
    }
}

/// Local part of an element name; empty when the name is not UTF-8.
fn local_name(raw: &str, repairs: Repairs<'_>) -> String {
    split_prefix(name_text(raw, repairs)).1.to_owned()
}

/// An element name as written; empty when the name is not UTF-8.
fn name_text<'n>(raw: &'n str, repairs: Repairs<'_>) -> &'n str {
    if repairs.not_utf8(raw) {
        ""
    } else {
        raw
    }
}

fn parse_inline_type(
    reader: &mut Reader<SchemaInput<'_>>,
    ns_map: &HashMap<String, String>,
    depth: usize,
) -> Result<Box<XsdType>> {
    check_nesting(depth)?;
    let mut buf = Vec::new();
    loop {
        match next_tag(reader, &mut buf, "inline type")? {
            Tag::Start { local, .. } => match local.as_str() {
                "complexType" => {
                    let mut ct = Box::new(XsdType::Complex(empty_complex_type()));
                    if let XsdType::Complex(inner) = ct.as_mut() {
                        parse_complex_type_into(reader, ns_map, depth + 1, inner)?;
                    }
                    return Ok(ct);
                }
                "simpleType" => return parse_inline_simple_type(reader, ns_map),
                other => skip_element(reader, other)?,
            },
            Tag::Empty { .. } => {}
            Tag::End { .. } | Tag::Eof => break,
        }
    }
    Ok(unbounded_string_type())
}

/// Parses the children of an inline `<xs:simpleType>` into a boxed type.
fn parse_inline_simple_type(
    reader: &mut Reader<SchemaInput<'_>>,
    ns_map: &HashMap<String, String>,
) -> Result<Box<XsdType>> {
    Ok(Box::new(XsdType::Simple(parse_simple_type(
        reader, ns_map,
    )?)))
}

/// The type of an element that declares none: an unconstrained string.
fn unbounded_string_type() -> Box<XsdType> {
    Box::new(XsdType::Simple(SimpleTypeDef::string_unbounded()))
}

/// A complex type with no content, no attributes and no base, as
/// `<xs:complexType/>` declares it.
///
/// It is marked empty in the source; [`parse_complex_type_into`] takes the
/// mark away at the first child that is not an annotation.
fn empty_complex_type() -> ComplexType {
    ComplexType {
        name: None,
        content: ContentModel::Empty,
        attributes: Vec::new(),
        base: None,
        derivation: Derivation::default(),
        prohibited_attributes: Vec::new(),
        empty_in_source: true,
        content_occurrence: OccurrenceConstraint::default(),
    }
}

// ── Complex type parser ──────────────────────────────────────────────────────

fn parse_complex_type(
    reader: &mut Reader<SchemaInput<'_>>,
    ns_map: &HashMap<String, String>,
    nesting: usize,
) -> Result<ComplexType> {
    let mut complex_type = empty_complex_type();
    parse_complex_type_into(reader, ns_map, nesting, &mut complex_type)?;
    Ok(complex_type)
}

/// Parses the children of an `<xs:complexType>` into `complex_type`.
///
/// Writing through the reference, instead of returning the type, keeps the
/// type out of this function's stack frame: it recurses once per nested
/// inline type.
fn parse_complex_type_into(
    reader: &mut Reader<SchemaInput<'_>>,
    ns_map: &HashMap<String, String>,
    nesting: usize,
    complex_type: &mut ComplexType,
) -> Result<()> {
    check_nesting(nesting)?;
    let mut depth = 1i32;
    let mut buf = Vec::new();

    loop {
        match next_tag(reader, &mut buf, "complexType")? {
            Tag::Start { local, attrs } => {
                depth += 1;
                // Anything but an annotation is content, modelled or not.
                if local != "annotation" {
                    complex_type.empty_in_source = false;
                }
                match local.as_str() {
                    "sequence" => {
                        complex_type.content_occurrence = parse_occurrence(&attrs);
                        complex_type.content = ContentModel::Sequence(parse_particles(
                            reader,
                            ns_map,
                            "sequence",
                            nesting + 1,
                        )?);
                        depth -= 1; // parse_particles consumed the End tag
                    }
                    "all" => {
                        complex_type.content_occurrence = parse_occurrence(&attrs);
                        complex_type.content =
                            ContentModel::All(parse_particles(reader, ns_map, "all", nesting + 1)?);
                        depth -= 1;
                    }
                    "choice" => {
                        complex_type.content_occurrence = parse_occurrence(&attrs);
                        complex_type.content = ContentModel::Choice(parse_particles(
                            reader,
                            ns_map,
                            "choice",
                            nesting + 1,
                        )?);
                        depth -= 1;
                    }
                    "simpleContent" | "complexContent" => {
                        parse_content_extension(reader, ns_map, &local, nesting + 1, complex_type)?;
                        depth -= 1;
                    }
                    "attribute" => {
                        // The attribute's children are read here, up to its
                        // end tag, so they are not taken for children of the
                        // complex type.
                        let inline_type = parse_attribute_children(reader, ns_map)?;
                        push_attribute_decl(
                            &attrs,
                            inline_type,
                            ns_map,
                            &mut complex_type.attributes,
                        );
                        depth -= 1;
                    }
                    "annotation" => {
                        skip_element(reader, "annotation")?;
                        depth -= 1;
                    }
                    "attributeGroup" => {
                        push_attribute_group_ref(&attrs, &mut complex_type.attributes);
                        skip_element(reader, "attributeGroup")?;
                        depth -= 1;
                    }
                    _ => {}
                }
            }
            Tag::Empty { local, attrs } => {
                if local != "annotation" {
                    complex_type.empty_in_source = false;
                }
                if local == "attribute" {
                    push_attribute_decl(&attrs, None, ns_map, &mut complex_type.attributes);
                } else if local == "attributeGroup" {
                    push_attribute_group_ref(&attrs, &mut complex_type.attributes);
                }
            }
            Tag::End { .. } => {
                depth -= 1;
                if depth <= 0 {
                    break;
                }
            }
            Tag::Eof => break,
        }
    }
    Ok(())
}

fn parse_particles(
    reader: &mut Reader<SchemaInput<'_>>,
    ns_map: &HashMap<String, String>,
    end_tag: &str,
    depth: usize,
) -> Result<Vec<Particle>> {
    check_nesting(depth)?;
    let mut particles = Vec::new();
    let mut buf = Vec::new();

    loop {
        match next_tag(reader, &mut buf, "particle")? {
            Tag::Start { local, attrs } => match local.as_str() {
                "element" => {
                    if let Some(particle) = reference_particle(&attrs) {
                        particles.push(particle);
                        skip_element(reader, "element")?;
                    } else if attrs.contains_key("name") {
                        let type_def = match declared_type(&attrs, ns_map) {
                            Some(declared) => declared,
                            None => parse_inline_type(reader, ns_map, depth + 1)?,
                        };
                        push_inline_particle(&attrs, *type_def, &mut particles);
                    }
                }
                "sequence" => {
                    let inner = parse_particles(reader, ns_map, "sequence", depth + 1)?;
                    push_nested_sequence(&attrs, inner, end_tag == "choice", &mut particles);
                }
                "choice" => {
                    // A nested choice is a discriminated union; preserve it
                    // as a Group particle rather than flattening its branches
                    // (which would lose the "exactly one of" semantics).
                    let inner = parse_particles(reader, ns_map, "choice", depth + 1)?;
                    push_choice_group(&attrs, inner, &mut particles);
                }
                "any" => {
                    push_wildcard(&attrs, ns_map, &mut particles);
                    skip_element(reader, "any")?;
                }
                other => skip_element(reader, other)?,
            },
            Tag::Empty { local, attrs } => {
                if local == "any" {
                    push_wildcard(&attrs, ns_map, &mut particles);
                } else if local == "element" {
                    if let Some(particle) = reference_particle(&attrs) {
                        particles.push(particle);
                    } else if attrs.contains_key("name") {
                        let type_def =
                            declared_type(&attrs, ns_map).unwrap_or_else(unbounded_string_type);
                        push_inline_particle(&attrs, *type_def, &mut particles);
                    }
                }
            }
            Tag::End { local } => {
                if local == end_tag {
                    break;
                }
            }
            Tag::Eof => break,
        }
    }
    Ok(particles)
}

/// Key under which a schema document's target namespace is kept in its
/// prefix map. It starts with a NUL, so no prefix read from XML is equal.
const TARGET_NAMESPACE_KEY: &str = "\0targetNamespace";

/// Appends the particle for an `<xs:any>` wildcard.
fn push_wildcard(
    attrs: &HashMap<String, String>,
    ns_map: &HashMap<String, String>,
    particles: &mut Vec<Particle>,
) {
    particles.push(Particle {
        occurrence: parse_occurrence(attrs),
        element: ElementRef::Any(Wildcard {
            namespace: attrs
                .get("namespace")
                .cloned()
                .unwrap_or_else(|| "##any".to_owned()),
            process_contents: attrs
                .get("processContents")
                .cloned()
                .unwrap_or_else(|| "strict".to_owned()),
            target_namespace: ns_map.get(TARGET_NAMESPACE_KEY).cloned(),
        }),
    });
}

/// The particle for `<xs:element ref="...">`, or `None` without a `ref`.
fn reference_particle(attrs: &HashMap<String, String>) -> Option<Particle> {
    let ref_name = attrs.get("ref")?;
    let (_, local_ref) = split_prefix(ref_name);
    Some(Particle {
        occurrence: parse_occurrence(attrs),
        element: ElementRef::Ref(QName::local(local_ref)),
    })
}

/// The type an element names in its `type` attribute, or `None` without one.
///
/// A built-in type becomes a simple type; anything else stays a reference to
/// be resolved against the named types of the schema set.
///
/// A name is resolved by its namespace. With a prefix bound to the XML Schema
/// namespace, or without a prefix where XML Schema is the default namespace,
/// a built-in name is the built-in. Without a prefix anywhere else, a name
/// like `short` or `date` is the schema's own type if the schema set declares
/// one, which is only known once every file is loaded: such a reference is
/// marked with [`UNPREFIXED_BUILTIN_NAME`] and settled by
/// [`settle_unprefixed_builtin_names`].
fn declared_type(
    attrs: &HashMap<String, String>,
    ns_map: &HashMap<String, String>,
) -> Option<Box<XsdType>> {
    let type_attr = attrs.get("type")?;
    let (_, type_local) = split_prefix(type_attr);
    Some(Box::new(if !is_xs_primitive(type_attr, ns_map) {
        XsdType::Ref(QName::local(type_local))
    } else if is_unprefixed_builtin_name(type_attr, ns_map) {
        XsdType::Ref(QName {
            namespace: Some(UNPREFIXED_BUILTIN_NAME.to_owned()),
            local: type_local.to_owned(),
        })
    } else {
        builtin_type(type_local)
    }))
}

/// Whether `type_ref` is a built-in name that may mean the schema's own type.
///
/// True for a built-in name written without a prefix where XML Schema is not
/// the default namespace. Which type it means is known once every file is
/// loaded; see [`settle_unprefixed_builtin_names`].
fn is_unprefixed_builtin_name(type_ref: &str, ns_map: &HashMap<String, String>) -> bool {
    let xs_is_default = ns_map
        .get("")
        .is_some_and(|uri| XS_URIS.contains(&uri.as_str()));
    split_prefix(type_ref).0.is_none() && !xs_is_default && is_xs_primitive(type_ref, ns_map)
}

/// The kind of the simple type a `base` or attribute `type` names.
///
/// A name that may mean the schema's own type is kept, marked with
/// [`UNPREFIXED_BUILTIN_NAME`], as a kind for
/// [`settle_unprefixed_builtin_names`] to decide. Any other name goes by its
/// local part: a built-in kind, or the name itself.
fn named_simple_kind(type_ref: &str, ns_map: &HashMap<String, String>) -> SimpleTypeKind {
    let local = split_prefix(type_ref).1;
    if is_unprefixed_builtin_name(type_ref, ns_map) {
        return SimpleTypeKind::Other(format!("{UNPREFIXED_BUILTIN_NAME}{local}"));
    }
    match SimpleTypeKind::from_local(local) {
        // A name outside the XML Schema namespace that is no built-in kind
        // may be the schema set's own simple type, known once every file is
        // loaded. `xs:ID` and the like are built-ins and stay as they are.
        SimpleTypeKind::Other(_) if !names_xs_namespace(type_ref, ns_map) => {
            SimpleTypeKind::Other(format!("{OWN_SIMPLE_TYPE_NAME}{local}"))
        }
        kind => kind,
    }
}

/// Whether `type_ref` is in the XML Schema namespace by its prefix, or by
/// the default namespace when it has none.
fn names_xs_namespace(type_ref: &str, ns_map: &HashMap<String, String>) -> bool {
    let prefix = split_prefix(type_ref).0.unwrap_or("");
    ns_map
        .get(prefix)
        .is_some_and(|uri| XS_URIS.contains(&uri.as_str()))
}

/// Marks a simple type kind, while files are loading, as naming a simple type
/// the schema set may declare itself.
///
/// Like [`UNPREFIXED_BUILTIN_NAME`] it starts with a NUL, which no name read
/// from XML can hold, and no kind keeps it once
/// [`settle_unprefixed_builtin_names`] has run: it becomes the kind of the
/// named type, or the name itself, as it was before simple types were
/// followed, when the schema set declares no simple type of that name.
const OWN_SIMPLE_TYPE_NAME: &str = "\0own-simple-type:";

/// The simple type a marked kind names, and the kind it means when the schema
/// set declares no simple type of that name; `None` for an unmarked kind.
fn marked_reference(kind: &SimpleTypeKind) -> Option<(&str, SimpleTypeKind)> {
    let SimpleTypeKind::Other(marked) = kind else {
        return None;
    };
    if let Some(name) = marked.strip_prefix(UNPREFIXED_BUILTIN_NAME) {
        return Some((name, SimpleTypeKind::from_local(name)));
    }
    marked
        .strip_prefix(OWN_SIMPLE_TYPE_NAME)
        .map(|name| (name, SimpleTypeKind::Other(name.to_owned())))
}

/// The schema set's own types, as the settling pass needs them.
struct OwnTypes {
    /// Names of the simple and complex types that are named like built-ins.
    names: HashSet<String>,
    /// Every simple type as parsed, before any of them is settled.
    simple: HashMap<String, SimpleTypeDef>,
    /// Names of every complex type.
    complex: HashSet<String>,
}

/// Longest chain of own simple types, each restricting the next, that is
/// followed, and deepest nesting of list items and union members settled.
///
/// Real chains are a few links long. The bound, with the check for a name met
/// twice, keeps a cyclic or hostile schema from being followed for ever, and
/// bounds the recursion of [`SimpleKinds::named`]. What lies beyond it keeps
/// the name it was written with, as every name did before chains were
/// followed.
const MAX_OWN_TYPE_CHAIN: usize = 64;

/// Union members as written, and settled.
type SettledMembers = (Arc<Vec<SimpleTypeDef>>, Arc<Vec<SimpleTypeDef>>);

/// The kind each own simple type settles to, worked out once per name, and
/// each own simple type settled, for the types that name it to share.
struct SimpleKinds<'a> {
    own: &'a OwnTypes,
    /// Each named simple type settled so far: `None` while it is being
    /// settled, so a name met again is a cycle and is not followed.
    settled: HashMap<String, Option<Arc<SimpleTypeDef>>>,
    /// Units [`MAX_SETTLE_COPY_UNITS`] still allows.
    remaining: usize,
    /// Each list item type settled so far, by the address of the item as
    /// written, which the entry keeps alive so no other item can take it.
    settled_items: HashMap<usize, (Arc<SimpleTypeDef>, Arc<SimpleTypeDef>)>,
    /// Each set of union members settled so far, keyed as `settled_items`.
    settled_members: HashMap<usize, SettledMembers>,
    /// The settled kind of each name met so far: `None` inside the outer
    /// option while it is being worked out, so a name met again is a cycle;
    /// `None` inside the inner one when the name settles to no kind of its
    /// own (see [`SimpleKinds::named`]).
    kinds: HashMap<String, Option<Option<SimpleTypeKind>>>,
}

impl SimpleKinds<'_> {
    /// The settled form of the own simple type `name`; `None` when there is
    /// no such type, it is being settled (a cycle), or the chain of types
    /// leading here is longer than [`MAX_OWN_TYPE_CHAIN`].
    fn settled_named(&mut self, name: &str, chain: usize) -> Option<Arc<SimpleTypeDef>> {
        if chain >= MAX_OWN_TYPE_CHAIN {
            return None;
        }
        if let Some(known) = self.settled.get(name) {
            return known.clone();
        }
        let own = self.own;
        let (name, written) = own.simple.get_key_value(name)?;
        let mut simple = written.clone();
        self.settled.insert(name.clone(), None);
        settle_simple_type_at(&mut simple, Some(name), self, chain, 0);
        let simple = Arc::new(simple);
        self.settled.insert(name.clone(), Some(Arc::clone(&simple)));
        Some(simple)
    }

    /// Spends `units`; `false`, with nothing spent, when they are not left.
    fn charge(&mut self, units: usize) -> bool {
        match self.remaining.checked_sub(units) {
            Some(remaining) => {
                self.remaining = remaining;
                true
            }
            None => false,
        }
    }

    /// The kind of a simple type as parsed: a union's shared member kind, a
    /// string for a list, the settled kind of the type a marked kind names.
    ///
    /// `None` when the type names an own type that settles to no kind.
    fn of(&mut self, simple: &SimpleTypeDef, depth: usize) -> Option<SimpleTypeKind> {
        if !simple.union_members.is_empty() {
            return Some(self.shared(&simple.union_members, depth));
        }
        if simple.list_item.is_some() {
            return Some(SimpleTypeKind::String);
        }
        let Some((name, fallback)) = marked_reference(&simple.kind) else {
            return Some(simple.kind.clone());
        };
        let builtin = matches!(&simple.kind, SimpleTypeKind::Other(marked)
            if marked.starts_with(UNPREFIXED_BUILTIN_NAME));
        match self.named(name, depth) {
            Some(kind) => Some(kind),
            // A built-in name the chain cannot follow is the built-in.
            None if builtin => Some(fallback),
            None => None,
        }
    }

    /// The settled kind of the own simple type `name`.
    ///
    /// `None` when there is no such type, it leads back to itself, the chain
    /// is longer than [`MAX_OWN_TYPE_CHAIN`], or it ends at a name that is
    /// none of these: the type that names it then keeps the name it was
    /// written with.
    fn named(&mut self, name: &str, depth: usize) -> Option<SimpleTypeKind> {
        if depth >= MAX_OWN_TYPE_CHAIN {
            return None;
        }
        if let Some(known) = self.kinds.get(name) {
            return known.clone().flatten();
        }
        let own = self.own;
        let simple = own.simple.get(name)?;
        self.kinds.insert(name.to_owned(), None);
        let kind = self.of(simple, depth + 1);
        self.kinds.insert(name.to_owned(), Some(kind.clone()));
        kind
    }

    /// The kind every member has, or a string when they differ or one of
    /// them settles to no kind.
    fn shared(&mut self, members: &[SimpleTypeDef], depth: usize) -> SimpleTypeKind {
        let mut shared: Option<SimpleTypeKind> = None;
        for member in members {
            let Some(kind) = self.of(member, depth + 1) else {
                return SimpleTypeKind::String;
            };
            match &shared {
                Some(seen) if *seen != kind => return SimpleTypeKind::String,
                Some(_) => {}
                None => shared = Some(kind),
            }
        }
        shared.unwrap_or(SimpleTypeKind::String)
    }
}

/// Decides a simple type whose kind names another simple type, and the
/// items or members it is built from.
///
/// Where the schema set declares a simple type of the name, the type takes
/// its kind and every facet it does not state itself, following the chain
/// of such types; a restriction of a list or a union takes the items or the
/// members too. Where it does not, or the name leads back to `own_name` (the
/// type being settled), a built-in name is the built-in and any other name
/// stays as written. A union takes the kind its members share. Other types
/// are left alone.
fn settle_simple_type(
    simple: &mut SimpleTypeDef,
    own_name: Option<&str>,
    kinds: &mut SimpleKinds<'_>,
) {
    settle_simple_type_at(simple, own_name, kinds, 0, 0);
}

/// [`settle_simple_type`] for a type nested `depth` lists or unions deep.
///
/// Below the top level, items and members are not taken from a base: their
/// kind is settled, and what they are built from is not copied again.
fn settle_simple_type_at(
    simple: &mut SimpleTypeDef,
    own_name: Option<&str>,
    kinds: &mut SimpleKinds<'_>,
    chain: usize,
    depth: usize,
) {
    let fallback = marked_reference(&simple.kind).map(|(_, fallback)| fallback);
    let kind = match own_name {
        Some(name) if fallback.is_some() => kinds.named(name, 0).or(fallback),
        _ if fallback.is_some() || !simple.union_members.is_empty() => {
            kinds.of(simple, 0).or(fallback)
        }
        _ => None,
    };
    let mut own_items = true;
    let mut own_members = true;
    let base_name = marked_reference(&simple.kind).map(|(name, _)| name.to_owned());
    if let Some(base_name) = base_name.filter(|base| own_name != Some(base.as_str())) {
        if let Some(base) = kinds.settled_named(&base_name, chain + 1) {
            // The base is settled already: what it holds is shared, and
            // copied only where the type adds facets of its own.
            if facets_unset(&simple.facets) {
                simple.facets = Arc::clone(&base.facets);
            } else if !facets_unset(&base.facets)
                && kinds.charge(inherit_cost(&simple.facets, &base.facets))
            {
                inherit_facets(Arc::make_mut(&mut simple.facets), &base.facets);
            }
            if simple.list_item.is_none() && base.list_item.is_some() {
                simple.list_item.clone_from(&base.list_item);
                own_items = false;
            }
            if simple.union_members.is_empty() && !base.union_members.is_empty() {
                simple.union_members = Arc::clone(&base.union_members);
                own_members = false;
            }
        }
    }
    if let Some(kind) = kind {
        simple.kind = kind;
    }
    if depth < MAX_SIMPLE_TYPE_NESTING {
        if let Some(item) = simple.list_item.as_mut().filter(|_| own_items) {
            // Copies of one attribute share one item type: it is settled
            // once, and the copies share the result.
            let key = Arc::as_ptr(item) as usize;
            if let Some((_, settled)) = kinds.settled_items.get(&key) {
                *item = Arc::clone(settled);
            } else {
                let written = Arc::clone(item);
                settle_simple_type_at(Arc::make_mut(item), None, kinds, chain, depth + 1);
                kinds.settled_items.insert(key, (written, Arc::clone(item)));
            }
        }
        if own_members && !simple.union_members.is_empty() {
            let key = Arc::as_ptr(&simple.union_members) as usize;
            if let Some((_, settled)) = kinds.settled_members.get(&key) {
                simple.union_members = Arc::clone(settled);
            } else {
                let written = Arc::clone(&simple.union_members);
                for member in Arc::make_mut(&mut simple.union_members) {
                    settle_simple_type_at(member, None, kinds, chain, depth + 1);
                }
                kinds
                    .settled_members
                    .insert(key, (written, Arc::clone(&simple.union_members)));
            }
        }
    }
}

/// Whether no facet is stated.
fn facets_unset(facets: &SimpleFacets) -> bool {
    facets.is_empty()
        && facets.min_exclusive.is_none()
        && facets.max_exclusive.is_none()
        && facets.total_digits.is_none()
        && facets.fraction_digits.is_none()
}

/// Budget units [`inherit_facets`] spends copying from `base` what `facets`
/// does not state: one per enumeration value, and one per
/// [`COPY_TEXT_BYTES_PER_UNIT`] bytes of copied text.
fn inherit_cost(facets: &SimpleFacets, base: &SimpleFacets) -> usize {
    let mut values = 0usize;
    let mut bytes = 0usize;
    if facets.enumerations.is_empty() {
        values = base.enumerations.len();
        for value in &base.enumerations {
            bytes = bytes.saturating_add(value.value.len());
            bytes = bytes.saturating_add(value.documentation.as_ref().map_or(0, String::len));
        }
    }
    for (own, base) in [
        (&facets.pattern, &base.pattern),
        (&facets.min_inclusive, &base.min_inclusive),
        (&facets.max_inclusive, &base.max_inclusive),
        (&facets.min_exclusive, &base.min_exclusive),
        (&facets.max_exclusive, &base.max_exclusive),
    ] {
        if own.is_none() {
            bytes = bytes.saturating_add(base.as_ref().map_or(0, String::len));
        }
    }
    values
        .saturating_add(1)
        .saturating_add(bytes / COPY_TEXT_BYTES_PER_UNIT)
}

/// Bytes of copied text charged as one unit of [`MAX_SETTLE_COPY_UNITS`]
/// and [`MAX_EXPANDED_NODES`], as inheritance folding charges text.
const COPY_TEXT_BYTES_PER_UNIT: usize = 256;

/// Most that copying facets into types that add facets of their own may
/// spend in one schema set, in units of one enumeration value or
/// [`COPY_TEXT_BYTES_PER_UNIT`] bytes.
///
/// A type that adds no facet of its own shares its base's and copies
/// nothing. One that adds some gets a copy holding both, and a schema can
/// ask for a copy of a large enumeration per such type. Past this bound the
/// type keeps only the facets it states, as every type did before chains of
/// named types were followed. The size is that of the inheritance budgets.
const MAX_SETTLE_COPY_UNITS: usize = 1 << 18;

/// Deepest nesting of list items and union members read and settled.
///
/// A list of a union of a list is already more than XML Schema allows; the
/// bound keeps a hostile schema from recursing without end. A list or union
/// nested deeper is read as a string, as every list and union was before.
const MAX_SIMPLE_TYPE_NESTING: usize = 8;

/// Fills each facet `facets` does not state from `base`.
fn inherit_facets(facets: &mut SimpleFacets, base: &SimpleFacets) {
    if facets.enumerations.is_empty() {
        facets.enumerations.clone_from(&base.enumerations);
    }
    fn inherit<T: Clone>(own: &mut Option<T>, base: &Option<T>) {
        if own.is_none() {
            own.clone_from(base);
        }
    }
    inherit(&mut facets.pattern, &base.pattern);
    inherit(&mut facets.min_length, &base.min_length);
    inherit(&mut facets.max_length, &base.max_length);
    inherit(&mut facets.min_inclusive, &base.min_inclusive);
    inherit(&mut facets.max_inclusive, &base.max_inclusive);
    inherit(&mut facets.min_exclusive, &base.min_exclusive);
    inherit(&mut facets.max_exclusive, &base.max_exclusive);
    inherit(&mut facets.total_digits, &base.total_digits);
    inherit(&mut facets.fraction_digits, &base.fraction_digits);
}

/// Settles a complex type: its attributes and everything in its content.
fn settle_complex_type(complex_type: &mut ComplexType, kinds: &mut SimpleKinds<'_>) {
    // `simpleContent` extending the schema set's own simple type was read as
    // extending a complex type, which no complex type of that name can fold
    // in; the text takes the simple type instead.
    if let (Some(base), Derivation::Extension, ContentModel::Sequence(particles)) = (
        &complex_type.base,
        complex_type.derivation,
        &complex_type.content,
    ) {
        if particles.is_empty()
            && !kinds.own.complex.contains(&base.local)
            && kinds.own.simple.contains_key(&base.local)
        {
            let text = SimpleTypeKind::Other(format!("{OWN_SIMPLE_TYPE_NAME}{}", base.local));
            complex_type.content =
                ContentModel::SimpleContent(Box::new(SimpleTypeDef::of_kind(text)));
        }
    }
    for attribute in &mut complex_type.attributes {
        settle_simple_type(&mut attribute.type_def, None, kinds);
    }
    settle_content(&mut complex_type.content, kinds);
}

/// The built-in simple type called `local`, with no facets.
fn builtin_type(local: &str) -> XsdType {
    XsdType::Simple(SimpleTypeDef::of_kind(SimpleTypeKind::from_local(local)))
}

/// Marks a reference, while files are loading, as an unprefixed built-in name.
///
/// Stored in the reference's namespace, which the parser otherwise leaves
/// unset. It is not a namespace: it starts with a NUL, which no namespace
/// read from XML can hold, and no reference keeps it once
/// [`settle_unprefixed_builtin_names`] has run.
const UNPREFIXED_BUILTIN_NAME: &str = "\0unprefixed-builtin-name";

/// Decides every reference marked with [`UNPREFIXED_BUILTIN_NAME`].
///
/// The reference becomes a plain reference to the schema set's own type of
/// that name when there is one, declared earlier, later or in another file,
/// and the built-in type otherwise. A schema set that declares no type named
/// like a built-in therefore comes out exactly as if this pass did not exist.
fn settle_unprefixed_builtin_names(model: &mut SchemaModel) {
    let is_builtin_name =
        |name: &&String| !matches!(SimpleTypeKind::from_local(name), SimpleTypeKind::Other(_));
    let names: HashSet<String> = model
        .simple_types
        .keys()
        .chain(model.complex_types.keys())
        .filter(is_builtin_name)
        .cloned()
        .collect();
    let simple = model
        .simple_types
        .iter()
        .map(|(name, simple)| (name.clone(), simple.clone()))
        .collect();
    let complex = model.complex_types.keys().cloned().collect();
    let own = OwnTypes {
        names,
        simple,
        complex,
    };
    let mut kinds = SimpleKinds {
        own: &own,
        settled: HashMap::new(),
        remaining: MAX_SETTLE_COPY_UNITS,
        settled_items: HashMap::new(),
        settled_members: HashMap::new(),
        kinds: HashMap::new(),
    };
    for (name, simple) in &mut model.simple_types {
        match kinds.settled_named(name, 0) {
            Some(settled) => *simple = SimpleTypeDef::clone(&settled),
            None => settle_simple_type(simple, Some(name), &mut kinds),
        }
    }
    for element in model.global_elements.values_mut() {
        settle_type(&mut element.type_def, &mut kinds);
    }
    for complex_type in model.complex_types.values_mut() {
        settle_complex_type(complex_type, &mut kinds);
    }
}

/// Settles one declared type, and the types declared inside it.
///
/// The recursion follows the nesting of the model, which the parser bounds.
fn settle_type(declared: &mut XsdType, kinds: &mut SimpleKinds<'_>) {
    let builtin = match declared {
        XsdType::Ref(name) if name.namespace.as_deref() == Some(UNPREFIXED_BUILTIN_NAME) => {
            name.namespace = None;
            (!kinds.own.names.contains(&name.local)).then(|| builtin_type(&name.local))
        }
        XsdType::Complex(complex_type) => {
            settle_complex_type(complex_type, kinds);
            None
        }
        XsdType::Simple(simple) => {
            settle_simple_type(simple, None, kinds);
            None
        }
        XsdType::Ref(_) => None,
    };
    if let Some(builtin) = builtin {
        *declared = builtin;
    }
}

/// Settles the types of the elements declared in one content model.
fn settle_content(content: &mut ContentModel, kinds: &mut SimpleKinds<'_>) {
    if let ContentModel::SimpleContent(simple) = content {
        settle_simple_type(simple, None, kinds);
        return;
    }
    let (ContentModel::Sequence(particles)
    | ContentModel::All(particles)
    | ContentModel::Choice(particles)) = content
    else {
        return;
    };
    for particle in particles {
        match &mut particle.element {
            ElementRef::Inline(element) => settle_type(&mut element.type_def, kinds),
            ElementRef::Group(group) => settle_content(group, kinds),
            ElementRef::Ref(_) | ElementRef::Any(_) => {}
        }
    }
}

/// Appends the particle for a named `<xs:element>` whose type is `type_def`.
fn push_inline_particle(
    attrs: &HashMap<String, String>,
    type_def: XsdType,
    particles: &mut Vec<Particle>,
) {
    let Some(name) = attrs.get("name") else {
        return;
    };
    let occurrence = parse_occurrence(attrs);
    particles.push(Particle {
        occurrence,
        element: ElementRef::Inline(Box::new(ElementDecl {
            name: QName::local(name.as_str()),
            type_def,
            occurrence,
            nillable: attrs.get("nillable").is_some_and(|v| v == "true"),
            documentation: None,
        })),
    });
}

/// Appends a nested `<xs:choice>` as one group particle.
fn push_choice_group(
    attrs: &HashMap<String, String>,
    branches: Vec<Particle>,
    particles: &mut Vec<Particle>,
) {
    particles.push(Particle {
        occurrence: parse_occurrence(attrs),
        element: ElementRef::Group(Box::new(ContentModel::Choice(branches))),
    });
}

/// Appends what a nested `<xs:sequence>` holds.
///
/// A sequence that occurs exactly once is order-equivalent to its particles
/// written in the parent, and is flattened into it. With any other
/// occurrence it is kept as one group particle: an optional sequence may be
/// left out whole, required children included, which its particles written
/// in the parent could not say.
///
/// In a choice (`in_choice`) a sequence is always kept as a group: it is one
/// branch, and written into the choice its particles would read as one
/// branch each, so `(a, b) | (a, c)` could not be told from one `(a, b, a)`
/// branch. Validation still reads such a branch's particles as the choice's
/// own, as it did when the parser wrote them there.
fn push_nested_sequence(
    attrs: &HashMap<String, String>,
    inner: Vec<Particle>,
    in_choice: bool,
    particles: &mut Vec<Particle>,
) {
    let occurrence = parse_occurrence(attrs);
    if occurrence == OccurrenceConstraint::default() && !in_choice {
        particles.extend(inner);
    } else {
        particles.push(Particle {
            occurrence,
            element: ElementRef::Group(Box::new(ContentModel::Sequence(inner))),
        });
    }
}

/// Appends the declaration for an `<xs:attribute>`, if it has a name.
///
/// `inline_type` is the `<xs:simpleType>` written inside the attribute, if any.
fn push_attribute_decl(
    attrs: &HashMap<String, String>,
    inline_type: Option<SimpleTypeDef>,
    ns_map: &HashMap<String, String>,
    attributes: &mut Vec<AttributeDecl>,
) {
    if let Some(attr_decl) = parse_attribute_decl(attrs, inline_type, ns_map) {
        attributes.push(attr_decl);
    } else if let Some(reference) = attrs.get("ref") {
        // The global declaration is known once every file is loaded; the
        // reference keeps what it states itself: `use`, `default`, `fixed`.
        attributes.push(AttributeDecl {
            name: format!("{ATTRIBUTE_REF}{}", split_prefix(reference).1),
            type_def: SimpleTypeDef::string_unbounded(),
            required: attrs.get("use").is_some_and(|u| u == "required"),
            default: attrs.get("default").cloned(),
            fixed: attrs.get("fixed").cloned(),
            referenced: true,
        });
    }
}

/// Reads the children of an `<xs:attribute>` up to and including its end tag.
///
/// Returns the attribute's inline `<xs:simpleType>`, if it has one. Anything
/// else inside the attribute (an annotation) is skipped.
///
/// Kept out of line so the complex-type parser, which recurses once per
/// nested inline type, does not carry this function's locals in its frame.
#[inline(never)]
fn parse_attribute_children(
    reader: &mut Reader<SchemaInput<'_>>,
    ns_map: &HashMap<String, String>,
) -> Result<Option<SimpleTypeDef>> {
    let mut inline_type = None;
    let mut buf = Vec::new();
    loop {
        match next_tag(reader, &mut buf, "attribute")? {
            Tag::Start { local, .. } => match local.as_str() {
                "simpleType" => inline_type = Some(parse_simple_type(reader, ns_map)?),
                other => skip_element(reader, other)?,
            },
            Tag::Empty { .. } => {}
            Tag::End { .. } | Tag::Eof => break,
        }
    }
    Ok(inline_type)
}

/// Parses a `simpleContent`/`complexContent` element into `complex_type`.
///
/// Sets the locally declared content and attributes and, for a derivation
/// from a *complex* base, the base's qualified name so
/// [`SchemaModel::resolve_inheritance`] can fold the base in afterwards:
/// particles and attributes for an extension, attributes only for a
/// `complexContent` restriction, which keeps the particles it states itself,
/// and attributes and the type of the text for a `simpleContent`
/// restriction. The attributes a restriction prohibits are recorded by name
/// and are not attributes of the type. An extension or restriction of a
/// built-in type and a restriction of `xs:anyType` leave the base unset.
///
/// [`SchemaModel::resolve_inheritance`]: crate::schema::SchemaModel::resolve_inheritance
fn parse_content_extension(
    reader: &mut Reader<SchemaInput<'_>>,
    ns_map: &HashMap<String, String>,
    outer_tag: &str,
    depth: usize,
    complex_type: &mut ComplexType,
) -> Result<()> {
    check_nesting(depth)?;
    complex_type.content = ContentModel::Empty;
    complex_type.base = None;
    complex_type.derivation = Derivation::default();
    let mut buf = Vec::new();
    loop {
        match next_tag(reader, &mut buf, "content extension")? {
            Tag::Start { local, attrs } => match local.as_str() {
                "extension" => {
                    let mut particles = Vec::new();
                    parse_derivation_children(
                        reader,
                        ns_map,
                        "extension",
                        depth + 1,
                        &mut particles,
                        &mut complex_type.attributes,
                        None,
                    )?;
                    skip_element(reader, outer_tag)?;
                    if let Some(content) = simple_extension_content(&attrs, ns_map) {
                        complex_type.content = content;
                    } else {
                        // Complex base: record the base name so inheritance
                        // can be resolved later, and keep the locally added
                        // particles.
                        complex_type.content = ContentModel::Sequence(particles);
                        complex_type.base = Some(extension_base(&attrs));
                    }
                    return Ok(());
                }
                "restriction" => {
                    let mut particles = Vec::new();
                    parse_derivation_children(
                        reader,
                        ns_map,
                        "restriction",
                        depth + 1,
                        &mut particles,
                        &mut complex_type.attributes,
                        Some(&mut complex_type.prohibited_attributes),
                    )?;
                    skip_element(reader, outer_tag)?;
                    if outer_tag == "simpleContent" {
                        // The text keeps the kind of a built-in base. The
                        // facets the restriction adds are not read. Any
                        // other base is a complex type with simple content:
                        // it is recorded, and its text and attributes are
                        // folded in afterwards.
                        if let Some(content) = simple_extension_content(&attrs, ns_map) {
                            complex_type.content = content;
                        } else {
                            record_restriction_base(&attrs, ns_map, complex_type);
                        }
                    } else {
                        // A restriction states its particles again, so they
                        // stay as written. Its attributes are the base's,
                        // which is recorded for that.
                        if !particles.is_empty() {
                            complex_type.content = ContentModel::Sequence(particles);
                        }
                        record_restriction_base(&attrs, ns_map, complex_type);
                    }
                    return Ok(());
                }
                other => skip_element(reader, other)?,
            },
            // `<xs:extension base="..."/>` adds nothing to its base, and is
            // read as the open-and-close form with no children is.
            Tag::Empty { local, attrs } => {
                if local == "extension" {
                    if let Some(content) = simple_extension_content(&attrs, ns_map) {
                        complex_type.content = content;
                    } else {
                        complex_type.content = ContentModel::Sequence(Vec::new());
                        complex_type.base = Some(extension_base(&attrs));
                    }
                } else if local == "restriction"
                    && (outer_tag == "complexContent"
                        || simple_extension_content(&attrs, ns_map).is_none())
                {
                    record_restriction_base(&attrs, ns_map, complex_type);
                }
            }
            Tag::End { local } => {
                if local == outer_tag {
                    break;
                }
            }
            Tag::Eof => break,
        }
    }
    Ok(())
}

/// Reads the children of an `<xs:extension>` or `<xs:restriction>` up to and
/// including its end tag, `end_tag`.
///
/// Model groups are appended to `particles` as they are inside a sequence:
/// a sequence or an `xs:all` by its particles (or as a group when it does not
/// occur exactly once), a choice as a group. Attribute declarations are
/// appended to `attributes`. Anything else (annotations, facets, attribute
/// group references, wildcards) is skipped.
///
/// With `prohibited`, which a restriction passes, an attribute declared with
/// `use="prohibited"` is not appended to `attributes`: its name goes to
/// `prohibited` instead.
fn parse_derivation_children(
    reader: &mut Reader<SchemaInput<'_>>,
    ns_map: &HashMap<String, String>,
    end_tag: &str,
    depth: usize,
    particles: &mut Vec<Particle>,
    attributes: &mut Vec<AttributeDecl>,
    mut prohibited: Option<&mut Vec<String>>,
) -> Result<()> {
    check_nesting(depth)?;
    let mut buf = Vec::new();
    loop {
        match next_tag(reader, &mut buf, "derivation")? {
            Tag::Start { local, attrs } => match local.as_str() {
                "sequence" | "all" => {
                    let inner = parse_particles(reader, ns_map, &local, depth + 1)?;
                    push_nested_sequence(&attrs, inner, false, particles);
                }
                "choice" => {
                    let inner = parse_particles(reader, ns_map, "choice", depth + 1)?;
                    push_choice_group(&attrs, inner, particles);
                }
                "attribute" => {
                    let inline_type = parse_attribute_children(reader, ns_map)?;
                    if !push_prohibited(&attrs, prohibited.as_deref_mut()) {
                        push_attribute_decl(&attrs, inline_type, ns_map, attributes);
                    }
                }
                "attributeGroup" => {
                    push_attribute_group_ref(&attrs, attributes);
                    skip_element(reader, "attributeGroup")?;
                }
                other => skip_element(reader, other)?,
            },
            Tag::Empty { local, attrs } => {
                if local == "attribute" && !push_prohibited(&attrs, prohibited.as_deref_mut()) {
                    push_attribute_decl(&attrs, None, ns_map, attributes);
                } else if local == "attributeGroup" {
                    push_attribute_group_ref(&attrs, attributes);
                }
            }
            // Every nested element is read to its end above, so the first
            // end tag met here closes the derivation itself.
            Tag::End { local } => {
                if local == end_tag {
                    break;
                }
            }
            Tag::Eof => break,
        }
    }
    Ok(())
}

/// Records the name of an `<xs:attribute use="prohibited">` in `prohibited`.
///
/// Returns whether the attribute was taken: `false` when it is not
/// prohibited, or when the caller keeps no list of prohibited names.
fn push_prohibited(attrs: &HashMap<String, String>, prohibited: Option<&mut Vec<String>>) -> bool {
    let Some(prohibited) = prohibited else {
        return false;
    };
    if attrs.get("use").is_none_or(|u| u != "prohibited") {
        return false;
    }
    if let Some(name) = attrs.get("name") {
        prohibited.push(name.clone());
    } else if let Some(reference) = attrs.get("ref") {
        prohibited.push(split_prefix(reference).1.to_owned());
    }
    true
}

/// Records the complex base of a restriction.
///
/// A restriction of the built-in `xs:anyType` has no base to take attributes
/// from, and neither has one that names no base.
fn record_restriction_base(
    attrs: &HashMap<String, String>,
    ns_map: &HashMap<String, String>,
    complex_type: &mut ComplexType,
) {
    let base = attrs.get("base").map_or("", String::as_str);
    let (prefix, local) = split_prefix(base);
    let namespace = prefix
        .and_then(|prefix| ns_map.get(prefix))
        .map_or("", String::as_str);
    if local.is_empty() || (local == "anyType" && is_xs_ns(namespace)) {
        return;
    }
    complex_type.base = Some(QName::local(local));
    complex_type.derivation = Derivation::Restriction;
}

/// Local name of the type an `<xs:extension>` derives from; empty without `base`.
fn extension_base(attrs: &HashMap<String, String>) -> QName {
    let base = attrs.get("base").map_or("", String::as_str);
    QName::local(split_prefix(base).1)
}

/// The content of an extension of a built-in type, or `None` for any other base.
fn simple_extension_content(
    attrs: &HashMap<String, String>,
    ns_map: &HashMap<String, String>,
) -> Option<ContentModel> {
    let base = attrs.get("base").map_or("", String::as_str);
    if !is_xs_primitive(base, ns_map) {
        return None;
    }
    Some(ContentModel::SimpleContent(Box::new(
        SimpleTypeDef::of_kind(named_simple_kind(base, ns_map)),
    )))
}

// ── Simple type parser ───────────────────────────────────────────────────────

fn parse_simple_type(
    reader: &mut Reader<SchemaInput<'_>>,
    ns_map: &HashMap<String, String>,
) -> Result<SimpleTypeDef> {
    parse_simple_type_at(reader, ns_map, 0)
}

/// Parses the children of an `<xs:simpleType>` nested `depth` lists or
/// unions deep, up to and including its end tag.
///
/// A restriction gives the kind and the facets. A list gives the type of its
/// items and a union its members, named by `itemType` / `memberTypes` or
/// written inline; past [`MAX_SIMPLE_TYPE_NESTING`] an inline one is skipped.
fn parse_simple_type_at(
    reader: &mut Reader<SchemaInput<'_>>,
    ns_map: &HashMap<String, String>,
    depth: usize,
) -> Result<SimpleTypeDef> {
    let mut simple = SimpleTypeDef::string_unbounded();
    let mut kind = SimpleTypeKind::String;
    let mut facets = SimpleFacets::default();
    let mut buf = Vec::with_capacity(256);

    loop {
        match reader.read_event_into(&mut buf) {
            Ok(Event::Start(ref e)) => {
                let name_bytes = e.name();
                let raw =
                    name_text(name_bytes.as_ref(), Repairs::tag(reader.get_ref(), e)).to_owned();
                let (_, local) = split_prefix(&raw);
                let attrs = collect_attrs(e, Repairs::tag(reader.get_ref(), e));
                match local {
                    "restriction" => {
                        if let Some(base) = attrs.get("base") {
                            kind = named_simple_kind(base, ns_map);
                        }
                        parse_facets(reader, &mut facets)?;
                    }
                    "list" | "union" => {
                        let local = local.to_owned();
                        let named = named_parts(&attrs, &local, ns_map);
                        let inline = parse_simple_type_parts(reader, ns_map, &local, depth)?;
                        set_parts(&mut simple, &local, named.into_iter().chain(inline));
                    }
                    "annotation" => {
                        skip_element(reader, "annotation")?;
                    }
                    other => {
                        let tag = other.to_owned();
                        skip_element(reader, &tag)?;
                    }
                }
            }
            Ok(Event::Empty(ref e)) => {
                let name_bytes = e.name();
                let raw =
                    name_text(name_bytes.as_ref(), Repairs::tag(reader.get_ref(), e)).to_owned();
                let (_, local) = split_prefix(&raw);
                if local == "restriction" {
                    let attrs = collect_attrs(e, Repairs::tag(reader.get_ref(), e));
                    if let Some(base) = attrs.get("base") {
                        kind = named_simple_kind(base, ns_map);
                    }
                } else if local == "list" || local == "union" {
                    let named = named_parts(
                        &collect_attrs(e, Repairs::tag(reader.get_ref(), e)),
                        local,
                        ns_map,
                    );
                    set_parts(&mut simple, local, named);
                }
            }
            Ok(Event::End(ref e)) => {
                let name_bytes = e.name();
                let raw =
                    name_text(name_bytes.as_ref(), Repairs::end(reader.get_ref(), e)).to_owned();
                let (_, local) = split_prefix(&raw);
                if local == "simpleType" {
                    break;
                }
            }
            Ok(Event::Eof) => break,
            Err(e) => return Err(anyhow::anyhow!("simpleType parse error: {e}")),
            _ => {}
        }
        buf.clear();
    }

    simple.kind = kind;
    simple.facets = Arc::new(facets);
    Ok(simple)
}

/// The item type `itemType` names on a list, or the member types
/// `memberTypes` names on a union.
fn named_parts(
    attrs: &HashMap<String, String>,
    local: &str,
    ns_map: &HashMap<String, String>,
) -> Vec<SimpleTypeDef> {
    let attribute = if local == "list" {
        "itemType"
    } else {
        "memberTypes"
    };
    attrs
        .get(attribute)
        .map(|names| {
            names
                .split_whitespace()
                .map(|name| SimpleTypeDef::of_kind(named_simple_kind(name, ns_map)))
                .collect()
        })
        .unwrap_or_default()
}

/// Reads the inline simple types of an `<xs:list>` or `<xs:union>` up to and
/// including its end tag.
fn parse_simple_type_parts(
    reader: &mut Reader<SchemaInput<'_>>,
    ns_map: &HashMap<String, String>,
    end_tag: &str,
    depth: usize,
) -> Result<Vec<SimpleTypeDef>> {
    let mut parts = Vec::new();
    let mut buf = Vec::new();
    loop {
        match next_tag(reader, &mut buf, "simpleType")? {
            Tag::Start { local, .. }
                if local == "simpleType" && depth < MAX_SIMPLE_TYPE_NESTING =>
            {
                parts.push(parse_simple_type_at(reader, ns_map, depth + 1)?);
            }
            Tag::Start { local, .. } => skip_element(reader, &local)?,
            Tag::Empty { local, .. } => {
                if local == "simpleType" {
                    parts.push(SimpleTypeDef::string_unbounded());
                }
            }
            Tag::End { local } => {
                if local == end_tag {
                    break;
                }
            }
            Tag::Eof => break,
        }
    }
    Ok(parts)
}

/// Makes `simple` the list or union `local` names, built from `parts`.
///
/// A list takes its first part as the item type; a union takes every part,
/// in order. A list or union with no parts stays a string.
fn set_parts(
    simple: &mut SimpleTypeDef,
    local: &str,
    parts: impl IntoIterator<Item = SimpleTypeDef>,
) {
    let mut parts = parts.into_iter();
    if local == "list" {
        simple.list_item = parts.next().map(Arc::new);
    } else {
        simple.union_members = Arc::new(parts.collect());
    }
}

fn parse_facets(reader: &mut Reader<SchemaInput<'_>>, facets: &mut SimpleFacets) -> Result<()> {
    let mut buf = Vec::with_capacity(256);
    loop {
        match reader.read_event_into(&mut buf) {
            Ok(Event::Start(ref e)) | Ok(Event::Empty(ref e)) => {
                let name_bytes = e.name();
                let raw =
                    name_text(name_bytes.as_ref(), Repairs::tag(reader.get_ref(), e)).to_owned();
                let (_, local) = split_prefix(&raw);
                let attrs = collect_attrs(e, Repairs::tag(reader.get_ref(), e));
                let value = attrs.get("value").cloned().unwrap_or_default();
                match local {
                    "enumeration" => {
                        facets.enumerations.push(EnumValue {
                            value,
                            documentation: None,
                        });
                    }
                    "pattern" => facets.pattern = Some(value),
                    "minLength" => facets.min_length = value.parse().ok(),
                    "maxLength" => facets.max_length = value.parse().ok(),
                    "minInclusive" => facets.min_inclusive = Some(value),
                    "maxInclusive" => facets.max_inclusive = Some(value),
                    "minExclusive" => facets.min_exclusive = Some(value),
                    "maxExclusive" => facets.max_exclusive = Some(value),
                    "totalDigits" => facets.total_digits = value.parse().ok(),
                    "fractionDigits" => facets.fraction_digits = value.parse().ok(),
                    _ => {}
                }
            }
            Ok(Event::End(ref e)) => {
                let name_bytes = e.name();
                let raw =
                    name_text(name_bytes.as_ref(), Repairs::end(reader.get_ref(), e)).to_owned();
                let (_, local) = split_prefix(&raw);
                if local == "restriction" {
                    break;
                }
            }
            Ok(Event::Eof) => break,
            Err(e) => return Err(anyhow::anyhow!("facet parse error: {e}")),
            _ => {}
        }
        buf.clear();
    }
    Ok(())
}

// ── Attribute declaration parser ─────────────────────────────────────────────

/// Marks an attribute, while files are loading, as a reference to the named
/// attribute group that follows it.
///
/// It starts with a NUL, which no attribute name read from XML can hold, and
/// no attribute keeps it once [`Expansion::expand_model`] has run: it is
/// replaced by the group's attributes, or removed.
const ATTRIBUTE_GROUP_REF: &str = "\0attribute-group:";

/// Appends a placeholder for `<xs:attributeGroup ref="...">`, if it has a `ref`.
fn push_attribute_group_ref(attrs: &HashMap<String, String>, attributes: &mut Vec<AttributeDecl>) {
    let Some(reference) = attrs.get("ref") else {
        return;
    };
    attributes.push(AttributeDecl {
        name: format!("{ATTRIBUTE_GROUP_REF}{}", split_prefix(reference).1),
        type_def: SimpleTypeDef::string_unbounded(),
        required: false,
        default: None,
        fixed: None,
        referenced: true,
    });
}

/// The attribute group an attribute placeholder names, if it is one.
fn attribute_group_ref(attribute: &AttributeDecl) -> Option<&str> {
    attribute.name.strip_prefix(ATTRIBUTE_GROUP_REF)
}

/// Marks an attribute, while files are loading, as a reference to the global
/// attribute that follows it; settled like [`ATTRIBUTE_GROUP_REF`].
const ATTRIBUTE_REF: &str = "\0attribute-ref:";

/// The global attribute an attribute placeholder names, if it is one.
fn attribute_ref(attribute: &AttributeDecl) -> Option<&str> {
    attribute.name.strip_prefix(ATTRIBUTE_REF)
}

/// Whether an attribute is a placeholder for a reference of either kind.
fn is_attribute_placeholder(attribute: &AttributeDecl) -> bool {
    attribute_group_ref(attribute).is_some() || attribute_ref(attribute).is_some()
}

/// Reads the children of a named `<xs:attributeGroup>` up to and including
/// its end tag: its attributes, and placeholders for the groups it names.
///
/// Wildcards and annotations are skipped.
fn parse_attribute_group(
    reader: &mut Reader<SchemaInput<'_>>,
    ns_map: &HashMap<String, String>,
) -> Result<Vec<AttributeDecl>> {
    let mut attributes = Vec::new();
    let mut buf = Vec::new();
    loop {
        match next_tag(reader, &mut buf, "attributeGroup")? {
            Tag::Start { local, attrs } => match local.as_str() {
                "attribute" => {
                    let inline_type = parse_attribute_children(reader, ns_map)?;
                    push_attribute_decl(&attrs, inline_type, ns_map, &mut attributes);
                }
                "attributeGroup" => {
                    push_attribute_group_ref(&attrs, &mut attributes);
                    skip_element(reader, "attributeGroup")?;
                }
                other => skip_element(reader, other)?,
            },
            Tag::Empty { local, attrs } => {
                if local == "attribute" {
                    push_attribute_decl(&attrs, None, ns_map, &mut attributes);
                } else if local == "attributeGroup" {
                    push_attribute_group_ref(&attrs, &mut attributes);
                }
            }
            // Every nested element is read to its end above, so the first
            // end tag met here closes the group itself.
            Tag::End { .. } | Tag::Eof => break,
        }
    }
    Ok(attributes)
}

/// Most attributes and particles that expanding group references may copy
/// in one schema set.
///
/// Every reference receives its own copy of the group, and a group can
/// name other groups, so a small schema can ask for many copies. The bound
/// is the size of each of the inheritance budgets
/// ([`crate::schema::SchemaModel::resolve_inheritance`]), so a fully used
/// expansion holds about as much as one of them. Past it, the references
/// not yet expanded add nothing, without an error: the type keeps what it
/// declares itself, as every type did before groups were expanded.
const MAX_EXPANDED_NODES: usize = 1 << 18;

/// Deepest chain of attribute groups, each naming the next, that is
/// followed. A group met again on the chain adds nothing more.
const MAX_ATTRIBUTE_GROUP_NESTING: usize = 32;

/// The state of expanding the attribute group references of one type.
struct GroupWalk<'a, 'w> {
    /// The groups being walked, outermost first.
    chain: &'w mut Vec<&'a str>,
    /// The groups walked so far for this type.
    visited: &'w mut HashSet<&'a str>,
    /// The attribute names the type has so far.
    seen: &'w mut HashSet<String>,
}

/// What copying one attribute costs [`Expansion`]: a unit, and a unit per
/// [`COPY_TEXT_BYTES_PER_UNIT`] bytes of the text it owns; and those bytes.
/// Its facets are shared with the original, not copied.
fn attribute_copy_cost(attribute: &AttributeDecl) -> (usize, usize) {
    let mut bytes = attribute.name.len();
    for text in [&attribute.default, &attribute.fixed] {
        bytes = bytes.saturating_add(text.as_ref().map_or(0, String::len));
    }
    (
        1usize.saturating_add(bytes / COPY_TEXT_BYTES_PER_UNIT),
        bytes,
    )
}

/// Most bytes of attribute names, defaults and fixed values that expanding
/// attribute references and attribute groups may copy in one schema set.
///
/// Every reference copies its attribute's text, so a long default or fixed
/// value referenced by many types is copied once per type. Real attributes
/// hold a few dozen bytes; 16 MiB is several hundred thousand of them. Past
/// it, an attribute whose text does not fit is left out, without an error.
const MAX_EXPANDED_TEXT_BYTES: usize = 16 * 1024 * 1024;

/// Replaces group references with what the groups hold.
struct Expansion<'a> {
    /// Named attribute groups, as written.
    attribute_groups: &'a HashMap<String, Vec<AttributeDecl>>,
    /// Global attribute declarations.
    global_attributes: &'a HashMap<String, AttributeDecl>,
    /// Attributes and particles that may still be copied.
    remaining: usize,
    /// Bytes of attribute text that may still be copied; see
    /// [`MAX_EXPANDED_TEXT_BYTES`].
    text_remaining: usize,
    /// Attribute groups walked so far, for tests to bound the work.
    walks: usize,
}

impl<'a> Expansion<'a> {
    /// Expands every group reference in the model.
    fn expand_model(&mut self, model: &mut SchemaModel) {
        for complex_type in model.complex_types.values_mut() {
            self.expand_complex_type(complex_type);
        }
        for element in model.global_elements.values_mut() {
            if let XsdType::Complex(complex_type) = &mut element.type_def {
                self.expand_complex_type(complex_type);
            }
        }
    }

    /// Expands the references in one complex type and in the inline types
    /// it holds. The recursion follows the nesting of the model, which the
    /// parser bounds.
    fn expand_complex_type(&mut self, complex_type: &mut ComplexType) {
        if complex_type.attributes.iter().any(is_attribute_placeholder) {
            let attributes = std::mem::take(&mut complex_type.attributes);
            complex_type.attributes = self.expand_attributes(attributes);
        }
        self.expand_content(&mut complex_type.content);
    }

    /// Expands the inline types of the elements in one content model.
    fn expand_content(&mut self, content: &mut ContentModel) {
        let (ContentModel::Sequence(particles)
        | ContentModel::All(particles)
        | ContentModel::Choice(particles)) = content
        else {
            return;
        };
        for particle in particles {
            match &mut particle.element {
                ElementRef::Inline(element) => {
                    if let XsdType::Complex(complex_type) = &mut element.type_def {
                        self.expand_complex_type(complex_type);
                    }
                }
                ElementRef::Group(group) => self.expand_content(group),
                ElementRef::Ref(_) | ElementRef::Any(_) => {}
            }
        }
    }

    /// Charges copying `attribute`; `false`, with nothing charged, when the
    /// budget cannot pay for it. Running out of units ends the expansion;
    /// an attribute whose text does not fit is left out alone.
    fn charge_copy(&mut self, attribute: &AttributeDecl) -> bool {
        let (units, bytes) = attribute_copy_cost(attribute);
        let Some(text_remaining) = self.text_remaining.checked_sub(bytes) else {
            return false;
        };
        let Some(remaining) = self.remaining.checked_sub(units) else {
            self.remaining = 0;
            return false;
        };
        self.remaining = remaining;
        self.text_remaining = text_remaining;
        true
    }

    /// Appends the global attribute a reference names, with the `use`,
    /// `default` and `fixed` the reference states, unless `seen` holds its
    /// name, the attribute is not declared, or the budget is spent.
    fn append_referenced(
        &mut self,
        reference: &AttributeDecl,
        seen: &mut HashSet<String>,
        expanded: &mut Vec<AttributeDecl>,
    ) {
        let Some(name) = attribute_ref(reference) else {
            return;
        };
        let Some(global) = self.global_attributes.get(name) else {
            return;
        };
        if seen.contains(&global.name) {
            return;
        }
        if !self.charge_copy(global) {
            return;
        }
        seen.insert(global.name.clone());
        let mut attribute = global.clone();
        attribute.referenced = true;
        attribute.required = reference.required;
        if reference.default.is_some() {
            attribute.default.clone_from(&reference.default);
        }
        if reference.fixed.is_some() {
            attribute.fixed.clone_from(&reference.fixed);
        }
        expanded.push(attribute);
    }

    /// `attributes` with each group reference replaced by the group's
    /// attributes, nested groups included.
    ///
    /// An attribute whose name the list already has, written in the type
    /// itself or brought in by an earlier group, is not added again.
    fn expand_attributes(&mut self, attributes: Vec<AttributeDecl>) -> Vec<AttributeDecl> {
        let mut seen: HashSet<String> = attributes
            .iter()
            .filter(|attribute| !is_attribute_placeholder(attribute))
            .map(|attribute| attribute.name.clone())
            .collect();
        let mut expanded = Vec::with_capacity(attributes.len());
        let mut chain = Vec::new();
        let mut visited = HashSet::new();
        for attribute in attributes {
            if let Some(group) = attribute_group_ref(&attribute) {
                let mut walk = GroupWalk {
                    chain: &mut chain,
                    visited: &mut visited,
                    seen: &mut seen,
                };
                self.append_attribute_group(group, &mut walk, &mut expanded);
            } else if attribute_ref(&attribute).is_some() {
                self.append_referenced(&attribute, &mut seen, &mut expanded);
            } else {
                expanded.push(attribute);
            }
        }
        expanded
    }

    /// Appends the attributes of the group `name` that `walk.seen` does not
    /// hold.
    ///
    /// Each group is walked at most once per type: a group met again, on
    /// its own chain or through another group, has nothing left to add.
    /// Every walk is charged one unit and every attribute copied its
    /// [`attribute_copy_cost`], so the work is bounded by the budget.
    fn append_attribute_group(
        &mut self,
        name: &str,
        walk: &mut GroupWalk<'a, '_>,
        expanded: &mut Vec<AttributeDecl>,
    ) {
        if walk.chain.len() >= MAX_ATTRIBUTE_GROUP_NESTING || self.remaining == 0 {
            return;
        }
        let groups = self.attribute_groups;
        let Some((name, attributes)) = groups.get_key_value(name) else {
            return;
        };
        if !walk.visited.insert(name.as_str()) {
            return;
        }
        self.remaining -= 1;
        self.walks += 1;
        walk.chain.push(name.as_str());
        for attribute in attributes {
            match attribute_group_ref(attribute) {
                Some(inner) => self.append_attribute_group(inner, walk, expanded),
                None if attribute_ref(attribute).is_some() => {
                    self.append_referenced(attribute, walk.seen, expanded);
                }
                None => {
                    if walk.seen.contains(&attribute.name) {
                        continue;
                    }
                    if !self.charge_copy(attribute) {
                        continue;
                    }
                    walk.seen.insert(attribute.name.clone());
                    let mut attribute = attribute.clone();
                    attribute.referenced = true;
                    expanded.push(attribute);
                }
            }
        }
        walk.chain.pop();
    }
}

/// The declaration an `<xs:attribute>` makes, or `None` without a `name`.
///
/// A `type` attribute decides the type; without one the inline simple type
/// does, and without either the attribute is an unconstrained string.
fn parse_attribute_decl(
    attrs: &HashMap<String, String>,
    inline_type: Option<SimpleTypeDef>,
    ns_map: &HashMap<String, String>,
) -> Option<AttributeDecl> {
    let name = attrs.get("name")?.clone();
    let required = attrs.get("use").is_some_and(|u| u == "required");
    let default = attrs.get("default").cloned();
    let fixed = attrs.get("fixed").cloned();
    let type_def = if let Some(type_attr) = attrs.get("type") {
        SimpleTypeDef::of_kind(named_simple_kind(type_attr, ns_map))
    } else {
        inline_type.unwrap_or_else(SimpleTypeDef::string_unbounded)
    };
    Some(AttributeDecl {
        name,
        type_def,
        required,
        default,
        fixed,
        referenced: false,
    })
}

// ── Helpers ──────────────────────────────────────────────────────────────────

fn split_prefix(name: &str) -> (Option<&str>, &str) {
    match name.find(':') {
        Some(pos) => (Some(&name[..pos]), &name[pos + 1..]),
        None => (None, name),
    }
}

fn is_xs_ns(uri: &str) -> bool {
    XS_URIS.contains(&uri) || uri.is_empty()
}

fn is_xs_primitive(type_ref: &str, ns_map: &HashMap<String, String>) -> bool {
    let (prefix, local) = split_prefix(type_ref);
    let ns = prefix
        .and_then(|p| ns_map.get(p))
        .map(|s| s.as_str())
        .unwrap_or("");
    if !ns.is_empty() && !is_xs_ns(ns) {
        return false;
    }
    !matches!(SimpleTypeKind::from_local(local), SimpleTypeKind::Other(_))
}

/// The value of one attribute as an XML processor reports it.
///
/// Entity and character references are resolved, and the value is normalized
/// as the XML specification requires of every attribute value: a literal tab,
/// newline or carriage return (a CR LF pair counting once) becomes one space.
/// A tab or newline written as a character reference (`&#9;`, `&#10;`) is
/// kept. The schema documents read here are XML 1.0, whose rules
/// [`XmlVersion::Implicit1_0`] selects.
///
/// # Errors
///
/// Fails on a value that is not UTF-8, names an entity that is not predefined,
/// holds a character reference to no character (`&#0;`) or holds a bare `&`.
/// Callers that collect attributes drop such an attribute.
fn attribute_value<'a>(
    attribute: &Attribute<'a>,
) -> Result<std::borrow::Cow<'a, str>, quick_xml::Error> {
    attribute.normalized_value(XmlVersion::Implicit1_0)
}

/// The attributes of `e` by name; one whose name or value is not UTF-8 is
/// left out.
fn collect_attrs(
    e: &quick_xml::events::BytesStart<'_>,
    repairs: Repairs<'_>,
) -> HashMap<String, String> {
    e.attributes()
        .with_checks(false)
        .flatten()
        .filter_map(|attr| {
            let key: &str = attr.key.as_ref();
            let val = attribute_value(&attr).ok()?;
            if repairs.not_utf8(key) || repairs.not_utf8(&attr.value) {
                return None;
            }
            Some((key.to_string(), val.into_owned()))
        })
        .collect()
}

fn schema_location(
    e: &quick_xml::events::BytesStart<'_>,
    max_bytes: usize,
    repairs: Repairs<'_>,
) -> Result<Option<String>> {
    for attribute in e.attributes().with_checks(false) {
        let attribute =
            attribute.map_err(|error| anyhow::anyhow!("XML attribute error: {error}"))?;
        if attribute.key.as_ref() != "schemaLocation" {
            continue;
        }
        let observed = attribute.value.len();
        if observed > max_bytes {
            return Err(anyhow::anyhow!(
                "resource_limit: schema_location_bytes: observed {observed} exceeds limit {max_bytes}"
            ));
        }
        let value = attribute_value(&attribute)
            .map_err(|error| anyhow::anyhow!("invalid schemaLocation: {error}"))?;
        if repairs.not_utf8(&attribute.value) {
            return Err(anyhow::anyhow!("invalid schemaLocation: not UTF-8"));
        }
        return Ok(Some(value.into_owned()));
    }
    Ok(None)
}

fn collect_xmlns(
    e: &quick_xml::events::BytesStart<'_>,
    ns_map: &mut HashMap<String, String>,
    repairs: Repairs<'_>,
) {
    for attr in e.attributes().with_checks(false).flatten() {
        let key: &str = attr.key.as_ref();
        if repairs.not_utf8(key) || repairs.not_utf8(&attr.value) {
            continue;
        }
        let val = match attribute_value(&attr) {
            Ok(v) => v.into_owned(),
            _ => continue,
        };
        if key == "xmlns" {
            ns_map.insert(String::new(), val);
        } else if let Some(prefix) = key.strip_prefix("xmlns:") {
            ns_map.insert(prefix.to_string(), val);
        }
    }
}

fn parse_occurrence(attrs: &HashMap<String, String>) -> OccurrenceConstraint {
    let min = attrs
        .get("minOccurs")
        .and_then(|v| v.parse().ok())
        .unwrap_or(1u32);
    let max = match attrs.get("maxOccurs").map(|s| s.as_str()) {
        Some("unbounded") => None,
        Some(v) => v.parse().ok(),
        None => Some(1),
    };
    OccurrenceConstraint { min, max }
}

fn skip_element(reader: &mut Reader<SchemaInput<'_>>, tag: &str) -> Result<()> {
    let mut depth = 1i32;
    let mut buf = Vec::with_capacity(256);
    loop {
        match reader.read_event_into(&mut buf) {
            Ok(Event::Start(_)) => depth += 1,
            Ok(Event::End(ref e)) => {
                depth -= 1;
                if depth <= 0 {
                    let name_bytes = e.name();
                    let raw = name_text(name_bytes.as_ref(), Repairs::end(reader.get_ref(), e))
                        .to_owned();
                    let (_, local) = split_prefix(&raw);
                    if local == tag || depth < 0 {
                        break;
                    }
                }
            }
            Ok(Event::Eof) => break,
            Err(e) => return Err(anyhow::anyhow!("skip error: {e}")),
            _ => {}
        }
        buf.clear();
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Attribute groups walked to expand a chain of `depth` groups, each
    /// naming the next `fan_out` times, referenced from one type.
    fn walks_for_fan_out(depth: usize, fan_out: usize) -> usize {
        let mut schema = String::from(
            r#"<xs:schema xmlns:xs="http://www.w3.org/2001/XMLSchema" xmlns:t="urn:t" targetNamespace="urn:t">"#,
        );
        for level in 0..depth {
            schema.push_str(&format!(r#"<xs:attributeGroup name="g{level}">"#));
            for _ in 0..fan_out {
                schema.push_str(&format!(r#"<xs:attributeGroup ref="t:g{}"/>"#, level + 1));
            }
            schema.push_str(&format!(
                r#"<xs:attribute name="a{level}"/></xs:attributeGroup>"#
            ));
        }
        schema.push_str(&format!(r#"<xs:attributeGroup name="g{depth}"/>"#));
        schema.push_str(
            r#"<xs:complexType name="T"><xs:attributeGroup ref="t:g0"/></xs:complexType></xs:schema>"#,
        );
        let options = ParseOptions::default();
        let mut loader = Loader::new(None, &options);
        loader
            .parse_bytes(schema.as_bytes())
            .expect("schema parses");
        let mut expansion = Expansion {
            attribute_groups: &loader.attribute_groups,
            global_attributes: &loader.global_attributes,
            remaining: MAX_EXPANDED_NODES,
            text_remaining: MAX_EXPANDED_TEXT_BYTES,
            walks: 0,
        };
        let mut model = std::mem::take(&mut loader.model);
        expansion.expand_model(&mut model);
        // Only the first 32 groups of the chain are followed.
        let expected = depth.min(MAX_ATTRIBUTE_GROUP_NESTING);
        assert_eq!(model.complex_types["T"].attributes.len(), expected);
        expansion.walks
    }

    #[test]
    fn each_attribute_group_is_walked_once_per_type() {
        for fan_out in [2, 3] {
            let walks = walks_for_fan_out(31, fan_out);
            assert!(walks <= 32, "fan-out {fan_out}: {walks} walks");
        }
    }
}
