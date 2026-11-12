// Rust guideline compliant 2026-02-21
//! Outlook MSG via a zero-copy OLE Compound File Binary reader.
//!
//! No CFB crate: we walk the on-disk FAT / mini-FAT / directory ourselves
//! (MS-CFB + MS-OXMSG). That keeps the hot path as slice indexing plus
//! explicit bounds checks, and it cannot return the whole OLE file as a body.

use crate::{
    html_to_text, parse_email_address, Attachment, Email, EmailAddress, EmailBody, ParseOptions,
    ThreadInfo,
};
use anyhow::{anyhow, Result};
use base64::Engine;
use encoding_rs::Encoding;

const OLE_SIGNATURE: [u8; 8] = [0xd0, 0xcf, 0x11, 0xe0, 0xa1, 0xb1, 0x1a, 0xe1];
const FREESECT: u32 = 0xFFFF_FFFF;
const ENDOFCHAIN: u32 = 0xFFFF_FFFE;
const FATSECT: u32 = 0xFFFF_FFFD;
const DIFSECT: u32 = 0xFFFF_FFFC;
const NOSTREAM: u32 = 0xFFFF_FFFF;
const DIR_ENTRY_SIZE: usize = 128;
const HEADER_SIZE: usize = 512;

const TYPE_STORAGE: u8 = 1;
const TYPE_STREAM: u8 = 2;
const TYPE_ROOT: u8 = 5;

const TAG_SUBJECT: &str = "0037";
const TAG_SENDER_EMAIL: &str = "0C1F";
const TAG_SENDER_NAME: &str = "0C1A";
const TAG_SENT_REPRESENTING_EMAIL: &str = "0065";
const TAG_DISPLAY_CC: &str = "0E03";
const TAG_DISPLAY_TO: &str = "0E04";
const TAG_BODY: &str = "1000";
const TAG_BODY_HTML: &str = "1013";
const TAG_INTERNET_MESSAGE_ID: &str = "1035";
const TAG_DISPLAY_NAME: &str = "3001";
const TAG_ATTACH_DATA_BIN: &str = "3701";
const TAG_ATTACH_FILENAME: &str = "3704";
const TAG_ATTACH_LONG_FILENAME: &str = "3707";
const TAG_ATTACH_MIME_TAG: &str = "370E";
const TAG_ATTACH_CONTENT_ID: &str = "3712";

/// Name prefix of the storage that holds one attachment (MS-OXMSG 2.2.2).
const ATTACHMENT_STORAGE_PREFIX: &str = "__attach_version1.0_";
/// Name of the stream holding an object's fixed-length properties (MS-OXMSG 2.4).
const PROPERTY_STREAM_NAME: &str = "__properties_version1.0";
/// Pseudo-tag under which the root property stream is carried with the
/// `__substg1.0_*` streams. It is not a valid four-hex-digit property ID, so
/// it cannot collide with a real one.
const TAG_PROPERTY_STREAM: &str = "__properties";
/// Size of the property stream header of a top-level message: 8 reserved
/// bytes, four 4-byte counters, 8 reserved bytes (MS-OXMSG 2.4.1.1).
const TOP_LEVEL_PROPERTY_HEADER_LEN: usize = 32;
/// Size of one property stream entry: tag, flags, 8-byte value (MS-OXMSG 2.4.2).
const PROPERTY_ENTRY_LEN: usize = 16;
/// `PidTagMessageCodepage` as PtypInteger32: the code page of the message's
/// non-Unicode (PtypString8) string properties.
const PROP_MESSAGE_CODEPAGE: u32 = 0x3FFD_0003;
/// `PidTagInternetCodepage` as PtypInteger32: the code page of the body
/// streams, including the binary HTML body.
const PROP_INTERNET_CODEPAGE: u32 = 0x3FDE_0003;

pub(crate) fn parse_msg_bytes(bytes: &[u8], options: ParseOptions) -> Result<Email> {
    if bytes.len() < HEADER_SIZE || bytes[..8] != OLE_SIGNATURE {
        return Err(anyhow!("Not a valid MSG file"));
    }
    let file = CfbFile::parse(bytes)?;
    let MessageStreams {
        root: streams,
        attachments: attach_storages,
    } = file.message_streams()?;
    if streams.iter().all(|s| s.tag == TAG_PROPERTY_STREAM) {
        return Err(anyhow!(
            "MSG file has no root __substg1.0_ property streams (not a message object)"
        ));
    }

    let property_stream = first_binary(&streams, TAG_PROPERTY_STREAM);
    let message_codepage = property_stream
        .and_then(|props| fixed_u32_property(props, PROP_MESSAGE_CODEPAGE))
        .and_then(encoding_for_codepage);
    let internet_codepage = property_stream
        .and_then(|props| fixed_u32_property(props, PROP_INTERNET_CODEPAGE))
        .and_then(encoding_for_codepage);
    // Strings follow the message code page and body streams the internet
    // one; each stands in for the other when only one was written.
    let string_encoding = message_codepage.or(internet_codepage);
    let body_encoding = internet_codepage.or(message_codepage);
    let first_string = |streams: &[PropStream], tag: &str| -> Option<String> {
        first_string_with(streams, tag, string_encoding)
    };

    let subject = first_string(&streams, TAG_SUBJECT);
    let from_addr = first_string(&streams, TAG_SENDER_EMAIL)
        .or_else(|| first_string(&streams, TAG_SENT_REPRESENTING_EMAIL));
    let from_name = first_string(&streams, TAG_SENDER_NAME);
    let to_raw = first_string(&streams, TAG_DISPLAY_TO);
    let cc_raw = first_string(&streams, TAG_DISPLAY_CC);
    let mut body_text = first_string_with(&streams, TAG_BODY, body_encoding);
    let body_html = first_string_with(&streams, TAG_BODY_HTML, body_encoding).or_else(|| {
        first_binary(&streams, TAG_BODY_HTML)
            .map(|b| {
                decode_ansi(b, body_encoding)
                    .trim_matches('\0')
                    .trim()
                    .to_string()
            })
            .filter(|s| !s.is_empty())
    });
    if options.html_to_text && body_text.is_none() {
        if let Some(html) = &body_html {
            body_text = Some(html_to_text(html));
        }
    }
    let message_id = first_string(&streams, TAG_INTERNET_MESSAGE_ID);

    let from = match (from_addr, from_name) {
        (Some(address), name) => EmailAddress { name, address },
        (None, Some(name)) => EmailAddress {
            name: Some(name),
            address: String::new(),
        },
        (None, None) => EmailAddress::default(),
    };

    let parse_addr_list = |raw_opt: Option<String>| -> Vec<EmailAddress> {
        raw_opt
            .map(|raw| {
                raw.split(';')
                    .map(str::trim)
                    .filter(|part| !part.is_empty())
                    .map(|part| {
                        parse_email_address(part).unwrap_or(EmailAddress {
                            name: None,
                            address: part.to_string(),
                        })
                    })
                    .collect()
            })
            .unwrap_or_default()
    };

    let to = parse_addr_list(to_raw);
    let cc = parse_addr_list(cc_raw);

    let mut attachments = Vec::new();
    let mut total_attachment_bytes = 0usize;
    for (idx, att_streams) in attach_storages.into_iter().enumerate() {
        let Some(raw_bytes) = first_binary(&att_streams, TAG_ATTACH_DATA_BIN) else {
            continue;
        };
        let size = raw_bytes.len();
        if size > options.max_attachment_size {
            continue;
        }
        let filename = first_string(&att_streams, TAG_ATTACH_LONG_FILENAME)
            .or_else(|| first_string(&att_streams, TAG_ATTACH_FILENAME))
            .or_else(|| first_string(&att_streams, TAG_DISPLAY_NAME))
            .unwrap_or_else(|| format!("attachment_{idx}.bin"));
        let content_type = first_string(&att_streams, TAG_ATTACH_MIME_TAG)
            .unwrap_or_else(|| infer_mime_type(&filename, raw_bytes));
        let content_id = first_string(&att_streams, TAG_ATTACH_CONTENT_ID);
        let is_inline = content_id.is_some();

        let content = if options.extract_attachment_content
            && total_attachment_bytes.saturating_add(size) <= options.max_total_attachment_bytes
        {
            total_attachment_bytes += size;
            Some(base64::engine::general_purpose::STANDARD.encode(raw_bytes))
        } else {
            None
        };

        attachments.push(Attachment {
            filename,
            content_type,
            size,
            content_id,
            is_inline,
            content,
        });
    }

    Ok(Email {
        message_id,
        from,
        reply_to: None,
        to,
        cc,
        bcc: vec![],
        subject,
        date: None,
        body: EmailBody {
            text: body_text,
            html: body_html,
        },
        attachments,
        thread_info: ThreadInfo::default(),
        priority: None,
        headers: None,
    })
}

