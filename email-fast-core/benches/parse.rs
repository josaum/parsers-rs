//! Message parsing: one large multipart message, a mailbox, and a `.msg`.
//!
//! Every input is built in memory and is the same on every run: a 2 MB
//! multipart message with a plain and an HTML body and two base64
//! attachments, a 5,000-message mbox, and a compound file written by the
//! crate's own fixture writer.

use std::fmt::Write as _;
use std::hint::black_box;
use std::io::Cursor;
use std::time::Duration;

use criterion::{criterion_group, criterion_main, Criterion, Throughput};
use email_fast_core::{
    parse_eml_bytes, parse_mbox_reader, parse_msg_bytes, write_msg_cfb_fixture, ParseOptions,
};

const MBOX_MESSAGES: usize = 5_000;
const BASE64: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";

/// `lines` lines of 76 base64 characters from a fixed generator.
fn base64_block(lines: usize, seed: u32) -> String {
    let mut out = String::with_capacity(lines * 78);
    let mut state = seed;
    for _ in 0..lines {
        for _ in 0..76 {
            state = state.wrapping_mul(1_664_525).wrapping_add(1_013_904_223);
            out.push(char::from(BASE64[(state >> 26) as usize]));
        }
        out.push_str("\r\n");
    }
    out
}

fn multipart_message() -> Vec<u8> {
    let mut plain = String::new();
    let mut html = String::from("<html><body>");
    for index in 0..400 {
        write!(
            plain,
            "Line {index}: the quarterly figures are attached for review; caf=C3=A9 costs rose.\r\n"
        )
        .unwrap();
        write!(
            html,
            "<p>Line {index}: the <b>quarterly</b> figures are attached &amp; ready.</p>\r\n"
        )
        .unwrap();
    }
    html.push_str("</body></html>\r\n");

    let mut message = String::from(concat!(
        "Message-ID: <bench-1@example.com>\r\n",
        "From: \"Ana Souza\" <ana@example.com>\r\n",
        "To: Bruno Lima <bruno@example.com>, carla@example.com\r\n",
        "Cc: =?UTF-8?Q?Jo=C3=A3o_Pereira?= <joao@example.com>\r\n",
        "Subject: =?UTF-8?B?UmVsYXTDs3JpbyB0cmltZXN0cmFs?=\r\n",
        "Date: Mon, 15 Jan 2024 14:30:00 +0000\r\n",
        "MIME-Version: 1.0\r\n",
        "Content-Type: multipart/mixed; boundary=\"outer\"\r\n",
        "\r\n",
        "--outer\r\n",
        "Content-Type: multipart/alternative; boundary=\"inner\"\r\n",
        "\r\n",
        "--inner\r\n",
        "Content-Type: text/plain; charset=utf-8\r\n",
        "Content-Transfer-Encoding: quoted-printable\r\n",
        "\r\n",
    ));
    message.push_str(&plain);
    message.push_str(concat!(
        "--inner\r\n",
        "Content-Type: text/html; charset=utf-8\r\n",
        "\r\n",
    ));
    message.push_str(&html);
    message.push_str("--inner--\r\n");
    for (name, kind, seed) in [
        ("report.pdf", "application/pdf", 7),
        ("figures.xlsx", "application/octet-stream", 11),
    ] {
        write!(
            message,
            "--outer\r\nContent-Type: {kind}; name=\"{name}\"\r\nContent-Disposition: attachment; filename=\"{name}\"\r\nContent-Transfer-Encoding: base64\r\n\r\n"
        )
        .unwrap();
        message.push_str(&base64_block(12_500, seed));
    }
    message.push_str("--outer--\r\n");
    message.into_bytes()
}

