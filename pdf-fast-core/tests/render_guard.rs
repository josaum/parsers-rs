//! The interpreted renderer must refuse content that never stops nesting.
//!
//! The full interpreter enters another content stream from a Form XObject
//! `Do`, an annotation appearance, a tiling pattern, a Type 3 glyph procedure
//! and a soft-mask group, and guards none of them. A stream that reaches
//! itself, or a long enough chain of distinct streams, overflows the stack
//! and aborts the process. Every refusal test here aborts the test binary if
//! the guard in front of the interpreter stops working.
//!
//! `probe_interpreter_stack` drives the interpreter with no guard at all. It
//! is how each fixture was confirmed to abort and how the stack cost of one
//! nesting level was measured; see its comment for how to run it.

#![cfg(feature = "rasterize")]

use pdf_fast_core::{ParseOptions, PdfDocument};

/// Builds a classic cross-reference PDF from `(object number, body)` pairs.
/// The catalog is object 1.
fn pdf(objects: &[(u32, Vec<u8>)]) -> Vec<u8> {
    let mut out = b"%PDF-1.7\n%\xE2\xE3\xCF\xD3\n".to_vec();
    let max = objects.iter().map(|(number, _)| *number).max().unwrap_or(0);
    let mut offsets = vec![None; max as usize + 1];
    for (number, body) in objects {
        offsets[*number as usize] = Some(out.len());
        out.extend_from_slice(format!("{number} 0 obj\n").as_bytes());
        out.extend_from_slice(body);
        out.extend_from_slice(b"\nendobj\n");
    }
    let xref = out.len();
    out.extend_from_slice(format!("xref\n0 {}\n", max + 1).as_bytes());
    for (number, offset) in offsets.iter().enumerate() {
        match offset {
            Some(offset) if number > 0 => {
                out.extend_from_slice(format!("{offset:010} 00000 n \n").as_bytes());
            }
            _ => out.extend_from_slice(b"0000000000 65535 f \n"),
        }
    }
    out.extend_from_slice(
        format!(
            "trailer\n<< /Size {} /Root 1 0 R >>\nstartxref\n{xref}\n%%EOF\n",
            max + 1
        )
        .as_bytes(),
    );
    out
}

fn stream(dict: &str, body: &str) -> Vec<u8> {
    format!(
        "<< {dict} /Length {} >>\nstream\n{body}\nendstream",
        body.len()
    )
    .into_bytes()
}

fn form(extra: &str, body: &str) -> Vec<u8> {
    stream(
        &format!("/Type /XObject /Subtype /Form /BBox [0 0 100 100] {extra}"),
        body,
    )
}

/// One 100 x 100 page (object 3, content 4) plus `rest`.
fn one_page(page_extra: &str, content: &str, rest: Vec<(u32, Vec<u8>)>) -> Vec<u8> {
    let mut objects = vec![
        (1u32, b"<< /Type /Catalog /Pages 2 0 R >>".to_vec()),
        (2, b"<< /Type /Pages /Kids [3 0 R] /Count 1 >>".to_vec()),
        (
            3,
            format!(
                "<< /Type /Page /Parent 2 0 R /MediaBox [0 0 100 100] /Contents 4 0 R \
                 {page_extra} >>"
            )
            .into_bytes(),
        ),
        (4, stream("", content)),
    ];
    objects.extend(rest);
    pdf(&objects)
}

const PAGE_DRAWS_F1: &str = "/Resources << /XObject << /F1 10 0 R >> >>";

/// The four single-form documents: the page draws `/F1`, which is form 10.
fn self_drawing_form(case: usize) -> Vec<u8> {
    let (resources, body) = match case {
        // Lists itself in its own resources and paints.
        1 => (
            "/Resources << /XObject << /F1 10 0 R >> >>",
            "0 0 10 10 re f /F1 Do",
        ),
        // No resources at all, paints nothing.
        2 => ("", "/F1 Do"),
        // Empty resources: `/F1` is found through the page. Paints.
        3 => ("/Resources << >>", "0 0 10 10 re f /F1 Do"),
        // As 3, painting nothing, so the fast rasterizer falls through.
        _ => ("/Resources << >>", "/F1 Do"),
    };
    one_page(PAGE_DRAWS_F1, "/F1 Do", vec![(10, form(resources, body))])
}

fn two_form_cycle() -> Vec<u8> {
    one_page(
        PAGE_DRAWS_F1,
        "/F1 Do",
        vec![
            (
                10,
                form("/Resources << /XObject << /F2 11 0 R >> >>", "/F2 Do"),
            ),
            (
                11,
                form("/Resources << /XObject << /F1 10 0 R >> >>", "/F1 Do"),
            ),
        ],
    )
}

/// Three forms with empty resources; every name resolves through the page,
/// two resource levels above the form that closes the cycle.
fn inherited_cycle() -> Vec<u8> {
    one_page(
        "/Resources << /XObject << /F1 10 0 R /F2 11 0 R /F3 12 0 R >> >>",
        "/F1 Do",
        vec![
            (10, form("/Resources << >>", "/F2 Do")),
            (11, form("/Resources << >>", "/F3 Do")),
            (12, form("/Resources << >>", "/F1 Do")),
        ],
    )
}

/// A page that draws a chain of `depth` distinct nested forms.
fn form_chain(depth: usize) -> Vec<u8> {
    let mut rest = Vec::new();
    for level in 1..=depth {
        let number = 9 + u32::try_from(level).expect("small depth");
        let (resources, body) = if level < depth {
            (
                format!(
                    "/Resources << /XObject << /F{} {} 0 R >> >>",
                    level + 1,
                    number + 1
                ),
                format!("0 0 5 5 re f /F{} Do", level + 1),
            )
        } else {
            (
                "/Resources << >>".to_string(),
                "60 60 20 20 re f".to_string(),
            )
        };
        rest.push((number, form(&resources, &body)));
    }
    one_page(PAGE_DRAWS_F1, "0 0 50 50 re f /F1 Do", rest)
}

const ANNOTATION: &str = "/Annots [ << /Type /Annot /Subtype /Square /Rect [10 10 60 60] \
                          /AP << /N 10 0 R >> >> ]";

