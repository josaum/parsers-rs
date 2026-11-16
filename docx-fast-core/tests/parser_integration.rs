use docx_fast_core::{parse_docx, Block, ParseOptions};
use std::fs::File;
use std::path::Path;
use tempfile::tempdir;
use zip::write::SimpleFileOptions;
use zip::{CompressionMethod, ZipWriter};

fn write_part(writer: &mut ZipWriter<File>, path: &str, contents: &str) {
    write_part_compressed(writer, path, contents, CompressionMethod::Stored);
}

fn write_part_compressed(
    writer: &mut ZipWriter<File>,
    path: &str,
    contents: &str,
    compression: CompressionMethod,
) {
    let options = SimpleFileOptions::default().compression_method(compression);
    writer.start_file(path, options).unwrap();
    std::io::Write::write_all(writer, contents.as_bytes()).unwrap();
}

fn write_docx_parts(path: &Path, parts: &[(&str, &str, CompressionMethod)]) {
    let file = File::create(path).unwrap();
    let mut zip = ZipWriter::new(file);
    for (part_path, contents, compression) in parts {
        write_part_compressed(&mut zip, part_path, contents, *compression);
    }
    zip.finish().unwrap();
}

fn build_docx(path: &Path) {
    let file = File::create(path).unwrap();
    let mut zip = ZipWriter::new(file);

    write_part(
        &mut zip,
        "_rels/.rels",
        r#"<?xml version="1.0" encoding="UTF-8"?>
<Relationships xmlns="http://schemas.openxmlformats.org/package/2006/relationships">
  <Relationship Id="rId1" Type="http://schemas.openxmlformats.org/officeDocument/2006/relationships/officeDocument" Target="word/document.xml"/>
</Relationships>"#,
    );
    write_part(
        &mut zip,
        "word/_rels/document.xml.rels",
        r#"<?xml version="1.0" encoding="UTF-8"?>
<Relationships xmlns="http://schemas.openxmlformats.org/package/2006/relationships"/>"#,
    );
    write_part(
        &mut zip,
        "word/document.xml",
        r#"<?xml version="1.0" encoding="UTF-8"?>
<w:document xmlns:w="http://schemas.openxmlformats.org/wordprocessingml/2006/main">
  <w:body>
    <w:p>
      <w:r><w:t>Hello DOCX</w:t></w:r>
    </w:p>
    <w:tbl>
      <w:tr>
        <w:tc>
          <w:p><w:r><w:t>Cell 1</w:t></w:r></w:p>
        </w:tc>
        <w:tc>
          <w:p><w:r><w:t>Cell 2</w:t></w:r></w:p>
        </w:tc>
      </w:tr>
    </w:tbl>
  </w:body>
</w:document>"#,
    );

    zip.finish().unwrap();
}

#[test]
fn parses_minimal_docx_document() {
    let dir = tempdir().unwrap();
    let path = dir.path().join("sample.docx");
    build_docx(&path);

    let doc = parse_docx(&path, ParseOptions::default()).unwrap();

    assert_eq!(doc.blocks.len(), 2);
    match &doc.blocks[0] {
        Block::Paragraph(paragraph) => assert_eq!(paragraph.text, "Hello DOCX"),
        other => panic!("expected paragraph block, got {other:?}"),
    }
    match &doc.blocks[1] {
        Block::Table(table) => {
            assert_eq!(table.rows.len(), 1);
            assert_eq!(table.rows[0].cells.len(), 2);
            match &table.rows[0].cells[0].blocks[0] {
                Block::Paragraph(paragraph) => assert_eq!(paragraph.text, "Cell 1"),
                other => panic!("expected table cell paragraph, got {other:?}"),
            }
        }
        other => panic!("expected table block, got {other:?}"),
    }
}

