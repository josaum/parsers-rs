use pptx_fast_core::{parse_pptx, ParseOptions};
use std::fs::File;
use std::path::Path;
use tempfile::tempdir;
use zip::write::FileOptions;
use zip::{CompressionMethod, ZipWriter};

fn write_part(writer: &mut ZipWriter<File>, path: &str, contents: &str) {
    let options = FileOptions::default().compression_method(CompressionMethod::Stored);
    writer.start_file(path, options).unwrap();
    std::io::Write::write_all(writer, contents.as_bytes()).unwrap();
}

fn build_pptx(path: &Path) {
    let file = File::create(path).unwrap();
    let mut zip = ZipWriter::new(file);

    write_part(
        &mut zip,
        "_rels/.rels",
        r#"<?xml version="1.0" encoding="UTF-8"?>
<Relationships xmlns="http://schemas.openxmlformats.org/package/2006/relationships">
  <Relationship Id="rId1" Type="http://schemas.openxmlformats.org/officeDocument/2006/relationships/officeDocument" Target="ppt/presentation.xml"/>
</Relationships>"#,
    );
    write_part(
        &mut zip,
        "ppt/presentation.xml",
        r#"<?xml version="1.0" encoding="UTF-8"?>
<p:presentation xmlns:p="http://schemas.openxmlformats.org/presentationml/2006/main" xmlns:r="http://schemas.openxmlformats.org/officeDocument/2006/relationships">
  <p:sldIdLst>
    <p:sldId id="256" r:id="rId1"/>
  </p:sldIdLst>
</p:presentation>"#,
    );
    write_part(
        &mut zip,
        "ppt/_rels/presentation.xml.rels",
        r#"<?xml version="1.0" encoding="UTF-8"?>
<Relationships xmlns="http://schemas.openxmlformats.org/package/2006/relationships">
  <Relationship Id="rId1" Type="http://schemas.openxmlformats.org/officeDocument/2006/relationships/slide" Target="slides/slide1.xml"/>
</Relationships>"#,
    );
    write_part(
        &mut zip,
        "ppt/slides/slide1.xml",
        r#"<?xml version="1.0" encoding="UTF-8"?>
<p:sld xmlns:p="http://schemas.openxmlformats.org/presentationml/2006/main" xmlns:a="http://schemas.openxmlformats.org/drawingml/2006/main">
  <p:cSld name="Intro">
    <p:spTree>
      <p:sp>
        <p:nvSpPr>
          <p:cNvPr id="2" name="Title 1"/>
        </p:nvSpPr>
        <p:txBody>
          <a:p><a:r><a:t>Hello PPTX</a:t></a:r></a:p>
        </p:txBody>
      </p:sp>
      <a:tbl>
        <a:tr>
          <a:tc><a:txBody><a:p><a:r><a:t>Cell A</a:t></a:r></a:p></a:txBody></a:tc>
          <a:tc><a:txBody><a:p><a:r><a:t>Cell B</a:t></a:r></a:p></a:txBody></a:tc>
        </a:tr>
      </a:tbl>
    </p:spTree>
  </p:cSld>
</p:sld>"#,
    );
    write_part(
        &mut zip,
        "ppt/slides/_rels/slide1.xml.rels",
        r#"<?xml version="1.0" encoding="UTF-8"?>
<Relationships xmlns="http://schemas.openxmlformats.org/package/2006/relationships">
  <Relationship Id="rId1" Type="http://schemas.openxmlformats.org/officeDocument/2006/relationships/notesSlide" Target="../notesSlides/notesSlide1.xml"/>
</Relationships>"#,
    );
    write_part(
        &mut zip,
        "ppt/notesSlides/notesSlide1.xml",
        r#"<?xml version="1.0" encoding="UTF-8"?>
<p:notes xmlns:p="http://schemas.openxmlformats.org/presentationml/2006/main" xmlns:a="http://schemas.openxmlformats.org/drawingml/2006/main">
  <p:cSld>
    <p:spTree>
      <p:sp>
        <p:nvSpPr>
          <p:cNvPr id="5" name="Notes"/>
        </p:nvSpPr>
        <p:txBody>
          <a:p><a:r><a:t>Speaker note</a:t></a:r></a:p>
        </p:txBody>
      </p:sp>
    </p:spTree>
  </p:cSld>
</p:notes>"#,
    );

    zip.finish().unwrap();
}

#[test]
fn parses_minimal_pptx_presentation() {
    let dir = tempdir().unwrap();
    let path = dir.path().join("sample.pptx");
    build_pptx(&path);

    let deck = parse_pptx(&path, ParseOptions::default()).unwrap();

    assert_eq!(deck.slides.len(), 1);
    let slide = &deck.slides[0];
    assert_eq!(slide.id, 256);
    assert_eq!(slide.name.as_deref(), Some("Intro"));
    assert_eq!(slide.shapes.len(), 1);
    assert_eq!(slide.shapes[0].paragraphs[0].text, "Hello PPTX");
    assert_eq!(slide.tables.len(), 1);
    assert_eq!(slide.tables[0].rows[0].cells[0].text, "Cell A");
    assert_eq!(slide.notes.len(), 1);
    assert_eq!(slide.notes[0].paragraphs[0].text, "Speaker note");
}

