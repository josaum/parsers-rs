//! Malformed mail must be rejected with an error: never a panic, a stack
//! overflow, or an allocation sized by a field of the file.
//!
//! The allocation and recursion cases abort the test binary when they fail
//! (through `smoke::CappedAllocator` and a 256 KiB stack), so each one is a
//! separate test that can be run alone with `-- --exact <name>`.

#[path = "../../tools/mutation_smoke.rs"]
mod smoke;

use base64::Engine;
use email_fast_core::{
    parse_eml_bytes, parse_mbox_reader, parse_msg_bytes, write_msg_cfb_fixture,
    write_msg_cfb_fixture_streams, Email, ParseOptions,
};

#[global_allocator]
static ALLOCATOR: smoke::CappedAllocator = smoke::CappedAllocator;

const SECTOR: usize = 512;
const DIR_ENTRY: usize = 128;
const FREESECT: u32 = 0xFFFF_FFFF;
const ENDOFCHAIN: u32 = 0xFFFF_FFFE;
const FATSECT: u32 = 0xFFFF_FFFD;
const OLE_SIGNATURE: [u8; 8] = [0xd0, 0xcf, 0x11, 0xe0, 0xa1, 0xb1, 0x1a, 0xe1];

/// Every optional code path switched on.
fn all_options() -> ParseOptions {
    ParseOptions {
        html_to_text: true,
        include_raw_headers: true,
        extract_attachment_content: true,
        ..ParseOptions::default()
    }
}

/// Touches the accessors a consumer calls on a parsed message.
fn exercise(email: &Email) {
    let _ = email.has_substantive_content();
    for attachment in &email.attachments {
        let _ = attachment.extension();
        let _ = attachment.is_nested_message();
        let _ = attachment.decode_content();
    }
    let _ = serde_json::to_string(email);
}

fn parse_eml_both_ways(bytes: &[u8]) {
    for options in [ParseOptions::default(), all_options()] {
        if let Ok(email) = parse_eml_bytes(bytes, options) {
            exercise(&email);
        }
    }
}

fn parse_msg_both_ways(bytes: &[u8]) {
    for options in [ParseOptions::default(), all_options()] {
        if let Ok(email) = parse_msg_bytes(bytes, options) {
            exercise(&email);
        }
    }
}

fn parse_mbox_both_ways(bytes: &[u8]) {
    for options in [ParseOptions::default(), all_options()] {
        let _ = parse_mbox_reader(std::io::Cursor::new(bytes), options, |email| {
            exercise(&email);
            Ok(())
        });
    }
}

// ---------------------------------------------------------------- seeds ----

const HTML_BODY: &str = concat!(
    "<html><head><style>p { color: red; }</style>",
    "<script>var a = 1 < 2;</script></head><body><!-- note -->",
    "<p>Ol&aacute; &lt;b&gt; &#233; &#x20AC;<br>mundo</p></body></html>",
);

/// A multipart message with encoded-words, RFC 2231 parameters, base64 and
/// quoted-printable parts, an inline image and a nested `message/rfc822`.
fn eml_seed() -> Vec<u8> {
    let html = base64::engine::general_purpose::STANDARD.encode(HTML_BODY);
    format!(
        concat!(
            "From: =?ISO-8859-1?Q?Jos=E9_Mu=F1oz?= <jose@example.com>\r\n",
            "To: =?UTF-8?B?U8O4cmVuLCBLaWVya2VnYWFyZA==?= <s@example.com>,\r\n",
            " \"Doe, Jane\" <j@example.com>, plain@example.com\r\n",
            "Cc: Team: a@example.com, Bob <b@example.com>;, c@example.com\r\n",
            "Reply-To: replies@example.com\r\n",
            "Subject: =?UTF-8?Q?Relat=C3=B3rio?= =?UTF-8?B?IHRyaW1lc3RyYWw=?=\r\n",
            "Date: Mon, 15 Jan 2024 14:30:00 +0000\r\n",
            "Message-ID: <msg-1@example.com>\r\n",
            "In-Reply-To: <msg-0@example.com>\r\n",
            "References: <a@example.com> <b@example.com>\r\n",
            "X-Priority: 3\r\n",
            "MIME-Version: 1.0\r\n",
            "Content-Type: multipart/mixed; boundary=\"outer\"\r\n",
            "\r\n",
            "preamble\r\n",
            "--outer\r\n",
            "Content-Type: multipart/alternative; boundary=\"alt\"\r\n",
            "\r\n",
            "--alt\r\n",
            "Content-Type: text/plain; charset=ISO-8859-1\r\n",
            "Content-Transfer-Encoding: quoted-printable\r\n",
            "\r\n",
            "Ol=E1 mundo=\r\n",
            " continua\r\n",
            "--alt\r\n",
            "Content-Type: text/html; charset=UTF-8\r\n",
            "Content-Transfer-Encoding: base64\r\n",
            "\r\n",
            "{html}\r\n",
            "--alt--\r\n",
            "--outer\r\n",
            "Content-Type: application/pdf\r\n",
            "Content-Transfer-Encoding: base64\r\n",
            "Content-Disposition: attachment;\r\n",
            " filename*0*=UTF-8''r%C3%A9sum%C3%A9; filename*1*=.pdf\r\n",
            "\r\n",
            "JVBERi0xLjQK\r\n",
            "--outer\r\n",
            "Content-Type: image/png; name=\"logo.png\"\r\n",
            "Content-Transfer-Encoding: base64\r\n",
            "Content-ID: <logo@example.com>\r\n",
            "\r\n",
            "iVBORw0KGgo=\r\n",
            "--outer\r\n",
            "Content-Type: message/rfc822\r\n",
            "Content-Disposition: attachment; filename=\"forwarded.eml\"\r\n",
            "\r\n",
            "From: nested@example.com\r\n",
            "To: recipient@example.com\r\n",
            "Subject: =?UTF-8?Q?Encaminhada?=\r\n",
            "Content-Type: multipart/mixed; boundary=\"inner\"\r\n",
            "\r\n",
            "--inner\r\n",
            "Content-Type: text/plain\r\n",
            "\r\n",
            "Nested body\r\n",
            "--inner--\r\n",
            "--outer--\r\n",
        ),
        html = html
    )
    .into_bytes()
}

