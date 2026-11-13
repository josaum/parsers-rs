//! Malformed PDFs must be rejected with an error, never a panic, a stack
//! overflow, or an allocation the input cannot justify.
//!
//! Three kinds of test live here:
//!
//! - **Mutation runs** feed fixed-seed mutations of realistic documents to the
//!   parser and then to the extraction API, because most of the code that reads
//!   a malformed file runs after the parse has succeeded.
//! - **Layered mutation runs** mutate a payload *before* it is compressed and
//!   embedded. A mutation inside a Flate stream only breaks the checksum, so
//!   the content interpreter, CMap parser, object-stream expander and font
//!   readers behind it would otherwise never see a malformed byte.
//! - **Explicit regressions** for inputs random mutation almost never builds:
//!   deep nesting, reference cycles, and small inputs that expand without bound.
//!
//! Everything is deterministic: fixed seeds and fixed iteration counts. Two
//! environment variables exist for local investigation only and change nothing
//! when unset: `PDF_SMOKE_SCALE` multiplies every iteration count, and
//! `PDF_SMOKE_DUMP` names a file that receives each document just before it is
//! parsed, so the input behind an abort or a hang can be recovered.

#[path = "../../tools/mutation_smoke.rs"]
mod smoke;

use std::io::Write;
use std::sync::{Mutex, MutexGuard};

use flate2::write::ZlibEncoder;
use flate2::Compression;
use pdf_fast_core::cff::CffFont;
use pdf_fast_core::tessellation::TessellationOptions;
use pdf_fast_core::truetype::TrueTypeFont;
use pdf_fast_core::{lzw, ParseOptions, PdfDocument};

#[global_allocator]
static ALLOCATOR: smoke::CappedAllocator = smoke::CappedAllocator;

const TRUETYPE_FIXTURE: &[u8] = include_bytes!("fixtures/DejaVuSans-subset.ttf");
const OPENTYPE_FIXTURE: &[u8] = include_bytes!("fixtures/Cabin-subset.otf");

/// Serializes the tests in this binary.
///
/// The shared harness swaps the process-wide panic hook, and the capped
/// allocator counts live bytes for the whole process. Run in parallel, one
/// test's panic report is swallowed by another's hook and the transient
/// buffers of several tests add up toward a cap none of them reaches alone.
fn serial() -> MutexGuard<'static, ()> {
    static SERIAL: Mutex<()> = Mutex::new(());
    SERIAL
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
}

/// Iteration count for a mutation run, scaled by `PDF_SMOKE_SCALE` when set.
///
/// The counts in this file keep the whole binary to about a minute in an
/// unoptimized build, because it runs on every pull request and on three
/// operating systems. A layered run builds, compresses and parses a document
/// per iteration, so it is the costly kind. For a deeper sweep, run with
/// `PDF_SMOKE_SCALE=8` or more.
fn iterations(base: usize) -> usize {
    let scale = std::env::var("PDF_SMOKE_SCALE")
        .ok()
        .and_then(|value| value.parse::<usize>().ok())
        .unwrap_or(1);
    base * scale
}

// ── Seed construction ───────────────────────────────────────────────────────

fn zlib(bytes: &[u8]) -> Vec<u8> {
    let mut encoder = ZlibEncoder::new(Vec::new(), Compression::default());
    encoder.write_all(bytes).expect("compress seed");
    encoder.finish().expect("finish seed compression")
}

/// LZW-encodes `bytes` as literal codes only.
///
/// A clear code every 200 symbols keeps the decoder's table below 512 entries,
/// so every code stays nine bits wide and no table has to be tracked here.
fn lzw_encode(bytes: &[u8]) -> Vec<u8> {
    let mut out = Vec::new();
    let mut pending = 0u32;
    let mut pending_bits = 0u32;
    let mut push = |code: u32, out: &mut Vec<u8>| {
        pending = (pending << 9) | code;
        pending_bits += 9;
        while pending_bits >= 8 {
            pending_bits -= 8;
            out.push((pending >> pending_bits) as u8);
        }
    };
    for chunk in bytes.chunks(200) {
        push(256, &mut out);
        for byte in chunk {
            push(u32::from(*byte), &mut out);
        }
    }
    push(257, &mut out);
    // Flush the last partial byte.
    push(0, &mut out);
    out
}

/// Applies the PNG "Up" predictor to rows of `columns` bytes.
///
/// The data is padded with spaces to a whole number of rows, which a content
/// stream ignores.
fn png_up_rows(bytes: &[u8], columns: usize) -> Vec<u8> {
    let mut padded = bytes.to_vec();
    while !padded.len().is_multiple_of(columns) {
        padded.push(b' ');
    }
    let mut out = Vec::with_capacity(padded.len() + padded.len() / columns);
    let mut previous = vec![0u8; columns];
    for row in padded.chunks(columns) {
        out.push(2);
        for (byte, above) in row.iter().zip(&previous) {
            out.push(byte.wrapping_sub(*above));
        }
        previous.copy_from_slice(row);
    }
    out
}

fn ascii85(bytes: &[u8]) -> Vec<u8> {
    let mut out = Vec::new();
    for chunk in bytes.chunks(4) {
        let mut group = [0u8; 4];
        group[..chunk.len()].copy_from_slice(chunk);
        let mut value = u32::from_be_bytes(group);
        let mut digits = [0u8; 5];
        for digit in digits.iter_mut().rev() {
            *digit = (value % 85) as u8 + b'!';
            value /= 85;
        }
        out.extend_from_slice(&digits[..chunk.len() + 1]);
    }
    out.extend_from_slice(b"~>");
    out
}

