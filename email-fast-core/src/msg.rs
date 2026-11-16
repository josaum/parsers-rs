//! MS-OXMSG property/recipient/attachment projection over the shared CFB reader.
use crate::{mime, Attachment, Email, EmailAddress, EmailBody, ParseOptions, ThreadInfo};
use anyhow::{anyhow, ensure, Result};
use base64::Engine;
use cfb::CompoundFile;
use chrono::{DateTime, Utc};
use std::collections::BTreeMap;
use std::io::{Cursor, Read, Seek, SeekFrom, Write};
use std::path::Path;

mod rtf;

type File<'a> = CompoundFile<Cursor<&'a [u8]>>;

pub(crate) fn parse_msg_bytes(bytes: &[u8], options: ParseOptions) -> Result<Email> {
    parse_with_sink(bytes, options, None)
}

pub(crate) fn parse_with_sink(
    bytes: &[u8],
    options: ParseOptions,
    mut sink: Option<&mut mime::AttachmentSink<'_>>,
) -> Result<Email> {
    ensure!(
        bytes.len() <= options.max_inflate,
        "MSG input exceeds max_inflate"
    );
    let mut file = CompoundFile::open(Cursor::new(bytes))
        .map_err(|error| anyhow!("read MSG CFB compound file: {error}"))?;
    let entries: Vec<_> = file.walk().collect();
    ensure!(
        entries.len() <= options.max_mime_parts,
        "MSG exceeds max_mime_parts"
    );
    let mut retained = 0u64;
    for entry in &entries {
        ensure!(
            entry.path().components().count() <= options.max_mime_depth.saturating_add(1),
            "MSG exceeds max_mime_depth"
        );
        retained = retained
            .checked_add(entry.len())
            .ok_or_else(|| anyhow!("MSG stream size overflow"))?;
        ensure!(
            retained <= options.max_inflate as u64,
            "MSG property streams exceed max_inflate"
        );
    }
    let root = Path::new("/");
    let props = properties(&mut file, root, 32, 1252)?;
    ensure!(!props.values.is_empty(), "MSG has no message properties");
    let headers = props.string(0x007D)?;
    let transport = if let Some(headers) = &headers {
        let mut raw = headers.as_bytes().to_vec();
        raw.extend_from_slice(b"\r\n\r\n");
        let options = ParseOptions {
            extract_attachment_content: false,
            ..options
        };
        Some(crate::parse_eml_bytes(&raw, options)?)
    } else {
        None
    };
    let subject = props
        .string(0x0037)?
        .or_else(|| transport.as_ref().and_then(|e| e.subject.clone()));
    let from = EmailAddress {
        address: props
            .string(0x5D01)?
            .or(props.string(0x0C1F)?)
            .or(props.string(0x0065)?)
            .unwrap_or_else(|| {
                transport
                    .as_ref()
                    .map(|e| e.from.address.clone())
                    .unwrap_or_default()
            }),
        name: props
            .string(0x0C1A)?
            .or_else(|| transport.as_ref().and_then(|e| e.from.name.clone())),
    };
    let mut to = Vec::new();
    let mut cc = Vec::new();
    let mut bcc = Vec::new();
    let mut recipient_count = 0;
    for entry in entries.iter().filter(|entry| {
        entry.is_storage()
            && entry.path().parent() == Some(root)
            && entry.name().starts_with("__recip_version1.0_")
    }) {
        recipient_count += 1;
        let recipient = properties(&mut file, entry.path(), 8, props.codepage())?;
        let address = EmailAddress {
            name: recipient.string(0x3001)?,
            address: recipient
                .string(0x39FE)?
                .or(
                    if recipient
                        .string(0x3002)?
                        .is_some_and(|kind| kind.eq_ignore_ascii_case("SMTP"))
                    {
                        recipient.string(0x3003)?
                    } else {
                        None
                    },
                )
                .unwrap_or_default(),
        };
        match recipient.integer(0x0C15).unwrap_or(1) & 0x0f {
            1 => to.push(address),
            2 => cc.push(address),
            3 => bcc.push(address),
            _ => {}
        }
    }
    if recipient_count == 0 {
        to = transport.as_ref().map(|e| e.to.clone()).unwrap_or_default();
        cc = transport.as_ref().map(|e| e.cc.clone()).unwrap_or_default();
        bcc = transport
            .as_ref()
            .map(|e| e.bcc.clone())
            .unwrap_or_default();
        // DisplayTo/DisplayCc are labels, not reliable email addresses. Preserve
        // them as names only when recipient objects/transport headers are absent.
        if to.is_empty() {
            to = display_recipients(props.string(0x0E04)?);
        }
        if cc.is_empty() {
            cc = display_recipients(props.string(0x0E03)?);
        }
        if bcc.is_empty() {
            bcc = display_recipients(props.string(0x0E02)?);
        }
    }
    let mut body = EmailBody {
        text: props.string(0x1000)?,
        html: props.string(0x1013)?,
    };
    if body.html.is_none() {
        if let Some(raw) = props.binary(0x1013) {
            body.html = Some(props.decode_with_codepage(
                raw,
                props.integer(0x3FDE).unwrap_or_else(|| props.codepage()),
            )?);
        }
    }
    if body.text.is_none() {
        if let Some(compressed) = props.binary(0x1009) {
            body.text = Some(rtf::text(
                compressed,
                options.max_inflate,
                options.max_mime_depth,
            )?);
        }
    }
    if options.html_to_text && body.text.is_none() {
        body.text = body.html.as_deref().map(crate::html_to_text);
    }
    let mut total = body
        .text
        .as_ref()
        .map_or(0, String::len)
        .saturating_add(body.html.as_ref().map_or(0, String::len));
    ensure!(
        total <= options.max_inflate,
        "MSG decoded body exceeds max_inflate"
    );
    let mut attachment_total = 0usize;
    let mut attachments = Vec::new();
    for (index, entry) in entries
        .iter()
        .filter(|entry| {
            entry.is_storage()
                && entry.path().parent() == Some(root)
                && entry.name().starts_with("__attach_version1.0_")
        })
        .enumerate()
    {
        let p = properties(&mut file, entry.path(), 8, props.codepage())?;
        let embedded = entry.path().join("__substg1.0_3701000D");
        let data_path = entry.path().join("__substg1.0_37010102");
        let is_embedded = file.is_storage(&embedded);
        let limit = options
            .max_attachment_size
            .min(
                options
                    .max_total_attachment_bytes
                    .saturating_sub(attachment_total),
            )
            .min(options.max_inflate.saturating_sub(total));
        let content_id = p.string(0x3712)?;
        let mut attachment = Attachment {
            filename: p
                .string(0x3707)?
                .or(p.string(0x3704)?)
                .or(p.string(0x3001)?)
                .unwrap_or_else(|| {
                    format!(
                        "attachment_{index}.{}",
                        if is_embedded { "msg" } else { "bin" }
                    )
                }),
            content_type: p.string(0x370E)?.unwrap_or_else(|| {
                if is_embedded {
                    "application/vnd.ms-outlook"
                } else {
                    "application/octet-stream"
                }
                .into()
            }),
            size: 0,
            is_inline: content_id.is_some(),
            content_id,
            content: None,
        };
        let embedded_bytes;
        let mut reader: Box<dyn Read> = if is_embedded {
            embedded_bytes = copy_embedded(&mut file, &embedded, limit)?;
            Box::new(Cursor::new(embedded_bytes))
        } else if file.is_stream(&data_path) {
            ensure!(
                file.entry(&data_path)?.len() <= limit as u64,
                "MSG attachment exceeds configured budget"
            );
            Box::new(file.open_stream(&data_path)?)
        } else {
            continue;
        };
        let size = if let Some(sink) = &mut sink {
            sink(&attachment, &mut reader, limit)?
        } else if options.extract_attachment_content {
            let mut bytes = Vec::new();
            let n = mime::copy_limited(&mut reader, &mut bytes, limit)?;
            attachment.content = Some(base64::engine::general_purpose::STANDARD.encode(bytes));
            n
        } else {
            mime::copy_limited(&mut reader, &mut std::io::sink(), limit)?
        };
        attachment.size = size;
        total += size;
        attachment_total += size;
        attachments.push(attachment);
    }
    let message_id = props
        .string(0x1035)?
        .or_else(|| transport.as_ref().and_then(|e| e.message_id.clone()));
    let in_reply_to = props.string(0x1042)?.or_else(|| {
        transport
            .as_ref()
            .and_then(|e| e.thread_info.in_reply_to.clone())
    });
    let references = props
        .string(0x1039)?
        .map(|s| s.split_whitespace().map(str::to_owned).collect())
        .unwrap_or_else(|| {
            transport
                .as_ref()
                .map(|e| e.thread_info.references.clone())
                .unwrap_or_default()
        });
    let reply_to = props
        .binary(0x004F)
        .map(|bytes| reply_recipient(bytes, &props))
        .transpose()?
        .flatten()
        .or_else(|| transport.as_ref().and_then(|e| e.reply_to.clone()))
        .or_else(|| {
            props
                .string(0x0050)
                .ok()
                .flatten()
                .and_then(|raw| display_recipients(Some(raw)).into_iter().next())
        });
    let date = props
        .timestamp(0x0039)?
        .or(props.timestamp(0x0E06)?)
        .or_else(|| transport.as_ref().and_then(|e| e.date));
    Ok(Email {
        message_id: message_id.clone(),
        from,
        reply_to,
        to,
        cc,
        bcc,
        subject,
        date,
        body,
        attachments,
        thread_info: ThreadInfo {
            message_id,
            in_reply_to,
            references,
        },
        priority: props.integer(0x0017).and_then(|n| match n {
            0 => Some(5),
            1 => Some(3),
            2 => Some(1),
            _ => None,
        }),
        headers: transport.and_then(|e| e.headers),
    })
}