/// An annotation whose appearance stream draws `chain` nested forms; with
/// `chain == 0` the appearance draws itself, finding its own name through the
/// page's resources. (Listing itself in its own resources is not enough: the
/// interpreter's object model will not follow a reference back into an
/// object it is already inside, so that lookup comes back empty.)
fn annotation_appearance(chain: usize) -> Vec<u8> {
    let mut rest = Vec::new();
    if chain == 0 {
        rest.push((10, form("/Resources << >>", "/A Do")));
        return one_page(
            &format!("{ANNOTATION} /Resources << /XObject << /A 10 0 R >> >>"),
            "0 0 10 10 re f",
            rest,
        );
    }
    for level in 1..=chain {
        let number = 9 + u32::try_from(level).expect("small depth");
        let (resources, body) = if level < chain {
            (
                format!("/Resources << /XObject << /A {} 0 R >> >>", number + 1),
                "0 0 5 5 re f /A Do".to_string(),
            )
        } else {
            ("/Resources << >>".to_string(), "0 0 50 50 re f".to_string())
        };
        rest.push((number, form(&resources, &body)));
    }
    one_page(ANNOTATION, "0 0 10 10 re f", rest)
}

fn tiling(extra: &str, body: &str) -> Vec<u8> {
    stream(
        &format!(
            "/Type /Pattern /PatternType 1 /PaintType 1 /TilingType 1 /BBox [0 0 10 10] \
             /XStep 10 /YStep 10 {extra}"
        ),
        body,
    )
}

/// A page filled with a tiling pattern whose cell fills with the next
/// pattern, `chain` cells deep; with `chain == 0` the cell fills with itself.
fn tiling_pattern(chain: usize) -> Vec<u8> {
    let mut rest = Vec::new();
    if chain == 0 {
        rest.push((
            10,
            tiling(
                "/Resources << /Pattern << /P 10 0 R >> >>",
                "/Pattern cs /P scn 0 0 8 8 re f",
            ),
        ));
    }
    for level in 1..=chain {
        let number = 9 + u32::try_from(level).expect("small depth");
        let (resources, body) = if level < chain {
            (
                format!("/Resources << /Pattern << /P {} 0 R >> >>", number + 1),
                "/Pattern cs /P scn 0 0 8 8 re f",
            )
        } else {
            ("/Resources << >>".to_string(), "0 0 8 8 re f")
        };
        rest.push((number, tiling(&resources, body)));
    }
    one_page(
        "/Resources << /Pattern << /P 10 0 R >> >>",
        "/Pattern cs /P scn 0 0 100 100 re f",
        rest,
    )
}

fn type3_font(resources: &str, glyph: u32) -> Vec<u8> {
    format!(
        "<< /Type /Font /Subtype /Type3 /FontBBox [0 0 10 10] /FontMatrix [0.1 0 0 0.1 0 0] \
         /CharProcs << /a {glyph} 0 R >> /Encoding << /Type /Encoding /Differences [97 /a] >> \
         /FirstChar 97 /LastChar 97 /Widths [10] {resources} >>"
    )
    .into_bytes()
}

/// A page showing one Type 3 glyph whose procedure shows a glyph of the next
/// Type 3 font, `chain` fonts deep; with `chain == 0` the glyph shows itself.
/// Font `n` is object `10 + 2n`, its glyph procedure the object after it.
fn type3_glyph(chain: usize) -> Vec<u8> {
    let mut rest = Vec::new();
    if chain == 0 {
        rest.push((10, type3_font("/Resources << /Font << /T 10 0 R >> >>", 11)));
        rest.push((11, stream("", "10 0 d0 BT /T 10 Tf (a) Tj ET")));
    }
    for level in 1..=chain {
        let number = 8 + 2 * u32::try_from(level).expect("small depth");
        if level < chain {
            rest.push((
                number,
                type3_font(
                    &format!("/Resources << /Font << /T {} 0 R >> >>", number + 2),
                    number + 1,
                ),
            ));
            rest.push((
                number + 1,
                stream("", "10 0 d0 0 0 2 2 re f BT /T 10 Tf (a) Tj ET"),
            ));
        } else {
            rest.push((number, type3_font("/Resources << >>", number + 1)));
            rest.push((number + 1, stream("", "10 0 d0 0 0 8 8 re f")));
        }
    }
    one_page(
        "/Resources << /Font << /T 10 0 R >> >>",
        "BT /T 10 Tf 10 50 Td (a) Tj ET",
        rest,
    )
}

/// A page painting through a soft mask whose group paints through the next
/// soft mask, `chain` groups deep; with `chain == 0` the group masks itself.
/// Graphics state `n` is object `10 + 2n`, its group the object after it.
fn soft_mask(chain: usize) -> Vec<u8> {
    let state = |group: u32| {
        format!("<< /Type /ExtGState /SMask << /Type /Mask /S /Luminosity /G {group} 0 R >> >>")
            .into_bytes()
    };
    let group = |resources: &str, body: &str| {
        form(
            &format!("/Group << /S /Transparency /CS /DeviceGray >> {resources}"),
            body,
        )
    };
    let mut rest = Vec::new();
    if chain == 0 {
        rest.push((10, state(11)));
        rest.push((
            11,
            group(
                "/Resources << /ExtGState << /G 10 0 R >> >>",
                "/G gs 1 g 0 0 50 50 re f",
            ),
        ));
    }
    for level in 1..=chain {
        let number = 8 + 2 * u32::try_from(level).expect("small depth");
        rest.push((number, state(number + 1)));
        if level < chain {
            rest.push((
                number + 1,
                group(
                    &format!("/Resources << /ExtGState << /G {} 0 R >> >>", number + 2),
                    "/G gs 1 g 0 0 50 50 re f",
                ),
            ));
        } else {
            rest.push((number + 1, group("/Resources << >>", "1 g 0 0 50 50 re f")));
        }
    }
    one_page(
        "/Resources << /ExtGState << /G 10 0 R >> >>",
        "/G gs 0 0 100 100 re f",
        rest,
    )
}

/// Three `/Kids` entries, two of them the same page. Object 3 is harmless;
/// object 5 draws a form that draws itself through the page's resources.
fn repeated_kids() -> Vec<u8> {
    pdf(&[
        (1u32, b"<< /Type /Catalog /Pages 2 0 R >>".to_vec()),
        (
            2,
            b"<< /Type /Pages /Kids [3 0 R 3 0 R 5 0 R] /Count 3 >>".to_vec(),
        ),
        (
            3,
            b"<< /Type /Page /Parent 2 0 R /MediaBox [0 0 100 100] /Contents 4 0 R >>".to_vec(),
        ),
        (4, stream("", "0 0 10 10 re f")),
        (
            5,
            b"<< /Type /Page /Parent 2 0 R /MediaBox [0 0 100 100] /Contents 6 0 R \
              /Resources << /XObject << /F1 10 0 R >> >> >>"
                .to_vec(),
        ),
        (6, stream("", "/F1 Do")),
        (10, form("/Resources << >>", "/F1 Do")),
    ])
}

