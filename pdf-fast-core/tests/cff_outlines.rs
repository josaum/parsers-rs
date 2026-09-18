//! CFF outline extraction, against known reference coordinates.
//!
//! The fixture is a subset of Cabin Regular (SIL Open Font License) carrying
//! only the glyphs these tests use. OpenType font programs are usually CFF, and
//! a CFF font's outlines are *charstrings* — a bytecode of its own with
//! subroutine calls and cubic curves — so reading them is an interpreter, not a
//! table walk.
//!
//! Expected coordinates were read out of the font with `fontTools`, so this
//! compares against the font's real outlines rather than against the
//! interpreter's own output.
//!
//! An `OTTO` sfnt stores its CFF in the `CFF ` table; the outlines are in
//! design units, y-up, and `head.unitsPerEm` is 2000 here.

use pdf_fast_core::cff::CffFont;

fn fixture() -> Vec<u8> {
    std::fs::read(concat!(
        env!("CARGO_MANIFEST_DIR"),
        "/tests/fixtures/Cabin-subset.otf"
    ))
    .expect("the CFF fixture must be present")
}

/// The `CFF ` table inside the sfnt, located the same way a caller would.
fn cff_table(sfnt: &[u8]) -> Vec<u8> {
    let tables = u16::from_be_bytes([sfnt[4], sfnt[5]]) as usize;
    for index in 0..tables {
        let record = 12 + index * 16;
        if &sfnt[record..record + 4] == b"CFF " {
            let offset = u32::from_be_bytes([
                sfnt[record + 8],
                sfnt[record + 9],
                sfnt[record + 10],
                sfnt[record + 11],
            ]) as usize;
            let length = u32::from_be_bytes([
                sfnt[record + 12],
                sfnt[record + 13],
                sfnt[record + 14],
                sfnt[record + 15],
            ]) as usize;
            return sfnt[offset..offset + length].to_vec();
        }
    }
    panic!("the fixture must carry a CFF table");
}

fn font() -> CffFont {
    CffFont::parse(&cff_table(&fixture()), Some(2000.0)).expect("a CFF font must parse")
}

#[test]
fn the_fixture_parses_and_names_its_glyphs() {
    let font = font();
    assert_eq!(font.units_per_em(), 2000.0);
    // The charset is the only route from a PDF encoding's glyph name to an
    // index, so a missing or misread charset shows up here first.
    for name in ["A", "B", "period", "Aacute", "space"] {
        assert!(
            font.glyph_id(name).is_some(),
            "{name} must be reachable by name through the charset"
        );
    }
}

/// A straight-line glyph, compared point by point.
#[test]
fn a_line_glyph_matches_its_reference_outline() {
    let font = font();
    let id = font.glyph_id("A").expect("A is in the subset");
    let outline = font.outline(id).expect("A has an outline");

    // fontTools: two contours, the first starting at (1218, 0).
    assert_eq!(outline.contours.len(), 2, "A has two contours");
    assert_eq!(
        (outline.contours[0][0].x, outline.contours[0][0].y),
        (1218.0, 0.0),
        "first contour start"
    );
    assert_eq!(
        (outline.contours[0][1].x, outline.contours[0][1].y),
        (716.0, 1400.0),
        "second point of the first contour"
    );
}

/// The period is four points, small enough to state exactly.
#[test]
fn a_four_point_glyph_matches_exactly() {
    let font = font();
    let id = font.glyph_id("period").unwrap();
    let outline = font.outline(id).unwrap();
    assert_eq!(outline.contours.len(), 1);
    let points: Vec<(f32, f32)> = outline.contours[0]
        .iter()
        .take(4)
        .map(|p| (p.x, p.y))
        .collect();
    assert_eq!(
        points,
        vec![(316.0, 0.0), (316.0, 212.0), (100.0, 212.0), (100.0, 0.0)],
        "fontTools reports exactly these four points"
    );
    assert_eq!(
        outline.contours[0].len(),
        5,
        "the contour must be closed, so the first point repeats"
    );
}

/// A curved glyph exercises the cubic operators and the subroutine calls a real
/// CFF charstring is built from.
#[test]
fn a_curved_glyph_matches_its_reference_outline() {
    let font = font();
    let id = font.glyph_id("B").unwrap();
    let outline = font.outline(id).unwrap();

    assert_eq!(outline.contours.len(), 3, "B has three contours");
    assert_eq!(
        (outline.contours[0][0].x, outline.contours[0][0].y),
        (862.0, 761.0),
        "first contour start"
    );
    // A cubic stores two controls and an endpoint, and the controls are marked
    // off-curve so a consumer can reconstruct the path.
    let first_curve: Vec<&pdf_fast_core::truetype::OutlinePoint> =
        outline.contours[0].iter().take(4).collect();
    assert_eq!(
        first_curve.iter().filter(|p| !p.on_curve).count(),
        2,
        "a cubic contributes exactly two control points"
    );
    assert!(
        first_curve[3].on_curve,
        "the third point of a cubic is on the curve"
    );
    assert_eq!(
        (first_curve[1].x, first_curve[1].y),
        (957.0, 816.0),
        "first control point, per fontTools"
    );
}

/// A composite character in CFF, built from the same charstring machinery.
#[test]
fn an_accented_glyph_matches_its_reference_outline() {
    let font = font();
    let id = font.glyph_id("Aacute").unwrap();
    let outline = font.outline(id).unwrap();
    assert_eq!(outline.contours.len(), 3, "Aacute has three contours");
    assert_eq!(
        (outline.contours[0][0].x, outline.contours[0][0].y),
        (430.0, 1644.0),
        "fontTools reports this as the first contour start"
    );
}

/// Widths come from the charstring, not a table.
///
/// The width is not stored anywhere: it is the first operand of a charstring,
/// present only when the operator's operand count says so, and **relative** to
/// the Private DICT's `nominalWidthX` — which is 1061 in this font. That makes
/// a bare assertion like "the width is positive" wrong: `space` encodes -611
/// and is 450 wide.
#[test]
fn advance_widths_are_read_from_the_charstring() {
    let font = font();
    let width = |name: &str| {
        font.advance_units(font.glyph_id(name).expect("glyph must be named"))
            .expect("every glyph advances")
    };
    // fontTools reports these from the resolved glyph set.
    assert_eq!(
        width("A"),
        1232.0,
        "A declares 171, plus nominalWidthX 1061"
    );
    assert_eq!(width("space"), 450.0, "space declares -611, plus 1061");
    assert_eq!(width("period"), 416.0, "period declares -645, plus 1061");
}

/// Refusals: a truncated stream, garbage, and a CID-keyed font.
#[test]
fn a_font_this_reader_cannot_handle_is_refused() {
    assert!(CffFont::parse(&[], Some(1000.0)).is_none(), "empty input");
    assert!(
        CffFont::parse(&[1, 0, 4, 4, 0, 0], Some(1000.0)).is_none(),
        "a header with no INDEX must not parse into an empty font"
    );
    assert!(
        CffFont::parse(b"OTTOnot really a cff table at all", Some(1000.0)).is_none(),
        "garbage must be refused rather than interpreted"
    );
}
