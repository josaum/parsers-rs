//! Text in a symbol font reads as the characters a reader sees, not as the
//! positions in the font that the file stores.

use docx_fast_core::{parse_docx_bytes, Block, DocxDocument, ParseOptions};
use std::io::{Cursor, Write};
use zip::write::SimpleFileOptions;
use zip::{CompressionMethod, ZipWriter};

const W: &str = "http://schemas.openxmlformats.org/wordprocessingml/2006/main";
const REL: &str = "http://schemas.openxmlformats.org/officeDocument/2006/relationships";
const PKG_REL: &str = "http://schemas.openxmlformats.org/package/2006/relationships";

fn w_part(root: &str, body: &str) -> Vec<u8> {
    format!(r#"<?xml version="1.0" encoding="UTF-8"?><w:{root} xmlns:w="{W}">{body}</w:{root}>"#)
        .into_bytes()
}

/// A document with `body` as its paragraphs and `styles` as its style sheet.
fn parse(body: &str, styles: &str) -> DocxDocument {
    let parts = [
        (
            "word/document.xml",
            w_part("document", &format!("<w:body>{body}</w:body>")),
        ),
        (
            "word/_rels/document.xml.rels",
            format!(
                r#"<Relationships xmlns="{PKG_REL}"><Relationship Id="rId1" Type="{REL}/styles" Target="styles.xml"/></Relationships>"#
            )
            .into_bytes(),
        ),
        ("word/styles.xml", w_part("styles", styles)),
    ];
    let mut zip = ZipWriter::new(Cursor::new(Vec::new()));
    let options = SimpleFileOptions::default().compression_method(CompressionMethod::Stored);
    for (name, bytes) in &parts {
        zip.start_file(*name, options).expect("start zip entry");
        zip.write_all(bytes).expect("write zip entry");
    }
    let bytes = zip.finish().expect("finish zip").into_inner();
    parse_docx_bytes(bytes, ParseOptions::default()).expect("document parses")
}

fn paragraphs(doc: &DocxDocument) -> Vec<&str> {
    doc.blocks
        .iter()
        .filter_map(|block| match block {
            Block::Paragraph(paragraph) => Some(paragraph.text.as_str()),
            _ => None,
        })
        .collect()
}

/// One paragraph holding one run in `fonts` (the `w:rFonts` attributes).
fn run(fonts: &str, text: &str) -> String {
    format!("<w:p><w:r><w:rPr><w:rFonts {fonts}/></w:rPr><w:t>{text}</w:t></w:r></w:p>")
}

fn sym(font: &str, code: &str) -> String {
    format!(r#"<w:p><w:r><w:sym w:font="{font}" w:char="{code}"/></w:r></w:p>"#)
}

#[test]
fn sym_elements_in_the_dingbat_fonts_read_as_unicode() {
    let body = [
        sym("Wingdings", "F0FC"),
        sym("Wingdings", "004A"),
        sym("wingdings", "F04A"),
        sym("Webdings", "F061"),
        sym("Wingdings 2", "F050"),
        sym("Wingdings 3", "F021"),
    ]
    .concat();
    assert_eq!(
        paragraphs(&parse(&body, "")),
        [
            "\u{2713}",
            "\u{263A}",
            "\u{263A}",
            "\u{2714}",
            "\u{1F5F8}",
            "\u{2B60}"
        ]
    );
}

#[test]
fn a_dingbat_code_without_a_unicode_character_keeps_what_is_stored() {
    // Wingdings 0xFF is the Windows logo, which Unicode does not encode, and
    // 0x26 has no settled mapping. The stored code is kept, so a consumer
    // that knows the font can still recover the glyph.
    let wingdings = r#"w:ascii="Wingdings" w:hAnsi="Wingdings""#;
    let body = [
        sym("Wingdings", "F0FF"),
        sym("Wingdings", "0026"),
        run(wingdings, "a\u{F0FF}\u{FF}b"),
        run(wingdings, "a&amp;\u{F026}b"),
    ]
    .concat();
    assert_eq!(
        paragraphs(&parse(&body, "")),
        [
            "\u{F0FF}",
            "&",
            "\u{264B}\u{F0FF}\u{FF}\u{264C}",
            "\u{264B}&\u{F026}\u{264C}"
        ]
    );
}

#[test]
fn dingbats_whose_name_was_reworded_before_publication_are_mapped() {
    let body = [
        sym("Webdings", "F0EC"),
        sym("Webdings", "F078"),
        sym("Wingdings", "F08A"),
        sym("Wingdings 2", "F073"),
        sym("Wingdings 3", "F04B"),
        run(r#"w:ascii="Webdings" w:hAnsi="Webdings""#, "B\u{FF}"),
    ]
    .concat();
    assert_eq!(
        paragraphs(&parse(&body, "")),
        [
            "\u{1F393}",
            "\u{29B8}",
            "\u{2789}",
            "\u{2469}",
            "\u{2B87}",
            "\u{1F3D8}\u{1F54A}"
        ]
    );
}

#[test]
fn run_text_in_a_dingbat_font_reads_as_unicode() {
    let wingdings = r#"w:ascii="Wingdings" w:hAnsi="Wingdings""#;
    let body = [
        // Stored as letters, which is what typing in the font produces.
        run(wingdings, "J"),
        // Stored in the Private Use Area, which is what Insert Symbol produces.
        run(wingdings, "\u{F04A}\u{F0FC}"),
        // Spaces, tabs and characters outside the font's 8-bit range stay.
        run(wingdings, "J J\u{2014}\u{65E5}"),
        run(r#"w:ascii="Webdings" w:hAnsi="Webdings""#, "a"),
        run(r#"w:ascii="Wingdings 2" w:hAnsi="Wingdings 2""#, "P"),
        run(r#"w:ascii="Wingdings 3" w:hAnsi="Wingdings 3""#, "!"),
    ]
    .concat();
    assert_eq!(
        paragraphs(&parse(&body, "")),
        [
            "\u{263A}",
            "\u{263A}\u{2713}",
            "\u{263A} \u{263A}\u{2014}\u{65E5}",
            "\u{2714}",
            "\u{1F5F8}",
            "\u{2B60}",
        ]
    );
}

#[test]
fn letters_are_mapped_only_when_the_font_applies_to_them() {
    let body = [
        // The complex-script font does not render Latin letters, so "J" is a
        // letter; a Private Use Area character has no other meaning.
        run(r#"w:ascii="Arial" w:cs="Wingdings""#, "J\u{F04A}"),
        // The ASCII font covers U+0000..=U+007F and the high-ANSI font the
        // rest of the 8-bit range.
        run(r#"w:ascii="Wingdings" w:hAnsi="Arial""#, "J\u{E9}"),
        run(r#"w:ascii="Arial" w:hAnsi="Wingdings""#, "J\u{FC}"),
        run(r#"w:ascii="Arial" w:hAnsi="Arial""#, "J\u{FC}"),
    ]
    .concat();
    assert_eq!(
        paragraphs(&parse(&body, "")),
        ["J\u{263A}", "\u{263A}\u{E9}", "J\u{2713}", "J\u{FC}"]
    );
}

#[test]
fn symbol_font_text_still_maps_only_its_private_use_characters() {
    let body = run(r#"w:ascii="Symbol" w:hAnsi="Symbol""#, "a\u{F061}");
    assert_eq!(paragraphs(&parse(&body, "")), ["a\u{3B1}"]);
}

/// A style named `id` whose run properties are `run_properties`.
fn style(kind: &str, id: &str, attributes: &str, based_on: Option<&str>, fonts: &str) -> String {
    let based_on = based_on
        .map(|parent| format!(r#"<w:basedOn w:val="{parent}"/>"#))
        .unwrap_or_default();
    let run_properties = if fonts.is_empty() {
        String::new()
    } else {
        format!("<w:rPr><w:rFonts {fonts}/></w:rPr>")
    };
    format!(
        r#"<w:style w:type="{kind}" w:styleId="{id}" {attributes}><w:name w:val="{id}"/>{based_on}{run_properties}</w:style>"#
    )
}

const SYMBOL: &str = r#"w:ascii="Symbol" w:hAnsi="Symbol""#;
const WINGDINGS: &str = r#"w:ascii="Wingdings" w:hAnsi="Wingdings""#;

#[test]
fn a_symbol_font_from_the_paragraph_style_is_applied() {
    let styles = style("paragraph", "Greek", "", None, SYMBOL);
    let body = concat!(
        r#"<w:p><w:pPr><w:pStyle w:val="Greek"/></w:pPr><w:r><w:t>"#,
        "\u{F061}\u{F062}</w:t></w:r></w:p>",
        "<w:p><w:r><w:t>\u{F061}</w:t></w:r></w:p>",
    );
    assert_eq!(
        paragraphs(&parse(body, &styles)),
        ["\u{3B1}\u{3B2}", "\u{F061}"]
    );
}

#[test]
fn a_symbol_font_from_the_character_style_chain_is_applied() {
    let styles = [
        style("character", "Base", "", None, WINGDINGS),
        style("character", "Middle", "", Some("Base"), ""),
        style("character", "Marks", "", Some("Middle"), ""),
    ]
    .concat();
    let body = concat!(
        r#"<w:p><w:r><w:rPr><w:rStyle w:val="Marks"/></w:rPr><w:t>J</w:t></w:r>"#,
        "<w:r><w:t>J</w:t></w:r></w:p>",
    );
    assert_eq!(paragraphs(&parse(body, &styles)), ["\u{263A}J"]);
}

#[test]
fn the_nearest_font_wins_over_an_inherited_symbol_font() {
    let styles = [
        style("paragraph", "Greek", "", None, SYMBOL),
        style(
            "paragraph",
            "Plain",
            "",
            Some("Greek"),
            r#"w:ascii="Arial" w:hAnsi="Arial""#,
        ),
        style("character", "Marks", "", None, WINGDINGS),
    ]
    .concat();
    let body = concat!(
        // The run's own font replaces the paragraph style's.
        r#"<w:p><w:pPr><w:pStyle w:val="Greek"/></w:pPr><w:r><w:rPr><w:rFonts w:ascii="Arial" w:hAnsi="Arial"/></w:rPr><w:t>"#,
        "\u{F061}</w:t></w:r></w:p>",
        // A style replaces the font of the style it is based on.
        r#"<w:p><w:pPr><w:pStyle w:val="Plain"/></w:pPr><w:r><w:t>"#,
        "\u{F061}</w:t></w:r></w:p>",
        // The character style replaces the paragraph style's font.
        r#"<w:p><w:pPr><w:pStyle w:val="Greek"/></w:pPr><w:r><w:rPr><w:rStyle w:val="Marks"/></w:rPr><w:t>"#,
        "\u{F04A}</w:t></w:r></w:p>",
    );
    assert_eq!(
        paragraphs(&parse(body, &styles)),
        ["\u{F061}", "\u{F061}", "\u{263A}"]
    );
}

#[test]
fn the_default_paragraph_style_applies_to_a_paragraph_without_a_style() {
    let styles = [
        style("paragraph", "Normal", r#"w:default="1""#, None, WINGDINGS),
        style(
            "paragraph",
            "Body",
            "",
            None,
            r#"w:ascii="Arial" w:hAnsi="Arial""#,
        ),
    ]
    .concat();
    let body = concat!(
        "<w:p><w:r><w:t>J</w:t></w:r></w:p>",
        r#"<w:p><w:pPr><w:pStyle w:val="Body"/></w:pPr><w:r><w:t>J</w:t></w:r></w:p>"#,
    );
    assert_eq!(paragraphs(&parse(body, &styles)), ["\u{263A}", "J"]);
}

#[test]
fn styles_based_on_each_other_in_a_circle_do_not_hang() {
    let styles = [
        style("paragraph", "A", "", Some("B"), ""),
        style("paragraph", "B", "", Some("C"), SYMBOL),
        style("paragraph", "C", "", Some("A"), ""),
    ]
    .concat();
    let body = concat!(
        r#"<w:p><w:pPr><w:pStyle w:val="A"/></w:pPr><w:r><w:t>"#,
        "\u{F061}</w:t></w:r></w:p>",
    );
    assert_eq!(paragraphs(&parse(body, &styles)), ["\u{3B1}"]);
}

#[test]
fn a_font_for_part_of_a_table_style_is_not_the_style_font() {
    let styles = r#"<w:style w:type="table" w:styleId="Grid"><w:name w:val="Grid"/><w:tblStylePr w:type="firstRow"><w:rPr><w:rFonts w:ascii="Symbol" w:hAnsi="Symbol"/></w:rPr></w:tblStylePr></w:style>"#;
    let body = concat!(
        r#"<w:p><w:pPr><w:pStyle w:val="Grid"/></w:pPr><w:r><w:t>"#,
        "\u{F061}</w:t></w:r></w:p>",
    );
    assert_eq!(paragraphs(&parse(body, styles)), ["\u{F061}"]);
}

#[test]
fn the_old_properties_of_a_tracked_format_change_are_not_the_run_font() {
    let styles = [
        style("paragraph", "Greek", "", None, SYMBOL),
        style("character", "Marks", "", None, WINGDINGS),
    ]
    .concat();
    let change = |old: &str| {
        format!(r#"<w:rPrChange w:id="1" w:author="a"><w:rPr>{old}</w:rPr></w:rPrChange>"#)
    };
    let body = [
        // Now Arial, formerly Wingdings: ordinary text.
        format!(
            r#"<w:p><w:r><w:rPr><w:rFonts w:ascii="Arial" w:hAnsi="Arial"/>{}</w:rPr><w:t>Hello</w:t></w:r></w:p>"#,
            change(&format!("<w:rFonts {WINGDINGS}/>"))
        ),
        // Now Wingdings, formerly Arial: symbols.
        format!(
            r#"<w:p><w:r><w:rPr><w:rFonts {WINGDINGS}/>{}</w:rPr><w:t>J</w:t></w:r></w:p>"#,
            change(r#"<w:rFonts w:ascii="Arial" w:hAnsi="Arial"/>"#)
        ),
        // Formerly in a Wingdings character style, now in none.
        format!(
            r#"<w:p><w:r><w:rPr>{}</w:rPr><w:t>J</w:t></w:r></w:p>"#,
            change(r#"<w:rStyle w:val="Marks"/>"#)
        ),
        // The paragraph was formerly in a Symbol style, now in none.
        concat!(
            r#"<w:p><w:pPr><w:pPrChange w:id="2" w:author="a"><w:pPr><w:pStyle w:val="Greek"/></w:pPr></w:pPrChange></w:pPr><w:r><w:t>"#,
            "\u{F061}</w:t></w:r></w:p>"
        )
        .to_string(),
        // The paragraph is in the Symbol style now, formerly in none.
        concat!(
            r#"<w:p><w:pPr><w:pStyle w:val="Greek"/><w:pPrChange w:id="3" w:author="a"><w:pPr/></w:pPrChange></w:pPr><w:r><w:t>"#,
            "\u{F061}</w:t></w:r></w:p>"
        )
        .to_string(),
    ]
    .concat();
    let doc = parse(&body, &styles);
    assert_eq!(
        paragraphs(&doc),
        ["Hello", "\u{263A}", "J", "\u{F061}", "\u{3B1}"]
    );
    let Block::Paragraph(fourth) = &doc.blocks[3] else {
        panic!("expected a paragraph");
    };
    assert_eq!(fourth.style_id, None);
}

#[test]
fn a_theme_font_is_an_ordinary_font() {
    let theme = r#"w:asciiTheme="minorHAnsi" w:hAnsiTheme="minorHAnsi" w:cstheme="minorBidi""#;
    let styles = [
        style("paragraph", "Marks", "", None, WINGDINGS),
        // A style whose theme font replaces the font of the style it is
        // based on.
        style("paragraph", "Body", "", Some("Marks"), theme),
    ]
    .concat();
    let body = [
        // The run's theme font replaces the paragraph style's Wingdings.
        format!(
            r#"<w:p><w:pPr><w:pStyle w:val="Marks"/></w:pPr><w:r><w:rPr><w:rFonts {theme}/></w:rPr><w:t>Hello {pua}</w:t></w:r></w:p>"#,
            pua = '\u{F04A}'
        ),
        // A theme font wins over a named font in the same slot.
        format!(
            r#"<w:p><w:r><w:rPr><w:rFonts {WINGDINGS} {theme}/></w:rPr><w:t>Hello</w:t></w:r></w:p>"#
        ),
        r#"<w:p><w:pPr><w:pStyle w:val="Body"/></w:pPr><w:r><w:t>Hello</w:t></w:r></w:p>"#
            .to_string(),
        // Only the ASCII slot is a theme font; the high-ANSI slot inherits.
        format!(
            r#"<w:p><w:pPr><w:pStyle w:val="Marks"/></w:pPr><w:r><w:rPr><w:rFonts w:asciiTheme="minorHAnsi"/></w:rPr><w:t>J{high}</w:t></w:r></w:p>"#,
            high = '\u{FC}'
        ),
    ]
    .concat();
    assert_eq!(
        paragraphs(&parse(&body, &styles)),
        ["Hello \u{F04A}", "Hello", "Hello", "J\u{2713}"]
    );
}