/// A form that decodes to different content depending on the path it is
/// reached by. Its `/DecodeParms` is object 12, which is also the `/XObject`
/// dictionary of form 11. Drawn from the page, the predictor in object 12
/// applies and the form decodes to nothing drawable. Drawn from form 11
/// through object 12, the interpreter will not follow the reference back
/// into object 12, the predictor is dropped, and the form decodes to
/// `/S2 Do`: itself, found through form 11's resources.
fn form_that_decodes_by_path() -> Vec<u8> {
    use std::io::Write;
    let mut encoder = flate2::write::ZlibEncoder::new(Vec::new(), flate2::Compression::default());
    encoder.write_all(b"/S2 Do\n").expect("compress");
    let hostile = encoder.finish().expect("finish compression");
    let mut form_10 = format!(
        "<< /Type /XObject /Subtype /Form /BBox [0 0 100 100] /Filter /FlateDecode \
         /DecodeParms 12 0 R /Length {} >>\nstream\n",
        hostile.len()
    )
    .into_bytes();
    form_10.extend_from_slice(&hostile);
    form_10.extend_from_slice(b"\nendstream");
    one_page(
        "/Resources << /XObject << /S1 10 0 R /G 11 0 R >> >>",
        "/S1 Do /G Do",
        vec![
            (10, form_10),
            (11, form("/Resources << /XObject 12 0 R >>", "/S2 Do")),
            (
                12,
                b"<< /S2 10 0 R /Predictor 2 /Columns 7 /Colors 1 /BitsPerComponent 8 >>".to_vec(),
            ),
        ],
    )
}

/// A page that draws `count` distinct names, none of which exists.
fn many_names(count: usize) -> Vec<u8> {
    let content = (0..count)
        .map(|index| format!("/X{index:07} Do"))
        .collect::<Vec<_>>()
        .join(" ");
    one_page("", &content, Vec::new())
}

/// A page under `depth` nested `/Pages` nodes.
fn deep_page_tree(depth: usize) -> Vec<u8> {
    let mut objects = vec![(1u32, b"<< /Type /Catalog /Pages 10 0 R >>".to_vec())];
    let depth = u32::try_from(depth).expect("small depth");
    for level in 0..depth {
        objects.push((
            10 + level,
            format!("<< /Type /Pages /Kids [{} 0 R] /Count 1 >>", 11 + level).into_bytes(),
        ));
    }
    objects.push((
        10 + depth,
        format!(
            "<< /Type /Page /Parent {} 0 R /MediaBox [0 0 100 100] /Contents 4 0 R >>",
            9 + depth
        )
        .into_bytes(),
    ));
    objects.push((4, stream("", "0 0 10 10 re f")));
    pdf(&objects)
}

const SHADED_PAGE: &str = "/Resources << /Shading << /S << /ShadingType 2 /ColorSpace \
                           /DeviceGray /Coords [0 0 100 100] /Function 10 0 R >> >> >>";

/// A shading whose function is `depth` stitching functions, each holding
/// the next.
fn nested_stitching_functions(depth: usize) -> Vec<u8> {
    let depth = u32::try_from(depth).expect("small depth");
    let mut rest = Vec::new();
    for level in 0..depth {
        rest.push((
            10 + level,
            format!(
                "<< /FunctionType 3 /Domain [0 1] /Functions [{} 0 R] /Bounds [] \
                 /Encode [0 1] >>",
                11 + level
            )
            .into_bytes(),
        ));
    }
    rest.push((
        10 + depth,
        b"<< /FunctionType 2 /Domain [0 1] /C0 [0] /C1 [1] /N 1 >>".to_vec(),
    ));
    one_page(SHADED_PAGE, "/S sh", rest)
}

/// A colour space that is `depth` `/Indexed` (or `/Separation`) spaces,
/// each based on the next.
fn nested_colour_spaces(depth: usize, separation: bool) -> Vec<u8> {
    let depth = u32::try_from(depth).expect("small depth");
    let mut rest = Vec::new();
    for level in 0..depth {
        let body = if separation {
            format!(
                "[/Separation /X {} 0 R << /FunctionType 2 /Domain [0 1] /C0 [0] /C1 [1] \
                 /N 1 >>]",
                11 + level
            )
        } else {
            format!("[/Indexed {} 0 R 0 <00>]", 11 + level)
        };
        rest.push((10 + level, body.into_bytes()));
    }
    rest.push((10 + depth, b"[/CalGray << /WhitePoint [1 1 1] >>]".to_vec()));
    one_page(
        "/Resources << /ColorSpace << /C 10 0 R >> >>",
        "/C cs 0 sc 0 0 10 10 re f",
        rest,
    )
}

/// A shading whose function is a PostScript calculator with `depth` nested
/// procedures.
fn nested_calculator_procedures(depth: usize) -> Vec<u8> {
    let program = format!("{}1 {}", "{ ".repeat(depth), "} ".repeat(depth));
    one_page(
        SHADED_PAGE,
        "/S sh",
        vec![(
            10,
            stream("/FunctionType 4 /Domain [0 1] /Range [0 1]", &program),
        )],
    )
}

/// A page whose content holds `depth` nested arrays.
fn nested_content_arrays(depth: usize) -> Vec<u8> {
    let content = format!(
        "BT {}{} TJ ET 0 0 10 10 re f",
        "[".repeat(depth),
        "]".repeat(depth)
    );
    one_page("", &content, Vec::new())
}

fn fixture(route: &str, depth: usize) -> Vec<u8> {
    match route {
        "stitch" => nested_stitching_functions(depth),
        "indexed" => nested_colour_spaces(depth, false),
        "separation" => nested_colour_spaces(depth, true),
        "calculator" => nested_calculator_procedures(depth),
        "arrays" => nested_content_arrays(depth),
        "pagetree" => deep_page_tree(depth),
        "decodepath" => form_that_decodes_by_path(),
        "form" => form_chain(depth),
        "annotation" => annotation_appearance(depth),
        "pattern" => tiling_pattern(depth),
        "type3" => type3_glyph(depth),
        "softmask" => soft_mask(depth),
        "self1" => self_drawing_form(1),
        "self2" => self_drawing_form(2),
        "self3" => self_drawing_form(3),
        "self4" => self_drawing_form(4),
        "mutual" => two_form_cycle(),
        "inherited" => inherited_cycle(),
        "kids" => repeated_kids(),
        other => panic!("unknown probe route {other}"),
    }
}

