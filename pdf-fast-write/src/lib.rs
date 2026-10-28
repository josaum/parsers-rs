//! # pdf-fast-write
//!
//! Sovereign PDF page manipulation — creates page subsets from existing
//! PDFs without any external dependencies (no PyMuPDF/fitz, no pypdf).
//!
//! Works by parsing the classic (uncompressed) cross-reference table of the
//! source PDF, extracting the page tree, and writing a new PDF that contains
//! only the requested pages. The output is a minimal, valid PDF 1.4 file with
//! uncompressed or Flate-compressed content streams.
//!
//! PDF 1.5+ compressed xref / object streams (`/Type /XRef`) are **not**
//! supported: the parser fails loud rather than emitting a corrupt subset.
//!
//! ## Quick Start
//!
//! ```rust,no_run
//! # fn main() -> anyhow::Result<()> {
//! use pdf_fast_write::PdfWriter;
//!
//! // Extract pages 1-3 and 5 from a PDF
//! PdfWriter::extract_pages("input.pdf", &[1, 2, 3, 5], "subset.pdf")?;
//! # Ok(())
//! # }
//! ```

#![deny(missing_docs)]

use anyhow::{anyhow, Context, Result};
use std::collections::HashMap;
use std::io::{BufWriter, Write};
use std::path::Path;

// ─── Public API ─────────────────────────────────────────────────────────────

/// PDF page subsetter / writer. Operates on the cross-reference table
/// to extract specific pages and write a new standalone PDF.
pub struct PdfWriter;

impl PdfWriter {
    /// Extract specific pages (1-based) from `source` into a new PDF at
    /// `output`. Pages not in the list are omitted; the page tree is
    /// rebuilt to reference only the selected pages.
    ///
    /// The output PDF is a valid PDF 1.4 file that any reader can open.
    pub fn extract_pages(
        source: impl AsRef<Path>,
        pages: &[usize],
        output: impl AsRef<Path>,
    ) -> Result<()> {
        let output = output.as_ref();
        let bytes = Self::extract_pages_to_bytes(source, pages)?;
        let file = std::fs::File::create(output)
            .with_context(|| format!("create output: {}", output.display()))?;
        let mut writer = BufWriter::new(file);
        writer.write_all(&bytes)?;
        writer.flush()?;
        Ok(())
    }

    /// Extract specific pages (1-based) from a PDF path and return the
    /// subset PDF bytes (parity with `xlsx-fast-write` in-memory writers).
    pub fn extract_pages_to_bytes(source: impl AsRef<Path>, pages: &[usize]) -> Result<Vec<u8>> {
        let source = source.as_ref();
        let data =
            std::fs::read(source).with_context(|| format!("read PDF: {}", source.display()))?;
        Self::extract_pages_from_bytes(&data, pages)
    }

    /// Extract specific pages (1-based) from in-memory PDF bytes.
    ///
    /// Prefer this on upload / queue paths that already hold the source in
    /// memory so callers avoid a redundant disk round-trip.
    pub fn extract_pages_from_bytes(data: &[u8], pages: &[usize]) -> Result<Vec<u8>> {
        let pdf = Pdf::parse(data)?;
        let subset = pdf.build_subset(pages)?;
        let mut out = Vec::new();
        subset.write(&mut out)?;
        Ok(out)
    }
}

// ─── Internal PDF structure ─────────────────────────────────────────────────

/// Parsed PDF document with cross-reference table.
struct Pdf {
    /// The raw bytes of the source PDF.
    data: Vec<u8>,
    /// Object number → (offset in data, generation)
    xref: HashMap<u32, (u64, u16)>,
    /// Trailer dictionary (retained for future incremental-update support)
    #[allow(dead_code)]
    trailer: HashMap<String, String>,
    /// Object number of the catalog (Root)
    root_obj: u32,
    /// Object number of the Pages tree root
    pages_obj: u32,
    /// Page object numbers in document order
    page_objs: Vec<u32>,
}

impl Pdf {
    fn parse(data: &[u8]) -> Result<Self> {
        let data = data.to_vec();

        // Find startxref
        let startxref = find_startxref(&data)?;

        // Parse xref table
        let (xref, trailer) = parse_xref(&data, startxref)?;

        // Get Root (catalog) object number
        let root_str = trailer
            .get("/Root")
            .ok_or_else(|| anyhow!("trailer missing /Root"))?;
        let root_obj = parse_obj_ref(root_str)?;

        // Get Pages tree root from catalog
        let catalog_data = get_object(&data, &xref, root_obj)?;
        let catalog_text = object_text(&catalog_data, root_obj)?;
        let pages_obj = find_dict_value(catalog_text, "/Pages")
            .and_then(|s| parse_obj_ref(s).ok())
            .ok_or_else(|| anyhow!("catalog missing /Pages"))?;

        // Walk the page tree to get page objects in order
        let page_objs = walk_pages(&data, &xref, pages_obj)?;

        Ok(Pdf {
            data,
            xref,
            trailer,
            root_obj,
            pages_obj,
            page_objs,
        })
    }

    fn build_subset(&self, pages: &[usize]) -> Result<SubsetPdf> {
        // Validate page numbers (1-based)
        for &page in pages {
            if page == 0 || page > self.page_objs.len() {
                return Err(anyhow!(
                    "page {} is out of range; document has {} pages",
                    page,
                    self.page_objs.len()
                ));
            }
        }
        if pages.is_empty() {
            return Err(anyhow!("no valid pages selected"));
        }
        let selected: Vec<u32> = pages.iter().map(|&page| self.page_objs[page - 1]).collect();

        // Collect all objects needed: catalog, pages, selected page objects,
        // and their resources (content streams, fonts, etc.)
        // Rebuild catalog and the top-level /Pages node below, so never walk
        // their original references: the old /Kids tree would retain every
        // page in the document. Each selected leaf contributes only its own
        // dependencies; its /Parent is replaced with the new Pages node.
        let mut visited = std::collections::HashSet::new();
        let mut inherited = HashMap::new();
        for &page in &selected {
            collect_page_dependencies(&self.data, &self.xref, page, self.pages_obj, &mut visited)?;
            let attributes =
                effective_page_attributes(&self.data, &self.xref, page, self.pages_obj)?;
            for (_, value) in &attributes {
                collect_value_references(&self.data, &self.xref, value, &mut visited)?;
            }
            inherited.insert(page, attributes);
        }
        let selected_set: std::collections::HashSet<u32> = selected.iter().copied().collect();
        for &object in &visited {
            if !selected_set.contains(&object) && is_page_object(&self.data, &self.xref, object)? {
                return Err(anyhow!(
                    "subset references unselected /Page object {}; refusing to retain cross-page destination",
                    object
                ));
            }
        }
        let mut needed: Vec<u32> = vec![self.root_obj, self.pages_obj];
        needed.extend(visited);
        needed.sort();
        needed.dedup();

        // Build the subset PDF
        let mut out = SubsetPdf {
            objects: Vec::new(),
            page_count: selected.len(),
            root_obj: 1, // placeholder, set below
        };

        // Write each needed object, remapping object numbers
        let mut id_map: HashMap<u32, u32> = HashMap::new();
        for (new_idx, &old_obj) in needed.iter().enumerate() {
            id_map.insert(old_obj, (new_idx + 1) as u32);
        }

        for &old_obj in &needed {
            let raw = get_object(&self.data, &self.xref, old_obj)?;
            let remapped = remap_refs(&self.data, &self.xref, &raw, &id_map)?;
            out.objects.push(remapped);
        }

        // Fix up the Pages object to have correct /Count and /Kids
        // The Pages object is at index 1 (second object, new ID = 2 if catalog is 1)
        let pages_new_id = id_map[&self.pages_obj];
        let pages_idx = (pages_new_id - 1) as usize;

        // Rebuild the Pages dict with only selected pages as kids
        let kids: String = selected
            .iter()
            .map(|&p| format!("{} 0 R", id_map[&p]))
            .collect::<Vec<_>>()
            .join(" ");
        let pages_obj_str = format!(
            "<< /Type /Pages /Count {} /Kids [{}] >>",
            selected.len(),
            kids
        );
        out.objects[pages_idx] = pages_obj_str.into_bytes();

        for &page in &selected {
            let page_idx = (id_map[&page] - 1) as usize;
            let attributes = inherited
                .get(&page)
                .ok_or_else(|| anyhow!("missing inherited attributes for page {}", page))?;
            out.objects[page_idx] = materialize_page_attributes(
                &self.data,
                &self.xref,
                &out.objects[page_idx],
                attributes,
                &id_map,
                pages_new_id,
            )?;
        }

        // Fix up the catalog to point to the new Pages object
        let catalog_new_id = id_map[&self.root_obj];
        let catalog_idx = (catalog_new_id - 1) as usize;
        out.objects[catalog_idx] =
            format!("<< /Type /Catalog /Pages {} 0 R >>", pages_new_id).into_bytes();

        out.root_obj = catalog_new_id;
        out.compact()?;
        Ok(out)
    }
}