#[test]
fn parses_valid_document_without_document_relationships_part() {
    let dir = tempdir().unwrap();
    let path = dir.path().join("relationship-free.docx");
    let document = r#"<?xml version="1.0" encoding="UTF-8"?>
<w:document xmlns:w="http://schemas.openxmlformats.org/wordprocessingml/2006/main">
  <w:body><w:p><w:r><w:t>No relationships required</w:t></w:r></w:p></w:body>
</w:document>"#;
    write_docx_parts(
        &path,
        &[("word/document.xml", document, CompressionMethod::Stored)],
    );

    let doc = parse_docx(&path, ParseOptions::default()).unwrap();
    match &doc.blocks[0] {
        Block::Paragraph(paragraph) => assert_eq!(paragraph.text, "No relationships required"),
        other => panic!("expected paragraph block, got {other:?}"),
    }
}

#[test]
fn rejects_deflated_part_over_max_inflate() {
    let dir = tempdir().unwrap();
    let path = dir.path().join("oversized.docx");
    let payload = "x".repeat(512);
    let document = format!(
        r#"<?xml version="1.0" encoding="UTF-8"?>
<w:document xmlns:w="http://schemas.openxmlformats.org/wordprocessingml/2006/main">
  <w:body><w:p><w:r><w:t>{payload}</w:t></w:r></w:p></w:body>
</w:document>"#
    );
    write_docx_parts(
        &path,
        &[("word/document.xml", &document, CompressionMethod::Deflated)],
    );

    let error = parse_docx(
        &path,
        ParseOptions {
            max_inflate: 256,
            ..ParseOptions::default()
        },
    )
    .unwrap_err();
    let error_chain = format!("{error:#}");
    assert!(
        error_chain.contains("max_inflate"),
        "unexpected error: {error_chain}"
    );
}

#[test]
fn resolves_strict_ooxml_relationship_types() {
    let dir = tempdir().unwrap();
    let path = dir.path().join("strict.docx");
    let root_rels = r#"<?xml version="1.0" encoding="UTF-8"?>
<Relationships xmlns="http://schemas.openxmlformats.org/package/2006/relationships">
  <Relationship Id="rId1" Type="http://purl.oclc.org/ooxml/officeDocument/relationships/officeDocument" Target="word/main.xml"/>
</Relationships>"#;
    let document_rels = r#"<?xml version="1.0" encoding="UTF-8"?>
<Relationships xmlns="http://schemas.openxmlformats.org/package/2006/relationships">
  <Relationship Id="rId1" Type="http://purl.oclc.org/ooxml/officeDocument/relationships/styles" Target="styles.xml"/>
</Relationships>"#;
    let document = r#"<?xml version="1.0" encoding="UTF-8"?>
<w:document xmlns:w="http://purl.oclc.org/ooxml/wordprocessingml/main">
  <w:body><w:p><w:pPr><w:pStyle w:val="Heading1"/></w:pPr><w:r><w:t>Strict heading</w:t></w:r></w:p></w:body>
</w:document>"#;
    let styles = r#"<?xml version="1.0" encoding="UTF-8"?>
<w:styles xmlns:w="http://purl.oclc.org/ooxml/wordprocessingml/main">
  <w:style w:type="paragraph" w:styleId="Heading1"><w:name w:val="heading 1"/></w:style>
</w:styles>"#;
    write_docx_parts(
        &path,
        &[
            ("_rels/.rels", root_rels, CompressionMethod::Stored),
            (
                "word/_rels/main.xml.rels",
                document_rels,
                CompressionMethod::Stored,
            ),
            ("word/main.xml", document, CompressionMethod::Stored),
            ("word/styles.xml", styles, CompressionMethod::Stored),
        ],
    );

    let doc = parse_docx(&path, ParseOptions::default()).unwrap();
    assert_eq!(doc.styles.len(), 1);
    match &doc.blocks[0] {
        Block::Paragraph(paragraph) => {
            assert_eq!(paragraph.text, "Strict heading");
            assert_eq!(paragraph.style_name.as_deref(), Some("heading 1"));
        }
        other => panic!("expected paragraph block, got {other:?}"),
    }
}

