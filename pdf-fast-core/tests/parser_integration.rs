#[cfg(feature = "arrow")]
use arrow::array::{BinaryArray, StringArray, UInt32Array};
use flate2::{write::ZlibEncoder, Compression};
#[cfg(feature = "rasterize")]
use image::GenericImageView;
#[cfg(feature = "rasterize")]
use pdf_fast_core::{bitmap_components, BitmapComponentOptions};
use pdf_fast_core::{DecodingPath, ParseOptions, PdfDocument, PdfStreamKind};
use std::fs;
use std::io::Write;
use std::path::{Path, PathBuf};
use tempfile::tempdir;

fn write_pdf_fixture(name: &str, bytes: &[u8]) -> (tempfile::TempDir, PathBuf) {
    let dir = tempdir().unwrap();
    let path = dir.path().join(name);
    fs::write(&path, bytes).unwrap();
    (dir, path)
}

fn zlib_compress(bytes: &[u8]) -> Vec<u8> {
    let mut encoder = ZlibEncoder::new(Vec::new(), Compression::default());
    encoder.write_all(bytes).unwrap();
    encoder.finish().unwrap()
}

fn open_pdf(path: &Path) -> PdfDocument {
    PdfDocument::open(path, ParseOptions::default()).unwrap()
}

/// A stream that is present and over budget aborts the parse.
///
/// `tests/decode_budget.rs` holds the same fixture with the same expectation.
/// Both exist on purpose: this file is the integration surface a consumer
/// reads, and the contract is worth stating where a consumer looks. The
/// alternative — succeed and mark the one stream `DecodeStatus::Failed` —
/// returns a document that looks complete while one stream is silently
/// missing, which is the failure mode this parser refuses everywhere else.
#[test]
fn an_over_budget_stream_aborts_the_parse_rather_than_failing_quietly() {
    let expanded = vec![b'A'; 4096];
    let encoded = zlib_compress(&expanded);
    assert!(encoded.len() < 64, "the fixture must compress below the limit");
    let mut pdf = format!(
        "%PDF-2.0\n1 0 obj\n<< /Filter /FlateDecode /Length {} >>\nstream\n",
        encoded.len()
    )
    .into_bytes();
    pdf.extend_from_slice(&encoded);
    pdf.extend_from_slice(b"\nendstream\nendobj\n%%EOF\n");

    let error = PdfDocument::from_bytes(
        pdf,
        ParseOptions {
            max_decoded_stream_len: 64,
            ..ParseOptions::default()
        },
    )
    .expect_err("an over-budget stream must abort the parse");
    assert!(
        error.to_string().contains("max_decoded_stream_len 64"),
        "the error must name the limit it exceeded: {error:#}"
    );
}

#[test]
fn accepts_pdf_headers_from_1_0_through_2_0() {
    for version in [
        "1.0", "1.1", "1.2", "1.3", "1.4", "1.5", "1.6", "1.7", "2.0",
    ] {
        let pdf = format!("%PDF-{version}\n1 0 obj\n<< /Type /Catalog >>\nendobj\n%%EOF\n");
        let doc = PdfDocument::from_slice(pdf.as_bytes(), ParseOptions::default()).unwrap();
        assert_eq!(
            doc.header.as_deref(),
            Some(format!("%PDF-{version}").as_str())
        );
        assert_eq!(doc.objects.len(), 1);
    }
}

#[test]
fn applies_png_predictor_after_flate_decode() {
    let content = b"BT /F1 12 Tf 10 80 Td (Predicted) Tj ET";
    let mut predicted = Vec::with_capacity(content.len() + 1);
    predicted.push(0); // PNG None predictor for this row.
    predicted.extend_from_slice(content);
    let encoded = zlib_compress(&predicted);

    let mut pdf = format!(
        "%PDF-1.7\n\
         1 0 obj\n<< /Type /Catalog /Pages 2 0 R >>\nendobj\n\
         2 0 obj\n<< /Type /Pages /Kids [3 0 R] /Count 1 >>\nendobj\n\
         3 0 obj\n<< /Type /Page /Parent 2 0 R /Contents 4 0 R >>\nendobj\n\
         4 0 obj\n<< /Filter /FlateDecode /DecodeParms << /Predictor 12 /Columns {} /Colors 1 /BitsPerComponent 8 >> /Length {} >>\nstream\n",
        content.len(),
        encoded.len()
    )
    .into_bytes();
    pdf.extend_from_slice(&encoded);
    pdf.extend_from_slice(b"\nendstream\nendobj\n%%EOF\n");

    let doc = PdfDocument::from_bytes(pdf, ParseOptions::default()).unwrap();
    assert_eq!(doc.stream_decoded_bytes(0), Some(content.as_slice()));
    assert_eq!(doc.text_fragments[0].text, "Predicted");
}

