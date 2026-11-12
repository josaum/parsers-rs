//! A malformed document must be rejected with an error or parsed as far as
//! it goes: never a panic, a stack overflow, or an allocation out of
//! proportion to the file.

#[path = "../../tools/mutation_smoke.rs"]
mod smoke;

use docx_fast_core::{parse_docx_bytes, Block, DocxDocument, ParseOptions};
use std::io::{Cursor, Write};
use zip::write::FileOptions;
use zip::{CompressionMethod, ZipWriter};

#[global_allocator]
static ALLOCATOR: smoke::CappedAllocator = smoke::CappedAllocator;

const W: &str = "http://schemas.openxmlformats.org/wordprocessingml/2006/main";
const REL: &str = "http://schemas.openxmlformats.org/officeDocument/2006/relationships";
const PKG_REL: &str = "http://schemas.openxmlformats.org/package/2006/relationships";

/// Wraps `body` in a WordprocessingML root element named `root`.
fn w_part(root: &str, body: &[u8]) -> Vec<u8> {
    let mut part = format!(
        r#"<?xml version="1.0" encoding="UTF-8"?><w:{root} xmlns:w="{W}" xmlns:r="{REL}" xmlns:w14="http://schemas.microsoft.com/office/word/2010/wordml">"#
    )
    .into_bytes();
    part.extend_from_slice(body);
    part.extend_from_slice(format!("</w:{root}>").as_bytes());
    part
}

/// `word/document.xml` with `body` as the content of `<w:body>`.
fn document(body: &[u8]) -> Vec<u8> {
    w_part(
        "document",
        &[b"<w:body>".as_slice(), body, b"</w:body>"].concat(),
    )
}

