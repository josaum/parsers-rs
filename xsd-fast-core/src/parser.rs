//! XSD file parser.
//!
//! Parses one or more XSD files (following `xs:include` / `xs:import`) into a
//! [`SchemaModel`]. Uses `quick-xml` for event parsing and `memmap2` for cheap
//! file reads.
//!
//! Each recursive parsing function allocates its own `Vec<u8>` read buffer so
//! that borrow lifetimes never conflict across nested calls, and reads element
//! boundaries as owned [`Tag`] values so its stack frame stays small.

use std::collections::{HashMap, HashSet};
use std::path::{Component, Path, PathBuf};

use anyhow::{Context, Result};
use memmap2::Mmap;
use quick_xml::events::attributes::Attribute;
use quick_xml::events::Event;
use quick_xml::{Reader, XmlVersion};

use crate::schema::{
    AttributeDecl, ComplexType, ContentModel, ElementDecl, ElementRef, EnumValue,
    OccurrenceConstraint, Particle, QName, SchemaModel, SimpleFacets, SimpleTypeDef,
    SimpleTypeKind, XsdType,
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
        // Safety: file is not modified while mapped; XSD files are small.
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
        let mut reader = Reader::from_reader(bytes);
        // No reader of a schema document collects element text: declarations
        // are read from attributes alone, and annotations are skipped. So
        // trimming, which works on each piece of text between references,
        // cannot change a value here.
        reader.config_mut().trim_text(true);
        // For the same reason a bare `&` in that text, which is not
        // well-formed, is passed over and does not fail the load: published
        // schemas carry them in documentation.
        reader.config_mut().allow_dangling_amp = true;

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
                Ok(Event::Empty(ref e)) => self.top_level_empty(e, &mut file)?,
                Ok(Event::End(ref e)) => {
                    if local_name(e.name().as_ref()) == "schema" {
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
        reader: &mut Reader<&[u8]>,
        file: &mut FileState,
    ) -> Result<Option<PathBuf>> {
        // Collect xmlns declarations.
        collect_xmlns(e, &mut file.ns_map);
        let local = local_name(e.name().as_ref());

        match local.as_str() {
            "schema" => {
                // Extract targetNamespace.
                let attrs = collect_attrs(e);
                if let Some(tns) = attrs.get("targetNamespace") {
                    self.model.target_namespace = Some(tns.clone());
                }
                file.in_schema = true;
            }
            "element" if file.in_schema => {
                let attrs = collect_attrs(e);
                // Top-level element: parse its children.
                let decl = parse_element_start(&attrs, reader, &file.ns_map, 0)?;
                if let Some(decl) = decl {
                    self.model
                        .global_elements
                        .insert(decl.name.local.clone(), decl);
                }
            }
            "complexType" if file.in_schema => {
                let attrs = collect_attrs(e);
                let name = attrs.get("name").cloned();
                let ct = parse_complex_type(reader, &file.ns_map, 0)?;
                if let Some(n) = name {
                    self.model.complex_types.insert(n, ct);
                }
            }
            "simpleType" if file.in_schema => {
                let attrs = collect_attrs(e);
                let name = attrs.get("name").cloned();
                let mut st = parse_simple_type(reader, &file.ns_map)?;
                if let Some(n) = &name {
                    st.name = Some(QName::local(n));
                    self.model.simple_types.insert(n.clone(), st);
                }
            }
            "include" | "import" => return self.include_target(e),
            _ => {}
        }
        Ok(None)
    }

    /// Handles one top-level self-closing tag; returns the file an include names.
    fn top_level_empty(
        &mut self,
        e: &quick_xml::events::BytesStart<'_>,
        file: &mut FileState,
    ) -> Result<Option<PathBuf>> {
        collect_xmlns(e, &mut file.ns_map);
        let local = local_name(e.name().as_ref());

        match local.as_str() {
            "include" | "import" => return self.include_target(e),
            // A self-closing named type declares a type with nothing in it:
            // no content and no attributes, or no restriction.
            "complexType" if file.in_schema => {
                if let Some(name) = collect_attrs(e).remove("name") {
                    self.model.complex_types.insert(name, empty_complex_type());
                }
            }
            "simpleType" if file.in_schema => {
                if let Some(name) = collect_attrs(e).remove("name") {
                    let mut simple = SimpleTypeDef::string_unbounded();
                    simple.name = Some(QName::local(name.as_str()));
                    self.model.simple_types.insert(name, simple);
                }
            }
            "element" if file.in_schema => {
                let attrs = collect_attrs(e);
                // Self-closing top-level element with type= attribute.
                if let Ok(Some(decl)) = parse_element_start(
                    &attrs,
                    // No children to parse since it's empty — pass a stub reader.
                    &mut Reader::from_reader(b"" as &[u8]),
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
    fn include_target(&self, e: &quick_xml::events::BytesStart<'_>) -> Result<Option<PathBuf>> {
        match schema_location(e, self.max_schema_location_bytes)? {
            Some(location) => self.resolve_schema_location(&location).map(Some),
            None => Ok(None),
        }
    }

    /// The loaded model, with every type reference settled.
    fn finish(mut self) -> SchemaModel {
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
    reader: &mut Reader<&[u8]>,
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
fn next_tag(reader: &mut Reader<&[u8]>, buf: &mut Vec<u8>, context: &str) -> Result<Tag> {
    loop {
        buf.clear();
        match reader.read_event_into(buf) {
            Ok(Event::Start(ref e)) => {
                return Ok(Tag::Start {
                    local: local_name(e.name().as_ref()),
                    attrs: collect_attrs(e),
                });
            }
            Ok(Event::Empty(ref e)) => {
                return Ok(Tag::Empty {
                    local: local_name(e.name().as_ref()),
                    attrs: collect_attrs(e),
                });
            }
            Ok(Event::End(ref e)) => {
                return Ok(Tag::End {
                    local: local_name(e.name().as_ref()),
                });
            }
            Ok(Event::Eof) => return Ok(Tag::Eof),
            Err(e) => return Err(anyhow::anyhow!("{context} parse error: {e}")),
            _ => {}
        }
    }
}

/// Local part of a raw element name; empty when the name is not UTF-8.
fn local_name(raw: &[u8]) -> String {
    let raw = std::str::from_utf8(raw).unwrap_or("");
    split_prefix(raw).1.to_owned()
}

fn parse_inline_type(
    reader: &mut Reader<&[u8]>,
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
    reader: &mut Reader<&[u8]>,
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

/// A complex type with no content, no attributes and no base.
fn empty_complex_type() -> ComplexType {
    ComplexType {
        name: None,
        content: ContentModel::Empty,
        attributes: Vec::new(),
        base: None,
        content_occurrence: OccurrenceConstraint::default(),
    }
}

// ── Complex type parser ──────────────────────────────────────────────────────

fn parse_complex_type(
    reader: &mut Reader<&[u8]>,
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
    reader: &mut Reader<&[u8]>,
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
                    _ => {}
                }
            }
            Tag::Empty { local, attrs } => {
                if local == "attribute" {
                    push_attribute_decl(&attrs, None, ns_map, &mut complex_type.attributes);
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
    reader: &mut Reader<&[u8]>,
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
                        push_inline_particle(&attrs, type_def, &mut particles);
                    }
                }
                "sequence" => {
                    // A nested sequence is order-equivalent to inlining its
                    // particles, so flatten it into the parent.
                    let inner = parse_particles(reader, ns_map, "sequence", depth + 1)?;
                    particles.extend(inner);
                }
                "choice" => {
                    // A nested choice is a discriminated union; preserve it
                    // as a Group particle rather than flattening its branches
                    // (which would lose the "exactly one of" semantics).
                    let inner = parse_particles(reader, ns_map, "choice", depth + 1)?;
                    push_choice_group(&attrs, inner, &mut particles);
                }
                other => skip_element(reader, other)?,
            },
            Tag::Empty { local, attrs } => {
                if local == "element" {
                    if let Some(particle) = reference_particle(&attrs) {
                        particles.push(particle);
                    } else if attrs.contains_key("name") {
                        let type_def =
                            declared_type(&attrs, ns_map).unwrap_or_else(unbounded_string_type);
                        push_inline_particle(&attrs, type_def, &mut particles);
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
        SimpleTypeKind::Other(format!("{UNPREFIXED_BUILTIN_NAME}{local}"))
    } else {
        SimpleTypeKind::from_local(local)
    }
}

/// The schema set's own types that are named like built-ins.
struct OwnTypes {
    /// Names of the simple and complex types.
    names: HashSet<String>,
    /// The simple types as parsed, before any of them is settled.
    simple: HashMap<String, SimpleTypeDef>,
}

/// Longest chain of own types, each restricting the next, that is followed.
///
/// Real chains are one or two long. The bound, with the check for a name met
/// twice, keeps a cyclic schema from being followed for ever.
const MAX_OWN_TYPE_CHAIN: usize = 8;

/// Decides a simple type whose kind is a marked built-in name.
///
/// Where the schema set declares a simple type of that name, the type takes
/// its kind and every facet it does not state itself, following a chain of
/// such types. Where it does not, or the name leads back to `own_name` (the
/// type being settled), the name is the built-in. Other types are left alone.
fn settle_simple_type(simple: &mut SimpleTypeDef, own_name: Option<&str>, own: &OwnTypes) {
    let SimpleTypeKind::Other(marked) = &simple.kind else {
        return;
    };
    let Some(first) = marked.strip_prefix(UNPREFIXED_BUILTIN_NAME) else {
        return;
    };
    let mut name = first.to_owned();
    let mut seen: Vec<String> = own_name.map(str::to_owned).into_iter().collect();
    for _ in 0..MAX_OWN_TYPE_CHAIN {
        let Some(base) = own.simple.get(&name).filter(|_| !seen.contains(&name)) else {
            break;
        };
        inherit_facets(&mut simple.facets, &base.facets);
        seen.push(name);
        match &base.kind {
            SimpleTypeKind::Other(next) if next.starts_with(UNPREFIXED_BUILTIN_NAME) => {
                name = next[UNPREFIXED_BUILTIN_NAME.len()..].to_owned();
            }
            kind => {
                simple.kind = kind.clone();
                return;
            }
        }
    }
    simple.kind = SimpleTypeKind::from_local(&name);
}

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
fn settle_complex_type(complex_type: &mut ComplexType, own: &OwnTypes) {
    for attribute in &mut complex_type.attributes {
        settle_simple_type(&mut attribute.type_def, None, own);
    }
    settle_content(&mut complex_type.content, own);
}

/// The built-in simple type called `local`, with no facets.
fn builtin_type(local: &str) -> XsdType {
    XsdType::Simple(SimpleTypeDef {
        name: None,
        kind: SimpleTypeKind::from_local(local),
        facets: SimpleFacets::default(),
    })
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
        .filter(|(name, _)| names.contains(*name))
        .map(|(name, simple)| (name.clone(), simple.clone()))
        .collect();
    let own = OwnTypes { names, simple };
    for (name, simple) in &mut model.simple_types {
        settle_simple_type(simple, Some(name), &own);
    }
    for element in model.global_elements.values_mut() {
        settle_type(&mut element.type_def, &own);
    }
    for complex_type in model.complex_types.values_mut() {
        settle_complex_type(complex_type, &own);
    }
}

/// Settles one declared type, and the types declared inside it.
///
/// The recursion follows the nesting of the model, which the parser bounds.
fn settle_type(declared: &mut XsdType, own: &OwnTypes) {
    let builtin = match declared {
        XsdType::Ref(name) if name.namespace.as_deref() == Some(UNPREFIXED_BUILTIN_NAME) => {
            name.namespace = None;
            (!own.names.contains(&name.local)).then(|| builtin_type(&name.local))
        }
        XsdType::Complex(complex_type) => {
            settle_complex_type(complex_type, own);
            None
        }
        XsdType::Simple(simple) => {
            settle_simple_type(simple, None, own);
            None
        }
        XsdType::Ref(_) => None,
    };
    if let Some(builtin) = builtin {
        *declared = builtin;
    }
}

/// Settles the types of the elements declared in one content model.
fn settle_content(content: &mut ContentModel, own: &OwnTypes) {
    if let ContentModel::SimpleContent(simple) = content {
        settle_simple_type(simple, None, own);
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
            ElementRef::Inline(element) => settle_type(&mut element.type_def, own),
            ElementRef::Group(group) => settle_content(group, own),
            ElementRef::Ref(_) => {}
        }
    }
}

/// Appends the particle for a named `<xs:element>` whose type is `type_def`.
fn push_inline_particle(
    attrs: &HashMap<String, String>,
    type_def: Box<XsdType>,
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
            type_def: *type_def,
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
    reader: &mut Reader<&[u8]>,
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
/// Sets the locally declared content and, for an extension against a
/// *complex* base, the base's qualified name so
/// [`SchemaModel::resolve_inheritance`] can fold the inherited particles and
/// attributes in afterwards. Simple-content and unknown extensions leave the
/// base unset.
///
/// [`SchemaModel::resolve_inheritance`]: crate::schema::SchemaModel::resolve_inheritance
fn parse_content_extension(
    reader: &mut Reader<&[u8]>,
    ns_map: &HashMap<String, String>,
    outer_tag: &str,
    depth: usize,
    complex_type: &mut ComplexType,
) -> Result<()> {
    check_nesting(depth)?;
    complex_type.content = ContentModel::Empty;
    complex_type.base = None;
    let mut buf = Vec::new();
    loop {
        match next_tag(reader, &mut buf, "content extension")? {
            Tag::Start { local, attrs } => match local.as_str() {
                "extension" => {
                    if let Some(content) = simple_extension_content(&attrs, ns_map) {
                        skip_element(reader, "extension")?;
                        skip_element(reader, outer_tag)?;
                        complex_type.content = content;
                        return Ok(());
                    }
                    // Complex base: record the base name so inheritance can
                    // be resolved later, and keep the locally added particles.
                    let particles = parse_particles(reader, ns_map, "extension", depth + 1)?;
                    skip_element(reader, outer_tag)?;
                    complex_type.content = ContentModel::Sequence(particles);
                    complex_type.base = Some(extension_base(&attrs));
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
    Some(ContentModel::SimpleContent(Box::new(SimpleTypeDef {
        name: None,
        kind: named_simple_kind(base, ns_map),
        facets: SimpleFacets::default(),
    })))
}

// ── Simple type parser ───────────────────────────────────────────────────────

fn parse_simple_type(
    reader: &mut Reader<&[u8]>,
    ns_map: &HashMap<String, String>,
) -> Result<SimpleTypeDef> {
    let mut kind = SimpleTypeKind::String;
    let mut facets = SimpleFacets::default();
    let mut buf = Vec::with_capacity(256);

    loop {
        match reader.read_event_into(&mut buf) {
            Ok(Event::Start(ref e)) => {
                let name_bytes = e.name();
                let raw = std::str::from_utf8(name_bytes.as_ref())
                    .unwrap_or("")
                    .to_owned();
                let (_, local) = split_prefix(&raw);
                let attrs = collect_attrs(e);
                match local {
                    "restriction" => {
                        if let Some(base) = attrs.get("base") {
                            kind = named_simple_kind(base, ns_map);
                        }
                        parse_facets(reader, &mut facets)?;
                    }
                    "list" | "union" => {
                        skip_element(reader, local)?;
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
                let raw = std::str::from_utf8(name_bytes.as_ref())
                    .unwrap_or("")
                    .to_owned();
                let (_, local) = split_prefix(&raw);
                if local == "restriction" {
                    let attrs = collect_attrs(e);
                    if let Some(base) = attrs.get("base") {
                        kind = named_simple_kind(base, ns_map);
                    }
                }
            }
            Ok(Event::End(ref e)) => {
                let name_bytes = e.name();
                let raw = std::str::from_utf8(name_bytes.as_ref())
                    .unwrap_or("")
                    .to_owned();
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

    Ok(SimpleTypeDef {
        name: None,
        kind,
        facets,
    })
}

fn parse_facets(reader: &mut Reader<&[u8]>, facets: &mut SimpleFacets) -> Result<()> {
    let mut buf = Vec::with_capacity(256);
    loop {
        match reader.read_event_into(&mut buf) {
            Ok(Event::Start(ref e)) | Ok(Event::Empty(ref e)) => {
                let name_bytes = e.name();
                let raw = std::str::from_utf8(name_bytes.as_ref())
                    .unwrap_or("")
                    .to_owned();
                let (_, local) = split_prefix(&raw);
                let attrs = collect_attrs(e);
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
                let raw = std::str::from_utf8(name_bytes.as_ref())
                    .unwrap_or("")
                    .to_owned();
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
        SimpleTypeDef {
            name: None,
            kind: named_simple_kind(type_attr, ns_map),
            facets: SimpleFacets::default(),
        }
    } else {
        inline_type.unwrap_or_else(SimpleTypeDef::string_unbounded)
    };
    Some(AttributeDecl {
        name,
        type_def,
        required,
        default,
        fixed,
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

fn collect_attrs(e: &quick_xml::events::BytesStart<'_>) -> HashMap<String, String> {
    e.attributes()
        .with_checks(false)
        .flatten()
        .filter_map(|attr| {
            let key = std::str::from_utf8(attr.key.as_ref()).ok()?.to_string();
            let val = attribute_value(&attr).ok()?.into_owned();
            Some((key, val))
        })
        .collect()
}

fn schema_location(
    e: &quick_xml::events::BytesStart<'_>,
    max_bytes: usize,
) -> Result<Option<String>> {
    for attribute in e.attributes().with_checks(false) {
        let attribute =
            attribute.map_err(|error| anyhow::anyhow!("XML attribute error: {error}"))?;
        if attribute.key.as_ref() != b"schemaLocation" {
            continue;
        }
        let observed = attribute.value.len();
        if observed > max_bytes {
            return Err(anyhow::anyhow!(
                "resource_limit: schema_location_bytes: observed {observed} exceeds limit {max_bytes}"
            ));
        }
        return attribute_value(&attribute)
            .map(|value| Some(value.into_owned()))
            .map_err(|error| anyhow::anyhow!("invalid schemaLocation: {error}"));
    }
    Ok(None)
}

fn collect_xmlns(e: &quick_xml::events::BytesStart<'_>, ns_map: &mut HashMap<String, String>) {
    for attr in e.attributes().with_checks(false).flatten() {
        let key = match std::str::from_utf8(attr.key.as_ref()) {
            Ok(k) => k.to_string(),
            Err(_) => continue,
        };
        let val = match attribute_value(&attr) {
            Ok(v) => v.into_owned(),
            Err(_) => continue,
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

fn skip_element(reader: &mut Reader<&[u8]>, tag: &str) -> Result<()> {
    let mut depth = 1i32;
    let mut buf = Vec::with_capacity(256);
    loop {
        match reader.read_event_into(&mut buf) {
            Ok(Event::Start(_)) => depth += 1,
            Ok(Event::End(ref e)) => {
                depth -= 1;
                if depth <= 0 {
                    let name_bytes = e.name();
                    let raw = std::str::from_utf8(name_bytes.as_ref())
                        .unwrap_or("")
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