/// A single-part 8-bit HTML message, so mutations reach the HTML scanner
/// directly instead of having to survive a base64 decode.
fn html_eml_seed() -> Vec<u8> {
    format!(
        concat!(
            "From: \"Maria\" <maria@example.com>\r\n",
            "To: joao@example.com\r\n",
            "Subject: html\r\n",
            "Content-Type: text/html; charset=UTF-8\r\n",
            "\r\n",
            "{html}\r\n",
        ),
        html = HTML_BODY
    )
    .into_bytes()
}

fn mbox_seed() -> Vec<u8> {
    concat!(
        "From sender@example.com Mon Jan 15 14:30:00 2024\n",
        "From: sender@example.com\n",
        "To: first@example.com\n",
        "Subject: =?UTF-8?Q?Primeira?=\n",
        "Content-Type: text/html\n",
        "\n",
        "<p>Primeira mensagem</p>\n",
        "From now on, this line is body text.\n",
        ">From the quoted form too.\n",
        "\n",
        "From other@example.com Tue Jan 16 08:00:00 +0000 2024\n",
        "From: other@example.com\n",
        "To: second@example.com\n",
        "Subject: Second\n",
        "\n",
        "Segunda mensagem\n",
    )
    .as_bytes()
    .to_vec()
}

fn utf16(value: &str) -> Vec<u8> {
    let mut out: Vec<u8> = value.encode_utf16().flat_map(u16::to_le_bytes).collect();
    out.extend_from_slice(&[0, 0]);
    out
}

/// A property stream: a `header_len`-byte header, then 16-byte entries.
fn property_stream(header_len: usize, properties: &[(u32, u64)]) -> Vec<u8> {
    let mut stream = vec![0u8; header_len];
    for (tag, value) in properties {
        stream.extend_from_slice(&tag.to_le_bytes());
        stream.extend_from_slice(&6u32.to_le_bytes());
        stream.extend_from_slice(&value.to_le_bytes());
    }
    stream
}

/// 2024-01-15 14:30:00 UTC as a FILETIME.
const SUBMIT_FILETIME: u64 = 133_498_026_000_000_000;