#[test]
fn parse_flags_skip_optional_notes_and_images() {
    // WHY: callers that only need slide text should not inflate notes/images.
    let dir = tempdir().unwrap();
    let path = dir.path().join("sample.pptx");
    build_pptx(&path);

    let deck = parse_pptx(
        &path,
        ParseOptions {
            extract_notes: false,
            extract_comments: false,
            extract_images: false,
            ..ParseOptions::default()
        },
    )
    .unwrap();

    assert_eq!(deck.slides.len(), 1);
    assert!(deck.slides[0].notes.is_empty());
    assert!(deck.comments.is_empty());
    assert!(deck.images.is_empty());
}

#[derive(Clone, Copy, Debug)]
enum OptionalPptxPart {
    Master,
    Layout,
    SlideRelationships,
    Notes,
    Comments,
    CommentAuthors,
}

impl OptionalPptxPart {
    fn name(self) -> &'static str {
        match self {
            Self::Master => "master",
            Self::Layout => "layout",
            Self::SlideRelationships => "slide relationships",
            Self::Notes => "notes",
            Self::Comments => "comments",
            Self::CommentAuthors => "comment authors",
        }
    }
}

const PPTX_OPTIONAL_CASES: &[OptionalPptxPart] = &[
    OptionalPptxPart::Master,
    OptionalPptxPart::Layout,
    OptionalPptxPart::SlideRelationships,
    OptionalPptxPart::Notes,
    OptionalPptxPart::Comments,
    OptionalPptxPart::CommentAuthors,
];

fn build_pptx_with_optional_part(
    path: &Path,
    case: OptionalPptxPart,
    optional_part: Option<&str>,
) -> usize {
    let (presentation_body, presentation_relationship) = match case {
        OptionalPptxPart::Master => (
            r#"<p:sldMasterIdLst><p:sldMasterId r:id="rIdOptional"/></p:sldMasterIdLst>"#,
            r#"<Relationship Id="rIdOptional" Type="http://schemas.openxmlformats.org/officeDocument/2006/relationships/slideMaster" Target="slideMasters/slideMaster1.xml"/>"#,
        ),
        OptionalPptxPart::Layout => (
            r#"<p:sldLayoutIdLst><p:sldLayoutId r:id="rIdOptional"/></p:sldLayoutIdLst>"#,
            r#"<Relationship Id="rIdOptional" Type="http://schemas.openxmlformats.org/officeDocument/2006/relationships/slideLayout" Target="slideLayouts/slideLayout1.xml"/>"#,
        ),
        OptionalPptxPart::SlideRelationships
        | OptionalPptxPart::Notes
        | OptionalPptxPart::Comments
        | OptionalPptxPart::CommentAuthors => (
            r#"<p:sldIdLst><p:sldId id="256" r:id="rIdSlide"/></p:sldIdLst>"#,
            r#"<Relationship Id="rIdSlide" Type="http://schemas.openxmlformats.org/officeDocument/2006/relationships/slide" Target="slides/slide1.xml"/>"#,
        ),
    };
    let presentation = format!(
        r#"<?xml version="1.0" encoding="UTF-8"?>
<p:presentation xmlns:p="http://schemas.openxmlformats.org/presentationml/2006/main"
                xmlns:r="http://schemas.openxmlformats.org/officeDocument/2006/relationships">
  {presentation_body}
</p:presentation>"#
    );
    let presentation_rels = format!(
        r#"<?xml version="1.0" encoding="UTF-8"?>
<Relationships xmlns="http://schemas.openxmlformats.org/package/2006/relationships">
  {presentation_relationship}
</Relationships>"#
    );
    let slide = r#"<?xml version="1.0" encoding="UTF-8"?>
<p:sld xmlns:p="http://schemas.openxmlformats.org/presentationml/2006/main">
  <p:cSld name="Slide"><p:spTree/></p:cSld>
</p:sld>"#;
    let slide_rels = match case {
        OptionalPptxPart::Notes => Some(
            r#"<?xml version="1.0" encoding="UTF-8"?>
<Relationships xmlns="http://schemas.openxmlformats.org/package/2006/relationships">
  <Relationship Id="rIdOptional" Type="http://schemas.openxmlformats.org/officeDocument/2006/relationships/notesSlide" Target="../notesSlides/notesSlide1.xml"/>
</Relationships>"#,
        ),
        OptionalPptxPart::Comments | OptionalPptxPart::CommentAuthors => Some(
            r#"<?xml version="1.0" encoding="UTF-8"?>
<Relationships xmlns="http://schemas.openxmlformats.org/package/2006/relationships">
  <Relationship Id="rIdOptional" Type="http://schemas.openxmlformats.org/officeDocument/2006/relationships/comments" Target="../comments/comment1.xml"/>
</Relationships>"#,
        ),
        _ => None,
    };
    let comment = r#"<?xml version="1.0" encoding="UTF-8"?>
<p:cmLst xmlns:p="http://schemas.openxmlformats.org/presentationml/2006/main">
  <p:cm authorId="0" idx="1"><p:text>Review</p:text></p:cm>
</p:cmLst>"#;

    let file = File::create(path).unwrap();
    let mut zip = ZipWriter::new(file);
    write_part(&mut zip, "ppt/presentation.xml", &presentation);
    write_part(
        &mut zip,
        "ppt/_rels/presentation.xml.rels",
        &presentation_rels,
    );

    let mut required_size = presentation.len() + presentation_rels.len();
    match case {
        OptionalPptxPart::Master => {
            if let Some(optional_part) = optional_part {
                write_part(&mut zip, "ppt/slideMasters/slideMaster1.xml", optional_part);
            }
        }
        OptionalPptxPart::Layout => {
            if let Some(optional_part) = optional_part {
                write_part(&mut zip, "ppt/slideLayouts/slideLayout1.xml", optional_part);
            }
        }
        OptionalPptxPart::SlideRelationships => {
            write_part(&mut zip, "ppt/slides/slide1.xml", slide);
            required_size += slide.len();
            if let Some(optional_part) = optional_part {
                write_part(&mut zip, "ppt/slides/_rels/slide1.xml.rels", optional_part);
            }
        }
        OptionalPptxPart::Notes => {
            write_part(&mut zip, "ppt/slides/slide1.xml", slide);
            write_part(
                &mut zip,
                "ppt/slides/_rels/slide1.xml.rels",
                slide_rels.unwrap(),
            );
            required_size += slide.len() + slide_rels.unwrap().len();
            if let Some(optional_part) = optional_part {
                write_part(&mut zip, "ppt/notesSlides/notesSlide1.xml", optional_part);
            }
        }
        OptionalPptxPart::Comments => {
            write_part(&mut zip, "ppt/slides/slide1.xml", slide);
            write_part(
                &mut zip,
                "ppt/slides/_rels/slide1.xml.rels",
                slide_rels.unwrap(),
            );
            required_size += slide.len() + slide_rels.unwrap().len();
            if let Some(optional_part) = optional_part {
                write_part(&mut zip, "ppt/comments/comment1.xml", optional_part);
            }
        }
        OptionalPptxPart::CommentAuthors => {
            write_part(&mut zip, "ppt/slides/slide1.xml", slide);
            write_part(
                &mut zip,
                "ppt/slides/_rels/slide1.xml.rels",
                slide_rels.unwrap(),
            );
            write_part(&mut zip, "ppt/comments/comment1.xml", comment);
            required_size += slide.len() + slide_rels.unwrap().len() + comment.len();
            if let Some(optional_part) = optional_part {
                write_part(&mut zip, "ppt/commentAuthors.xml", optional_part);
            }
        }
    }
    zip.finish().unwrap();

    required_size + 16
}

