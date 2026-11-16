//! What page extraction does with selections and sources at the edges: the
//! output always opens in pdf-fast-core with the pages asked for, and nothing
//! from outside those pages comes along.

mod common;

use common::{build, page, page_texts, revision, simple_document, stream, text_stream, Revision};
use pdf_fast_write::PdfWriter;

fn extract(pdf: &[u8], pages: &[usize]) -> Result<Vec<u8>, String> {
    PdfWriter::extract_pages_from_bytes(pdf, pages).map_err(|error| format!("{error:#}"))
}

/// Runs `work` on another thread and fails if it has not finished in time.
fn within_seconds<T: Send + 'static>(seconds: u64, work: impl FnOnce() -> T + Send + 'static) -> T {
    let (sender, receiver) = std::sync::mpsc::channel();
    std::thread::spawn(move || {
        let _ = sender.send(work());
    });
    receiver
        .recv_timeout(std::time::Duration::from_secs(seconds))
        .expect("extraction did not finish: unbounded loop")
}

/// WHY: the selection is a list, not a set: its order is the output order
/// and a page named twice appears twice.
#[test]
fn a_selection_is_written_in_the_order_given() {
    let pdf = simple_document(3);
    let texts = |pages: &[usize]| page_texts(&extract(&pdf, pages).expect("extracts"));
    assert_eq!(texts(&[1, 2, 3]), ["Page 1", "Page 2", "Page 3"]);
    assert_eq!(texts(&[3, 1]), ["Page 3", "Page 1"]);
    assert_eq!(texts(&[2]), ["Page 2"]);
    assert_eq!(
        texts(&[1, 1, 2, 1]),
        ["Page 1", "Page 1", "Page 2", "Page 1"]
    );
}

/// WHY: a page number that does not exist must stop the extraction; writing
/// the pages that do exist would hand back a different document than asked.
#[test]
fn a_selection_outside_the_document_is_an_error() {
    let pdf = simple_document(3);
    for pages in [&[0][..], &[4], &[1, 4], &[usize::MAX], &[]] {
        assert!(extract(&pdf, pages).is_err(), "{pages:?} was extracted");
    }
    // Every entry becomes an object, and a PDF holds at most 8,388,607.
    let huge = vec![1usize; 8_388_608];
    let error = extract(&pdf, &huge).expect_err("too many pages selected");
    assert!(error.contains("8388607"), "{error}");
}

/// WHY: `/Count` is the source's claim, not a fact. The pages are the leaves
/// actually reachable from the page tree.
#[test]
fn a_page_count_the_source_only_claims_is_not_trusted() {
    let pdf = build(&[revision(
        vec![
            (1, b"<< /Type /Catalog /Pages 2 0 R >>".to_vec()),
            (
                2,
                b"<< /Type /Pages /Count 4000000000 /Kids [3 0 R] >>".to_vec(),
            ),
            (3, page(2, 4, "")),
            (4, text_stream("Only")),
        ],
        1,
    )]);
    assert_eq!(
        page_texts(&extract(&pdf, &[1]).expect("extracts")),
        ["Only"]
    );
    assert!(extract(&pdf, &[2]).is_err());
}

/// WHY: an encrypted source has encrypted strings and streams. Copying them
/// into a file without the `/Encrypt` dictionary gives pages of noise.
#[test]
fn an_encrypted_source_is_refused() {
    let mut objects = vec![
        (1, b"<< /Type /Catalog /Pages 2 0 R >>".to_vec()),
        (2, b"<< /Type /Pages /Count 1 /Kids [3 0 R] >>".to_vec()),
        (3, page(2, 4, "")),
        (4, text_stream("Secret")),
        (
            5,
            b"<< /Filter /Standard /V 1 /R 2 /O (o) /U (u) /P -44 >>".to_vec(),
        ),
    ];
    let indirect = Revision {
        objects: objects.clone(),
        freed: Vec::new(),
        trailer: "/Root 1 0 R /Encrypt 5 0 R /ID [<00><00>]".to_string(),
    };
    objects.pop();
    let direct = Revision {
        objects,
        freed: Vec::new(),
        trailer: "/Encrypt << /Filter /Standard /V 1 /R 2 /O (o) /U (u) /P -44 >> /Root 1 0 R"
            .to_string(),
    };
    for source in [indirect, direct] {
        let error = extract(&build(&[source]), &[1]).expect_err("encrypted source");
        assert!(error.to_lowercase().contains("encrypt"), "{error}");
    }
}

