//! Valid and near-valid documents that the malformed-input hardening broke.
//!
//! Each test here failed on the first version of that hardening and is the
//! reason one of its limits was reshaped: object streams listed out of order
//! lost objects, pages with nested saved states and a clip stopped
//! rasterizing, and the interpreted renderer refused nested forms and spent
//! the document's decode budget on every call.

use pdf_fast_core::{ParseOptions, PdfDocument};
use std::io::Write;

fn zlib(bytes: &[u8]) -> Vec<u8> {
    let mut encoder = flate2::write::ZlibEncoder::new(Vec::new(), flate2::Compression::default());
    encoder.write_all(bytes).expect("compress");
    encoder.finish().expect("finish compression")
}

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

fn stream(dict: &str, body: &[u8]) -> Vec<u8> {
    let mut out = format!("<< {dict} /Length {} >>\nstream\n", body.len()).into_bytes();
    out.extend_from_slice(body);
    out.extend_from_slice(b"\nendstream");
    out
}

/// A document whose page tree lives in one object stream.
///
/// `entries` are `(object number, body)` in the order they are stored;
/// `table` holds, for each row of the offset table, the entry it lists.
fn object_stream_document(entries: &[(u32, &str)], table: &[usize]) -> Vec<u8> {
    let mut data = String::new();
    let mut offsets = Vec::new();
    for (_, body) in entries {
        offsets.push(data.len());
        data.push_str(body);
        data.push('\n');
    }
    let mut header = String::new();
    for index in table {
        header.push_str(&format!("{} {} ", entries[*index].0, offsets[*index]));
    }
    let first = header.len();
    let payload = zlib(format!("{header}{data}").as_bytes());
    pdf(&[
        (1, b"<< /Type /Catalog /Pages 2 0 R >>".to_vec()),
        (
            20,
            stream(
                &format!(
                    "/Type /ObjStm /N {} /First {first} /Filter /FlateDecode",
                    table.len()
                ),
                &payload,
            ),
        ),
        (21, stream("", b"0 0 10 10 re f")),
    ])
}

const PAGE_TREE: [(u32, &str); 3] = [
    (2, "<< /Type /Pages /Kids [3 0 R] /Count 1 >>"),
    (
        3,
        "<< /Type /Page /Parent 2 0 R /MediaBox [0 0 100 100] /Contents 21 0 R >>",
    ),
    (4, "<< /Marker /Third >>"),
];

#[test]
fn an_object_stream_listed_out_of_offset_order_keeps_every_object() {
    for table in [[0usize, 1, 2], [2, 1, 0], [1, 2, 0]] {
        let bytes = object_stream_document(&PAGE_TREE, &table);
        let document = PdfDocument::from_slice(&bytes, ParseOptions::default()).expect("parses");
        for (number, _) in PAGE_TREE {
            assert!(
                document.object_by_id(number, 0).is_some(),
                "object {number} lost with table order {table:?}"
            );
        }
        assert_eq!(document.pages().len(), 1, "table order {table:?}");
    }
}

#[test]
fn offsets_aimed_at_one_object_materialize_it_once() {
    // Three table rows naming three object numbers, all at the first entry.
    let entries = [
        (5u32, "<< /Marker /Shared >>"),
        (6u32, "<< /Marker /Shared >>"),
        (7u32, "<< /Marker /Shared >>"),
    ];
    let header = "5 0 6 0 7 0 ";
    let payload = zlib(format!("{header}{}\n", entries[0].1).as_bytes());
    let bytes = pdf(&[
        (1, b"<< /Type /Catalog >>".to_vec()),
        (
            20,
            stream(
                &format!(
                    "/Type /ObjStm /N 3 /First {} /Filter /FlateDecode",
                    header.len()
                ),
                &payload,
            ),
        ),
    ]);
    let document = PdfDocument::from_slice(&bytes, ParseOptions::default()).expect("parses");
    let found = entries
        .iter()
        .filter(|(number, _)| document.object_by_id(*number, 0).is_some())
        .count();
    assert_eq!(found, 1, "an overlapping offset must not be read again");
}

/// A page whose content opens 300 saved states, changes something inside
/// the last one, closes it, then draws. 300 is past the 256 the content
/// interpreters keep, so that last save is counted and not stored.
fn page_past_the_saved_state_limit(inside: &str, after: &str) -> Vec<u8> {
    let content = format!("{}{inside} Q {after}", "q ".repeat(300));
    pdf(&[
        (1u32, b"<< /Type /Catalog /Pages 2 0 R >>".to_vec()),
        (2, b"<< /Type /Pages /Kids [3 0 R] /Count 1 >>".to_vec()),
        (
            3,
            b"<< /Type /Page /Parent 2 0 R /MediaBox [0 0 200 200] /Contents 4 0 R \
              /Resources << /Font << /F1 5 0 R >> >> >>"
                .to_vec(),
        ),
        (4, stream("", content.as_bytes())),
        (
            5,
            b"<< /Type /Font /Subtype /Type1 /BaseFont /Helvetica >>".to_vec(),
        ),
    ])
}