/// A message with a property stream, Unicode and ANSI strings, a binary HTML
/// body, two recipient storages and two attachment storages (one small enough
/// for the mini stream, one stored in regular sectors).
fn msg_seed() -> Vec<u8> {
    let mut html = b"<html><style>p{}</style><p>".to_vec();
    html.extend_from_slice(&[0xA3, 0xF3, 0x64, 0xBC]);
    html.extend_from_slice(b"</p><script>x()</script></html>");
    let mut png = b"\x89PNG\r\n\x1a\n".to_vec();
    png.resize(5000, 0x5A);
    write_msg_cfb_fixture_streams(&[
        (
            "__properties_version1.0",
            property_stream(
                32,
                &[
                    (0x3FFD_0003, 1252),
                    (0x3FDE_0003, 28592),
                    (0x0039_0040, SUBMIT_FILETIME),
                    (0x0E06_0040, SUBMIT_FILETIME + 10_000_000),
                ],
            ),
        ),
        ("__substg1.0_0037001F", utf16("Relatório trimestral")),
        ("__substg1.0_0C1A001E", b"Jos\xE9 Silva\0".to_vec()),
        ("__substg1.0_0C1F001F", utf16("jose@example.com")),
        ("__substg1.0_0E04001F", utf16("Maria Souza; Team")),
        ("__substg1.0_0E03001F", utf16("Carlos")),
        ("__substg1.0_1000001F", utf16("Corpo da mensagem")),
        ("__substg1.0_10130102", html),
        ("__substg1.0_1035001F", utf16("<msg-1@example.com>")),
        (
            "__recip_version1.0_#00000000/__properties_version1.0",
            property_stream(8, &[(0x0C15_0003, 1)]),
        ),
        (
            "__recip_version1.0_#00000000/__substg1.0_3001001F",
            utf16("Maria Souza"),
        ),
        (
            "__recip_version1.0_#00000000/__substg1.0_3003001F",
            utf16("/O=EXAMPLE/CN=RECIPIENTS/CN=MARIA"),
        ),
        (
            "__recip_version1.0_#00000000/__substg1.0_39FE001F",
            utf16("maria@example.com"),
        ),
        (
            "__recip_version1.0_#00000001/__properties_version1.0",
            property_stream(8, &[(0x0C15_0003, 2)]),
        ),
        (
            "__recip_version1.0_#00000001/__substg1.0_3001001E",
            b"Carlos\0".to_vec(),
        ),
        (
            "__recip_version1.0_#00000001/__substg1.0_3003001F",
            utf16("carlos@example.com"),
        ),
        (
            "__attach_version1.0_#00000000/__properties_version1.0",
            property_stream(8, &[(0x3705_0003, 1)]),
        ),
        (
            "__attach_version1.0_#00000000/__substg1.0_37010102",
            b"%PDF-1.4 fixture".to_vec(),
        ),
        (
            "__attach_version1.0_#00000000/__substg1.0_3707001F",
            utf16("relatório.pdf"),
        ),
        (
            "__attach_version1.0_#00000000/__substg1.0_370E001F",
            utf16("application/pdf"),
        ),
        ("__attach_version1.0_#00000001/__substg1.0_37010102", png),
        (
            "__attach_version1.0_#00000001/__substg1.0_3704001E",
            b"logo.png\0".to_vec(),
        ),
        (
            "__attach_version1.0_#00000001/__substg1.0_3712001F",
            utf16("logo@example"),
        ),
    ])
}

/// The same message with the mini stream cutoff zeroed, so every stream is
/// read through the regular FAT chain reader instead of the mini FAT.
fn msg_seed_without_mini_stream() -> Vec<u8> {
    let mut bytes = msg_seed();
    bytes[0x38..0x3C].copy_from_slice(&0u32.to_le_bytes());
    bytes
}

// ------------------------------------------------------- mutation smoke ----

#[test]
fn seeds_parse_cleanly() {
    let eml = parse_eml_bytes(&eml_seed(), all_options()).expect("eml seed");
    assert_eq!(eml.subject.as_deref(), Some("Relatório trimestral"));
    assert_eq!(eml.attachments.len(), 3);
    assert_eq!(eml.attachments[0].filename, "résumé.pdf");

    let html = parse_eml_bytes(&html_eml_seed(), all_options()).expect("html seed");
    assert!(html.body.html.is_some());

    let msg = parse_msg_bytes(&msg_seed(), all_options()).expect("msg seed");
    assert_eq!(msg.subject.as_deref(), Some("Relatório trimestral"));
    assert_eq!(msg.from.name.as_deref(), Some("José Silva"));
    assert_eq!(msg.attachments.len(), 2);
    assert_eq!(msg.attachments[0].filename, "relatório.pdf");
    assert_eq!(msg.attachments[1].size, 5000);
    assert_eq!(
        msg.date.map(|date| date.to_rfc3339()).as_deref(),
        Some("2024-01-15T14:30:00+00:00")
    );
    // Recipients come from the recipient storages, not the display strings:
    // the first has an SMTP address, the second an address of no stated type.
    let recipients = |list: &[email_fast_core::EmailAddress]| -> Vec<(Option<String>, String)> {
        list.iter()
            .map(|entry| (entry.name.clone(), entry.address.clone()))
            .collect()
    };
    assert_eq!(
        recipients(&msg.to),
        [(
            Some("Maria Souza".to_string()),
            "maria@example.com".to_string()
        )]
    );
    assert_eq!(
        recipients(&msg.cc),
        [(Some("Carlos".to_string()), String::new())]
    );

    let mut subjects = Vec::new();
    let count = parse_mbox_reader(
        std::io::Cursor::new(mbox_seed()),
        ParseOptions::default(),
        |email| {
            subjects.push(email.subject.unwrap_or_default());
            Ok(())
        },
    )
    .expect("mbox seed");
    assert_eq!(count, 2, "{subjects:?}");
}