fn open(bytes: &[u8]) -> PdfDocument {
    PdfDocument::from_slice(bytes, ParseOptions::default()).expect("parses")
}

/// Parses and renders page 0 on a thread with a stack of its own.
///
/// Several fixtures are hostile to this crate's parser too (100,000 nested
/// page-tree nodes, over a million nested arrays). Test threads get 2 MiB
/// and unoptimized frames differ by platform, so the size is fixed here.
fn interpreted(bytes: &[u8]) -> anyhow::Result<Option<Vec<u8>>> {
    let bytes = bytes.to_vec();
    std::thread::Builder::new()
        .stack_size(16 * 1024 * 1024)
        .spawn(move || open(&bytes).page_rasterized_png_interpreted_checked(0, 72.0))
        .expect("spawns")
        .join()
        .expect("the render did not panic")
}

fn assert_refused(what: &str, bytes: &[u8]) {
    let error = match interpreted(bytes) {
        Err(error) => error.to_string(),
        Ok(_) => panic!("{what}: the interpreted renderer accepted it"),
    };
    assert!(
        error.contains("interpreted render refused"),
        "{what}: unexpected error {error}"
    );
}

fn assert_renders(what: &str, bytes: &[u8]) {
    let png = interpreted(bytes).unwrap_or_else(|error| panic!("{what}: {error}"));
    assert!(png.is_some(), "{what}: rendered nothing");
}

#[test]
fn a_form_that_draws_itself_is_refused_by_the_interpreted_entry() {
    for case in 1..=4 {
        assert_refused(&format!("case {case}"), &self_drawing_form(case));
    }
}

#[test]
fn a_form_that_draws_itself_never_reaches_the_interpreter_from_the_fast_entry() {
    for case in 1..=4 {
        let document = open(&self_drawing_form(case));
        let result = document.page_rasterized_png(0, 72.0);
        if case == 3 {
            // The fast rasterizer paints the rectangle and does not inherit
            // resources, so it never recurses and never falls through.
            assert!(matches!(result, Ok(Some(_))), "case 3 stopped rendering");
        } else {
            assert!(result.is_err(), "case {case} was accepted");
        }
    }
    // The convenience entry swallows the refusal; it must not abort either.
    assert!(open(&self_drawing_form(4))
        .page_rasterized_png_interpreted(0, 72.0)
        .is_none());
}

#[test]
fn two_forms_drawing_each_other_are_refused() {
    assert_refused("mutual cycle", &two_form_cycle());
}

#[test]
fn a_cycle_closed_through_resources_two_levels_up_is_refused() {
    assert_refused("inherited cycle", &inherited_cycle());
}

#[test]
fn an_annotation_appearance_that_draws_itself_is_refused() {
    assert_refused("annotation", &annotation_appearance(0));
}

#[test]
fn a_tiling_pattern_that_fills_with_itself_is_refused() {
    assert_refused("tiling pattern", &tiling_pattern(0));
}

#[test]
fn a_type3_glyph_that_shows_itself_is_refused() {
    assert_refused("type 3 glyph", &type3_glyph(0));
}

#[test]
fn a_soft_mask_group_that_masks_itself_is_refused() {
    assert_refused("soft mask", &soft_mask(0));
}

#[test]
fn ten_thousand_distinct_nested_forms_are_refused() {
    assert_refused("10,000-deep chain", &form_chain(10_000));
}

#[test]
fn deep_chains_through_every_other_route_are_refused() {
    assert_refused("annotation chain", &annotation_appearance(2_000));
    assert_refused("pattern chain", &tiling_pattern(2_000));
    assert_refused("soft mask chain", &soft_mask(2_000));
}

#[test]
fn ordinary_use_of_every_route_still_renders() {
    // 100 nested forms need about 2 MiB of unoptimized interpreter stack,
    // more than a test thread has: it renders on a stack of its own.
    for depth in [1usize, 8, 12, 100] {
        assert_renders(&format!("form chain {depth}"), &form_chain(depth));
    }
    assert_renders("annotation", &annotation_appearance(1));
    assert_renders("annotation over forms", &annotation_appearance(4));
    assert_renders("tiling pattern", &tiling_pattern(1));
    assert_renders("nested tiling patterns", &tiling_pattern(2));
    assert_renders("type 3 glyph", &type3_glyph(1));
    assert_renders("soft mask", &soft_mask(1));
    assert_renders("nested soft masks", &soft_mask(2));
}

#[test]
fn a_form_that_decodes_differently_by_path_is_refused() {
    assert_refused("decode by path", &form_that_decodes_by_path());
}

#[test]
fn a_hundred_thousand_distinct_names_are_accepted() {
    // Deduplicating names by a linear search took over twenty seconds here,
    // and counting each name as a context refused the page. There is no
    // clock in this test: a time limit failed on a loaded machine.
    assert_renders("many names", &many_names(100_000));
}

#[test]
fn a_page_tree_nested_100_000_deep_is_refused() {
    assert_refused("deep page tree", &deep_page_tree(100_000));
    assert_renders("page tree 20 deep", &deep_page_tree(20));
}

#[test]
fn deeply_nested_functions_and_colour_spaces_are_refused() {
    assert_refused("stitching functions", &nested_stitching_functions(30_000));
    assert_refused(
        "indexed colour spaces",
        &nested_colour_spaces(50_000, false),
    );
    assert_refused(
        "separation colour spaces",
        &nested_colour_spaces(50_000, true),
    );
    assert_refused(
        "calculator procedures",
        &nested_calculator_procedures(200_000),
    );
    assert_renders("stitching functions 5 deep", &nested_stitching_functions(5));
    assert_renders(
        "indexed colour spaces 3 deep",
        &nested_colour_spaces(3, false),
    );
    assert_renders(
        "calculator procedures 5 deep",
        &nested_calculator_procedures(5),
    );
}

#[test]
fn deeply_nested_arrays_in_content_are_refused() {
    assert_refused("content arrays", &nested_content_arrays(1_500_000));
    assert_renders("content arrays 20 deep", &nested_content_arrays(20));
}