fn display_recipients(raw: Option<String>) -> Vec<EmailAddress> {
    raw.into_iter()
        .flat_map(|raw| {
            raw.split(';')
                .filter(|s| !s.trim().is_empty())
                .map(|s| {
                    mailparse::addrparse(s)
                        .ok()
                        .and_then(|list| crate::flatten_addresses(&list).into_iter().next())
                        .unwrap_or(EmailAddress {
                            name: Some(s.trim().into()),
                            address: String::new(),
                        })
                })
                .collect::<Vec<_>>()
        })
        .collect()
}

struct Properties {
    values: BTreeMap<u32, Vec<u8>>,
    inherited_codepage: u32,
}
impl Properties {
    fn binary(&self, id: u32) -> Option<&[u8]> {
        self.values.get(&((id << 16) | 0x0102)).map(Vec::as_slice)
    }
    fn integer(&self, id: u32) -> Option<u32> {
        self.values
            .get(&((id << 16) | 0x0003))
            .and_then(|v| v.get(..4))
            .map(|v| u32::from_le_bytes(v.try_into().unwrap()))
    }
    fn string(&self, id: u32) -> Result<Option<String>> {
        if let Some(bytes) = self.values.get(&((id << 16) | 0x001F)) {
            ensure!(bytes.len() % 2 == 0, "odd length MSG Unicode property");
            let units: Vec<_> = bytes
                .as_chunks::<2>()
                .0
                .iter()
                .map(|p| u16::from_le_bytes([p[0], p[1]]))
                .collect();
            return Ok(Some(
                String::from_utf16(&units)?.trim_end_matches('\0').into(),
            ));
        }
        self.values
            .get(&((id << 16) | 0x001E))
            .map(|bytes| {
                self.decode_with_codepage(
                    bytes,
                    if matches!(id, 0x1000 | 0x1013) {
                        self.integer(0x3FDE).unwrap_or_else(|| self.codepage())
                    } else {
                        self.codepage()
                    },
                )
            })
            .transpose()
    }
    fn codepage(&self) -> u32 {
        self.integer(0x3FFD).unwrap_or(self.inherited_codepage)
    }
    fn decode_ansi(&self, bytes: &[u8]) -> Result<String> {
        self.decode_with_codepage(bytes, self.codepage())
    }
    fn decode_with_codepage(&self, bytes: &[u8], cp: u32) -> Result<String> {
        let encoding = u16::try_from(cp)
            .ok()
            .and_then(codepage::to_encoding)
            .ok_or_else(|| anyhow!("unsupported MSG codepage {cp}"))?;
        let (text, _, errors) = encoding.decode(bytes);
        ensure!(!errors, "invalid MSG text for codepage {cp}");
        Ok(text.trim_end_matches('\0').into())
    }
    fn timestamp(&self, id: u32) -> Result<Option<DateTime<Utc>>> {
        let Some(value) = self.values.get(&((id << 16) | 0x0040)) else {
            return Ok(None);
        };
        ensure!(value.len() >= 8, "truncated MSG FILETIME");
        let ticks = u64::from_le_bytes(value[..8].try_into()?);
        let seconds = i64::try_from(ticks / 10_000_000)? - 11_644_473_600;
        Ok(Some(
            DateTime::from_timestamp(seconds, ((ticks % 10_000_000) * 100) as u32)
                .ok_or_else(|| anyhow!("MSG FILETIME out of range"))?,
        ))
    }
}