#[test]
fn mutated_eml_never_panics() {
    // The whole run uses a 256 KiB stack, so a mutation that nests deeper
    // than the parser allows is caught as well as one that panics.
    smoke::run_on_small_stack(|| {
        smoke::assert_no_panics(
            "email-fast-core eml",
            &[eml_seed(), html_eml_seed()],
            6_000,
            parse_eml_both_ways,
        );
    });
}

#[test]
fn mutated_msg_never_panics() {
    smoke::run_on_small_stack(|| {
        smoke::assert_no_panics(
            "email-fast-core msg",
            &[
                msg_seed(),
                msg_seed_without_mini_stream(),
                write_msg_cfb_fixture("Subject", "a@example.com", "b@example.com", "Body"),
            ],
            20_000,
            parse_msg_both_ways,
        );
    });
}

#[test]
fn mutated_mbox_never_panics() {
    smoke::run_on_small_stack(|| {
        smoke::assert_no_panics(
            "email-fast-core mbox",
            &[mbox_seed()],
            6_000,
            parse_mbox_both_ways,
        );
    });
}

// ------------------------------------------------------ EML regressions ----

#[test]
fn address_with_the_closing_bracket_first_is_not_a_panic() {
    let email = parse_eml_bytes(
        b"From: a> <b\r\nSubject: x\r\n\r\nbody\r\n",
        ParseOptions::default(),
    )
    .expect("lenient address parse");
    assert_eq!(email.subject.as_deref(), Some("x"));
    assert!(email.from.address.contains('b'), "{:?}", email.from);
}

/// `depth` multipart containers, each holding the next; every level has its
/// own boundary, which is what makes the MIME tree actually nest.
fn nested_multipart(depth: usize) -> Vec<u8> {
    let mut out = b"From: a@example.com\r\nSubject: deep\r\nMIME-Version: 1.0\r\n".to_vec();
    for level in 0..depth {
        out.extend_from_slice(
            format!("Content-Type: multipart/mixed; boundary=b{level:06}\r\n\r\n--b{level:06}\r\n")
                .as_bytes(),
        );
    }
    out.extend_from_slice(b"Content-Type: text/plain\r\n\r\nleaf\r\n");
    out
}

#[test]
fn deeply_nested_multipart_is_bounded_on_a_small_stack() {
    for depth in [200, 5_000] {
        let message = nested_multipart(depth);
        smoke::run_on_small_stack(move || {
            assert!(
                parse_eml_bytes(&message, all_options()).is_err(),
                "{depth} nested multiparts must be rejected"
            );
        });
    }
}

#[test]
fn ordinary_multipart_nesting_is_still_accepted() {
    let message = nested_multipart(8);
    smoke::run_on_small_stack(move || {
        let email = parse_eml_bytes(&message, all_options()).expect("eight levels parse");
        assert_eq!(email.body.text.as_deref().map(str::trim), Some("leaf"));
    });
}

#[test]
fn deep_nesting_inside_headers_and_html_is_bounded_on_a_small_stack() {
    let cases: Vec<Vec<u8>> = vec![
        // Nested comments and angle brackets in an address header.
        [
            b"From: ".to_vec(),
            smoke::nested(b"(", b"a@example.com", b")", 100_000),
            b"\r\nSubject: x\r\n\r\nbody\r\n".to_vec(),
        ]
        .concat(),
        [
            b"To: ".to_vec(),
            smoke::nested(b"<", b"a@example.com", b">", 100_000),
            b"\r\n\r\nbody\r\n".to_vec(),
        ]
        .concat(),
        [
            b"To: ".to_vec(),
            smoke::nested(b"g: ", b"a@example.com", b";", 100_000),
            b"\r\n\r\nbody\r\n".to_vec(),
        ]
        .concat(),
        // Nested encoded-words. `mailparse` decodes such a line in time
        // quadratic in its encoded-word count; the parser refuses the line
        // before `mailparse` sees it.
        [
            b"Subject: ".to_vec(),
            smoke::nested(b"=?UTF-8?Q?", b"x", b"?=", 100_000),
            b"\r\n\r\nbody\r\n".to_vec(),
        ]
        .concat(),
        // Nested forwarded messages.
        [smoke::nested(
            b"Content-Type: message/rfc822\r\n\r\n",
            b"Subject: leaf\r\n\r\nbody\r\n",
            b"",
            50_000,
        )]
        .concat(),
        // Nested markup, unterminated elements and comments.
        [
            b"Content-Type: text/html\r\n\r\n".to_vec(),
            smoke::nested(b"<div>", b"text", b"</div>", 100_000),
        ]
        .concat(),
        [
            b"Content-Type: text/html\r\n\r\n".to_vec(),
            smoke::nested(b"<script>", b"text", b"", 100_000),
        ]
        .concat(),
        [
            b"Content-Type: text/html\r\n\r\n".to_vec(),
            smoke::nested(b"<!--", b"text", b"", 100_000),
        ]
        .concat(),
        [
            b"Content-Type: text/html\r\n\r\n".to_vec(),
            smoke::nested(b"&", b"amp;", b"", 100_000),
        ]
        .concat(),
    ];
    for message in cases {
        smoke::run_on_small_stack(move || parse_eml_both_ways(&message));
    }
}

