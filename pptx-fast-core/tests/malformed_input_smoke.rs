//! A malformed presentation must be rejected with an error or parsed as far
//! as it goes: never a panic, a stack overflow, or an allocation out of
//! proportion to the file.

#[path = "../../tools/mutation_smoke.rs"]
mod smoke;

use pptx_fast_core::{parse_pptx_bytes, ParseOptions};
use std::io::{Cursor, Write};
use zip::write::SimpleFileOptions;
use zip::{CompressionMethod, ZipWriter};

#[global_allocator]
static ALLOCATOR: smoke::CappedAllocator = smoke::CappedAllocator;

const P: &str = "http://schemas.openxmlformats.org/presentationml/2006/main";
const A: &str = "http://schemas.openxmlformats.org/drawingml/2006/main";
const REL: &str = "http://schemas.openxmlformats.org/officeDocument/2006/relationships";
const MODERN: &str = "http://schemas.microsoft.com/office/2018/10/relationships";
const P188: &str = "http://schemas.microsoft.com/office/powerpoint/2018/8/main";
const PKG_REL: &str = "http://schemas.openxmlformats.org/package/2006/relationships";

/// Wraps `body` as the shape tree of a slide-like part rooted at `root`.
fn sp_tree(root: &str, body: &[u8]) -> Vec<u8> {
    let mut part = format!(
        r#"<p:{root} xmlns:p="{P}" xmlns:a="{A}" xmlns:r="{REL}"><p:cSld name="Intro"><p:spTree>"#
    )
    .into_bytes();
    part.extend_from_slice(body);
    part.extend_from_slice(format!("</p:spTree></p:cSld></p:{root}>").as_bytes());
    part
}

