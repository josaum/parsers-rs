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

const OLE_SIGNATURE: [u8; 8] = [0xd0, 0xcf, 0x11, 0xe0, 0xa1, 0xb1, 0x1a, 0xe1];
const FREESECT: u32 = 0xFFFF_FFFF;
const ENDOFCHAIN: u32 = 0xFFFF_FFFE;
const FATSECT: u32 = 0xFFFF_FFFD;
const DIFSECT: u32 = 0xFFFF_FFFC;
const NOSTREAM: u32 = 0xFFFF_FFFF;
const DIR_ENTRY_SIZE: usize = 128;
const HEADER_SIZE: usize = 512;
const MAX_CHAIN: usize = 1 << 20;

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

pub(crate) fn parse_msg_bytes(bytes: &[u8], options: ParseOptions) -> Result<Email> {
    if bytes.len() < HEADER_SIZE || bytes[..8] != OLE_SIGNATURE {
        return Err(anyhow!("Not a valid MSG file"));
    }
    let file = CfbFile::parse(bytes)?;
    let (streams, attach_storages) = file.root_streams_and_attachments()?;
    if streams.is_empty() {
        return Err(anyhow!(
            "MSG file has no root __substg1.0_ property streams (not a message object)"
        ));
    }

    let subject = first_string(&streams, TAG_SUBJECT);
    let from_addr = first_string(&streams, TAG_SENDER_EMAIL)
        .or_else(|| first_string(&streams, TAG_SENT_REPRESENTING_EMAIL));
    let from_name = first_string(&streams, TAG_SENDER_NAME);
    let to_raw = first_string(&streams, TAG_DISPLAY_TO);
    let cc_raw = first_string(&streams, TAG_DISPLAY_CC);
    let mut body_text = first_string(&streams, TAG_BODY);
    let body_html = first_string(&streams, TAG_BODY_HTML).or_else(|| {
        first_binary(&streams, TAG_BODY_HTML)
            .map(|b| {
                String::from_utf8_lossy(b)
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

impl<'a> CfbFile<'a> {
    fn parse(data: &'a [u8]) -> Result<Self> {
        if u16_le(data, 0x1C)? != 0xFFFE {
            return Err(anyhow!("MSG CFB: unexpected byte order"));
        }
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
        let num_difat = u32_le(data, 0x48)? as usize;

        let fat = load_fat(data, sector_size, first_difat, num_difat)?;
        let directory = load_directory(data, sector_size, &fat, first_dir)?;
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
                    root.stream_size as usize,
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
        self.root_streams_and_attachments().map(|(s, _)| s)
    }

    fn root_streams_and_attachments(&self) -> Result<(Vec<PropStream>, Vec<Vec<PropStream>>)> {
        let Some(root) = self.directory.first() else {
            return Err(anyhow!("MSG CFB: empty directory"));
        };
        let mut out = Vec::new();
        let mut attachments = Vec::new();
        // A malformed CFB can point `left`/`right`/`child` back into the tree.
        // Without this, `walk_dir` recurses until the stack dies on a file we
        // do not control.
        let mut visited = vec![false; self.directory.len()];
        if root.child != NOSTREAM {
            self.walk_dir(root.child, true, &mut out, &mut attachments, &mut visited)?;
        }
        Ok((out, attachments))
    }

    fn walk_dir(
        &self,
        idx: u32,
        take_streams: bool,
        out: &mut Vec<PropStream>,
        attachments: &mut Vec<Vec<PropStream>>,
        visited: &mut [bool],
    ) -> Result<()> {
        if idx == NOSTREAM {
            return Ok(());
        }
        let slot = visited
            .get_mut(idx as usize)
            .ok_or_else(|| anyhow!("MSG CFB: directory index {idx} out of range"))?;
        if *slot {
            return Err(anyhow!("MSG CFB: directory cycle at index {idx}"));
        }
        *slot = true;
        let entry = self
            .directory
            .get(idx as usize)
            .ok_or_else(|| anyhow!("MSG CFB: directory index {idx} out of range"))?;
        self.walk_dir(entry.left, take_streams, out, attachments, visited)?;
        if entry.obj_type == TYPE_STORAGE && entry.child != NOSTREAM {
            if take_streams && entry.name.starts_with("__attach_version1.0_") {
                let mut att_streams = Vec::new();
                let mut dummy_nested = Vec::new();
                self.walk_dir(
                    entry.child,
                    true,
                    &mut att_streams,
                    &mut dummy_nested,
                    visited,
                )?;
                attachments.push(att_streams);
            } else {
                // Recipients / other storages have their own substg streams; ignore
                // them so attachment/recipient bodies cannot masquerade as the message body.
                self.walk_dir(entry.child, false, out, attachments, visited)?;
            }
        }
        if take_streams && entry.obj_type == TYPE_STREAM {
            if let Some((tag, kind)) = parse_substg_name(&entry.name) {
                let bytes = self.read_stream(entry)?;
                out.push(PropStream { tag, kind, bytes });
            }
        }
        self.walk_dir(entry.right, take_streams, out, attachments, visited)?;
        Ok(())
    }

    fn read_stream(&self, entry: &DirEntry) -> Result<Vec<u8>> {
        let len = entry.stream_size as usize;
        if entry.stream_size < self.mini_cutoff && !self.mini_stream.is_empty() {
            read_mini_chain(
                &self.mini_stream,
                &self.mini_fat,
                self.mini_sector_size,
                entry.start_sector,
                len,
            )
        } else {
            read_fat_chain_len(
                self.data,
                self.sector_size,
                &self.fat,
                entry.start_sector,
                len,
            )
        }
    }
}

fn load_fat(
    data: &[u8],
    sector_size: usize,
    first_difat: u32,
    num_difat: usize,
) -> Result<Vec<u32>> {
    let mut fat_sectors =
        Vec::with_capacity(109 + num_difat * ((sector_size / 4).saturating_sub(1)));
    for i in 0..109 {
        let sect = u32_le(data, 0x4C + i * 4)?;
        if sect != FREESECT {
            fat_sectors.push(sect);
        }
    }
    let mut difat_sect = first_difat;
    let mut hops = 0usize;
    while difat_sect != ENDOFCHAIN && difat_sect != FREESECT {
        hops += 1;
        if hops > MAX_CHAIN {
            return Err(anyhow!("MSG CFB: DIFAT cycle"));
        }
        let raw = sector_slice(data, sector_size, difat_sect)?;
        let entries = raw.len() / 4;
        if entries == 0 {
            break;
        }
        for i in 0..entries - 1 {
            let sect = u32_le(raw, i * 4)?;
            if sect != FREESECT {
                fat_sectors.push(sect);
            }
        }
        difat_sect = u32_le(raw, (entries - 1) * 4)?;
    }
    let mut fat = Vec::new();
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
        out.push(DirEntry {
            name,
            obj_type,
            left: u32_le(chunk, 0x44)?,
            right: u32_le(chunk, 0x48)?,
            child: u32_le(chunk, 0x4C)?,
            start_sector: u32_le(chunk, 0x74)?,
            stream_size: u64_le(chunk, 0x78)?,
        });
    }
    if out.is_empty() || out[0].obj_type != TYPE_ROOT {
        return Err(anyhow!("MSG CFB: missing root directory entry"));
    }
    Ok(out)
}

fn read_fat_chain(data: &[u8], sector_size: usize, fat: &[u32], start: u32) -> Result<Vec<u8>> {
    read_fat_chain_len(data, sector_size, fat, start, usize::MAX)
}

fn read_fat_chain_len(
    data: &[u8],
    sector_size: usize,
    fat: &[u32],
    start: u32,
    max_len: usize,
) -> Result<Vec<u8>> {
    if start == ENDOFCHAIN || start == FREESECT {
        return Ok(Vec::new());
    }
    let mut out = Vec::new();
    let mut sect = start;
    let mut hops = 0usize;
    // Exact cycle detection, not just a hop ceiling: with `max_len` unbounded a
    // two-sector loop would otherwise append `sector_size` bytes per hop until
    // MAX_CHAIN, inflating the buffer into the hundreds of MB before failing.
    let mut seen = vec![false; fat.len()];
    while sect != ENDOFCHAIN && sect != FREESECT {
        hops += 1;
        if hops > MAX_CHAIN {
            return Err(anyhow!("MSG CFB: FAT chain too long"));
        }
        if let Some(slot) = seen.get_mut(sect as usize) {
            if *slot {
                return Err(anyhow!("MSG CFB: FAT cycle at sector {sect}"));
            }
            *slot = true;
        }
        if sect == FATSECT || sect == DIFSECT {
            return Err(anyhow!("MSG CFB: FAT points at metadata sector"));
        }
        let raw = sector_slice(data, sector_size, sect)?;
        let remaining = max_len.saturating_sub(out.len());
        if remaining == 0 {
            break;
        }
        let take = remaining.min(raw.len());
        out.extend_from_slice(&raw[..take]);
        sect = fat_next(fat, sect)?;
    }
    if max_len != usize::MAX && out.len() > max_len {
        out.truncate(max_len);
    }
    Ok(out)
}

fn read_mini_chain(
    mini_stream: &[u8],
    mini_fat: &[u32],
    mini_sector_size: usize,
    start: u32,
    max_len: usize,
) -> Result<Vec<u8>> {
    if start == ENDOFCHAIN || start == FREESECT || max_len == 0 {
        return Ok(Vec::new());
    }
    let mut out = Vec::with_capacity(max_len);
    let mut sect = start;
    let mut hops = 0usize;
    while sect != ENDOFCHAIN && sect != FREESECT {
        hops += 1;
        if hops > MAX_CHAIN {
            return Err(anyhow!("MSG CFB: mini-FAT cycle"));
        }
        let off = sect as usize * mini_sector_size;
        let end = off
            .checked_add(mini_sector_size)
            .ok_or_else(|| anyhow!("MSG CFB: mini sector overflow"))?;
        if end > mini_stream.len() {
            return Err(anyhow!("MSG CFB: mini sector {sect} past mini stream"));
        }
        let remaining = max_len.saturating_sub(out.len());
        if remaining == 0 {
            break;
        }
        let take = remaining.min(mini_sector_size);
        out.extend_from_slice(&mini_stream[off..off + take]);
        sect = fat_next(mini_fat, sect)?;
    }
    if out.len() > max_len {
        out.truncate(max_len);
    }
    Ok(out)
}

fn fat_next(fat: &[u32], sect: u32) -> Result<u32> {
    fat.get(sect as usize)
        .copied()
        .ok_or_else(|| anyhow!("MSG CFB: sector {sect} missing from FAT"))
}

fn sector_slice(data: &[u8], sector_size: usize, sect: u32) -> Result<&[u8]> {
    let off = HEADER_SIZE
        .checked_add(sect as usize * sector_size)
        .ok_or_else(|| anyhow!("MSG CFB: sector offset overflow"))?;
    let end = off
        .checked_add(sector_size)
        .ok_or_else(|| anyhow!("MSG CFB: sector end overflow"))?;
    data.get(off..end)
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
    buf.get(off..off + 2)
        .and_then(|s| s.try_into().ok())
        .map(u16::from_le_bytes)
        .ok_or_else(|| anyhow!("MSG CFB: truncated u16 at {off}"))
}

fn u32_le(buf: &[u8], off: usize) -> Result<u32> {
    buf.get(off..off + 4)
        .and_then(|s| s.try_into().ok())
        .map(u32::from_le_bytes)
        .ok_or_else(|| anyhow!("MSG CFB: truncated u32 at {off}"))
}

fn u64_le(buf: &[u8], off: usize) -> Result<u64> {
    buf.get(off..off + 8)
        .and_then(|s| s.try_into().ok())
        .map(u64::from_le_bytes)
        .ok_or_else(|| anyhow!("MSG CFB: truncated u64 at {off}"))
}

fn parse_substg_name(name: &str) -> Option<(String, PropKind)> {
    let rest = name.strip_prefix("__substg1.0_")?;
    if rest.len() < 8 {
        return None;
    }
    let tag = rest[..4].to_ascii_uppercase();
    let kind = match rest[4..8].to_ascii_uppercase().as_str() {
        "001F" => PropKind::Unicode,
        "001E" => PropKind::Ansi,
        "0102" => PropKind::Binary,
        _ => return None,
    };
    Some((tag, kind))
}

fn first_binary<'a>(streams: &'a [PropStream], tag: &str) -> Option<&'a [u8]> {
    streams
        .iter()
        .find(|s| s.tag.eq_ignore_ascii_case(tag) && matches!(s.kind, PropKind::Binary))
        .map(|s| s.bytes.as_slice())
}

fn first_string(streams: &[PropStream], tag: &str) -> Option<String> {
    let unicode = streams
        .iter()
        .find(|s| s.tag.eq_ignore_ascii_case(tag) && matches!(s.kind, PropKind::Unicode));
    if let Some(stream) = unicode {
        return decode_prop(&stream.bytes, &stream.kind);
    }
    streams
        .iter()
        .find(|s| s.tag.eq_ignore_ascii_case(tag) && matches!(s.kind, PropKind::Ansi))
        .and_then(|stream| decode_prop(&stream.bytes, &stream.kind))
}

fn decode_prop(bytes: &[u8], kind: &PropKind) -> Option<String> {
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
        PropKind::Ansi => String::from_utf8_lossy(bytes).into_owned(),
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

fn utf16_le(value: &str) -> Vec<u8> {
    let mut out = Vec::new();
    for unit in value.encode_utf16() {
        out.extend_from_slice(&unit.to_le_bytes());
    }
    out.extend_from_slice(&0u16.to_le_bytes());
    out
}

fn write_ministream_cfb(streams: &[(&str, Vec<u8>)]) -> Vec<u8> {
    const SECTOR: usize = 512;
    const MINI: usize = 64;
    let n = streams.len();
    let dir_entries = 1 + n;
    let dir_bytes = dir_entries * DIR_ENTRY_SIZE;
    let dir_sectors = dir_bytes.div_ceil(SECTOR);
    let mini_sectors: Vec<Vec<u8>> = streams
        .iter()
        .map(|(_, data)| {
            let mut padded = data.clone();
            let rem = padded.len() % MINI;
            if rem != 0 {
                padded.resize(padded.len() + (MINI - rem), 0);
            }
            if padded.is_empty() {
                padded.resize(MINI, 0);
            }
            padded
        })
        .collect();
    let mini_stream: Vec<u8> = mini_sectors.iter().flatten().copied().collect();
    let mini_stream_sectors = mini_stream.len().div_ceil(SECTOR).max(1);
    let mini_fat_sectors = 1;

    // Layout after the 512-byte header:
    // 0: FAT
    // 1..dir_sectors: directory
    // next: mini FAT
    // next: mini stream data
    let fat_sect = 0u32;
    let dir_start = 1u32;
    let mini_fat_start = dir_start + dir_sectors as u32;
    let mini_data_start = mini_fat_start + mini_fat_sectors as u32;
    let total_sectors = mini_data_start as usize + mini_stream_sectors;

    let mut fat = vec![FREESECT; SECTOR / 4];
    fat[0] = FATSECT;
    for i in 0..dir_sectors {
        let s = dir_start as usize + i;
        fat[s] = if i + 1 == dir_sectors {
            ENDOFCHAIN
        } else {
            (s + 1) as u32
        };
    }
    fat[mini_fat_start as usize] = ENDOFCHAIN;
    for i in 0..mini_stream_sectors {
        let s = mini_data_start as usize + i;
        fat[s] = if i + 1 == mini_stream_sectors {
            ENDOFCHAIN
        } else {
            (s + 1) as u32
        };
    }

    let mut mini_fat = vec![FREESECT; SECTOR / 4];
    let mut mini_index = 0u32;
    for padded in &mini_sectors {
        let count = (padded.len() / MINI) as u32;
        for j in 0..count {
            let idx = mini_index + j;
            mini_fat[idx as usize] = if j + 1 == count { ENDOFCHAIN } else { idx + 1 };
        }
        mini_index += count;
    }

    let mut dir = vec![0u8; dir_sectors * SECTOR];
    write_dir_entry(
        &mut dir,
        0,
        DirWrite {
            name: "Root Entry",
            obj_type: TYPE_ROOT,
            left: NOSTREAM,
            right: NOSTREAM,
            child: 1,
            start: mini_data_start,
            size: mini_stream.len() as u64,
        },
    );
    let mut mini_start = 0u32;
    for (i, (name, data)) in streams.iter().enumerate() {
        let idx = i + 1;
        let right = if idx + 1 < dir_entries {
            idx as u32 + 1
        } else {
            NOSTREAM
        };
        write_dir_entry(
            &mut dir,
            idx,
            DirWrite {
                name,
                obj_type: TYPE_STREAM,
                left: NOSTREAM,
                right,
                child: NOSTREAM,
                start: mini_start,
                size: data.len() as u64,
            },
        );
        mini_start += (mini_sectors[i].len() / MINI) as u32;
    }

    let mut out = vec![0u8; HEADER_SIZE + total_sectors * SECTOR];
    out[..8].copy_from_slice(&OLE_SIGNATURE);
    out[0x18..0x1A].copy_from_slice(&0x003Eu16.to_le_bytes());
    out[0x1A..0x1C].copy_from_slice(&0x0003u16.to_le_bytes());
    out[0x1C..0x1E].copy_from_slice(&0xFFFEu16.to_le_bytes());
    out[0x1E..0x20].copy_from_slice(&9u16.to_le_bytes());
    out[0x20..0x22].copy_from_slice(&6u16.to_le_bytes());
    out[0x2C..0x30].copy_from_slice(&1u32.to_le_bytes());
    out[0x30..0x34].copy_from_slice(&dir_start.to_le_bytes());
    out[0x38..0x3C].copy_from_slice(&4096u32.to_le_bytes());
    out[0x3C..0x40].copy_from_slice(&mini_fat_start.to_le_bytes());
    out[0x40..0x44].copy_from_slice(&1u32.to_le_bytes());
    out[0x44..0x48].copy_from_slice(&ENDOFCHAIN.to_le_bytes());
    out[0x4C..0x50].copy_from_slice(&fat_sect.to_le_bytes());
    for i in 1..109 {
        let off = 0x4C + i * 4;
        out[off..off + 4].copy_from_slice(&FREESECT.to_le_bytes());
    }

    write_u32_table(&mut out[HEADER_SIZE..HEADER_SIZE + SECTOR], &fat);
    let dir_off = HEADER_SIZE + dir_start as usize * SECTOR;
    out[dir_off..dir_off + dir.len()].copy_from_slice(&dir);
    let mini_fat_off = HEADER_SIZE + mini_fat_start as usize * SECTOR;
    write_u32_table(&mut out[mini_fat_off..mini_fat_off + SECTOR], &mini_fat);
    let mini_data_off = HEADER_SIZE + mini_data_start as usize * SECTOR;
    out[mini_data_off..mini_data_off + mini_stream.len()].copy_from_slice(&mini_stream);
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
}
