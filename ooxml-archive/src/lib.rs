//! Shared OOXML package access for `.docx` / `.xlsx` / `.pptx`.
//!
//! Memory-maps (or owns) a ZIP package, indexes parts once, and returns
//! stored parts as borrowed slices so XML parsers never copy uncompressed
//! bytes. Deflate parts are inflated through a capped decoder before being
//! exposed to callers.

use anyhow::{anyhow, Context, Result};
use flate2::read::DeflateDecoder;
use memmap2::Mmap;
use quick_xml::events::Event;
use quick_xml::Reader;
use rawzip::{CompressionMethod, ZipArchive, ZipArchiveEntryWayfinder, ZipSliceArchive};
use std::collections::HashMap;
use std::fs::File;
use std::io::{BufRead, BufReader, Cursor, Read};
use std::ops::Deref;
use std::path::Path;
use std::sync::{Arc, Mutex};

/// Transitional Office Open XML officeDocument relationship type.
pub const REL_OFFICE_DOCUMENT: &str =
    "http://schemas.openxmlformats.org/officeDocument/2006/relationships/officeDocument";

/// ISO/IEC 29500 strict relationship type prefix.
pub const STRICT_RELATIONSHIP_PREFIX: &str =
    "http://purl.oclc.org/ooxml/officeDocument/relationships/";

/// Default per-part inflate ceiling (128 MiB).
///
/// Matches the historical `ParseOptions::max_inflate` default in the
/// office parsers. Raising it increases zip-bomb exposure.
pub const DEFAULT_MAX_INFLATE: usize = 128 * 1024 * 1024;

/// Request-scoped aggregate budget shared by every view of one OOXML package.
///
/// Parsers that reopen a package for parallel reads must clone the same
/// [`SharedInflateBudget`]. The lock is intentionally held while a Deflate
/// member is expanded, so concurrent workers cannot each allocate against the
/// same remaining bytes before either charge becomes visible.
#[derive(Debug, Default)]
pub struct InflateBudget {
    consumed: Mutex<usize>,
}

/// Shared handle for one request/document inflate budget.
pub type SharedInflateBudget = Arc<InflateBudget>;

impl InflateBudget {
    /// Create a fresh shared budget for one package parse.
    pub fn shared() -> SharedInflateBudget {
        Arc::new(Self::default())
    }
}

#[derive(Debug)]
enum ArchiveStorage {
    Mmap(Mmap),
    Owned(Vec<u8>),
}

impl AsRef<[u8]> for ArchiveStorage {
    fn as_ref(&self) -> &[u8] {
        match self {
            ArchiveStorage::Mmap(mmap) => mmap.as_ref(),
            ArchiveStorage::Owned(data) => data.as_slice(),
        }
    }
}

#[derive(Debug, Clone, Copy)]
struct EntryMeta {
    wayfinder: ZipArchiveEntryWayfinder,
    compression: CompressionMethod,
}

/// Memory-mapped or owned OOXML ZIP package with a path index.
#[derive(Debug)]
pub struct OoxmlArchive {
    archive: ZipSliceArchive<ArchiveStorage>,
    entries: HashMap<String, EntryMeta>,
    inflate_budget: SharedInflateBudget,
}

impl OoxmlArchive {
    /// Open a package from a filesystem path, preferring a read-only mmap.
    pub fn open(path: &Path) -> Result<Self> {
        Self::open_with_budget(path, InflateBudget::shared())
    }

    /// Open a package while sharing an existing request-scoped inflate budget.
    ///
    /// This is required when parallel workers reopen the same package: each
    /// archive owns its ZIP index, while every part read charges one common
    /// aggregate limit.
    pub fn open_with_budget(path: &Path, inflate_budget: SharedInflateBudget) -> Result<Self> {
        let file = File::open(path).with_context(|| format!("open file: {}", path.display()))?;
        let storage = match unsafe {
            // SAFETY: the file is mapped read-only and treated as an
            // immutable Office package. Callers never write through this
            // mapping; `Mmap` is dropped with the archive.
            Mmap::map(&file)
        } {
            Ok(mmap) => ArchiveStorage::Mmap(mmap),
            Err(_) => ArchiveStorage::Owned(read_all(&file)?),
        };
        Self::from_storage(storage, inflate_budget)
    }