#[test]
fn applies_decode_parms_at_the_matching_filter_index() {
    let content = b"BT (Filter array) Tj ET";
    let mut predicted = vec![0];
    predicted.extend_from_slice(content);
    let compressed = zlib_compress(&predicted);
    let ascii_hex = compressed
        .iter()
        .map(|byte| format!("{byte:02X}"))
        .collect::<String>()
        + ">";
    let pdf = format!(
        "%PDF-2.0\n\
         1 0 obj\n<< /Filter [/ASCIIHexDecode /FlateDecode] \
         /DecodeParms [null << /Predictor 12 /Columns {} /Colors 1 /BitsPerComponent 8 >>] \
         /Length {} >>\nstream\n{}\nendstream\nendobj\n%%EOF\n",
        content.len(),
        ascii_hex.len(),
        ascii_hex
    );

    let doc = PdfDocument::from_slice(pdf.as_bytes(), ParseOptions::default()).unwrap();
    assert_eq!(doc.stream_decoded_bytes(0), Some(content.as_slice()));
    assert_eq!(doc.text_fragments[0].text, "Filter array");
}

#[test]
fn resolves_indirect_stream_length_without_mistaking_payload_for_endstream() {
    let content = b"BT /F1 12 Tf 10 80 Td (literal endstream token) Tj ET";
    let mut pdf = b"%PDF-1.2\n\
         1 0 obj\n<< /Type /Catalog /Pages 2 0 R >>\nendobj\n\
         2 0 obj\n<< /Type /Pages /Kids [3 0 R] /Count 1 >>\nendobj\n\
         3 0 obj\n<< /Type /Page /Parent 2 0 R /Contents 4 0 R >>\nendobj\n\
         4 0 obj\n<< /Length 9 0 R >>\nstream\n"
        .to_vec();
    pdf.extend_from_slice(content);
    pdf.extend_from_slice(
        format!(
            "\nendstream\nendobj\n9 0 obj\n{}\nendobj\n%%EOF\n",
            content.len()
        )
        .as_bytes(),
    );

    let doc = PdfDocument::from_bytes(pdf, ParseOptions::default()).unwrap();
    assert_eq!(doc.stream_raw_bytes(0), Some(content.as_slice()));
    assert_eq!(doc.text_fragments[0].text, "literal endstream token");
}

#[test]
fn reads_page_dictionary_from_flate_compressed_object_stream() {
    let page = b"<< /Type /Page /Parent 2 0 R /Contents 4 0 R >>";
    let object_header = b"3 0 ";
    let first = object_header.len();
    let mut object_stream = object_header.to_vec();
    object_stream.extend_from_slice(page);
    let compressed_objects = zlib_compress(&object_stream);
    let content = b"BT /F1 12 Tf 10 80 Td (Object stream page) Tj ET";

    let mut pdf = format!(
        "%PDF-1.5\n\
         1 0 obj\n<< /Type /Catalog /Pages 2 0 R >>\nendobj\n\
         2 0 obj\n<< /Type /Pages /Kids [3 0 R] /Count 1 >>\nendobj\n\
         4 0 obj\n<< /Length {} >>\nstream\n{}\nendstream\nendobj\n\
         8 0 obj\n<< /Type /ObjStm /N 1 /First {} /Filter /FlateDecode /Length {} >>\nstream\n",
        content.len(),
        String::from_utf8_lossy(content),
        first,
        compressed_objects.len()
    )
    .into_bytes();
    pdf.extend_from_slice(&compressed_objects);
    pdf.extend_from_slice(b"\nendstream\nendobj\n%%EOF\n");

    let doc = PdfDocument::from_bytes(pdf, ParseOptions::default()).unwrap();
    assert!(doc.object_by_id(3, 0).is_some());
    let pages = doc.pages();
    assert_eq!(pages.len(), 1);
    let runs = doc.semantic_text_runs_for_page(&pages[0]);
    assert_eq!(runs[0].text, "Object stream page");
}