/// A small document that reaches every reader: styled and numbered
/// paragraphs, a nested table, complex and simple fields, tracked changes,
/// comment ranges, note references, symbols, breaks, headers and footers,
/// and all three property parts.
fn seed_parts() -> Vec<(&'static str, Vec<u8>)> {
    let body = concat!(
        r#"<w:p w14:paraId="0A1B2C3D"><w:pPr><w:pStyle w:val="Heading1"/><w:numPr><w:ilvl w:val="0"/><w:numId w:val="1"/></w:numPr></w:pPr>"#,
        r#"<w:r><w:rPr><w:b/><w:i w:val="0"/><w:u w:val="single"/><w:rStyle w:val="Strong"/></w:rPr><w:t xml:space="preserve">Caf&#233; &amp; tea </w:t></w:r>"#,
        r#"<w:commentRangeStart w:id="1"/><w:r><w:t>noted</w:t><w:tab/><w:br/><w:noBreakHyphen/><w:softHyphen/><w:sym w:font="Symbol" w:char="F0B3"/></w:r><w:commentRangeEnd w:id="1"/>"#,
        r#"<w:r><w:commentReference w:id="1"/></w:r>"#,
        r#"<w:r><w:t>claim</w:t><w:footnoteReference w:id="2"/><w:endnoteReference w:id="3"/></w:r></w:p>"#,
        r#"<w:p><w:pPr><w:outlineLvl w:val="2"/></w:pPr><w:ins w:id="7" w:author="Ada" w:date="2024-01-02T03:04:05Z"><w:r><w:t>added</w:t></w:r></w:ins>"#,
        r#"<w:del w:id="8" w:author="Bob"><w:r><w:delText>removed</w:delText></w:r></w:del>"#,
        r#"<w:r><w:fldChar w:fldCharType="begin"/></w:r><w:r><w:instrText xml:space="preserve"> PAGE \* MERGEFORMAT </w:instrText></w:r>"#,
        r#"<w:r><w:fldChar w:fldCharType="separate"/></w:r><w:r><w:t>4</w:t></w:r><w:r><w:fldChar w:fldCharType="end"/></w:r>"#,
        r#"<w:fldSimple w:instr=" DATE "><w:r><w:t>2024-01-02</w:t></w:r></w:fldSimple>"#,
        r#"<w:r><w:rPr><w:rFonts w:ascii="Symbol" w:hAnsi="Symbol"/></w:rPr><w:t><![CDATA[a < b]]></w:t></w:r></w:p>"#,
        r#"<w:tbl><w:tblPr/><w:tr><w:tc><w:p><w:r><w:t>A1</w:t></w:r></w:p>"#,
        r#"<w:tbl><w:tr><w:tc><w:p><w:pPr><w:pStyle w:val="TOC1"/></w:pPr><w:r><w:t>nested</w:t></w:r></w:p></w:tc></w:tr></w:tbl>"#,
        r#"<w:p/></w:tc><w:tc><w:p><w:r><w:t>B1</w:t></w:r></w:p></w:tc></w:tr></w:tbl>"#,
        r#"<w:sectPr><w:headerReference w:type="default" r:id="rId8"/><w:footerReference w:type="first" r:id="rId9"/></w:sectPr>"#,
    );
    let text = |xml: String| xml.into_bytes();
    vec![
        (
            "[Content_Types].xml",
            text(r#"<Types xmlns="http://schemas.openxmlformats.org/package/2006/content-types"><Default Extension="png" ContentType="image/png"/><Default Extension="xml" ContentType="application/xml"/><Override PartName="/word/document.xml" ContentType="application/vnd.openxmlformats-officedocument.wordprocessingml.document.main+xml"/></Types>"#.to_string()),
        ),
        (
            "_rels/.rels",
            text(format!(
                r#"<Relationships xmlns="{PKG_REL}"><Relationship Id="rId1" Type="{REL}/officeDocument" Target="word/document.xml"/></Relationships>"#
            )),
        ),
        ("word/document.xml", document(body.as_bytes())),
        (
            "word/_rels/document.xml.rels",
            text(format!(
                r#"<Relationships xmlns="{PKG_REL}"><Relationship Id="rId1" Type="{REL}/styles" Target="styles.xml"/><Relationship Id="rId2" Type="{REL}/numbering" Target="numbering.xml"/><Relationship Id="rId3" Type="{REL}/footnotes" Target="footnotes.xml"/><Relationship Id="rId4" Type="{REL}/endnotes" Target="endnotes.xml"/><Relationship Id="rId5" Type="{REL}/comments" Target="comments.xml"/><Relationship Id="rId6" Type="http://schemas.microsoft.com/office/2011/relationships/commentsExtended" Target="commentsExtended.xml"/><Relationship Id="rId7" Type="{REL}/image" Target="media\image1.png"/><Relationship Id="rId8" Type="{REL}/header" Target="header1.xml"/><Relationship Id="rId9" Type="{REL}/footer" Target="/word/footer1.xml"/></Relationships>"#
            )),
        ),
        (
            "word/styles.xml",
            w_part(
                "styles",
                br#"<w:style w:type="paragraph" w:styleId="Heading1" w:default="0"><w:name w:val="heading 1"/><w:basedOn w:val="Normal"/><w:next w:val="Normal"/><w:uiPriority w:val="9"/><w:qFormat/><w:pPr><w:outlineLvl w:val="0"/></w:pPr></w:style><w:style w:type="paragraph" w:styleId="TOC1"><w:name w:val="TOC1"/></w:style>"#,
            ),
        ),
        (
            "word/numbering.xml",
            w_part(
                "numbering",
                br#"<w:abstractNum w:abstractNumId="0"><w:lvl w:ilvl="0"><w:start w:val="1"/><w:numFmt w:val="decimal"/><w:lvlText w:val="%1."/></w:lvl></w:abstractNum><w:num w:numId="1"><w:abstractNumId w:val="0"/></w:num>"#,
            ),
        ),
        (
            "word/footnotes.xml",
            w_part(
                "footnotes",
                br#"<w:footnote w:id="-1" w:type="separator"><w:p><w:r><w:separator/></w:r></w:p></w:footnote><w:footnote w:id="2"><w:p><w:r><w:t>User note &amp; more</w:t></w:r></w:p></w:footnote>"#,
            ),
        ),
        (
            "word/endnotes.xml",
            w_part(
                "endnotes",
                br#"<w:endnote w:id="3"><w:p><w:r><w:t>End note</w:t></w:r></w:p><w:tbl><w:tr><w:tc><w:p/></w:tc></w:tr></w:tbl></w:endnote>"#,
            ),
        ),
        (
            "word/comments.xml",
            w_part(
                "comments",
                br#"<w:comment w:id="1" w:author="Ada &amp; Co" w:initials="A" w:date="2024-01-02T03:04:05Z"><w:p w14:paraId="11111111"><w:r><w:t>Check this</w:t></w:r></w:p></w:comment><w:comment w:id="2" w:author="Bob"><w:p w14:paraId="22222222"><w:r><w:t>Agreed</w:t></w:r></w:p></w:comment>"#,
            ),
        ),
        (
            "word/commentsExtended.xml",
            text(r#"<w15:commentsEx xmlns:w15="http://schemas.microsoft.com/office/word/2012/wordml"><w15:comment w15:id="2" w15:parentId="1" w15:done="0"/></w15:commentsEx>"#.to_string()),
        ),
        (
            "word/settings.xml",
            w_part(
                "settings",
                br#"<w:zoom w:percent="100"/><w:trackRevisions/><w:defaultTabStop w:val="720"/>"#,
            ),
        ),
        (
            "word/header1.xml",
            w_part(
                "hdr",
                br#"<w:p><w:r><w:t>Header </w:t></w:r><w:fldSimple w:instr="PAGE"><w:r><w:t>1</w:t></w:r></w:fldSimple></w:p>"#,
            ),
        ),
        (
            "word/footer1.xml",
            w_part("ftr", br#"<w:p><w:r><w:t>Footer</w:t></w:r></w:p>"#),
        ),
        (
            "docProps/core.xml",
            text(r#"<cp:coreProperties xmlns:cp="http://schemas.openxmlformats.org/package/2006/metadata/core-properties" xmlns:dc="http://purl.org/dc/elements/1.1/" xmlns:dcterms="http://purl.org/dc/terms/"><dc:title>Report &amp; notes</dc:title><dcterms:created>2024-01-02T03:04:05Z</dcterms:created></cp:coreProperties>"#.to_string()),
        ),
        (
            "docProps/app.xml",
            text(r#"<Properties xmlns="http://schemas.openxmlformats.org/officeDocument/2006/extended-properties"><Application>Smoke</Application><Pages>3</Pages></Properties>"#.to_string()),
        ),
        (
            "docProps/custom.xml",
            text(r#"<Properties xmlns="http://schemas.openxmlformats.org/officeDocument/2006/custom-properties" xmlns:vt="http://schemas.openxmlformats.org/officeDocument/2006/docPropsVTypes"><property fmtid="{D5CDD505}" pid="2" name="Client"><vt:lpwstr>Acme</vt:lpwstr></property></Properties>"#.to_string()),
        ),
        ("word/media/image1.png", b"\x89PNG\r\n\x1a\n".to_vec()),
    ]
}

/// Zips `parts`, with `replacement` standing in for the part it names.
fn package(
    parts: &[(&'static str, Vec<u8>)],
    replacement: Option<(&str, &[u8])>,
    method: CompressionMethod,
) -> Vec<u8> {
    let mut zip = ZipWriter::new(Cursor::new(Vec::new()));
    let options = FileOptions::default().compression_method(method);
    for (name, body) in parts {
        let body = match replacement {
            Some((replaced, bytes)) if replaced == *name => bytes,
            _ => body.as_slice(),
        };
        zip.start_file(*name, options).expect("start zip entry");
        zip.write_all(body).expect("write zip entry");
    }
    zip.finish().expect("finish zip").into_inner()
}

/// The seed document with one part replaced.
fn docx_with(name: &str, body: &[u8]) -> Vec<u8> {
    package(&seed_parts(), Some((name, body)), CompressionMethod::Stored)
}

/// Serializes and shreds a parsed document the way the bindings do.
fn consume(doc: &DocxDocument) {
    let _ = serde_json::to_string(doc);
    #[cfg(feature = "arrow")]
    for table in docx_fast_core::arrow::TABLES {
        use docx_fast_core::arrow::{docx_document_to_arrow_table_batches, ArrowOptions};
        let _ = docx_document_to_arrow_table_batches(
            doc,
            ArrowOptions { batch_size: 3 },
            table,
            |_| Ok(()),
        );
    }
}

fn parse_bytes(bytes: &[u8]) {
    if let Ok(doc) = parse_docx_bytes(bytes.to_vec(), ParseOptions::default()) {
        consume(&doc);
    }
}

/// `depth` tables, each inside the single cell of the one before.
fn nested_tables(depth: usize) -> Vec<u8> {
    document(&smoke::nested(
        b"<w:tbl><w:tr><w:tc>",
        b"<w:p><w:r><w:t>deep</w:t></w:r></w:p>",
        b"</w:tc></w:tr></w:tbl>",
        depth,
    ))
}

/// Number of tables on the path from the document to its innermost block.
fn table_depth(blocks: &[Block]) -> usize {
    let mut depth = 0;
    let mut blocks = blocks;
    while let Some(Block::Table(table)) = blocks.first() {
        depth += 1;
        blocks = &table.rows[0].cells[0].blocks;
    }
    depth
}

#[test]
fn the_seed_document_parses() {
    let bytes = package(&seed_parts(), None, CompressionMethod::Deflated);
    let doc = parse_docx_bytes(bytes, ParseOptions::default()).expect("seed parses");
    assert_eq!(doc.blocks.len(), 3);
    assert_eq!(doc.footnotes.len(), 1);
    assert_eq!(doc.endnotes.len(), 1);
    assert_eq!(doc.comments.len(), 2);
    assert_eq!(doc.comments[1].parent_id, Some(1));
    assert_eq!(doc.revisions.len(), 2);
    assert_eq!(doc.fields.len(), 3);
    assert_eq!((doc.headers.len(), doc.footers.len()), (1, 1));
    assert_eq!(doc.images.len(), 1);
    assert_eq!(doc.page_count, Some(3));
    assert_eq!(table_depth(&doc.blocks[2..]), 1);
}

#[test]
fn mutated_parts_never_panic() {
    let parts = seed_parts();
    for (name, body) in &parts {
        smoke::assert_no_panics(
            &format!("docx-fast-core part {name}"),
            std::slice::from_ref(body),
            500,
            |mutated| {
                parse_bytes(&package(
                    &parts,
                    Some((name, mutated)),
                    CompressionMethod::Stored,
                ));
            },
        );
    }
}

#[test]
fn mutated_containers_never_panic() {
    let parts = seed_parts();
    let seeds = [
        package(&parts, None, CompressionMethod::Stored),
        package(&parts, None, CompressionMethod::Deflated),
    ];
    smoke::assert_no_panics("docx-fast-core container", &seeds, 8_000, parse_bytes);
}

/// Nested tables are the one place the output is a tree, so everything that
/// walks a parsed document recurses once per level: dropping it, collecting
/// the table of contents, serializing it, shredding it into Arrow tables.
/// The reader must refuse a depth those walks cannot survive.
#[test]
fn hostile_table_nesting_is_refused_on_a_small_stack() {
    for part in [
        "word/document.xml",
        "word/header1.xml",
        "word/footnotes.xml",
        "word/comments.xml",
    ] {
        let tables = smoke::nested(
            b"<w:tbl><w:tr><w:tc>",
            b"<w:p><w:r><w:t>deep</w:t></w:r></w:p>",
            b"</w:tc></w:tr></w:tbl>",
            100_000,
        );
        let body = match part {
            "word/document.xml" => document(&tables),
            "word/header1.xml" => w_part("hdr", &tables),
            "word/footnotes.xml" => w_part(
                "footnotes",
                &[
                    b"<w:footnote w:id=\"2\">".as_slice(),
                    &tables,
                    b"</w:footnote>",
                ]
                .concat(),
            ),
            _ => w_part(
                "comments",
                &[
                    b"<w:comment w:id=\"1\">".as_slice(),
                    &tables,
                    b"</w:comment>",
                ]
                .concat(),
            ),
        };
        let bytes = docx_with(part, &body);
        smoke::run_on_small_stack(move || {
            let error = parse_docx_bytes(bytes, ParseOptions::default())
                .expect_err("100,000 nested tables must be refused");
            assert!(
                format!("{error:#}").contains("nested"),
                "unexpected error: {error:#}"
            );
        });
    }
}

/// Deepest table nesting the reader accepts; one more is an error.
///
/// Mirrors `MAX_TABLE_DEPTH` in the library, which is private. The point of
/// this test is that a document at exactly the limit can still be dropped,
/// serialized and shredded on a small stack.
const DEEPEST_ACCEPTED_TABLE_NESTING: usize = 32;

#[test]
fn the_deepest_accepted_table_nesting_is_usable_on_a_small_stack() {
    let bytes = docx_with(
        "word/document.xml",
        &nested_tables(DEEPEST_ACCEPTED_TABLE_NESTING),
    );
    smoke::run_on_small_stack(move || {
        let doc =
            parse_docx_bytes(bytes, ParseOptions::default()).expect("nesting at the limit parses");
        assert_eq!(table_depth(&doc.blocks), DEEPEST_ACCEPTED_TABLE_NESTING);
        let json = serde_json::to_string(&doc).expect("document serializes");
        assert!(json.contains("deep"));
        consume(&doc);
        drop(doc);
    });

    let bytes = docx_with(
        "word/document.xml",
        &nested_tables(DEEPEST_ACCEPTED_TABLE_NESTING + 1),
    );
    assert!(parse_docx_bytes(bytes, ParseOptions::default()).is_err());
}

#[test]
fn other_deep_nesting_is_bounded_on_a_small_stack() {
    const DEPTH: usize = 100_000;
    let bodies: Vec<Vec<u8>> = vec![
        smoke::nested(b"<w:p><w:r>", b"<w:t>x</w:t>", b"</w:r></w:p>", DEPTH),
        smoke::nested(b"<w:tc>", b"<w:p/>", b"</w:tc>", DEPTH),
        smoke::nested(b"<w:tr><w:tc>", b"<w:p/>", b"</w:tc></w:tr>", DEPTH),
        smoke::nested(
            b"<w:ins w:id=\"1\"><w:del w:id=\"2\">",
            b"<w:r><w:t>x</w:t></w:r>",
            b"</w:del></w:ins>",
            DEPTH,
        ),
        smoke::nested(
            b"<w:fldSimple w:instr=\"PAGE\">",
            b"<w:r><w:t>1</w:t></w:r>",
            b"</w:fldSimple>",
            DEPTH,
        ),
        smoke::nested(
            b"<w:r><w:fldChar w:fldCharType=\"begin\"/></w:r><w:sdt><w:sdtContent>",
            b"<w:p><w:r><w:t>x</w:t></w:r></w:p>",
            b"</w:sdtContent></w:sdt>",
            DEPTH,
        ),
    ];
    for body in bodies {
        let bytes = docx_with("word/document.xml", &document(&body));
        smoke::run_on_small_stack(move || parse_bytes(&bytes));
    }
    for (part, body) in [
        (
            "word/styles.xml",
            w_part(
                "styles",
                &smoke::nested(b"<w:style w:styleId=\"a\">", b"", b"</w:style>", DEPTH),
            ),
        ),
        (
            "word/numbering.xml",
            w_part(
                "numbering",
                &smoke::nested(
                    b"<w:abstractNum w:abstractNumId=\"0\"><w:lvl w:ilvl=\"0\">",
                    b"",
                    b"</w:lvl></w:abstractNum>",
                    DEPTH,
                ),
            ),
        ),
        (
            "docProps/core.xml",
            smoke::nested(b"<dc:title>", b"x", b"</dc:title>", DEPTH),
        ),
    ] {
        let bytes = docx_with(part, &body);
        smoke::run_on_small_stack(move || parse_bytes(&bytes));
    }
}

/// quick-xml 0.36 checks each attribute name against all earlier ones
/// (RUSTSEC-2026-0194), so this is quadratic until the dependency moves. The
/// time is printed, not asserted: the test only pins that it finishes.
#[test]
fn a_start_tag_with_twenty_thousand_attributes_is_parsed() {
    let mut body = b"<w:p".to_vec();
    for index in 0..20_000 {
        body.extend_from_slice(format!(" a{index}=\"v\"").as_bytes());
    }
    body.extend_from_slice(br#" w14:paraId="0A1B2C3D"><w:r><w:t>kept</w:t></w:r></w:p>"#);
    let bytes = docx_with("word/document.xml", &document(&body));

    let started = std::time::Instant::now();
    let doc = parse_docx_bytes(bytes, ParseOptions::default()).expect("document parses");
    eprintln!(
        "docx-fast-core: start tag with 20,000 attributes parsed in {:?}",
        started.elapsed()
    );
    let Block::Paragraph(paragraph) = &doc.blocks[0] else {
        panic!("expected a paragraph");
    };
    assert_eq!(paragraph.id.as_deref(), Some("0A1B2C3D"));
    assert_eq!(paragraph.text, "kept");
}