    /// Open a package from owned bytes without writing a temp file.
    pub fn from_bytes(bytes: Vec<u8>) -> Result<Self> {
        Self::from_bytes_with_budget(bytes, InflateBudget::shared())
    }

    /// Open owned package bytes while sharing an existing inflate budget.
    pub fn from_bytes_with_budget(
        bytes: Vec<u8>,
        inflate_budget: SharedInflateBudget,
    ) -> Result<Self> {
        Self::from_storage(ArchiveStorage::Owned(bytes), inflate_budget)
    }

    fn from_storage(storage: ArchiveStorage, inflate_budget: SharedInflateBudget) -> Result<Self> {
        let archive = ZipArchive::from_slice(storage).context("open zip archive")?;
        let entries = build_index(&archive)?;
        Ok(Self {
            archive,
            entries,
            inflate_budget,
        })
    }

    /// Clone the request-scoped budget for another archive view or worker.
    pub fn shared_inflate_budget(&self) -> SharedInflateBudget {
        Arc::clone(&self.inflate_budget)
    }

    /// Whether a package part exists in the central directory index.
    pub fn has_part(&self, name: &str) -> bool {
        self.entries.contains_key(name)
    }

    /// Central-directory part names, in hash order.
    pub fn part_names(&self) -> impl Iterator<Item = &str> {
        self.entries.keys().map(String::as_str)
    }

    /// Read a part, borrowing stored bytes and owning inflated bytes.
    ///
    /// Fails when the uncompressed size exceeds `max_inflate`.
    pub fn read_part(&self, name: &str, max_inflate: usize) -> Result<PartBytes<'_>> {
        let (meta, data) = self.part_data(name)?;
        self.with_inflate_budget(name, max_inflate, |remaining| match meta.compression {
            CompressionMethod::Store => {
                ensure_within_max(name, data.len(), remaining)?;
                Ok((PartBytes::Borrowed(data), data.len()))
            }
            CompressionMethod::Deflate => {
                let inflated = inflate_capped(name, data, remaining)?;
                let inflated_len = inflated.len();
                Ok((PartBytes::Owned(inflated), inflated_len))
            }
            _ => Err(anyhow!("unsupported compression for part: {name}")),
        })
    }

    /// Open a buffered reader over a part.
    ///
    /// Deflate data is bounded by `max_inflate` before any bytes are exposed.
    /// `stream_oversize` is retained for compatibility and cannot bypass that
    /// boundary.
    pub fn open_part_reader(
        &self,
        name: &str,
        max_inflate: usize,
        _stream_oversize: bool,
    ) -> Result<PartReader<'_>> {
        let (meta, data) = self.part_data(name)?;
        self.with_inflate_budget(name, max_inflate, |remaining| match meta.compression {
            CompressionMethod::Store => {
                ensure_within_max(name, data.len(), remaining)?;
                Ok((PartReader::Stored(Cursor::new(data)), data.len()))
            }
            CompressionMethod::Deflate => {
                let inflated = inflate_capped(name, data, remaining)?;
                let inflated_len = inflated.len();
                Ok((PartReader::Owned(Cursor::new(inflated)), inflated_len))
            }
            _ => Err(anyhow!("unsupported compression for part: {name}")),
        })
    }

    /// Resolve the officeDocument part from `_rels/.rels`, else `fallback`.
    ///
    /// Fails when neither the relationship nor the fallback part exists.
    pub fn office_document_path(&self, max_inflate: usize, fallback: &str) -> Result<String> {
        if self.has_part("_rels/.rels") {
            let rels = self.read_part("_rels/.rels", max_inflate)?;
            if let Some(path) = parse_root_rels(rels.as_slice()) {
                return Ok(path);
            }
        }
        if self.has_part(fallback) {
            return Ok(fallback.to_string());
        }
        Err(anyhow!(
            "missing officeDocument relationship and fallback part {fallback}"
        ))
    }

    fn part_data(&self, name: &str) -> Result<(EntryMeta, &[u8])> {
        let meta = *self
            .entries
            .get(name)
            .ok_or_else(|| anyhow!("missing part: {name}"))?;
        let entry = self
            .archive
            .get_entry(meta.wayfinder)
            .with_context(|| format!("open part: {name}"))?;
        Ok((meta, entry.data()))
    }

    fn with_inflate_budget<T>(
        &self,
        name: &str,
        max_inflate: usize,
        operation: impl FnOnce(usize) -> Result<(T, usize)>,
    ) -> Result<T> {
        let mut consumed = self
            .inflate_budget
            .consumed
            .lock()
            .map_err(|_| anyhow!("OOXML inflate budget lock poisoned"))?;
        let remaining = max_inflate
            .checked_sub(*consumed)
            .ok_or_else(|| oversize_error(name, max_inflate))?;
        let (value, actual_bytes) = operation(remaining)?;
        let next = consumed
            .checked_add(actual_bytes)
            .ok_or_else(|| oversize_error(name, max_inflate))?;
        ensure_within_max(name, next, max_inflate)?;
        *consumed = next;
        Ok(value)
    }
}

