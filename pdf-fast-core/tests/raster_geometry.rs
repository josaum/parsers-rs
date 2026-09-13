//! Which box does a rasterized page use: `/MediaBox` or `/CropBox`?
//!
//! The two repositories' rasterizers read different sources — one
//! `render_dimensions()` (which resolves the visible region), the other the raw
//! `/MediaBox` — so a page where those differ produces different pixels, and
//! those pixels feed downstream OCR and vision models. A page whose /CropBox is
//! smaller than its /MediaBox is ordinary in scanned contracts, where a scanner
//! captures a larger sheet than the document region.
//!
//! PDF 32000-1 §14.11.2 is explicit about which one is the page for output
//! purposes: the crop box is "the region to which the contents of the page
//! shall be clipped (cropped) when displayed or printed", while the media box
//! is "the boundaries of the physical medium on which the page shall be
//! displayed or printed". So the rasterized image must follow `/CropBox`.
//!
//! These tests pin that, and pin the rotation interaction, because both are
//! silent when wrong: the PNG is still a valid PNG of a plausible size.

use pdf_fast_core::{ParseOptions, PdfDocument};

fn pdf_stream_object(dictionary_entries: &[u8], body: &[u8]) -> Vec<u8> {
    let mut object = format!(
        "<< {} /Length {} >>\nstream\n",
        String::from_utf8_lossy(dictionary_entries),
        body.len()
    )
    .into_bytes();
    object.extend_from_slice(body);
    object.extend_from_slice(b"\nendstream");
    object
}

fn pdf_with_xref(objects: Vec<Vec<u8>>) -> Vec<u8> {
    let mut pdf = b"%PDF-1.7\n%\xE2\xE3\xCF\xD3\n".to_vec();
    let mut offsets = Vec::with_capacity(objects.len());
    for (index, object) in objects.iter().enumerate() {
        offsets.push(pdf.len());
        pdf.extend_from_slice(format!("{} 0 obj\n", index + 1).as_bytes());
        pdf.extend_from_slice(object);
        pdf.extend_from_slice(b"\nendobj\n");
    }
    let xref_offset = pdf.len();
    pdf.extend_from_slice(format!("xref\n0 {}\n", objects.len() + 1).as_bytes());
    pdf.extend_from_slice(b"0000000000 65535 f \n");
    for offset in offsets {
        pdf.extend_from_slice(format!("{offset:010} 00000 n \n").as_bytes());
    }
    pdf.extend_from_slice(
        format!(
            "trailer\n<< /Size {} /Root 1 0 R >>\nstartxref\n{xref_offset}\n%%EOF\n",
            objects.len() + 1
        )
        .as_bytes(),
    );
    pdf
}

/// One page whose `/CropBox` is a small region inside a much larger
/// `/MediaBox`, with a rectangle drawn in page coordinates.
fn page_with_boxes(media: &str, crop: Option<&str>, rotate: Option<i32>) -> Vec<u8> {
    let mut page = format!("<< /Type /Page /Parent 2 0 R /MediaBox [{media}]");
    if let Some(crop) = crop {
        page.push_str(&format!(" /CropBox [{crop}]"));
    }
    if let Some(rotate) = rotate {
        page.push_str(&format!(" /Rotate {rotate}"));
    }
    page.push_str(" /Resources << >> /Contents 4 0 R >>");
    pdf_with_xref(vec![
        b"<< /Type /Catalog /Pages 2 0 R >>".to_vec(),
        b"<< /Type /Pages /Kids [3 0 R] /Count 1 >>".to_vec(),
        page.into_bytes(),
        pdf_stream_object(
            b"",
            // A filled rectangle covering the whole media box, so any crop is
            // visibly a crop rather than an empty margin.
            b"0 0 0 rg 0 0 612 792 re f\n",
        ),
    ])
}

/// Decode a PNG to (width, height) without pulling in a test-only decoder.
fn png_size(png: &[u8]) -> (u32, u32) {
    let img = image::load_from_memory(png).expect("rasterized output must decode as an image");
    (img.width(), img.height())
}

#[test]
fn rasterized_page_follows_the_crop_box_not_the_media_box() {
    // MediaBox 612x792 (US Letter), CropBox 100x200 at (10, 20).
    let pdf = page_with_boxes("0 0 612 792", Some("10 20 110 220"), None);
    let doc = PdfDocument::from_bytes(pdf, ParseOptions::default()).expect("parse");

    let png = doc
        .page_rasterized_png(0, 72.0)
        .expect("rasterize")
        .expect("a page with contents must rasterize");
    let (w, h) = png_size(&png);

    assert_eq!(
        (w, h),
        (100, 200),
        "at 72 dpi the raster must be the crop box (100x200), not the media box (612x792)"
    );
}

#[test]
fn rasterized_page_without_a_crop_box_uses_the_media_box() {
    let pdf = page_with_boxes("0 0 300 400", None, None);
    let doc = PdfDocument::from_bytes(pdf, ParseOptions::default()).expect("parse");

    let png = doc
        .page_rasterized_png(0, 72.0)
        .expect("rasterize")
        .expect("a page with contents must rasterize");
    assert_eq!(
        png_size(&png),
        (300, 400),
        "with no /CropBox the media box is the page"
    );
}

#[test]
fn rasterized_page_scales_the_crop_box_by_dpi() {
    let pdf = page_with_boxes("0 0 612 792", Some("0 0 100 100"), None);
    let doc = PdfDocument::from_bytes(pdf, ParseOptions::default()).expect("parse");

    let png = doc
        .page_rasterized_png(0, 144.0)
        .expect("rasterize")
        .expect("a page with contents must rasterize");
    assert_eq!(
        png_size(&png),
        (200, 200),
        "144 dpi is 2x, applied to the crop box"
    );
}

#[test]
fn a_rotated_page_swaps_the_rasterized_dimensions() {
    // /Rotate 90 means the output is the crop box turned on its side: a
    // 100x200 crop becomes 200x100.
    let pdf = page_with_boxes("0 0 612 792", Some("10 20 110 220"), Some(90));
    let doc = PdfDocument::from_bytes(pdf, ParseOptions::default()).expect("parse");

    let png = doc
        .page_rasterized_png(0, 72.0)
        .expect("rasterize")
        .expect("a page with contents must rasterize");
    assert_eq!(
        png_size(&png),
        (200, 100),
        "a 90-degree page rotation must swap the rasterized width and height"
    );
}

#[test]
fn an_unsupported_rotation_is_refused_rather_than_rendered_unrotated() {
    // PDF 32000-1 §7.7.3.3 allows only multiples of 90. A file claiming
    // otherwise is malformed, and returning the image unrotated would hand
    // downstream OCR a plausible-looking page rotated the wrong way.
    let pdf = page_with_boxes("0 0 612 792", Some("0 0 100 100"), Some(45));
    let doc = PdfDocument::from_bytes(pdf, ParseOptions::default()).expect("parse");

    match doc.page_rasterized_png(0, 72.0) {
        Err(error) => {
            let message = error.to_string();
            assert!(
                message.contains("rotation") || message.contains("rotate"),
                "the refusal must name the rotation: {message}"
            );
        }
        Ok(rendered) => panic!(
            "an unsupported /Rotate must not produce an image; got {rendered:?} \
             which would be silently unrotated"
        ),
    }
}