#[test]
fn opens_pdf_from_path_and_decodes_flate_semantic_page_text() {
    let content = b"BT\n/F1 12 Tf\n72 720 Td\n(Compressed Path) Tj\nET\n";
    let encoded = zlib_compress(content);

    let mut pdf = format!(
        "%PDF-1.7\n1 0 obj\n<< /Type /Catalog /Pages 2 0 R >>\nendobj\n2 0 obj\n<< /Type /Pages /Kids [3 0 R] /Count 1 >>\nendobj\n3 0 obj\n<< /Type /Page /Parent 2 0 R /Contents 4 0 R >>\nendobj\n4 0 obj\n<< /Filter /FlateDecode /Length {} >>\nstream\n",
        encoded.len()
    )
    .into_bytes();
    pdf.extend_from_slice(&encoded);
    pdf.extend_from_slice(b"\nendstream\nendobj\n");

    let (_dir, path) = write_pdf_fixture("compressed-page.pdf", &pdf);
    let doc = open_pdf(&path);

    assert_eq!(doc.streams.len(), 1);
    assert_eq!(doc.streams[0].kind, PdfStreamKind::Content);
    assert_eq!(doc.stream_raw_bytes(0).unwrap().len(), encoded.len());
    let decoded = std::str::from_utf8(doc.stream_decoded_bytes(0).unwrap()).unwrap();
    assert!(decoded.contains("Compressed Path"));

    let pages = doc.pages();
    assert_eq!(pages.len(), 1);
    let runs = doc.semantic_text_runs_for_page(&pages[0]);
    assert_eq!(runs.len(), 1);
    assert_eq!(runs[0].text, "Compressed Path");
}

#[test]
#[cfg(feature = "arrow")]
fn opens_pdf_from_path_and_exposes_embedded_images_in_multimodal_batch() {
    let jpeg = vec![
        0xFF, 0xD8, 0xFF, 0xE0, 0x00, 0x10, b'J', b'F', b'I', b'F', 0x00,
    ];
    let content =
        "BT\n/F1 12 Tf\n10 180 Td\n(Invoice 42) Tj\nET\nq\n10 0 0 20 5 7 cm\n/Im1 Do\nQ\n";

    let mut pdf = format!(
        "%PDF-1.7\n1 0 obj\n<< /Type /Catalog /Pages 2 0 R >>\nendobj\n2 0 obj\n<< /Type /Pages /Kids [3 0 R] /Count 1 >>\nendobj\n3 0 obj\n<< /Type /Page /Parent 2 0 R /MediaBox [0 0 200 200] /Contents 4 0 R /Resources 5 0 R >>\nendobj\n4 0 obj\n<< /Length {} >>\nstream\n{}endstream\nendobj\n5 0 obj\n<< /Font << /F1 7 0 R >> /XObject << /Im1 6 0 R >> >>\nendobj\n6 0 obj\n<< /Subtype /Image /Filter /DCTDecode /Width 1 /Height 1 /ColorSpace /DeviceRGB /BitsPerComponent 8 /Length {} >>\nstream\n",
        content.len(),
        content,
        jpeg.len()
    )
    .into_bytes();
    pdf.extend_from_slice(&jpeg);
    pdf.extend_from_slice(
        br#"
endstream
endobj
7 0 obj
<< /Type /Font /Subtype /Type1 /BaseFont /Helvetica >>
endobj
"#,
    );

    let (_dir, path) = write_pdf_fixture("image-page.pdf", &pdf);
    let doc = open_pdf(&path);

    let placements = doc.page_image_placements(0).unwrap();
    assert_eq!(placements.len(), 1);
    assert_eq!(placements[0].name, "Im1");
    assert_eq!(placements[0].object_number, 6);
    assert!((placements[0].x - 5.0).abs() < 0.01);
    assert!((placements[0].y - 7.0).abs() < 0.01);
    assert!((placements[0].w - 10.0).abs() < 0.01);
    assert!((placements[0].h - 20.0).abs() < 0.01);

    let (bytes, format) = doc.page_embedded_image(0).unwrap().unwrap();
    assert_eq!(format.as_deref(), Some("jpeg"));
    assert_eq!(bytes, jpeg);

    let batch = doc.page_multimodal_record_batch(0).unwrap();
    assert_eq!(batch.num_rows(), 1);

    let input_text = batch
        .column_by_name("input_text")
        .unwrap()
        .as_any()
        .downcast_ref::<StringArray>()
        .unwrap();
    let image_format = batch
        .column_by_name("image_format")
        .unwrap()
        .as_any()
        .downcast_ref::<StringArray>()
        .unwrap();
    let image_bytes = batch
        .column_by_name("image_bytes")
        .unwrap()
        .as_any()
        .downcast_ref::<BinaryArray>()
        .unwrap();

    assert_eq!(input_text.value(0), "Invoice 42");
    assert_eq!(image_format.value(0), "jpeg");
    assert_eq!(image_bytes.value(0), jpeg.as_slice());
}

