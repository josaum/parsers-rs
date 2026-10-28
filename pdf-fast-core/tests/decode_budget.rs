use flate2::{write::ZlibEncoder, Compression};
use pdf_fast_core::{
    extract_page_vector_text_runs_checked, ParseOptions, PdfDocument, PdfStreamKind,
};
use std::io::Write;

fn zlib_compress(bytes: &[u8]) -> Vec<u8> {
    let mut encoder = ZlibEncoder::new(Vec::new(), Compression::default());
    encoder.write_all(bytes).unwrap();
    encoder.finish().unwrap()
}

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

fn pdf_with_form_chain(form_count: usize) -> Vec<u8> {
    assert!(form_count > 0);
    let mut objects = vec![
        b"<< /Type /Catalog /Pages 2 0 R >>".to_vec(),
        b"<< /Type /Pages /Kids [3 0 R] /Count 1 >>".to_vec(),
        b"<< /Type /Page /Parent 2 0 R /MediaBox [0 0 10 10] /Contents 4 0 R /Resources << /XObject << /Fm0 5 0 R >> >> >>".to_vec(),
        pdf_stream_object(b"", b"/Fm0 Do\n"),
    ];
    for index in 0..form_count {
        if index + 1 == form_count {
            objects.push(pdf_stream_object(
                b"/Type /XObject /Subtype /Form /BBox [0 0 10 10]",
                b"0 0 10 10 re S\n",
            ));
        } else {
            let next_name = format!("Fm{}", index + 1);
            let next_object = index + 6;
            let dictionary = format!(
                "/Type /XObject /Subtype /Form /BBox [0 0 10 10] /Resources << /XObject << /{next_name} {next_object} 0 R >> >>"
            );
            let body = format!("/{next_name} Do\n");
            objects.push(pdf_stream_object(dictionary.as_bytes(), body.as_bytes()));
        }
    }
    pdf_with_xref(objects)
}

fn pdf_with_self_referential_form() -> Vec<u8> {
    pdf_with_xref(vec![
        b"<< /Type /Catalog /Pages 2 0 R >>".to_vec(),
        b"<< /Type /Pages /Kids [3 0 R] /Count 1 >>".to_vec(),
        b"<< /Type /Page /Parent 2 0 R /MediaBox [0 0 10 10] /Contents 4 0 R /Resources << /XObject << /Fm0 5 0 R >> >> >>".to_vec(),
        pdf_stream_object(b"", b"/Fm0 Do\n"),
        pdf_stream_object(
            b"/Type /XObject /Subtype /Form /BBox [0 0 10 10] /Resources << /XObject << /Fm0 5 0 R >> >>",
            b"/Fm0 Do\n",
        ),
    ])
}

#[cfg(feature = "rasterize")]
fn vector_page_pdf(media_box: &str, rotate: Option<u32>, content: &[u8]) -> Vec<u8> {
    let rotate_entry = rotate
        .map(|degrees| format!(" /Rotate {degrees}"))
        .unwrap_or_default();
    pdf_with_xref(vec![
        b"<< /Type /Catalog /Pages 2 0 R >>".to_vec(),
        b"<< /Type /Pages /Kids [3 0 R] /Count 1 >>".to_vec(),
        format!(
            "<< /Type /Page /Parent 2 0 R /MediaBox {media_box}{rotate_entry} /Contents 4 0 R >>"
        )
        .into_bytes(),
        pdf_stream_object(b"", content),
    ])
}

#[cfg(feature = "rasterize")]
fn hayro_fallback_pdf() -> Vec<u8> {
    pdf_with_xref(vec![
        b"<< /Type /Catalog /Pages 2 0 R >>".to_vec(),
        b"<< /Type /Pages /Kids [3 0 R] /Count 1 >>".to_vec(),
        b"<< /Type /Page /Parent 2 0 R /MediaBox [0 0 10 10] /Contents 4 0 R >>".to_vec(),
        pdf_stream_object(b"", b"BT ET\n"),
    ])
}

#[test]
fn decoded_stream_limit_aborts_the_document_parse() {
    let expanded = vec![b'A'; 4096];
    let encoded = zlib_compress(&expanded);
    assert!(encoded.len() < 64);
    let mut pdf = format!(
        "%PDF-2.0\n1 0 obj\n<< /Filter /FlateDecode /Length {} >>\nstream\n",
        encoded.len()
    )
    .into_bytes();
    pdf.extend_from_slice(&encoded);
    pdf.extend_from_slice(b"\nendstream\nendobj\n%%EOF\n");

    let err = PdfDocument::from_bytes(
        pdf,
        ParseOptions {
            max_decoded_stream_len: 64,
            ..ParseOptions::default()
        },
    )
    .expect_err("decoded-stream exhaustion must abort the document parse");
    assert!(
        err.to_string().contains("max_decoded_stream_len 64"),
        "{err:#}"
    );
}