/// A subset PDF ready to be written.
struct SubsetPdf {
    /// Object bodies (without the "N 0 obj" prefix), indexed by new object number - 1.
    objects: Vec<Vec<u8>>,
    #[allow(dead_code)]
    page_count: usize,
    root_obj: u32,
}

impl SubsetPdf {
    fn compact(&mut self) -> Result<()> {
        let original = std::mem::take(&mut self.objects);
        let mut reachable = std::collections::BTreeSet::new();
        let mut pending = vec![self.root_obj];
        while let Some(object) = pending.pop() {
            if !reachable.insert(object) {
                continue;
            }
            let body = original
                .get((object - 1) as usize)
                .ok_or_else(|| anyhow!("output reference points to missing object {}", object))?;
            for reference in subset_references(body, &original)? {
                if reference.generation != 0 {
                    return Err(anyhow!(
                        "output reference {} has nonzero generation",
                        reference.object
                    ));
                }
                pending.push(reference.object);
            }
        }
        let id_map: HashMap<u32, u32> = reachable
            .iter()
            .enumerate()
            .map(|(index, object)| (*object, (index + 1) as u32))
            .collect();
        let mut compacted = Vec::with_capacity(reachable.len());
        for object in reachable {
            let body = &original[(object - 1) as usize];
            compacted.push(remap_subset_refs(body, &original, &id_map)?);
        }
        self.root_obj = id_map[&self.root_obj];
        self.objects = compacted;
        Ok(())
    }

    fn write<W: Write>(&self, writer: &mut W) -> Result<()> {
        let mut out: Vec<u8> = Vec::new();
        out.extend_from_slice(b"%PDF-1.4\n");
        out.extend_from_slice(b"%\xE2\xE3\xCF\xD3\n");

        let mut offsets: Vec<u64> = Vec::with_capacity(self.objects.len());
        for (i, obj_body) in self.objects.iter().enumerate() {
            let obj_num = (i + 1) as u32;
            offsets.push(out.len() as u64);
            out.extend_from_slice(
                format!(
                    "{} 0 obj
",
                    obj_num
                )
                .as_bytes(),
            );
            out.extend_from_slice(obj_body);
            out.extend_from_slice(
                b"
endobj
",
            );
        }

        let xref_pos = out.len() as u64;
        out.extend_from_slice(
            b"xref
",
        );
        out.extend_from_slice(
            format!(
                "0 {}
",
                self.objects.len() + 1
            )
            .as_bytes(),
        );
        out.extend_from_slice(b"0000000000 65535 f \n");
        for offset in offsets {
            out.extend_from_slice(format!("{:010} 00000 n \n", offset).as_bytes());
        }
        out.extend_from_slice(
            format!(
                "trailer
<< /Size {} /Root {} 0 R >>
startxref
{}
%%EOF
",
                self.objects.len() + 1,
                self.root_obj,
                xref_pos
            )
            .as_bytes(),
        );

        writer.write_all(&out)?;
        Ok(())
    }
}

// ─── PDF parsing helpers ────────────────────────────────────────────────────

fn find_startxref(data: &[u8]) -> Result<u64> {
    // Search backwards from end for "startxref"
    let needle = b"startxref";
    let pos = data
        .windows(needle.len())
        .rposition(|w| w == needle)
        .ok_or_else(|| anyhow!("startxref not found"))?;
    let rest = &data[pos + needle.len()..];
    let rest = std::str::from_utf8(rest).map_err(|_| anyhow!("invalid UTF-8 after startxref"))?;
    let offset: u64 = rest
        .trim_start()
        .split(|c: char| !c.is_ascii_digit())
        .next()
        .unwrap_or("0")
        .parse()
        .map_err(|_| anyhow!("invalid startxref offset"))?;
    Ok(offset)
}

/// Parsed cross-reference table: object number → `(byte offset, generation)`,
/// plus the trailer key/value dictionary.
type ParsedXref = (HashMap<u32, (u64, u16)>, HashMap<String, String>);

fn parse_xref(data: &[u8], start: u64) -> Result<ParsedXref> {
    let mut merged_xref = HashMap::new();
    let mut latest_trailer = None;
    let mut current = start;
    let mut seen = std::collections::HashSet::new();
    loop {
        if !seen.insert(current) {
            return Err(anyhow!("cyclic /Prev chain in classic xref tables"));
        }
        let (xref, trailer) = parse_xref_section(data, current)?;
        for (object, entry) in xref {
            merged_xref.entry(object).or_insert(entry);
        }
        let previous = trailer
            .get("/Prev")
            .map(|value| {
                value
                    .parse::<u64>()
                    .map_err(|_| anyhow!("invalid /Prev offset: {value}"))
            })
            .transpose()?;
        if latest_trailer.is_none() {
            latest_trailer = Some(trailer);
        }
        let Some(previous) = previous else { break };
        if previous as usize >= data.len() {
            return Err(anyhow!("/Prev offset {} is outside the file", previous));
        }
        current = previous;
    }
    Ok((merged_xref, latest_trailer.unwrap_or_default()))
}

fn parse_xref_section(data: &[u8], start: u64) -> Result<ParsedXref> {
    let slice = &data[start as usize..];
    // Compressed cross-reference streams (PDF 1.5+) begin with an object
    // header (`N 0 obj`) rather than the classic `xref` keyword. Detect that
    // early and fail loud — do not attempt a partial classic parse that would
    // yield an empty/wrong object map and corrupt subset output.
    if looks_like_compressed_xref(slice) {
        return Err(anyhow!(
            "compressed xref / object stream (PDF 1.5+) is not supported; \
             pdf-fast-write requires a classic uncompressed xref table (PDF 1.4)"
        ));
    }
    let text = std::str::from_utf8(slice).map_err(|_| anyhow!("invalid UTF-8 in xref"))?;

    let mut xref: HashMap<u32, (u64, u16)> = HashMap::new();
    let mut trailer: HashMap<String, String> = HashMap::new();

    let mut lines = text.lines();
    let first = lines.next().unwrap_or("").trim();
    if first != "xref" {
        return Err(anyhow!(
            "expected classic 'xref' table, got '{}'; \
             compressed xref / ObjStm is not supported",
            first
        ));
    }

    // Parse xref subsections
    let mut current_obj: u32 = 0;
    for line in lines {
        let line = line.trim();
        if line == "trailer" {
            break;
        }
        if line.is_empty() {
            continue;
        }
        // Subsection header: "start count"
        if let Some(space) = line.find(' ') {
            let parts: Vec<&str> = line.splitn(2, ' ').collect();
            if let (Ok(s), Ok(c)) = (parts[0].parse::<u32>(), parts[1].parse::<u32>()) {
                current_obj = s;
                let _ = c;
                let _ = space;
                continue;
            }
        }
        // Entry: "offset generation f/n"
        let parts: Vec<&str> = line.splitn(3, ' ').collect();
        if parts.len() == 3 {
            if let (Ok(offset), Ok(gen)) = (parts[0].parse::<u64>(), parts[1].parse::<u16>()) {
                if parts[2].starts_with('n') {
                    xref.insert(current_obj, (offset, gen));
                }
                current_obj += 1;
            }
        }
    }

    // Parse trailer dict
    let trailer_start = text.find("trailer").unwrap_or(0);
    let trailer_text = &text[trailer_start..];
    if let Some(start_brace) = trailer_text.find("<<") {
        if let Some(end_brace) = trailer_text.rfind(">>") {
            let dict = &trailer_text[start_brace + 2..end_brace];
            // Simple key-value extraction for /Root and /Size
            for key in &["/Root", "/Size", "/Info", "/Prev"] {
                if let Some(val) = find_dict_value(dict, key) {
                    trailer.insert(key.to_string(), val.trim().to_string());
                }
            }
        }
    }

    Ok((xref, trailer))
}