/// Uncompressed package-part bytes.
///
/// Stored ZIP entries borrow the mmap or owned buffer. Deflate entries
/// own the inflated buffer.
#[derive(Debug)]
pub enum PartBytes<'a> {
    /// Uncompressed stored part, borrowed from the archive.
    Borrowed(&'a [u8]),
    /// Inflated Deflate part.
    Owned(Vec<u8>),
}

impl PartBytes<'_> {
    /// View the part as a byte slice.
    pub fn as_slice(&self) -> &[u8] {
        match self {
            Self::Borrowed(data) => data,
            Self::Owned(data) => data.as_slice(),
        }
    }

    /// Consume the part into an owned buffer, copying only when borrowed.
    pub fn into_owned(self) -> Vec<u8> {
        match self {
            Self::Borrowed(data) => data.to_vec(),
            Self::Owned(data) => data,
        }
    }

    /// Whether this part is a zero-copy stored ZIP entry.
    pub fn is_borrowed(&self) -> bool {
        matches!(self, Self::Borrowed(_))
    }
}

impl Deref for PartBytes<'_> {
    type Target = [u8];

    fn deref(&self) -> &Self::Target {
        self.as_slice()
    }
}

impl AsRef<[u8]> for PartBytes<'_> {
    fn as_ref(&self) -> &[u8] {
        self.as_slice()
    }
}

/// Buffered reader over a stored or bounded inflated part.
pub enum PartReader<'a> {
    /// Stored ZIP entry.
    Stored(Cursor<&'a [u8]>),
    /// Inflated Deflate entry held in memory.
    Owned(Cursor<Vec<u8>>),
}

impl Read for PartReader<'_> {
    fn read(&mut self, buf: &mut [u8]) -> std::io::Result<usize> {
        match self {
            Self::Stored(reader) => reader.read(buf),
            Self::Owned(reader) => reader.read(buf),
        }
    }
}

impl BufRead for PartReader<'_> {
    fn fill_buf(&mut self) -> std::io::Result<&[u8]> {
        match self {
            Self::Stored(reader) => reader.fill_buf(),
            Self::Owned(reader) => reader.fill_buf(),
        }
    }

    fn consume(&mut self, amt: usize) {
        match self {
            Self::Stored(reader) => reader.consume(amt),
            Self::Owned(reader) => reader.consume(amt),
        }
    }
}

/// Package relationship tables keyed by Id and Type.
#[derive(Debug, Default)]
pub struct Relationships {
    /// Target keyed by relationship Id (`rId1`, …).
    pub by_id: HashMap<String, String>,
    /// First target keyed by relationship Type URI.
    pub by_type: HashMap<String, String>,
    /// All relationships in document order (needed when many share a Type).
    pub entries: Vec<RelEntry>,
}

/// One `<Relationship>` row from a `.rels` part.
#[derive(Debug, Clone)]
pub struct RelEntry {
    /// Relationship Id.
    pub id: String,
    /// Relationship Type URI.
    pub rel_type: String,
    /// Target part, as written in the `.rels` file.
    pub target: String,
}

