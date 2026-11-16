use email_fast_core::*;
use std::fs;

fn attachment(name: &str, body: &str) -> Vec<u8> {
    format!("From: a@example.com\r\nContent-Type: application/octet-stream\r\nContent-Disposition: attachment; filename=\"{name}\"\r\n\r\n{body}").into_bytes()
}

#[test]
fn extraction_contains_names_and_never_overwrites() {
    let dir = tempfile::tempdir().unwrap();
    let source = dir.path().join("mail.eml");
    let out = dir.path().join("out");
    fs::create_dir(&out).unwrap();
    fs::write(out.join("escaped.txt"), "existing").unwrap();
    fs::write(&source, attachment("../escaped.txt", "new")).unwrap();
    let paths = extract_attachments(&source, &out, ParseOptions::default()).unwrap();
    assert_eq!(paths.len(), 1);
    assert_eq!(paths[0].parent().unwrap(), out);
    assert_eq!(fs::read_to_string(&paths[0]).unwrap(), "new");
    assert_eq!(
        fs::read_to_string(out.join("escaped.txt")).unwrap(),
        "existing"
    );
    assert!(!dir.path().join("escaped.txt").exists());
}

#[cfg(unix)]
#[test]
fn extraction_does_not_follow_existing_symlinks() {
    let dir = tempfile::tempdir().unwrap();
    let out = dir.path().join("out");
    fs::create_dir(&out).unwrap();
    let target = dir.path().join("target");
    fs::write(&target, "keep").unwrap();
    std::os::unix::fs::symlink(&target, out.join("link.txt")).unwrap();
    let source = dir.path().join("mail.eml");
    fs::write(&source, attachment("link.txt", "new")).unwrap();
    let paths = extract_attachments(source, &out, ParseOptions::default()).unwrap();
    assert_eq!(fs::read_to_string(target).unwrap(), "keep");
    assert_eq!(fs::read_to_string(&paths[0]).unwrap(), "new");
}

#[test]
fn addresses_and_headers_are_decoded_exactly_once() {
    let mail=parse_eml_bytes(b"To: \"Doe, Jane\" <jane@example.com>, Team: bob@example.com, c@example.com;\r\nSubject: =?UTF-8?Q?=3D=3FUTF-8=3FQ=3Fhello=3F=3D?=\r\nMessage-ID: <one@example.com>\r\n\r\nbody", ParseOptions::default()).unwrap();
    assert_eq!(mail.to.len(), 3);
    assert_eq!(mail.to[0].address, "jane@example.com");
    assert_eq!(mail.to[0].name.as_deref(), Some("Doe, Jane"));
    assert_eq!(mail.subject.as_deref(), Some("=?UTF-8?Q?hello?="));
    assert_eq!(
        serde_json::to_string(&mail)
            .unwrap()
            .matches("\"message_id\":")
            .count(),
        1
    );
}

#[test]
fn related_root_and_preferred_alternative_are_bodies() {
    let bytes=b"Content-Type: multipart/related; boundary=x; start=\"<root>\"\r\n\r\n--x\r\nContent-Type: image/png\r\nContent-ID: <image>\r\n\r\npng\r\n--x\r\nContent-Type: text/html\r\nContent-ID: <root>\r\n\r\n<p>Hello</p>\r\n--x--\r\n";
    let email = parse_eml_bytes(bytes, ParseOptions::default()).unwrap();
    assert!(email.body.html.unwrap().contains("Hello"));
    assert_eq!(email.attachments.len(), 1);
    let email=parse_eml_bytes(b"Content-Type: multipart/alternative; boundary=x\r\n\r\n--x\r\nContent-Type: text/plain\r\n\r\nfirst\r\n--x\r\nContent-Type: text/plain\r\n\r\nlast\r\n--x--\r\n",ParseOptions::default()).unwrap();
    assert_eq!(email.body.text.unwrap().trim(), "last");
}