/// True when `startxref` points at a PDF 1.5+ cross-reference *stream*
/// (`N gen obj` …) rather than a classic `xref` table.
fn looks_like_compressed_xref(slice: &[u8]) -> bool {
    let head = &slice[..slice.len().min(64)];
    // Classic tables start with the ASCII keyword `xref`.
    if head.starts_with(b"xref") || head.starts_with(b"\nxref") || head.starts_with(b"\r\nxref") {
        return false;
    }
    // Compressed xref streams are ordinary PDF objects: `12 0 obj<<.../Type/XRef...`.
    // Match a leading object header without requiring UTF-8 for the stream body.
    let ascii: String = head
        .iter()
        .take_while(|b| b.is_ascii())
        .map(|&b| b as char)
        .collect();
    let trimmed = ascii.trim_start();
    let mut parts = trimmed.split_whitespace();
    let Some(num) = parts.next() else {
        return false;
    };
    let Some(gen) = parts.next() else {
        return false;
    };
    let Some(kw) = parts.next() else {
        return false;
    };
    num.chars().all(|c| c.is_ascii_digit())
        && gen.chars().all(|c| c.is_ascii_digit())
        && kw.starts_with("obj")
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct PdfRef {
    start: usize,
    end: usize,
    object: u32,
    generation: u16,
}

#[derive(Clone, Copy)]
struct SyntaxToken<'a> {
    start: usize,
    end: usize,
    bytes: &'a [u8],
}

fn get_object(data: &[u8], xref: &HashMap<u32, (u64, u16)>, obj_num: u32) -> Result<Vec<u8>> {
    let (offset, generation) = xref
        .get(&obj_num)
        .ok_or_else(|| anyhow!("object {} not in xref", obj_num))?;
    let slice = data
        .get(*offset as usize..)
        .ok_or_else(|| anyhow!("object {} xref offset is outside the file", obj_num))?;
    let header_end = object_header_end(slice, obj_num, *generation)?;
    let body = &slice[header_end..];
    let end_pos = find_object_end(data, xref, body, obj_num)?;
    Ok(trim_pdf_whitespace(&body[..end_pos]).to_vec())
}

fn object_text(bytes: &[u8], obj_num: u32) -> Result<&str> {
    std::str::from_utf8(bytes).map_err(|_| anyhow!("invalid UTF-8 in object {} syntax", obj_num))
}

fn trim_pdf_whitespace(bytes: &[u8]) -> &[u8] {
    let start = bytes
        .iter()
        .position(|byte| !byte.is_ascii_whitespace())
        .unwrap_or(bytes.len());
    let end = bytes
        .iter()
        .rposition(|byte| !byte.is_ascii_whitespace())
        .map(|index| index + 1)
        .unwrap_or(start);
    &bytes[start..end]
}

fn object_header_end(slice: &[u8], object: u32, generation: u16) -> Result<usize> {
    let mut cursor = 0;
    let first = next_syntax_token(slice, &mut cursor);
    let second = next_syntax_token(slice, &mut cursor);
    let third = next_syntax_token(slice, &mut cursor);
    let header = [first, second, third];
    let [Some(first), Some(second), Some(third)] = header else {
        return Err(anyhow!("object {} has no header", object));
    };
    if parse_u32(first.bytes) != Some(object)
        || parse_u16(second.bytes) != Some(generation)
        || third.bytes != b"obj"
    {
        return Err(anyhow!(
            "xref entry for object {} generation {} does not point to a matching object header",
            object,
            generation
        ));
    }
    Ok(third.end)
}

fn stream_data_range(
    data: &[u8],
    xref: &HashMap<u32, (u64, u16)>,
    body: &[u8],
) -> Result<Option<std::ops::Range<usize>>> {
    let mut cursor = 0;
    let mut tokens = Vec::new();
    let stream = loop {
        let Some(token) = next_syntax_token(body, &mut cursor) else {
            return Ok(None);
        };
        if token.bytes == b"endobj" {
            return Ok(None);
        }
        if token.bytes == b"stream" {
            break token;
        }
        tokens.push(token);
    };
    let data_start = match body.get(stream.end..stream.end + 2) {
        Some(b"\r\n") => stream.end + 2,
        _ if body.get(stream.end) == Some(&b'\n') || body.get(stream.end) == Some(&b'\r') => {
            stream.end + 1
        }
        _ => return Err(anyhow!("malformed stream delimiter")),
    };
    let length = stream_length(data, xref, &tokens)?;
    let data_end = data_start
        .checked_add(length)
        .filter(|end| *end <= body.len())
        .ok_or_else(|| anyhow!("stream length exceeds enclosing object"))?;
    let after_data = skip_pdf_eol(body, data_end);
    if body.get(after_data..after_data + b"endstream".len()) != Some(b"endstream")
        || !is_pdf_delimiter(body.get(after_data + b"endstream".len()).copied())
    {
        return Err(anyhow!(
            "stream length {} does not lead to endstream at byte {} (next {:?})",
            length,
            after_data,
            &body[after_data..body.len().min(after_data + 16)]
        ));
    }
    Ok(Some(data_start..data_end))
}

fn stream_length(
    data: &[u8],
    xref: &HashMap<u32, (u64, u16)>,
    tokens: &[SyntaxToken<'_>],
) -> Result<usize> {
    let Some(index) = tokens.iter().rposition(|token| token.bytes == b"/Length") else {
        return Err(anyhow!("stream dictionary missing /Length"));
    };
    let reference = tokens
        .get(index..index + 4)
        .and_then(|tokens| reference_from_tokens(&tokens[1..]));
    if let Some(reference) = reference {
        validate_reference(xref, reference)?;
        let length_object = get_object(data, xref, reference.object)?;
        return parse_usize(trim_pdf_whitespace(&length_object)).ok_or_else(|| {
            anyhow!(
                "indirect /Length object {} is not an integer",
                reference.object
            )
        });
    }
    tokens
        .get(index + 1)
        .and_then(|value| parse_usize(value.bytes))
        .ok_or_else(|| anyhow!("unsupported /Length value"))
}

fn skip_pdf_eol(body: &[u8], offset: usize) -> usize {
    if body.get(offset..offset + 2) == Some(b"\r\n") {
        offset + 2
    } else if body
        .get(offset)
        .is_some_and(|byte| *byte == b'\n' || *byte == b'\r')
    {
        offset + 1
    } else {
        offset
    }
}

fn find_object_end(
    data: &[u8],
    xref: &HashMap<u32, (u64, u16)>,
    body: &[u8],
    obj_num: u32,
) -> Result<usize> {
    let search_from = if let Some(stream) = stream_data_range(data, xref, body)? {
        let after_stream = skip_pdf_eol(body, stream.end);
        after_stream + b"endstream".len()
    } else {
        0
    };
    let mut cursor = search_from;
    std::iter::from_fn(|| next_syntax_token(body, &mut cursor))
        .find(|token| token.bytes == b"endobj")
        .map(|token| token.start)
        .ok_or_else(|| anyhow!("no 'endobj' for object {}", obj_num))
}

fn syntax_tokens(bytes: &[u8]) -> Vec<SyntaxToken<'_>> {
    let mut tokens = Vec::new();
    let mut index = 0;
    while let Some(token) = next_syntax_token(bytes, &mut index) {
        tokens.push(token);
    }
    tokens
}

fn next_syntax_token<'a>(bytes: &'a [u8], index: &mut usize) -> Option<SyntaxToken<'a>> {
    while *index < bytes.len() {
        if bytes[*index].is_ascii_whitespace() {
            *index += 1;
        } else if bytes[*index] == b'%' {
            *index = bytes[*index..]
                .iter()
                .position(|byte| *byte == b'\n' || *byte == b'\r')
                .map(|offset| *index + offset + 1)
                .unwrap_or(bytes.len());
        } else if bytes[*index] == b'(' {
            let start = *index;
            *index = skip_literal_string(bytes, *index + 1);
            return Some(SyntaxToken {
                start,
                end: *index,
                bytes: &bytes[start..*index],
            });
        } else if bytes[*index] == b'<' && bytes.get(*index + 1) != Some(&b'<') {
            let start = *index;
            *index = bytes[*index + 1..]
                .iter()
                .position(|byte| *byte == b'>')
                .map(|offset| *index + offset + 2)
                .unwrap_or(bytes.len());
            return Some(SyntaxToken {
                start,
                end: *index,
                bytes: &bytes[start..*index],
            });
        } else {
            let start = *index;
            if bytes.get(*index..*index + 2) == Some(b"<<")
                || bytes.get(*index..*index + 2) == Some(b">>")
            {
                *index += 2;
            } else if bytes[*index] == b'/' {
                *index += 1;
                while *index < bytes.len() && !is_pdf_delimiter(Some(bytes[*index])) {
                    *index += 1;
                }
            } else if matches!(bytes[*index], b'[' | b']' | b'{' | b'}') {
                *index += 1;
            } else {
                *index += 1;
                while *index < bytes.len() && !is_pdf_delimiter(Some(bytes[*index])) {
                    *index += 1;
                }
            }
            return Some(SyntaxToken {
                start,
                end: *index,
                bytes: &bytes[start..*index],
            });
        }
    }
    None
}