fn infer_mime_type(filename: &str, bytes: &[u8]) -> String {
    let lower = filename.to_ascii_lowercase();
    if lower.ends_with(".pdf") || bytes.starts_with(b"%PDF") {
        "application/pdf".to_string()
    } else if lower.ends_with(".png") || bytes.starts_with(b"\x89PNG") {
        "image/png".to_string()
    } else if lower.ends_with(".jpg") || lower.ends_with(".jpeg") {
        "image/jpeg".to_string()
    } else if lower.ends_with(".docx") {
        "application/vnd.openxmlformats-officedocument.wordprocessingml.document".to_string()
    } else if lower.ends_with(".xlsx") {
        "application/vnd.openxmlformats-officedocument.spreadsheetml.sheet".to_string()
    } else if lower.ends_with(".csv") {
        "text/csv".to_string()
    } else {
        "application/octet-stream".to_string()
    }
}

struct CfbFile<'a> {
    data: &'a [u8],
    sector_size: usize,
    mini_sector_size: usize,
    mini_cutoff: u64,
    fat: Vec<u32>,
    mini_fat: Vec<u32>,
    directory: Vec<DirEntry>,
    mini_stream: Vec<u8>,
}

struct DirEntry {
    name: String,
    obj_type: u8,
    left: u32,
    right: u32,
    child: u32,
    start_sector: u32,
    stream_size: u64,
}

struct PropStream {
    tag: String,
    kind: PropKind,
    bytes: Vec<u8>,
}

enum PropKind {
    Unicode,
    Ansi,
    Binary,
}

/// The property streams of a message object, grouped by the storage they
/// were found in.
#[derive(Default)]
struct MessageStreams {
    /// Streams directly under the root storage: the message's own properties.
    root: Vec<PropStream>,
    /// One entry per `__attach_version1.0_#*` storage, in directory order.
    attachments: Vec<Vec<PropStream>>,
}

/// Marks the sectors one chain has visited, and is reused from chain to chain.
///
/// A chain that returns to a sector it already read is a cycle. Each chain
/// takes a fresh generation number instead of a cleared table, so reading
/// every stream of a file costs the sectors read, not streams times table
/// size.
struct ChainGuard {
    stamps: Vec<u32>,
    generation: u32,
}

impl ChainGuard {
    /// Creates a guard for chains over `sectors` sectors.
    ///
    /// Callers pass a count the file itself bounds (its sectors, or the
    /// entries of an allocation table already read from it).
    fn new(sectors: usize) -> Self {
        Self {
            stamps: vec![0; sectors],
            generation: 0,
        }
    }

    /// Starts a new chain, forgetting the previous chain's visits.
    fn start_chain(&mut self) {
        self.generation = self.generation.wrapping_add(1);
        if self.generation == 0 {
            // Generation numbers wrapped; stale stamps could match again.
            self.stamps.fill(0);
            self.generation = 1;
        }
    }

    /// Records a visit and reports whether it is this chain's first.
    ///
    /// A sector past the table is always a first visit: it lies outside the
    /// file or outside the allocation table, so the chain cannot continue
    /// through it and cannot loop on it.
    fn first_visit(&mut self, sector: u32) -> bool {
        match self.stamps.get_mut(sector as usize) {
            Some(stamp) if *stamp == self.generation => false,
            Some(stamp) => {
                *stamp = self.generation;
                true
            }
            None => true,
        }
    }
}

/// What reading the streams of one file shares: cycle guards for both
/// allocation tables and the bytes streams may still claim.
struct StreamReader {
    fat_guard: ChainGuard,
    mini_guard: ChainGuard,
    /// Stream bytes that may still be copied out of the file.
    ///
    /// Streams of a well-formed file occupy disjoint sectors, so together
    /// they cannot hold more bytes than the file. Without this, a directory
    /// whose entries all name the same chain yields one copy of it per entry.
    budget: usize,
}

impl<'a> CfbFile<'a> {
    fn parse(data: &'a [u8]) -> Result<Self> {
        if u16_le(data, 0x1C)? != 0xFFFE {
            return Err(anyhow!("MSG CFB: unexpected byte order"));
        }
        let major_version = u16_le(data, 0x1A)?;
        let sector_shift = u16_le(data, 0x1E)? as u32;
        let mini_shift = u16_le(data, 0x20)? as u32;
        if !(9..=12).contains(&sector_shift) || mini_shift == 0 || mini_shift >= sector_shift {
            return Err(anyhow!("MSG CFB: invalid sector shift"));
        }
        let sector_size = 1usize << sector_shift;
        let mini_sector_size = 1usize << mini_shift;
        let mini_cutoff = u32_le(data, 0x38)? as u64;
        let first_dir = u32_le(data, 0x30)?;
        let first_mini_fat = u32_le(data, 0x3C)?;
        let first_difat = u32_le(data, 0x44)?;
        // The header also carries FAT, mini FAT and DIFAT sector counts. They
        // are not read: the chains themselves say how long they are, and a
        // count is a number the file's author chose.

        let fat = load_fat(data, sector_size, first_difat)?;
        let directory = load_directory(data, sector_size, &fat, first_dir, major_version)?;
        let mini_fat = if first_mini_fat == ENDOFCHAIN || first_mini_fat == FREESECT {
            Vec::new()
        } else {
            let raw = read_fat_chain(data, sector_size, &fat, first_mini_fat)?;
            parse_u32_le_table(&raw)
        };
        let mini_stream = if let Some(root) = directory.first() {
            if root.stream_size == 0 || root.start_sector == ENDOFCHAIN {
                Vec::new()
            } else {
                read_fat_chain_len(
                    data,
                    sector_size,
                    &fat,
                    root.start_sector,
                    usize::try_from(root.stream_size).unwrap_or(usize::MAX),
                    &mut ChainGuard::new(fat.len().min(sector_count(data, sector_size))),
                )?
            }
        } else {
            Vec::new()
        };

        Ok(Self {
            data,
            sector_size,
            mini_sector_size,
            mini_cutoff,
            fat,
            mini_fat,
            directory,
            mini_stream,
        })
    }

