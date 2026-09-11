use email_fast_core::{
    parse_email, parse_eml, parse_mbox, parse_msg, write_msg_cfb_fixture, ParseOptions,
};
use std::fs;
use std::path::Path;
use tempfile::tempdir;

fn write_msg_fixture(path: &Path) {
    fs::write(
        path,
        write_msg_cfb_fixture(
            "Integration MSG",
            "sender@example.com",
            "recipient@example.com",
            "Mensagem MSG",
        ),
    )
    .unwrap();
}

#[test]
fn parses_eml_and_msg_from_disk() {
    let dir = tempdir().unwrap();

    let eml_path = dir.path().join("sample.eml");
    fs::write(
        &eml_path,
        concat!(
            "From: Maria <maria@example.com>\r\n",
            "To: joao@example.com\r\n",
            "Subject: Integração EML\r\n",
            "\r\n",
            "Corpo EML\r\n"
        ),
    )
    .unwrap();

    let eml = parse_eml(&eml_path, ParseOptions::default()).unwrap();
    assert_eq!(eml.subject.as_deref(), Some("Integração EML"));
    assert!(eml
        .body
        .text
        .as_deref()
        .unwrap_or_default()
        .contains("Corpo EML"));

    let detected = parse_email(&eml_path, ParseOptions::default()).unwrap();
    assert_eq!(detected.subject.as_deref(), Some("Integração EML"));

    let msg_path = dir.path().join("sample.msg");
    write_msg_fixture(&msg_path);

    let msg = parse_msg(&msg_path, ParseOptions::default()).unwrap();
    assert_eq!(msg.subject.as_deref(), Some("Integration MSG"));
    assert_eq!(msg.from.address, "sender@example.com");
    assert!(msg
        .body
        .text
        .as_deref()
        .unwrap_or_default()
        .contains("Mensagem MSG"));
}

#[test]
fn parses_mbox_with_multiple_messages_from_disk() {
    let dir = tempdir().unwrap();
    let path = dir.path().join("mailbox.mbox");
    fs::write(
        &path,
        concat!(
            "From sender@example.com Mon Jan 15 14:30:00 2024\n",
            "From: sender@example.com\n",
            "To: first@example.com\n",
            "Subject: First\n",
            "\n",
            "Primeira mensagem\n",
            "From other@example.com Tue Jan 16 08:00:00 2024\n",
            "From: other@example.com\n",
            "To: second@example.com\n",
            "Subject: Second\n",
            "\n",
            "Segunda mensagem\n"
        ),
    )
    .unwrap();

    let mut subjects = Vec::new();
    let count = parse_mbox(&path, ParseOptions::default(), |email| {
        subjects.push(email.subject.unwrap_or_default());
        Ok(())
    })
    .unwrap();

    assert_eq!(count, 2);
    assert_eq!(subjects, vec!["First", "Second"]);
}

#[test]
fn parses_email_with_inline_and_nested_attachments() {
    let dir = tempdir().unwrap();
    let path = dir.path().join("attachments.eml");
    fs::write(
        &path,
        concat!(
            "From: sender@example.com\r\n",
            "To: recipient@example.com\r\n",
            "Subject: Attachments\r\n",
            "MIME-Version: 1.0\r\n",
            "Content-Type: multipart/mixed; boundary=\"outer\"\r\n",
            "\r\n",
            "--outer\r\n",
            "Content-Type: text/plain; charset=UTF-8\r\n",
            "\r\n",
            "Mensagem principal\r\n",
            "--outer\r\n",
            "Content-Type: image/png\r\n",
            "Content-Transfer-Encoding: base64\r\n",
            "Content-Disposition: inline; filename=\"inline.png\"\r\n",
            "Content-ID: <img-1>\r\n",
            "\r\n",
            "aGVsbG8=\r\n",
            "--outer\r\n",
            "Content-Type: message/rfc822\r\n",
            "Content-Disposition: attachment; filename=\"forwarded.eml\"\r\n",
            "\r\n",
            "From: nested@example.com\r\n",
            "To: recipient@example.com\r\n",
            "Subject: Forwarded\r\n",
            "\r\n",
            "Nested body\r\n",
            "--outer\r\n",
            "Content-Type: text/plain; charset=UTF-8\r\n",
            "Content-Disposition: attachment; filename=\"notes.txt\"\r\n",
            "\r\n",
            "Attachment body\r\n",
            "--outer--\r\n"
        ),
    )
    .unwrap();

    let email = parse_eml(
        &path,
        ParseOptions {
            extract_attachment_content: true,
            ..ParseOptions::default()
        },
    )
    .unwrap();

    assert!(email
        .body
        .text
        .as_deref()
        .unwrap_or_default()
        .contains("Mensagem principal"));
    assert_eq!(email.attachments.len(), 3);
    assert!(email.attachments.iter().any(|attachment| {
        attachment.filename == "inline.png"
            && attachment.is_inline
            && attachment.content_id.as_deref() == Some("img-1")
            && attachment.content_type == "image/png"
    }));
    assert!(email.attachments.iter().any(|attachment| {
        attachment.filename == "forwarded.eml" && attachment.content_type == "message/rfc822"
    }));
    assert!(email
        .attachments
        .iter()
        .all(|attachment| attachment.content.is_some()));
}