/// A header line of `count` encoded-words that are opened and never ended:
/// each `?=` is followed by a letter, not by a delimiter.
fn unended_encoded_words(count: usize) -> String {
    " =?a?=x".repeat(count)
}

/// `mailparse` walks a header line from its start for every `=?` and `?=`
/// it examines, and searches a line of unended words once per word: 300 of
/// those took 3.7 s in an unoptimized build, and 4,000 would take hours.
/// Such lines are refused wherever `mailparse` would decode them: in a
/// header this crate asks for, in the `Content-Type` it reads while
/// splitting the message, and in the headers of a part.
#[test]
fn header_lines_that_are_costly_to_decode_are_refused() {
    let unended = unended_encoded_words(4_000);
    let nested =
        String::from_utf8(smoke::nested(b"=?UTF-8?Q?", b"x", b"?=", 100_000)).expect("ASCII");
    let adjacent = " =?UTF-8?Q?x?=".repeat(100_000);
    let cases = [
        format!("Subject:{unended}\r\n\r\nbody\r\n"),
        format!("Subject: {nested}\r\n\r\nbody\r\n"),
        format!("Subject:{adjacent}\r\n\r\nbody\r\n"),
        format!("Content-Type: text/plain;{unended}\r\n\r\nbody\r\n"),
        format!(
            "Content-Type: multipart/mixed; boundary=b\r\n\r\n--b\r\n\
             Content-Disposition: inline;{unended}\r\n\r\ntext\r\n--b--\r\n"
        ),
    ];
    for message in cases {
        let started = std::time::Instant::now();
        let err = parse_eml_bytes(message.as_bytes(), ParseOptions::default())
            .expect_err("a costly header line must be refused");
        assert!(err.to_string().contains("too costly"), "{err}");
        assert!(
            started.elapsed() < std::time::Duration::from_secs(5),
            "refusing took {:?}",
            started.elapsed()
        );
    }
}

/// Body text is never costed as a header, whatever line precedes it: only a
/// delimiter line of a declared boundary starts a part's headers.
#[test]
fn body_lines_after_a_dash_line_are_not_costed_as_headers() {
    let words = unended_encoded_words(4_000);
    for dashes in ["-- ", "-----Original Message-----", "-- a comment"] {
        let message = format!("Subject: s\r\n\r\nhello\r\n{dashes}\r\nX:{words}\r\n\r\n");
        let email = parse_eml_bytes(message.as_bytes(), ParseOptions::default())
            .expect("body text is not a header");
        assert_eq!(email.subject.as_deref(), Some("s"), "{dashes}");
    }
}

/// `mailparse` searches a multipart body for its boundary in time
/// proportional to body size times boundary length (a 900-dash boundary
/// over 5 MB took 1.4 s optimized). RFC 2046 allows 70 characters; a
/// boundary far past that is refused, at any nesting level.
#[test]
fn an_overlong_multipart_boundary_is_refused() {
    let long = "-".repeat(900);
    let cases = [
        format!("Content-Type: multipart/mixed; boundary=\"{long}\"\r\n\r\n--{long}\r\n\r\ntext\r\n"),
        format!(
            "Content-Type: multipart/mixed; boundary=a\r\n\r\n--a\r\n\
             Content-Type: multipart/alternative;\r\n boundary={long}\r\n\r\n--{long}\r\n\r\ntext\r\n--a--\r\n"
        ),
    ];
    for message in cases {
        let err = parse_eml_bytes(message.as_bytes(), ParseOptions::default())
            .expect_err("an overlong boundary must be refused");
        assert!(err.to_string().contains("boundary longer than"), "{err}");
    }

    // The longest boundary the standard allows, and some slack, still parse.
    for len in [70, 200] {
        let boundary = "b".repeat(len);
        let message = format!(
            "Content-Type: multipart/mixed; boundary={boundary}\r\n\r\n--{boundary}\r\n\r\ntext\r\n--{boundary}--\r\n"
        );
        let email = parse_eml_bytes(message.as_bytes(), ParseOptions::default()).expect("parses");
        assert_eq!(
            email.body.text.as_deref().map(str::trim),
            Some("text"),
            "{len}"
        );
    }
}