#[test]
fn many_shallow_brackets_are_not_mistaken_for_nesting() {
    // 60,000 arrays, hex strings and inline dictionaries, none nested, with
    // brackets inside strings, a comment and inline image data.
    let mut content = String::from("BT /F1 10 Tf ");
    for _ in 0..60_000 {
        content.push_str("[(a[[\\(b) <5b5b>] TJ /P <</MCID 1>> BDC EMC % [[[[\n");
    }
    content.push_str("ET BI /W 1 /H 1 /BPC 8 /CS /G ID [ EI 0 0 10 10 re f");
    assert_renders("shallow brackets", &one_page("", &content, Vec::new()));
}

#[test]
fn a_type3_glyph_that_draws_a_local_form_renders() {
    // What Typst and Chromium write: the glyph procedure draws a form named
    // in the font's own resources.
    let bytes = one_page(
        "/Resources << /Font << /T 10 0 R >> >>",
        "BT /T 10 Tf 10 50 Td (a) Tj ET",
        vec![
            (
                10,
                type3_font("/Resources << /XObject << /X 12 0 R >> >>", 11),
            ),
            (11, stream("", "10 0 d0 /X Do")),
            (12, form("/Resources << >>", "0 0 8 8 re f")),
        ],
    );
    assert_renders("glyph with a local form", &bytes);
    // The same glyph naming a form only the page defines is still refused.
    let bytes = one_page(
        "/Resources << /Font << /T 10 0 R >> /XObject << /X 12 0 R >> >>",
        "BT /T 10 Tf 10 50 Td (a) Tj ET",
        vec![
            (10, type3_font("", 11)),
            (11, stream("", "10 0 d0 /X Do")),
            (12, form("/Resources << >>", "0 0 8 8 re f")),
        ],
    );
    assert_refused("glyph using the page's resources", &bytes);
}

/// A page showing one Type 3 glyph whose font carries `resources` and whose
/// procedure is `procedure`, plus `rest`.
fn glyph_page(resources: &str, procedure: &str, rest: Vec<(u32, Vec<u8>)>) -> Vec<u8> {
    let mut objects = vec![
        (10, type3_font(&format!("/Resources << {resources} >>"), 11)),
        (11, stream("", procedure)),
    ];
    objects.extend(rest);
    one_page(
        "/Resources << /Font << /T 10 0 R >> >>",
        "BT /T 10 Tf 10 50 Td (a) Tj ET",
        objects,
    )
}

#[test]
fn a_type3_glyph_may_enter_patterns_masks_and_fonts_its_font_defines() {
    let fill = "10 0 d0 /Pattern cs /P scn 0 0 8 8 re f";
    assert_renders(
        "glyph with a local tiling pattern",
        &glyph_page(
            "/Pattern << /P 12 0 R >>",
            fill,
            vec![(12, tiling("/Resources << >>", "0 0 4 4 re f"))],
        ),
    );
    assert_refused(
        "glyph whose local pattern fills with itself",
        &glyph_page(
            "/Pattern << /P 12 0 R >>",
            fill,
            vec![(12, tiling("", "/Pattern cs /P scn 0 0 4 4 re f"))],
        ),
    );

    let mask = "/ExtGState << /G << /SMask << /S /Luminosity /G 12 0 R >> >> >>";
    let group = |body: &str| form("/Group << /S /Transparency /CS /DeviceGray >>", body);
    assert_renders(
        "glyph with a local soft mask",
        &glyph_page(
            mask,
            "10 0 d0 /G gs 0 0 8 8 re f",
            vec![(12, group("1 g 0 0 8 8 re f"))],
        ),
    );
    assert_refused(
        "glyph whose local soft mask masks itself",
        &glyph_page(
            mask,
            "10 0 d0 /G gs 0 0 8 8 re f",
            vec![(12, group("/G gs 0 0 8 8 re f"))],
        ),
    );

    let show = "10 0 d0 BT /U 10 Tf (a) Tj ET";
    assert_renders(
        "glyph showing a glyph of a second local Type 3 font",
        &glyph_page(
            "/Font << /U 12 0 R >>",
            show,
            vec![
                (12, type3_font("/Resources << >>", 13)),
                (13, stream("", "10 0 d0 0 0 8 8 re f")),
            ],
        ),
    );
    assert_refused(
        "glyph showing a font only the page defines",
        &one_page(
            "/Resources << /Font << /T 10 0 R /U 12 0 R >> >>",
            "BT /T 10 Tf 10 50 Td (a) Tj ET",
            vec![
                (10, type3_font("", 11)),
                (11, stream("", show)),
                (12, type3_font("/Resources << >>", 13)),
                (13, stream("", "10 0 d0 0 0 8 8 re f")),
            ],
        ),
    );
}

#[test]
fn a_form_stamped_300_times_renders_and_keeps_rendering() {
    let content = "/F1 Do\n".repeat(300);
    let bytes = one_page(
        PAGE_DRAWS_F1,
        &content,
        vec![(10, form("/Resources << >>", "0 0 10 10 re f"))],
    );
    let document = open(&bytes);
    for render in 1..=40 {
        let png = document
            .page_rasterized_png_interpreted_checked(0, 72.0)
            .unwrap_or_else(|error| panic!("render {render}: {error}"));
        assert!(png.is_some(), "render {render} produced nothing");
    }
}

#[test]
fn a_repeated_kids_entry_does_not_move_the_guard_off_the_rendered_page() {
    // This crate lists a page once however often `/Kids` names it; the
    // interpreter lists it once per entry. So the interpreter's index 2 is
    // this crate's index 1, the page whose form draws itself. Whichever
    // index is asked for, the page that would be rendered is the one vetted.
    let document = open(&repeated_kids());
    assert_eq!(document.pages().len(), 2);
    let harmless = document
        .page_rasterized_png_interpreted_checked(0, 72.0)
        .expect("page 0 renders");
    assert!(harmless.is_some());
    assert!(document
        .page_rasterized_png_interpreted_checked(1, 72.0)
        .is_err());
    assert!(document.page_rasterized_png(1, 72.0).is_err());
    // Past this crate's last page nothing is rendered at all.
    assert!(!matches!(
        document.page_rasterized_png_interpreted_checked(2, 72.0),
        Ok(Some(_))
    ));
}