#[test]
fn filters_technical_notes_and_preserves_note_references() {
    let dir = tempdir().unwrap();
    let path = dir.path().join("notes.docx");
    let document_rels = r#"<?xml version="1.0" encoding="UTF-8"?>
<Relationships xmlns="http://schemas.openxmlformats.org/package/2006/relationships">
  <Relationship Id="rId1" Type="http://schemas.openxmlformats.org/officeDocument/2006/relationships/footnotes" Target="footnotes.xml"/>
  <Relationship Id="rId2" Type="http://schemas.openxmlformats.org/officeDocument/2006/relationships/endnotes" Target="endnotes.xml"/>
</Relationships>"#;
    let document = r#"<?xml version="1.0" encoding="UTF-8"?>
<w:document xmlns:w="http://schemas.openxmlformats.org/wordprocessingml/2006/main">
  <w:body><w:p><w:r><w:t>Claim</w:t><w:footnoteReference w:id="1"/><w:endnoteReference w:id="2"/></w:r></w:p></w:body>
</w:document>"#;
    let footnotes = r#"<?xml version="1.0" encoding="UTF-8"?>
<w:footnotes xmlns:w="http://schemas.openxmlformats.org/wordprocessingml/2006/main">
  <w:footnote w:id="-1" w:type="separator"><w:p><w:r><w:t>separator</w:t></w:r></w:p></w:footnote>
  <w:footnote w:id="0" w:type="continuationSeparator"><w:p><w:r><w:t>continuation</w:t></w:r></w:p></w:footnote>
  <w:footnote w:id="1"><w:p><w:r><w:t>User note</w:t></w:r></w:p></w:footnote>
</w:footnotes>"#;
    let endnotes = r#"<?xml version="1.0" encoding="UTF-8"?>
<w:endnotes xmlns:w="http://schemas.openxmlformats.org/wordprocessingml/2006/main">
  <w:endnote w:id="-1" w:type="separator"><w:p><w:r><w:t>separator</w:t></w:r></w:p></w:endnote>
  <w:endnote w:id="2"><w:p><w:r><w:t>User endnote</w:t></w:r></w:p></w:endnote>
</w:endnotes>"#;
    write_docx_parts(
        &path,
        &[
            (
                "word/_rels/document.xml.rels",
                document_rels,
                CompressionMethod::Stored,
            ),
            ("word/document.xml", document, CompressionMethod::Stored),
            ("word/footnotes.xml", footnotes, CompressionMethod::Stored),
            ("word/endnotes.xml", endnotes, CompressionMethod::Stored),
        ],
    );

    let doc = parse_docx(&path, ParseOptions::default()).unwrap();
    assert_eq!(
        doc.footnotes.iter().map(|note| note.id).collect::<Vec<_>>(),
        vec![1]
    );
    assert_eq!(
        doc.endnotes.iter().map(|note| note.id).collect::<Vec<_>>(),
        vec![2]
    );
    match &doc.blocks[0] {
        Block::Paragraph(paragraph) => {
            assert_eq!(paragraph.runs[0].note_references.len(), 2);
            assert_eq!(paragraph.runs[0].note_references[0].kind, "footnote");
            assert_eq!(paragraph.runs[0].note_references[0].id, 1);
            assert_eq!(paragraph.runs[0].note_references[1].kind, "endnote");
            assert_eq!(paragraph.runs[0].note_references[1].id, 2);
        }
        other => panic!("expected paragraph block, got {other:?}"),
    }
}

#[test]
fn parse_flags_skip_optional_comments_images_headers() {
    // WHY: text-only ingest should not inflate comments, images, or headers.
    let dir = tempdir().unwrap();
    let path = dir.path().join("flags.docx");
    build_docx(&path);

    let doc = parse_docx(
        &path,
        ParseOptions {
            extract_comments: false,
            extract_images: false,
            extract_headers: false,
            ..ParseOptions::default()
        },
    )
    .unwrap();

    assert!(doc.comments.is_empty());
    assert!(doc.images.is_empty());
    assert!(doc.headers.is_empty());
    assert!(doc.footers.is_empty());
    assert!(!doc.blocks.is_empty());
}