#[test]
fn second_stream_exhaustion_aborts_the_document_parse() {
    let expanded = vec![b'A'; 96];
    let encoded = zlib_compress(&expanded);
    let mut pdf = b"%PDF-2.0\n".to_vec();
    for object in 1..=2 {
        pdf.extend_from_slice(
            format!(
                "{object} 0 obj\n<< /Filter /FlateDecode /Length {} >>\nstream\n",
                encoded.len()
            )
            .as_bytes(),
        );
        pdf.extend_from_slice(&encoded);
        pdf.extend_from_slice(b"\nendstream\nendobj\n");
    }
    pdf.extend_from_slice(b"%%EOF\n");

    let err = PdfDocument::from_bytes(
        pdf,
        ParseOptions {
            max_decoded_stream_len: 128,
            max_total_decoded_bytes: 128,
            ..ParseOptions::default()
        },
    )
    .expect_err("two streams must not bypass the document budget");
    assert!(
        err.to_string().contains("max_total_decoded_bytes 128"),
        "{err:#}"
    );
}

#[test]
fn later_image_redecode_shares_the_document_decoded_byte_budget() {
    let decoded_image = vec![0xAA; 64];
    let compressed_image = zlib_compress(&decoded_image);
    let content = b"q\n8 0 0 8 0 0 cm\n/Im1 Do\nQ\n";
    let objects = vec![
        b"<< /Type /Catalog /Pages 2 0 R >>".to_vec(),
        b"<< /Type /Pages /Kids [3 0 R] /Count 1 >>".to_vec(),
        b"<< /Type /Page /Parent 2 0 R /MediaBox [0 0 8 8] /Contents 4 0 R /Resources << /XObject << /Im1 5 0 R >> >> >>".to_vec(),
        pdf_stream_object(b"", content),
        pdf_stream_object(
            b"/Type /XObject /Subtype /Image /Width 8 /Height 8 /BitsPerComponent 8 /ColorSpace /DeviceGray /Filter /FlateDecode",
            &compressed_image,
        ),
    ];
    let doc = PdfDocument::from_bytes(
        pdf_with_xref(objects),
        ParseOptions {
            max_decoded_stream_len: 128,
            max_total_decoded_bytes: 96,
            keep_decoded_streams: false,
            ..ParseOptions::default()
        },
    )
    .unwrap();

    let image_stream_index = doc
        .streams
        .iter()
        .position(|stream| stream.kind == PdfStreamKind::Image)
        .expect("image stream");
    assert!(
        doc.stream_decoded_bytes(image_stream_index).is_none(),
        "the image must require a later re-decode"
    );
    let err = doc
        .page_embedded_image(0)
        .expect_err("later decode must share the remaining document budget");
    assert!(
        err.to_string().contains("max_total_decoded_bytes 96"),
        "{err:#}"
    );
}

#[test]
fn nested_form_xobject_budget_exhaustion_reaches_vector_text_caller() {
    let nested_form = b"0 0 m 10 0 l 10 10 l h f\n";
    let compressed_nested_form = zlib_compress(nested_form);
    let objects = vec![
        b"<< /Type /Catalog /Pages 2 0 R >>".to_vec(),
        b"<< /Type /Pages /Kids [3 0 R] /Count 1 >>".to_vec(),
        b"<< /Type /Page /Parent 2 0 R /MediaBox [0 0 10 10] /Contents 4 0 R /Resources << /XObject << /FmA 5 0 R >> >> >>".to_vec(),
        pdf_stream_object(b"", b"/FmA Do\n"),
        pdf_stream_object(
            b"/Type /XObject /Subtype /Form /BBox [0 0 10 10] /Resources << /XObject << /FmB 6 0 R >> >>",
            b"/FmB Do\n",
        ),
        pdf_stream_object(
            b"/Type /XObject /Subtype /Form /BBox [0 0 10 10] /Filter /FlateDecode",
            &compressed_nested_form,
        ),
    ];
    let doc = PdfDocument::from_bytes(
        pdf_with_xref(objects),
        ParseOptions {
            max_decoded_stream_len: 64,
            max_total_decoded_bytes: 26,
            keep_decoded_streams: false,
            ..ParseOptions::default()
        },
    )
    .unwrap();

    let err = doc
        .page_vector_text_runs(0)
        .expect_err("nested Form XObject re-decode must fail the vector-text request");
    assert!(
        err.to_string().contains("max_total_decoded_bytes 26"),
        "{err:#}"
    );
}