/// A page that shows text in a Type 0 font (object 10) whose `/Encoding`
/// is the CMap stream object 20; `cmaps` supplies object 20 and any others.
fn type0_page(cmaps: Vec<(u32, Vec<u8>)>) -> Vec<u8> {
    let font = b"<< /Type /Font /Subtype /Type0 /BaseFont /X /Encoding 20 0 R \
        /DescendantFonts [<< /Type /Font /Subtype /CIDFontType2 /BaseFont /X \
        /CIDSystemInfo << /Registry (Adobe) /Ordering (Identity) /Supplement 0 >> \
        /FontDescriptor << /Type /FontDescriptor /FontName /X /Flags 4 \
        /FontBBox [0 0 1000 1000] /ItalicAngle 0 /Ascent 800 /Descent -200 \
        /CapHeight 700 /StemV 80 >> >>] >>"
        .to_vec();
    let mut rest = vec![(10, font)];
    rest.extend(cmaps);
    one_page(
        "/Resources << /Font << /F1 10 0 R >> >>",
        "BT /F1 12 Tf 10 10 Td <0102> Tj ET 0 0 10 10 re f",
        rest,
    )
}

const CMAP_BODY: &str = "/CIDInit /ProcSet findresource begin 12 dict begin begincmap \
    1 begincodespacerange <00> <FF> endcodespacerange \
    1 begincidrange <00> <FF> 0 endcidrange endcmap end end";

/// `count` CMaps from object 20 on, each naming the next with both
/// `/UseCMap` and `usecmap`; with `cycle` the last names the first.
fn usecmap_chain(count: u32, cycle: bool) -> Vec<u8> {
    let mut cmaps = Vec::new();
    for index in 0..count {
        let number = 20 + index;
        let next = if index + 1 < count {
            Some(number + 1)
        } else if cycle {
            Some(20)
        } else {
            None
        };
        let (dict, body) = match next {
            Some(next) => (
                format!("/Type /CMap /CMapName /C{number} /UseCMap {next} 0 R"),
                format!("/C{next} usecmap {CMAP_BODY}"),
            ),
            None => (
                format!("/Type /CMap /CMapName /C{number}"),
                CMAP_BODY.to_string(),
            ),
        };
        cmaps.push((number, stream(&dict, &body)));
    }
    type0_page(cmaps)
}

/// A Type 0 font whose CMap opens with `comments` consecutive comment lines.
fn cmap_with_comments(comments: usize) -> Vec<u8> {
    let body = format!("{}{CMAP_BODY}", "%\n".repeat(comments));
    type0_page(vec![(20, stream("/Type /CMap /CMapName /C", &body))])
}

#[test]
fn usecmap_chains_and_cycles_render_because_the_interpreter_never_follows_them() {
    // hayro-interpret 0.5.0 only logs `/UseCMap` and treats `usecmap` as a
    // no-op, so neither a cycle nor a long chain costs it anything.
    assert_renders("two CMaps using each other", &usecmap_chain(2, true));
    assert_renders("a chain of 1,000 CMaps", &usecmap_chain(1_000, false));
}

#[test]
fn a_cmap_opening_with_a_million_comment_lines_is_refused() {
    // The CMap lexer calls itself once per consecutive comment line.
    assert_refused("a million comment lines", &cmap_with_comments(1_000_000));
    assert_renders("twenty comment lines", &cmap_with_comments(20));
}

/// A page showing text in a Type 1 font whose descriptor holds a chain of
/// `depth` arrays, each the next object; nothing but the font reaches them.
fn deep_object_in_font(depth: u32) -> Vec<u8> {
    let mut rest = vec![
        (
            10,
            b"<< /Type /Font /Subtype /Type1 /BaseFont /Helvetica /FontDescriptor 11 0 R >>"
                .to_vec(),
        ),
        (
            11,
            b"<< /Type /FontDescriptor /FontName /Helvetica /Flags 32 \
              /FontBBox [0 0 1000 1000] /ItalicAngle 0 /Ascent 800 /Descent -200 \
              /CapHeight 700 /StemV 80 /Extra 12 0 R >>"
                .to_vec(),
        ),
    ];
    for level in 0..depth {
        rest.push((12 + level, format!("[{} 0 R]", 13 + level).into_bytes()));
    }
    rest.push((12 + depth, b"[0]".to_vec()));
    one_page(
        "/Resources << /Font << /F1 10 0 R >> >>",
        "BT /F1 12 Tf 10 10 Td (a) Tj ET",
        rest,
    )
}

#[test]
fn an_object_nested_deeply_inside_a_font_is_refused() {
    assert_refused(
        "1,000 arrays under a font descriptor",
        &deep_object_in_font(1_000),
    );
    assert_renders("8 arrays under a font descriptor", &deep_object_in_font(8));
}

/// A page painting a shading whose function is a sampled function with
/// these `/Size` entries over `inputs` inputs, one output, and `samples`
/// bytes of 8-bit samples.
fn sampled_function(sizes: &str, inputs: usize, samples: usize) -> Vec<u8> {
    let domain = "0 1 ".repeat(inputs);
    one_page(
        SHADED_PAGE,
        "/S sh",
        vec![(
            10,
            stream(
                &format!(
                    "/FunctionType 0 /Domain [{domain}] /Range [0 1] /Size [{sizes}] \
                     /BitsPerSample 8"
                ),
                &"A".repeat(samples),
            ),
        )],
    )
}

#[test]
fn a_sampled_function_with_sixteen_inputs_is_refused() {
    let sizes = "2 ".repeat(16);
    assert_refused(
        "16 inputs of 2 samples",
        &sampled_function(&sizes, 16, 1 << 16),
    );
    assert_renders("256 samples", &sampled_function("256", 1, 256));
    assert_renders(
        "4 inputs of 8 samples",
        &sampled_function("8 8 8 8", 4, 4096),
    );
}

#[test]
fn a_sampled_function_with_a_huge_overflowing_or_empty_size_is_refused() {
    assert_refused(
        "one input of 4,000,000,000 samples",
        &sampled_function("4000000000", 1, 16),
    );
    assert_refused(
        "a /Size whose product overflows",
        &sampled_function("4294967295 4294967295 4294967295", 3, 16),
    );
    assert_refused("a /Size of zero", &sampled_function("0", 1, 0));
    assert_refused(
        "a stream of 4,000,000 samples",
        &sampled_function("2", 1, 4_000_000),
    );
}

