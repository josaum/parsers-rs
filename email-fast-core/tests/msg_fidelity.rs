use cfb::CompoundFile;
use email_fast_core::*;
use std::io::{Cursor, Write};

type Cfb = CompoundFile<Cursor<Vec<u8>>>;
fn stream(file: &mut Cfb, path: &str, data: &[u8]) {
    file.create_stream(path).unwrap().write_all(data).unwrap();
}
fn string(file: &mut Cfb, path: &str, text: &str) {
    let bytes: Vec<_> = text
        .encode_utf16()
        .chain(Some(0))
        .flat_map(u16::to_le_bytes)
        .collect();
    stream(file, path, &bytes);
}
fn fixed(file: &mut Cfb, path: &str, header: usize, values: &[(u32, u64)]) {
    let mut data = vec![0; header];
    for (tag, value) in values {
        data.extend(tag.to_le_bytes());
        data.extend([0u8; 4]);
        data.extend(value.to_le_bytes());
    }
    stream(file, path, &data);
}
fn rich_msg() -> Vec<u8> {
    let mut file = CompoundFile::create(Cursor::new(Vec::new())).unwrap();
    fixed(
        &mut file,
        "/__properties_version1.0",
        32,
        &[
            (0x00390040, 133498026000000000),
            (0x00170003, 2),
            (0x3FFD0003, 1252),
        ],
    );
    string(&mut file, "/__substg1.0_0037001F", "Rich MSG");
    string(&mut file, "/__substg1.0_5D01001F", "sender@example.com");
    string(&mut file, "/__substg1.0_1035001F", "<id@example.com>");
    string(&mut file, "/__substg1.0_1042001F", "<parent@example.com>");
    string(
        &mut file,
        "/__substg1.0_1039001F",
        "<root@example.com> <parent@example.com>",
    );
    string(
        &mut file,
        "/__substg1.0_007D001F",
        "Reply-To: Replies <reply@example.com>\r\n",
    );
    stream(
        &mut file,
        "/__substg1.0_100900102",
        b"ignored noncanonical name",
    );
    let rtf = compressed_rtf::compress_rtf(r"{\rtf1\ansi Hello \b Outlook\b0.}").unwrap();
    stream(&mut file, "/__substg1.0_10090102", &rtf);
    for (i, kind) in [1u64, 2, 3].into_iter().enumerate() {
        let root = format!("/__recip_version1.0_#{i:08X}");
        file.create_storage(&root).unwrap();
        fixed(
            &mut file,
            &format!("{root}/__properties_version1.0"),
            8,
            &[(0x0C150003, kind)],
        );
        string(
            &mut file,
            &format!("{root}/__substg1.0_3001001F"),
            "Recipient",
        );
        string(
            &mut file,
            &format!("{root}/__substg1.0_39FE001F"),
            &format!("r{kind}@example.com"),
        );
    }
    let attach = "/__attach_version1.0_#00000000";
    file.create_storage(attach).unwrap();
    string(
        &mut file,
        &format!("{attach}/__substg1.0_3707001F"),
        "nested.msg",
    );
    let nested = format!("{attach}/__substg1.0_3701000D");
    file.create_storage(&nested).unwrap();
    fixed(
        &mut file,
        &format!("{nested}/__properties_version1.0"),
        24,
        &[(0x00390040, 133498026000000000)],
    );
    string(
        &mut file,
        &format!("{nested}/__substg1.0_0037001F"),
        "Nested subject",
    );
    string(
        &mut file,
        &format!("{nested}/__substg1.0_1000001F"),
        "Nested body",
    );
    file.into_inner().into_inner()
}

#[test]
fn reads_recipient_objects_dates_threads_and_compressed_rtf() {
    let email = parse_msg_bytes(&rich_msg(), ParseOptions::default()).unwrap();
    assert_eq!(email.to[0].address, "r1@example.com");
    assert_eq!(email.cc[0].address, "r2@example.com");
    assert_eq!(email.bcc[0].address, "r3@example.com");
    assert_eq!(email.reply_to.unwrap().address, "reply@example.com");
    assert_eq!(
        email.date.unwrap().to_rfc3339(),
        "2024-01-15T14:30:00+00:00"
    );
    assert_eq!(email.priority, Some(1));
    assert_eq!(
        email.thread_info.in_reply_to.as_deref(),
        Some("<parent@example.com>")
    );
    assert_eq!(email.thread_info.references.len(), 2);
    assert!(email.body.text.unwrap().contains("Hello Outlook"));
}