fn properties(
    file: &mut File<'_>,
    path: &Path,
    header: usize,
    inherited_codepage: u32,
) -> Result<Properties> {
    let entries: Vec<_> = file.read_storage(path)?.collect();
    let mut values = BTreeMap::new();
    for entry in entries {
        if !entry.is_stream() {
            continue;
        }
        let name = entry.name();
        // Attachment payloads are read once, directly into the caller's sink.
        if name == "__substg1.0_37010102" {
            continue;
        }
        if let Some(tag) = name
            .strip_prefix("__substg1.0_")
            .filter(|s| s.len() == 8)
            .and_then(|s| u32::from_str_radix(s, 16).ok())
        {
            let mut bytes = Vec::new();
            file.open_stream(entry.path())?.read_to_end(&mut bytes)?;
            values.insert(tag, bytes);
        } else if name == "__properties_version1.0" {
            let mut bytes = Vec::new();
            file.open_stream(entry.path())?.read_to_end(&mut bytes)?;
            ensure!(
                bytes.len() >= header && (bytes.len() - header).is_multiple_of(16),
                "invalid MSG property stream"
            );
            for property in bytes[header..].as_chunks::<16>().0 {
                let tag = u32::from_le_bytes(property[..4].try_into()?);
                if matches!(tag & 0xFFFF, 0x0002 | 0x0003 | 0x000B | 0x0040) {
                    values.insert(tag, property[8..16].to_vec());
                }
            }
        }
    }
    Ok(Properties {
        values,
        inherited_codepage,
    })
}