/// WHY: an editor that saves a change appends a revision: new objects, a new
/// cross-reference section and a new trailer. The document is what the LAST
/// trailer describes; the first revision alone is an older document.
#[test]
fn a_source_saved_in_several_revisions_is_read_from_the_latest() {
    let first = revision(
        vec![
            (1, b"<< /Type /Catalog /Pages 2 0 R >>".to_vec()),
            (
                2,
                b"<< /Type /Pages /Count 2 /Kids [3 0 R 5 0 R] >>".to_vec(),
            ),
            (3, page(2, 4, "")),
            // Stream bytes that are not UTF-8, as compressed content is.
            (
                4,
                stream(
                    "",
                    b"BT /F1 12 Tf 100 700 Td (Old 1) Tj ET % \xff\xfe >> trailer",
                ),
            ),
            (5, page(2, 6, "")),
            (6, text_stream("Old 2")),
            (7, b"<< /Title (First revision) >>".to_vec()),
        ],
        1,
    );
    // The second save rewrites page 1's content, adds a third page under a
    // new catalog and page tree, and deletes the old page 2.
    let second = Revision {
        objects: vec![
            (
                4,
                stream("", b"BT /F1 12 Tf 100 700 Td (New 1) Tj ET % \xff\xfe"),
            ),
            (8, b"<< /Type /Catalog /Pages 9 0 R >>".to_vec()),
            (
                9,
                b"<< /Type /Pages /Count 2 /Kids [3 0 R 10 0 R] >>".to_vec(),
            ),
            (3, page(9, 4, "")),
            (10, page(9, 11, "")),
            (11, text_stream("New 3")),
        ],
        freed: vec![5, 6],
        trailer: "/Root 8 0 R".to_string(),
    };
    let third = Revision {
        objects: vec![(11, text_stream("Newest 3"))],
        freed: Vec::new(),
        trailer: "/Root 8 0 R".to_string(),
    };
    let pdf = build(&[first, second, third]);
    let output = extract(&pdf, &[1, 2]).expect("extracts from the latest revision");
    assert_eq!(page_texts(&output), ["New 1", "Newest 3"]);
    assert!(
        extract(&pdf, &[3]).is_err(),
        "the latest revision has two pages"
    );
    for stale in ["Old 1", "Old 2", "New 3", "First revision"] {
        assert!(
            !output
                .windows(stale.len())
                .any(|window| window == stale.as_bytes()),
            "{stale:?} from an older revision was copied"
        );
    }
}