/// Parse a `.rels` XML part into Id/Type maps and an entry list.
pub fn parse_relationships(xml: &[u8]) -> Result<Relationships> {
    let mut reader = Reader::from_reader(Cursor::new(xml));
    reader.config_mut().trim_text(false);
    let mut buf = Vec::new();
    let mut rels = Relationships::default();
    loop {
        match reader.read_event_into(&mut buf) {
            Ok(Event::Start(e)) | Ok(Event::Empty(e)) => {
                if local_name(e.name().as_ref()) != b"Relationship" {
                    buf.clear();
                    continue;
                }
                let mut id = None;
                let mut rel_type = None;
                let mut target = None;
                for attr in e.attributes().with_checks(false).flatten() {
                    let key = local_name(attr.key.as_ref());
                    if key == b"Id" {
                        id = Some(
                            attr.decode_and_unescape_value(reader.decoder())?
                                .to_string(),
                        );
                    } else if key == b"Type" {
                        rel_type = Some(
                            attr.decode_and_unescape_value(reader.decoder())?
                                .to_string(),
                        );
                    } else if key == b"Target" {
                        target = Some(
                            attr.decode_and_unescape_value(reader.decoder())?
                                .to_string(),
                        );
                    }
                }
                if let (Some(id), Some(rel_type), Some(target)) = (id, rel_type, target) {
                    rels.by_id.insert(id.clone(), target.clone());
                    rels.by_type.insert(rel_type.clone(), target.clone());
                    rels.entries.push(RelEntry {
                        id,
                        rel_type,
                        target,
                    });
                }
            }
            Ok(Event::Eof) => break,
            Err(err) => return Err(anyhow!("parse relationships: {err}")),
            _ => {}
        }
        buf.clear();
    }
    Ok(rels)
}

/// Return the officeDocument target from package `_rels/.rels`.
pub fn parse_root_rels(xml: &[u8]) -> Option<String> {
    let rels = parse_relationships(xml).ok()?;
    relationship_target(&rels, REL_OFFICE_DOCUMENT).map(|target| normalize_part(target))
}

/// Compare a relationship Type against a transitional canonical URI.
///
/// Accepts the transitional URI and the ISO strict form that shares the
/// same trailing kind (`…/relationships/officeDocument`).
pub fn relationship_matches(actual: &str, canonical: &str) -> bool {
    if actual == canonical {
        return true;
    }
    canonical
        .rsplit('/')
        .next()
        .filter(|kind| !kind.is_empty())
        .is_some_and(|kind| actual.strip_prefix(STRICT_RELATIONSHIP_PREFIX) == Some(kind))
}

/// Look up a relationship target by transitional Type, then by strict alias.
pub fn relationship_target<'a>(rels: &'a Relationships, canonical: &str) -> Option<&'a String> {
    rels.by_type.get(canonical).or_else(|| {
        rels.by_type.iter().find_map(|(rel_type, target)| {
            relationship_matches(rel_type, canonical).then_some(target)
        })
    })
}

/// Strip a leading `/` and normalize `\` to `/`.
pub fn normalize_part(target: &str) -> String {
    let trimmed = target.trim_start_matches(PART_SEPARATORS);
    normalize_join("", trimmed)
}

/// Directory of a part path, without a trailing slash.
pub fn part_dir(part: &str) -> String {
    match part.rfind('/') {
        Some(idx) => part[..idx].to_string(),
        None => String::new(),
    }
}

/// Resolve `target` against `base_dir`, including `..` segments.
pub fn resolve_part(base_dir: &str, target: &str) -> String {
    if target.starts_with(PART_SEPARATORS) {
        return normalize_part(target);
    }
    normalize_join(base_dir, target)
}

/// Relationships part for `part` (`word/document.xml` → `word/_rels/document.xml.rels`).
pub fn rels_path_for_part(part: &str) -> String {
    let part = part.trim_start_matches('/');
    if let Some(idx) = part.rfind('/') {
        let dir = &part[..idx];
        let name = &part[idx + 1..];
        format!("{dir}/_rels/{name}.rels")
    } else {
        format!("_rels/{part}.rels")
    }
}

