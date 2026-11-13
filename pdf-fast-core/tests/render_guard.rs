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

fn fixture(route: &str, depth: usize) -> Vec<u8> {
    match route {
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

fn interpreted(bytes: &[u8]) -> anyhow::Result<Option<Vec<u8>>> {
    open(bytes).page_rasterized_png_interpreted_checked(0, 72.0)
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
fn a_hundred_thousand_distinct_names_are_scanned_quickly() {
    // Deduplicating names by a linear search took over twenty seconds here.
    let started = std::time::Instant::now();
    assert_renders("many names", &many_names(100_000));
    assert!(
        started.elapsed() < std::time::Duration::from_secs(10),
        "scanning took {:?}",
        started.elapsed()
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