/// WHY: the output is a new document made of the selected pages. Whatever
/// the source's catalog and trailer carry (title, XMP, attachments, scripts
/// that run on open, outlines, forms) describes the whole source and must
/// not come along; what a selected page itself refers to does.
#[test]
fn only_what_the_selected_pages_refer_to_is_copied() {
    let catalog = "<< /Type /Catalog /Pages 2 0 R /Metadata 20 0 R /Names 21 0 R \
                   /OpenAction 23 0 R /Outlines 24 0 R /AcroForm << /Fields [] /DA (catalog-da) >> \
                   /AA << /WC 23 0 R >> /Lang (xx-catalog) >>";
    let objects = vec![
        (1, catalog.as_bytes().to_vec()),
        (2, b"<< /Type /Pages /Count 2 /Kids [3 0 R 5 0 R] >>".to_vec()),
        (3, page(2, 4, "/Annots [7 0 R] /Metadata 8 0 R")),
        (4, text_stream("Kept page")),
        (5, page(2, 6, "")),
        (6, text_stream("Hidden page")),
        (7, b"<< /Type /Annot /Subtype /Link /Rect [0 0 9 9] /A << /S /URI /URI (https://kept.example/) >> >>".to_vec()),
        (8, stream("/Type /Metadata /Subtype /XML", b"<x>page-level-xmp</x>")),
        (20, stream("/Type /Metadata /Subtype /XML", b"<x>document-xmp</x>")),
        (21, b"<< /EmbeddedFiles << /Names [(attached.txt) 22 0 R] >> /JavaScript << /Names [(boot) 23 0 R] >> >>".to_vec()),
        (22, b"<< /Type /Filespec /F (attached.txt) >>".to_vec()),
        (23, b"<< /S /JavaScript /JS (app.alert\\('document-script'\\)) >>".to_vec()),
        (24, b"<< /Type /Outlines /Count 0 /Title (outline-title) >>".to_vec()),
        (25, b"<< /Title (Secret Title) /Author (Secret Author) >>".to_vec()),
    ];
    let source = Revision {
        objects,
        freed: Vec::new(),
        trailer: "/Root 1 0 R /Info 25 0 R /ID [<aa> <bb>]".to_string(),
    };
    let output = extract(&build(&[source]), &[1]).expect("extracts");
    assert_eq!(page_texts(&output), ["Kept page"]);
    let has = |needle: &str| output.windows(needle.len()).any(|w| w == needle.as_bytes());
    for dropped in [
        "Secret Title",
        "Secret Author",
        "document-xmp",
        "attached.txt",
        "document-script",
        "outline-title",
        "catalog-da",
        "xx-catalog",
        "Hidden page",
        "/Info",
        "/Names",
        "/OpenAction",
        "/Outlines",
        "/AcroForm",
        "/JavaScript",
        "/EmbeddedFiles",
        "/AA",
        "/ID",
    ] {
        assert!(!has(dropped), "{dropped:?} leaked into the output");
    }
    // Part of the page: its annotations with their actions, and its own
    // metadata stream.
    for kept in ["https://kept.example/", "page-level-xmp"] {
        assert!(has(kept), "{kept:?} belongs to the page and was lost");
    }
}

/// WHY: a page tree with a cycle, or with one node listed under two parents,
/// is not a tree. Walking it never ends, or yields a number of pages out of
/// all proportion to the file.
#[test]
fn a_page_tree_that_is_not_a_tree_is_an_error() {
    let cyclic = build(&[revision(
        vec![
            (1, b"<< /Type /Catalog /Pages 2 0 R >>".to_vec()),
            (
                2,
                b"<< /Type /Pages /Count 1 /Kids [3 0 R 2 0 R] >>".to_vec(),
            ),
            (3, page(2, 4, "")),
            (4, text_stream("x")),
        ],
        1,
    )]);
    // Each of 12 levels lists the next level 16 times: 16^12 leaves.
    let mut objects = vec![(1, b"<< /Type /Catalog /Pages 2 0 R >>".to_vec())];
    for level in 0..12u32 {
        let kids = format!("{} 0 R ", level + 3).repeat(16);
        objects.push((
            level + 2,
            format!("<< /Type /Pages /Kids [{kids}] >>").into_bytes(),
        ));
    }
    objects.push((14, page(13, 15, "")));
    objects.push((15, text_stream("x")));
    let fan_out = build(&[revision(objects, 1)]);
    // A node whose /Parent chain loops without reaching the root.
    let parent_loop = build(&[revision(
        vec![
            (1, b"<< /Type /Catalog /Pages 2 0 R >>".to_vec()),
            (2, b"<< /Type /Pages /Count 1 /Kids [5 0 R] >>".to_vec()),
            (
                5,
                b"<< /Type /Pages /Parent 5 0 R /Count 1 /Kids [3 0 R] >>".to_vec(),
            ),
            (3, page(5, 4, "")),
            (4, text_stream("x")),
        ],
        1,
    )]);
    for (name, pdf) in [
        ("cycle", cyclic),
        ("fan-out", fan_out),
        ("parent loop", parent_loop),
    ] {
        let result = within_seconds(20, move || {
            std::thread::Builder::new()
                .stack_size(256 * 1024)
                .spawn(move || extract(&pdf, &[1]))
                .expect("spawn")
                .join()
                .expect("extraction panicked")
        });
        assert!(
            result.is_err(),
            "{name}: a malformed page tree was extracted"
        );
    }
}

