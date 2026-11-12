//! Entity and character references in every place the parser reads text.
//!
//! The same input is planted in each text path and each attribute path, one
//! deck per path, and the outcome is compared against a table. The tables are
//! the contract: an XML library upgrade that changes how references arrive
//! must leave them unchanged, or change them on purpose in the same commit.

use pptx_fast_core::{parse_pptx_bytes, ParseOptions, PptxDocument};
use std::io::{Cursor, Write};
use zip::write::SimpleFileOptions;
use zip::{CompressionMethod, ZipWriter};

const P: &str = "http://schemas.openxmlformats.org/presentationml/2006/main";
const A: &str = "http://schemas.openxmlformats.org/drawingml/2006/main";
const REL: &str = "http://schemas.openxmlformats.org/officeDocument/2006/relationships";
const PKG_REL: &str = "http://schemas.openxmlformats.org/package/2006/relationships";
const CORE: &str = "http://schemas.openxmlformats.org/package/2006/metadata/core-properties";
const CUSTOM: &str = "http://schemas.openxmlformats.org/officeDocument/2006/custom-properties";
const VT: &str = "http://schemas.openxmlformats.org/officeDocument/2006/docPropsVTypes";

/// The variable parts of a one-slide deck; everything else is fixed.
#[derive(Default)]
struct Parts {
    /// Attributes of the slide's `<p:cSld>`, with a leading space.
    slide_attrs: String,
    /// Inner XML of the slide's `<p:spTree>`.
    slide: String,
    /// Inner XML of the notes slide's `<p:spTree>`.
    notes: String,
    /// Inner XML of `<p:cmLst>`.
    comments: String,
    /// Value of the comment author's `name` attribute.
    author: String,
    /// Inner XML of `<cp:coreProperties>`.
    core: String,
    /// Inner XML of the custom `<Properties>`.
    custom: String,
}