#[test]
#[cfg(feature = "rasterize")]
fn opens_ccitt_g4_scanned_page_as_embedded_png() {
    // CCITT G4 bitstream for one all-white 8px row, followed by EOFB.
    let ccitt = vec![0x80, 0x10, 0x00, 0x80];
    let content = "q\n8 0 0 1 0 0 cm\n/Im1 Do\nQ\n";

    let mut pdf = format!(
        "%PDF-1.7\n1 0 obj\n<< /Type /Catalog /Pages 2 0 R >>\nendobj\n2 0 obj\n<< /Type /Pages /Kids [3 0 R] /Count 1 >>\nendobj\n3 0 obj\n<< /Type /Page /Parent 2 0 R /MediaBox [0 0 8 1] /Contents 4 0 R /Resources 5 0 R >>\nendobj\n4 0 obj\n<< /Length {} >>\nstream\n{}endstream\nendobj\n5 0 obj\n<< /XObject << /Im1 6 0 R >> >>\nendobj\n6 0 obj\n<< /Subtype /Image /Filter /CCITTFaxDecode /DecodeParms << /K -1 /Columns 8 /Rows 1 >> /Width 8 /Height 1 /ColorSpace /DeviceGray /BitsPerComponent 1 /Length {} >>\nstream\n",
        content.len(),
        content,
        ccitt.len()
    )
    .into_bytes();
    pdf.extend_from_slice(&ccitt);
    pdf.extend_from_slice(b"\nendstream\nendobj\n");

    let (_dir, path) = write_pdf_fixture("ccitt-g4-page.pdf", &pdf);
    let doc = open_pdf(&path);

    let (bytes, format) = doc.page_embedded_image(0).unwrap().unwrap();
    assert_eq!(format.as_deref(), Some("png"));
    assert_eq!(&bytes[..8], b"\x89PNG\r\n\x1a\n");

    let image = image::load_from_memory(&bytes).unwrap();
    assert_eq!(image.dimensions(), (8, 1));
    assert!(image.to_luma8().pixels().all(|pixel| pixel[0] == 0xFF));
}

#[test]
#[cfg(feature = "rasterize")]
fn rasterizes_flate_indexed_image_page_for_ocr() {
    use flate2::write::ZlibEncoder;
    use flate2::Compression;
    use image::GenericImageView;
    use std::io::Write;

    let width = 8usize;
    let height = 8usize;
    let indices = (0..width * height)
        .map(|index| u8::from((index / width + index % width).is_multiple_of(2)))
        .collect::<Vec<_>>();
    let mut encoder = ZlibEncoder::new(Vec::new(), Compression::default());
    encoder.write_all(&indices).unwrap();
    let compressed = encoder.finish().unwrap();

    let content = format!("q\n{width} 0 0 {height} 0 0 cm\n/Im1 Do\nQ\n");
    let objects = vec![
        b"<< /Type /Catalog /Pages 2 0 R >>".to_vec(),
        b"<< /Type /Pages /Kids [3 0 R] /Count 1 >>".to_vec(),
        format!(
            "<< /Type /Page /Parent 2 0 R /MediaBox [0 0 {width} {height}] /Contents 4 0 R /Resources << /XObject << /Im1 5 0 R >> >> >>"
        )
        .into_bytes(),
        pdf_stream_object(b"", content.as_bytes()),
        pdf_stream_object(
            format!(
                "/Type /XObject /Subtype /Image /Width {width} /Height {height} /BitsPerComponent 8 /ColorSpace [/Indexed /DeviceRGB 1 <FF000000FF00>] /Filter /FlateDecode"
            )
            .as_bytes(),
            &compressed,
        ),
    ];
    let pdf = pdf_with_xref(objects);
    let (_dir, path) = write_pdf_fixture("flate-indexed-image-page.pdf", &pdf);
    let doc = open_pdf(&path);

    assert_eq!(
        doc.page_embedded_image(0).unwrap().unwrap().1.as_deref(),
        Some("raw")
    );
    let png = doc.page_rasterized_png(0, 72.0).unwrap().unwrap();
    let rendered = image::load_from_memory(&png).unwrap();

    assert_eq!(rendered.dimensions(), (width as u32, height as u32));
    assert!(rendered
        .to_rgb8()
        .pixels()
        .any(|pixel| pixel.0 != [255, 255, 255]));
}