    #[cfg(test)]
    fn root_substg_streams(&self) -> Result<Vec<PropStream>> {
        self.message_streams().map(|message| message.root)
    }

    /// Collects the property streams of the message and of its attachments.
    ///
    /// Only the root storage and the attachment storages directly under it are
    /// read. Other storages (recipients, named properties, embedded messages)
    /// are not entered, so their streams cannot pass for the message's own.
    fn message_streams(&self) -> Result<MessageStreams> {
        let Some(root) = self.directory.first() else {
            return Err(anyhow!("MSG CFB: empty directory"));
        };
        // A malformed directory can point `left`/`right`/`child` back into the
        // tree; every entry may be reached once.
        let mut visited = vec![false; self.directory.len()];
        let mut reader = StreamReader {
            fat_guard: ChainGuard::new(
                self.fat
                    .len()
                    .min(sector_count(self.data, self.sector_size)),
            ),
            mini_guard: ChainGuard::new(
                self.mini_fat
                    .len()
                    .min(self.mini_stream.len() / self.mini_sector_size),
            ),
            budget: self.data.len(),
        };
        let mut message = MessageStreams::default();
        for idx in self.siblings_in_order(root.child, &mut visited)? {
            let entry = &self.directory[idx];
            if entry.obj_type == TYPE_STREAM {
                message.root.extend(self.prop_stream(entry, &mut reader)?);
            } else if entry.obj_type == TYPE_STORAGE
                && entry.child != NOSTREAM
                && entry.name.starts_with(ATTACHMENT_STORAGE_PREFIX)
            {
                let mut streams = Vec::new();
                for child_idx in self.siblings_in_order(entry.child, &mut visited)? {
                    let child = &self.directory[child_idx];
                    if child.obj_type == TYPE_STREAM {
                        streams.extend(self.prop_stream(child, &mut reader)?);
                    }
                }
                message.attachments.push(streams);
            }
        }
        Ok(message)
    }

    /// Lists the entries of one storage in directory (in-order) sequence.
    ///
    /// A storage's entries form a binary tree linked through `left` and
    /// `right`, rooted at `first`. The tree's shape is the file's choice (a
    /// writer may emit one long chain), so it is walked with an explicit
    /// stack rather than by recursion.
    fn siblings_in_order(&self, first: u32, visited: &mut [bool]) -> Result<Vec<usize>> {
        let mut order = Vec::new();
        let mut pending = Vec::new();
        let mut next = first;
        loop {
            while next != NOSTREAM {
                let idx = next as usize;
                let (Some(entry), Some(slot)) = (self.directory.get(idx), visited.get_mut(idx))
                else {
                    return Err(anyhow!("MSG CFB: directory index {next} out of range"));
                };
                if *slot {
                    return Err(anyhow!("MSG CFB: directory cycle at index {next}"));
                }
                *slot = true;
                pending.push(idx);
                next = entry.left;
            }
            let Some(idx) = pending.pop() else {
                break;
            };
            order.push(idx);
            next = self.directory[idx].right;
        }
        Ok(order)
    }

    /// Reads `entry` if it is a property stream this parser understands.
    fn prop_stream(
        &self,
        entry: &DirEntry,
        reader: &mut StreamReader,
    ) -> Result<Option<PropStream>> {
        let (tag, kind) = if let Some(parsed) = parse_substg_name(&entry.name) {
            parsed
        } else if entry.name == PROPERTY_STREAM_NAME {
            (TAG_PROPERTY_STREAM.to_string(), PropKind::Binary)
        } else {
            return Ok(None);
        };
        let bytes = self.read_stream(entry, reader)?;
        Ok(Some(PropStream { tag, kind, bytes }))
    }

    fn read_stream(&self, entry: &DirEntry, reader: &mut StreamReader) -> Result<Vec<u8>> {
        // The declared size is checked against what the file can still hold
        // before anything is sized or copied from it.
        let len = usize::try_from(entry.stream_size)
            .ok()
            .filter(|len| *len <= reader.budget)
            .ok_or_else(|| {
                anyhow!(
                    "MSG CFB: stream {:?} declares {} bytes, more than the file holds",
                    entry.name,
                    entry.stream_size
                )
            })?;
        let bytes = if entry.stream_size < self.mini_cutoff && !self.mini_stream.is_empty() {
            read_mini_chain(
                &self.mini_stream,
                &self.mini_fat,
                self.mini_sector_size,
                entry.start_sector,
                len,
                &mut reader.mini_guard,
            )?
        } else {
            read_fat_chain_len(
                self.data,
                self.sector_size,
                &self.fat,
                entry.start_sector,
                len,
                &mut reader.fat_guard,
            )?
        };
        reader.budget -= bytes.len();
        Ok(bytes)
    }
}

/// Number of whole sectors `data` holds after the header.
fn sector_count(data: &[u8], sector_size: usize) -> usize {
    data.len().saturating_sub(HEADER_SIZE) / sector_size
}