impl Parts {
    /// Builds the package. Every `@` becomes the byte 0xE9, which is not
    /// valid UTF-8 on its own.
    fn build(&self) -> Vec<u8> {
        let author = if self.author.is_empty() {
            "Ada"
        } else {
            &self.author
        };
        let parts: Vec<(&str, String)> = vec![
            (
                "_rels/.rels",
                format!(
                    r#"<Relationships xmlns="{PKG_REL}"><Relationship Id="rId1" Type="{REL}/officeDocument" Target="ppt/presentation.xml"/></Relationships>"#
                ),
            ),
            (
                "ppt/presentation.xml",
                format!(
                    r#"<p:presentation xmlns:p="{P}" xmlns:r="{REL}"><p:sldIdLst><p:sldId id="256" r:id="rId1"/></p:sldIdLst></p:presentation>"#
                ),
            ),
            (
                "ppt/_rels/presentation.xml.rels",
                format!(
                    r#"<Relationships xmlns="{PKG_REL}"><Relationship Id="rId1" Type="{REL}/slide" Target="slides/slide1.xml"/></Relationships>"#
                ),
            ),
            (
                "ppt/slides/slide1.xml",
                format!(
                    r#"<p:sld xmlns:p="{P}" xmlns:a="{A}"><p:cSld{}><p:spTree>{}</p:spTree></p:cSld></p:sld>"#,
                    self.slide_attrs, self.slide
                ),
            ),
            (
                "ppt/slides/_rels/slide1.xml.rels",
                format!(
                    r#"<Relationships xmlns="{PKG_REL}"><Relationship Id="rId1" Type="{REL}/notesSlide" Target="../notesSlides/notesSlide1.xml"/><Relationship Id="rId2" Type="{REL}/comments" Target="../comments/comment1.xml"/></Relationships>"#
                ),
            ),
            (
                "ppt/notesSlides/notesSlide1.xml",
                format!(
                    r#"<p:notes xmlns:p="{P}" xmlns:a="{A}"><p:cSld><p:spTree>{}</p:spTree></p:cSld></p:notes>"#,
                    self.notes
                ),
            ),
            (
                "ppt/comments/comment1.xml",
                format!(r#"<p:cmLst xmlns:p="{P}">{}</p:cmLst>"#, self.comments),
            ),
            (
                "ppt/commentAuthors.xml",
                format!(
                    r#"<p:cmAuthorLst xmlns:p="{P}"><p:cmAuthor id="0" name="{author}" initials="A" lastIdx="1" clrIdx="0"/></p:cmAuthorLst>"#
                ),
            ),
            (
                "docProps/core.xml",
                format!(
                    r#"<cp:coreProperties xmlns:cp="{CORE}" xmlns:dc="http://purl.org/dc/elements/1.1/">{}</cp:coreProperties>"#,
                    self.core
                ),
            ),
            (
                "docProps/custom.xml",
                format!(
                    r#"<Properties xmlns="{CUSTOM}" xmlns:vt="{VT}">{}</Properties>"#,
                    self.custom
                ),
            ),
        ];

        let mut zip = ZipWriter::new(Cursor::new(Vec::new()));
        let options = SimpleFileOptions::default().compression_method(CompressionMethod::Stored);
        for (name, body) in parts {
            let bytes: Vec<u8> = body
                .bytes()
                .map(|byte| if byte == b'@' { 0xE9 } else { byte })
                .collect();
            zip.start_file(name, options).unwrap();
            zip.write_all(&bytes).unwrap();
        }
        zip.finish().unwrap().into_inner()
    }

    /// Parses the deck and reads one value out of it.
    ///
    /// A deck that does not parse reads as `<parse error>` and a value the
    /// parser did not produce as `<absent>`, so that both show in the table.
    fn read(&self, pick: impl Fn(&PptxDocument) -> Option<String>) -> String {
        match parse_pptx_bytes(self.build(), ParseOptions::default()) {
            Ok(doc) => pick(&doc).unwrap_or_else(|| "<absent>".to_string()),
            Err(_) => "<parse error>".to_string(),
        }
    }
}

fn run(text: &str) -> String {
    format!("<p:sp><p:txBody><a:bodyPr/><a:p><a:r><a:t>{text}</a:t></a:r></a:p></p:txBody></p:sp>")
}

fn first_paragraph(shapes: &[pptx_fast_core::ShapeText]) -> Option<String> {
    Some(shapes.first()?.paragraphs.first()?.text.clone())
}

/// Every place that reads element text, as `(name, value read for an input)`.
fn text_paths(input: &str) -> Vec<(&'static str, String)> {
    vec![
        (
            "slide run",
            Parts {
                slide: run(input),
                ..Parts::default()
            }
            .read(|doc| first_paragraph(&doc.slides.first()?.shapes)),
        ),
        (
            "table cell",
            Parts {
                slide: format!(
                    "<p:graphicFrame><a:graphic><a:graphicData><a:tbl><a:tr h=\"1\"><a:tc><a:txBody><a:p><a:r><a:t>{input}</a:t></a:r></a:p></a:txBody></a:tc></a:tr></a:tbl></a:graphicData></a:graphic></p:graphicFrame>"
                ),
                ..Parts::default()
            }
            .read(|doc| {
                let table = doc.slides.first()?.tables.first()?;
                Some(table.rows.first()?.cells.first()?.text.clone())
            }),
        ),
        (
            "notes run",
            Parts {
                notes: run(input),
                ..Parts::default()
            }
            .read(|doc| first_paragraph(&doc.slides.first()?.notes)),
        ),
        (
            "legacy comment",
            Parts {
                comments: format!(
                    r#"<p:cm authorId="0" idx="1"><p:pos x="1" y="1"/><p:text>{input}</p:text></p:cm>"#
                ),
                ..Parts::default()
            }
            .read(|doc| doc.comments.first()?.text.clone()),
        ),
        (
            "core property",
            Parts {
                core: format!("<dc:title>{input}</dc:title>"),
                ..Parts::default()
            }
            .read(|doc| doc.metadata.core.get("title").cloned()),
        ),
        (
            "custom property",
            Parts {
                custom: format!(
                    r#"<property fmtid="x" pid="2" name="Client"><vt:lpwstr>{input}</vt:lpwstr></property>"#
                ),
                ..Parts::default()
            }
            .read(|doc| doc.metadata.custom.get("Client").cloned()),
        ),
    ]
}

/// Every place that reads an attribute value into the output.
fn attribute_paths(input: &str) -> Vec<(&'static str, String)> {
    vec![
        (
            "shape name",
            Parts {
                slide: format!(
                    r#"<p:sp><p:nvSpPr><p:cNvPr id="2" name="{input}"/></p:nvSpPr><p:txBody><a:p><a:r><a:t>x</a:t></a:r></a:p></p:txBody></p:sp>"#
                ),
                ..Parts::default()
            }
            .read(|doc| doc.slides.first()?.shapes.first()?.name.clone()),
        ),
        (
            "slide name",
            Parts {
                slide_attrs: format!(r#" name="{input}""#),
                ..Parts::default()
            }
            .read(|doc| doc.slides.first()?.name.clone()),
        ),
        (
            "custom property name",
            Parts {
                custom: format!(
                    r#"<property fmtid="x" pid="2" name="{input}"><vt:lpwstr>v</vt:lpwstr></property>"#
                ),
                ..Parts::default()
            }
            .read(|doc| {
                let mut names: Vec<_> = doc.metadata.custom.keys().cloned().collect();
                names.sort();
                names.into_iter().next()
            }),
        ),
        (
            "comment author",
            Parts {
                author: input.to_string(),
                comments: r#"<p:cm authorId="0" idx="1"><p:text>t</p:text></p:cm>"#.to_string(),
                ..Parts::default()
            }
            .read(|doc| doc.comments.first()?.author.clone()),
        ),
    ]
}

/// Renders one table: an input every path agrees on takes one line, an input
/// the paths disagree on takes one line per path.
fn table(inputs: &[&str], paths: impl Fn(&str) -> Vec<(&'static str, String)>) -> String {
    let mut out = String::new();
    for input in inputs {
        let results = paths(input);
        let first = &results[0].1;
        if results.iter().all(|(_, value)| value == first) {
            out.push_str(&format!("{input:?} => {first:?}\n"));
        } else {
            out.push_str(&format!("{input:?} =>\n"));
            for (path, value) in &results {
                out.push_str(&format!("    {path}: {value:?}\n"));
            }
        }
    }
    out
}

fn assert_table(actual: &str, expected: &str) {
    let expected: String = expected
        .lines()
        .skip_while(|line| line.is_empty())
        .map(|line| format!("{line}\n"))
        .collect();
    assert!(
        actual == expected,
        "the table changed.\n--- actual ---\n{actual}--- expected ---\n{expected}"
    );
}

const TEXT_INPUTS: &[&str] = &[
    "a &amp; b",
    "1 &lt; 2 &gt; 0",
    "&quot;q&quot; &apos;s&apos;",
    "caf&#233;",
    "&#x1F600;",
    "tab&#9;nl&#10;end",
    // A reference between spaces: the spaces are text, not padding.
    " &amp; ",
    "a &foo; b",
    "a&#0;b",
    "a &#xD800; b",
    // One bad reference next to good ones.
    "a &amp; b &foo; c",
    "x &amp; &#0; y",
    // A reference next to a CDATA section, and text on both sides of one.
    "a &amp;<![CDATA[<b> &amp;]]> c",
    "a<![CDATA[b]]>c",
    // Not well-formed, and still written by real tools.
    "AT&T",
    "a &amp b",
    "a&;b",
    // An invalid byte next to a reference.
    "caf@ &amp; x",
    // Line endings in text are not normalized.
    "l1\r\nl2",
];

const ATTRIBUTE_INPUTS: &[&str] = &[
    "A &amp; B",
    "&lt;x&gt; &quot;q&quot; &apos;s&apos;",
    "caf&#233;",
    "&#x1F600;",
    " &amp; ",
    // Whitespace written literally and as character references.
    "a\tb",
    "a\nb",
    "a\r\nb",
    "a&#9;b&#10;c",
    "a &foo; b",
    "a&#0;b",
    "AT&T",
    "caf@",
];

#[test]
fn references_in_text() {
    assert_table(
        &table(TEXT_INPUTS, text_paths),
        r##"
"a &amp; b" => "a & b"
"1 &lt; 2 &gt; 0" => "1 < 2 > 0"
"&quot;q&quot; &apos;s&apos;" => "\"q\" 's'"
"caf&#233;" => "café"
"&#x1F600;" => "😀"
"tab&#9;nl&#10;end" => "tab\tnl\nend"
" &amp; " => " & "
"a &foo; b" => "a &foo; b"
"a&#0;b" => "a&#0;b"
"a &#xD800; b" => "a &#xD800; b"
"a &amp; b &foo; c" => "a &amp; b &foo; c"
"x &amp; &#0; y" => "x &amp; &#0; y"
"a &amp;<![CDATA[<b> &amp;]]> c" =>
    slide run: "a &<b> &amp; c"
    table cell: "a &<b> &amp; c"
    notes run: "a &<b> &amp; c"
    legacy comment: "a &<b> &amp; c"
    core property: "a &"
    custom property: "a &"
"a<![CDATA[b]]>c" =>
    slide run: "abc"
    table cell: "abc"
    notes run: "abc"
    legacy comment: "abc"
    core property: "a"
    custom property: "a"
"AT&T" => "AT&T"
"a &amp b" => "a &amp b"
"a&;b" => "a&;b"
"caf@ &amp; x" => "caf� & x"
"l1\r\nl2" => "l1\r\nl2"
"##,
    );
}

#[test]
fn references_in_attributes() {
    assert_table(
        &table(ATTRIBUTE_INPUTS, attribute_paths),
        r##"
"A &amp; B" => "A & B"
"&lt;x&gt; &quot;q&quot; &apos;s&apos;" => "<x> \"q\" 's'"
"caf&#233;" => "café"
"&#x1F600;" => "😀"
" &amp; " => " & "
"a\tb" => "a\tb"
"a\nb" => "a\nb"
"a\r\nb" => "a\r\nb"
"a&#9;b&#10;c" => "a\tb\nc"
"a &foo; b" => "<parse error>"
"a&#0;b" => "<parse error>"
"AT&T" => "<parse error>"
"caf@" => "<parse error>"
"##,
    );
}