#[cfg(feature = "rasterize")]
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

#[cfg(feature = "rasterize")]
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

#[test]
#[cfg(feature = "rasterize")]
fn extracts_bitmap_components_from_luma_without_ocr_engine() {
    let mut pixels = vec![0xFF; 12 * 8];
    for y in 1..6 {
        for x in 1..4 {
            pixels[y * 12 + x] = 0x00;
        }
    }
    for y in 2..7 {
        for x in 7..10 {
            pixels[y * 12 + x] = 0x00;
        }
    }

    let page =
        bitmap_components::components_from_luma(12, 8, &pixels, BitmapComponentOptions::default())
            .unwrap();

    assert_eq!(page.candidates.len(), 2);
    assert_eq!(page.lines.len(), 1);
    assert_eq!(page.lines[0].candidate_indices, vec![0, 1]);
    assert_eq!(page.candidates[0].x, 1);
    assert_eq!(page.candidates[1].x, 7);
}

#[test]
#[cfg(feature = "rasterize")]
fn opens_pdf_from_path_and_applies_rotate_to_rasterized_png() {
    let pdf = br#"%PDF-1.7
1 0 obj
<< /Type /Catalog /Pages 2 0 R >>
endobj
2 0 obj
<< /Type /Pages /Kids [3 0 R] /Count 1 >>
endobj
3 0 obj
<< /Type /Page /Parent 2 0 R /MediaBox [0 0 72 144] /Rotate 90 /Contents 4 0 R >>
endobj
4 0 obj
<< /Length 27 >>
stream
10 20 30 100 re
f
endstream
endobj
"#;

    let (_dir, path) = write_pdf_fixture("rotated-page.pdf", pdf);
    let doc = open_pdf(&path);

    let png = doc.page_rasterized_png(0, 72.0).unwrap().unwrap();
    let image = image::load_from_memory(&png).unwrap();
    assert_eq!(image.dimensions(), (144, 72));
}

#[test]
#[cfg(feature = "arrow")]
fn opens_pdf_from_path_and_materializes_table_batches() {
    let content = "0 0 100 100 re S\n0 50 m 100 50 l S\n50 0 m 50 50 l S\nBT\n/F1 12 Tf\n10 80 Td\n(HEADER) Tj\nET\nBT\n/F1 12 Tf\n10 30 Td\n(A2) Tj\nET\nBT\n/F1 12 Tf\n60 30 Td\n(B2) Tj\nET\n";
    let pdf = format!(
        "%PDF-1.7\n1 0 obj\n<< /Type /Catalog /Pages 2 0 R >>\nendobj\n2 0 obj\n<< /Type /Pages /Kids [3 0 R] /Count 1 >>\nendobj\n3 0 obj\n<< /Type /Page /Parent 2 0 R /MediaBox [0 0 100 100] /Contents 4 0 R >>\nendobj\n4 0 obj\n<< /Length {} >>\nstream\n{}endstream\nendobj\n",
        content.len(),
        content
    );

    let (_dir, path) = write_pdf_fixture("table-page.pdf", pdf.as_bytes());
    let doc = open_pdf(&path);

    let tables = doc.page_tables(0, 2.0).unwrap();
    assert_eq!(tables.len(), 1);
    assert_eq!(tables[0].header_row_count, 1);
    assert_eq!(tables[0].col_count, 2);
    assert!(tables[0].markdown.contains("| HEADER | HEADER |"));
    assert!(tables[0].markdown.contains("| A2 | B2 |"));

    let batch = doc.page_table_record_batch(0, 2.0).unwrap();
    assert_eq!(batch.num_rows(), 1);

    let header_row_count = batch
        .column_by_name("header_row_count")
        .unwrap()
        .as_any()
        .downcast_ref::<UInt32Array>()
        .unwrap();
    let markdown = batch
        .column_by_name("markdown")
        .unwrap()
        .as_any()
        .downcast_ref::<StringArray>()
        .unwrap();

    assert_eq!(header_row_count.value(0), 1);
    assert!(markdown.value(0).contains("| HEADER | HEADER |"));
    assert!(markdown.value(0).contains("| A2 | B2 |"));
}

