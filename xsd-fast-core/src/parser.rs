//! XSD file parser.
//!
//! Parses one or more XSD files (following `xs:include` / `xs:import`) into a
//! [`SchemaModel`]. Uses `quick-xml` for event parsing and `memmap2` for cheap
//! file reads.
//!
//! Each recursive parsing function allocates its own `Vec<u8>` read buffer so
//! that borrow lifetimes never conflict across nested calls.

use std::collections::{HashMap, HashSet};
use std::path::{Component, Path, PathBuf};

use anyhow::{Context, Result};
use memmap2::Mmap;
use quick_xml::events::Event;
use quick_xml::Reader;

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
        model.resolve_inheritance();
        Ok(Self { model, options })
    }

    /// Load a schema from an in-memory XSD byte slice (no `xs:include` resolution).
    pub fn load_bytes(bytes: &[u8], options: ParseOptions) -> Result<Self> {
        let mut loader = Loader::new(None, &options);
        loader.charge_schema_bytes(bytes.len())?;
        loader.parse_bytes(bytes)?;
        let mut model = loader.finish();
        model.resolve_inheritance();
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
        }
    }

    fn load_root_file(&mut self, path: &Path) -> Result<()> {
        self.load_file(path, false)
    }

    fn load_include_file(&mut self, path: &Path) -> Result<()> {
        self.load_file(path, true)
    }

    fn load_file(&mut self, path: &Path, is_include: bool) -> Result<()> {
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
            return Ok(());
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
        self.visited.insert(canon.clone());
        let parent = canon.parent().unwrap_or(Path::new(".")).to_path_buf();
        let prev_dir = std::mem::replace(&mut self.base_dir, parent);
        let result = self.parse_bytes(&mmap);
        self.base_dir = prev_dir;
        result
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

    fn resolve_schema_location(&self, location: &str) -> Result<PathBuf> {
        let path = Path::new(location);
        if location.contains("://")
            || path.is_absolute()
            || path.components().any(|component| {
                matches!(
                    component,
                    Component::ParentDir | Component::RootDir | Component::Prefix(_)
                )
            })
        {
            return Err(anyhow::anyhow!("rejected schemaLocation: {location}"));
        }
        if self.root_dir.is_none() {
            return Err(anyhow::anyhow!("rejected schemaLocation: {location}"));
        }
        Ok(self.base_dir.join(path))
    }

    fn parse_bytes(&mut self, bytes: &[u8]) -> Result<()> {
        let mut reader = Reader::from_reader(bytes);
        reader.config_mut().trim_text(true);

        // namespace prefix → URI map for this file
        let mut ns_map: HashMap<String, String> = HashMap::new();
        // track whether we've entered <xs:schema>
        let mut in_schema = false;

        let mut buf = Vec::with_capacity(4096);

        loop {
            match reader.read_event_into(&mut buf) {
                Ok(Event::Start(ref e)) => {
                    // Collect xmlns declarations.
                    collect_xmlns(e, &mut ns_map);
                    let name_bytes = e.name();
                    let raw = std::str::from_utf8(name_bytes.as_ref())
                        .unwrap_or("")
                        .to_owned();
                    let (_, local) = split_prefix(&raw);

                    match local {
                        "schema" => {
                            // Extract targetNamespace.
                            let attrs = collect_attrs(e);
                            if let Some(tns) = attrs.get("targetNamespace") {
                                self.model.target_namespace = Some(tns.clone());
                            }
                            in_schema = true;
                        }
                        "element" if in_schema => {
                            let attrs = collect_attrs(e);
                            // Top-level element: parse its children.
                            let ns_map_clone = ns_map.clone();
                            let decl = parse_element_start(&attrs, &mut reader, &ns_map_clone)?;
                            if let Some(decl) = decl {
                                self.model
                                    .global_elements
                                    .insert(decl.name.local.clone(), decl);
                            }
                        }
                        "complexType" if in_schema => {
                            let attrs = collect_attrs(e);
                            let name = attrs.get("name").cloned();
                            let ns_map_clone = ns_map.clone();
                            let ct = parse_complex_type(&mut reader, &ns_map_clone)?;
                            if let Some(n) = name {
                                self.model.complex_types.insert(n, ct);
                            }
                        }
                        "simpleType" if in_schema => {
                            let attrs = collect_attrs(e);
                            let name = attrs.get("name").cloned();
                            let ns_map_clone = ns_map.clone();
                            let mut st = parse_simple_type(&mut reader, &ns_map_clone)?;
                            if let Some(n) = &name {
                                st.name = Some(QName::local(n));
                                self.model.simple_types.insert(n.clone(), st);
                            }
                        }
                        "include" | "import" => {
                            if let Some(location) =
                                schema_location(e, self.max_schema_location_bytes)?
                            {
                                let path = self.resolve_schema_location(&location)?;
                                self.load_include_file(&path)?;
                            }
                        }
                        _ => {}
                    }
                }
                Ok(Event::Empty(ref e)) => {
                    collect_xmlns(e, &mut ns_map);
                    let name_bytes = e.name();
                    let raw = std::str::from_utf8(name_bytes.as_ref())
                        .unwrap_or("")
                        .to_owned();
                    let (_, local) = split_prefix(&raw);

                    match local {
                        "include" | "import" => {
                            if let Some(location) =
                                schema_location(e, self.max_schema_location_bytes)?
                            {
                                let path = self.resolve_schema_location(&location)?;
                                self.load_include_file(&path)?;
                            }
                        }
                        "element" if in_schema => {
                            let attrs = collect_attrs(e);
                            // Self-closing top-level element with type= attribute.
                            let ns_map_clone = ns_map.clone();
                            if let Ok(Some(decl)) = parse_element_start(
                                &attrs,
                                // No children to parse since it's empty — pass a stub reader.
                                &mut Reader::from_reader(b"" as &[u8]),
                                &ns_map_clone,
                            ) {
                                self.model
                                    .global_elements
                                    .insert(decl.name.local.clone(), decl);
                            }
                        }
                        _ => {}
                    }
                }
                Ok(Event::End(ref e)) => {
                    let name_bytes = e.name();
                    let raw = std::str::from_utf8(name_bytes.as_ref())
                        .unwrap_or("")
                        .to_owned();
                    let (_, local) = split_prefix(&raw);
                    if local == "schema" {
                        in_schema = false;
                    }
                }
                Ok(Event::Eof) => break,
                Err(e) => return Err(anyhow::anyhow!("XML parse error: {e}")),
                _ => {}
            }
            buf.clear();
        }
        Ok(())
    }

    fn finish(self) -> SchemaModel {
        self.model
    }
}