/// Inverse of [`rels_path_for_part`].
pub fn source_part_from_rels(rels_path: &str) -> Option<String> {
    let rels_path = rels_path.trim_start_matches('/');
    let suffix = ".rels";
    if !rels_path.ends_with(suffix) {
        return None;
    }
    let base = &rels_path[..rels_path.len() - suffix.len()];
    if let Some(pos) = base.rfind("/_rels/") {
        let prefix = &base[..pos];
        let name = &base[pos + 7..];
        if prefix.is_empty() {
            Some(name.to_string())
        } else {
            Some(format!("{prefix}/{name}"))
        }
    } else {
        base.strip_prefix("_rels/").map(str::to_string)
    }
}

/// Split on both separators: OOXML relationship targets authored on Windows
/// use `\`, and a ZIP part name is always `/`-separated. Losing this rewrite
/// silently skips styles, images and sheets in those packages.
const PART_SEPARATORS: [char; 2] = ['/', '\\'];

fn normalize_join(base_dir: &str, target: &str) -> String {
    let mut parts: Vec<&str> = Vec::new();
    if !base_dir.is_empty() {
        parts.extend(base_dir.split(PART_SEPARATORS).filter(|s| !s.is_empty()));
    }
    for seg in target.split(PART_SEPARATORS) {
        match seg {
            "" | "." => {}
            ".." => {
                parts.pop();
            }
            _ => parts.push(seg),
        }
    }
    parts.join("/")
}

fn local_name(name: &[u8]) -> &[u8] {
    match name.iter().position(|&b| b == b':') {
        Some(pos) => &name[pos + 1..],
        None => name,
    }
}

fn build_index(archive: &ZipSliceArchive<ArchiveStorage>) -> Result<HashMap<String, EntryMeta>> {
    let mut map = HashMap::new();
    for entry_result in archive.entries() {
        let entry = entry_result.map_err(|err| anyhow!("read zip entry: {err}"))?;
        if entry.is_dir() {
            continue;
        }
        let path = entry
            .file_path()
            .try_normalize()
            .map(|p| p.as_ref().to_string())
            .unwrap_or_else(|_| String::from_utf8_lossy(entry.file_path().as_ref()).to_string());
        let meta = EntryMeta {
            wayfinder: entry.wayfinder(),
            compression: entry.compression_method(),
        };
        map.insert(path, meta);
    }
    Ok(map)
}

fn inflate_capped(name: &str, src: &[u8], max_inflate: usize) -> Result<Vec<u8>> {
    let mut decoder = DeflateDecoder::new(Cursor::new(src));
    let mut out = Vec::new();
    decoder
        .by_ref()
        .take(max_inflate.saturating_add(1) as u64)
        .read_to_end(&mut out)?;
    ensure_within_max(name, out.len(), max_inflate)?;
    Ok(out)
}

fn ensure_within_max(name: &str, size: usize, max_inflate: usize) -> Result<()> {
    if size > max_inflate {
        Err(oversize_error(name, max_inflate))
    } else {
        Ok(())
    }
}

fn oversize_error(name: &str, max_inflate: usize) -> anyhow::Error {
    anyhow!("uncompressed part exceeds max_inflate ({max_inflate} bytes): {name}")
}