#[test]
fn self_referential_form_aborts_vector_text_with_typed_limit() {
    let doc =
        PdfDocument::from_bytes(pdf_with_self_referential_form(), ParseOptions::default()).unwrap();

    let err = doc
        .page_vector_text_runs(0)
        .expect_err("a Form XObject cycle must abort vector-text traversal");
    assert!(
        err.to_string().contains("Form XObject traversal cycle"),
        "{err:#}"
    );
}

#[test]
fn public_checked_page_vector_extraction_propagates_form_cycle() {
    let doc =
        PdfDocument::from_bytes(pdf_with_self_referential_form(), ParseOptions::default()).unwrap();
    let page = doc.pages().into_iter().next().expect("fixture page");

    let error = extract_page_vector_text_runs_checked(&doc, &page)
        .expect_err("public checked page extraction must propagate a Form XObject cycle");
    assert!(
        error.to_string().contains("Form XObject traversal cycle"),
        "{error:#}"
    );
}

#[test]
fn checked_page_vector_text_string_propagates_form_cycle() {
    let doc =
        PdfDocument::from_bytes(pdf_with_self_referential_form(), ParseOptions::default()).unwrap();

    let error = doc
        .page_vector_text_as_string_checked(0)
        .expect_err("checked vector-text rendering must propagate a Form XObject cycle");
    assert!(
        error.to_string().contains("Form XObject traversal cycle"),
        "{error:#}"
    );
}

#[test]
fn checked_document_glyph_clusters_propagate_form_cycle() {
    let doc =
        PdfDocument::from_bytes(pdf_with_self_referential_form(), ParseOptions::default()).unwrap();

    let error = doc
        .vector_glyph_clusters_checked(0.01)
        .expect_err("document glyph clustering must propagate a Form XObject cycle");
    assert!(
        error.to_string().contains("Form XObject traversal cycle"),
        "{error:#}"
    );
}

#[test]
fn checked_document_glyph_descriptors_propagate_form_cycle() {
    let doc =
        PdfDocument::from_bytes(pdf_with_self_referential_form(), ParseOptions::default()).unwrap();

    let error = doc
        .vector_glyph_descriptors_checked(32)
        .expect_err("document glyph descriptors must propagate a Form XObject cycle");
    assert!(
        error.to_string().contains("Form XObject traversal cycle"),
        "{error:#}"
    );
}

#[test]
fn checked_document_descriptor_lines_propagate_form_cycle() {
    let doc =
        PdfDocument::from_bytes(pdf_with_self_referential_form(), ParseOptions::default()).unwrap();

    let error = doc
        .vector_glyph_descriptor_lines_checked(32, 2.0, true)
        .expect_err("document descriptor lines must propagate a Form XObject cycle");
    assert!(
        error.to_string().contains("Form XObject traversal cycle"),
        "{error:#}"
    );
}

#[test]
fn checked_document_cluster_lines_propagate_form_cycle() {
    let doc =
        PdfDocument::from_bytes(pdf_with_self_referential_form(), ParseOptions::default()).unwrap();

    let error = doc
        .vector_glyph_cluster_lines_checked(0.01, 2.0, true)
        .expect_err("document cluster lines must propagate a Form XObject cycle");
    assert!(
        error.to_string().contains("Form XObject traversal cycle"),
        "{error:#}"
    );
}

#[cfg(feature = "glyph-catalog")]
#[test]
fn checked_document_glyph_learning_propagates_form_cycle() {
    let doc =
        PdfDocument::from_bytes(pdf_with_self_referential_form(), ParseOptions::default()).unwrap();

    let error = pdf_fast_core::learn_document_glyph_mappings_checked(&doc)
        .expect_err("document glyph learning must propagate a Form XObject cycle");
    assert!(
        error.to_string().contains("Form XObject traversal cycle"),
        "{error:#}"
    );
}

#[test]
fn deep_form_chain_aborts_table_primitive_traversal() {
    let doc = PdfDocument::from_bytes(pdf_with_form_chain(32), ParseOptions::default()).unwrap();

    let err = doc
        .page_line_segments(0)
        .expect_err("a deep Form XObject chain must abort table traversal");
    assert!(
        err.to_string()
            .contains("Form XObject traversal exceeds max depth"),
        "{err:#}"
    );
}