#[test]
fn text_after_a_restore_past_the_limit_does_not_keep_a_color_set_inside() {
    let bytes = page_past_the_saved_state_limit("1 0 0 rg", "BT /F1 12 Tf 10 50 Td (Hi) Tj ET");
    let document = PdfDocument::from_slice(&bytes, ParseOptions::default()).expect("parses");
    let runs = document.semantic_text_runs();
    let run = runs
        .iter()
        .find(|run| run.text.contains("Hi"))
        .expect("text found");
    assert_eq!(
        run.fill_color.components,
        vec![0.0],
        "the text kept the red set inside the closed state"
    );
}

#[test]
fn a_line_after_a_restore_past_the_limit_does_not_keep_a_matrix_set_inside() {
    let bytes = page_past_the_saved_state_limit("1 0 0 1 40 40 cm", "10 10 m 90 10 l S");
    let document = PdfDocument::from_slice(&bytes, ParseOptions::default()).expect("parses");
    let segments = document.page_line_segments(0).expect("segments");
    let segment = segments.first().expect("one line");
    let left = segment.x0.min(segment.x1);
    assert!(
        (left - 10.0).abs() < 0.01,
        "the line kept the translation set inside the closed state: starts at {left}"
    );
}

#[cfg(feature = "rasterize")]
mod rendering {
    use super::{pdf, stream};
    use pdf_fast_core::{ParseOptions, PdfDocument};

    /// A page that draws a chain of `depth` nested Form XObjects, none cyclic.
    fn form_chain(depth: usize) -> Vec<u8> {
        let mut objects = vec![
            (1u32, b"<< /Type /Catalog /Pages 2 0 R >>".to_vec()),
            (2, b"<< /Type /Pages /Kids [3 0 R] /Count 1 >>".to_vec()),
            (
                3,
                b"<< /Type /Page /Parent 2 0 R /MediaBox [0 0 100 100] /Contents 4 0 R \
                  /Resources << /XObject << /F1 10 0 R >> >> >>"
                    .to_vec(),
            ),
            (4, stream("", b"0 0 50 50 re f /F1 Do")),
        ];
        for level in 1..=depth {
            let number = 9 + u32::try_from(level).expect("small depth");
            let (resources, content) = if level < depth {
                (
                    format!(
                        "/Resources << /XObject << /F{} {} 0 R >> >>",
                        level + 1,
                        number + 1
                    ),
                    format!("{0} {0} 5 5 re f /F{1} Do", level * 5, level + 1),
                )
            } else {
                (
                    "/Resources << >>".to_string(),
                    "60 60 20 20 re f".to_string(),
                )
            };
            objects.push((
                number,
                stream(
                    &format!("/Type /XObject /Subtype /Form /BBox [0 0 100 100] {resources}"),
                    content.as_bytes(),
                ),
            ));
        }
        pdf(&objects)
    }

    /// One Form XObject of `form_len` bytes drawn `draws` times by the page.
    fn repeated_form(form_len: usize, draws: usize) -> Vec<u8> {
        let mut form = b"0 0 10 10 re f\n".to_vec();
        form.resize(form_len, b' ');
        let content = "/F1 Do\n".repeat(draws);
        pdf(&[
            (1u32, b"<< /Type /Catalog /Pages 2 0 R >>".to_vec()),
            (2, b"<< /Type /Pages /Kids [3 0 R] /Count 1 >>".to_vec()),
            (
                3,
                b"<< /Type /Page /Parent 2 0 R /MediaBox [0 0 100 100] /Contents 4 0 R \
                  /Resources << /XObject << /F1 10 0 R >> >> >>"
                    .to_vec(),
            ),
            (4, stream("", content.as_bytes())),
            (
                10,
                stream(
                    "/Type /XObject /Subtype /Form /BBox [0 0 100 100] /Resources << >>",
                    &form,
                ),
            ),
        ])
    }