/// An unfolded `To:` of 2,000 properly encoded recipients is ordinary work
/// for `mailparse` (a few milliseconds optimized), with or without a raw
/// non-ASCII character on the line, and base64 padding (`==?=`) does not
/// count as the start of a word.
#[test]
fn a_long_unfolded_recipient_line_still_parses() {
    let recipients = "=?UTF-8?B?eA==?= <a@example.com>, ".repeat(2_000);
    for first in ["J <j@example.com>", "J\u{e9} <j@example.com>"] {
        let message = format!("To: {recipients}{first}\r\nSubject: s\r\n\r\nbody\r\n");
        let email = parse_eml_bytes(message.as_bytes(), ParseOptions::default())
            .expect("a long recipient line parses");
        assert_eq!(email.to.len(), 2_001, "{first}");
        assert_eq!(email.to[0].name.as_deref(), Some("x"));
        assert_eq!(email.to[0].address, "a@example.com");
    }
}

/// The bound is on cost, not on the number of encoded-words: properly ended
/// words still decode, on one long unfolded line or folded one per line.
#[test]
fn many_proper_encoded_words_in_a_header_still_decode() {
    let unfolded = format!("Subject:{}\r\n\r\nbody\r\n", " =?UTF-8?Q?x?=".repeat(500));
    let email = parse_eml_bytes(unfolded.as_bytes(), ParseOptions::default()).expect("parses");
    assert_eq!(email.subject, Some("x".repeat(500)));

    let folded = format!(
        "Subject:{} end\r\n\r\nbody\r\n",
        " =?UTF-8?Q?x?=\r\n".repeat(5_000)
    );
    let email = parse_eml_bytes(folded.as_bytes(), ParseOptions::default()).expect("parses");
    assert_eq!(email.subject, Some(format!("{} end", "x".repeat(5_000))));
}

// ------------------------------------------------------ MSG regressions ----

/// Offset of directory entry `index` in a fixture whose directory is one
/// contiguous run of sectors (true for everything the fixture writer emits).
fn dir_entry_offset(bytes: &[u8], index: usize) -> usize {
    let first_dir = u32::from_le_bytes(bytes[0x30..0x34].try_into().unwrap()) as usize;
    SECTOR + first_dir * SECTOR + index * DIR_ENTRY
}

fn small_msg() -> Vec<u8> {
    write_msg_cfb_fixture("Subject", "a@example.com", "b@example.com", "Body")
}

#[test]
fn msg_difat_count_does_not_size_an_allocation() {
    let mut bytes = small_msg();
    // The header's DIFAT sector count is not backed by anything in the file.
    bytes[0x48..0x4C].copy_from_slice(&u32::MAX.to_le_bytes());
    // The count is advisory; the DIFAT chain itself is empty, so this parses.
    let email = parse_msg_bytes(&bytes, ParseOptions::default()).expect("count is advisory");
    assert_eq!(email.subject.as_deref(), Some("Subject"));
}

#[test]
fn msg_stream_size_does_not_size_an_allocation() {
    let mut bytes = small_msg();
    // Raise the mini stream cutoff so a 2 GiB stream still counts as "small",
    // then declare that size on the first property stream.
    bytes[0x38..0x3C].copy_from_slice(&u32::MAX.to_le_bytes());
    let size_at = dir_entry_offset(&bytes, 1) + 0x78;
    bytes[size_at..size_at + 8].copy_from_slice(&0x8000_0000u64.to_le_bytes());
    assert!(parse_msg_bytes(&bytes, ParseOptions::default()).is_err());

    // The same declared size on the regular FAT path.
    let mut bytes = small_msg();
    bytes[0x38..0x3C].copy_from_slice(&0u32.to_le_bytes());
    let size_at = dir_entry_offset(&bytes, 1) + 0x78;
    bytes[size_at..size_at + 8].copy_from_slice(&u64::MAX.to_le_bytes());
    assert!(parse_msg_bytes(&bytes, ParseOptions::default()).is_err());
}

