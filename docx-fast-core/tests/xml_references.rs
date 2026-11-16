//! Entity and character references reach the output the same way on every
//! path that collects text or reads an attribute.
//!
//! The XML reader hands references over separately from the text around
//! them, so each path is pinned here: body runs, deleted text, footnotes,
//! endnotes, comments, headers, footers, field instructions and results, the
//! three property parts, and the attributes the reader keeps.

use docx_fast_core::{parse_docx_bytes, Block, DocxDocument, ParseOptions};
use std::io::{Cursor, Write};
use zip::write::SimpleFileOptions;
use zip::{CompressionMethod, ZipWriter};

const W: &str = "http://schemas.openxmlformats.org/wordprocessingml/2006/main";
const REL: &str = "http://schemas.openxmlformats.org/officeDocument/2006/relationships";
const PKG_REL: &str = "http://schemas.openxmlformats.org/package/2006/relationships";

/// What is written into each place of the fixture document.
///
/// `text` is element content and goes to every text path at once; a text
/// path never fails the parse. Each attribute has its own field so that
/// each attribute reader is observed on its own.
#[derive(Clone)]
struct Samples<'a> {
    text: &'a str,
    comment_author: &'a str,
    revision_author: &'a str,
    field_instruction: &'a str,
    style_name: &'a str,
    setting: &'a str,
    property_name: &'a str,
    paragraph_id: &'a str,
}

impl Default for Samples<'_> {
    fn default() -> Self {
        Self {
            text: "x",
            comment_author: "x",
            revision_author: "x",
            field_instruction: "x",
            style_name: "x",
            setting: "x",
            property_name: "x",
            paragraph_id: "x",
        }
    }
}

fn w_part(root: &str, body: &str) -> Vec<u8> {
    format!(
        r#"<?xml version="1.0" encoding="UTF-8"?><w:{root} xmlns:w="{W}" xmlns:r="{REL}" xmlns:w14="http://schemas.microsoft.com/office/word/2010/wordml">{body}</w:{root}>"#
    )
    .into_bytes()
}