// ── Element declaration parser ───────────────────────────────────────────────

fn parse_element_start(
    attrs: &HashMap<String, String>,
    reader: &mut Reader<&[u8]>,
    ns_map: &HashMap<String, String>,
) -> Result<Option<ElementDecl>> {
    let name = match attrs.get("name") {
        Some(n) => n.clone(),
        None => return Ok(None),
    };

    let occurrence = parse_occurrence(attrs);
    let nillable = attrs.get("nillable").is_some_and(|v| v == "true");

    let type_def = if let Some(type_attr) = attrs.get("type") {
        let (_, local) = split_prefix(type_attr);
        if is_xs_primitive(type_attr, ns_map) {
            XsdType::Simple(SimpleTypeDef {
                name: None,
                kind: SimpleTypeKind::from_local(local),
                facets: SimpleFacets::default(),
            })
        } else {
            XsdType::Ref(QName::local(local))
        }
    } else {
        // Inline type in children.
        parse_inline_type(reader, ns_map)?
    };

    Ok(Some(ElementDecl {
        name: QName::local(name),
        type_def,
        occurrence,
        nillable,
        documentation: None,
    }))
}

fn parse_inline_type(
    reader: &mut Reader<&[u8]>,
    ns_map: &HashMap<String, String>,
) -> Result<XsdType> {
    let mut buf = Vec::with_capacity(512);
    loop {
        match reader.read_event_into(&mut buf) {
            Ok(Event::Start(ref e)) => {
                let name_bytes = e.name();
                let raw = std::str::from_utf8(name_bytes.as_ref())
                    .unwrap_or("")
                    .to_owned();
                let (_, local) = split_prefix(&raw);
                match local {
                    "complexType" => {
                        let ct = parse_complex_type(reader, ns_map)?;
                        return Ok(XsdType::Complex(ct));
                    }
                    "simpleType" => {
                        let st = parse_simple_type(reader, ns_map)?;
                        return Ok(XsdType::Simple(st));
                    }
                    other => {
                        let tag = other.to_owned();
                        skip_element(reader, &tag)?;
                    }
                }
            }
            Ok(Event::Empty(_)) => {}
            Ok(Event::End(_)) => break,
            Ok(Event::Eof) => break,
            Err(e) => return Err(anyhow::anyhow!("inline type parse error: {e}")),
            _ => {}
        }
        buf.clear();
    }
    Ok(XsdType::Simple(SimpleTypeDef::string_unbounded()))
}