#[test]
fn mbox_preserves_charset_bytes_and_reports_errors() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("box.mbox");
    let mut bytes=b"From a@example.com Mon Jan 1 00:00:00 2024\nContent-Type: text/plain; charset=iso-8859-1\n\ncaf\xe9\n>From quoted\n".to_vec();
    fs::write(&path, &bytes).unwrap();
    parse_mbox(&path, ParseOptions::default(), |mail| {
        assert!(mail.body.text.unwrap().contains("café\nFrom quoted"));
        Ok(())
    })
    .unwrap();
    bytes = b"From a@example.com Mon Jan 1 00:00:00 2024\n".to_vec();
    bytes.extend(attachment("data.bin", "payload"));
    fs::write(&path, bytes).unwrap();
    let error = parse_mbox(
        path,
        ParseOptions {
            max_attachment_size: 1,
            ..Default::default()
        },
        |_| Ok(()),
    )
    .unwrap_err();
    assert!(format!("{error:#}").contains("offset"));
}

#[test]
fn input_and_decoded_attachment_limits_are_enforced() {
    assert!(parse_eml_bytes(
        b"Subject: Hi\r\n\r\nbody",
        ParseOptions {
            max_inflate: 1,
            ..Default::default()
        }
    )
    .is_err());
    let mail = parse_eml_bytes(
        &attachment("a.bin", "x"),
        ParseOptions {
            max_attachment_size: 1,
            max_total_attachment_bytes: 1,
            ..Default::default()
        },
    )
    .unwrap();
    assert_eq!(mail.attachments[0].size, 1);
    assert!(parse_msg_bytes(
        &write_msg_cfb_fixture("s", "a@b", "c@d", "body"),
        ParseOptions {
            max_inflate: 1,
            ..Default::default()
        }
    )
    .is_err());
}

#[test]
fn structure_limits_and_recovery_callback_are_explicit() {
    let bytes=b"Content-Type: multipart/mixed; boundary=x\r\n\r\n--x\r\nContent-Type: text/plain\r\n\r\nbody\r\n--x--\r\n";
    for options in [
        ParseOptions {
            max_mime_depth: 1,
            ..Default::default()
        },
        ParseOptions {
            max_mime_parts: 1,
            ..Default::default()
        },
        ParseOptions {
            max_header_bytes: 8,
            ..Default::default()
        },
    ] {
        assert!(parse_eml_bytes(bytes, options).is_err());
    }
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("box.mbox");
    let mut bytes = b"From a@example.com Mon Jan 1 00:00:00 2024\n".to_vec();
    bytes.extend(attachment("a.bin", "bad"));
    bytes.extend(b"\nFrom b@example.com Tue Jan 2 00:00:00 2024\nSubject: next\n\nbody\n");
    fs::write(&path, bytes).unwrap();
    let mut errors = Vec::new();
    let count = parse_mbox_with_errors(
        path,
        ParseOptions {
            max_attachment_size: 1,
            ..Default::default()
        },
        |e| {
            assert_eq!(e.subject.as_deref(), Some("next"));
            Ok(())
        },
        |index, offset, _| {
            errors.push((index, offset));
            Ok(())
        },
    )
    .unwrap();
    assert_eq!(count, 1);
    assert_eq!(errors, [(1, 0)]);
}

#[cfg(feature = "arrow")]
#[test]
fn multifile_arrow_honors_batch_size_and_rejects_zero() {
    use email_fast_core::arrow::{parse_emails_to_arrow_ipc, ArrowOptions};
    let dir = tempfile::tempdir().unwrap();
    let mut paths = Vec::new();
    for index in 0..5 {
        let path = dir.path().join(format!("{index}.eml"));
        fs::write(&path, format!("Subject: {index}\n\nbody")).unwrap();
        paths.push(path);
    }
    let out = dir.path().join("emails.arrow");
    parse_emails_to_arrow_ipc(
        &paths,
        &out,
        ParseOptions::default(),
        ArrowOptions { batch_size: 2 },
    )
    .unwrap();
    let reader =
        arrow_ipc::reader::FileReader::try_new(fs::File::open(&out).unwrap(), None).unwrap();
    assert_eq!(
        reader
            .map(|batch| batch.unwrap().num_rows())
            .collect::<Vec<_>>(),
        [2, 2, 1]
    );
    assert!(parse_emails_to_arrow_ipc(
        &paths,
        &out,
        ParseOptions::default(),
        ArrowOptions { batch_size: 0 }
    )
    .is_err());
}