/// Acrobat Distiller 3.0 era PDFs (e.g. CBHPM 5ª ed) embed Type 1 fonts that
/// rely on `/Encoding /Differences` overrides instead of `/ToUnicode` CMaps.
/// Verify that we resolve high-ASCII bytes through the encoding map and tag
/// the resulting run with `DecodingPath::EncodingDifferences`.
#[test]
fn opens_pdf_with_type1_differences_encoding_and_decodes_portuguese_accents() {
    // Content stream: write three high-ASCII bytes that — through the
    // Differences array below — should resolve to "São".  Bytes are written
    // as octal escapes inside the Tj literal: \123 = 'S', \343 = /atilde,
    // \157 = 'o'.
    let content = b"BT\n/F1 12 Tf\n10 80 Td\n(S\xe3o) Tj\nET\n";
    let encoded = zlib_compress(content);

    let pdf_template = format!(
        "%PDF-1.4\n\
         1 0 obj\n<< /Type /Catalog /Pages 2 0 R >>\nendobj\n\
         2 0 obj\n<< /Type /Pages /Kids [3 0 R] /Count 1 >>\nendobj\n\
         3 0 obj\n<< /Type /Page /Parent 2 0 R /MediaBox [0 0 200 200] /Contents 4 0 R /Resources 5 0 R >>\nendobj\n\
         4 0 obj\n<< /Filter /FlateDecode /Length {flen} >>\nstream\n",
        flen = encoded.len()
    );

    let mut pdf = pdf_template.into_bytes();
    pdf.extend_from_slice(&encoded);
    pdf.extend_from_slice(
        b"\nendstream\nendobj\n\
          5 0 obj\n<< /Font << /F1 6 0 R >> >>\nendobj\n\
          6 0 obj\n<< /Type /Font /Subtype /Type1 /BaseFont /Times-Roman /Encoding 7 0 R >>\nendobj\n\
          7 0 obj\n<< /Type /Encoding /Differences [227 /atilde] >>\nendobj\n",
    );

    let (_dir, path) = write_pdf_fixture("type1-differences.pdf", &pdf);
    let doc = open_pdf(&path);

    let pages = doc.pages();
    assert_eq!(pages.len(), 1);
    let runs = doc.semantic_text_runs_for_page(&pages[0]);
    assert_eq!(runs.len(), 1, "expected exactly one run, got {:?}", runs);
    assert_eq!(runs[0].text, "São");
    assert_eq!(runs[0].cmap_fallback_chars, 0);
    assert_eq!(runs[0].decoding_path, DecodingPath::EncodingDifferences);
}

/// Helvetica without an explicit `/Encoding` should still decode ASCII text
/// via the implicit Standard-14 encoding instead of falling through to the
/// raw-byte path.
#[test]
fn standard14_helvetica_without_encoding_uses_implicit_standard_encoding() {
    let content = b"BT\n/F1 12 Tf\n10 80 Td\n(Hello) Tj\nET\n";
    let encoded = zlib_compress(content);

    let mut pdf = format!(
        "%PDF-1.7\n\
         1 0 obj\n<< /Type /Catalog /Pages 2 0 R >>\nendobj\n\
         2 0 obj\n<< /Type /Pages /Kids [3 0 R] /Count 1 >>\nendobj\n\
         3 0 obj\n<< /Type /Page /Parent 2 0 R /MediaBox [0 0 200 200] /Contents 4 0 R /Resources 5 0 R >>\nendobj\n\
         4 0 obj\n<< /Filter /FlateDecode /Length {} >>\nstream\n",
        encoded.len()
    )
    .into_bytes();
    pdf.extend_from_slice(&encoded);
    pdf.extend_from_slice(
        b"\nendstream\nendobj\n\
          5 0 obj\n<< /Font << /F1 6 0 R >> >>\nendobj\n\
          6 0 obj\n<< /Type /Font /Subtype /Type1 /BaseFont /Helvetica >>\nendobj\n",
    );

    let (_dir, path) = write_pdf_fixture("helvetica-implicit.pdf", &pdf);
    let doc = open_pdf(&path);
    let pages = doc.pages();
    let runs = doc.semantic_text_runs_for_page(&pages[0]);
    assert_eq!(runs.len(), 1);
    assert_eq!(runs[0].text, "Hello");
    assert_eq!(runs[0].decoding_path, DecodingPath::StandardImplicit);
}