// ── Complex type parser ──────────────────────────────────────────────────────

fn parse_complex_type(
    reader: &mut Reader<&[u8]>,
    ns_map: &HashMap<String, String>,
) -> Result<ComplexType> {
    let mut attributes: Vec<AttributeDecl> = Vec::new();
    let mut content = ContentModel::Empty;
    let mut base: Option<QName> = None;
    let mut depth = 1i32;
    let mut buf = Vec::with_capacity(512);

    loop {
        match reader.read_event_into(&mut buf) {
            Ok(Event::Start(ref e)) => {
                depth += 1;
                let name_bytes = e.name();
                let raw = std::str::from_utf8(name_bytes.as_ref())
                    .unwrap_or("")
                    .to_owned();
                let (_, local) = split_prefix(&raw);
                let attrs = collect_attrs(e);
                match local {
                    "sequence" => {
                        let particles = parse_particles(reader, ns_map, "sequence")?;
                        content = ContentModel::Sequence(particles);
                        depth -= 1; // parse_particles consumed the End tag
                    }
                    "all" => {
                        let particles = parse_particles(reader, ns_map, "all")?;
                        content = ContentModel::All(particles);
                        depth -= 1;
                    }
                    "choice" => {
                        let particles = parse_particles(reader, ns_map, "choice")?;
                        content = ContentModel::Choice(particles);
                        depth -= 1;
                    }
                    "simpleContent" | "complexContent" => {
                        let outer = local.to_owned();
                        let extension = parse_content_extension(reader, ns_map, &outer)?;
                        content = extension.content;
                        base = extension.base;
                        depth -= 1;
                    }
                    "attribute" => {
                        if let Some(attr_decl) = parse_attribute_decl(&attrs) {
                            attributes.push(attr_decl);
                        }
                        depth -= 1;
                    }
                    "annotation" => {
                        skip_element(reader, "annotation")?;
                        depth -= 1;
                    }
                    _ => {}
                }
            }
            Ok(Event::Empty(ref e)) => {
                let name_bytes = e.name();
                let raw = std::str::from_utf8(name_bytes.as_ref())
                    .unwrap_or("")
                    .to_owned();
                let (_, local) = split_prefix(&raw);
                if local == "attribute" {
                    let attrs = collect_attrs(e);
                    if let Some(attr_decl) = parse_attribute_decl(&attrs) {
                        attributes.push(attr_decl);
                    }
                }
            }
            Ok(Event::End(_)) => {
                depth -= 1;
                if depth <= 0 {
                    break;
                }
            }
            Ok(Event::Eof) => break,
            Err(e) => return Err(anyhow::anyhow!("complexType parse error: {e}")),
            _ => {}
        }
        buf.clear();
    }

    Ok(ComplexType {
        name: None,
        content,
        attributes,
        base,
    })
}

