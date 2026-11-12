//! Slide text must survive the cases real decks contain: a stray bad byte,
//! CDATA, line breaks, fields, and property elements written self-closed.

use pptx_fast_core::{parse_pptx_bytes, ParseOptions, PptxDocument};
use std::io::{Cursor, Write};
use zip::write::SimpleFileOptions;
use zip::{CompressionMethod, ZipWriter};

const P: &str = "http://schemas.openxmlformats.org/presentationml/2006/main";
const A: &str = "http://schemas.openxmlformats.org/drawingml/2006/main";
const REL: &str = "http://schemas.openxmlformats.org/officeDocument/2006/relationships";
const MODERN: &str = "http://schemas.microsoft.com/office/2018/10/relationships";
const P188: &str = "http://schemas.microsoft.com/office/powerpoint/2018/8/main";
const PKG_REL: &str = "http://schemas.openxmlformats.org/package/2006/relationships";

/// The parts of a one-slide deck. Every body is the inner XML of the part's
/// main container, as bytes so a test can plant a byte that is not UTF-8.
#[derive(Default)]
struct Deck {
    /// Inner XML of the slide's `<p:spTree>`.
    slide: Vec<u8>,
    /// Inner XML of the notes slide's `<p:spTree>`.
    notes: Vec<u8>,
    /// Inner XML of `<p:cmLst>`.
    comments: Vec<u8>,
    /// Inner XML of the modern comments part's `<p188:cmLst>`.
    modern_comments: Vec<u8>,
    /// Whole `docProps/core.xml`.
    core: Vec<u8>,
    /// Whole `docProps/custom.xml`.
    custom: Vec<u8>,
}

impl Deck {
    fn slide(body: impl AsRef<[u8]>) -> Self {
        Self {
            slide: body.as_ref().to_vec(),
            ..Self::default()
        }
    }