#[test]
fn embedded_message_is_an_independently_readable_msg() {
    let email = parse_msg_bytes(
        &rich_msg(),
        ParseOptions {
            extract_attachment_content: true,
            ..Default::default()
        },
    )
    .unwrap();
    let attachment = &email.attachments[0];
    assert!(attachment.is_nested_message());
    let bytes = attachment.decode_content().unwrap();
    let nested = parse_msg_bytes(&bytes, ParseOptions::default()).unwrap();
    assert_eq!(nested.subject.as_deref(), Some("Nested subject"));
    assert_eq!(nested.body.text.as_deref(), Some("Nested body"));
    assert!(nested.date.is_some());
    assert!(parse_msg_bytes(
        &rich_msg(),
        ParseOptions {
            max_attachment_size: 1,
            ..Default::default()
        }
    )
    .is_err());
}

#[test]
fn ansi_codepage_is_honored_and_invalid_cfb_returns_errors() {
    let mut file = CompoundFile::create(Cursor::new(Vec::new())).unwrap();
    fixed(
        &mut file,
        "/__properties_version1.0",
        32,
        &[(0x3FFD0003, 1252)],
    );
    stream(&mut file, "/__substg1.0_0037001E", b"caf\xe9\0");
    let bytes = file.into_inner().into_inner();
    assert_eq!(
        parse_msg_bytes(&bytes, ParseOptions::default())
            .unwrap()
            .subject
            .as_deref(),
        Some("café")
    );
    for length in [0, 8, 128, 512] {
        assert!(parse_msg_bytes(&bytes[..length], ParseOptions::default()).is_err());
    }
    let mut bad = bytes;
    bad[0x1e] = 31;
    assert!(parse_msg_bytes(&bad, ParseOptions::default()).is_err());
}

#[test]
fn smtp_one_off_reply_entry_is_decoded_without_transport_headers() {
    let mut file = CompoundFile::create(Cursor::new(Vec::new())).unwrap();
    string(&mut file, "/__substg1.0_0037001F", "Reply entries");
    let mut entry = vec![0u8; 4];
    entry.extend(b"\xa4\x1f\x2b\x81\xa3\xbe\x19\x10\x9d\x6e\x00\xdd\x01\x0f\x54\x02");
    entry.extend([0, 0, 0, 0x80]);
    for field in ["Réplies", "SMTP", "reply@example.com"] {
        entry.extend(
            field
                .encode_utf16()
                .chain(Some(0))
                .flat_map(u16::to_le_bytes),
        );
    }
    let mut flat = 1u32.to_le_bytes().to_vec();
    let padded = (entry.len() + 3) & !3;
    flat.extend(((4 + padded) as u32).to_le_bytes());
    flat.extend((entry.len() as u32).to_le_bytes());
    flat.extend(&entry);
    flat.resize(12 + padded, 0);
    stream(&mut file, "/__substg1.0_004F0102", &flat);
    let reply = parse_msg_bytes(&file.into_inner().into_inner(), ParseOptions::default())
        .unwrap()
        .reply_to
        .unwrap();
    assert_eq!(reply.address, "reply@example.com");
    assert_eq!(reply.name.as_deref(), Some("Réplies"));
}

#[test]
fn cfb_directory_chain_cycles_are_rejected() {
    let mut bytes = write_msg_cfb_fixture("cycle", "a@b", "c@d", "body");
    let sector = 1usize << u16::from_le_bytes(bytes[0x1e..0x20].try_into().unwrap());
    let directory = u32::from_le_bytes(bytes[0x30..0x34].try_into().unwrap());
    let fat = u32::from_le_bytes(bytes[0x4c..0x50].try_into().unwrap());
    let offset = (fat as usize + 1) * sector + directory as usize * 4;
    bytes[offset..offset + 4].copy_from_slice(&directory.to_le_bytes());
    assert!(parse_msg_bytes(&bytes, ParseOptions::default()).is_err());
}