fn mbox() -> Vec<u8> {
    let mut out = String::new();
    for index in 0..MBOX_MESSAGES {
        write!(
            out,
            "From sender{index}@example.com Mon Jan 15 14:30:00 2024\n\
             Message-ID: <m{index}@example.com>\n\
             From: Sender {index} <sender{index}@example.com>\n\
             To: list@example.com\n\
             Subject: Status update {index}\n\
             Date: Mon, 15 Jan 2024 14:{:02}:{:02} +0000\n\
             Content-Type: text/plain; charset=utf-8\n\
             \n\
             Hello team,\n\
             \n\
             This is update number {index}. The build passed and the release is on track.\n\
             >From the last report nothing has changed.\n\
             \n\
             Regards,\n\
             Sender {index}\n\
             \n",
            index % 60,
            (index / 60) % 60
        )
        .unwrap();
    }
    out.into_bytes()
}

fn bench(c: &mut Criterion) {
    let options = ParseOptions {
        extract_attachment_content: true,
        ..ParseOptions::default()
    };

    let message = multipart_message();
    let parsed = parse_eml_bytes(&message, options).unwrap();
    assert_eq!(parsed.attachments.len(), 2);
    assert_eq!(parsed.to.len(), 2);
    assert!(parsed
        .body
        .text
        .as_deref()
        .is_some_and(|t| t.contains("Line 399")));
    assert!(parsed.body.html.is_some());
    eprintln!(
        "email multipart input: {} bytes, 2 attachments",
        message.len()
    );
    drop(parsed);

    let mut group = c.benchmark_group("email_2mb_multipart");
    group
        .throughput(Throughput::Bytes(message.len() as u64))
        .sample_size(30)
        .measurement_time(Duration::from_secs(5));
    group.bench_function("json", |b| {
        b.iter(|| {
            let email = parse_eml_bytes(&message, options).unwrap();
            black_box(serde_json::to_vec(&email).unwrap())
        });
    });
    group.bench_function("json_metadata_only", |b| {
        b.iter(|| {
            let email = parse_eml_bytes(&message, ParseOptions::default()).unwrap();
            black_box(serde_json::to_vec(&email).unwrap())
        });
    });
    group.finish();

    let mailbox = mbox();
    let count =
        parse_mbox_reader(Cursor::new(&mailbox), ParseOptions::default(), |_| Ok(())).unwrap();
    assert_eq!(count, MBOX_MESSAGES);
    eprintln!(
        "email mbox input: {} bytes, {count} messages",
        mailbox.len()
    );

    let mut group = c.benchmark_group("email_mbox_5000_messages");
    group
        .throughput(Throughput::Bytes(mailbox.len() as u64))
        .sample_size(20)
        .measurement_time(Duration::from_secs(6));
    group.bench_function("ndjson", |b| {
        b.iter(|| {
            let mut out = Vec::with_capacity(mailbox.len() * 2);
            parse_mbox_reader(Cursor::new(&mailbox), ParseOptions::default(), |email| {
                serde_json::to_writer(&mut out, &email)?;
                out.push(b'\n');
                Ok(())
            })
            .unwrap();
            black_box(out)
        });
    });
    group.finish();

    let body = "The contract is attached. Please sign and return it by Friday.\r\n".repeat(40);
    let msg = write_msg_cfb_fixture(
        "Contract for signature",
        "Ana Souza <ana@example.com>",
        "bruno@example.com",
        &body,
    );
    let parsed = parse_msg_bytes(&msg, ParseOptions::default()).unwrap();
    assert_eq!(parsed.subject.as_deref(), Some("Contract for signature"));
    assert!(parsed
        .body
        .text
        .as_deref()
        .is_some_and(|t| t.contains("Friday")));
    eprintln!("email msg input: {} bytes", msg.len());
    drop(parsed);

    let mut group = c.benchmark_group("email_msg_fixture");
    group
        .throughput(Throughput::Bytes(msg.len() as u64))
        .sample_size(50)
        .measurement_time(Duration::from_secs(3));
    group.bench_function("json", |b| {
        b.iter(|| {
            let email = parse_msg_bytes(&msg, ParseOptions::default()).unwrap();
            black_box(serde_json::to_vec(&email).unwrap())
        });
    });
    group.finish();
}

criterion_group!(benches, bench);
criterion_main!(benches);