fn skip_literal_string(bytes: &[u8], mut index: usize) -> usize {
    let mut depth = 1_u32;
    while index < bytes.len() && depth > 0 {
        match bytes[index] {
            b'\\' => index = (index + 2).min(bytes.len()),
            b'(' => {
                depth += 1;
                index += 1;
            }
            b')' => {
                depth -= 1;
                index += 1;
            }
            _ => index += 1,
        }
    }
    index
}

fn is_pdf_delimiter(byte: Option<u8>) -> bool {
    byte.is_none_or(|byte| {
        byte.is_ascii_whitespace()
            || matches!(
                byte,
                b'(' | b')' | b'<' | b'>' | b'[' | b']' | b'{' | b'}' | b'/' | b'%'
            )
    })
}

fn parse_u32(bytes: &[u8]) -> Option<u32> {
    std::str::from_utf8(bytes).ok()?.parse().ok()
}

fn parse_u16(bytes: &[u8]) -> Option<u16> {
    std::str::from_utf8(bytes).ok()?.parse().ok()
}

fn parse_usize(bytes: &[u8]) -> Option<usize> {
    std::str::from_utf8(bytes).ok()?.parse().ok()
}

fn reference_from_tokens(tokens: &[SyntaxToken<'_>]) -> Option<PdfRef> {
    let [object, generation, marker] = tokens else {
        return None;
    };
    (marker.bytes == b"R").then_some(PdfRef {
        start: object.start,
        end: marker.end,
        object: parse_u32(object.bytes)?,
        generation: parse_u16(generation.bytes)?,
    })
}

fn syntax_references(bytes: &[u8]) -> Vec<PdfRef> {
    syntax_tokens(bytes)
        .windows(3)
        .filter_map(reference_from_tokens)
        .collect()
}

fn validate_reference(xref: &HashMap<u32, (u64, u16)>, reference: PdfRef) -> Result<()> {
    let Some((_, actual_generation)) = xref.get(&reference.object) else {
        return Err(anyhow!(
            "referenced object {} is absent from xref",
            reference.object
        ));
    };
    if *actual_generation != reference.generation {
        return Err(anyhow!(
            "reference {} {} R does not match xref generation {}",
            reference.object,
            reference.generation,
            actual_generation
        ));
    }
    Ok(())
}

fn parse_obj_ref(s: &str) -> Result<u32> {
    let s = s.trim();
    let parts: Vec<&str> = s.split_whitespace().collect();
    if parts.is_empty() {
        return Err(anyhow!("empty object reference"));
    }
    parts[0]
        .parse::<u32>()
        .map_err(|_| anyhow!("invalid object reference: {}", s))
}

fn find_dict_value<'a>(dict: &'a str, key: &str) -> Option<&'a str> {
    let pos = find_top_level_dict_key(dict, key)?;
    let rest = dict[pos + key.len()..].trim_start();
    if rest.is_empty() {
        return None;
    }

    // Name object: /Type /Page, /Type /Pages, etc.
    if let Some(stripped) = rest.strip_prefix('/') {
        let end = stripped
            .find(|c: char| c.is_whitespace() || c == '/' || c == '>' || c == '[' || c == ']')
            .map(|i| i + 1)
            .unwrap_or(rest.len());
        return Some(&rest[..end]);
    }

    // Indirect reference: N G R
    let parts: Vec<&str> = rest.split_whitespace().take(3).collect();
    if parts.len() == 3
        && parts[0].chars().all(|c| c.is_ascii_digit())
        && parts[1].chars().all(|c| c.is_ascii_digit())
        && parts[2] == "R"
    {
        let len = parts.iter().map(|p| p.len()).sum::<usize>() + 2; // two spaces
        return Some(&rest[..len]);
    }

    // Number or scalar token.
    let end = rest
        .find(|c: char| c.is_whitespace() || c == '/' || c == '>' || c == '[' || c == ']')
        .unwrap_or(rest.len());
    Some(&rest[..end])
}

fn find_top_level_dict_key(dict: &str, key: &str) -> Option<usize> {
    let bytes = dict.as_bytes();
    let tokens = syntax_tokens(bytes);
    let target_depth = usize::from(tokens.first().is_some_and(|token| token.bytes == b"<<"));
    let mut depth: usize = 0;
    for token in tokens {
        if token.bytes == b"<<" {
            depth += 1;
            continue;
        }
        if token.bytes == b">>" {
            depth = depth.saturating_sub(1);
            continue;
        }
        if depth == target_depth && token.bytes == key.as_bytes() {
            return Some(token.start);
        }
    }
    None
}

fn walk_pages(data: &[u8], xref: &HashMap<u32, (u64, u16)>, pages_obj: u32) -> Result<Vec<u32>> {
    let mut pages = Vec::new();
    walk_pages_recursive(data, xref, pages_obj, &mut pages)?;
    Ok(pages)
}

fn walk_pages_recursive(
    data: &[u8],
    xref: &HashMap<u32, (u64, u16)>,
    node_obj: u32,
    pages: &mut Vec<u32>,
) -> Result<()> {
    let node = get_object(data, xref, node_obj)?;
    let node = object_text(&node, node_obj)?;
    let type_val = find_dict_value(node, "/Type").unwrap_or("");

    if type_val == "/Page" {
        pages.push(node_obj);
        return Ok(());
    }

    // It's a /Pages node — walk /Kids
    if let Some(kids_str) = find_in_array(node, "/Kids") {
        let tokens: Vec<&str> = kids_str.split_whitespace().collect();
        let mut i = 0;
        while i + 2 < tokens.len() {
            if tokens[i + 2] == "R" {
                if let Ok(child_obj) = tokens[i].parse::<u32>() {
                    walk_pages_recursive(data, xref, child_obj, pages)?;
                }
                i += 3;
            } else {
                i += 1;
            }
        }
    }
    Ok(())
}

fn is_page_object(data: &[u8], xref: &HashMap<u32, (u64, u16)>, object: u32) -> Result<bool> {
    let body = get_object(data, xref, object)?;
    Ok(top_level_value_bytes(&body, b"/Type").as_deref() == Some(b"/Page"))
}

fn find_in_array(dict: &str, key: &str) -> Option<String> {
    let bytes = dict.as_bytes();
    let tokens = syntax_tokens(bytes);
    let target_depth = usize::from(tokens.first().is_some_and(|token| token.bytes == b"<<"));
    let mut depth: usize = 0;
    let mut key_index = None;
    for (index, token) in tokens.iter().enumerate() {
        match token.bytes {
            b"<<" => depth += 1,
            b">>" => depth = depth.saturating_sub(1),
            _ if depth == target_depth && token.bytes == key.as_bytes() => {
                key_index = Some(index);
                break;
            }
            _ => {}
        }
    }
    let start = tokens.get(key_index? + 1)?;
    if start.bytes != b"[" {
        return None;
    }
    let mut array_depth = 1;
    for token in tokens.iter().skip(key_index? + 2) {
        match token.bytes {
            b"[" => array_depth += 1,
            b"]" => {
                array_depth -= 1;
                if array_depth == 0 {
                    return std::str::from_utf8(&bytes[start.end..token.start])
                        .ok()
                        .map(str::to_string);
                }
            }
            _ => {}
        }
    }
    None
}

fn collect_refs(
    data: &[u8],
    xref: &HashMap<u32, (u64, u16)>,
    obj_num: u32,
    visited: &mut std::collections::HashSet<u32>,
) -> Result<()> {
    if visited.contains(&obj_num) {
        return Ok(());
    }
    visited.insert(obj_num);
    let obj = get_object(data, xref, obj_num)?;
    for syntax in syntax_segments(data, xref, &obj)? {
        for reference in syntax_references(syntax) {
            validate_reference(xref, reference)?;
            collect_refs(data, xref, reference.object, visited)?;
        }
    }
    Ok(())
}

fn collect_page_dependencies(
    data: &[u8],
    xref: &HashMap<u32, (u64, u16)>,
    page_obj: u32,
    old_pages_root: u32,
    visited: &mut std::collections::HashSet<u32>,
) -> Result<()> {
    if !visited.insert(page_obj) {
        return Ok(());
    }
    let page = get_object(data, xref, page_obj)?;
    let parent = dictionary_reference(&page, b"/Parent")?;
    for syntax in syntax_segments(data, xref, &page)? {
        for reference in syntax_references(syntax) {
            validate_reference(xref, reference)?;
            if Some(reference) == parent || reference.object == old_pages_root {
                continue;
            }
            collect_refs(data, xref, reference.object, visited)?;
        }
    }
    Ok(())
}

const INHERITABLE_PAGE_KEYS: &[&[u8]] = &[b"/Resources", b"/MediaBox", b"/CropBox", b"/Rotate"];