fn ascii_hex(bytes: &[u8]) -> Vec<u8> {
    let mut out: Vec<u8> = bytes
        .iter()
        .flat_map(|byte| format!("{byte:02X}").into_bytes())
        .collect();
    out.push(b'>');
    out
}

/// Run-length encodes `bytes` as literal runs.
fn run_length(bytes: &[u8]) -> Vec<u8> {
    let mut out = Vec::new();
    for chunk in bytes.chunks(128) {
        out.push((chunk.len() - 1) as u8);
        out.extend_from_slice(chunk);
    }
    out.push(128);
    out
}

/// Numbers `objects` from 1 and wraps them in a header, xref table and trailer.
fn pdf(objects: &[Vec<u8>]) -> Vec<u8> {
    let mut out = b"%PDF-1.7\n%\xE2\xE3\xCF\xD3\n".to_vec();
    let mut offsets = Vec::with_capacity(objects.len());
    for (index, body) in objects.iter().enumerate() {
        offsets.push(out.len());
        out.extend_from_slice(format!("{} 0 obj\n", index + 1).as_bytes());
        out.extend_from_slice(body);
        out.extend_from_slice(b"\nendobj\n");
    }
    let xref = out.len();
    out.extend_from_slice(format!("xref\n0 {}\n", objects.len() + 1).as_bytes());
    out.extend_from_slice(b"0000000000 65535 f \n");
    for offset in offsets {
        out.extend_from_slice(format!("{offset:010} 00000 n \n").as_bytes());
    }
    out.extend_from_slice(
        format!(
            "trailer\n<< /Size {} /Root 1 0 R /Info << /Title (T \\(nested\\) \\101) \
             /Author <FEFF00E9> >> >>\nstartxref\n{xref}\n%%EOF\n",
            objects.len() + 1
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

/// The payloads a layered mutation run replaces, in plaintext.
#[derive(Clone)]
struct Payloads {
    content: Vec<u8>,
    table_content: Vec<u8>,
    cmap: Vec<u8>,
    object_stream: Vec<u8>,
    char_proc: Vec<u8>,
    form: Vec<u8>,
    inner_form: Vec<u8>,
    truetype: Vec<u8>,
    opentype: Vec<u8>,
    predictor_rows: Vec<u8>,
}

impl Payloads {
    fn seed() -> Self {
        let mut content = Vec::new();
        content.extend_from_slice(
            b"q 1 0 0 1 0 0 cm 0.2 0.4 0.6 rg 0.1 G 0 0 0 1 k 0.3 0.3 0.3 sc /G1 gs\n\
              BT /F1 12 Tf 14 TL 1 Tc 2 Tw 90 Tz 1 Ts 0 Tr 20 180 Td \
              (Hello \\(World\\) \\101\\n) Tj [(AB) -120 (C) 3 <4445>] TJ T* (x) ' 1 2 (y) \" \
              /F2 10 Tf 1 0 0 1 20 150 Tm <00410042004600480043> Tj \
              /F3 10 Tf (AB) Tj /F4 10 Tf (ab) Tj /F5 10 Tf 0 -14 TD (AB) Tj ET\n\
              0 0 m 100 0 l 100 50 l 0 50 l h S 10 10 80 30 re f \
              10 10 m 20 20 30 30 40 10 c 50 50 60 60 v 70 70 80 80 y B* W n \
              [3 2] 0 d 2 w 1 J 1 j 4 M\n\
              BI /W 2 /H 2 /BPC 8 /CS /G ID \x01\x02\x03\x04 EI\n\
              BI /W 4 /H 1 /BPC 8 /CS /RGB /F /AHx ID 00ff00ff00ff00ff00ff00ff> EI\n\
              /Fm1 Do /Im1 Do /Im2 Do /Im3 Do Q\n\
              99999999999999999999999999999999999999999999 0 m 1e5 -.5 l S\n",
        );
        let table_content = b"0.5 w 20 20 m 180 20 l S 20 60 m 180 60 l S 20 100 m 180 100 l S \
              20 20 m 20 100 l S 100 20 m 100 100 l S 180 20 m 180 100 l S\n\
              20 110 160 40 re S 20 110 80 40 re S\n\
              BT /F1 9 Tf 30 30 Td (r1c1) Tj 80 0 Td (r1c2) Tj -80 40 Td (r2c1) Tj \
              80 0 Td (r2c2) Tj 0 60 Td (above) Tj ET\n"
            .to_vec();
        let cmap = b"/CIDInit /ProcSet findresource begin\n12 dict begin\nbegincmap\n\
              /CMapName /Adobe-Identity-UCS def\n/CMapType 2 def\n\
              2 begincodespacerange\n<00> <FF>\n<0000> <FFFF>\nendcodespacerange\n\
              4 beginbfchar\n<41> <0041>\n<0041> <0041>\n<0042> <00660069>\n\
              <0048> <D83DDE00>\nendbfchar\n\
              2 beginbfrange\n<0043> <0045> <0043>\n<0046> <0047> [<0046> <00470047>]\n\
              endbfrange\nendcmap\nCMapName currentdict /CMap defineresource pop\nend\nend\n"
            .to_vec();
        let first_object = b"<< /Type /Font /Subtype /Type1 /BaseFont /Courier >> ";
        let second_object = b"[ 278 278 355 (s) <41> /N true null 1.5 -2 3 0 R ]";
        let header = format!("40 0 41 {} ", first_object.len());
        let mut object_stream = header.clone().into_bytes();
        object_stream.extend_from_slice(first_object);
        object_stream.extend_from_slice(second_object);
        Self {
            content,
            table_content,
            cmap,
            object_stream,
            char_proc: b"500 0 0 0 400 700 d1 0 0 m 400 0 l 400 700 l h f \
                         BT /F4 1 Tf (a) Tj ET 0 0 400 400 re f"
                .to_vec(),
            form: b"q 0.5 0 0 0.5 10 10 cm /Fm2 Do BT /F1 8 Tf (form) Tj ET \
                    0 0 20 20 re S Q"
                .to_vec(),
            inner_form: b"0 0 10 10 re S BT /F1 5 Tf (inner) Tj ET".to_vec(),
            truetype: TRUETYPE_FIXTURE.to_vec(),
            opentype: OPENTYPE_FIXTURE.to_vec(),
            predictor_rows: png_up_rows(b"BT /F1 9 Tf 20 20 Td (predicted text) Tj ET", 8),
        }
    }

    /// The seed with its two Form XObjects drawing each other.
    ///
    /// Kept apart from [`Self::seed`] because every interpreter refuses a
    /// form cycle with an error, which would end each page before the rest of
    /// its content was read.
    fn with_form_cycle() -> Self {
        let mut payloads = Self::seed();
        payloads.form = b"q /Fm2 Do /Fm1 Do BT /F1 8 Tf (form) Tj ET Q".to_vec();
        payloads.inner_form = b"/Fm1 Do 0 0 10 10 re S".to_vec();
        payloads
    }
}

/// A document that reaches every reader in the crate.
///
/// Objects, in order: catalog; a page tree whose `/Kids` lists itself (a
/// cycle); two pages with inherited resources; Flate, LZW, ASCII85, ASCIIHex
/// and RunLength streams, with PNG and TIFF predictors; a Type1 font with
/// `/Differences` and an indirect `/Widths`; a Type0/CID font with a ToUnicode
/// CMap (bfchar, bfrange and bfrange-array forms); embedded TrueType and
/// OpenType/CFF programs; a Type 3 font whose glyph draws text in itself;
/// nested Form XObjects; Flate-predicted, CCITT and DCT
/// images; an object stream; and a cross-reference stream with `/W`.
fn document(payloads: &Payloads) -> Vec<u8> {
    let object_stream_first = payloads
        .object_stream
        .iter()
        .position(|byte| *byte == b'<')
        .unwrap_or(0);
    // 46 objects, four bytes each (type, two offset bytes, generation).
    let xref_rows: Vec<u8> = (0..46u16)
        .flat_map(|index| [1u8, (index >> 8) as u8, index as u8, 0u8])
        .collect();
    let objects: Vec<Vec<u8>> = vec![
        // 1
        b"<< /Type /Catalog /Pages 2 0 R /Outlines 30 0 R /Metadata 31 0 R >>".to_vec(),
        // 2: the page tree root lists itself as a kid.
        b"<< /Type /Pages /Kids [3 0 R 20 0 R 2 0 R 32 0 R] /Count 3 \
          /MediaBox [0 0 200 200] /Resources 4 0 R /Rotate 0 >>"
            .to_vec(),
        // 3
        b"<< /Type /Page /Parent 2 0 R /Contents [5 0 R 6 0 R 29 0 R] /Rotate 90 \
          /CropBox [0 0 200 200] >>"
            .to_vec(),
        // 4
        b"<< /Font << /F1 7 0 R /F2 10 0 R /F3 14 0 R /F4 17 0 R /F5 33 0 R >> \
          /XObject << /Fm1 21 0 R /Im1 23 0 R /Im2 24 0 R /Im3 28 0 R >> \
          /ExtGState << /G1 << /LW 2 /CA 0.5 >> >> /ProcSet [/PDF /Text] >>"
            .to_vec(),
        // 5: Flate content.
        stream("/Filter /FlateDecode", &zlib(&payloads.content)),
        // 6: Flate content behind a PNG predictor.
        stream(
            "/Filter /FlateDecode /DecodeParms << /Predictor 12 /Columns 8 >>",
            &zlib(&payloads.predictor_rows),
        ),
        // 7
        b"<< /Type /Font /Subtype /Type1 /BaseFont /Helvetica /Encoding << /Type /Encoding \
          /BaseEncoding /WinAnsiEncoding /Differences 34 0 R >> /ToUnicode 8 0 R \
          /FirstChar 32 /LastChar 34 /Widths 9 0 R >>"
            .to_vec(),
        // 8
        stream("/Filter /FlateDecode", &zlib(&payloads.cmap)),
        // 9
        b"[ 278 278 355 ]".to_vec(),
        // 10
        b"<< /Type /Font /Subtype /Type0 /BaseFont /DejaVuSans /Encoding /Identity-H \
          /DescendantFonts [11 0 R] /ToUnicode 8 0 R >>"
            .to_vec(),
        // 11
        b"<< /Type /Font /Subtype /CIDFontType2 /BaseFont /DejaVuSans \
          /CIDSystemInfo << /Registry (Adobe) /Ordering (Identity) /Supplement 0 >> \
          /FontDescriptor 12 0 R /DW 1000 /W [0 [500 600] 65 70 700] /CIDToGIDMap /Identity >>"
            .to_vec(),
        // 12
        b"<< /Type /FontDescriptor /FontName /DejaVuSans /Flags 32 \
          /FontBBox [-1021 -463 1793 1232] /ItalicAngle 0 /Ascent 1556 /Descent -492 \
          /CapHeight 1493 /StemV 80 /FontFile2 13 0 R >>"
            .to_vec(),
        // 13
        stream(
            &format!("/Length1 {}", payloads.truetype.len()),
            &payloads.truetype,
        ),
        // 14
        b"<< /Type /Font /Subtype /Type1 /BaseFont /Cabin-Regular /FirstChar 65 /LastChar 66 \
          /Widths [500 500] /FontDescriptor 15 0 R /Encoding /WinAnsiEncoding >>"
            .to_vec(),
        // 15
        b"<< /Type /FontDescriptor /FontName /Cabin-Regular /Flags 32 \
          /FontBBox [0 0 1000 1000] /ItalicAngle 0 /Ascent 900 /Descent -200 \
          /CapHeight 700 /StemV 80 /FontFile3 16 0 R >>"
            .to_vec(),
        // 16
        stream("/Subtype /OpenType", &payloads.opentype),
        // 17: a Type 3 font whose glyph `b` shows text in the same font.
        b"<< /Type /Font /Subtype /Type3 /FontBBox [0 0 500 700] \
          /FontMatrix [0.001 0 0 0.001 0 0] /CharProcs << /a 18 0 R /b 19 0 R >> \
          /Encoding << /Type /Encoding /Differences [97 /a /b] >> /FirstChar 97 /LastChar 98 \
          /Widths [500 500] /Resources << /Font << /F4 17 0 R /F1 7 0 R >> >> >>"
            .to_vec(),
        // 18
        stream("", b"500 0 d0 0 0 m 400 0 l 400 700 l 0 700 l h f"),
        // 19
        stream("", &payloads.char_proc),
        // 20
        b"<< /Type /Page /Parent 2 0 R /Contents 22 0 R >>".to_vec(),
        // 21: a form that draws a second form.
        stream(
            "/Type /XObject /Subtype /Form /BBox [0 0 100 100] /Matrix [1 0 0 1 5 5] \
             /Resources << /XObject << /Fm1 21 0 R /Fm2 25 0 R >> /Font << /F1 7 0 R >> >>",
            &payloads.form,
        ),
        // 22: table rules and cell text, ASCII85 over Flate.
        stream(
            "/Filter [/ASCII85Decode /FlateDecode]",
            &ascii85(&zlib(&payloads.table_content)),
        ),
        // 23: an image behind a PNG "optimum" predictor.
        stream(
            "/Type /XObject /Subtype /Image /Width 2 /Height 2 /BitsPerComponent 8 \
             /ColorSpace /DeviceRGB /Filter /FlateDecode \
             /DecodeParms << /Predictor 15 /Colors 3 /Columns 2 /BitsPerComponent 8 >>",
            &zlib(&[0, 1, 2, 3, 4, 5, 6, 1, 7, 8, 9, 10, 11, 12]),
        ),
        // 24: a CCITT Group 4 image, four all-white rows then end-of-block.
        stream(
            "/Type /XObject /Subtype /Image /Width 16 /Height 4 /BitsPerComponent 1 \
             /ImageMask true /Filter /CCITTFaxDecode \
             /DecodeParms << /K -1 /Columns 16 /Rows 4 /BlackIs1 false >>",
            &[0xF0, 0x01, 0x00, 0x10],
        ),
        // 25
        stream(
            "/Type /XObject /Subtype /Form /BBox [0 0 50 50] \
             /Resources << /XObject << /Fm1 21 0 R >> >>",
            &payloads.inner_form,
        ),
        // 26: an object stream holding objects 40 and 41.
        stream(
            &format!("/Type /ObjStm /N 2 /First {object_stream_first} /Filter /FlateDecode"),
            &zlib(&payloads.object_stream),
        ),
        // 27: a cross-reference stream with `/W` and a predictor.
        stream(
            "/Type /XRef /Size 46 /W [1 2 1] /Index [0 46] /Root 1 0 R /Filter /FlateDecode \
             /DecodeParms << /Predictor 12 /Columns 4 >>",
            &zlib(&png_up_rows(&xref_rows, 4)),
        ),
        // 28: a DCT image, which the parser passes through undecoded.
        stream(
            "/Type /XObject /Subtype /Image /Width 1 /Height 1 /BitsPerComponent 8 \
             /ColorSpace /DeviceGray /Filter /DCTDecode",
            b"\xFF\xD8\xFF\xE0\x00\x10JFIF\x00\x01\x01\x00\x00\x01\x00\x01\x00\x00\xFF\xD9",
        ),
        // 29: LZW content behind a TIFF predictor, one row wide.
        stream(
            &format!(
                "/Filter /LZWDecode /DecodeParms << /Predictor 2 /Columns {} /Colors 1 \
                 /BitsPerComponent 8 /EarlyChange 1 >>",
                LZW_CONTENT.len()
            ),
            &lzw_encode(&tiff_predict(LZW_CONTENT)),
        ),
        // 30
        b"<< /Type /Outlines /Count 0 >>".to_vec(),
        // 31: ASCIIHex over RunLength.
        stream(
            "/Type /Metadata /Subtype /XML /Filter [/ASCIIHexDecode /RunLengthDecode]",
            &ascii_hex(&run_length(b"<x:xmpmeta>BT (meta) Tj ET</x:xmpmeta>")),
        ),
        // 32: an intermediate node with a deeper kid.
        b"<< /Type /Pages /Parent 2 0 R /Kids [35 0 R 2 0 R] /Count 1 >>".to_vec(),
        // 33: a simple TrueType font sharing the embedded program.
        b"<< /Type /Font /Subtype /TrueType /BaseFont /DejaVuSans /FirstChar 65 /LastChar 66 \
          /Widths [1401 1367] /FontDescriptor 12 0 R /Encoding /WinAnsiEncoding >>"
            .to_vec(),
        // 34
        b"[ 65 /Alpha /fi 200 /emdash /uni00E9 /g123 ]".to_vec(),
        // 35
        b"<< /Type /Page /Parent 32 0 R /MediaBox [0 0 100 100] /Contents 36 0 R \
          /Resources << /Font << /F1 7 0 R >> >> >>"
            .to_vec(),
        // 36: unfiltered content, so whole-document mutations reach operators.
        stream("", &payloads.content),
    ];
    pdf(&objects)
}

const LZW_CONTENT: &[u8] = b"BT /F1 9 Tf 20 40 Td (lzw text) Tj ET";

/// Applies the TIFF horizontal predictor for one-byte samples.
fn tiff_predict(bytes: &[u8]) -> Vec<u8> {
    let mut out = Vec::with_capacity(bytes.len());
    let mut previous = 0u8;
    for byte in bytes {
        out.push(byte.wrapping_sub(previous));
        previous = *byte;
    }
    out
}

/// The thin single-page document the first fuzzing pass used, kept as a seed
/// because mutations of a small file concentrate on the object syntax.
fn small_document() -> Vec<u8> {
    let content =
        b"q 1 0 0 1 0 0 cm BT /F1 12 Tf 72 720 Td (Hello) Tj [(AB) -120 (C) 3 <4445>] TJ \
        T* (x) ' ET\n0 0 m 100 0 l 100 50 l 0 50 l h S 10 10 80 30 re f\n\
        BI /W 2 /H 2 /BPC 8 /CS /G ID \x01\x02\x03\x04 EI Q";
    pdf(&[
        b"<< /Type /Catalog /Pages 2 0 R >>".to_vec(),
        b"<< /Type /Pages /Kids [3 0 R] /Count 1 /MediaBox [0 0 612 792] >>".to_vec(),
        b"<< /Type /Page /Parent 2 0 R /Resources << /Font << /F1 5 0 R >> >> \
          /Contents 4 0 R /Rotate 90 >>"
            .to_vec(),
        stream("", content),
        b"<< /Type /Font /Subtype /Type1 /BaseFont /Helvetica /Encoding /WinAnsiEncoding \
          /FirstChar 32 /LastChar 34 /Widths [278 278 355] >>"
            .to_vec(),
    ])
}

// ── What a consumer does with a document ────────────────────────────────────

/// Options with decode budgets small enough to keep a mutation run fast.
///
/// A mutated `/MediaBox` or `/Length` can legitimately ask for a surface or a
/// stream as large as the budget allows; the default budgets make each such
/// input cost a quarter of a gigabyte of zeroed memory. The limits are still
/// far above anything these kilobyte-sized seeds decode to.
fn tight_options() -> ParseOptions {
    ParseOptions {
        max_decoded_stream_len: 8 * 1024 * 1024,
        max_total_decoded_bytes: 16 * 1024 * 1024,
        ..ParseOptions::default()
    }
}

fn dump(bytes: &[u8]) {
    if let Some(path) = std::env::var_os("PDF_SMOKE_DUMP") {
        std::fs::write(path, bytes).expect("write smoke input");
    }
}

fn parse(bytes: &[u8], options: ParseOptions) -> Option<PdfDocument> {
    dump(bytes);
    PdfDocument::from_slice(bytes, options).ok()
}

/// Parses `bytes` and calls the text and glyph extraction API.
fn exercise_text(bytes: &[u8], options: ParseOptions) {
    let Some(document) = parse(bytes, options) else {
        return;
    };
    let pages = document.pages();
    let _ = document.semantic_text_runs();
    let _ = document.layout_blocks();
    let _ = document.vector_glyph_clusters_checked(0.05);
    let _ = document.vector_glyph_descriptors_checked(8);
    let _ = document.vector_glyph_descriptor_lines_checked(8, 2.0, true);
    let _ = document.vector_glyph_cluster_lines_checked(0.05, 2.0, true);
    for index in 0..document.objects.len().min(64) {
        let _ = document.object_bytes(index);
    }
    for index in 0..document.streams.len().min(64) {
        let _ = document.stream_raw_bytes(index);
        let _ = document.stream_decoded_bytes(index);
    }
    #[cfg(feature = "arrow")]
    {
        let _ = document.semantic_text_runs_record_batch();
    }
    for (index, page) in pages.iter().enumerate().take(4) {
        let _ = page.page_box();
        let _ = document.semantic_text_runs_for_page(page);
        let _ = document.page_vector_text_runs(index);
        let _ = document.page_vector_text_as_string(index);
        let _ = document.page_vector_text_as_string_checked(index);
        let _ = document.page_text_line_boxes(index, 2.0);
        let _ = document.page_text_line_boxes_with(index, 2.0, true);
        let _ = document.page_embedded_image(index);
    }
}

/// Parses `bytes` and calls the table, path and layout extraction API.
fn exercise_tables(bytes: &[u8], options: ParseOptions) {
    let Some(document) = parse(bytes, options) else {
        return;
    };
    let pages = document.pages();
    #[cfg(feature = "arrow")]
    {
        let _ = document.semantic_text_runs_record_batch_parallel();
        let _ = document.table_layout_record_batch(1.0);
        let _ = document.table_record_batch(1.0);
    }
    for (index, page) in pages.iter().enumerate().take(4) {
        let _ = document.page_tables(index, 1.0);
        let _ = document.page_table_grid(index, 1.0);
        let _ = document.page_table_layout(index, 1.0);
        let _ = document.page_table_text_assignments(index, 1.0);
        let _ = document.page_line_segments(index);
        let _ = document.page_image_placements(index);
        let _ = document.reading_order_blocks(page, TessellationOptions::default());
        let _ = document.tessellate_page(index, TessellationOptions::default());
        #[cfg(feature = "arrow")]
        {
            let _ = document.page_table_layout_record_batch(index, 1.0);
            let _ = document.page_table_record_batch(index, 1.0);
        }
    }
}

/// Parses `bytes` and calls the image and rasterization API.
///
/// The full-interpreter entry points (`page_rasterized_png_interpreted` and
/// the `interpreted` flag of `raster_glyph_descriptor_lines_with`) are left
/// out on purpose. They hand the document to a third-party interpreter that
/// recurses without a depth limit, and a parser cannot bound what another
/// parser makes of a malformed file.
#[cfg(feature = "rasterize")]
fn exercise_raster(bytes: &[u8], options: ParseOptions) {
    use pdf_fast_core::BitmapComponentOptions;

    let Some(document) = parse(bytes, options) else {
        return;
    };
    let pages = document.pages();
    for index in 0..pages.len().min(3) {
        let _ = document.page_rasterized_png(index, 18.0);
        let _ = document.page_bitmap_components(index, BitmapComponentOptions::default());
        let _ = document.raster_glyph_descriptor_lines_with(index, 18.0, 8, 128, 2.0, true, false);
        #[cfg(feature = "arrow")]
        {
            let _ = document.page_multimodal_record_batch(index);
        }
    }
}

#[cfg(not(feature = "rasterize"))]
fn exercise_raster(_bytes: &[u8], _options: ParseOptions) {}

/// Walks every glyph of a TrueType program.
fn exercise_truetype(bytes: &[u8]) {
    let Some(font) = TrueTypeFont::parse(bytes) else {
        return;
    };
    let _ = font.units_per_em();
    for code in 0..512u32 {
        let _ = font.glyph_index(code);
        let _ = font.advance_units(code);
    }
    for glyph in 0..=u16::MAX {
        let _ = font.outline(glyph);
    }
}

/// Walks every glyph of a CFF program, bare or wrapped in an `OTTO` sfnt.
fn exercise_cff(bytes: &[u8]) {
    let table = cff_table(bytes).unwrap_or(bytes);
    let Some(font) = CffFont::parse(table, None) else {
        return;
    };
    let _ = font.units_per_em();
    let _ = font.glyph_id("A");
    for glyph in 0..=u16::MAX {
        if font.glyph_name(glyph).is_none() && glyph > 1024 {
            break;
        }
        let _ = font.outline(glyph);
        let _ = font.advance_units(glyph);
    }
}

/// The `CFF ` table of an `OTTO` sfnt, when `bytes` is one.
fn cff_table(bytes: &[u8]) -> Option<&[u8]> {
    if bytes.get(..4)? != b"OTTO" {
        return None;
    }
    let tables = u16::from_be_bytes([*bytes.get(4)?, *bytes.get(5)?]) as usize;
    for index in 0..tables {
        let record = bytes.get(12 + index * 16..28 + index * 16)?;
        if &record[..4] == b"CFF " {
            let offset = u32::from_be_bytes([record[8], record[9], record[10], record[11]]);
            let length = u32::from_be_bytes([record[12], record[13], record[14], record[15]]);
            let end = (offset as usize).checked_add(length as usize)?;
            return bytes.get(offset as usize..end);
        }
    }
    None
}

// ── Mutation runs ───────────────────────────────────────────────────────────

/// Whole-document seeds: every reader, the form cycle, and a small file.
fn document_seeds() -> Vec<Vec<u8>> {
    vec![
        document(&Payloads::seed()),
        document(&Payloads::with_form_cycle()),
        small_document(),
    ]
}

#[test]
fn mutated_documents_never_panic_in_parsing_or_text_extraction() {
    let _serial = serial();
    smoke::assert_no_panics(
        "pdf-fast-core document text",
        &document_seeds(),
        iterations(600),
        |bytes| exercise_text(bytes, ParseOptions::default()),
    );
}

#[test]
fn mutated_documents_never_panic_in_table_or_layout_extraction() {
    let _serial = serial();
    smoke::assert_no_panics(
        "pdf-fast-core document tables",
        &document_seeds(),
        iterations(300),
        |bytes| exercise_tables(bytes, ParseOptions::default()),
    );
}

#[test]
fn mutated_documents_never_panic_in_rasterization() {
    let _serial = serial();
    // No form cycle here: see `exercise_raster`.
    let seeds = [document(&Payloads::seed()), small_document()];
    smoke::assert_no_panics(
        "pdf-fast-core document raster",
        &seeds,
        iterations(300),
        |bytes| exercise_raster(bytes, tight_options()),
    );
}

/// One layered run: mutate a plaintext payload, embed it, parse the document.
fn layered_run(name: &str, seed: Vec<u8>, count: usize, place: fn(&mut Payloads, Vec<u8>)) {
    let base = Payloads::seed();
    smoke::assert_no_panics(name, &[seed], iterations(count), |bytes| {
        let mut payloads = base.clone();
        place(&mut payloads, bytes.to_vec());
        let document = document(&payloads);
        exercise_text(&document, tight_options());
        exercise_tables(&document, tight_options());
    });
}

#[test]
fn mutated_content_streams_never_panic_behind_their_filters() {
    let _serial = serial();
    let seed = Payloads::seed();
    layered_run(
        "pdf-fast-core content",
        seed.content.clone(),
        45,
        |payloads, bytes| payloads.content = bytes,
    );
    layered_run(
        "pdf-fast-core table content",
        seed.table_content.clone(),
        30,
        |payloads, bytes| payloads.table_content = bytes,
    );
    layered_run(
        "pdf-fast-core form content",
        seed.form.clone(),
        20,
        |payloads, bytes| payloads.form = bytes,
    );
    layered_run(
        "pdf-fast-core inner form content",
        seed.inner_form.clone(),
        15,
        |payloads, bytes| payloads.inner_form = bytes,
    );
    layered_run(
        "pdf-fast-core char proc",
        seed.char_proc.clone(),
        20,
        |payloads, bytes| payloads.char_proc = bytes,
    );
    layered_run(
        "pdf-fast-core predictor rows",
        seed.predictor_rows.clone(),
        20,
        |payloads, bytes| payloads.predictor_rows = bytes,
    );
}

#[test]
fn mutated_content_streams_never_panic_in_rasterization() {
    let _serial = serial();
    let base = Payloads::seed();
    smoke::assert_no_panics(
        "pdf-fast-core content raster",
        &[base.content.clone(), base.table_content.clone()],
        iterations(150),
        |bytes| {
            let mut payloads = base.clone();
            payloads.content = bytes.to_vec();
            exercise_raster(&document(&payloads), tight_options());
        },
    );
}

#[test]
fn mutated_cmaps_and_object_streams_never_panic_behind_their_filters() {
    let _serial = serial();
    let seed = Payloads::seed();
    layered_run(
        "pdf-fast-core cmap",
        seed.cmap.clone(),
        45,
        |payloads, bytes| payloads.cmap = bytes,
    );
    layered_run(
        "pdf-fast-core object stream",
        seed.object_stream.clone(),
        45,
        |payloads, bytes| payloads.object_stream = bytes,
    );
}

#[test]
fn mutated_font_programs_never_panic() {
    let _serial = serial();
    smoke::assert_no_panics(
        "pdf-fast-core truetype",
        &[TRUETYPE_FIXTURE.to_vec()],
        iterations(100),
        exercise_truetype,
    );
    smoke::assert_no_panics(
        "pdf-fast-core cff",
        &[OPENTYPE_FIXTURE.to_vec()],
        iterations(200),
        exercise_cff,
    );
    // The same programs embedded, so the font dictionary plumbing and the
    // glyph placement that consumes the outlines run too.
    layered_run(
        "pdf-fast-core embedded truetype",
        TRUETYPE_FIXTURE.to_vec(),
        300,
        |payloads, bytes| payloads.truetype = bytes,
    );
    layered_run(
        "pdf-fast-core embedded opentype",
        OPENTYPE_FIXTURE.to_vec(),
        300,
        |payloads, bytes| payloads.opentype = bytes,
    );
}

#[test]
fn mutated_filter_payloads_never_panic() {
    let _serial = serial();
    let text = b"BT /F1 9 Tf 20 40 Td (lzw text lzw text lzw text) Tj ET";
    smoke::assert_no_panics(
        "pdf-fast-core lzw",
        &[lzw_encode(text)],
        iterations(20_000),
        |bytes| {
            let _ = lzw::decode_lzw_limited(bytes, 1, 1 << 20);
            let _ = lzw::decode_lzw_limited(bytes, 0, 1 << 20);
        },
    );
    smoke::assert_no_panics(
        "pdf-fast-core predictor",
        &[png_up_rows(text, 8), tiff_predict(text)],
        iterations(4_000),
        |bytes| {
            // The first three bytes pick the parameters, so the mutations
            // reach every predictor, sample depth and row shape.
            let predictor =
                [2u8, 10, 11, 12, 13, 14, 15, 3][bytes.first().copied().unwrap_or(0) as usize % 8];
            let columns = bytes.get(1).copied().unwrap_or(8) as usize;
            let colors = bytes.get(2).copied().unwrap_or(1) as usize % 6;
            for bits in [1usize, 2, 4, 8, 16, 3] {
                let _ = lzw::apply_predictor(bytes, predictor, columns, colors, bits);
                let _ = lzw::apply_predictor(bytes, predictor, 8, 1, bits);
            }
            let _ = lzw::apply_predictor(bytes, predictor, usize::MAX, usize::MAX, 16);
        },
    );
}

// ── Explicit regressions ────────────────────────────────────────────────────

/// A stray delimiter inside an array used to parse as an empty bareword that
/// consumed nothing, so the array loop pushed values until memory ran out.
#[test]
fn a_stray_delimiter_in_an_array_does_not_loop() {
    let _serial = serial();
    for delimiter in [&b")"[..], b">", b"{", b"}"] {
        for (open, close) in [
            (&b"[ "[..], &b" ]"[..]),
            (b"<< /A [ ", b" ] >>"),
            (b"<< /A ", b" >>"),
            (b"[ 1 ", b" ]"),
        ] {
            let mut document = b"%PDF-1.7\n1 0 obj\n".to_vec();
            document.extend_from_slice(open);
            document.extend_from_slice(delimiter);
            document.extend_from_slice(close);
            document.extend_from_slice(b"\nendobj\n");
            exercise_text(&document, ParseOptions::default());
        }
    }
}

/// Arrays and dictionaries nested far past any real document must be refused
/// by a depth limit, not followed until the stack overflows.
#[test]
fn deeply_nested_values_are_bounded_on_a_small_stack() {
    let _serial = serial();
    for (open, close) in [
        (&b"["[..], &b"]"[..]),
        (b"<< /A ", b" >>"),
        (b"[ << /A ", b" >> ]"),
    ] {
        // Unterminated, as the fuzzer found it, and properly closed.
        for closed in [false, true] {
            let mut document = b"%PDF-1.7\n1 0 obj\n".to_vec();
            if closed {
                document.extend(smoke::nested(open, b"1", close, 200_000));
                document.extend_from_slice(b"\nendobj\n");
            } else {
                document.extend(smoke::nested(open, b"", b"", 200_000));
            }
            smoke::run_on_small_stack(move || {
                exercise_text(&document, ParseOptions::default());
            });
        }
    }
}

/// The same nesting inside a content stream, which has its own tokenizer.
#[test]
fn deeply_nested_content_arrays_are_bounded_on_a_small_stack() {
    let _serial = serial();
    let mut payloads = Payloads::seed();
    payloads.content = smoke::nested(b"[", b"(x)", b"]", 200_000);
    payloads.content.extend_from_slice(b" TJ");
    let document = document(&payloads);
    smoke::run_on_small_stack(move || {
        exercise_text(&document, tight_options());
        exercise_tables(&document, tight_options());
    });
}

/// A content stream of nothing but `q`. The table extractor kept one saved
/// matrix per `q` with no limit, 24 bytes for every two bytes of content, so
/// this 72 MB stream asked for more than the allocator's 1 GiB cap.
#[test]
fn a_run_of_saved_states_does_not_grow_the_table_extractor() {
    let _serial = serial();
    let mut payloads = Payloads::seed();
    payloads.table_content = b"q ".repeat(36_000_000);
    let document = document(&payloads);
    // On the harness's small stack, so the test behaves the same on every
    // platform whatever stack its test threads get.
    smoke::run_on_small_stack(move || {
        exercise_tables(&document, ParseOptions::default());
    });
}

/// Runs `parse` on a thread with half of the harness's small stack.
fn run_on_half_the_small_stack<F>(parse: F)
where
    F: FnOnce() + Send + 'static,
{
    std::thread::Builder::new()
        .stack_size(smoke::SMALL_STACK_BYTES / 2)
        .spawn(parse)
        .expect("spawn half-stack thread")
        .join()
        .expect("parser panicked on half the small stack");
}

/// The two nesting inputs again, on half the stack.
///
/// Passing on the small stack says the recursion fits; it does not say by
/// how much. With the limits these inputs hit, the parsers once needed over
/// three quarters of that stack in an unoptimized macOS build, which passed
/// here and overflowed now and then on Windows, where frames are larger.
/// Halving the stack makes a lost margin fail on every platform.
#[test]
fn deep_nesting_leaves_half_the_small_stack_unused() {
    let _serial = serial();
    for (open, close) in [
        (&b"["[..], &b"]"[..]),
        (b"<< /A ", b" >>"),
        (b"[ << /A ", b" >> ]"),
    ] {
        let mut document = b"%PDF-1.7\n1 0 obj\n".to_vec();
        document.extend(smoke::nested(open, b"1", close, 200_000));
        document.extend_from_slice(b"\nendobj\n");
        run_on_half_the_small_stack(move || {
            exercise_text(&document, ParseOptions::default());
        });
    }
    let mut payloads = Payloads::seed();
    payloads.content = smoke::nested(b"[", b"(x)", b"]", 200_000);
    payloads.content.extend_from_slice(b" TJ");
    let document = document(&payloads);
    run_on_half_the_small_stack(move || {
        exercise_text(&document, tight_options());
        exercise_tables(&document, tight_options());
    });
}

// ── Seed sanity ─────────────────────────────────────────────────────────────

/// The seed has to reach the readers it claims to; a seed whose streams fail
/// to decode would leave the mutation runs testing nothing behind them.
#[test]
fn the_seed_document_reaches_every_reader() {
    let _serial = serial();
    let bytes = document(&Payloads::seed());
    let document = PdfDocument::from_slice(&bytes, ParseOptions::default()).expect("seed parses");

    // The page tree lists itself as a kid; each page is still found once.
    let pages = document.pages();
    assert_eq!(pages.len(), 3, "three pages despite the /Kids cycle");

    // Every filter chain decodes, so the readers behind them see the payloads.
    for stream in &document.streams {
        assert!(
            !matches!(stream.decode_status, pdf_fast_core::DecodeStatus::Failed(_)),
            "stream {} failed to decode: {:?}",
            stream.object_number,
            stream.decode_status
        );
    }
    let text: Vec<String> = document
        .semantic_text_runs()
        .into_iter()
        .map(|run| run.text)
        .collect();
    for expected in [
        "predicted text", // Flate + PNG predictor
        "lzw text",       // LZW + TIFF predictor
        "r1c1",           // ASCII85 over Flate
        "form",           // Form XObject
        "Afi CDE",        // ToUnicode bfchar and bfrange
    ] {
        assert!(
            text.iter().any(|run| run == expected),
            "missing {expected:?} in {text:?}"
        );
    }
    // The bfrange-array form and a surrogate pair, through the CID font.
    assert!(text
        .iter()
        .any(|run| run.contains("GG") || run.contains('F')));
    assert_eq!(document.cmaps.len(), 1);
    assert_eq!(document.cmaps[0].entries.len(), 6);

    // The object stream was expanded into its two objects.
    assert!(document
        .object_by_id(40, 0)
        .is_some_and(|object| object.dictionary.is_some()));
    assert_eq!(
        document
            .object_by_id(41, 0)
            .and_then(|object| object.array.as_ref())
            .map(Vec::len),
        Some(11)
    );

    // Path, table, image and glyph extraction all have something to read.
    assert!(!document.page_tables(0, 1.0).expect("tables").is_empty());
    assert!(!document.page_line_segments(0).expect("segments").is_empty());
    assert_eq!(document.page_image_placements(0).expect("images").len(), 3);
    assert!(!document
        .page_vector_text_runs(0)
        .expect("vector runs")
        .is_empty());
    assert!(document.page_embedded_image(0).expect("image").is_some());
    #[cfg(feature = "rasterize")]
    assert!(document
        .page_rasterized_png(0, 18.0)
        .expect("raster")
        .is_some());

    // Both embedded font programs are readable on their own.
    assert!(TrueTypeFont::parse(TRUETYPE_FIXTURE).is_some());
    assert!(cff_table(OPENTYPE_FIXTURE)
        .and_then(|table| CffFont::parse(table, None))
        .is_some());
}