/// How directory entry N points at entry N + 1 in [`chained_directory_cfb`].
#[derive(Clone, Copy)]
enum Link {
    Left,
    Right,
    Child,
}

fn write_raw_dir_entry(entry: &mut [u8], name: &str, obj_type: u8, start: u32, size: u64) {
    let units: Vec<u16> = name.encode_utf16().collect();
    for (index, unit) in units.iter().enumerate() {
        entry[index * 2..index * 2 + 2].copy_from_slice(&unit.to_le_bytes());
    }
    entry[0x40..0x42].copy_from_slice(&(((units.len() + 1) * 2) as u16).to_le_bytes());
    entry[0x42] = obj_type;
    entry[0x43] = 1;
    for link in [0x44, 0x48, 0x4C] {
        entry[link..link + 4].copy_from_slice(&FREESECT.to_le_bytes());
    }
    entry[0x74..0x78].copy_from_slice(&start.to_le_bytes());
    entry[0x78..0x80].copy_from_slice(&size.to_le_bytes());
}

fn write_raw_header(out: &mut [u8], fat_sectors: usize, first_dir: usize) {
    out[..8].copy_from_slice(&OLE_SIGNATURE);
    out[0x18..0x1A].copy_from_slice(&0x003Eu16.to_le_bytes());
    out[0x1A..0x1C].copy_from_slice(&3u16.to_le_bytes());
    out[0x1C..0x1E].copy_from_slice(&0xFFFEu16.to_le_bytes());
    out[0x1E..0x20].copy_from_slice(&9u16.to_le_bytes());
    out[0x20..0x22].copy_from_slice(&6u16.to_le_bytes());
    out[0x2C..0x30].copy_from_slice(&(fat_sectors as u32).to_le_bytes());
    out[0x30..0x34].copy_from_slice(&(first_dir as u32).to_le_bytes());
    // Mini stream cutoff 0 and no mini FAT: every stream uses the FAT.
    out[0x38..0x3C].copy_from_slice(&0u32.to_le_bytes());
    out[0x3C..0x40].copy_from_slice(&ENDOFCHAIN.to_le_bytes());
    out[0x44..0x48].copy_from_slice(&ENDOFCHAIN.to_le_bytes());
    for slot in 0..109 {
        let sector = if slot < fat_sectors {
            slot as u32
        } else {
            FREESECT
        };
        out[0x4C + slot * 4..0x50 + slot * 4].copy_from_slice(&sector.to_le_bytes());
    }
}

/// A compound file whose directory is one chain of `count` entries below the
/// root, entry N pointing at entry N + 1 through `link`.
///
/// Every entry is a `__substg1.0_1000001F` stream (a storage for
/// [`Link::Child`]) that claims `stream_size` bytes of one shared run of
/// `payload_sectors` sectors.
fn chained_directory_cfb(
    count: usize,
    link: Link,
    stream_size: u64,
    payload_sectors: usize,
) -> Vec<u8> {
    let dir_sectors = ((count + 1) * DIR_ENTRY).div_ceil(SECTOR);
    let mut fat_sectors = 1;
    while fat_sectors * (SECTOR / 4) < fat_sectors + dir_sectors + payload_sectors {
        fat_sectors += 1;
    }
    assert!(fat_sectors <= 109, "layout needs a DIFAT sector");
    let dir_start = fat_sectors;
    let payload_start = dir_start + dir_sectors;
    let total_sectors = payload_start + payload_sectors;

    let mut out = vec![0u8; SECTOR + total_sectors * SECTOR];
    write_raw_header(&mut out, fat_sectors, dir_start);

    let mut fat = vec![FREESECT; fat_sectors * (SECTOR / 4)];
    fat[..fat_sectors].fill(FATSECT);
    for (start, len) in [(dir_start, dir_sectors), (payload_start, payload_sectors)] {
        for offset in 0..len {
            fat[start + offset] = if offset + 1 == len {
                ENDOFCHAIN
            } else {
                (start + offset + 1) as u32
            };
        }
    }
    for (index, value) in fat.iter().enumerate() {
        out[SECTOR + index * 4..SECTOR + index * 4 + 4].copy_from_slice(&value.to_le_bytes());
    }

    let dir_off = SECTOR + dir_start * SECTOR;
    let (obj_type, link_at) = match link {
        Link::Left => (2, 0x44),
        Link::Right => (2, 0x48),
        Link::Child => (1, 0x4C),
    };
    let stream_start = if payload_sectors == 0 {
        ENDOFCHAIN
    } else {
        payload_start as u32
    };
    for index in 0..=count {
        let entry = &mut out[dir_off + index * DIR_ENTRY..dir_off + (index + 1) * DIR_ENTRY];
        if index == 0 {
            write_raw_dir_entry(entry, "Root Entry", 5, ENDOFCHAIN, 0);
            if count > 0 {
                entry[0x4C..0x50].copy_from_slice(&1u32.to_le_bytes());
            }
            continue;
        }
        write_raw_dir_entry(
            entry,
            "__substg1.0_1000001F",
            obj_type,
            stream_start,
            stream_size,
        );
        if index < count {
            entry[link_at..link_at + 4].copy_from_slice(&(index as u32 + 1).to_le_bytes());
        }
    }
    out
}