/// Reads the file allocation table named by the header and the DIFAT chain.
///
/// Every FAT and DIFAT sector may be named once, and must lie inside the
/// file, so the table cannot be larger than the file that holds it. The
/// header's own sector counts are not trusted for anything.
fn load_fat(data: &[u8], sector_size: usize, first_difat: u32) -> Result<Vec<u32>> {
    /// FAT sector slots in the header, before any DIFAT sector (MS-CFB 2.2).
    const HEADER_DIFAT_ENTRIES: usize = 109;

    let sectors = sector_count(data, sector_size);
    let mut is_fat_sector = vec![false; sectors];
    let mut fat_sectors: Vec<u32> = Vec::new();
    let mut add_fat_sector = |sect: u32| -> Result<()> {
        if sect == FREESECT {
            return Ok(());
        }
        let slot = is_fat_sector
            .get_mut(sect as usize)
            .ok_or_else(|| anyhow!("MSG CFB: sector {sect} truncated"))?;
        if *slot {
            return Err(anyhow!("MSG CFB: FAT sector {sect} listed twice"));
        }
        *slot = true;
        fat_sectors.push(sect);
        Ok(())
    };

    for i in 0..HEADER_DIFAT_ENTRIES {
        add_fat_sector(u32_le(data, 0x4C + i * 4)?)?;
    }
    let mut is_difat_sector = vec![false; sectors];
    let mut difat_sect = first_difat;
    while difat_sect != ENDOFCHAIN && difat_sect != FREESECT {
        let raw = sector_slice(data, sector_size, difat_sect)?;
        match is_difat_sector.get_mut(difat_sect as usize) {
            Some(slot) if *slot => {
                return Err(anyhow!("MSG CFB: DIFAT cycle at sector {difat_sect}"));
            }
            Some(slot) => *slot = true,
            // `sector_slice` succeeded, so the sector is inside the file.
            None => return Err(anyhow!("MSG CFB: sector {difat_sect} truncated")),
        }
        let (entries, next) = raw
            .as_chunks::<4>()
            .0
            .split_last()
            .map(|(next, entries)| (entries, u32::from_le_bytes(*next)))
            .ok_or_else(|| anyhow!("MSG CFB: empty DIFAT sector"))?;
        for entry in entries {
            add_fat_sector(u32::from_le_bytes(*entry))?;
        }
        difat_sect = next;
    }

    let mut fat = Vec::with_capacity(fat_sectors.len() * (sector_size / 4));
    for sect in fat_sectors {
        let raw = sector_slice(data, sector_size, sect)?;
        fat.extend(parse_u32_le_table(raw));
    }
    Ok(fat)
}

fn load_directory(
    data: &[u8],
    sector_size: usize,
    fat: &[u32],
    first_dir: u32,
    major_version: u16,
) -> Result<Vec<DirEntry>> {
    let raw = read_fat_chain(data, sector_size, fat, first_dir)?;
    if raw.len() < DIR_ENTRY_SIZE {
        return Err(anyhow!("MSG CFB: truncated directory"));
    }
    let mut out = Vec::with_capacity(raw.len() / DIR_ENTRY_SIZE);
    for chunk in raw.as_chunks::<DIR_ENTRY_SIZE>().0 {
        let obj_type = chunk[0x42];
        let name_len = u16_le(chunk, 0x40)? as usize;
        let name = if obj_type == 0 || !(2..=64).contains(&name_len) {
            String::new()
        } else {
            let units = name_len / 2 - 1;
            let mut utf16 = Vec::with_capacity(units);
            for i in 0..units {
                utf16.push(u16_le(chunk, i * 2)?);
            }
            String::from_utf16_lossy(&utf16)
        };
        let mut stream_size = u64_le(chunk, 0x78)?;
        if major_version == 3 {
            // A version 3 file stores a 32-bit size; writers are known to
            // leave the high half uninitialized, and readers are told to
            // ignore it (MS-CFB 2.6.3).
            stream_size &= 0xFFFF_FFFF;
        }
        out.push(DirEntry {
            name,
            obj_type,
            left: u32_le(chunk, 0x44)?,
            right: u32_le(chunk, 0x48)?,
            child: u32_le(chunk, 0x4C)?,
            start_sector: u32_le(chunk, 0x74)?,
            stream_size,
        });
    }
    if out.is_empty() || out[0].obj_type != TYPE_ROOT {
        return Err(anyhow!("MSG CFB: missing root directory entry"));
    }
    Ok(out)
}

/// Reads a whole FAT chain: the directory or the mini FAT.
fn read_fat_chain(data: &[u8], sector_size: usize, fat: &[u32], start: u32) -> Result<Vec<u8>> {
    let mut guard = ChainGuard::new(fat.len().min(sector_count(data, sector_size)));
    read_fat_chain_len(data, sector_size, fat, start, usize::MAX, &mut guard)
}

/// Reads up to `max_len` bytes of the FAT chain that begins at `start`.
///
/// The result grows only by sectors actually read, and `guard` stops a chain
/// at the first sector it repeats, so the output is never larger than the
/// file whatever `max_len` says.
fn read_fat_chain_len(
    data: &[u8],
    sector_size: usize,
    fat: &[u32],
    start: u32,
    max_len: usize,
    guard: &mut ChainGuard,
) -> Result<Vec<u8>> {
    let mut out = Vec::new();
    if max_len == 0 {
        return Ok(out);
    }
    guard.start_chain();
    let mut sect = start;
    while sect != ENDOFCHAIN && sect != FREESECT {
        // Exact cycle detection, not a hop ceiling: with `max_len` unbounded
        // a two-sector loop would otherwise append a sector per hop.
        if !guard.first_visit(sect) {
            return Err(anyhow!("MSG CFB: FAT cycle at sector {sect}"));
        }
        if sect == FATSECT || sect == DIFSECT {
            return Err(anyhow!("MSG CFB: FAT points at metadata sector"));
        }
        let raw = sector_slice(data, sector_size, sect)?;
        let take = (max_len - out.len()).min(raw.len());
        out.extend_from_slice(&raw[..take]);
        if out.len() == max_len {
            break;
        }
        sect = fat_next(fat, sect)?;
    }
    Ok(out)
}

/// Reads up to `max_len` bytes of the mini FAT chain that begins at `start`.
///
/// Like [`read_fat_chain_len`], the output is bounded by the mini stream it
/// is copied from, not by `max_len`.
fn read_mini_chain(
    mini_stream: &[u8],
    mini_fat: &[u32],
    mini_sector_size: usize,
    start: u32,
    max_len: usize,
    guard: &mut ChainGuard,
) -> Result<Vec<u8>> {
    let mut out = Vec::with_capacity(max_len.min(mini_stream.len()));
    if max_len == 0 {
        return Ok(out);
    }
    guard.start_chain();
    let mut sect = start;
    while sect != ENDOFCHAIN && sect != FREESECT {
        if !guard.first_visit(sect) {
            return Err(anyhow!("MSG CFB: mini-FAT cycle at sector {sect}"));
        }
        let raw = (sect as usize)
            .checked_mul(mini_sector_size)
            .and_then(|off| Some(off..off.checked_add(mini_sector_size)?))
            .and_then(|range| mini_stream.get(range))
            .ok_or_else(|| anyhow!("MSG CFB: mini sector {sect} past mini stream"))?;
        let take = (max_len - out.len()).min(raw.len());
        out.extend_from_slice(&raw[..take]);
        if out.len() == max_len {
            break;
        }
        sect = fat_next(mini_fat, sect)?;
    }
    Ok(out)
}

fn fat_next(fat: &[u32], sect: u32) -> Result<u32> {
    fat.get(sect as usize)
        .copied()
        .ok_or_else(|| anyhow!("MSG CFB: sector {sect} missing from FAT"))
}

fn sector_slice(data: &[u8], sector_size: usize, sect: u32) -> Result<&[u8]> {
    (sect as usize)
        .checked_mul(sector_size)
        .and_then(|rel| rel.checked_add(HEADER_SIZE))
        .and_then(|off| Some(off..off.checked_add(sector_size)?))
        .and_then(|range| data.get(range))
        .ok_or_else(|| anyhow!("MSG CFB: sector {sect} truncated"))
}