fn fixture(samples: &Samples<'_>) -> Vec<u8> {
    let Samples {
        text,
        comment_author,
        revision_author,
        field_instruction,
        style_name,
        setting,
        property_name,
        paragraph_id,
    } = samples.clone();
    let run = format!(r#"<w:r><w:t xml:space="preserve">{text}</w:t></w:r>"#);
    let body = [
        format!(r#"<w:body><w:p w14:paraId="{paragraph_id}">{run}</w:p>"#),
        format!(
            r#"<w:p><w:del w:id="8" w:author="{revision_author}"><w:r><w:delText xml:space="preserve">{text}</w:delText></w:r></w:del></w:p>"#
        ),
        format!(
            r#"<w:p><w:r><w:fldChar w:fldCharType="begin"/></w:r><w:r><w:instrText xml:space="preserve">{text}</w:instrText></w:r><w:r><w:fldChar w:fldCharType="separate"/></w:r>{run}<w:r><w:fldChar w:fldCharType="end"/></w:r></w:p>"#
        ),
        format!(
            r#"<w:p><w:fldSimple w:instr="{field_instruction}"><w:r><w:t>1</w:t></w:r></w:fldSimple></w:p>"#
        ),
        r#"<w:sectPr><w:headerReference w:type="default" r:id="rId8"/><w:footerReference w:type="default" r:id="rId9"/></w:sectPr></w:body>"#.to_string(),
    ]
    .concat();
    let parts: Vec<(&str, Vec<u8>)> = vec![
        (
            "_rels/.rels",
            format!(
                r#"<Relationships xmlns="{PKG_REL}"><Relationship Id="rId1" Type="{REL}/officeDocument" Target="word/document.xml"/></Relationships>"#
            )
            .into_bytes(),
        ),
        ("word/document.xml", w_part("document", &body)),
        (
            "word/_rels/document.xml.rels",
            format!(
                r#"<Relationships xmlns="{PKG_REL}"><Relationship Id="rId1" Type="{REL}/styles" Target="styles.xml"/><Relationship Id="rId3" Type="{REL}/footnotes" Target="footnotes.xml"/><Relationship Id="rId4" Type="{REL}/endnotes" Target="endnotes.xml"/><Relationship Id="rId5" Type="{REL}/comments" Target="comments.xml"/><Relationship Id="rId8" Type="{REL}/header" Target="header1.xml"/><Relationship Id="rId9" Type="{REL}/footer" Target="footer1.xml"/></Relationships>"#
            )
            .into_bytes(),
        ),
        (
            "word/styles.xml",
            w_part(
                "styles",
                &format!(
                    r#"<w:style w:type="paragraph" w:styleId="S1"><w:name w:val="{style_name}"/></w:style>"#
                ),
            ),
        ),
        (
            "word/footnotes.xml",
            w_part(
                "footnotes",
                &format!(r#"<w:footnote w:id="2"><w:p>{run}</w:p></w:footnote>"#),
            ),
        ),
        (
            "word/endnotes.xml",
            w_part(
                "endnotes",
                &format!(r#"<w:endnote w:id="3"><w:p>{run}</w:p></w:endnote>"#),
            ),
        ),
        (
            "word/comments.xml",
            w_part(
                "comments",
                &format!(
                    r#"<w:comment w:id="1" w:author="{comment_author}"><w:p>{run}</w:p></w:comment>"#
                ),
            ),
        ),
        (
            "word/settings.xml",
            w_part("settings", &format!(r#"<w:compat w:val="{setting}"/>"#)),
        ),
        ("word/header1.xml", w_part("hdr", &format!("<w:p>{run}</w:p>"))),
        ("word/footer1.xml", w_part("ftr", &format!("<w:p>{run}</w:p>"))),
        (
            "docProps/core.xml",
            format!(
                r#"<cp:coreProperties xmlns:cp="http://schemas.openxmlformats.org/package/2006/metadata/core-properties" xmlns:dc="http://purl.org/dc/elements/1.1/"><dc:title>{text}</dc:title></cp:coreProperties>"#
            )
            .into_bytes(),
        ),
        (
            "docProps/app.xml",
            format!(
                r#"<Properties xmlns="http://schemas.openxmlformats.org/officeDocument/2006/extended-properties"><Company>{text}</Company></Properties>"#
            )
            .into_bytes(),
        ),
        (
            "docProps/custom.xml",
            format!(
                r#"<Properties xmlns="http://schemas.openxmlformats.org/officeDocument/2006/custom-properties" xmlns:vt="http://schemas.openxmlformats.org/officeDocument/2006/docPropsVTypes"><property fmtid="x" pid="2" name="{property_name}"><vt:lpwstr>{text}</vt:lpwstr></property></Properties>"#
            )
            .into_bytes(),
        ),
    ];
    package(&parts)
}

fn package(parts: &[(&str, Vec<u8>)]) -> Vec<u8> {
    let mut zip = ZipWriter::new(Cursor::new(Vec::new()));
    let options = SimpleFileOptions::default().compression_method(CompressionMethod::Stored);
    for (name, body) in parts {
        zip.start_file(*name, options).expect("start zip entry");
        zip.write_all(body).expect("write zip entry");
    }
    zip.finish().expect("finish zip").into_inner()
}

fn parse(samples: &Samples<'_>) -> Result<DocxDocument, String> {
    parse_docx_bytes(fixture(samples), ParseOptions::default()).map_err(|err| format!("{err:#}"))
}

fn first_paragraph(blocks: &[Block]) -> String {
    match blocks.first() {
        Some(Block::Paragraph(paragraph)) => paragraph.text.clone(),
        other => panic!("expected a paragraph, got {other:?}"),
    }
}

/// The text that reached each run-text path, in a fixed order.
fn run_text_paths(doc: &DocxDocument) -> Vec<(&'static str, String)> {
    let paragraph = |index: usize| match &doc.blocks[index] {
        Block::Paragraph(paragraph) => paragraph.text.clone(),
        other => panic!("expected a paragraph, got {other:?}"),
    };
    vec![
        ("body", paragraph(0)),
        ("deleted text", paragraph(1)),
        ("footnote", first_paragraph(&doc.footnotes[0].blocks)),
        ("endnote", first_paragraph(&doc.endnotes[0].blocks)),
        ("comment", first_paragraph(&doc.comments[0].blocks)),
        ("header", first_paragraph(&doc.headers[0].blocks)),
        ("footer", first_paragraph(&doc.footers[0].blocks)),
        (
            "field instruction",
            doc.fields[0].instr.clone().unwrap_or_default(),
        ),
        ("field result", doc.fields[0].result.clone()),
    ]
}

/// The text that reached each document property, in a fixed order.
fn property_paths(doc: &DocxDocument) -> Vec<(&'static str, String)> {
    let get = |map: &std::collections::HashMap<String, String>, key: &str| {
        map.get(key).cloned().unwrap_or_else(|| "<absent>".into())
    };
    vec![
        ("core property", get(&doc.metadata.core, "title")),
        ("app property", get(&doc.metadata.app, "Company")),
        ("custom property", get(&doc.metadata.custom, "x")),
    ]
}

/// Asserts that `input` as element content reads as `runs` on every run-text
/// path and as `properties` on every property path.
#[track_caller]
fn assert_text(input: &str, runs: &str, properties: &str) {
    let doc = parse(&Samples {
        text: input,
        ..Samples::default()
    })
    .unwrap_or_else(|err| panic!("{input:?} failed the parse: {err}"));
    for (path, observed) in run_text_paths(&doc) {
        eprintln!("text {input:?} | {path} | {observed:?}");
        assert_eq!(observed, runs, "{path} for {input:?}");
    }
    for (path, observed) in property_paths(&doc) {
        eprintln!("text {input:?} | {path} | {observed:?}");
        assert_eq!(observed, properties, "{path} for {input:?}");
    }
}

/// What each attribute path reads for `input`, `Err` when the parse fails.
fn attribute_paths(input: &str) -> Vec<(&'static str, Result<String, String>)> {
    let base = Samples::default();
    let observe = |samples: Samples<'_>, pick: &dyn Fn(&DocxDocument) -> Option<String>| {
        parse(&samples).map(|doc| pick(&doc).unwrap_or_else(|| "<absent>".into()))
    };
    vec![
        (
            "comment author",
            observe(
                Samples {
                    comment_author: input,
                    ..base.clone()
                },
                &|doc| doc.comments[0].author.clone(),
            ),
        ),
        (
            "revision author",
            observe(
                Samples {
                    revision_author: input,
                    ..base.clone()
                },
                &|doc| doc.revisions[0].author.clone(),
            ),
        ),
        (
            "field instruction attribute",
            observe(
                Samples {
                    field_instruction: input,
                    ..base.clone()
                },
                &|doc| doc.fields[1].instr.clone(),
            ),
        ),
        (
            "style name",
            observe(
                Samples {
                    style_name: input,
                    ..base.clone()
                },
                &|doc| doc.styles[0].name.clone(),
            ),
        ),
        (
            "setting",
            observe(
                Samples {
                    setting: input,
                    ..base.clone()
                },
                &|doc| doc.settings.get("compat").cloned(),
            ),
        ),
        (
            "custom property name",
            observe(
                Samples {
                    property_name: input,
                    ..base.clone()
                },
                &|doc| doc.metadata.custom.keys().next().cloned(),
            ),
        ),
        (
            "paragraph id",
            observe(
                Samples {
                    paragraph_id: input,
                    ..base.clone()
                },
                &|doc| match &doc.blocks[0] {
                    Block::Paragraph(paragraph) => paragraph.id.clone(),
                    _ => None,
                },
            ),
        ),
    ]
}

/// Asserts that `input` as an attribute value reads as `expected` on every
/// attribute path; `None` means the document is rejected.
#[track_caller]
fn assert_attribute(input: &str, expected: Option<&str>) {
    for (path, observed) in attribute_paths(input) {
        eprintln!("attribute {input:?} | {path} | {observed:?}");
        match (expected, observed) {
            (Some(expected), Ok(observed)) => assert_eq!(observed, expected, "{path}"),
            (None, Err(_)) => {}
            (expected, observed) => {
                panic!("{path} for {input:?}: expected {expected:?}, got {observed:?}")
            }
        }
    }
}

#[test]
fn predefined_entities_resolve_in_text() {
    assert_text("a &amp; b", "a & b", "a & b");
    assert_text("&lt;c&gt;", "<c>", "<c>");
    assert_text("&quot;q&apos;", "\"q'", "\"q'");
    assert_text("&amp;", "&", "&");
}

#[test]
fn character_references_resolve_in_text() {
    assert_text("&#233;", "é", "é");
    assert_text("caf&#233; &#x1F600;!", "café 😀!", "café 😀!");
    assert_text("a&#9;b&#10;c", "a\tb\nc", "a\tb\nc");
}

#[test]
fn spaces_around_a_reference_are_kept_in_text() {
    assert_text(" &amp; ", " & ", " & ");
    assert_text("a &lt; b  &gt;  c ", "a < b  >  c ", "a < b  >  c ");
}

#[test]
fn an_unknown_entity_is_kept_as_written_in_text() {
    assert_text("x &foo; y", "x &foo; y", "x &foo; y");
    assert_text("&foo;", "&foo;", "&foo;");
}

#[test]
fn an_invalid_character_reference_is_kept_as_written_in_text() {
    assert_text("n&#0;m", "n&#0;m", "n&#0;m");
    assert_text("&#xD800;", "&#xD800;", "&#xD800;");
    assert_text("&#x110000;", "&#x110000;", "&#x110000;");
    assert_text("&#xZZ;", "&#xZZ;", "&#xZZ;");
}

#[test]
fn a_bare_ampersand_is_kept_as_written_in_text() {
    assert_text("AT&T", "AT&T", "AT&T");
    assert_text("fish & chips", "fish & chips", "fish & chips");
    assert_text("end &", "end &", "end &");
}

#[test]
fn a_reference_next_to_cdata_keeps_both_in_text() {
    assert_text(
        "x&amp;<![CDATA[<raw&amp;>]]>&lt;y",
        "x&<raw&amp;><y",
        "x&<raw&amp;><y",
    );
}

#[test]
fn one_bad_reference_does_not_cost_the_good_ones_around_it() {
    assert_text("a &amp; &foo;", "a & &foo;", "a & &foo;");
    assert_text("a &lt; &#0; &gt;", "a < &#0; >", "a < &#0; >");
    assert_text("AT&T &amp; co", "AT&T & co", "AT&T & co");
    assert_text(
        "fish & chips; &amp; peas",
        "fish & chips; & peas",
        "fish & chips; & peas",
    );
}

#[test]
fn references_resolve_in_attributes() {
    assert_attribute("A &amp; B", Some("A & B"));
    assert_attribute("&lt;c&gt;", Some("<c>"));
    assert_attribute("caf&#233; &#x1F600;", Some("café 😀"));
    assert_attribute(" &amp; ", Some(" & "));
    assert_attribute("a&#9;b&#10;c", Some("a\tb\nc"));
}

#[test]
fn an_attribute_with_a_reference_that_cannot_be_resolved_is_kept_as_written() {
    assert_attribute("x &foo; y", Some("x &foo; y"));
    assert_attribute("n&#0;m", Some("n&#0;m"));
    assert_attribute("AT&T", Some("AT&T"));
    // The references around the bad one still resolve, and literal white
    // space is still normalized.
    assert_attribute("a &amp; b &foo; c&#233;\td", Some("a & b &foo; c\u{e9} d"));
}

/// A document whose every other attribute reader meets `bad`: section
/// references, content types, numbering, notes, style fonts and the
/// extended comments part.
fn other_readers(bad: &str) -> Result<DocxDocument, String> {
    let body = format!(
        r#"<w:body><w:p><w:pPr><w:pStyle w:val="{bad}"/><w:numPr><w:ilvl w:val="0"/><w:numId w:val="1"/></w:numPr></w:pPr><w:r><w:rPr><w:rFonts w:ascii="{bad}"/></w:rPr><w:t>body</w:t></w:r><w:r><w:sym w:font="{bad}" w:char="0041"/></w:r></w:p><w:sectPr><w:headerReference w:type="{bad}" r:id="rId8"/></w:sectPr></w:body>"#
    );
    let parts: Vec<(&str, Vec<u8>)> = vec![
        (
            "[Content_Types].xml",
            format!(
                r#"<Types xmlns="http://schemas.openxmlformats.org/package/2006/content-types"><Default Extension="xml" ContentType="application/xml{bad}"/><Override PartName="/word/document.xml" ContentType="main{bad}"/></Types>"#
            )
            .into_bytes(),
        ),
        (
            "_rels/.rels",
            format!(
                r#"<Relationships xmlns="{PKG_REL}"><Relationship Id="rId1" Type="{REL}/officeDocument" Target="word/document.xml"/></Relationships>"#
            )
            .into_bytes(),
        ),
        ("word/document.xml", w_part("document", &body)),
        (
            "word/_rels/document.xml.rels",
            format!(
                r#"<Relationships xmlns="{PKG_REL}"><Relationship Id="rId1" Type="{REL}/styles" Target="styles.xml"/><Relationship Id="rId2" Type="{REL}/numbering" Target="numbering.xml"/><Relationship Id="rId3" Type="{REL}/footnotes" Target="footnotes.xml"/><Relationship Id="rId5" Type="{REL}/comments" Target="comments.xml"/><Relationship Id="rId6" Type="http://schemas.microsoft.com/office/2011/relationships/commentsExtended" Target="commentsExtended.xml"/><Relationship Id="rId8" Type="{REL}/header" Target="header1.xml"/></Relationships>"#
            )
            .into_bytes(),
        ),
        (
            "word/styles.xml",
            w_part(
                "styles",
                &format!(
                    r#"<w:style w:type="paragraph" w:styleId="S1"><w:name w:val="Name"/><w:basedOn w:val="{bad}"/><w:rPr><w:rFonts w:ascii="{bad}"/></w:rPr></w:style>"#
                ),
            ),
        ),
        (
            "word/numbering.xml",
            w_part(
                "numbering",
                &format!(
                    r#"<w:abstractNum w:abstractNumId="0"><w:lvl w:ilvl="0"><w:numFmt w:val="decimal"/><w:lvlText w:val="{bad}"/></w:lvl></w:abstractNum><w:num w:numId="1"><w:abstractNumId w:val="0"/></w:num>"#
                ),
            ),
        ),
        (
            "word/footnotes.xml",
            w_part(
                "footnotes",
                &format!(
                    r#"<w:footnote w:id="1" w:type="{bad}"><w:p/></w:footnote><w:footnote w:id="2"><w:p><w:pPr><w:pStyle w:val="{bad}"/></w:pPr><w:r><w:t>note</w:t></w:r></w:p></w:footnote>"#
                ),
            ),
        ),
        (
            "word/comments.xml",
            w_part(
                "comments",
                r#"<w:comment w:id="1" w:author="A"><w:p w14:paraId="P1"><w:r><w:t>c</w:t></w:r></w:p></w:comment>"#,
            ),
        ),
        (
            "word/commentsExtended.xml",
            format!(
                r#"<w15:commentsEx xmlns:w15="http://schemas.microsoft.com/office/word/2012/wordml"><w15:commentEx w15:paraId="P1" w15:paraIdParent="{bad}" w15:done="0"/></w15:commentsEx>"#
            )
            .into_bytes(),
        ),
        (
            "word/header1.xml",
            w_part("hdr", "<w:p><w:r><w:t>head</w:t></w:r></w:p>"),
        ),
    ];
    parse_docx_bytes(package(&parts), ParseOptions::default()).map_err(|error| error.to_string())
}

#[test]
fn a_bad_attribute_reference_survives_in_every_other_reader() {
    for bad in ["x &foo; y", "n&#0;m", "AT&T"] {
        let doc = other_readers(bad).unwrap_or_else(|error| panic!("{bad:?}: {error}"));
        // Numbering: the level text is the attribute as written.
        assert_eq!(
            doc.numbering.abstract_nums[0].levels[0].text.as_deref(),
            Some(bad),
            "numbering level text"
        );
        // Styles: the base style name.
        assert_eq!(doc.styles[0].based_on.as_deref(), Some(bad), "style base");
        // Notes, the body (paragraph style, run fonts, w:sym), the section
        // reference and the comments are all still read.
        // A note whose type is not `normal` is not a user note, as before.
        assert_eq!(doc.footnotes.len(), 1, "footnotes");
        assert_eq!(doc.comments.len(), 1, "comments");
        assert!(first_paragraph(&doc.blocks).starts_with("body"), "body");
    }
}

// XML attribute-value normalization: a literal tab or line break in an
// attribute value reads as a space; only a character reference (`&#9;`,
// `&#10;`, pinned above) carries the control character itself.
#[test]
fn literal_whitespace_in_an_attribute_reads_as_spaces() {
    assert_attribute("a\tb\nc", Some("a b c"));
    assert_attribute("a\r\nb", Some("a b"));
}

#[test]
fn a_reference_in_symbol_font_text_is_mapped_like_the_text_around_it() {
    let body = concat!(
        r#"<w:body><w:p><w:r><w:rPr><w:rFonts w:ascii="Symbol" w:hAnsi="Symbol"/></w:rPr>"#,
        "<w:t>\u{F061}&#xF062;&#61539; &amp; &lt;</w:t></w:r></w:p></w:body>",
    );
    let bytes = package(&[("word/document.xml", w_part("document", body))]);
    let doc = parse_docx_bytes(bytes, ParseOptions::default()).expect("document parses");
    assert_eq!(first_paragraph(&doc.blocks), "αβχ & <");
}

const CORE_NS: &str = r#"xmlns:cp="http://schemas.openxmlformats.org/package/2006/metadata/core-properties" xmlns:dc="http://purl.org/dc/elements/1.1/""#;
const APP_NS: &str = r#"xmlns="http://schemas.openxmlformats.org/officeDocument/2006/extended-properties" xmlns:vt="http://schemas.openxmlformats.org/officeDocument/2006/docPropsVTypes""#;
const CUSTOM_NS: &str = r#"xmlns="http://schemas.openxmlformats.org/officeDocument/2006/custom-properties" xmlns:vt="http://schemas.openxmlformats.org/officeDocument/2006/docPropsVTypes""#;

/// The three property parts, with `gap` between every two elements: empty
/// as Word writes them, a line break and indentation as other producers do.
fn properties(gap: &str) -> DocxDocument {
    let lines = |parts: &[&str]| parts.join(gap).into_bytes();
    let core = lines(&[
        &format!("<cp:coreProperties {CORE_NS}>"),
        "<dc:title>R&amp;D &lt;plan&gt;</dc:title>",
        "<dc:creator>Ada</dc:creator>",
        "<dc:subject> </dc:subject>",
        "<cp:keywords/>",
        "</cp:coreProperties>",
    ]);
    let app = lines(&[
        &format!("<Properties {APP_NS}>"),
        "<Company>Acme &amp; Co</Company>",
        "<Pages>3</Pages>",
        "<HeadingPairs>",
        "<vt:vector size=\"0\" baseType=\"variant\">",
        "</vt:vector>",
        "</HeadingPairs>",
        "<TitlesOfParts>",
        "<vt:vector size=\"1\" baseType=\"lpstr\">",
        "<vt:lpstr>Title</vt:lpstr>",
        "</vt:vector>",
        "</TitlesOfParts>",
        "</Properties>",
    ]);
    let custom = lines(&[
        &format!("<Properties {CUSTOM_NS}>"),
        r#"<property fmtid="x" pid="2" name="Client">"#,
        "<vt:lpwstr>A &amp; B</vt:lpwstr>",
        "</property>",
        r#"<property fmtid="x" pid="3" name="Reviewed">"#,
        "<vt:bool>true</vt:bool>",
        "</property>",
        "</Properties>",
    ]);
    let bytes = package(&[
        (
            "word/document.xml",
            w_part("document", "<w:body><w:p/></w:body>"),
        ),
        ("docProps/core.xml", core),
        ("docProps/app.xml", app),
        ("docProps/custom.xml", custom),
    ]);
    parse_docx_bytes(bytes, ParseOptions::default()).expect("document parses")
}

fn sorted(map: &std::collections::HashMap<String, String>) -> Vec<(&str, &str)> {
    let mut pairs: Vec<_> = map
        .iter()
        .map(|(key, value)| (key.as_str(), value.as_str()))
        .collect();
    pairs.sort_unstable();
    pairs
}

#[test]
fn whitespace_between_property_elements_is_not_a_value() {
    for gap in ["", "\n  ", "\r\n\t"] {
        let doc = properties(gap);
        // The root element is not a property, and neither is an element
        // that holds only other elements or only a line break. A value that
        // is itself a space is still a value.
        assert_eq!(
            sorted(&doc.metadata.core),
            [
                ("creator", "Ada"),
                ("subject", " "),
                ("title", "R&D <plan>")
            ],
            "core with gap {gap:?}"
        );
        assert_eq!(
            sorted(&doc.metadata.app),
            [
                ("Company", "Acme & Co"),
                ("Pages", "3"),
                ("TitlesOfParts", "Title")
            ],
            "app with gap {gap:?}"
        );
        assert_eq!(
            sorted(&doc.metadata.custom),
            [("Client", "A & B"), ("Reviewed", "true")],
            "custom with gap {gap:?}"
        );
        assert_eq!(doc.page_count, Some(3));
    }
}

// A property whose value is a vector is keyed by the property, not by the
// type of its items, and holds every item, one per line, in document order.
#[test]
fn a_nested_app_property_is_keyed_by_the_property_that_holds_it() {
    let app = [
        &format!("<Properties {APP_NS}>"),
        "<Company>Acme</Company>",
        "<HeadingPairs><vt:vector size=\"4\" baseType=\"variant\">",
        "<vt:variant><vt:lpstr>Title</vt:lpstr></vt:variant>",
        "<vt:variant><vt:i4>1</vt:i4></vt:variant>",
        "<vt:variant><vt:lpstr>Headings</vt:lpstr></vt:variant>",
        "<vt:variant><vt:i4>2</vt:i4></vt:variant>",
        "</vt:vector></HeadingPairs>",
        "<TitlesOfParts><vt:vector size=\"3\" baseType=\"lpstr\">",
        "<vt:lpstr>Report</vt:lpstr><vt:lpstr>Scope</vt:lpstr><vt:lpstr>R&amp;D</vt:lpstr>",
        "</vt:vector></TitlesOfParts>",
        "<AppVersion>16.0000</AppVersion>",
        "</Properties>",
    ]
    .concat()
    .into_bytes();
    let bytes = package(&[
        (
            "word/document.xml",
            w_part("document", "<w:body><w:p/></w:body>"),
        ),
        ("docProps/app.xml", app),
    ]);
    let doc = parse_docx_bytes(bytes, ParseOptions::default()).expect("document parses");
    assert_eq!(
        sorted(&doc.metadata.app),
        [
            ("AppVersion", "16.0000"),
            ("Company", "Acme"),
            ("HeadingPairs", "Title\n1\nHeadings\n2"),
            ("TitlesOfParts", "Report\nScope\nR&D"),
        ]
    );
}

/// The app properties of a document whose `docProps/app.xml` is `app`.
fn app_properties(app: &str) -> std::collections::HashMap<String, String> {
    let bytes = package(&[
        (
            "word/document.xml",
            w_part("document", "<w:body><w:p/></w:body>"),
        ),
        ("docProps/app.xml", app.as_bytes().to_vec()),
    ]);
    parse_docx_bytes(bytes, ParseOptions::default())
        .expect("document parses")
        .metadata
        .app
}

// Every item of a vector is one line, empty or not, so the items can be
// counted and paired with `HeadingPairs` again.
#[test]
fn an_empty_item_of_a_nested_app_property_keeps_its_line() {
    let app = app_properties(&format!(
        "<?xml version=\"1.0\"?><Properties {APP_NS}><Company>ACo</Company><HeadingPairs><vt:vector size=\"4\" baseType=\"variant\"><vt:variant><vt:lpstr>Title</vt:lpstr></vt:variant><vt:variant><vt:i4>1</vt:i4></vt:variant><vt:variant><vt:lpstr>Headings</vt:lpstr></vt:variant><vt:variant><vt:i4>2</vt:i4></vt:variant></vt:vector></HeadingPairs><TitlesOfParts><vt:vector size=\"3\" baseType=\"lpstr\"><vt:lpstr></vt:lpstr><vt:lpstr>Scope</vt:lpstr><vt:lpstr/></vt:vector></TitlesOfParts><lpstr>toplevel</lpstr><Pages>3</Pages></Properties>"
    ));
    assert_eq!(
        sorted(&app),
        [
            ("Company", "ACo"),
            ("HeadingPairs", "Title\n1\nHeadings\n2"),
            ("Pages", "3"),
            ("TitlesOfParts", "\nScope\n"),
            ("lpstr", "toplevel"),
        ]
    );

    // A vector of nothing but empty items is still there, with its count.
    let app = app_properties(&format!(
        "<Properties {APP_NS}><TitlesOfParts><vt:vector size=\"3\" baseType=\"lpstr\">\n  <vt:lpstr></vt:lpstr>\n  <vt:lpstr/>\n  <vt:lpstr>\n  </vt:lpstr>\n</vt:vector></TitlesOfParts><Single><vt:vector size=\"1\" baseType=\"lpstr\"><vt:lpstr/></vt:vector></Single><None><vt:vector size=\"0\" baseType=\"lpstr\"/></None></Properties>"
    ));
    assert_eq!(sorted(&app), [("Single", ""), ("TitlesOfParts", "\n\n")]);
}

// A line break inside an item reads as a space, so a line is always one
// item. Properties that are not nested are read as before.
#[test]
fn a_line_break_inside_a_nested_item_does_not_split_it() {
    let app = app_properties(&format!(
        "<?xml version=\"1.0\"?><Properties {APP_NS}><Company>ACo</Company><TitlesOfParts><vt:vector size=\"2\" baseType=\"lpstr\"><vt:lpstr>Line1\nLine2</vt:lpstr><vt:lpstr>2024-01-02T03:04:05Z</vt:lpstr></vt:vector></TitlesOfParts><Company>Second</Company><Mixed>text<vt:lpstr>inner</vt:lpstr>tail</Mixed><Notes>one\r\ntwo&#10;three</Notes></Properties>"
    ));
    assert_eq!(
        sorted(&app),
        [
            ("Company", "Second"),
            ("Mixed", "text\ninner"),
            ("Notes", "one\r\ntwo\nthree"),
            ("TitlesOfParts", "Line1 Line2\n2024-01-02T03:04:05+00:00"),
        ]
    );

    let app = app_properties(&format!(
        "<Properties {APP_NS}><TitlesOfParts><vt:vector size=\"2\" baseType=\"lpstr\"><vt:lpstr>a\r\nb&#10;c&#13;d</vt:lpstr><vt:lpstr>e</vt:lpstr></vt:vector></TitlesOfParts></Properties>"
    ));
    assert_eq!(sorted(&app), [("TitlesOfParts", "a b c d\ne")]);
}