struct BoundedCursor {
    inner: Cursor<Vec<u8>>,
    limit: usize,
}
impl Read for BoundedCursor {
    fn read(&mut self, b: &mut [u8]) -> std::io::Result<usize> {
        self.inner.read(b)
    }
}
impl Seek for BoundedCursor {
    fn seek(&mut self, p: SeekFrom) -> std::io::Result<u64> {
        self.inner.seek(p)
    }
}
impl Write for BoundedCursor {
    fn write(&mut self, b: &[u8]) -> std::io::Result<usize> {
        if self.inner.position().saturating_add(b.len() as u64) > self.limit as u64 {
            return Err(std::io::Error::other(
                "embedded MSG exceeds attachment budget",
            ));
        }
        self.inner.write(b)
    }
    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}

fn copy_embedded(source: &mut File<'_>, root: &Path, limit: usize) -> Result<Vec<u8>> {
    let entries: Vec<_> = source.walk_storage(root)?.collect();
    let mut target = CompoundFile::create(BoundedCursor {
        inner: Cursor::new(Vec::new()),
        limit,
    })?;
    for entry in entries {
        let relative = entry.path().strip_prefix(root)?;
        if relative.as_os_str().is_empty() {
            continue;
        }
        let path = Path::new("/").join(relative);
        if entry.is_storage() {
            target.create_storage_all(&path)?;
        } else if entry.is_stream() {
            if let Some(parent) = path.parent() {
                target.create_storage_all(parent)?;
            }
            let mut input = source.open_stream(entry.path())?;
            let mut output = target.create_stream(&path)?;
            if relative == Path::new("__properties_version1.0") {
                // Embedded message headers are 24 bytes; top-level headers are 32.
                let mut header = [0u8; 24];
                input.read_exact(&mut header)?;
                output.write_all(&header)?;
                output.write_all(&[0u8; 8])?;
            }
            std::io::copy(&mut input, &mut output)?;
        }
    }
    // Named properties of an embedded object use the outer message's mapping.
    if source.is_storage("/__nameid_version1.0") && !target.is_storage("/__nameid_version1.0") {
        target.create_storage("/__nameid_version1.0")?;
        let entries: Vec<_> = source.read_storage("/__nameid_version1.0")?.collect();
        for entry in entries.into_iter().filter(|e| e.is_stream()) {
            std::io::copy(
                &mut source.open_stream(entry.path())?,
                &mut target.create_stream(entry.path())?,
            )?;
        }
    }
    target.flush()?;
    Ok(target.into_inner().inner.into_inner())
}

pub(crate) fn write_fixture_msg(subject: &str, sender: &str, to: &str, body: &str) -> Vec<u8> {
    let mut file = CompoundFile::create(Cursor::new(Vec::new())).unwrap();
    for (name, value) in [
        ("0037001F", subject),
        ("0C1F001F", sender),
        ("0E04001F", to),
        ("1000001F", body),
    ] {
        let data: Vec<u8> = value
            .encode_utf16()
            .chain(Some(0))
            .flat_map(u16::to_le_bytes)
            .collect();
        file.create_stream(format!("/__substg1.0_{name}"))
            .unwrap()
            .write_all(&data)
            .unwrap();
    }
    let paths: Vec<_> = file.walk().map(|entry| entry.path().to_owned()).collect();
    for path in paths {
        file.set_created_time(&path, std::time::UNIX_EPOCH)
            .expect("fixture creation time");
        file.set_modified_time(&path, std::time::UNIX_EPOCH)
            .expect("fixture modification time");
    }
    file.into_inner().into_inner()
}

