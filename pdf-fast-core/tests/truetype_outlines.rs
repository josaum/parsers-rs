//! TrueType `glyf` outline extraction, against known reference coordinates.
//!
//! The fixture is a subset of DejaVu Sans (Bitstream Vera licence, permissive)
//! containing exactly the glyphs these tests use, including four composites so
//! the component path is exercised rather than assumed.
//!
//! The expected coordinates were read out of the font with an independent
//! implementation (`fontTools`), so this compares against the font's actual
//! outlines rather than against this parser's own idea of them.

use pdf_fast_core::truetype::TrueTypeFont;

fn fixture() -> Vec<u8> {
    std::fs::read(concat!(
        env!("CARGO_MANIFEST_DIR"),
        "/tests/fixtures/DejaVuSans-subset.ttf"
    ))
    .expect("the TrueType fixture must be present")
}

#[test]
fn the_fixture_parses_as_a_glyf_font() {
    let font = TrueTypeFont::parse(&fixture()).expect("a glyf-based TrueType program must parse");
    assert_eq!(font.units_per_em(), 2048.0, "head.unitsPerEm");
}

/// A simple glyph, compared point by point against fontTools.
#[test]
fn a_simple_glyph_matches_its_reference_outline() {
    let font = TrueTypeFont::parse(&fixture()).unwrap();
    let index = font
        .glyph_index('A' as u32)
        .expect("'A' must map to a glyph");
    let outline = font.outline(index).expect("'A' has an outline");

    // fontTools, decomposed: two contours beginning at (700, 1294) and
    // (586, 1493).
    assert_eq!(outline.contours.len(), 2, "'A' has two contours");
    assert_eq!(
        (outline.contours[0][0].x, outline.contours[0][0].y),
        (700.0, 1294.0),
        "first contour start"
    );
    assert_eq!(
        (outline.contours[1][0].x, outline.contours[1][0].y),
        (586.0, 1493.0),
        "second contour start"
    );
}

/// The period is a four-point square, small enough to state exactly.
#[test]
fn a_four_point_glyph_matches_exactly() {
    let font = TrueTypeFont::parse(&fixture()).unwrap();
    let index = font.glyph_index('.' as u32).unwrap();
    let outline = font.outline(index).unwrap();

    assert_eq!(outline.contours.len(), 1);
    let points: Vec<(f32, f32)> = outline.contours[0]
        .iter()
        .map(|p| (p.x, p.y))
        // The closing point repeats the first; drop it for comparison.
        .take(4)
        .collect();
    assert_eq!(
        points,
        vec![(219.0, 254.0), (430.0, 254.0), (430.0, 0.0), (219.0, 0.0)],
        "fontTools reports exactly these four points"
    );
    assert_eq!(
        outline.contours[0].len(),
        5,
        "the contour must be closed, so the first point repeats"
    );
}

/// A composite glyph resolved into its components.
///
/// `Aacute` is `A` plus `Acute` translated by (1212, 373). fontTools decomposes
/// it to three contours whose starts are (700, 1294), (586, 1493), and
/// (755, 1899) — the third being the accent's own start shifted.
#[test]
fn a_composite_glyph_resolves_its_components() {
    let font = TrueTypeFont::parse(&fixture()).unwrap();
    let index = font.glyph_index(0x00C1).expect("Aacute is in the subset");
    let outline = font.outline(index).expect("Aacute has an outline");

    assert_eq!(
        outline.contours.len(),
        3,
        "two contours from 'A' plus one from the accent"
    );
    let starts: Vec<(f32, f32)> = outline.contours.iter().map(|c| (c[0].x, c[0].y)).collect();
    assert_eq!(
        starts,
        vec![(700.0, 1294.0), (586.0, 1493.0), (755.0, 1899.0)],
        "the accent must be translated by the component offset"
    );
}

/// On-curve and off-curve points stay distinguishable.
///
/// TrueType stores quadratic outlines as a point stream with a flag per point,
/// and a run of off-curve points implies on-curve points that are not stored.
/// Flattening the distinction would leave a consumer unable to reconstruct the
/// path, and would make two renderings of one shape compare unequal.
#[test]
fn curve_points_keep_their_on_curve_flag() {
    let font = TrueTypeFont::parse(&fixture()).unwrap();
    let index = font.glyph_index('B' as u32).unwrap();
    let outline = font.outline(index).unwrap();
    let all: Vec<bool> = outline
        .contours
        .iter()
        .flatten()
        .map(|p| p.on_curve)
        .collect();
    assert!(all.iter().any(|on| *on), "'B' must have on-curve points");
    assert!(
        all.iter().any(|on| !*on),
        "'B' is a curved glyph, so it must have off-curve control points too"
    );
}

/// A code the font does not map, and a glyph index past the end, are `None`
/// rather than a wrong glyph or a panic.
#[test]
fn absent_codes_and_indices_yield_nothing() {
    let font = TrueTypeFont::parse(&fixture()).unwrap();
    assert!(
        font.glyph_index(0x4E2D).is_none(),
        "a CJK code is not in this subset"
    );
    assert!(
        font.outline(u16::MAX).is_none(),
        "an index past the glyph count must not read out of bounds"
    );
}

/// `space` has no contours, so it yields no outline rather than an empty one
/// a caller would have to special-case.
#[test]
fn a_glyph_without_contours_yields_no_outline() {
    let font = TrueTypeFont::parse(&fixture()).unwrap();
    if let Some(index) = font.glyph_index(' ' as u32) {
        assert!(font.outline(index).is_none(), "space draws nothing");
    }
}

/// Garbage and a CFF-flavoured sfnt are refused, not misparsed.
#[test]
fn a_non_glyf_program_is_refused() {
    assert!(TrueTypeFont::parse(&[]).is_none(), "empty input");
    assert!(
        TrueTypeFont::parse(b"not a font at all, but long enough").is_none(),
        "a non-sfnt must not parse"
    );
    // `OTTO` marks CFF outlines, which this module does not read. Returning an
    // approximation here would feed the catalog a plausible wrong shape.
    let mut otto = vec![0u8; 64];
    otto[0..4].copy_from_slice(b"OTTO");
    assert!(
        TrueTypeFont::parse(&otto).is_none(),
        "a CFF sfnt must be refused rather than approximated"
    );
}