#[test]
fn a_million_saved_graphics_states_are_refused() {
    let content = format!("{}0 0 10 10 re f", "q\n".repeat(1_000_000));
    assert_refused("a million q", &one_page("", &content, Vec::new()));
    let nested = format!("{}0 0 10 10 re f{}", "q ".repeat(100), " Q".repeat(100));
    assert_renders("a hundred nested q", &one_page("", &nested, Vec::new()));
}

#[test]
fn saved_graphics_states_add_up_across_nested_forms() {
    // Each stream alone is under the bound; the form is drawn inside the
    // page's 3,000 saved states and adds 3,000 of its own.
    let page = format!("{}/F1 Do", "q ".repeat(3_000));
    let inner = format!("{}0 0 10 10 re f", "q ".repeat(3_000));
    assert_refused(
        "3,000 q on the page, 3,000 in its form",
        &one_page(
            PAGE_DRAWS_F1,
            &page,
            vec![(10, form("/Resources << >>", &inner))],
        ),
    );
}

#[test]
fn the_guards_own_decoding_never_refuses_a_page_whatever_the_document_budget() {
    // A real file was refused once the guard also read font CMaps: its
    // content streams alone took 99% of what the document budget had left
    // after the render's own reserve. The guard's decoding is overhead and
    // must not compete with the render for that budget.
    let cmap = format!("{CMAP_BODY}{}", " ".repeat(2 * 1024 * 1024));
    let bytes = type0_page(vec![(20, stream("/Type /CMap /CMapName /C", &cmap))]);
    let mut rendered = 0;
    for step in 1..=96 {
        let options = ParseOptions {
            max_total_decoded_bytes: step * 128 * 1024,
            ..ParseOptions::default()
        };
        let Ok(document) = PdfDocument::from_slice(&bytes, options) else {
            continue;
        };
        match document.page_rasterized_png_interpreted_checked(0, 72.0) {
            Ok(Some(_)) => rendered += 1,
            Ok(None) => {}
            Err(error) => assert!(
                !error.to_string().contains("interpreted render refused"),
                "budget {} KiB: {error}",
                step * 128
            ),
        }
    }
    assert!(rendered > 0, "no budget in the sweep rendered the page");
}

/// A page drawing an image XObject that declares `width` x `height` 8-bit
/// grey pixels but holds only 16 bytes of samples.
fn declared_image(width: u32, height: u32) -> Vec<u8> {
    one_page(
        "/Resources << /XObject << /I 10 0 R >> >>",
        "q 50 0 0 50 0 0 cm /I Do Q",
        vec![(
            10,
            stream(
                &format!(
                    "/Type /XObject /Subtype /Image /Width {width} /Height {height} \
                     /ColorSpace /DeviceGray /BitsPerComponent 8"
                ),
                &"A".repeat(16),
            ),
        )],
    )
}

/// The same as an inline image.
fn declared_inline_image(width: u32, height: u32) -> Vec<u8> {
    let content = format!(
        "q 50 0 0 50 0 0 cm BI /W {width} /H {height} /CS /G /BPC 8 ID {} EI Q",
        "A".repeat(16)
    );
    one_page("", &content, Vec::new())
}

#[test]
fn images_declaring_more_pixels_than_the_render_may_decode_are_refused() {
    // This crate already reserves an image XObject's declared RGBA size
    // against the document budget before rendering, so the guard is only
    // reached here with a budget large enough to hold that reservation.
    let options = ParseOptions {
        max_total_decoded_bytes: 8 << 30,
        ..ParseOptions::default()
    };
    let document =
        PdfDocument::from_slice(&declared_image(20_000, 20_000), options).expect("parses");
    let error = match document.page_rasterized_png_interpreted_checked(0, 72.0) {
        Err(error) => error.to_string(),
        Ok(_) => panic!("a 20,000 x 20,000 image XObject: the interpreted renderer accepted it"),
    };
    assert!(
        error.contains("interpreted render refused"),
        "a 20,000 x 20,000 image XObject: unexpected error {error}"
    );
    assert_refused(
        "a 20,000 x 20,000 inline image",
        &declared_inline_image(20_000, 20_000),
    );
    assert_renders("a 4 x 4 image XObject", &declared_image(4, 4));
    assert_renders("a 4 x 4 inline image", &declared_inline_image(4, 4));
}

/// A page filled with one tiling pattern whose 1 x 1 cell repeats every
/// `step` units.
fn sparse_tiling(step: u32) -> Vec<u8> {
    one_page(
        "/Resources << /Pattern << /P 10 0 R >> >>",
        "/Pattern cs /P scn 0 0 100 100 re f",
        vec![(
            10,
            stream(
                &format!(
                    "/Type /Pattern /PatternType 1 /PaintType 1 /TilingType 1 \
                     /BBox [0 0 1 1] /XStep {step} /YStep {step} /Resources << >>"
                ),
                "0 0 1 1 re f",
            ),
        )],
    )
}

#[test]
fn a_tiling_cell_larger_than_the_render_may_hold_is_refused() {
    // The interpreter sizes a cell's renderer from its step, not the page:
    // this one was 8,000 x 8,000 pixels at 72 dpi, 287 MB at peak, and
    // 2.95 GB at 300 dpi.
    assert_refused(
        "a 1 x 1 cell repeating every 8,000 units",
        &sparse_tiling(8_000),
    );
    assert_renders("a 1 x 1 cell repeating every 2 units", &sparse_tiling(2));
    assert_renders("8 nested tiling cells", &tiling_pattern(8));
}

/// A page showing text in `count` Type 0 fonts that share one CMap of
/// `cmap_bytes` of Flate-compressed text, object 99.
fn many_compressed_cmaps(count: u32, cmap_bytes: usize) -> Vec<u8> {
    use std::io::Write;
    let mut encoder = flate2::write::ZlibEncoder::new(Vec::new(), flate2::Compression::best());
    encoder
        .write_all(format!("{CMAP_BODY}{}", " ".repeat(cmap_bytes)).as_bytes())
        .expect("compresses");
    let packed = encoder.finish().expect("compresses");
    let mut fonts = String::new();
    let mut content = String::from("BT ");
    let mut rest = Vec::new();
    for index in 0..count {
        let font = 100 + 2 * index;
        fonts.push_str(&format!("/F{index} {font} 0 R "));
        content.push_str(&format!("/F{index} 12 Tf <01> Tj "));
        rest.push((
            font,
            "<< /Type /Font /Subtype /Type0 /BaseFont /X /Encoding 99 0 R \
                 /DescendantFonts [<< /Type /Font /Subtype /CIDFontType2 /BaseFont /X \
                 /CIDSystemInfo << /Registry (Adobe) /Ordering (Identity) /Supplement 0 >> \
                 /FontDescriptor << /Type /FontDescriptor /FontName /X /Flags 4 \
                 /FontBBox [0 0 1000 1000] /ItalicAngle 0 /Ascent 800 /Descent -200 \
                 /CapHeight 700 /StemV 80 >> >>] >>"
                .as_bytes()
                .to_vec(),
        ));
    }
    let mut cmap = format!(
        "<< /Type /CMap /Filter /FlateDecode /Length {} >>\nstream\n",
        packed.len()
    )
    .into_bytes();
    cmap.extend_from_slice(&packed);
    cmap.extend_from_slice(b"\nendstream");
    rest.push((99, cmap));
    content.push_str("ET");
    one_page(
        &format!("/Resources << /Font << {fonts}>> >>"),
        &content,
        rest,
    )
}