fn parse_u32_le_table(raw: &[u8]) -> Vec<u32> {
    raw.as_chunks::<4>()
        .0
        .iter()
        .map(|chunk| u32::from_le_bytes(*chunk))
        .collect()
}

fn u16_le(buf: &[u8], off: usize) -> Result<u16> {
    buf.get(off..)
        .and_then(|tail| tail.first_chunk::<2>())
        .map(|bytes| u16::from_le_bytes(*bytes))
        .ok_or_else(|| anyhow!("MSG CFB: truncated u16 at {off}"))
}

fn u32_le(buf: &[u8], off: usize) -> Result<u32> {
    buf.get(off..)
        .and_then(|tail| tail.first_chunk::<4>())
        .map(|bytes| u32::from_le_bytes(*bytes))
        .ok_or_else(|| anyhow!("MSG CFB: truncated u32 at {off}"))
}

fn u64_le(buf: &[u8], off: usize) -> Result<u64> {
    buf.get(off..)
        .and_then(|tail| tail.first_chunk::<8>())
        .map(|bytes| u64::from_le_bytes(*bytes))
        .ok_or_else(|| anyhow!("MSG CFB: truncated u64 at {off}"))
}

/// Splits a `__substg1.0_<tag><type>` stream name into its tag and type.
fn parse_substg_name(name: &str) -> Option<(String, PropKind)> {
    let rest = name.strip_prefix("__substg1.0_")?;
    // `get`, not indexing: a directory name is arbitrary UTF-16 text, so byte
    // 4 or 8 can fall inside a multi-byte character.
    let tag = rest.get(..4)?;
    let kind = rest.get(4..8)?;
    if !tag.bytes().all(|byte| byte.is_ascii_hexdigit()) {
        return None;
    }
    let kind = match kind.to_ascii_uppercase().as_str() {
        "001F" => PropKind::Unicode,
        "001E" => PropKind::Ansi,
        "0102" => PropKind::Binary,
        _ => return None,
    };
    Some((tag.to_ascii_uppercase(), kind))
}

fn first_binary<'a>(streams: &'a [PropStream], tag: &str) -> Option<&'a [u8]> {
    streams
        .iter()
        .find(|s| s.tag.eq_ignore_ascii_case(tag) && matches!(s.kind, PropKind::Binary))
        .map(|s| s.bytes.as_slice())
}

/// Reads a string property, preferring its Unicode stream over the ANSI one.
///
/// `ansi_encoding` is the code page the message declares for its non-Unicode
/// strings, if any; see [`decode_ansi`].
fn first_string_with(
    streams: &[PropStream],
    tag: &str,
    ansi_encoding: Option<&'static Encoding>,
) -> Option<String> {
    let unicode = streams
        .iter()
        .find(|s| s.tag.eq_ignore_ascii_case(tag) && matches!(s.kind, PropKind::Unicode));
    if let Some(stream) = unicode {
        return decode_prop(&stream.bytes, &stream.kind, ansi_encoding);
    }
    streams
        .iter()
        .find(|s| s.tag.eq_ignore_ascii_case(tag) && matches!(s.kind, PropKind::Ansi))
        .and_then(|stream| decode_prop(&stream.bytes, &stream.kind, ansi_encoding))
}

/// Finds a 4-byte fixed-length property in a top-level property stream.
///
/// `tag` is the full property tag (property ID in the high word, property
/// type in the low word), stored little-endian at the start of each entry
/// with the value in the entry's last eight bytes.
fn fixed_u32_property(property_stream: &[u8], tag: u32) -> Option<u32> {
    property_stream
        .get(TOP_LEVEL_PROPERTY_HEADER_LEN..)?
        .as_chunks::<PROPERTY_ENTRY_LEN>()
        .0
        .iter()
        .find(|entry| u32_le(entry.as_slice(), 0).ok() == Some(tag))
        .and_then(|entry| u32_le(entry.as_slice(), 8).ok())
}

/// Maps a Windows code page identifier to its decoder.
///
/// Covers the code pages Outlook writes for `PidTagMessageCodepage` and
/// `PidTagInternetCodepage`. `encoding_rs` looks encodings up by WHATWG label,
/// not by number, hence the explicit table. US-ASCII (20127) and Latin-1
/// (28591) map to windows-1252, their superset, as the WHATWG standard does.
fn encoding_for_codepage(codepage: u32) -> Option<&'static Encoding> {
    use encoding_rs as enc;
    Some(match codepage {
        65001 => enc::UTF_8,
        1250 => enc::WINDOWS_1250,
        1251 => enc::WINDOWS_1251,
        1252 | 20127 | 28591 => enc::WINDOWS_1252,
        1253 => enc::WINDOWS_1253,
        1254 | 28599 => enc::WINDOWS_1254,
        1255 => enc::WINDOWS_1255,
        1256 => enc::WINDOWS_1256,
        1257 => enc::WINDOWS_1257,
        1258 => enc::WINDOWS_1258,
        874 => enc::WINDOWS_874,
        866 => enc::IBM866,
        932 => enc::SHIFT_JIS,
        936 => enc::GBK,
        949 | 51949 => enc::EUC_KR,
        950 => enc::BIG5,
        10000 => enc::MACINTOSH,
        20866 => enc::KOI8_R,
        21866 => enc::KOI8_U,
        28592 => enc::ISO_8859_2,
        28593 => enc::ISO_8859_3,
        28594 => enc::ISO_8859_4,
        28595 => enc::ISO_8859_5,
        28596 => enc::ISO_8859_6,
        28597 => enc::ISO_8859_7,
        28598 | 38598 => enc::ISO_8859_8,
        28603 => enc::ISO_8859_13,
        28605 => enc::ISO_8859_15,
        50220..=50222 => enc::ISO_2022_JP,
        51932 => enc::EUC_JP,
        54936 => enc::GB18030,
        _ => return None,
    })
}

/// Decodes a non-Unicode (PtypString8 or binary body) value.
///
/// Valid UTF-8 is taken as UTF-8, since writers that ignore the declared code
/// page almost always emit UTF-8. Otherwise the declared code page decides,
/// and windows-1252 is the last resort: it maps every byte, so nothing
/// degrades to U+FFFD the way a UTF-8 lossy read does.
fn decode_ansi(bytes: &[u8], declared: Option<&'static Encoding>) -> String {
    if let Ok(text) = std::str::from_utf8(bytes) {
        return text.to_string();
    }
    match declared {
        Some(encoding) if encoding != encoding_rs::UTF_8 => {
            encoding.decode_without_bom_handling(bytes).0.into_owned()
        }
        _ => crate::decode_unlabeled_bytes(bytes),
    }
}