/// WHY: objects refer to each other in chains as long as the file allows
/// (linked annotations, form fields, `/Next` actions). Following them by
/// recursion overflows the stack of a worker thread.
#[test]
fn long_reference_chains_do_not_overflow_a_small_stack() {
    const LINKS: u32 = 60_000;
    let mut objects = vec![
        (1, b"<< /Type /Catalog /Pages 2 0 R >>".to_vec()),
        (2, b"<< /Type /Pages /Count 1 /Kids [3 0 R] >>".to_vec()),
        (3, page(2, 4, "/Chain 10 0 R")),
        (4, text_stream("Chained")),
    ];
    for link in 0..LINKS {
        let body = if link + 1 == LINKS {
            "<< /Last true >>".to_string()
        } else {
            format!("<< /Next {} 0 R >>", link + 11)
        };
        objects.push((link + 10, body.into_bytes()));
    }
    let pdf = build(&[revision(objects, 1)]);
    let output = std::thread::Builder::new()
        .stack_size(256 * 1024)
        .spawn(move || extract(&pdf, &[1]))
        .expect("spawn")
        .join()
        .expect("extraction panicked")
        .expect("a long chain is valid");
    assert_eq!(page_texts(&output), ["Chained"]);
}

/// WHY: a stream's `/Length` may be an indirect object. One that names the
/// stream itself, or another stream whose length names the first, sent the
/// parser into unbounded recursion.
#[test]
fn a_stream_length_that_refers_to_a_stream_is_an_error() {
    for length_of_5 in ["4 0 R", "5 0 R"] {
        let mut content = b"<< /Length 5 0 R >>\nstream\nBT ET\nendstream".to_vec();
        content.extend_from_slice(b"");
        let other = format!("<< /Length {length_of_5} >>\nstream\nx\nendstream");
        let pdf = build(&[revision(
            vec![
                (1, b"<< /Type /Catalog /Pages 2 0 R >>".to_vec()),
                (2, b"<< /Type /Pages /Count 1 /Kids [3 0 R] >>".to_vec()),
                (3, page(2, 4, "")),
                (4, content),
                (5, other.into_bytes()),
            ],
            1,
        )]);
        let result = std::thread::Builder::new()
            .stack_size(256 * 1024)
            .spawn(move || extract(&pdf, &[1]))
            .expect("spawn")
            .join()
            .expect("extraction panicked");
        assert!(result.is_err(), "/Length {length_of_5}");
    }
}

/// WHY: offsets and references in the file are the source's claims. One
/// that points outside the file, or at object 0, must be an error.
#[test]
fn offsets_and_references_outside_the_file_are_errors() {
    let pdf = simple_document(1);
    let text = String::from_utf8_lossy(&pdf).into_owned();
    let start = text.rfind("startxref").expect("startxref");
    let mut beyond = pdf[..start].to_vec();
    beyond.extend_from_slice(b"startxref\n99999999\n%%EOF\n");
    assert!(extract(&beyond, &[1]).is_err());
    let mut enormous = pdf[..start].to_vec();
    enormous.extend_from_slice(b"startxref\n18446744073709551615\n%%EOF\n");
    assert!(extract(&enormous, &[1]).is_err());

    // Unicode whitespace between the parts of the catalog's /Pages reference.
    let odd = build(&[revision(
        vec![
            (
                1,
                "<< /Type /Catalog /Pages 2\u{2003}0\u{2003}R\u{e9} >>"
                    .as_bytes()
                    .to_vec(),
            ),
            (2, b"<< /Type /Pages /Count 1 /Kids [3 0 R] >>".to_vec()),
            (3, page(2, 4, "")),
            (4, text_stream("x")),
        ],
        1,
    )]);
    let _ = extract(&odd, &[1]);

    // Object 0 used as an ordinary object and referred to from a page.
    let mut zero = build(&[revision(
        vec![
            (1, b"<< /Type /Catalog /Pages 2 0 R >>".to_vec()),
            (2, b"<< /Type /Pages /Count 1 /Kids [3 0 R] >>".to_vec()),
            (3, page(2, 4, "/Zero 0 0 R /Big 4294967295 0 R")),
            (4, text_stream("x")),
        ],
        1,
    )]);
    let _ = extract(&zero, &[1]);
    let at = String::from_utf8_lossy(&zero)
        .find("0000000000 65535 f")
        .expect("free entry");
    zero[at..at + 18].copy_from_slice(b"0000000015 00000 n");
    let _ = extract(&zero, &[1]);
}