#[test]
fn the_guard_decodes_no_more_than_the_callers_decode_limit() {
    // The file decodes to about 128 KiB, within the caller's 1 MiB limit,
    // but the guard reads the shared CMap once per font: 8 MiB in all. It
    // must stop at the caller's limit and refuse.
    let limit = 1024 * 1024;
    let options = ParseOptions {
        max_total_decoded_bytes: limit,
        ..ParseOptions::default()
    };
    let document =
        PdfDocument::from_slice(&many_compressed_cmaps(64, 128 * 1024), options).expect("parses");
    let error = match document.page_rasterized_png_interpreted_checked(0, 72.0) {
        Err(error) => error.to_string(),
        Ok(_) => panic!("the guard decoded past the caller's 1 MiB limit"),
    };
    let decoded: usize = error
        .split("after decoding ")
        .nth(1)
        .and_then(|rest| rest.split(' ').next())
        .and_then(|number| number.parse().ok())
        .unwrap_or_else(|| panic!("no decoded-byte count in: {error}"));
    // One CMap past the limit at most: each stream is counted once decoded.
    assert!(decoded <= limit + 1024 * 1024, "decoded {decoded} bytes");
}

/// A page whose colour spaces break two bounds at once: `/A` nests 100
/// `/Indexed` spaces, `/B` has a 16-input sampled tint transform.
fn two_broken_colour_spaces() -> Vec<u8> {
    let mut rest = Vec::new();
    for level in 0..100u32 {
        rest.push((
            20 + level,
            format!("[/Indexed {} 0 R 0 <00>]", 21 + level).into_bytes(),
        ));
    }
    rest.push((120, b"/DeviceGray".to_vec()));
    rest.push((
        200,
        stream(
            &format!(
                "/FunctionType 0 /Domain [{}] /Range [0 1] /Size [{}] /BitsPerSample 8",
                "0 1 ".repeat(16),
                "2 ".repeat(16)
            ),
            &"A".repeat(1 << 16),
        ),
    ));
    one_page(
        "/Resources << /ColorSpace << /A 20 0 R /B [/Separation /X /DeviceGray 200 0 R] >> >>",
        "0 0 10 10 re f",
        rest,
    )
}

#[test]
fn a_page_breaking_two_bounds_gives_the_same_reason_in_every_process() {
    const CHILD: &str = "RENDER_GUARD_REASON_CHILD";
    if std::env::var_os(CHILD).is_some() {
        let reason = match interpreted(&two_broken_colour_spaces()) {
            Err(error) => error.to_string(),
            Ok(_) => "accepted".to_string(),
        };
        println!("REASON {reason}");
        return;
    }
    // Dictionary keys come back in hash-map order, which changes between
    // processes, so the check runs in fresh processes.
    let exe = std::env::current_exe().expect("test binary");
    let mut reasons = std::collections::BTreeSet::new();
    for _ in 0..8 {
        let output = std::process::Command::new(&exe)
            .args([
                "--exact",
                "a_page_breaking_two_bounds_gives_the_same_reason_in_every_process",
                "--nocapture",
                "--test-threads=1",
            ])
            .env(CHILD, "1")
            .output()
            .expect("runs the test binary");
        let stdout = String::from_utf8_lossy(&output.stdout);
        let reason = stdout
            .lines()
            .find_map(|line| {
                line.split_once("REASON ")
                    .map(|(_, reason)| reason.to_string())
            })
            .unwrap_or_else(|| panic!("no reason reported: {stdout}"));
        reasons.insert(reason);
    }
    assert_eq!(
        reasons.len(),
        1,
        "reasons differ between processes: {reasons:?}"
    );
}

/// Runs the interpreter with no guard, on a thread with a chosen stack.
///
/// Not a test: it exists to confirm that a fixture aborts the process and to
/// measure how much stack one nesting level costs. Run one probe per process,
/// since an overflow kills the binary:
///
/// ```text
/// RENDER_GUARD_PROBE=<route>:<depth>:<stack KiB> cargo test -p pdf-fast-core \
///     --test render_guard probe_interpreter_stack -- --ignored --exact
/// ```
///
/// Routes are the names in `fixture`. For the chain routes, the deepest
/// `depth` that survives at two stack sizes gives the bytes per level.
#[test]
#[ignore = "drives the unguarded interpreter; aborts by design"]
fn probe_interpreter_stack() {
    use hayro::hayro_interpret::InterpreterSettings;
    use hayro::hayro_syntax::Pdf;
    use hayro::RenderSettings;
    use std::sync::Arc;

    let Ok(spec) = std::env::var("RENDER_GUARD_PROBE") else {
        return;
    };
    let mut parts = spec.split(':');
    let route = parts.next().expect("route").to_string();
    let depth: usize = parts.next().expect("depth").parse().expect("depth");
    let stack_kib: usize = parts.next().expect("stack").parse().expect("stack KiB");
    let page_index: usize = parts.next().map_or(0, |index| index.parse().expect("page"));
    let bytes = fixture(&route, depth);
    let rendered = std::thread::Builder::new()
        .stack_size(stack_kib * 1024)
        .spawn(move || {
            let pdf = Pdf::new(Arc::new(bytes)).expect("interpreter parses the fixture");
            let page = pdf.pages().get(page_index).expect("page exists");
            let pixmap = hayro::render(
                page,
                &InterpreterSettings::default(),
                &RenderSettings::default(),
            );
            pixmap.width()
        })
        .expect("spawns")
        .join()
        .expect("render thread finished");
    println!("probe {spec}: rendered, width {rendered}");
}