fn decode_prop(
    bytes: &[u8],
    kind: &PropKind,
    ansi_encoding: Option<&'static Encoding>,
) -> Option<String> {
    let text = match kind {
        PropKind::Unicode => {
            if bytes.len() < 2 {
                return None;
            }
            let units: Vec<u16> = bytes
                .as_chunks::<2>()
                .0
                .iter()
                .map(|chunk| u16::from_le_bytes(*chunk))
                .collect();
            String::from_utf16_lossy(&units)
        }
        PropKind::Ansi => decode_ansi(bytes, ansi_encoding),
        PropKind::Binary => return None,
    };
    let trimmed = text.trim_matches('\0').trim();
    if trimmed.is_empty() {
        None
    } else {
        Some(trimmed.to_string())
    }
}

/// Build a v3 CFB with mini-stream `__substg1.0_*` properties (real MSG layout).
#[doc(hidden)]
pub fn write_fixture_msg(subject: &str, sender: &str, to: &str, body: &str) -> Vec<u8> {
    let props = [
        ("__substg1.0_0037001F", utf16_le(subject)),
        ("__substg1.0_0C1F001F", utf16_le(sender)),
        ("__substg1.0_0E04001F", utf16_le(to)),
        ("__substg1.0_1000001F", utf16_le(body)),
    ];
    write_ministream_cfb(&props)
}

/// Builds a fixture compound file from arbitrary named streams.
///
/// See [`write_ministream_cfb`] for the `storage/stream` naming.
#[doc(hidden)]
pub fn write_fixture_streams(streams: &[(&str, Vec<u8>)]) -> Vec<u8> {
    write_ministream_cfb(streams)
}

fn utf16_le(value: &str) -> Vec<u8> {
    let mut out = Vec::new();
    for unit in value.encode_utf16() {
        out.extend_from_slice(&unit.to_le_bytes());
    }
    out.extend_from_slice(&0u16.to_le_bytes());
    out
}

/// Fixture streams at least this long are stored in regular sectors; shorter
/// ones go to the mini stream. This is the cutoff the fixture header declares.
const FIXTURE_MINI_CUTOFF: usize = 4096;

/// One directory entry of a fixture compound file.
struct FixtureNode<'a> {
    name: &'a str,
    obj_type: u8,
    /// Index into the caller's stream list, for stream entries.
    stream: Option<usize>,
    /// Directory indices of the entries this storage holds, in sibling order.
    children: Vec<usize>,
}

/// Builds a version 3 compound file holding `streams`.
///
/// A name of the form `storage/stream` puts the stream in a storage directly
/// under the root, created the first time it is named; that is how attachment
/// (`__attach_version1.0_#00000000/...`) and recipient storages are written.
/// Siblings are chained through their `right` links in the order given.
///
/// # Panics
///
/// Panics if the file would need more FAT sectors than the header can list.
/// This is a test fixture writer; its inputs are written by hand.
fn write_ministream_cfb(streams: &[(&str, Vec<u8>)]) -> Vec<u8> {
    const SECTOR: usize = 512;
    const MINI: usize = 64;
    const ENTRIES_PER_SECTOR: usize = SECTOR / 4;
    /// FAT sector slots in the header; the fixture writes no DIFAT sectors.
    const HEADER_FAT_SLOTS: usize = 109;

    // Directory: entry 0 is the root, the rest follow in creation order.
    let mut nodes = vec![FixtureNode {
        name: "Root Entry",
        obj_type: TYPE_ROOT,
        stream: None,
        children: Vec::new(),
    }];
    for (index, (path, _)) in streams.iter().enumerate() {
        let (parent, leaf) = match path.split_once('/') {
            Some((storage, leaf)) => {
                let existing =
                    nodes[0].children.iter().copied().find(|&id| {
                        nodes[id].obj_type == TYPE_STORAGE && nodes[id].name == storage
                    });
                let parent = match existing {
                    Some(id) => id,
                    None => {
                        let id = nodes.len();
                        nodes.push(FixtureNode {
                            name: storage,
                            obj_type: TYPE_STORAGE,
                            stream: None,
                            children: Vec::new(),
                        });
                        nodes[0].children.push(id);
                        id
                    }
                };
                (parent, leaf)
            }
            None => (0, *path),
        };
        let id = nodes.len();
        nodes.push(FixtureNode {
            name: leaf,
            obj_type: TYPE_STREAM,
            stream: Some(index),
            children: Vec::new(),
        });
        nodes[parent].children.push(id);
    }

    // Stream data: small streams are packed into the mini stream, large ones
    // into their own run of regular sectors. `starts` is relative to the
    // owning area until the layout is known.
    let mut mini_stream: Vec<u8> = Vec::new();
    let mut mini_fat: Vec<u32> = Vec::new();
    let mut large_data: Vec<u8> = Vec::new();
    let mut large_runs: Vec<(usize, usize)> = Vec::new();
    let mut starts = vec![0usize; streams.len()];
    for (index, (_, data)) in streams.iter().enumerate() {
        if data.len() >= FIXTURE_MINI_CUTOFF {
            let count = data.len().div_ceil(SECTOR);
            starts[index] = large_data.len() / SECTOR;
            large_runs.push((starts[index], count));
            large_data.extend_from_slice(data);
            large_data.resize((starts[index] + count) * SECTOR, 0);
        } else {
            // An empty stream still owns one mini sector.
            let count = data.len().div_ceil(MINI).max(1);
            let first = mini_fat.len();
            starts[index] = first;
            for offset in 0..count {
                mini_fat.push(if offset + 1 == count {
                    ENDOFCHAIN
                } else {
                    (first + offset + 1) as u32
                });
            }
            mini_stream.extend_from_slice(data);
            mini_stream.resize((first + count) * MINI, 0);
        }
    }

    // Layout after the 512-byte header: FAT, directory, mini FAT, the mini
    // stream container, then the large streams.
    let dir_sectors = (nodes.len() * DIR_ENTRY_SIZE).div_ceil(SECTOR);
    let mini_fat_sectors = mini_fat.len().div_ceil(ENTRIES_PER_SECTOR).max(1);
    let mini_stream_sectors = mini_stream.len().div_ceil(SECTOR);
    let large_sectors = large_data.len() / SECTOR;
    let payload_sectors = dir_sectors + mini_fat_sectors + mini_stream_sectors + large_sectors;
    let mut fat_sectors = 1;
    while fat_sectors * ENTRIES_PER_SECTOR < fat_sectors + payload_sectors {
        fat_sectors += 1;
    }
    assert!(
        fat_sectors <= HEADER_FAT_SLOTS,
        "fixture too large for a header-only DIFAT"
    );
    let dir_start = fat_sectors;
    let mini_fat_start = dir_start + dir_sectors;
    let mini_data_start = mini_fat_start + mini_fat_sectors;
    let large_start = mini_data_start + mini_stream_sectors;
    let total_sectors = large_start + large_sectors;

    let mut fat = vec![FREESECT; fat_sectors * ENTRIES_PER_SECTOR];
    fat[..fat_sectors].fill(FATSECT);
    let mut chain = |start: usize, count: usize| {
        for offset in 0..count {
            fat[start + offset] = if offset + 1 == count {
                ENDOFCHAIN
            } else {
                (start + offset + 1) as u32
            };
        }
    };
    chain(dir_start, dir_sectors);
    chain(mini_fat_start, mini_fat_sectors);
    chain(mini_data_start, mini_stream_sectors);
    for (first, count) in &large_runs {
        chain(large_start + first, *count);
    }

    let mut dir = vec![0u8; dir_sectors * SECTOR];
    for (id, node) in nodes.iter().enumerate() {
        let (start, size) = match node.stream {
            Some(index) => {
                let len = streams[index].1.len();
                let start = if len >= FIXTURE_MINI_CUTOFF {
                    large_start + starts[index]
                } else {
                    starts[index]
                };
                (start as u32, len as u64)
            }
            None if id == 0 && mini_stream_sectors > 0 => {
                (mini_data_start as u32, mini_stream.len() as u64)
            }
            None if id == 0 => (ENDOFCHAIN, 0),
            None => (0, 0),
        };
        write_dir_entry(
            &mut dir,
            id,
            DirWrite {
                name: node.name,
                obj_type: node.obj_type,
                left: NOSTREAM,
                right: NOSTREAM,
                child: node
                    .children
                    .first()
                    .map_or(NOSTREAM, |&child| child as u32),
                start,
                size,
            },
        );
    }
    // Chain each storage's entries through their `right` links.
    for node in &nodes {
        for pair in node.children.windows(2) {
            let off = pair[0] * DIR_ENTRY_SIZE + 0x48;
            dir[off..off + 4].copy_from_slice(&(pair[1] as u32).to_le_bytes());
        }
    }

    let mut out = vec![0u8; HEADER_SIZE + total_sectors * SECTOR];
    out[..8].copy_from_slice(&OLE_SIGNATURE);
    out[0x18..0x1A].copy_from_slice(&0x003Eu16.to_le_bytes());
    out[0x1A..0x1C].copy_from_slice(&0x0003u16.to_le_bytes());
    out[0x1C..0x1E].copy_from_slice(&0xFFFEu16.to_le_bytes());
    out[0x1E..0x20].copy_from_slice(&9u16.to_le_bytes());
    out[0x20..0x22].copy_from_slice(&6u16.to_le_bytes());
    out[0x2C..0x30].copy_from_slice(&(fat_sectors as u32).to_le_bytes());
    out[0x30..0x34].copy_from_slice(&(dir_start as u32).to_le_bytes());
    out[0x38..0x3C].copy_from_slice(&(FIXTURE_MINI_CUTOFF as u32).to_le_bytes());
    out[0x3C..0x40].copy_from_slice(&(mini_fat_start as u32).to_le_bytes());
    out[0x40..0x44].copy_from_slice(&(mini_fat_sectors as u32).to_le_bytes());
    out[0x44..0x48].copy_from_slice(&ENDOFCHAIN.to_le_bytes());
    for slot in 0..HEADER_FAT_SLOTS {
        let sector = if slot < fat_sectors {
            slot as u32
        } else {
            FREESECT
        };
        let off = 0x4C + slot * 4;
        out[off..off + 4].copy_from_slice(&sector.to_le_bytes());
    }

    let sector_off = |sector: usize| HEADER_SIZE + sector * SECTOR;
    write_u32_table(&mut out[sector_off(0)..sector_off(fat_sectors)], &fat);
    out[sector_off(dir_start)..sector_off(dir_start) + dir.len()].copy_from_slice(&dir);
    mini_fat.resize(mini_fat_sectors * ENTRIES_PER_SECTOR, FREESECT);
    write_u32_table(
        &mut out[sector_off(mini_fat_start)..sector_off(mini_data_start)],
        &mini_fat,
    );
    out[sector_off(mini_data_start)..sector_off(mini_data_start) + mini_stream.len()]
        .copy_from_slice(&mini_stream);
    out[sector_off(large_start)..sector_off(large_start) + large_data.len()]
        .copy_from_slice(&large_data);
    out
}