    fn build(&self) -> Vec<u8> {
        let wrap = |open: String, body: &[u8], close: &str| {
            let mut part = open.into_bytes();
            part.extend_from_slice(body);
            part.extend_from_slice(close.as_bytes());
            part
        };
        let slide = wrap(
            format!(r#"<p:sld xmlns:p="{P}" xmlns:a="{A}"><p:cSld><p:spTree>"#),
            &self.slide,
            "</p:spTree></p:cSld></p:sld>",
        );
        let notes = wrap(
            format!(r#"<p:notes xmlns:p="{P}" xmlns:a="{A}"><p:cSld><p:spTree>"#),
            &self.notes,
            "</p:spTree></p:cSld></p:notes>",
        );
        let comments = wrap(
            format!(r#"<p:cmLst xmlns:p="{P}">"#),
            &self.comments,
            "</p:cmLst>",
        );
        let modern_comments = wrap(
            format!(r#"<p188:cmLst xmlns:p188="{P188}" xmlns:a="{A}">"#),
            &self.modern_comments,
            "</p188:cmLst>",
        );
        let parts: Vec<(&str, Vec<u8>)> = vec![
            (
                "_rels/.rels",
                format!(
                    r#"<Relationships xmlns="{PKG_REL}"><Relationship Id="rId1" Type="{REL}/officeDocument" Target="ppt/presentation.xml"/></Relationships>"#
                )
                .into_bytes(),
            ),
            (
                "ppt/presentation.xml",
                format!(
                    r#"<p:presentation xmlns:p="{P}" xmlns:r="{REL}"><p:sldIdLst><p:sldId id="256" r:id="rId1"/></p:sldIdLst></p:presentation>"#
                )
                .into_bytes(),
            ),
            (
                "ppt/_rels/presentation.xml.rels",
                format!(
                    r#"<Relationships xmlns="{PKG_REL}"><Relationship Id="rId1" Type="{REL}/slide" Target="slides/slide1.xml"/><Relationship Id="rId2" Type="{MODERN}/authors" Target="authors.xml"/></Relationships>"#
                )
                .into_bytes(),
            ),
            ("ppt/slides/slide1.xml", slide),
            (
                "ppt/slides/_rels/slide1.xml.rels",
                format!(
                    r#"<Relationships xmlns="{PKG_REL}"><Relationship Id="rId1" Type="{REL}/notesSlide" Target="../notesSlides/notesSlide1.xml"/><Relationship Id="rId2" Type="{REL}/comments" Target="../comments/comment1.xml"/><Relationship Id="rId3" Type="{MODERN}/comments" Target="../comments/modernComment_100_0.xml"/></Relationships>"#
                )
                .into_bytes(),
            ),
            ("ppt/notesSlides/notesSlide1.xml", notes),
            ("ppt/comments/comment1.xml", comments),
            (
                "ppt/commentAuthors.xml",
                format!(
                    r#"<p:cmAuthorLst xmlns:p="{P}"><p:cmAuthor id="0" name="Ada" initials="A" lastIdx="1" clrIdx="0"/></p:cmAuthorLst>"#
                )
                .into_bytes(),
            ),
            ("ppt/comments/modernComment_100_0.xml", modern_comments),
            (
                "ppt/authors.xml",
                format!(
                    r#"<p188:authorLst xmlns:p188="{P188}"><p188:author id="{{AAAA}}" name="Grace &amp; Co" initials="G" userId="u1" providerId="AD"/><p188:author id="{{BBBB}}" name="Linus" initials="L" userId="u2" providerId="AD"/></p188:authorLst>"#
                )
                .into_bytes(),
            ),
            ("docProps/core.xml", self.core.clone()),
            ("docProps/custom.xml", self.custom.clone()),
        ];

        let mut zip = ZipWriter::new(Cursor::new(Vec::new()));
        let options = SimpleFileOptions::default().compression_method(CompressionMethod::Stored);
        for (name, body) in parts {
            let optional = name.starts_with("docProps/") || name.contains("modernComment");
            if optional
                && (body.is_empty()
                    || self.modern_comments.is_empty() && name.contains("modernComment"))
            {
                continue;
            }
            zip.start_file(name, options).unwrap();
            zip.write_all(&body).unwrap();
        }
        zip.finish().unwrap().into_inner()
    }

    fn parse(&self) -> PptxDocument {
        parse_pptx_bytes(self.build(), ParseOptions::default()).expect("deck parses")
    }
}

/// Replaces every `@` with the byte 0xE9, which is not valid UTF-8 on its own.
fn with_bad_byte(xml: &str) -> Vec<u8> {
    xml.bytes()
        .map(|byte| if byte == b'@' { 0xE9 } else { byte })
        .collect()
}

/// A shape holding `paragraphs` (the inner XML of its `<p:txBody>`).
fn shape(paragraphs: &str) -> String {
    format!("<p:sp><p:txBody><a:bodyPr/>{paragraphs}</p:txBody></p:sp>")
}

/// A one-row table whose cells hold the given `<a:txBody>` inner XML.
fn table(cells: &[&str]) -> String {
    let cells: String = cells
        .iter()
        .map(|cell| format!("<a:tc><a:txBody><a:bodyPr/>{cell}</a:txBody><a:tcPr/></a:tc>"))
        .collect();
    format!(
        "<p:graphicFrame><a:graphic><a:graphicData><a:tbl><a:tblPr/><a:tr h=\"1\">{cells}</a:tr></a:tbl></a:graphicData></a:graphic></p:graphicFrame>"
    )
}

fn shape_texts(doc: &PptxDocument) -> Vec<Vec<String>> {
    doc.slides[0]
        .shapes
        .iter()
        .map(|shape| shape.paragraphs.iter().map(|p| p.text.clone()).collect())
        .collect()
}

fn note_texts(doc: &PptxDocument) -> Vec<Vec<String>> {
    doc.slides[0]
        .notes
        .iter()
        .map(|shape| shape.paragraphs.iter().map(|p| p.text.clone()).collect())
        .collect()
}

fn cell_texts(doc: &PptxDocument) -> Vec<String> {
    doc.slides[0].tables[0].rows[0]
        .cells
        .iter()
        .map(|cell| cell.text.clone())
        .collect()
}

#[test]
fn one_invalid_byte_in_slide_text_costs_one_character() {
    let body = with_bad_byte(&format!(
        "{}{}",
        shape("<a:p><a:r><a:t>caf@ ok &amp; fine</a:t></a:r></a:p><a:p><a:r><a:t>next</a:t></a:r></a:p>"),
        table(&["<a:p><a:r><a:t>c@ll &lt;1&gt;</a:t></a:r></a:p>"]),
    ));
    let doc = Deck::slide(body).parse();
    assert_eq!(shape_texts(&doc), [["caf\u{fffd} ok & fine", "next"]]);
    assert_eq!(cell_texts(&doc), ["c\u{fffd}ll <1>"]);
}

#[test]
fn one_invalid_byte_in_notes_and_comments_costs_one_character() {
    let deck = Deck {
        notes: with_bad_byte(&shape("<a:p><a:r><a:t>n@te &amp; more</a:t></a:r></a:p>")),
        comments: with_bad_byte(
            r#"<p:cm authorId="0" idx="1"><p:pos x="1" y="1"/><p:text>c@mment &amp; more</p:text></p:cm>"#,
        ),
        ..Deck::default()
    };
    let doc = deck.parse();
    assert_eq!(note_texts(&doc), [["n\u{fffd}te & more"]]);
    assert_eq!(doc.comments.len(), 1);
    assert_eq!(
        doc.comments[0].text.as_deref(),
        Some("c\u{fffd}mment & more")
    );
    assert_eq!(doc.comments[0].author.as_deref(), Some("Ada"));
}

#[test]
fn one_invalid_byte_in_document_properties_costs_one_character() {
    let deck = Deck {
        core: with_bad_byte(
            r#"<cp:coreProperties xmlns:cp="http://schemas.openxmlformats.org/package/2006/metadata/core-properties" xmlns:dc="http://purl.org/dc/elements/1.1/"><dc:title>T@tle &amp; more</dc:title><dc:creator>Ada</dc:creator></cp:coreProperties>"#,
        ),
        custom: with_bad_byte(
            r#"<Properties xmlns="http://schemas.openxmlformats.org/officeDocument/2006/custom-properties" xmlns:vt="http://schemas.openxmlformats.org/officeDocument/2006/docPropsVTypes"><property fmtid="x" pid="2" name="Client"><vt:lpwstr>Acm@ &lt;Ltd&gt;</vt:lpwstr></property></Properties>"#,
        ),
        ..Deck::default()
    };
    let doc = deck.parse();
    assert_eq!(
        doc.metadata.core.get("title").map(String::as_str),
        Some("T\u{fffd}tle & more")
    );
    assert_eq!(
        doc.metadata.core.get("creator").map(String::as_str),
        Some("Ada")
    );
    assert_eq!(
        doc.metadata.custom.get("Client").map(String::as_str),
        Some("Acm\u{fffd} <Ltd>")
    );
}

#[test]
fn an_unknown_entity_is_kept_as_written() {
    let doc = Deck::slide(shape("<a:p><a:r><a:t>a &bogus; b</a:t></a:r></a:p>")).parse();
    assert_eq!(shape_texts(&doc), [["a &bogus; b"]]);
}

#[test]
fn cdata_text_is_kept_literally() {
    let deck = Deck {
        slide: format!(
            "{}{}",
            shape("<a:p><a:r><a:t><![CDATA[caf\u{e9} <raw> &amp;]]></a:t></a:r></a:p>"),
            table(&["<a:p><a:r><a:t><![CDATA[a < b]]></a:t></a:r></a:p>"]),
        )
        .into_bytes(),
        notes: shape("<a:p><a:r><a:t><![CDATA[x & y]]></a:t></a:r></a:p>").into_bytes(),
        comments: br#"<p:cm authorId="0" idx="1"><p:text><![CDATA[1 < 2]]></p:text></p:cm>"#
            .to_vec(),
        modern_comments: Vec::new(),
        core: br#"<cp:coreProperties xmlns:cp="http://schemas.openxmlformats.org/package/2006/metadata/core-properties" xmlns:dc="http://purl.org/dc/elements/1.1/"><dc:title><![CDATA[R&D <plan>]]></dc:title></cp:coreProperties>"#.to_vec(),
        custom: br#"<Properties xmlns="http://schemas.openxmlformats.org/officeDocument/2006/custom-properties" xmlns:vt="http://schemas.openxmlformats.org/officeDocument/2006/docPropsVTypes"><property fmtid="x" pid="2" name="Client"><vt:lpwstr><![CDATA[A&B]]></vt:lpwstr></property></Properties>"#.to_vec(),
    };
    let doc = deck.parse();
    assert_eq!(shape_texts(&doc), [["caf\u{e9} <raw> &amp;"]]);
    assert_eq!(cell_texts(&doc), ["a < b"]);
    assert_eq!(note_texts(&doc), [["x & y"]]);
    assert_eq!(doc.comments[0].text.as_deref(), Some("1 < 2"));
    assert_eq!(
        doc.metadata.core.get("title").map(String::as_str),
        Some("R&D <plan>")
    );
    assert_eq!(
        doc.metadata.custom.get("Client").map(String::as_str),
        Some("A&B")
    );
}

#[test]
fn line_breaks_become_newlines() {
    // `a:br` is a sibling of the runs, in both its self-closed form and the
    // form PowerPoint writes, which carries run properties.
    let paragraph = concat!(
        "<a:p><a:r><a:t>line1</a:t></a:r><a:br/><a:r><a:t>line2</a:t></a:r>",
        r#"<a:br><a:rPr lang="en-US" b="1"/></a:br><a:r><a:t>line3</a:t></a:r></a:p>"#,
    );
    let deck = Deck {
        slide: format!("{}{}", shape(paragraph), table(&[paragraph])).into_bytes(),
        notes: shape(paragraph).into_bytes(),
        ..Deck::default()
    };
    let doc = deck.parse();
    assert_eq!(shape_texts(&doc), [["line1\nline2\nline3"]]);
    assert_eq!(cell_texts(&doc), ["line1\nline2\nline3"]);
    assert_eq!(note_texts(&doc), [["line1\nline2\nline3"]]);
    // The break is not bold just because its own properties say so.
    let runs = &doc.slides[0].shapes[0].paragraphs[0].runs;
    assert!(runs.iter().all(|run| !run.bold));
}

#[test]
fn field_text_is_kept() {
    let paragraph = concat!(
        "<a:p><a:r><a:t>Page </a:t></a:r>",
        r#"<a:fld id="{B6F15528-21DE-4FAA-801E-634DDDAF4B2B}" type="slidenum"><a:rPr lang="en-US" i="1"/><a:pPr/><a:t>7</a:t></a:fld>"#,
        "<a:r><a:t> of 9</a:t></a:r></a:p>",
    );
    let deck = Deck {
        slide: format!("{}{}", shape(paragraph), table(&[paragraph])).into_bytes(),
        notes: shape(paragraph).into_bytes(),
        ..Deck::default()
    };
    let doc = deck.parse();
    assert_eq!(shape_texts(&doc), [["Page 7 of 9"]]);
    assert_eq!(cell_texts(&doc), ["Page 7 of 9"]);
    assert_eq!(note_texts(&doc), [["Page 7 of 9"]]);
    let runs = &doc.slides[0].shapes[0].paragraphs[0].runs;
    assert_eq!(runs.len(), 3);
    assert!(runs[1].italic && !runs[0].italic);
}

#[test]
fn tab_stops_and_symbol_fonts_are_not_text() {
    // In DrawingML `a:tab` only defines a tab stop and `a:sym` only names a
    // font; a tab in the text is a literal tab character inside `a:t`.
    let doc = Deck::slide(shape(concat!(
        r#"<a:p><a:pPr><a:tabLst><a:tab pos="914400" algn="l"/><a:tab pos="1828800" algn="r"/></a:tabLst></a:pPr>"#,
        r#"<a:r><a:rPr lang="en-US"><a:latin typeface="Arial"/><a:sym typeface="Wingdings"/></a:rPr>"#,
        "<a:t>a\tb</a:t></a:r></a:p>",
    )))
    .parse();
    assert_eq!(shape_texts(&doc), [["a\tb"]]);
}

#[test]
fn self_closed_property_elements_are_read() {
    // PowerPoint writes these elements self-closed far more often than not.
    let body = concat!(
        r#"<p:sp><p:nvSpPr><p:cNvPr id="7" name="Title 1"/><p:cNvSpPr/><p:nvPr><p:ph type="title"/></p:nvPr></p:nvSpPr>"#,
        r#"<p:txBody><a:bodyPr/><a:p><a:pPr lvl="2"/>"#,
        r#"<a:r><a:rPr lang="en-US" b="1" i="1" u="sng"/><a:t>styled</a:t></a:r>"#,
        r#"<a:r><a:rPr lang="en-US" b="0" u="none"/><a:t> plain</a:t></a:r></a:p></p:txBody></p:sp>"#,
    );
    let deck = Deck {
        slide: body.as_bytes().to_vec(),
        notes: body.as_bytes().to_vec(),
        ..Deck::default()
    };
    let doc = deck.parse();
    for shapes in [&doc.slides[0].shapes, &doc.slides[0].notes] {
        let shape = &shapes[0];
        assert_eq!(shape.shape_id, Some(7));
        assert_eq!(shape.name.as_deref(), Some("Title 1"));
        let paragraph = &shape.paragraphs[0];
        assert_eq!(paragraph.level, Some(2));
        assert_eq!(paragraph.text, "styled plain");
        let (styled, plain) = (&paragraph.runs[0], &paragraph.runs[1]);
        assert!(styled.bold && styled.italic && styled.underline);
        assert!(!plain.bold && !plain.italic && !plain.underline);
    }
    assert_eq!(
        doc.slides[0].shapes[0].placeholder.as_deref(),
        Some("title")
    );
}

#[test]
fn table_cell_paragraphs_and_empty_cells_keep_their_boundaries() {
    let doc = Deck::slide(
        "<p:graphicFrame><a:graphic><a:graphicData><a:tbl><a:tr h=\"1\">\
         <a:tc><a:txBody><a:p><a:r><a:t>Total</a:t></a:r></a:p><a:p><a:r><a:t>Revenue</a:t></a:r></a:p></a:txBody></a:tc>\
         <a:tc/>\
         <a:tc><a:txBody><a:p><a:r><a:t>third</a:t></a:r></a:p></a:txBody></a:tc>\
         </a:tr></a:tbl></a:graphicData></a:graphic></p:graphicFrame>",
    )
    .parse();
    assert_eq!(cell_texts(&doc), ["Total\nRevenue", "", "third"]);
}

#[test]
fn legacy_comment_text_is_read() {
    // Legacy comments keep their text in `p:text`; a comment written
    // self-closed has no text but is still a comment.
    let deck = Deck {
        comments: br#"<p:cm authorId="0" dt="2024-01-02T03:04:05.000" idx="1"><p:pos x="1" y="1"/><p:text>Review this</p:text></p:cm><p:cm authorId="0" idx="2"/>"#.to_vec(),
        ..Deck::default()
    };
    let doc = deck.parse();
    assert_eq!(doc.comments[0].text.as_deref(), Some("Review this"));
    assert_eq!(doc.comments[0].author.as_deref(), Some("Ada"));
    assert_eq!(doc.comments.len(), 2);
    assert_eq!(
        (doc.comments[1].id, doc.comments[1].text.as_deref()),
        (2, None)
    );
}

#[test]
fn whitespace_only_runs_are_kept() {
    let doc = Deck::slide(shape(
        "<a:p><a:r><a:t>a</a:t></a:r><a:r><a:t> </a:t></a:r><a:r><a:t>b</a:t></a:r></a:p>",
    ))
    .parse();
    assert_eq!(shape_texts(&doc), [["a b"]]);
}

#[test]
fn notes_shapes_carry_their_placeholder_type() {
    // The notes body is the shape with `type="body"`; without the type a
    // consumer cannot tell the notes text from the slide number or the date.
    let deck = Deck {
        notes: concat!(
            r#"<p:sp><p:nvSpPr><p:cNvPr id="2" name="Notes Placeholder 2"/><p:cNvSpPr/><p:nvPr><p:ph type="body" idx="1"/></p:nvPr></p:nvSpPr>"#,
            "<p:txBody><a:bodyPr/><a:p><a:r><a:t>speak slowly</a:t></a:r></a:p></p:txBody></p:sp>",
            r#"<p:sp><p:nvSpPr><p:cNvPr id="3" name="Slide Number Placeholder 3"/><p:cNvSpPr/><p:nvPr><p:ph type="sldNum" sz="quarter" idx="5"></p:ph></p:nvPr></p:nvSpPr>"#,
            "<p:txBody><a:bodyPr/><a:p><a:fld id=\"{X}\" type=\"slidenum\"><a:t>1</a:t></a:fld></a:p></p:txBody></p:sp>",
        )
        .as_bytes()
        .to_vec(),
        ..Deck::default()
    };
    let doc = deck.parse();
    let placeholders: Vec<_> = doc.slides[0]
        .notes
        .iter()
        .map(|shape| shape.placeholder.as_deref())
        .collect();
    assert_eq!(placeholders, [Some("body"), Some("sldNum")]);
}

#[test]
fn an_empty_paragraph_is_a_paragraph_however_it_is_written() {
    // Decision: an empty paragraph is kept. It is a blank line the author
    // put there, and `<a:p/>` and `<a:p></a:p>` are the same XML, so both
    // give one paragraph with no runs. A shape whose only paragraph is empty
    // is therefore emitted too (an empty notes body, for example); a shape
    // with no paragraph at all is not.
    for empty in [
        "<a:p/>",
        "<a:p></a:p>",
        "<a:p><a:endParaRPr lang=\"en-US\"/></a:p>",
    ] {
        let body = format!(
            "{}{}<p:sp><p:txBody><a:bodyPr/></p:txBody></p:sp>",
            shape(&format!(
                "<a:p><a:r><a:t>a</a:t></a:r></a:p>{empty}<a:p><a:r><a:t>b</a:t></a:r></a:p>"
            )),
            shape(empty),
        );
        let deck = Deck {
            slide: body.clone().into_bytes(),
            notes: body.into_bytes(),
            ..Deck::default()
        };
        let doc = deck.parse();
        let expected = vec![
            vec!["a".to_string(), String::new(), "b".to_string()],
            vec![String::new()],
        ];
        assert_eq!(shape_texts(&doc), expected, "slide, written as {empty}");
        assert_eq!(note_texts(&doc), expected, "notes, written as {empty}");
        assert!(doc.slides[0].shapes[1].paragraphs[0].runs.is_empty());
    }
}

#[test]
fn modern_comments_are_read_with_author_date_and_replies() {
    // PowerPoint's threaded comments live in their own part, behind their own
    // relationship type, name authors by GUID and keep text in DrawingML.
    let deck = Deck {
        modern_comments: concat!(
            r#"<p188:cm id="{1111}" authorId="{AAAA}" created="2024-05-06T07:08:09.123">"#,
            r#"<pc:sldMkLst xmlns:pc="x"><pc:docMk/><pc:sldMk cId="1" sldId="256"/></pc:sldMkLst>"#,
            r#"<p188:replyLst><p188:reply id="{2222}" authorId="{BBBB}" created="2024-05-07T00:00:00.000">"#,
            "<p188:txBody><a:bodyPr/><a:p><a:r><a:t>Agreed &lt;mostly&gt;</a:t></a:r></a:p></p188:txBody></p188:reply></p188:replyLst>",
            r#"<p188:txBody><a:bodyPr/><a:lstStyle/><a:p><a:r><a:rPr lang="en-US"/><a:t>R&amp;D </a:t></a:r><a:r><a:t>first</a:t></a:r></a:p>"#,
            "<a:p><a:r><a:t>second line</a:t></a:r></a:p></p188:txBody></p188:cm>",
            r#"<p188:cm id="{3333}" authorId="{ZZZZ}" created="2024-05-08T00:00:00.000"/>"#,
        )
        .as_bytes()
        .to_vec(),
        // A legacy part on the same slide is still read, and comes first.
        comments: br#"<p:cm authorId="0" idx="9"><p:text>old style</p:text></p:cm>"#.to_vec(),
        ..Deck::default()
    };
    let doc = deck.parse();
    let seen: Vec<_> = doc
        .comments
        .iter()
        .map(|c| {
            (
                c.id,
                c.slide_id,
                c.author.as_deref(),
                c.date.as_deref(),
                c.text.as_deref(),
            )
        })
        .collect();
    assert_eq!(
        seen,
        [
            (9, 256, Some("Ada"), None, Some("old style")),
            (
                1,
                256,
                Some("Grace & Co"),
                Some("2024-05-06T07:08:09.123"),
                Some("R&D first\nsecond line")
            ),
            // A reply is a comment of its own, right after its parent.
            (
                2,
                256,
                Some("Linus"),
                Some("2024-05-07T00:00:00.000"),
                Some("Agreed <mostly>")
            ),
            // An unknown author and a comment without a body are still listed.
            (3, 256, None, Some("2024-05-08T00:00:00.000"), None),
        ]
    );
}