/// WHY: real page trees are nested. A page's `/Parent` is then an
/// intermediate node, and following it reaches every sibling page; the
/// extraction was refused as "references unselected /Page". A catalog that
/// refers to an object a later revision deleted must not stop it either,
/// because the catalog is written anew.
#[test]
fn one_page_under_a_nested_page_tree_is_extracted_alone() {
    let first = revision(
        vec![
            (
                1,
                b"<< /Type /Catalog /Pages 2 0 R /Metadata 12 0 R >>".to_vec(),
            ),
            (
                2,
                b"<< /Type /Pages /Count 2 /Kids [7 0 R] /MediaBox [0 0 612 792] >>".to_vec(),
            ),
            (
                7,
                format!(
                    "<< /Type /Pages /Parent 2 0 R /Count 2 /Kids [3 0 R 5 0 R] \
                     /Resources << /Font << /F1 {} >> >> >>",
                    common::FONT
                )
                .into_bytes(),
            ),
            (
                3,
                b"<< /Type /Page /Parent 7 0 R /Contents 4 0 R /Up 7 0 R >>".to_vec(),
            ),
            (4, text_stream("Left")),
            (
                5,
                b"<< /Type /Page /Parent 7 0 R /Contents 6 0 R >>".to_vec(),
            ),
            (6, text_stream("Right")),
            (12, stream("/Type /Metadata", b"<xmp/>")),
        ],
        1,
    );
    let second = Revision {
        objects: Vec::new(),
        freed: vec![12],
        trailer: "/Root 1 0 R".to_string(),
    };
    let pdf = build(&[first, second]);
    let output = extract(&pdf, &[2]).expect("extracts one sibling");
    assert_eq!(page_texts(&output), ["Right"]);
    let output = extract(&pdf, &[1]).expect("extracts the other sibling");
    assert_eq!(page_texts(&output), ["Left"]);
    assert!(
        !output.windows(5).any(|window| window == b"Right"),
        "the sibling page came along"
    );
}

/// WHY: a PDF name may spell any character as `#xx`, so `/Encr#79pt` is
/// `/Encrypt` and `/P#61ge` is `/Page`. Comparing the raw bytes let an
/// encrypted source through and lost the pages of a document.
#[test]
fn names_spelled_with_hex_escapes_are_recognised() {
    let objects = vec![
        (1, b"<< /Type /Catalog /Pages 2 0 R >>".to_vec()),
        (2, b"<< /Type /Pages /Count 1 /Kids [3 0 R] >>".to_vec()),
        (3, page(2, 4, "")),
        (4, text_stream("Secret")),
        (
            5,
            b"<< /Filter /Standard /V 1 /R 2 /O (o) /U (u) /P -44 >>".to_vec(),
        ),
    ];
    for name in ["/Encr#79pt", "/#45ncrypt", "/#45#6e#63#72#79#70#74"] {
        let source = Revision {
            objects: objects.clone(),
            freed: Vec::new(),
            trailer: format!("/Root 1 0 R {name} 5 0 R"),
        };
        let error = extract(&build(&[source]), &[1]).expect_err("encrypted source");
        assert!(error.to_lowercase().contains("encrypt"), "{name}: {error}");
    }

    let escaped = |body: &[u8], from: &str, to: &str| {
        String::from_utf8_lossy(body).replace(from, to).into_bytes()
    };
    let pdf = build(&[revision(
        vec![
            (1, b"<< /Type /Catalog /P#61ges 2 0 R >>".to_vec()),
            (
                2,
                b"<< /Type /Pages /Count 2 /K#69ds [3 0 R 5 0 R] /Medi#61Box [0 0 200 200] >>"
                    .to_vec(),
            ),
            (3, escaped(&page(2, 4, ""), "/Type /Page", "/Type /P#61ge")),
            (4, text_stream("First")),
            (5, escaped(&page(2, 6, ""), "/Parent", "/P#61rent")),
            (6, text_stream("Second")),
        ],
        1,
    )]);
    let both = extract(&pdf, &[2, 1]).expect("extracts");
    assert_eq!(page_texts(&both), ["Second", "First"]);
    let one = extract(&pdf, &[2]).expect("extracts");
    assert_eq!(page_texts(&one), ["Second"]);
    assert!(!String::from_utf8_lossy(&one).contains("First"));
}