struct DirWrite<'a> {
    name: &'a str,
    obj_type: u8,
    left: u32,
    right: u32,
    child: u32,
    start: u32,
    size: u64,
}

fn write_dir_entry(dir: &mut [u8], index: usize, spec: DirWrite<'_>) {
    let DirWrite {
        name,
        obj_type,
        left,
        right,
        child,
        start,
        size,
    } = spec;
    let off = index * DIR_ENTRY_SIZE;
    let entry = &mut dir[off..off + DIR_ENTRY_SIZE];
    let units: Vec<u16> = name.encode_utf16().collect();
    for (i, unit) in units.iter().enumerate() {
        let o = i * 2;
        entry[o..o + 2].copy_from_slice(&unit.to_le_bytes());
    }
    let name_len = ((units.len() + 1) * 2) as u16;
    entry[0x40..0x42].copy_from_slice(&name_len.to_le_bytes());
    entry[0x42] = obj_type;
    entry[0x43] = 1;
    entry[0x44..0x48].copy_from_slice(&left.to_le_bytes());
    entry[0x48..0x4C].copy_from_slice(&right.to_le_bytes());
    entry[0x4C..0x50].copy_from_slice(&child.to_le_bytes());
    entry[0x74..0x78].copy_from_slice(&start.to_le_bytes());
    entry[0x78..0x80].copy_from_slice(&size.to_le_bytes());
}

fn write_u32_table(dst: &mut [u8], values: &[u32]) {
    for (i, value) in values.iter().enumerate() {
        let off = i * 4;
        dst[off..off + 4].copy_from_slice(&value.to_le_bytes());
    }
}

#[cfg(test)]
mod cfb_hardening_tests {
    use super::*;

    fn dir_entry(name: &str, obj_type: u8, left: u32, right: u32, child: u32) -> DirEntry {
        DirEntry {
            name: name.to_string(),
            obj_type,
            left,
            right,
            child,
            start_sector: ENDOFCHAIN,
            stream_size: 0,
        }
    }

    /// A `.msg` is untrusted input. A FAT that loops must be rejected on the
    /// second visit, not after MAX_CHAIN hops — with an unbounded `max_len`
    /// each hop appends a whole sector, so the hop ceiling alone lets a
    /// two-sector loop inflate the buffer into the hundreds of MB first.
    #[test]
    fn fat_cycle_is_rejected_before_the_buffer_can_grow() {
        let sector_size = 64usize;
        let data = vec![0u8; HEADER_SIZE + sector_size * 4];
        // 0 -> 1 -> 0 -> ...
        let fat = vec![1u32, 0u32, ENDOFCHAIN, ENDOFCHAIN];

        // Assert the *sector-precise* message: the old hop-ceiling path also
        // ends in a "FAT cycle" error, just after ~1M hops and a buffer in the
        // hundreds of MB. Only exact detection names the repeated sector, so
        // this is what distinguishes the fix from the bug.
        let err = read_fat_chain(&data, sector_size, &fat, 0).expect_err("cycle must be rejected");
        assert!(err.to_string().contains("FAT cycle at sector 0"), "{err}");

        // A self-loop is the degenerate case and must behave the same.
        let fat_self = vec![0u32, ENDOFCHAIN, ENDOFCHAIN, ENDOFCHAIN];
        let err = read_fat_chain(&data, sector_size, &fat_self, 0)
            .expect_err("self-loop must be rejected");
        assert!(err.to_string().contains("FAT cycle at sector 0"), "{err}");
    }