fn effective_page_attributes(
    data: &[u8],
    xref: &HashMap<u32, (u64, u16)>,
    page_obj: u32,
    pages_root: u32,
) -> Result<Vec<(Vec<u8>, Vec<u8>)>> {
    let mut attributes: Vec<(Vec<u8>, Vec<u8>)> = Vec::new();
    let mut current = page_obj;
    loop {
        let node = get_object(data, xref, current)?;
        for &key in INHERITABLE_PAGE_KEYS {
            if attributes
                .iter()
                .all(|(existing, _)| existing.as_slice() != key)
            {
                if let Some(value) = top_level_value_bytes(&node, key) {
                    attributes.push((key.to_vec(), value));
                }
            }
        }
        if current == pages_root {
            break;
        }
        let parent = dictionary_reference(&node, b"/Parent")?
            .ok_or_else(|| anyhow!("page tree node {} has no /Parent", current))?;
        validate_reference(xref, parent)?;
        current = parent.object;
    }
    Ok(attributes)
}

fn collect_value_references(
    data: &[u8],
    xref: &HashMap<u32, (u64, u16)>,
    value: &[u8],
    visited: &mut std::collections::HashSet<u32>,
) -> Result<()> {
    for reference in syntax_references(value) {
        validate_reference(xref, reference)?;
        collect_refs(data, xref, reference.object, visited)?;
    }
    Ok(())
}

fn materialize_page_attributes(
    data: &[u8],
    xref: &HashMap<u32, (u64, u16)>,
    page: &[u8],
    attributes: &[(Vec<u8>, Vec<u8>)],
    id_map: &HashMap<u32, u32>,
    pages_root: u32,
) -> Result<Vec<u8>> {
    let parent = format!("{} 0 R", pages_root);
    let mut page = replace_top_level_value(page, b"/Parent", parent.as_bytes())
        .ok_or_else(|| anyhow!("selected page has no direct /Parent to rewrite"))?;
    for (key, value) in attributes {
        if top_level_value_range(&page, key).is_none() {
            let value = remap_refs(data, xref, value, id_map)?;
            let close = top_level_dict_close(&page)
                .ok_or_else(|| anyhow!("selected page has no closing dictionary delimiter"))?;
            let mut expanded = Vec::with_capacity(page.len() + key.len() + value.len() + 2);
            expanded.extend_from_slice(&page[..close]);
            expanded.extend_from_slice(b" ");
            expanded.extend_from_slice(key);
            expanded.extend_from_slice(b" ");
            expanded.extend_from_slice(&value);
            expanded.extend_from_slice(b" ");
            expanded.extend_from_slice(&page[close..]);
            page = expanded;
        }
    }
    Ok(page)
}

fn replace_top_level_value(dict: &[u8], key: &[u8], replacement: &[u8]) -> Option<Vec<u8>> {
    let range = top_level_value_range(dict, key)?;
    let mut rewritten = Vec::with_capacity(dict.len() + replacement.len() - range.len());
    rewritten.extend_from_slice(&dict[..range.start]);
    rewritten.extend_from_slice(replacement);
    rewritten.extend_from_slice(&dict[range.end..]);
    Some(rewritten)
}

fn top_level_value_bytes(dict: &[u8], key: &[u8]) -> Option<Vec<u8>> {
    let range = top_level_value_range(dict, key)?;
    Some(dict[range].to_vec())
}

fn top_level_value_range(dict: &[u8], key: &[u8]) -> Option<std::ops::Range<usize>> {
    let tokens = syntax_tokens(dict);
    let mut depth: usize = 0;
    for (index, token) in tokens.iter().enumerate() {
        match token.bytes {
            b"<<" => depth += 1,
            b">>" => depth = depth.saturating_sub(1),
            _ if depth == 1 && token.bytes == key => {
                let value = tokens.get(index + 1)?;
                let end = match value.bytes {
                    b"[" => matching_token_end(&tokens, index + 1, b"[", b"]")?,
                    b"<<" => matching_token_end(&tokens, index + 1, b"<<", b">>")?,
                    _ => tokens
                        .get(index + 3)
                        .and_then(|marker| (marker.bytes == b"R").then_some(marker.end))
                        .unwrap_or(value.end),
                };
                return Some(value.start..end);
            }
            _ => {}
        }
    }
    None
}

fn matching_token_end(
    tokens: &[SyntaxToken<'_>],
    start: usize,
    open: &[u8],
    close: &[u8],
) -> Option<usize> {
    let mut depth = 0;
    for token in tokens.iter().skip(start) {
        if token.bytes == open {
            depth += 1;
        } else if token.bytes == close {
            depth -= 1;
            if depth == 0 {
                return Some(token.end);
            }
        }
    }
    None
}

fn top_level_dict_close(dict: &[u8]) -> Option<usize> {
    let mut depth = 0;
    for token in syntax_tokens(dict) {
        if token.bytes == b"<<" {
            depth += 1;
        } else if token.bytes == b">>" {
            depth -= 1;
            if depth == 0 {
                return Some(token.start);
            }
        }
    }
    None
}

fn dictionary_reference(obj: &[u8], key: &[u8]) -> Result<Option<PdfRef>> {
    Ok(top_level_value_bytes(obj, key)
        .as_deref()
        .and_then(|value| syntax_references(value).into_iter().next()))
}

fn syntax_segments<'a>(
    data: &[u8],
    xref: &HashMap<u32, (u64, u16)>,
    obj: &'a [u8],
) -> Result<Vec<&'a [u8]>> {
    if let Some(stream) = stream_data_range(data, xref, obj)? {
        Ok(vec![&obj[..stream.start], &obj[stream.end..]])
    } else {
        Ok(vec![obj])
    }
}

fn remap_refs(
    data: &[u8],
    xref: &HashMap<u32, (u64, u16)>,
    obj: &[u8],
    id_map: &HashMap<u32, u32>,
) -> Result<Vec<u8>> {
    let mut result = Vec::with_capacity(obj.len());
    if let Some(stream) = stream_data_range(data, xref, obj)? {
        remap_refs_in_syntax(&obj[..stream.start], xref, id_map, &mut result)?;
        result.extend_from_slice(&obj[stream.start..stream.end]);
        remap_refs_in_syntax(&obj[stream.end..], xref, id_map, &mut result)?;
    } else {
        remap_refs_in_syntax(obj, xref, id_map, &mut result)?;
    }
    Ok(result)
}

fn remap_refs_in_syntax(
    syntax: &[u8],
    xref: &HashMap<u32, (u64, u16)>,
    id_map: &HashMap<u32, u32>,
    result: &mut Vec<u8>,
) -> Result<()> {
    let mut search_pos = 0;
    for reference in syntax_references(syntax) {
        validate_reference(xref, reference)?;
        if let Some(&new_num) = id_map.get(&reference.object) {
            result.extend_from_slice(&syntax[search_pos..reference.start]);
            result.extend_from_slice(new_num.to_string().as_bytes());
            result.extend_from_slice(b" 0 R");
            search_pos = reference.end;
        }
    }
    result.extend_from_slice(&syntax[search_pos..]);
    Ok(())
}

fn subset_references(body: &[u8], objects: &[Vec<u8>]) -> Result<Vec<PdfRef>> {
    if let Some(stream) = subset_stream_data_range(body, objects)? {
        let mut references = syntax_references(&body[..stream.start]);
        references.extend(syntax_references(&body[stream.end..]));
        Ok(references)
    } else {
        Ok(syntax_references(body))
    }
}

fn remap_subset_refs(
    body: &[u8],
    objects: &[Vec<u8>],
    id_map: &HashMap<u32, u32>,
) -> Result<Vec<u8>> {
    if let Some(stream) = subset_stream_data_range(body, objects)? {
        let mut output = remap_subset_syntax(&body[..stream.start], id_map)?;
        output.extend_from_slice(&body[stream.start..stream.end]);
        output.extend_from_slice(&remap_subset_syntax(&body[stream.end..], id_map)?);
        Ok(output)
    } else {
        remap_subset_syntax(body, id_map)
    }
}

fn remap_subset_syntax(syntax: &[u8], id_map: &HashMap<u32, u32>) -> Result<Vec<u8>> {
    let mut output = Vec::with_capacity(syntax.len());
    let mut cursor = 0;
    for reference in syntax_references(syntax) {
        if reference.generation != 0 {
            return Err(anyhow!(
                "output reference {} has nonzero generation",
                reference.object
            ));
        }
        let new_object = id_map.get(&reference.object).ok_or_else(|| {
            anyhow!(
                "reachable object refers to pruned object {}",
                reference.object
            )
        })?;
        output.extend_from_slice(&syntax[cursor..reference.start]);
        output.extend_from_slice(new_object.to_string().as_bytes());
        output.extend_from_slice(b" 0 R");
        cursor = reference.end;
    }
    output.extend_from_slice(&syntax[cursor..]);
    Ok(output)
}