/// A small deck that reaches every reader: a master, a layout, a slide with a
/// grouped shape, a field, a line break and a table, notes, legacy and modern
/// comments with their authors, an image relationship, and all three property parts.
fn seed_parts() -> Vec<(&'static str, Vec<u8>)> {
    let slide_body = concat!(
        r#"<p:nvGrpSpPr><p:cNvPr id="1" name=""/><p:cNvGrpSpPr/><p:nvPr/></p:nvGrpSpPr>"#,
        r#"<p:sp><p:nvSpPr><p:cNvPr id="2" name="Title 1"/><p:cNvSpPr/><p:nvPr><p:ph type="title"/></p:nvPr></p:nvSpPr>"#,
        r#"<p:txBody><a:bodyPr/><a:p><a:pPr lvl="1"><a:tabLst><a:tab pos="914400" algn="l"/></a:tabLst></a:pPr>"#,
        r#"<a:r><a:rPr lang="en-US" b="1" i="true" u="sng"><a:sym typeface="Wingdings"/></a:rPr><a:t>Caf&#233; &amp; tea</a:t></a:r>"#,
        r#"<a:br><a:rPr lang="en-US"/></a:br><a:fld id="{B6F1}" type="slidenum"><a:rPr/><a:t>7</a:t></a:fld>"#,
        r#"<a:r><a:t><![CDATA[a < b]]></a:t></a:r><a:endParaRPr lang="en-US"/></a:p></p:txBody></p:sp>"#,
        r#"<p:grpSp><p:nvGrpSpPr><p:cNvPr id="5" name="Group"/></p:nvGrpSpPr><p:grpSp>"#,
        r#"<p:sp><p:nvSpPr><p:cNvPr id="6" name="Inner"><a:hlinkClick r:id="rId9"/></p:cNvPr></p:nvSpPr>"#,
        r#"<p:txBody><a:p><a:r><a:t>grouped</a:t></a:r></a:p><a:p/></p:txBody></p:sp></p:grpSp></p:grpSp>"#,
        r#"<p:graphicFrame><a:graphic><a:graphicData><a:tbl><a:tblPr/><a:tblGrid><a:gridCol w="1"/></a:tblGrid>"#,
        r#"<a:tr h="1"><a:tc><a:txBody><a:bodyPr/><a:p><a:r><a:t>Cell A</a:t></a:r></a:p><a:p><a:r><a:t>second</a:t></a:r></a:p></a:txBody><a:tcPr/></a:tc>"#,
        r#"<a:tc/><a:tc gridSpan="2"><a:txBody><a:p><a:r><a:t>Cell B</a:t></a:r><a:br/></a:p></a:txBody></a:tc></a:tr>"#,
        r#"</a:tbl></a:graphicData></a:graphic></p:graphicFrame>"#,
        r#"<p:pic><p:nvPicPr><p:cNvPr id="9" name="Picture"/></p:nvPicPr><p:blipFill><a:blip r:embed="rId3"/></p:blipFill></p:pic>"#,
    );
    let notes_body = concat!(
        r#"<p:sp><p:nvSpPr><p:cNvPr id="3" name="Notes Placeholder"/><p:nvPr><p:ph type="body" idx="1"/></p:nvPr></p:nvSpPr>"#,
        r#"<p:txBody><a:p><a:pPr lvl="0"/><a:r><a:rPr b="1"/><a:t>Speaker note</a:t></a:r><a:br/>"#,
        r#"<a:fld type="slidenum"><a:t>1</a:t></a:fld></a:p></p:txBody></p:sp>"#,
    );
    let master_body = r#"<p:sp><p:nvSpPr><p:cNvPr id="2" name="Title Placeholder"/></p:nvSpPr><p:txBody><a:p><a:r><a:t>Click to edit</a:t></a:r></a:p></p:txBody></p:sp>"#;
    let text = |xml: String| xml.into_bytes();
    vec![
        (
            "[Content_Types].xml",
            text(r#"<Types xmlns="http://schemas.openxmlformats.org/package/2006/content-types"><Default Extension="png" ContentType="image/png"/><Default Extension="xml" ContentType="application/xml"/><Override PartName="/ppt/slides/slide1.xml" ContentType="application/vnd.openxmlformats-officedocument.presentationml.slide+xml"/></Types>"#.to_string()),
        ),
        (
            "_rels/.rels",
            text(format!(
                r#"<Relationships xmlns="{PKG_REL}"><Relationship Id="rId1" Type="{REL}/officeDocument" Target="ppt/presentation.xml"/></Relationships>"#
            )),
        ),
        (
            "ppt/presentation.xml",
            text(format!(
                r#"<p:presentation xmlns:p="{P}" xmlns:r="{REL}"><p:sldMasterIdLst><p:sldMasterId id="2147483648" r:id="rId2"/></p:sldMasterIdLst><p:sldIdLst><p:sldId id="256" r:id="rId1"/></p:sldIdLst><p:sldSz cx="9144000" cy="6858000"/></p:presentation>"#
            )),
        ),
        (
            "ppt/_rels/presentation.xml.rels",
            text(format!(
                r#"<Relationships xmlns="{PKG_REL}"><Relationship Id="rId1" Type="{REL}/slide" Target="slides/slide1.xml"/><Relationship Id="rId2" Type="{REL}/slideMaster" Target="slideMasters/slideMaster1.xml"/><Relationship Id="rId3" Type="{MODERN}/authors" Target="authors.xml"/></Relationships>"#
            )),
        ),
        ("ppt/slides/slide1.xml", sp_tree("sld", slide_body.as_bytes())),
        (
            "ppt/slides/_rels/slide1.xml.rels",
            text(format!(
                r#"<Relationships xmlns="{PKG_REL}"><Relationship Id="rId1" Type="{REL}/notesSlide" Target="../notesSlides/notesSlide1.xml"/><Relationship Id="rId2" Type="{REL}/comments" Target="../comments/comment1.xml"/><Relationship Id="rId3" Type="{REL}/image" Target="../media/image1.png"/><Relationship Id="rId4" Type="{MODERN}/comments" Target="../comments/modernComment_100_0.xml"/></Relationships>"#
            )),
        ),
        (
            "ppt/notesSlides/notesSlide1.xml",
            sp_tree("notes", notes_body.as_bytes()),
        ),
        (
            "ppt/comments/comment1.xml",
            text(format!(
                r#"<p:cmLst xmlns:p="{P}"><p:cm authorId="0" dt="2024-01-02T03:04:05.000" idx="1"><p:pos x="10" y="10"/><p:text>Review &amp; fix</p:text></p:cm><p:cm authorId="7" idx="2"><p:text/></p:cm></p:cmLst>"#
            )),
        ),
        (
            "ppt/commentAuthors.xml",
            text(format!(
                r#"<p:cmAuthorLst xmlns:p="{P}"><p:cmAuthor id="0" name="Ada &amp; Co" initials="A" lastIdx="2" clrIdx="0"/></p:cmAuthorLst>"#
            )),
        ),
        (
            "ppt/comments/modernComment_100_0.xml",
            text(format!(
                r#"<p188:cmLst xmlns:p188="{P188}" xmlns:a="{A}"><p188:cm id="{{1111}}" authorId="{{AAAA}}" created="2024-05-06T07:08:09.123"><p188:replyLst><p188:reply id="{{2222}}" authorId="{{aaaa}}" created="2024-05-07T00:00:00.000"><p188:txBody><a:bodyPr/><a:p><a:r><a:t>Agreed</a:t></a:r></a:p></p188:txBody></p188:reply></p188:replyLst><p188:txBody><a:bodyPr/><a:lstStyle/><a:p><a:r><a:rPr lang="en-US"/><a:t>Thread &amp; reply</a:t></a:r><a:br/></a:p><a:p/></p188:txBody><p188:extLst><p188:ext uri="x"><a:t>ignored</a:t></p188:ext></p188:extLst></p188:cm></p188:cmLst>"#
            )),
        ),
        (
            "ppt/authors.xml",
            text(format!(
                r#"<p188:authorLst xmlns:p188="{P188}"><p188:author id="{{AAAA}}" name="Grace &amp; Co" initials="G" userId="u1" providerId="AD"/></p188:authorLst>"#
            )),
        ),
        (
            "ppt/slideMasters/slideMaster1.xml",
            sp_tree("sldMaster", master_body.as_bytes()),
        ),
        (
            "docProps/core.xml",
            text(r#"<cp:coreProperties xmlns:cp="http://schemas.openxmlformats.org/package/2006/metadata/core-properties" xmlns:dc="http://purl.org/dc/elements/1.1/" xmlns:dcterms="http://purl.org/dc/terms/"><dc:title>Deck &amp; notes</dc:title><dcterms:created>2024-01-02T03:04:05Z</dcterms:created></cp:coreProperties>"#.to_string()),
        ),
        (
            "docProps/app.xml",
            text(r#"<Properties xmlns="http://schemas.openxmlformats.org/officeDocument/2006/extended-properties"><Application>Smoke</Application><Slides>1</Slides></Properties>"#.to_string()),
        ),
        (
            "docProps/custom.xml",
            text(r#"<Properties xmlns="http://schemas.openxmlformats.org/officeDocument/2006/custom-properties" xmlns:vt="http://schemas.openxmlformats.org/officeDocument/2006/docPropsVTypes"><property fmtid="{D5CDD505}" pid="2" name="Client"><vt:lpwstr>Acme</vt:lpwstr></property></Properties>"#.to_string()),
        ),
        ("ppt/media/image1.png", b"\x89PNG\r\n\x1a\n".to_vec()),
    ]
}

/// Zips `parts`, with `replacement` standing in for the part it names.
fn package(
    parts: &[(&'static str, Vec<u8>)],
    replacement: Option<(&str, &[u8])>,
    method: CompressionMethod,
) -> Vec<u8> {
    let mut zip = ZipWriter::new(Cursor::new(Vec::new()));
    let options = SimpleFileOptions::default().compression_method(method);
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

/// The seed deck with one part replaced.
fn deck_with(name: &str, body: &[u8]) -> Vec<u8> {
    package(&seed_parts(), Some((name, body)), CompressionMethod::Stored)
}

/// Parses `bytes`, then serializes and shreds what parsed, since the bindings
/// hand the result straight to a serializer or the Arrow exporter.
fn parse_bytes(bytes: &[u8]) {
    let Ok(doc) = parse_pptx_bytes(bytes.to_vec(), ParseOptions::default()) else {
        return;
    };
    let _ = serde_json::to_string(&doc);
    #[cfg(feature = "arrow")]
    for table in pptx_fast_core::arrow::TABLES {
        use pptx_fast_core::arrow::{pptx_document_to_arrow_table_batches, ArrowOptions};
        let _ = pptx_document_to_arrow_table_batches(
            &doc,
            ArrowOptions { batch_size: 3 },
            table,
            |_| Ok(()),
        );
    }
}

#[test]
fn the_seed_deck_parses() {
    let bytes = package(&seed_parts(), None, CompressionMethod::Deflated);
    let doc = parse_pptx_bytes(bytes, ParseOptions::default()).expect("seed parses");
    assert_eq!(doc.slides.len(), 1);
    let slide = &doc.slides[0];
    assert_eq!(slide.shapes.len(), 2);
    assert_eq!(
        slide.shapes[0].paragraphs[0].text,
        "Caf\u{e9} & tea\n7a < b"
    );
    assert_eq!(slide.tables[0].rows[0].cells.len(), 3);
    assert_eq!(slide.notes[0].paragraphs[0].text, "Speaker note\n1");
    assert_eq!(doc.comments.len(), 4);
    assert_eq!(doc.comments[0].text.as_deref(), Some("Review & fix"));
    // The modern comment and its reply, after the two legacy ones.
    assert_eq!(doc.comments[2].text.as_deref(), Some("Thread & reply\n\n"));
    assert_eq!(doc.comments[2].author.as_deref(), Some("Grace & Co"));
    assert_eq!(doc.comments[3].text.as_deref(), Some("Agreed"));
    assert_eq!(doc.comments[3].author.as_deref(), Some("Grace & Co"));
    assert_eq!(doc.masters.len(), 1);
    assert_eq!(doc.images.len(), 1);
}

#[test]
fn mutated_parts_never_panic() {
    let parts = seed_parts();
    for (name, body) in &parts {
        smoke::assert_no_panics(
            &format!("pptx-fast-core part {name}"),
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
    smoke::assert_no_panics("pptx-fast-core container", &seeds, 8_000, parse_bytes);
}

#[test]
fn deeply_nested_elements_are_bounded_on_a_small_stack() {
    const DEPTH: usize = 100_000;
    let shape = br#"<p:sp><p:txBody><a:p><a:r><a:t>deep</a:t></a:r></a:p></p:txBody></p:sp>"#;
    let slide_cases: Vec<Vec<u8>> = vec![
        smoke::nested(b"<p:grpSp>", shape, b"</p:grpSp>", DEPTH),
        smoke::nested(b"<p:sp><p:txBody>", b"<a:p/>", b"</p:txBody></p:sp>", DEPTH),
        smoke::nested(
            b"<a:tbl><a:tr><a:tc>",
            b"<a:p><a:r><a:t>x</a:t></a:r></a:p>",
            b"</a:tc></a:tr></a:tbl>",
            DEPTH,
        ),
        smoke::nested(b"<a:p><a:r>", b"<a:t>x</a:t>", b"</a:r></a:p>", DEPTH),
        smoke::nested(b"<a:fld><a:br>", b"<a:t>x</a:t>", b"</a:br></a:fld>", DEPTH),
    ];
    for (index, body) in slide_cases.iter().enumerate() {
        // Notes and masters share the slide reader, so two shapes of nesting
        // are enough to cover their entry points.
        let parts: &[(&str, &str)] = if index < 2 {
            &[
                ("ppt/slides/slide1.xml", "sld"),
                ("ppt/notesSlides/notesSlide1.xml", "notes"),
                ("ppt/slideMasters/slideMaster1.xml", "sldMaster"),
            ]
        } else {
            &[("ppt/slides/slide1.xml", "sld")]
        };
        for (part, root) in parts {
            let bytes = deck_with(part, &sp_tree(root, body));
            smoke::run_on_small_stack(move || parse_bytes(&bytes));
        }
    }

    let other_cases: Vec<(&str, Vec<u8>)> = vec![
        (
            "ppt/comments/comment1.xml",
            smoke::nested(
                b"<p:cm idx=\"1\"><p:text>",
                b"x",
                b"</p:text></p:cm>",
                DEPTH,
            ),
        ),
        (
            "docProps/core.xml",
            smoke::nested(b"<dc:title>", b"x", b"</dc:title>", DEPTH),
        ),
        (
            "docProps/custom.xml",
            smoke::nested(b"<property name=\"n\">", b"x", b"</property>", DEPTH),
        ),
        // The slide ids name no relationship: resolving each one would read
        // the slide part again until the inflate budget ran out, which is
        // bounded but is not what this test measures.
        (
            "ppt/presentation.xml",
            smoke::nested(
                b"<p:sldIdLst><p:sldId id=\"256\" r:id=\"rId404\">",
                b"",
                b"</p:sldId></p:sldIdLst>",
                DEPTH,
            ),
        ),
    ];
    for (part, body) in other_cases {
        let bytes = deck_with(part, &body);
        smoke::run_on_small_stack(move || parse_bytes(&bytes));
    }
}

/// quick-xml 0.36 checks each attribute name against all earlier ones
/// (RUSTSEC-2026-0194), so this is quadratic until the dependency moves. The
/// time is printed, not asserted: the test only pins that it finishes.
#[test]
fn a_start_tag_with_twenty_thousand_attributes_is_parsed() {
    let mut body = b"<p:sp><p:nvSpPr><p:cNvPr".to_vec();
    for index in 0..20_000 {
        body.extend_from_slice(format!(" a{index}=\"v\"").as_bytes());
    }
    body.extend_from_slice(
        br#" id="4" name="Wide"></p:cNvPr></p:nvSpPr><p:txBody><a:p><a:r><a:t>kept</a:t></a:r></a:p></p:txBody></p:sp>"#,
    );
    let bytes = deck_with("ppt/slides/slide1.xml", &sp_tree("sld", &body));

    let started = std::time::Instant::now();
    let doc = parse_pptx_bytes(bytes, ParseOptions::default()).expect("deck parses");
    eprintln!(
        "pptx-fast-core: start tag with 20,000 attributes parsed in {:?}",
        started.elapsed()
    );
    let shape = &doc.slides[0].shapes[0];
    assert_eq!(shape.shape_id, Some(4));
    assert_eq!(shape.name.as_deref(), Some("Wide"));
    assert_eq!(shape.paragraphs[0].text, "kept");
}

/// The seeds above are also the starting corpus of the fuzz targets in
/// `fuzz/`; `WRITE_FUZZ_SEEDS=1` writes them out again after a seed changes.
#[test]
fn the_fuzz_corpus_holds_these_seeds() {
    let parts = seed_parts();
    let slide = parts
        .iter()
        .find(|(name, _)| *name == "ppt/slides/slide1.xml")
        .expect("the seed has a first slide")
        .1
        .clone();
    smoke::fuzz_seeds(
        env!("CARGO_MANIFEST_DIR"),
        &[
            (
                "pptx_container",
                "stored.pptx",
                package(&parts, None, CompressionMethod::Stored),
            ),
            (
                "pptx_container",
                "deflated.pptx",
                package(&parts, None, CompressionMethod::Deflated),
            ),
            ("pptx_slide_xml", "slide1.xml", slide),
        ],
    );
}