#[test]
fn absent_optional_pptx_parts_are_omitted() {
    for &case in PPTX_OPTIONAL_CASES {
        let dir = tempdir().unwrap();
        let path = dir.path().join(format!("absent-{}.pptx", case.name()));
        build_pptx_with_optional_part(&path, case, None);

        let result = parse_pptx(
            &path,
            ParseOptions {
                extract_images: false,
                ..ParseOptions::default()
            },
        );
        assert!(
            result.is_ok(),
            "absent optional {} must be omitted: {result:?}",
            case.name()
        );
    }
}

#[test]
fn present_optional_pptx_parts_cannot_hide_budget_failures() {
    let oversized = "x".repeat(4_096);
    for &case in PPTX_OPTIONAL_CASES {
        let dir = tempdir().unwrap();
        let path = dir.path().join(format!("over-budget-{}.pptx", case.name()));
        let max_inflate = build_pptx_with_optional_part(&path, case, Some(&oversized));

        let error = parse_pptx(
            &path,
            ParseOptions {
                max_inflate,
                extract_images: false,
                ..ParseOptions::default()
            },
        )
        .expect_err("a present optional part must propagate its read failure");
        assert!(
            format!("{error:#}").contains("max_inflate"),
            "{} returned the wrong error: {error:#}",
            case.name()
        );
    }
}

#[test]
fn present_malformed_optional_pptx_parts_fail_loudly() {
    for &case in PPTX_OPTIONAL_CASES {
        let dir = tempdir().unwrap();
        let path = dir.path().join(format!("malformed-{}.pptx", case.name()));
        build_pptx_with_optional_part(&path, case, Some("<"));

        let result = parse_pptx(
            &path,
            ParseOptions {
                extract_images: false,
                ..ParseOptions::default()
            },
        );
        assert!(
            result.is_err(),
            "present malformed optional {} was ignored",
            case.name()
        );
    }
}