#[test]
fn base64_extraction_streams_and_duplicate_names_are_disambiguated() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("mail.eml");
    let out = dir.path().join("out");
    let mut bytes = b"Content-Type: multipart/mixed; boundary=x\r\n\r\n".to_vec();
    for payload in ["aGVsbG8=", "d29ybGQ="] {
        bytes.extend(format!("--x\r\nContent-Type: application/octet-stream\r\nContent-Disposition: attachment; filename=\"file.bin\"\r\nContent-Transfer-Encoding: base64\r\n\r\n{payload}\r\n").as_bytes());
    }
    bytes.extend(b"--x--\r\n");
    fs::write(&path, bytes).unwrap();
    let paths = extract_attachments(path, out, ParseOptions::default()).unwrap();
    assert_eq!(paths.len(), 2);
    assert_ne!(paths[0], paths[1]);
    assert_eq!(fs::read(&paths[0]).unwrap(), b"hello");
    assert_eq!(fs::read(&paths[1]).unwrap(), b"world");
}

#[test]
fn quoted_printable_and_extended_filenames_are_decoded_once() {
    let bytes=b"Content-Type: application/octet-stream\r\nContent-Disposition: attachment; filename*=utf-8''caf%C3%A9.txt\r\nContent-Transfer-Encoding: quoted-printable\r\n\r\ncaf=C3=A9=\r\n next  \r\n";
    let mail = parse_eml_bytes(
        bytes,
        ParseOptions {
            extract_attachment_content: true,
            ..Default::default()
        },
    )
    .unwrap();
    assert_eq!(mail.attachments[0].filename, "café.txt");
    assert_eq!(
        mail.attachments[0].decode_content().unwrap(),
        "café next\r\n".as_bytes()
    );
    let directory = tempfile::tempdir().unwrap();
    let source = directory.path().join("a.eml");
    fs::write(&source, bytes).unwrap();
    let paths = extract_attachments(
        source,
        directory.path().join("output"),
        ParseOptions::default(),
    )
    .unwrap();
    assert_eq!(fs::read(&paths[0]).unwrap(), "café next\r\n".as_bytes());
}

#[test]
fn related_root_filename_does_not_hide_the_body() {
    let bytes=b"Content-Type: multipart/related; boundary=x; start=\"<root>\"\r\n\r\n--x\r\nContent-Type: text/html; name=index.html\r\nContent-Disposition: inline; filename=index.html\r\nContent-ID: <root>\r\n\r\n<p>Root</p>\r\n--x--\r\n";
    let mail = parse_eml_bytes(bytes, ParseOptions::default()).unwrap();
    assert!(mail.body.html.unwrap().contains("Root"));
    assert!(mail.attachments.is_empty());
}

#[test]
fn excessive_mime_nesting_is_rejected_without_unbounded_recursion() {
    let mut bytes = Vec::new();
    for depth in 0..300 {
        bytes.extend(
            format!("Content-Type: multipart/mixed; boundary=b{depth}\r\n\r\n--b{depth}\r\n")
                .as_bytes(),
        );
    }
    bytes.extend(b"Content-Type: text/plain\r\n\r\nbody\r\n");
    for depth in (0..300).rev() {
        bytes.extend(format!("--b{depth}--\r\n").as_bytes());
    }
    assert!(parse_eml_bytes(&bytes, ParseOptions::default()).is_err());
}