    /// An A4 page that clips to the page, then nests `levels` saved states
    /// around one fill. With `clip_each_level`, every level clips again.
    fn nested_states_under_a_clip(levels: usize, clip_each_level: bool) -> Vec<u8> {
        let open = if clip_each_level {
            "q 10 10 500 700 re W n "
        } else {
            "q "
        };
        let content = format!(
            "0 0 595 842 re W n {} 1 0 0 rg 100 100 200 200 re f {}",
            open.repeat(levels),
            "Q ".repeat(levels)
        );
        pdf(&[
            (1u32, b"<< /Type /Catalog /Pages 2 0 R >>".to_vec()),
            (2, b"<< /Type /Pages /Kids [3 0 R] /Count 1 >>".to_vec()),
            (
                3,
                b"<< /Type /Page /Parent 2 0 R /MediaBox [0 0 595 842] /Contents 4 0 R >>".to_vec(),
            ),
            (4, stream("", content.as_bytes())),
        ])
    }

    #[test]
    fn the_interpreted_renderer_draws_forms_nested_past_the_fast_path_limit() {
        for depth in [1usize, 8, 9, 12] {
            let bytes = form_chain(depth);
            let document =
                PdfDocument::from_slice(&bytes, ParseOptions::default()).expect("parses");
            let png = document
                .page_rasterized_png_interpreted_checked(0, 72.0)
                .unwrap_or_else(|error| panic!("depth {depth}: {error}"));
            assert!(png.is_some(), "depth {depth} rendered nothing");
        }
    }

    #[test]
    fn a_form_stamped_many_times_renders_within_the_default_budget() {
        let bytes = repeated_form(1_000_000, 300);
        let document = PdfDocument::from_slice(&bytes, ParseOptions::default()).expect("parses");
        let png = document
            .page_rasterized_png_interpreted_checked(0, 72.0)
            .expect("renders");
        assert!(png.is_some());
    }

    #[test]
    fn rendering_one_page_repeatedly_does_not_drain_the_budget_per_draw() {
        // Twenty draws of a 1 MB form. Charging the form once per draw on
        // every call exhausted the default budget after eleven renders.
        let bytes = repeated_form(1_000_000, 20);
        let document = PdfDocument::from_slice(&bytes, ParseOptions::default()).expect("parses");
        for render in 1..=30 {
            let png = document
                .page_rasterized_png_interpreted_checked(0, 72.0)
                .unwrap_or_else(|error| panic!("render {render}: {error}"));
            assert!(png.is_some(), "render {render} produced nothing");
        }
    }

    #[test]
    fn saved_states_under_one_clip_share_its_mask() {
        // 26 levels is inside the 28 a reader must support (PDF 32000-1,
        // Annex C). At 300 dpi one mask is 8.7 MB; a copy per level would be
        // 226 MB and was refused against the default budget.
        let bytes = nested_states_under_a_clip(26, false);
        let document = PdfDocument::from_slice(&bytes, ParseOptions::default()).expect("parses");
        let png = document.page_rasterized_png(0, 300.0).expect("renders");
        assert!(png.is_some());
    }

    #[test]
    fn a_restore_past_the_saved_state_limit_does_not_leak_what_was_set_inside() {
        // 300 saves is past the 256 the rasterizer keeps. The red set after
        // them sits inside a save that was counted, not stored; the `Q` that
        // closes it used to restore nothing, so the square came out red.
        let content = format!("{}1 0 0 rg Q 0 0 100 100 re f", "q ".repeat(300));
        let bytes = pdf(&[
            (1u32, b"<< /Type /Catalog /Pages 2 0 R >>".to_vec()),
            (2, b"<< /Type /Pages /Kids [3 0 R] /Count 1 >>".to_vec()),
            (
                3,
                b"<< /Type /Page /Parent 2 0 R /MediaBox [0 0 100 100] /Contents 4 0 R >>".to_vec(),
            ),
            (4, stream("", content.as_bytes())),
        ]);
        let document = PdfDocument::from_slice(&bytes, ParseOptions::default()).expect("parses");
        let png = document
            .page_rasterized_png(0, 72.0)
            .expect("renders")
            .expect("paints");
        let image = image::load_from_memory(&png).expect("decodes").to_rgba8();
        let pixel = image.get_pixel(50, 50).0;
        assert_eq!(pixel, [0, 0, 0, 255], "the fill kept a color set inside");
    }

    #[test]
    fn a_new_clip_at_every_level_is_still_bounded_by_the_budget() {
        // Each level changes the clip, so each needs a mask of its own. With
        // a 64 MiB budget and 8.7 MB masks, 200 levels cannot all be held.
        let bytes = nested_states_under_a_clip(200, true);
        let options = ParseOptions {
            max_total_decoded_bytes: 64 * 1024 * 1024,
            ..ParseOptions::default()
        };
        let document = PdfDocument::from_slice(&bytes, options).expect("parses");
        assert!(document.page_rasterized_png(0, 300.0).is_err());
    }
}