#[test]
fn raw_form_clone_shares_document_resource_budget() {
    let mut form_body = vec![b' '; 96];
    form_body.extend_from_slice(b"0 0 10 10 re S\n");
    let objects = vec![
        b"<< /Type /Catalog /Pages 2 0 R >>".to_vec(),
        b"<< /Type /Pages /Kids [3 0 R] /Count 1 >>".to_vec(),
        b"<< /Type /Page /Parent 2 0 R /MediaBox [0 0 10 10] /Contents 4 0 R /Resources << /XObject << /Fm0 5 0 R >> >> >>".to_vec(),
        pdf_stream_object(b"", b"/Fm0 Do /Fm0 Do\n"),
        pdf_stream_object(
            b"/Type /XObject /Subtype /Form /BBox [0 0 10 10]",
            &form_body,
        ),
    ];
    let doc = PdfDocument::from_bytes(
        pdf_with_xref(objects),
        ParseOptions {
            max_total_decoded_bytes: 129,
            ..ParseOptions::default()
        },
    )
    .unwrap();

    let err = doc
        .page_line_segments(0)
        .expect_err("raw Form materialization must share the document budget");
    assert!(
        err.to_string().contains("max_total_decoded_bytes 129"),
        "{err:#}"
    );
}

#[cfg(feature = "rasterize")]
#[test]
fn deep_form_chain_aborts_fast_raster_traversal() {
    let doc = PdfDocument::from_bytes(pdf_with_form_chain(32), ParseOptions::default()).unwrap();

    let err = doc
        .page_rasterized_png(0, 72.0)
        .expect_err("a deep Form XObject chain must abort fast raster traversal");
    assert!(
        err.to_string()
            .contains("Form XObject traversal exceeds max depth"),
        "{err:#}"
    );
}

#[cfg(feature = "rasterize")]
#[test]
fn fast_raster_surface_shares_document_resource_budget() {
    let content = b"0 0 100 100 re f\n";
    let doc = PdfDocument::from_bytes(
        vector_page_pdf("[0 0 100 100]", None, content),
        ParseOptions {
            max_total_decoded_bytes: 1_024,
            ..ParseOptions::default()
        },
    )
    .unwrap();

    let err = doc
        .page_rasterized_png(0, 72.0)
        .expect_err("fast Pixmap allocation must share the document budget");
    assert!(
        err.to_string().contains("max_total_decoded_bytes 1024"),
        "{err:#}"
    );
}

#[cfg(feature = "rasterize")]
#[test]
fn fast_raster_png_output_shares_document_resource_budget() {
    let content = b"0 0 100 100 re f\n";
    let doc = PdfDocument::from_bytes(
        vector_page_pdf("[0 0 100 100]", None, content),
        ParseOptions {
            max_total_decoded_bytes: content.len() + 80_007,
            ..ParseOptions::default()
        },
    )
    .unwrap();

    let err = doc
        .page_rasterized_png(0, 72.0)
        .expect_err("fast PNG retention must share the document budget");
    assert!(
        err.to_string().contains("max_total_decoded_bytes 80024"),
        "{err:#}"
    );
}

#[cfg(feature = "rasterize")]
#[test]
fn fast_raster_rotation_shares_document_resource_budget() {
    let content = b"10 20 30 100 re f\n";
    let doc = PdfDocument::from_bytes(
        vector_page_pdf("[0 0 72 144]", Some(90), content),
        ParseOptions {
            max_total_decoded_bytes: content.len() + 124_416 + 4_096,
            ..ParseOptions::default()
        },
    )
    .unwrap();

    let err = doc
        .page_rasterized_png(0, 72.0)
        .expect_err("rotation decode and output surfaces must share the document budget");
    assert!(
        err.to_string().contains("max_total_decoded_bytes 128530"),
        "{err:#}"
    );
}

#[cfg(feature = "rasterize")]
#[test]
fn hayro_fallback_charges_reconstructed_document_before_rendering() {
    let doc = PdfDocument::from_bytes(
        hayro_fallback_pdf(),
        ParseOptions {
            max_total_decoded_bytes: 64,
            ..ParseOptions::default()
        },
    )
    .unwrap();

    let err = doc
        .page_rasterized_png(0, 72.0)
        .expect_err("Hayro's document reconstruction must share the document resource budget");
    assert!(
        err.to_string().contains("max_total_decoded_bytes 64"),
        "{err:#}"
    );
}

#[cfg(feature = "rasterize")]
#[test]
fn checked_interpreted_raster_propagates_budget_exhaustion() {
    let doc = PdfDocument::from_bytes(
        hayro_fallback_pdf(),
        ParseOptions {
            max_total_decoded_bytes: 64,
            ..ParseOptions::default()
        },
    )
    .unwrap();

    let error = doc
        .page_rasterized_png_interpreted_checked(0, 72.0)
        .expect_err("checked interpreted raster must propagate resource exhaustion");
    assert!(
        error.to_string().contains("max_total_decoded_bytes 64"),
        "{error:#}"
    );
}