/// LZWDecode is the default content-stream filter for Acrobat 3.0-era PDFs.
/// Verify that streams compressed with LZW are decoded and classified as
/// content streams, and that text fragments are extracted from them.
#[test]
fn decodes_lzw_content_stream_and_extracts_text_fragments() {
    use std::collections::HashMap;

    /// Tiny LZW encoder with the same bump timing as the production decoder
    /// (early_change=1).  Used here only to manufacture a fixture stream.
    fn encode_pdf_lzw(input: &[u8]) -> Vec<u8> {
        let mut bytes: Vec<u8> = Vec::new();
        let mut bit_pos: u8 = 0;
        let write_bits = |value: u32, n: u8, bytes: &mut Vec<u8>, bit_pos: &mut u8| {
            let mut value = value & ((1u64 << n) as u32 - 1);
            let mut n = n;
            while n > 0 {
                if *bit_pos == 0 {
                    bytes.push(0);
                }
                let avail = 8 - *bit_pos;
                let take = avail.min(n);
                let shift = n - take;
                let frag = ((value >> shift) & ((1u32 << take) - 1)) as u8;
                let last = bytes.last_mut().unwrap();
                *last |= frag << (avail - take);
                *bit_pos = (*bit_pos + take) & 7;
                n -= take;
                if shift == 0 {
                    value = 0;
                } else {
                    value &= (1u32 << shift) - 1;
                }
            }
        };

        let mut code_len: u8 = 9;
        let mut next_code: u32 = 258;
        let mut dict: HashMap<Vec<u8>, u32> = HashMap::new();
        for i in 0u32..256 {
            dict.insert(vec![i as u8], i);
        }
        write_bits(256, code_len, &mut bytes, &mut bit_pos);
        let mut w: Vec<u8> = Vec::new();
        for &byte in input {
            let mut wb = w.clone();
            wb.push(byte);
            if dict.contains_key(&wb) {
                w = wb;
            } else {
                let prev_code = *dict.get(&w).unwrap();
                write_bits(prev_code, code_len, &mut bytes, &mut bit_pos);
                if next_code < 4096 {
                    dict.insert(wb, next_code);
                    next_code += 1;
                    if code_len < 12 && next_code >= (1u32 << code_len) {
                        code_len += 1;
                    }
                }
                w = vec![byte];
            }
        }
        if !w.is_empty() {
            let last = *dict.get(&w).unwrap();
            write_bits(last, code_len, &mut bytes, &mut bit_pos);
        }
        write_bits(257, code_len, &mut bytes, &mut bit_pos);
        bytes
    }

    let content = b"BT\n/F1 12 Tf\n10 80 Td\n(LZW Hello) Tj\nET\n";
    let encoded = encode_pdf_lzw(content);

    let mut pdf = format!(
        "%PDF-1.3\n\
         1 0 obj\n<< /Type /Catalog /Pages 2 0 R >>\nendobj\n\
         2 0 obj\n<< /Type /Pages /Kids [3 0 R] /Count 1 >>\nendobj\n\
         3 0 obj\n<< /Type /Page /Parent 2 0 R /MediaBox [0 0 200 200] /Contents 4 0 R /Resources 5 0 R >>\nendobj\n\
         4 0 obj\n<< /Filter /LZWDecode /Length {} >>\nstream\n",
        encoded.len()
    )
    .into_bytes();
    pdf.extend_from_slice(&encoded);
    pdf.extend_from_slice(
        b"\nendstream\nendobj\n\
          5 0 obj\n<< /Font << /F1 6 0 R >> >>\nendobj\n\
          6 0 obj\n<< /Type /Font /Subtype /Type1 /BaseFont /Helvetica >>\nendobj\n",
    );

    let (_dir, path) = write_pdf_fixture("lzw-content.pdf", &pdf);
    let doc = open_pdf(&path);
    assert_eq!(doc.streams.len(), 1);
    assert_eq!(doc.streams[0].kind, PdfStreamKind::Content);
    let pages = doc.pages();
    let runs = doc.semantic_text_runs_for_page(&pages[0]);
    assert_eq!(runs.len(), 1);
    assert_eq!(runs[0].text, "LZW Hello");
}