#[test]
fn msg_streams_sharing_one_chain_are_bounded_by_the_file() {
    // 4096 directory entries all claim the same 512 KiB run of sectors: about
    // 1 MiB of file asking for 2 GiB of stream copies.
    let bytes = chained_directory_cfb(4096, Link::Right, 1024 * SECTOR as u64, 1024);
    assert!(parse_msg_bytes(&bytes, ParseOptions::default()).is_err());
}

#[test]
fn msg_long_directory_chains_are_walked_on_a_small_stack() {
    for link in [Link::Right, Link::Left, Link::Child] {
        let bytes = chained_directory_cfb(20_000, link, 0, 0);
        smoke::run_on_small_stack(move || parse_msg_both_ways(&bytes));
    }
}

#[test]
fn msg_difat_listing_one_fat_sector_repeatedly_is_rejected() {
    // Sector 0 is the FAT, sector 1 the directory, and every later sector is a
    // DIFAT sector that lists sector 0 127 times. Loading each listed FAT
    // sector would build a FAT 127 times the size of the file.
    const DIFAT_SECTORS: usize = 17_000;
    let mut out = vec![0u8; SECTOR + (2 + DIFAT_SECTORS) * SECTOR];
    write_raw_header(&mut out, 1, 1);
    out[0x44..0x48].copy_from_slice(&2u32.to_le_bytes());
    out[0x48..0x4C].copy_from_slice(&(DIFAT_SECTORS as u32).to_le_bytes());
    let fat_off = SECTOR;
    out[fat_off..fat_off + SECTOR].fill(0xFF);
    out[fat_off..fat_off + 4].copy_from_slice(&FATSECT.to_le_bytes());
    out[fat_off + 4..fat_off + 8].copy_from_slice(&ENDOFCHAIN.to_le_bytes());
    let dir_off = SECTOR + SECTOR;
    write_raw_dir_entry(
        &mut out[dir_off..dir_off + DIR_ENTRY],
        "Root Entry",
        5,
        ENDOFCHAIN,
        0,
    );
    for index in 0..DIFAT_SECTORS {
        let sector = 2 + index;
        let off = SECTOR + sector * SECTOR;
        // 127 entries naming FAT sector 0 are already zero; link to the next.
        let next = if index + 1 == DIFAT_SECTORS {
            ENDOFCHAIN
        } else {
            sector as u32 + 1
        };
        out[off + SECTOR - 4..off + SECTOR].copy_from_slice(&next.to_le_bytes());
    }
    assert!(parse_msg_bytes(&out, ParseOptions::default()).is_err());
}

#[test]
fn msg_difat_cycle_is_detected_at_the_repeated_sector() {
    let mut bytes = small_msg();
    // Append a DIFAT sector that lists no FAT sectors and names itself next.
    let sector = ((bytes.len() - SECTOR) / SECTOR) as u32;
    let mut difat = vec![0xFFu8; SECTOR];
    difat[SECTOR - 4..].copy_from_slice(&sector.to_le_bytes());
    bytes.extend_from_slice(&difat);
    bytes[0x44..0x48].copy_from_slice(&sector.to_le_bytes());
    let error = parse_msg_bytes(&bytes, ParseOptions::default()).expect_err("cycle");
    assert!(
        error.to_string().contains("DIFAT cycle at sector"),
        "{error}"
    );
}

#[test]
fn msg_substg_name_with_non_ascii_characters_is_not_a_panic() {
    // Byte 4 of the tag part falls inside a two-byte character.
    let bytes = write_msg_cfb_fixture_streams(&[
        ("__substg1.0_0037001F", utf16("Subject")),
        ("__substg1.0_a\u{e9}\u{e9}\u{e9}\u{e9}", utf16("ignored")),
        ("__substg1.0_\u{e9}\u{e9}\u{e9}\u{e9}001F", utf16("ignored")),
    ]);
    let email = parse_msg_bytes(&bytes, ParseOptions::default()).expect("unknown names skipped");
    assert_eq!(email.subject.as_deref(), Some("Subject"));
}