    #[test]
    fn acyclic_fat_chain_still_reads_every_sector() {
        let sector_size = 64usize;
        let mut data = vec![0u8; HEADER_SIZE + sector_size * 3];
        data[HEADER_SIZE] = 0xAA;
        data[HEADER_SIZE + sector_size] = 0xBB;
        let fat = vec![1u32, ENDOFCHAIN, ENDOFCHAIN];

        let out = read_fat_chain(&data, sector_size, &fat, 0).expect("acyclic chain reads");
        assert_eq!(out.len(), sector_size * 2);
        assert_eq!(out[0], 0xAA);
        assert_eq!(out[sector_size], 0xBB);
    }

    /// A directory whose `left`/`right`/`child` point back into the tree must
    /// error, not recurse until the stack dies.
    #[test]
    fn directory_cycle_is_rejected_instead_of_exhausting_the_stack() {
        // root -> child 1; entry 1 points its `right` back at itself.
        let directory = vec![
            dir_entry("Root Entry", TYPE_STORAGE, NOSTREAM, NOSTREAM, 1),
            dir_entry("__substg1.0_1000001F", TYPE_STREAM, NOSTREAM, 1, NOSTREAM),
        ];
        let cfb = CfbFile {
            data: &[],
            sector_size: 64,
            mini_sector_size: 64,
            mini_cutoff: 4096,
            fat: Vec::new(),
            mini_fat: Vec::new(),
            directory,
            mini_stream: Vec::new(),
        };

        let err = cfb
            .root_substg_streams()
            .map(|_| ())
            .expect_err("directory cycle must be rejected");
        assert!(err.to_string().contains("directory cycle"), "{err}");
    }

    #[test]
    fn an_out_of_range_directory_index_is_an_error_not_a_panic() {
        let directory = vec![dir_entry(
            "Root Entry",
            TYPE_STORAGE,
            NOSTREAM,
            NOSTREAM,
            99,
        )];
        let cfb = CfbFile {
            data: &[],
            sector_size: 64,
            mini_sector_size: 64,
            mini_cutoff: 4096,
            fat: Vec::new(),
            mini_fat: Vec::new(),
            directory,
            mini_stream: Vec::new(),
        };

        let err = cfb
            .root_substg_streams()
            .map(|_| ())
            .expect_err("out-of-range must error");
        assert!(err.to_string().contains("out of range"), "{err}");
    }

    /// Builds a top-level property stream holding the given 4-byte properties.
    fn property_stream(properties: &[(u32, u32)]) -> Vec<u8> {
        let mut stream = vec![0u8; TOP_LEVEL_PROPERTY_HEADER_LEN];
        for (tag, value) in properties {
            stream.extend_from_slice(&tag.to_le_bytes());
            stream.extend_from_slice(&0x0000_0006u32.to_le_bytes()); // readable | writable
            stream.extend_from_slice(&value.to_le_bytes());
            stream.extend_from_slice(&[0u8; 4]);
        }
        stream
    }

    fn parse_fixture(streams: &[(&str, Vec<u8>)]) -> Email {
        parse_msg_bytes(&write_ministream_cfb(streams), ParseOptions::default())
            .expect("fixture parses")
    }

    #[test]
    fn ansi_strings_decode_through_the_declared_message_code_page() {
        // "Привет" in windows-1251; not valid UTF-8.
        let email = parse_fixture(&[
            (
                "__properties_version1.0",
                property_stream(&[(PROP_MESSAGE_CODEPAGE, 1251)]),
            ),
            (
                "__substg1.0_0037001E",
                vec![0xCF, 0xF0, 0xE8, 0xE2, 0xE5, 0xF2, 0x00],
            ),
            ("__substg1.0_1000001E", b"\xD2\xE5\xEA\xF1\xF2\0".to_vec()),
        ]);
        assert_eq!(email.subject.as_deref(), Some("Привет"));
        assert_eq!(email.body.text.as_deref(), Some("Текст"));
    }

    #[test]
    fn ansi_strings_without_a_code_page_fall_back_to_windows_1252() {
        let email = parse_fixture(&[("__substg1.0_0037001E", b"Caf\xE9 \x80 ok\0".to_vec())]);
        let subject = email.subject.expect("subject");
        assert_eq!(subject, "Café € ok");
        assert!(!subject.contains(char::REPLACEMENT_CHARACTER));
    }

    #[test]
    fn ansi_strings_that_are_valid_utf8_stay_utf8_despite_the_code_page() {
        let email = parse_fixture(&[
            (
                "__properties_version1.0",
                property_stream(&[(PROP_MESSAGE_CODEPAGE, 1252)]),
            ),
            ("__substg1.0_0037001E", "Olá € 日本".as_bytes().to_vec()),
        ]);
        assert_eq!(email.subject.as_deref(), Some("Olá € 日本"));
    }

    #[test]
    fn binary_html_body_decodes_through_the_internet_code_page() {
        // `PidTagHtml` (0x1013) is PtypBinary; its bytes follow
        // `PidTagInternetCodepage`, here ISO-8859-2: "Łódź".
        let mut html = b"<p>".to_vec();
        html.extend_from_slice(&[0xA3, 0xF3, 0x64, 0xBC]);
        html.extend_from_slice(b"</p>");
        let email = parse_fixture(&[
            (
                "__properties_version1.0",
                property_stream(&[
                    (PROP_MESSAGE_CODEPAGE, 1252),
                    (PROP_INTERNET_CODEPAGE, 28592),
                ]),
            ),
            ("__substg1.0_0037001F", utf16_le("s")),
            ("__substg1.0_10130102", html),
        ]);
        assert_eq!(email.body.html.as_deref(), Some("<p>Łódź</p>"));
    }

    #[test]
    fn a_property_stream_alone_is_not_a_message() {
        let bytes = write_ministream_cfb(&[("__properties_version1.0", property_stream(&[]))]);
        assert!(parse_msg_bytes(&bytes, ParseOptions::default()).is_err());
    }

    #[test]
    fn unicode_streams_are_unaffected_by_the_code_page() {
        let email = parse_fixture(&[
            (
                "__properties_version1.0",
                property_stream(&[(PROP_MESSAGE_CODEPAGE, 1251)]),
            ),
            ("__substg1.0_0037001F", utf16_le("Olá € 日本語 😀")),
        ]);
        assert_eq!(email.subject.as_deref(), Some("Olá € 日本語 😀"));
    }
}