fn parse_particles(
    reader: &mut Reader<&[u8]>,
    ns_map: &HashMap<String, String>,
    end_tag: &str,
) -> Result<Vec<Particle>> {
    let mut particles = Vec::new();
    let mut buf = Vec::with_capacity(512);

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
                    "element" => {
                        let occurrence = parse_occurrence(&attrs);
                        if let Some(ref_name) = attrs.get("ref") {
                            let ref_owned = ref_name.clone();
                            let (_, local_ref) = split_prefix(&ref_owned);
                            particles.push(Particle {
                                occurrence,
                                element: ElementRef::Ref(QName::local(local_ref)),
                            });
                            skip_element(reader, "element")?;
                        } else if let Some(name) = attrs.get("name") {
                            let name = name.clone();
                            let type_def = if let Some(type_attr) = attrs.get("type") {
                                let type_owned = type_attr.clone();
                                let (_, type_local) = split_prefix(&type_owned);
                                if is_xs_primitive(&type_owned, ns_map) {
                                    XsdType::Simple(SimpleTypeDef {
                                        name: None,
                                        kind: SimpleTypeKind::from_local(type_local),
                                        facets: SimpleFacets::default(),
                                    })
                                } else {
                                    XsdType::Ref(QName::local(type_local))
                                }
                            } else {
                                parse_inline_type(reader, ns_map)?
                            };
                            particles.push(Particle {
                                occurrence,
                                element: ElementRef::Inline(Box::new(ElementDecl {
                                    name: QName::local(name),
                                    type_def,
                                    occurrence,
                                    nillable: attrs.get("nillable").is_some_and(|v| v == "true"),
                                    documentation: None,
                                })),
                            });
                        }
                    }
                    "sequence" => {
                        // A nested sequence is order-equivalent to inlining its
                        // particles, so flatten it into the parent.
                        let inner = parse_particles(reader, ns_map, "sequence")?;
                        particles.extend(inner);
                    }
                    "choice" => {
                        // A nested choice is a discriminated union; preserve it
                        // as a Group particle rather than flattening its branches
                        // (which would lose the "exactly one of" semantics).
                        let occurrence = parse_occurrence(&attrs);
                        let inner = parse_particles(reader, ns_map, "choice")?;
                        particles.push(Particle {
                            occurrence,
                            element: ElementRef::Group(Box::new(ContentModel::Choice(inner))),
                        });
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
                if local == "element" {
                    let attrs = collect_attrs(e);
                    let occurrence = parse_occurrence(&attrs);
                    if let Some(ref_name) = attrs.get("ref") {
                        let ref_owned = ref_name.clone();
                        let (_, local_ref) = split_prefix(&ref_owned);
                        particles.push(Particle {
                            occurrence,
                            element: ElementRef::Ref(QName::local(local_ref)),
                        });
                    } else if let Some(name) = attrs.get("name") {
                        let name = name.clone();
                        let type_def = if let Some(type_attr) = attrs.get("type") {
                            let type_owned = type_attr.clone();
                            let (_, type_local) = split_prefix(&type_owned);
                            if is_xs_primitive(&type_owned, ns_map) {
                                XsdType::Simple(SimpleTypeDef {
                                    name: None,
                                    kind: SimpleTypeKind::from_local(type_local),
                                    facets: SimpleFacets::default(),
                                })
                            } else {
                                XsdType::Ref(QName::local(type_local))
                            }
                        } else {
                            XsdType::Simple(SimpleTypeDef::string_unbounded())
                        };
                        particles.push(Particle {
                            occurrence,
                            element: ElementRef::Inline(Box::new(ElementDecl {
                                name: QName::local(name),
                                type_def,
                                occurrence,
                                nillable: attrs.get("nillable").is_some_and(|v| v == "true"),
                                documentation: None,
                            })),
                        });
                    }
                }
            }
            Ok(Event::End(ref e)) => {
                let name_bytes = e.name();
                let raw = std::str::from_utf8(name_bytes.as_ref())
                    .unwrap_or("")
                    .to_owned();
                let (_, local) = split_prefix(&raw);
                if local == end_tag {
                    break;
                }
            }
            Ok(Event::Eof) => break,
            Err(e) => return Err(anyhow::anyhow!("particle parse error: {e}")),
            _ => {}
        }
        buf.clear();
    }
    Ok(particles)
}

/// Result of parsing a `simpleContent`/`complexContent` extension.
///
/// Carries the locally declared content and, for extensions against a *complex*
/// base, the base's qualified name so [`SchemaModel::resolve_inheritance`] can
/// fold the inherited particles and attributes in afterwards.
///
/// [`SchemaModel::resolve_inheritance`]: crate::schema::SchemaModel::resolve_inheritance
struct ParsedExtension {
    /// Content model declared locally by the derived type.
    content: ContentModel,
    /// Complex base name, or `None` for simple-content / unknown extensions.
    base: Option<QName>,
}