fn subset_stream_data_range(
    body: &[u8],
    objects: &[Vec<u8>],
) -> Result<Option<std::ops::Range<usize>>> {
    let mut cursor = 0;
    let mut tokens = Vec::new();
    let stream = loop {
        let Some(token) = next_syntax_token(body, &mut cursor) else {
            return Ok(None);
        };
        if token.bytes == b"endobj" {
            return Ok(None);
        }
        if token.bytes == b"stream" {
            break token;
        }
        tokens.push(token);
    };
    let data_start = match body.get(stream.end..stream.end + 2) {
        Some(b"\r\n") => stream.end + 2,
        _ if body.get(stream.end) == Some(&b'\n') || body.get(stream.end) == Some(&b'\r') => {
            stream.end + 1
        }
        _ => return Err(anyhow!("malformed output stream delimiter")),
    };
    let length_index = tokens
        .iter()
        .rposition(|token| token.bytes == b"/Length")
        .ok_or_else(|| anyhow!("output stream dictionary missing /Length"))?;
    let length = if let Some(reference) = tokens
        .get(length_index + 1..length_index + 4)
        .and_then(reference_from_tokens)
    {
        if reference.generation != 0 {
            return Err(anyhow!("output /Length reference has nonzero generation"));
        }
        let value = objects
            .get((reference.object - 1) as usize)
            .ok_or_else(|| anyhow!("output /Length object {} is absent", reference.object))?;
        parse_usize(trim_pdf_whitespace(value))
            .ok_or_else(|| anyhow!("output indirect /Length is not an integer"))?
    } else {
        tokens
            .get(length_index + 1)
            .and_then(|token| parse_usize(token.bytes))
            .ok_or_else(|| anyhow!("unsupported output /Length value"))?
    };
    let data_end = data_start
        .checked_add(length)
        .filter(|end| *end <= body.len())
        .ok_or_else(|| anyhow!("output stream length exceeds object"))?;
    let after_data = skip_pdf_eol(body, data_end);
    if body.get(after_data..after_data + b"endstream".len()) != Some(b"endstream") {
        return Err(anyhow!("output stream length does not lead to endstream"));
    }
    Ok(Some(data_start..data_end))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn make_minimal_pdf() -> Vec<u8> {
        // Build a minimal 2-page PDF with correct xref offsets.
        let header = b"%PDF-1.4\n%\xE2\xE3\xCF\xD3\n";

        let obj1 = b"1 0 obj\n<< /Type /Catalog /Pages 2 0 R >>\nendobj\n";
        let obj2 = b"2 0 obj\n<< /Type /Pages /Count 2 /Kids [3 0 R 4 0 R] >>\nendobj\n";
        let obj3 = b"3 0 obj\n<< /Type /Page /Parent 2 0 R /MediaBox [0 0 612 792] /Contents 5 0 R >>\nendobj\n";
        let obj4 = b"4 0 obj\n<< /Type /Page /Parent 2 0 R /MediaBox [0 0 612 792] /Contents 6 0 R >>\nendobj\n";
        let obj5 = b"5 0 obj\n<< /Length 39 >>\nstream\nBT /F1 12 Tf 100 700 Td (Page 1) Tj ET\nendstream\nendobj\n";
        let obj6 = b"6 0 obj\n<< /Length 39 >>\nstream\nBT /F1 12 Tf 100 700 Td (Page 2) Tj ET\nendstream\nendobj\n";

        let objs: [&[u8]; 6] = [obj1, obj2, obj3, obj4, obj5, obj6];
        let header_len = header.len() as u64;
        let mut offsets: Vec<u64> = Vec::new();
        let mut pos = header_len;
        for obj in &objs {
            offsets.push(pos);
            pos += obj.len() as u64;
        }

        let xref_pos = pos;
        let mut pdf = Vec::new();
        pdf.extend_from_slice(header);
        for obj in &objs {
            pdf.extend_from_slice(obj);
        }

        // xref table
        let xref = format!(
            "xref\n0 7\n0000000000 65535 f \n{:010} 00000 n \n{:010} 00000 n \n{:010} 00000 n \n{:010} 00000 n \n{:010} 00000 n \n{:010} 00000 n \ntrailer\n<< /Size 7 /Root 1 0 R >>\nstartxref\n{}\n%%EOF\n",
            offsets[0], offsets[1], offsets[2], offsets[3], offsets[4], offsets[5], xref_pos
        );
        pdf.extend_from_slice(xref.as_bytes());
        pdf
    }

    fn make_pdf_with_invalid_utf8_content_stream() -> (Vec<u8>, Vec<u8>) {
        // PDF stream payloads are binary by definition. This content stream is
        // syntactically valid PDF text containing a non-UTF-8 character code.
        let header = b"%PDF-1.4\n%\xE2\xE3\xCF\xD3\n";
        let content = b"BT (\xFF) Tj ET\n".to_vec();

        let obj1 = b"1 0 obj\n<< /Type /Catalog /Pages 2 0 R >>\nendobj\n".to_vec();
        let obj2 = b"2 0 obj\n<< /Type /Pages /Count 1 /Kids [3 0 R] >>\nendobj\n".to_vec();
        let obj3 = b"3 0 obj\n<< /Type /Page /Parent 2 0 R /MediaBox [0 0 612 792] /Contents 4 0 R >>\nendobj\n".to_vec();
        let mut obj4 = format!("4 0 obj\n<< /Length {} >>\nstream\n", content.len()).into_bytes();
        obj4.extend_from_slice(&content);
        obj4.extend_from_slice(b"endstream\nendobj\n");

        let objs = [obj1, obj2, obj3, obj4];
        let mut offsets = Vec::with_capacity(objs.len());
        let mut pos = header.len() as u64;
        for obj in &objs {
            offsets.push(pos);
            pos += obj.len() as u64;
        }

        let mut pdf = Vec::from(&header[..]);
        for obj in &objs {
            pdf.extend_from_slice(obj);
        }
        pdf.extend_from_slice(
            format!(
                "xref\n0 5\n0000000000 65535 f \n{:010} 00000 n \n{:010} 00000 n \n{:010} 00000 n \n{:010} 00000 n \ntrailer\n<< /Size 5 /Root 1 0 R >>\nstartxref\n{}\n%%EOF\n",
                offsets[0], offsets[1], offsets[2], offsets[3], pos
            )
            .as_bytes(),
        );
        (pdf, content)
    }

    fn make_pdf_with_indirect_length_and_cr_stream() -> (Vec<u8>, Vec<u8>) {
        let header = b"%PDF-1.4\n%\xE2\xE3\xCF\xD3\n";
        let content = b"BT (\xFE) Tj ET".to_vec();

        let obj1 = b"1 0 obj\n<< /Type /Catalog /Pages 2 0 R >>\nendobj\n".to_vec();
        let obj2 = b"2 0 obj\n<< /Type /Pages /Count 1 /Kids [3 0 R] >>\nendobj\n".to_vec();
        let obj3 = b"3 0 obj\n<< /Type /Page /Parent 2 0 R /MediaBox [0 0 612 792] /Contents 4 0 R >>\nendobj\n".to_vec();
        let mut obj4 = b"4 0 obj\n<< /Length 5 0 R >>\rstream\r".to_vec();
        obj4.extend_from_slice(&content);
        obj4.extend_from_slice(b"\rendstream\rendobj\n");
        let obj5 = format!("5 0 obj\n{}\nendobj\n", content.len()).into_bytes();

        let objs = [obj1, obj2, obj3, obj4, obj5];
        let mut offsets = Vec::with_capacity(objs.len());
        let mut pos = header.len() as u64;
        for obj in &objs {
            offsets.push(pos);
            pos += obj.len() as u64;
        }

        let mut pdf = Vec::from(&header[..]);
        for obj in &objs {
            pdf.extend_from_slice(obj);
        }
        pdf.extend_from_slice(
            format!(
                "xref\n0 6\n0000000000 65535 f \n{:010} 00000 n \n{:010} 00000 n \n{:010} 00000 n \n{:010} 00000 n \n{:010} 00000 n \ntrailer\n<< /Size 6 /Root 1 0 R >>\nstartxref\n{}\n%%EOF\n",
                offsets[0], offsets[1], offsets[2], offsets[3], offsets[4], pos
            )
            .as_bytes(),
        );
        (pdf, content)
    }

    fn make_classic_pdf(objects: Vec<(u32, u16, Vec<u8>)>, root: (u32, u16)) -> Vec<u8> {
        let header = b"%PDF-1.4\n%\xE2\xE3\xCF\xD3\n";
        let size = objects
            .iter()
            .map(|(number, _, _)| *number)
            .max()
            .unwrap_or(0)
            + 1;
        let mut offsets = vec![(0_u64, 0_u16); size as usize];
        let mut pdf = Vec::from(&header[..]);
        for (number, generation, object) in &objects {
            offsets[*number as usize] = (pdf.len() as u64, *generation);
            pdf.extend_from_slice(object);
        }
        let xref_pos = pdf.len();
        pdf.extend_from_slice(format!("xref\n0 {size}\n0000000000 65535 f \n").as_bytes());
        for (offset, generation) in offsets.iter().skip(1) {
            pdf.extend_from_slice(format!("{offset:010} {generation:05} n \n").as_bytes());
        }
        pdf.extend_from_slice(
            format!(
                "trailer\n<< /Size {size} /Root {} {} R >>\nstartxref\n{xref_pos}\n%%EOF\n",
                root.0, root.1
            )
            .as_bytes(),
        );
        pdf
    }

    #[test]
    fn test_subset_does_not_retain_unselected_page_dependencies() {
        let data = make_minimal_pdf();

        let output = PdfWriter::extract_pages_from_bytes(&data, &[1]).unwrap();

        assert!(output
            .windows(b"Page 1".len())
            .any(|window| window == b"Page 1"));
        assert!(
            !output
                .windows(b"Page 2".len())
                .any(|window| window == b"Page 2"),
            "the unselected page content stream must not be retained"
        );
    }

    #[test]
    fn test_subset_rejects_annotation_destination_to_unselected_page() {
        let data = make_classic_pdf(
            vec![
                (1, 0, b"1 0 obj\n<< /Type /Catalog /Pages 2 0 R >>\nendobj\n".to_vec()),
                (2, 0, b"2 0 obj\n<< /Type /Pages /Count 2 /Kids [3 0 R 4 0 R] >>\nendobj\n".to_vec()),
                (3, 0, b"3 0 obj\n<< /Type /Page /Parent 2 0 R /Contents 5 0 R /Annots [7 0 R] >>\nendobj\n".to_vec()),
                (4, 0, b"4 0 obj\n<< /Type /Page /Parent 2 0 R /Contents 6 0 R >>\nendobj\n".to_vec()),
                (5, 0, b"5 0 obj\n<< /Length 12 >>\nstream\nBT (safe)ET\nendstream\nendobj\n".to_vec()),
                (6, 0, b"6 0 obj\n<< /Length 23 >>\nstream\nSECRET_UNSELECTED_PAGE\nendstream\nendobj\n".to_vec()),
                (7, 0, b"7 0 obj\n<< /Type /Annot /Subtype /Link /Dest [4 0 R /Fit] >>\nendobj\n".to_vec()),
            ],
            (1, 0),
        );

        let error = PdfWriter::extract_pages_from_bytes(&data, &[1]).unwrap_err();

        assert!(error.to_string().contains("unselected /Page"));
    }

    #[test]
    fn test_extract_pages_remaps_nonzero_generation_references() {
        let content = b"BT (nonzero generation) Tj ET\n";
        let stream = [
            format!("4 7 obj\n<< /Length {} >>\nstream\n", content.len()).into_bytes(),
            content.to_vec(),
            b"endstream\nendobj\n".to_vec(),
        ]
        .concat();
        let data = make_classic_pdf(
            vec![
                (
                    1,
                    7,
                    b"1 7 obj\n<< /Type /Catalog /Pages 2 7 R >>\nendobj\n".to_vec(),
                ),
                (
                    2,
                    7,
                    b"2 7 obj\n<< /Type /Pages /Count 1 /Kids [3 7 R] >>\nendobj\n".to_vec(),
                ),
                (
                    3,
                    7,
                    b"3 7 obj\n<< /Type /Page /Parent 2 7 R /Contents 4 7 R >>\nendobj\n".to_vec(),
                ),
                (4, 7, stream),
            ],
            (1, 7),
        );

        let output = PdfWriter::extract_pages_from_bytes(&data, &[1]).unwrap();

        assert!(output
            .windows(content.len())
            .any(|window| window == content));
        assert!(
            !output
                .windows(b" 7 R".len())
                .any(|window| window == b" 7 R"),
            "all retained references must be emitted with generation 0"
        );
    }

    #[test]
    fn test_indirect_length_ignores_keywords_inside_literal_and_stream_payload() {
        let content = b"q endobj stream\n Q \xFF".to_vec();
        let stream = [
            b"4 0 obj\n<< /Length 5 0 R /Note (stream\nendobj) >>\nstream\n".to_vec(),
            content.clone(),
            b"\nendstream\nendobj\n".to_vec(),
        ]
        .concat();
        let data = make_classic_pdf(
            vec![
                (
                    1,
                    0,
                    b"1 0 obj\n<< /Type /Catalog /Pages 2 0 R >>\nendobj\n".to_vec(),
                ),
                (
                    2,
                    0,
                    b"2 0 obj\n<< /Type /Pages /Count 1 /Kids [3 0 R] >>\nendobj\n".to_vec(),
                ),
                (
                    3,
                    0,
                    b"3 0 obj\n<< /Type /Page /Parent 2 0 R /Contents 4 0 R >>\nendobj\n".to_vec(),
                ),
                (4, 0, stream),
                (
                    5,
                    0,
                    format!("5 0 obj\n{}\nendobj\n", content.len()).into_bytes(),
                ),
            ],
            (1, 0),
        );

        let output = PdfWriter::extract_pages_from_bytes(&data, &[1]).unwrap();

        assert!(output
            .windows(content.len())
            .any(|window| window == content));
    }

    #[test]
    fn test_find_startxref() {
        let data = make_minimal_pdf();
        let pos = find_startxref(&data).unwrap();
        assert!(pos > 0);
    }

    #[test]
    fn test_parse_xref() {
        let data = make_minimal_pdf();
        let startxref = find_startxref(&data).unwrap();
        let (xref, trailer) = parse_xref(&data, startxref).unwrap();
        assert!(xref.contains_key(&1));
        assert!(xref.contains_key(&2));
        assert_eq!(trailer.get("/Root"), Some(&"1 0 R".to_string()));
    }

    #[test]
    fn test_parse_xref_follows_classic_prev_chain() {
        let mut data = make_minimal_pdf();
        let previous = find_startxref(&data).unwrap();
        let object_offset = data.len();
        data.extend_from_slice(b"7 0 obj\n<< /Producer (incremental) >>\nendobj\n");
        let xref_offset = data.len();
        data.extend_from_slice(
            format!(
                "xref\n7 1\n{object_offset:010} 00000 n \ntrailer\n<< /Size 8 /Root 1 0 R /Prev {previous} >>\nstartxref\n{xref_offset}\n%%EOF\n"
            )
            .as_bytes(),
        );

        let startxref = find_startxref(&data).unwrap();
        let (xref, trailer) = parse_xref(&data, startxref).unwrap();

        assert!(xref.contains_key(&1), "base xref entries must survive");
        assert!(xref.contains_key(&7), "latest xref entries must be present");
        assert_eq!(trailer.get("/Root"), Some(&"1 0 R".to_string()));
    }

    #[test]
    fn test_walk_pages() {
        let data = make_minimal_pdf();
        let startxref = find_startxref(&data).unwrap();
        let (xref, _) = parse_xref(&data, startxref).unwrap();
        let pages = walk_pages(&data, &xref, 2).unwrap();
        assert_eq!(pages.len(), 2);
        assert_eq!(pages[0], 3);
        assert_eq!(pages[1], 4);
    }

    #[test]
    fn test_find_dict_value_ignores_nested_page_group_type() {
        // WHY: page dictionaries can carry a nested transparency /Group
        // dictionary before their own /Type /Page declaration.
        let page = "<< /Group << /Type /Group /S /Transparency >> /Type /Page >>";
        assert_eq!(find_dict_value(page, "/Type"), Some("/Page"));
    }

    #[test]
    fn test_dictionary_lookup_ignores_delimiters_inside_literals_comments_and_hex() {
        let page = "<< /Note (<< /Type /Wrong >>) % << /Type /Comment >>\n /Blob <3c3c2f547970652f4865783e3e> /Type /Page >>";
        assert_eq!(find_dict_value(page, "/Type"), Some("/Page"));
    }

    #[test]
    fn test_find_in_array_uses_only_top_level_kids() {
        let pages = "<< /Group << /Kids [9 0 R] >> /Kids [3 0 R 4 0 R] >>";
        assert_eq!(
            find_in_array(pages, "/Kids"),
            Some("3 0 R 4 0 R".to_string())
        );
    }

    #[test]
    fn test_subset_materializes_inherited_page_attributes_and_rewrites_parent() {
        let data = make_classic_pdf(
            vec![
                (1, 0, b"1 0 obj\n<< /Type /Catalog /Pages 2 0 R >>\nendobj\n".to_vec()),
                (2, 0, b"2 0 obj\n<< /Type /Pages /Count 1 /Kids [3 0 R] /Resources 7 0 R /MediaBox [0 0 612 792] /Rotate 90 >>\nendobj\n".to_vec()),
                (3, 0, b"3 0 obj\n<< /Type /Pages /Parent 2 0 R /Count 1 /Kids [4 0 R] /CropBox [10 10 600 780] >>\nendobj\n".to_vec()),
                (4, 0, b"4 0 obj\n<< /Type /Page /Parent 3 0 R /Contents 6 0 R >>\nendobj\n".to_vec()),
                (5, 0, b"5 0 obj\n<< /Producer (unused) >>\nendobj\n".to_vec()),
                (6, 0, b"6 0 obj\n<< /Length 12 >>\nstream\nBT (one) ET\nendstream\nendobj\n".to_vec()),
                (7, 0, b"7 0 obj\n<< /Font << /F1 9 0 R >> >>\nendobj\n".to_vec()),
                (8, 0, b"8 0 obj\n<< /Length 12 >>\nstream\nBT (two) ET\nendstream\nendobj\n".to_vec()),
                (9, 0, b"9 0 obj\n<< /Type /Font /Subtype /Type1 /BaseFont /Helvetica >>\nendobj\n".to_vec()),
            ],
            (1, 0),
        );

        let output = PdfWriter::extract_pages_from_bytes(&data, &[1]).unwrap();
        let startxref = find_startxref(&output).unwrap();
        let (xref, trailer) = parse_xref(&output, startxref).unwrap();
        let root = parse_obj_ref(&trailer["/Root"]).unwrap();
        let catalog_bytes = get_object(&output, &xref, root).unwrap();
        let catalog = object_text(&catalog_bytes, root).unwrap();
        let output_pages = parse_obj_ref(find_dict_value(catalog, "/Pages").unwrap()).unwrap();
        let pages_bytes = get_object(&output, &xref, output_pages).unwrap();
        let pages = object_text(&pages_bytes, output_pages).unwrap();
        let child = parse_obj_ref(find_in_array(pages, "/Kids").unwrap().as_str()).unwrap();
        let page_bytes = get_object(&output, &xref, child).unwrap();
        let page = object_text(&page_bytes, child).unwrap();

        assert_eq!(
            parse_obj_ref(find_dict_value(page, "/Parent").unwrap()).unwrap(),
            output_pages
        );
        for key in ["/Resources", "/MediaBox", "/CropBox", "/Rotate"] {
            assert!(
                find_dict_value(page, key).is_some(),
                "missing inherited {key}"
            );
        }
        let resources = parse_obj_ref(find_dict_value(page, "/Resources").unwrap()).unwrap();
        assert!(xref.contains_key(&resources));
        assert!(!output.windows(b"two".len()).any(|window| window == b"two"));
        assert_eq!(reachable_output_objects(&output).len(), xref.len());
    }

    fn reachable_output_objects(data: &[u8]) -> std::collections::HashSet<u32> {
        let parsed = Pdf::parse(data).unwrap();
        let mut reachable = std::collections::HashSet::new();
        collect_refs(&parsed.data, &parsed.xref, parsed.root_obj, &mut reachable).unwrap();
        reachable
    }

    #[test]
    fn test_extract_pages() {
        let tmp_in = tempfile::NamedTempFile::new().unwrap();
        let tmp_out = tempfile::NamedTempFile::new().unwrap();
        std::fs::write(tmp_in.path(), make_minimal_pdf()).unwrap();

        PdfWriter::extract_pages(tmp_in.path(), &[1], tmp_out.path()).unwrap();

        let output = std::fs::read(tmp_out.path()).unwrap();
        assert!(output.starts_with(b"%PDF-1.4"));
        assert!(output.ends_with(b"%%EOF\n"));

        // Verify the output can be parsed back
        let startxref = find_startxref(&output).unwrap();
        let (xref, trailer) = parse_xref(&output, startxref).unwrap();
        assert!(trailer.contains_key("/Root"));
        let root_obj = parse_obj_ref(&trailer["/Root"]).unwrap();
        let catalog = get_object(&output, &xref, root_obj).unwrap();
        assert!(catalog
            .windows(b"/Catalog".len())
            .any(|window| window == b"/Catalog"));
    }

    #[test]
    fn test_extract_all_pages() {
        let tmp_in = tempfile::NamedTempFile::new().unwrap();
        let tmp_out = tempfile::NamedTempFile::new().unwrap();
        std::fs::write(tmp_in.path(), make_minimal_pdf()).unwrap();

        PdfWriter::extract_pages(tmp_in.path(), &[1, 2], tmp_out.path()).unwrap();
        let output = std::fs::read(tmp_out.path()).unwrap();
        assert!(output.starts_with(b"%PDF-1.4"));
    }

    #[test]
    fn test_extract_pages_from_bytes_round_trip() {
        // WHY: upload/queue paths already hold PDF bytes; the in-memory API
        // must produce a parseable subset without a disk round-trip.
        let data = make_minimal_pdf();
        let output = PdfWriter::extract_pages_from_bytes(&data, &[2]).unwrap();
        assert!(output.starts_with(b"%PDF-1.4"));
        assert!(output.ends_with(b"%%EOF\n"));
        let startxref = find_startxref(&output).unwrap();
        let (xref, trailer) = parse_xref(&output, startxref).unwrap();
        assert!(trailer.contains_key("/Root"));
        assert!(xref.contains_key(&1));
    }

    #[test]
    fn test_extract_pages_preserves_invalid_utf8_content_stream_bytes() {
        // WHY: classic-xref PDFs may carry arbitrary binary bytes in streams;
        // extracting a page must neither UTF-8 decode nor alter that payload.
        let (data, content) = make_pdf_with_invalid_utf8_content_stream();

        let output = PdfWriter::extract_pages_from_bytes(&data, &[1]).unwrap();

        assert!(output
            .windows(content.len())
            .any(|window| window == content));
    }

    #[test]
    fn test_extract_pages_preserves_indirect_length_cr_stream_bytes() {
        // WHY: classic PDFs may use an indirect /Length and CR-only stream
        // delimiters; binary content must still be copied exactly.
        let (data, content) = make_pdf_with_indirect_length_and_cr_stream();
        let startxref = find_startxref(&data).unwrap();
        let (xref, _) = parse_xref(&data, startxref).unwrap();
        let stream_object = get_object(&data, &xref, 4).unwrap();
        let range = stream_data_range(&data, &xref, &stream_object)
            .unwrap()
            .unwrap();
        assert_eq!(&stream_object[range], content.as_slice());

        let output = PdfWriter::extract_pages_from_bytes(&data, &[1]).unwrap();

        assert!(output
            .windows(content.len())
            .any(|window| window == content));
    }

    #[test]
    fn test_extract_pages_rejects_empty_selection() {
        let data = make_minimal_pdf();
        let err = PdfWriter::extract_pages_from_bytes(&data, &[]).unwrap_err();
        assert!(err.to_string().contains("no valid pages"));
    }

    #[test]
    fn test_extract_pages_rejects_partially_invalid_selection() {
        let data = make_minimal_pdf();
        let err = PdfWriter::extract_pages_from_bytes(&data, &[1, 3]).unwrap_err();
        assert!(err.to_string().contains("page 3 is out of range"));
        assert!(err.to_string().contains("2 pages"));
    }

    #[test]
    fn test_parse_xref_rejects_compressed_xref_stream() {
        // WHY: PDF 1.5+ files point startxref at an object stream (`N 0 obj`
        // with /Type /XRef). A classic-table parser must fail loud — not
        // return an empty map that would produce a corrupt subset.
        let mut pdf = Vec::from(&b"%PDF-1.5\n"[..]);
        let xref_obj_pos = pdf.len();
        pdf.extend_from_slice(
            b"5 0 obj\n<< /Type /XRef /Length 10 /W [1 2 1] /Size 1 >>\nstream\nxxxxxxxxxx\nendstream\nendobj\n",
        );
        pdf.extend_from_slice(format!("startxref\n{}\n%%EOF\n", xref_obj_pos).as_bytes());
        let startxref = find_startxref(&pdf).unwrap();
        let err = parse_xref(&pdf, startxref).unwrap_err();
        let msg = err.to_string();
        assert!(
            msg.contains("compressed xref") || msg.contains("object stream"),
            "unexpected error: {msg}"
        );
    }

    #[test]
    fn test_looks_like_compressed_xref_detects_obj_header() {
        assert!(looks_like_compressed_xref(b"12 0 obj<< /Type /XRef >>"));
        assert!(!looks_like_compressed_xref(b"xref\n0 1\n"));
    }
}