fn read_all(file: &File) -> Result<Vec<u8>> {
    let mut reader = BufReader::new(file);
    let mut data = Vec::new();
    reader.read_to_end(&mut data)?;
    Ok(data)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;
    use tempfile::tempdir;
    use zip::write::FileOptions;
    use zip::{CompressionMethod as ZipCompression, ZipWriter};

    fn write_pkg(path: &Path, parts: &[(&str, &[u8], ZipCompression)]) {
        let file = File::create(path).unwrap();
        let mut writer = ZipWriter::new(file);
        for (name, body, method) in parts {
            let options = FileOptions::default().compression_method(*method);
            writer.start_file(*name, options).unwrap();
            writer.write_all(body).unwrap();
        }
        writer.finish().unwrap();
    }

    #[test]
    fn stored_part_is_borrowed_without_copy() {
        let dir = tempdir().unwrap();
        let path = dir.path().join("store.zip");
        let body = b"<?xml version=\"1.0\"?><ok/>";
        write_pkg(
            &path,
            &[("word/document.xml", body, ZipCompression::Stored)],
        );

        let archive = OoxmlArchive::open(&path).unwrap();
        let part = archive
            .read_part("word/document.xml", DEFAULT_MAX_INFLATE)
            .unwrap();
        assert!(part.is_borrowed(), "Store parts must borrow the mmap");
        assert_eq!(part.as_slice(), body);
    }

    #[test]
    fn deflated_part_is_owned_and_round_trips() {
        let dir = tempdir().unwrap();
        let path = dir.path().join("deflate.zip");
        let body = b"deflated-payload-xxxxxxxxxxxxxxxx";
        write_pkg(
            &path,
            &[("xl/workbook.xml", body, ZipCompression::Deflated)],
        );

        let archive = OoxmlArchive::open(&path).unwrap();
        let part = archive
            .read_part("xl/workbook.xml", DEFAULT_MAX_INFLATE)
            .unwrap();
        assert!(!part.is_borrowed());
        assert_eq!(part.as_slice(), body);
    }

    #[test]
    fn read_part_rejects_oversize_deflate() {
        let dir = tempdir().unwrap();
        let path = dir.path().join("big.zip");
        let body = vec![b'x'; 512];
        write_pkg(
            &path,
            &[("word/document.xml", &body, ZipCompression::Deflated)],
        );

        let archive = OoxmlArchive::open(&path).unwrap();
        let err = archive.read_part("word/document.xml", 256).unwrap_err();
        assert!(
            err.to_string().contains("max_inflate"),
            "unexpected error: {err:#}"
        );
    }

    #[test]
    fn reader_rejects_oversize_deflate_even_when_streaming_is_requested() {
        let dir = tempdir().unwrap();
        let path = dir.path().join("big-reader.zip");
        let body = vec![b'x'; 512];
        write_pkg(
            &path,
            &[("xl/workbook.xml", &body, ZipCompression::Deflated)],
        );

        let archive = OoxmlArchive::open(&path).unwrap();
        let err = match archive.open_part_reader("xl/workbook.xml", 256, true) {
            Ok(_) => panic!("streaming must not bypass max_inflate"),
            Err(err) => err,
        };
        assert!(err.to_string().contains("max_inflate"), "{err:#}");
    }

    #[test]
    fn cumulative_inflate_budget_rejects_multiple_small_parts() {
        let dir = tempdir().unwrap();
        let path = dir.path().join("aggregate.zip");
        let body = vec![b'x'; 200];
        write_pkg(
            &path,
            &[
                ("xl/one.xml", &body, ZipCompression::Deflated),
                ("xl/two.xml", &body, ZipCompression::Deflated),
            ],
        );
        let archive = OoxmlArchive::open(&path).unwrap();
        archive.read_part("xl/one.xml", 300).unwrap();
        let err = archive
            .read_part("xl/two.xml", 300)
            .expect_err("aggregate inflated bytes must remain bounded");
        assert!(err.to_string().contains("max_inflate"), "{err:#}");
    }

    #[test]
    fn reopened_archives_share_one_inflate_budget() {
        let dir = tempdir().unwrap();
        let path = dir.path().join("shared-aggregate.zip");
        let body = vec![b'x'; 200];
        write_pkg(
            &path,
            &[
                ("xl/one.xml", &body, ZipCompression::Deflated),
                ("xl/two.xml", &body, ZipCompression::Deflated),
            ],
        );
        let budget = InflateBudget::shared();
        let first = OoxmlArchive::open_with_budget(&path, Arc::clone(&budget)).unwrap();
        let second = OoxmlArchive::open_with_budget(&path, budget).unwrap();

        first.read_part("xl/one.xml", 300).unwrap();
        let err = second
            .read_part("xl/two.xml", 300)
            .expect_err("reopening must not reset the aggregate budget");
        assert!(err.to_string().contains("max_inflate"), "{err:#}");
    }

    #[test]
    fn rels_path_keeps_full_part_extension() {
        assert_eq!(
            rels_path_for_part("word/document.xml"),
            "word/_rels/document.xml.rels"
        );
        assert_eq!(
            rels_path_for_part("ppt/slides/slide1.xml"),
            "ppt/slides/_rels/slide1.xml.rels"
        );
        assert_eq!(
            rels_path_for_part("xl/workbook.xml"),
            "xl/_rels/workbook.xml.rels"
        );
        assert_eq!(
            source_part_from_rels("ppt/slides/_rels/slide1.xml.rels").as_deref(),
            Some("ppt/slides/slide1.xml")
        );
    }

    #[test]
    fn parse_relationships_collects_every_entry() {
        let xml = br#"<?xml version="1.0"?>
<Relationships xmlns="http://schemas.openxmlformats.org/package/2006/relationships">
  <Relationship Id="rId1" Type="http://schemas.openxmlformats.org/officeDocument/2006/relationships/image" Target="media/a.png"/>
  <Relationship Id="rId2" Type="http://schemas.openxmlformats.org/officeDocument/2006/relationships/image" Target="media/b.png"/>
</Relationships>"#;
        let rels = parse_relationships(xml).unwrap();
        assert_eq!(rels.entries.len(), 2);
        assert_eq!(
            rels.by_id.get("rId2").map(String::as_str),
            Some("media/b.png")
        );
        assert_eq!(rels.by_type.len(), 1);
    }

    #[test]
    fn parse_root_rels_accepts_strict_office_document() {
        let xml = br#"<?xml version="1.0"?>
<Relationships xmlns="http://schemas.openxmlformats.org/package/2006/relationships">
  <Relationship Id="rId1" Type="http://purl.oclc.org/ooxml/officeDocument/relationships/officeDocument" Target="/word/main.xml"/>
</Relationships>"#;
        assert_eq!(parse_root_rels(xml).as_deref(), Some("word/main.xml"));
    }

    /// Relationship targets authored on Windows use `\`, but ZIP part names are
    /// always `/`-separated. The extraction into this crate dropped that rewrite
    /// once already; without it, styles/images/sheets from those packages are
    /// silently skipped because the resolved name never matches an entry.
    #[test]
    fn windows_separators_in_relationship_targets_resolve_to_zip_part_names() {
        assert_eq!(normalize_part("word\\styles.xml"), "word/styles.xml");
        assert_eq!(normalize_part("\\word\\styles.xml"), "word/styles.xml");
        assert_eq!(
            resolve_part("word", "media\\image1.png"),
            "word/media/image1.png"
        );
        assert_eq!(resolve_part("word", "\\xl\\sheet1.xml"), "xl/sheet1.xml");
        // Mixed separators and `..` traversal still collapse correctly.
        assert_eq!(
            resolve_part("word\\_rels", "..\\media/image1.png"),
            "word/media/image1.png"
        );
        // Forward-slash behaviour is unchanged.
        assert_eq!(
            resolve_part("word", "media/image1.png"),
            "word/media/image1.png"
        );
    }

    #[test]
    fn resolve_part_walks_parent_segments() {
        assert_eq!(
            resolve_part("ppt/slides", "../notesSlides/notesSlide1.xml"),
            "ppt/notesSlides/notesSlide1.xml"
        );
        assert_eq!(
            resolve_part("word", "../docProps/core.xml"),
            "docProps/core.xml"
        );
    }

    #[test]
    fn office_document_path_fails_when_missing() {
        let dir = tempdir().unwrap();
        let path = dir.path().join("empty.zip");
        write_pkg(
            &path,
            &[("[Content_Types].xml", b"<Types/>", ZipCompression::Stored)],
        );
        let archive = OoxmlArchive::open(&path).unwrap();
        let err = archive
            .office_document_path(DEFAULT_MAX_INFLATE, "word/document.xml")
            .unwrap_err();
        assert!(err.to_string().contains("officeDocument"));
    }
}