fn parse_content_extension(
    reader: &mut Reader<&[u8]>,
    ns_map: &HashMap<String, String>,
    outer_tag: &str,
) -> Result<ParsedExtension> {
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
                    "extension" => {
                        let base = attrs.get("base").cloned().unwrap_or_default();
                        let (_, base_local) = split_prefix(&base);
                        if is_xs_primitive(&base, ns_map) {
                            let st = SimpleTypeDef {
                                name: None,
                                kind: SimpleTypeKind::from_local(base_local),
                                facets: SimpleFacets::default(),
                            };
                            skip_element(reader, "extension")?;
                            skip_element(reader, outer_tag)?;
                            return Ok(ParsedExtension {
                                content: ContentModel::SimpleContent(Box::new(st)),
                                base: None,
                            });
                        }
                        // Complex base: record the base name so inheritance can
                        // be resolved later, and keep the locally added particles.
                        let base_qname = QName::local(base_local);
                        let particles = parse_particles(reader, ns_map, "extension")?;
                        skip_element(reader, outer_tag)?;
                        return Ok(ParsedExtension {
                            content: ContentModel::Sequence(particles),
                            base: Some(base_qname),
                        });
                    }
                    "restriction" => {
                        skip_element(reader, "restriction")?;
                    }
                    other => {
                        let tag = other.to_owned();
                        skip_element(reader, &tag)?;
                    }
                }
            }
            Ok(Event::Empty(_)) => {}
            Ok(Event::End(ref e)) => {
                let name_bytes = e.name();
                let raw = std::str::from_utf8(name_bytes.as_ref())
                    .unwrap_or("")
                    .to_owned();
                let (_, local) = split_prefix(&raw);
                if local == outer_tag {
                    break;
                }
            }
            Ok(Event::Eof) => break,
            Err(e) => return Err(anyhow::anyhow!("content extension parse error: {e}")),
            _ => {}
        }
        buf.clear();
    }
    Ok(ParsedExtension {
        content: ContentModel::Empty,
        base: None,
    })
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
                            let (_, base_local) = split_prefix(base);
                            kind = SimpleTypeKind::from_local(base_local);
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
                        let (_, base_local) = split_prefix(base);
                        kind = SimpleTypeKind::from_local(base_local);
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

    let _ = ns_map;
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

fn parse_attribute_decl(attrs: &HashMap<String, String>) -> Option<AttributeDecl> {
    let name = attrs.get("name")?.clone();
    let required = attrs.get("use").is_some_and(|u| u == "required");
    let default = attrs.get("default").cloned();
    let fixed = attrs.get("fixed").cloned();
    let type_def = if let Some(type_attr) = attrs.get("type") {
        let (_, local) = split_prefix(type_attr);
        SimpleTypeDef {
            name: None,
            kind: SimpleTypeKind::from_local(local),
            facets: SimpleFacets::default(),
        }
    } else {
        SimpleTypeDef::string_unbounded()
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

fn collect_attrs(e: &quick_xml::events::BytesStart<'_>) -> HashMap<String, String> {
    e.attributes()
        .flatten()
        .filter_map(|attr| {
            let key = std::str::from_utf8(attr.key.as_ref()).ok()?.to_string();
            let val = attr.unescape_value().ok()?.to_string();
            Some((key, val))
        })
        .collect()
}

fn schema_location(
    e: &quick_xml::events::BytesStart<'_>,
    max_bytes: usize,
) -> Result<Option<String>> {
    for attribute in e.attributes() {
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
        return attribute
            .unescape_value()
            .map(|value| Some(value.into_owned()))
            .map_err(|error| anyhow::anyhow!("invalid schemaLocation: {error}"));
    }
    Ok(None)
}

fn collect_xmlns(e: &quick_xml::events::BytesStart<'_>, ns_map: &mut HashMap<String, String>) {
    for attr in e.attributes().flatten() {
        let key = match std::str::from_utf8(attr.key.as_ref()) {
            Ok(k) => k.to_string(),
            Err(_) => continue,
        };
        let val = match attr.unescape_value() {
            Ok(v) => v.to_string(),
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