/// Returns a source whose `count` array objects have no `endobj` of their
/// own: only the last one is closed.
fn objects_sharing_one_endobj(count: u32) -> Vec<u8> {
    let mut pdf = b"%PDF-1.4\n".to_vec();
    let mut offsets = Vec::new();
    let mut add = |pdf: &mut Vec<u8>, text: String| {
        offsets.push(pdf.len());
        pdf.extend_from_slice(text.as_bytes());
    };
    add(
        &mut pdf,
        "1 0 obj\n<< /Type /Catalog /Pages 2 0 R >>\nendobj\n".to_string(),
    );
    add(
        &mut pdf,
        "2 0 obj\n<< /Type /Pages /Count 1 /Kids [3 0 R] >>\nendobj\n".to_string(),
    );
    add(
        &mut pdf,
        format!(
            "3 0 obj\n{}\nendobj\n",
            String::from_utf8_lossy(&page(2, 4, "/X 5 0 R"))
        ),
    );
    let mut contents = b"4 0 obj\n".to_vec();
    contents.extend_from_slice(&text_stream("Shown"));
    contents.extend_from_slice(b"\nendobj\n");
    add(&mut pdf, String::from_utf8_lossy(&contents).into_owned());
    for number in 5..5 + count {
        add(&mut pdf, format!("{number} 0 obj\n[{} 0 R]\n", number + 1));
    }
    add(&mut pdf, format!("{} 0 obj\n[]\nendobj\n", 5 + count));
    let xref_at = pdf.len();
    pdf.extend_from_slice(
        format!("xref\n0 {}\n0000000000 65535 f \n", offsets.len() + 1).as_bytes(),
    );
    for offset in &offsets {
        pdf.extend_from_slice(format!("{offset:010} 00000 n \n").as_bytes());
    }
    pdf.extend_from_slice(
        format!(
            "trailer\n<< /Size {} /Root 1 0 R >>\nstartxref\n{xref_at}\n%%EOF\n",
            offsets.len() + 1
        )
        .as_bytes(),
    );
    pdf
}

/// WHY: an object ends where the next one starts. Reading each up to the
/// next `endobj` anywhere in the file copied the rest of the file once per
/// object, so the output grew with the square of the source.
#[test]
fn an_object_without_endobj_ends_where_the_next_object_starts() {
    let pdf = objects_sharing_one_endobj(600);
    let source_len = pdf.len();
    let output = within_seconds(60, move || extract(&pdf, &[1])).expect("extracts");
    assert!(
        output.len() <= 2 * source_len,
        "{} bytes out of a {source_len} byte source",
        output.len()
    );
    assert_eq!(page_texts(&output), ["Shown"]);
}

/// WHY: a page selected again is written as another copy of its dictionary.
/// With a large dictionary and a long selection the output was the product
/// of the two, a hundred megabytes from a hundred kilobytes.
#[test]
fn repeating_a_large_page_dictionary_is_bounded() {
    let big = format!("/Big ({})", "a".repeat(100_000));
    let pdf = build(&[revision(
        vec![
            (1, b"<< /Type /Catalog /Pages 2 0 R >>".to_vec()),
            (2, b"<< /Type /Pages /Count 1 /Kids [3 0 R] >>".to_vec()),
            (3, page(2, 4, &big)),
            (4, text_stream("Big")),
        ],
        1,
    )]);
    assert_eq!(
        page_texts(&extract(&pdf, &[1, 1, 1]).expect("a few copies")),
        ["Big", "Big", "Big"]
    );
    let error = extract(&pdf, &[1; 1000]).expect_err("a hundred megabytes of copies");
    assert!(error.contains("repeated"), "{error}");
    // Small pages may still be repeated many times.
    let small = simple_document(1);
    assert_eq!(
        page_texts(&extract(&small, &[1; 5000]).expect("extracts")).len(),
        5000
    );
}