// MS-OXCDATA FlatEntryList and SMTP One-Off EntryIDs. Directory EntryIDs require
// an external address book; transport Reply-To remains the fallback for those.
fn reply_recipient(bytes: &[u8], props: &Properties) -> Result<Option<EmailAddress>> {
    ensure!(bytes.len() >= 8, "truncated MSG reply recipient list");
    let count = u32::from_le_bytes(bytes[..4].try_into()?) as usize;
    let size = u32::from_le_bytes(bytes[4..8].try_into()?) as usize;
    ensure!(
        size == bytes.len() - 8 && count <= size / 4,
        "invalid MSG reply recipient list size"
    );
    let mut tail = &bytes[8..];
    let mut result = None;
    const PROVIDER: &[u8; 16] = b"\xa4\x1f\x2b\x81\xa3\xbe\x19\x10\x9d\x6e\x00\xdd\x01\x0f\x54\x02";
    for _ in 0..count {
        ensure!(tail.len() >= 4, "truncated MSG reply entry size");
        let length = u32::from_le_bytes(tail[..4].try_into()?) as usize;
        tail = &tail[4..];
        let entry = tail
            .get(..length)
            .ok_or_else(|| anyhow!("truncated MSG reply entry"))?;
        let padded = length
            .checked_add(3)
            .ok_or_else(|| anyhow!("reply entry overflow"))?
            & !3;
        tail = tail
            .get(padded..)
            .or_else(|| tail.get(length..).filter(|tail| tail.is_empty()))
            .ok_or_else(|| anyhow!("truncated reply entry padding"))?;
        if result.is_some() || entry.get(4..20) != Some(PROVIDER.as_slice()) {
            continue;
        }
        ensure!(entry.len() >= 24, "truncated MSG One-Off EntryID");
        let unicode = u16::from_le_bytes(entry[22..24].try_into()?) & 0x8000 != 0;
        let mut strings = &entry[24..];
        let mut fields = Vec::new();
        for _ in 0..3 {
            if unicode {
                let end = strings
                    .as_chunks::<2>()
                    .0
                    .iter()
                    .position(|p| *p == [0, 0])
                    .ok_or_else(|| anyhow!("unterminated MSG One-Off string"))?
                    * 2;
                let units: Vec<_> = strings[..end]
                    .as_chunks::<2>()
                    .0
                    .iter()
                    .map(|p| u16::from_le_bytes(*p))
                    .collect();
                fields.push(String::from_utf16(&units)?);
                strings = &strings[end + 2..];
            } else {
                let end = strings
                    .iter()
                    .position(|b| *b == 0)
                    .ok_or_else(|| anyhow!("unterminated MSG One-Off string"))?;
                fields.push(props.decode_ansi(&strings[..end])?);
                strings = &strings[end + 1..];
            }
        }
        if fields[1].eq_ignore_ascii_case("SMTP") {
            result = Some(EmailAddress {
                name: Some(fields[0].clone()).filter(|s| !s.is_empty()),
                address: fields[2].clone(),
            });
        }
    }
    ensure!(tail.is_empty(), "trailing MSG reply entries");
    Ok(result)
}

/// Build a CFB fixture containing the supplied streams, creating parent storages.
pub(crate) fn write_fixture_streams(streams: &[(&str, Vec<u8>)]) -> Vec<u8> {
    let mut file = CompoundFile::create(Cursor::new(Vec::new())).expect("fixture CFB");
    for (name, bytes) in streams {
        let path = Path::new("/").join(name);
        if let Some(parent) = path.parent() {
            file.create_storage_all(parent).expect("fixture storage");
        }
        file.create_stream(path)
            .expect("fixture stream")
            .write_all(bytes)
            .expect("fixture data");
    }
    let paths: Vec<_> = file.walk().map(|entry| entry.path().to_owned()).collect();
    for path in paths {
        file.set_created_time(&path, std::time::UNIX_EPOCH)
            .expect("fixture creation time");
        file.set_modified_time(&path, std::time::UNIX_EPOCH)
            .expect("fixture modification time");
    }
    file.into_inner().into_inner()
}