/// WHY: a page's annotations, actions and metadata are copied with it by
/// default. A caller that must not pass on active content can ask for the
/// page content alone.
#[test]
fn page_content_only_leaves_out_annotations_actions_and_metadata() {
    use pdf_fast_write::ExtractOptions;
    let document = |dangling: &str| {
        let extra = format!(
            "/Annots [7 0 R 8 0 R{dangling}] /AA << /O << /S /JavaScript /JS (PAGEAAJS) >> >> \
                 /Metadata 9 0 R /PieceInfo << /App << /Private (PIECEMARK) >> >> /Thumb 10 0 R \
                 /#41A << /C << /S /JavaScript /JS (ESCAPEDAAJS) >> >> /Annots [8 0 R]"
        );
        build(&[revision(
        vec![
            (1, b"<< /Type /Catalog /Pages 2 0 R >>".to_vec()),
            (2, b"<< /Type /Pages /Count 2 /Kids [3 0 R 5 0 R] >>".to_vec()),
            (3, page(2, 4, &extra)),
            (4, text_stream("Kept")),
            (5, page(2, 6, "")),
            (6, text_stream("Other")),
            (
                7,
                b"<< /Type /Annot /Subtype /Link /Rect [0 0 9 9] /A << /S /URI /URI (http://URIMARK.example) >> >>".to_vec(),
            ),
            (
                8,
                b"<< /Type /Annot /Subtype /FileAttachment /Rect [0 0 9 9] /FS << /Type /Filespec /F (a.bin) /EF << /F 11 0 R >> >> /A << /S /JavaScript /JS (ANNOTJS) >> >>".to_vec(),
            ),
            (9, stream("/Type /Metadata /Subtype /XML", b"XMPMARK")),
            (10, stream("/Width 1 /Height 1", b"THUMBMARK")),
            (11, stream("/Type /EmbeddedFile", b"ATTACHMARK")),
        ],
        1,
    )])
    };
    let pdf = document(" 12 0 R");
    let markers = [
        "PAGEAAJS",
        "PIECEMARK",
        "ESCAPEDAAJS",
        "URIMARK",
        "ANNOTJS",
        "XMPMARK",
        "THUMBMARK",
        "ATTACHMARK",
        "/Annots",
        "/AA",
        "/Metadata",
        "/PieceInfo",
        "/Thumb",
        "JavaScript",
    ];
    // Object 12 does not exist, so the default extraction refuses the page;
    // without the annotations nothing refers to it.
    assert!(extract(&pdf, &[1]).is_err());
    let options = ExtractOptions::new().page_content_only(true);
    let output = PdfWriter::extract_pages_from_bytes_with(&pdf, &[1, 1], &options)
        .expect("extracts the page content");
    assert_eq!(page_texts(&output), ["Kept", "Kept"]);
    let text = String::from_utf8_lossy(&output);
    for marker in markers {
        assert!(!text.contains(marker), "{marker} is in the output");
    }

    // The default keeps what belongs to the page.
    let kept = document("");
    let output = PdfWriter::extract_pages_from_bytes_with(&kept, &[1], &ExtractOptions::new())
        .expect("extracts");
    let text = String::from_utf8_lossy(&output);
    for marker in ["PAGEAAJS", "URIMARK", "ANNOTJS", "XMPMARK", "ATTACHMARK"] {
        assert!(text.contains(marker), "{marker} is not in the output");
    }
    assert_eq!(output, extract(&kept, &[1]).expect("extracts"));
}

/// WHY: an entry whose value never closes cannot be cut out, and a lenient
/// reader still runs it. Asked for the page content alone, the extraction
/// must refuse such a page instead of handing the entry on.
#[test]
fn page_content_only_refuses_an_entry_it_cannot_remove() {
    let pdf = build(&[revision(
        vec![
            (1, b"<< /Type /Catalog /Pages 2 0 R >>".to_vec()),
            (2, b"<< /Type /Pages /Count 1 /Kids [3 0 R] >>".to_vec()),
            (3, page(2, 4, "/AA << /O << /S /JavaScript /JS (KEPTJS)")),
            (4, text_stream("Kept")),
        ],
        1,
    )]);
    let options = pdf_fast_write::ExtractOptions::new().page_content_only(true);
    let result = PdfWriter::extract_pages_from_bytes_with(&pdf, &[1], &options);
    let error = result
        .map(|output| String::from_utf8_lossy(&output).contains("KEPTJS"))
        .expect_err("the entry was not removed");
    assert!(format!("{error:#}").contains("/AA"), "{error:#}");
}

/// Returns a two-page document whose cross-reference table also lists the
/// offsets `strays` returns, given the real offsets and the body length, as
/// objects in use.
fn document_with_stray_entries(strays: impl FnOnce(&[usize], usize) -> Vec<usize>) -> Vec<u8> {
    let objects = [
        b"<< /Type /Catalog /Pages 2 0 R >>".to_vec(),
        b"<< /Type /Pages /Count 2 /Kids [3 0 R 5 0 R] >>".to_vec(),
        page(2, 4, ""),
        text_stream("Page 1"),
        page(2, 6, ""),
        text_stream("Page 2"),
    ];
    let mut pdf = b"%PDF-1.4\n".to_vec();
    let mut offsets = Vec::new();
    for (index, body) in objects.iter().enumerate() {
        offsets.push(pdf.len());
        pdf.extend_from_slice(format!("{} 0 obj\n", index + 1).as_bytes());
        pdf.extend_from_slice(body);
        pdf.extend_from_slice(b"\nendobj\n");
    }
    let xref_at = pdf.len();
    let strays = strays(&offsets, xref_at);
    offsets.extend(strays);
    let mut table = format!("xref\n0 {}\n0000000000 65535 f \n", offsets.len() + 1);
    for offset in &offsets {
        table.push_str(&format!("{offset:010} 00000 n \n"));
    }
    table.push_str(&format!(
        "trailer\n<< /Size {} /Root 1 0 R >>\nstartxref\n{xref_at}\n%%EOF\n",
        offsets.len() + 1
    ));
    pdf.extend_from_slice(table.as_bytes());
    pdf
}

/// WHY: an xref offset bounds the object before it only when an object
/// header stands there. A stray in-use entry pointing into a page dictionary
/// used to cut the page off mid-entry, and the cut page was written as a
/// success: no `/Contents`, no `/MediaBox`, no closing `>>`.
#[test]
fn a_stray_entry_inside_a_page_dictionary_does_not_cut_the_page() {
    let whole = extract(&document_with_stray_entries(|_, _| Vec::new()), &[1]).expect("extracts");
    let pdf = document_with_stray_entries(|offsets, _| vec![offsets[2] + 40]);
    let output = extract(&pdf, &[1]).expect("extracts");
    let text = String::from_utf8_lossy(&output);
    assert!(text.contains("/Contents"), "{text}");
    assert!(text.contains("/MediaBox"), "{text}");
    assert_eq!(output, whole, "the page is copied whole");
    assert_eq!(page_texts(&output), ["Page 1"]);
}

/// WHY: the same stray entry inside a content stream made the stream look
/// longer than its object, and a source that used to extract was refused.
#[test]
fn a_stray_entry_inside_a_content_stream_does_not_stop_extraction() {
    let whole = extract(&document_with_stray_entries(|_, _| Vec::new()), &[1]).expect("extracts");
    let pdf = document_with_stray_entries(|offsets, _| vec![offsets[3] + 40]);
    assert_eq!(extract(&pdf, &[1]).expect("extracts"), whole);
}

/// WHY: a table may hold any number of stray entries; one at every offset of
/// the file must leave every object whole.
#[test]
fn a_million_stray_entries_at_every_offset_leave_the_objects_whole() {
    let whole =
        extract(&document_with_stray_entries(|_, _| Vec::new()), &[1, 2]).expect("extracts");
    let pdf = document_with_stray_entries(|_, len| (0..1_000_000).map(|n| n % len).collect());
    let output = extract(&pdf, &[1, 2]).expect("extracts");
    assert_eq!(output, whole);
    assert_eq!(page_texts(&output), ["Page 1", "Page 2"]);
}

/// WHY: the refusal of too many repeats is read by callers; its text had a
/// run of spaces in it from a string continued without a backslash.
#[test]
fn the_repeat_limit_error_reads_as_one_sentence() {
    let big = format!("/Big ({})", "a".repeat(100_000));
    let pdf = build(&[revision(
        vec![
            (1, b"<< /Type /Catalog /Pages 2 0 R >>".to_vec()),
            (2, b"<< /Type /Pages /Count 1 /Kids [3 0 R] >>".to_vec()),
            (3, page(2, 4, &big)),
            (4, text_stream("Big")),
        ],
        1,
    )]);
    assert_eq!(
        extract(&pdf, &[1; 1000]).expect_err("over the limit"),
        "the repeated pages need more than 16777216 bytes of page dictionary copies; \
         select fewer repeats"
    );
}
