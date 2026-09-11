//! Property-based invariants for `ooxml-archive`.
//!
//! Complements the inline unit tests in `src/lib.rs`: path helpers must be
//! total and non-escaping, `max_inflate` must fail loud on real archives,
//! and stored parts must stay zero-copy borrowed.

use ooxml_archive::{
    part_dir, relationship_matches, rels_path_for_part, resolve_part, source_part_from_rels,
    OoxmlArchive, STRICT_RELATIONSHIP_PREFIX,
};
use proptest::prelude::*;
use std::io::{Cursor, Write};
use zip::write::FileOptions;
use zip::{CompressionMethod as ZipCompression, ZipWriter};

/// Path segment that can appear inside an OOXML package part path.
fn segment() -> impl Strategy<Value = String> {
    "[a-z][a-z0-9_]{0,7}"
}

/// Leaf file name with a realistic package extension.
fn file_name() -> impl Strategy<Value = String> {
    (
        segment(),
        prop::sample::select(vec!["xml", "bin", "jpeg", "png", "vml"]),
    )
        .prop_map(|(base, ext)| format!("{base}.{ext}"))
}

/// OOXML-ish part path such as `word/document.xml` or `ppt/slides/slide1.xml`.
fn part_path() -> impl Strategy<Value = String> {
    (prop::collection::vec(segment(), 0..=3), file_name()).prop_map(|(dirs, name)| {
        if dirs.is_empty() {
            name
        } else {
            format!("{}/{}", dirs.join("/"), name)
        }
    })
}

/// Directory path without `.`/`..` segments.
fn clean_dir() -> impl Strategy<Value = String> {
    prop::collection::vec(segment(), 0..=4).prop_map(|dirs| dirs.join("/"))
}

/// Arbitrary path-ish string, including separators and `..` segments.
fn arb_path() -> impl Strategy<Value = String> {
    "[a-zA-Z0-9./_\\\\]{0,40}"
}

/// Relationship Type URI candidates: transitional, strict, and junk.
fn rel_type() -> impl Strategy<Value = String> {
    prop_oneof![
        Just(
            "http://schemas.openxmlformats.org/officeDocument/2006/relationships/officeDocument"
                .to_string()
        ),
        Just(
            "http://schemas.openxmlformats.org/officeDocument/2006/relationships/image".to_string()
        ),
        Just("http://purl.oclc.org/ooxml/officeDocument/relationships/officeDocument".to_string()),
        "[a-z]{3,12}",
        Just(String::new()),
    ]
}

/// Build an in-memory ZIP package with the given parts.
fn zip_bytes(parts: &[(&str, &[u8], ZipCompression)]) -> Vec<u8> {
    let mut writer = ZipWriter::new(Cursor::new(Vec::new()));
    for (name, body, method) in parts {
        let options = FileOptions::default().compression_method(*method);
        writer.start_file(*name, options).unwrap();
        writer.write_all(body).unwrap();
    }
    writer.finish().unwrap().into_inner()
}

proptest! {
    #![proptest_config(ProptestConfig::with_cases(64))]

    /// `rels_path_for_part` and `source_part_from_rels` are exact inverses on
    /// package part paths, and the rels path always lives under `_rels/`.
    #[test]
    fn rels_path_round_trips(part in part_path()) {
        let rels = rels_path_for_part(&part);
        prop_assert!(rels.ends_with(".rels"), "rels path lost suffix: {rels}");
        prop_assert!(!rels.contains(".."), "rels path escapes: {rels}");
        let dir = part_dir(&part);
        if dir.is_empty() {
            prop_assert!(rels.starts_with("_rels/"), "rels path outside _rels: {rels}");
        } else {
            prop_assert!(
                rels.starts_with(&format!("{dir}/_rels/")),
                "rels path outside {dir}/_rels: {rels}"
            );
        }
        let round_tripped = source_part_from_rels(&rels);
        prop_assert_eq!(round_tripped.as_deref(), Some(part.as_str()));
    }

    /// A relative target can never escape the package root when the base
    /// directory is itself a clean in-package path.
    #[test]
    fn resolve_part_never_escapes(base_dir in clean_dir(), target in arb_path()) {
        let resolved = resolve_part(&base_dir, &target);
        prop_assert!(!resolved.starts_with('/'), "resolved is absolute: {resolved}");
        prop_assert!(
            !resolved.split('/').any(|seg| seg == ".."),
            "resolved retains .. segment: {resolved}"
        );
    }

    /// With arbitrary base directories, `..` segments can only ever come
    /// from the base itself; the target can introduce none.
    #[test]
    fn resolve_part_dotdot_comes_only_from_base(base_dir in arb_path(), target in arb_path()) {
        let resolved = resolve_part(&base_dir, &target);
        prop_assert!(!resolved.starts_with('/'), "resolved is absolute: {resolved}");
        let base_normalized = base_dir.replace('\\', "/");
        let base_dotdot = base_normalized.split('/').filter(|&seg| seg == "..").count();
        let out_dotdot = resolved.split('/').filter(|&seg| seg == "..").count();
        prop_assert!(out_dotdot <= base_dotdot, "target injected ..: {resolved}");
    }


    /// Every Type URI matches itself.
    #[test]
    fn relationship_matches_is_reflexive(rel in rel_type()) {
        prop_assert!(relationship_matches(&rel, &rel));
    }

    /// The strict ISO form matches the transitional canonical URI, but the
    /// comparison is intentionally one-directional.
    #[test]
    fn strict_matches_transitional_but_not_reverse(kind in "[a-z]{3,12}") {
        let transitional = format!(
            "http://schemas.openxmlformats.org/officeDocument/2006/relationships/{kind}"
        );
        let strict = format!("{STRICT_RELATIONSHIP_PREFIX}{kind}");
        prop_assert!(relationship_matches(&strict, &transitional));
        prop_assert!(!relationship_matches(&transitional, &strict));
    }

    /// An isolated `read_part` request succeeds iff the uncompressed size fits
    /// `max_inflate`, on both the stored and deflated paths. Each compression
    /// path gets its own request budget; aggregate behavior is covered by the
    /// shared-budget tests in the library suite.
    #[test]
    fn max_inflate_fails_loud(size in 0usize..2048, max_inflate in 0usize..2048, fill in any::<u8>()) {
        let body = vec![fill; size];
        let bytes = zip_bytes(&[
            ("stored/part.bin", &body, ZipCompression::Stored),
            ("deflated/part.bin", &body, ZipCompression::Deflated),
        ]);
        for name in ["stored/part.bin", "deflated/part.bin"] {
            let archive = OoxmlArchive::from_bytes(bytes.clone()).unwrap();
            match archive.read_part(name, max_inflate) {
                Ok(part) => {
                    prop_assert!(
                        size <= max_inflate,
                        "{name} read {size} bytes above max_inflate {max_inflate}"
                    );
                    prop_assert_eq!(part.as_slice(), body.as_slice());
                }
                Err(err) => {
                    prop_assert!(
                        size > max_inflate,
                        "{name} rejected {size} bytes within max_inflate {max_inflate}: {err:#}"
                    );
                    prop_assert!(
                        err.to_string().contains("max_inflate"),
                        "{name} error does not name max_inflate: {err:#}"
                    );
                }
            }
        }
    }

    /// `max_inflate` is an **aggregate** budget for one archive view, not a
    /// per-part cap: two parts of `size` bytes each fit exactly when their sum
    /// does, and the second read is what reports the overflow.
    ///
    /// This is the property the per-part form cannot express, and the reason a
    /// package with many individually-small parts cannot multiply the limit.
    ///
    /// `size` starts at 1: a zero-length part is a degenerate case the budget
    /// arithmetic has nothing to say about, and asserting anything about it
    /// here would be testing the generator rather than the archive.
    #[test]
    fn max_inflate_is_charged_across_parts(
        size in 1usize..1024,
        fill in any::<u8>(),
    ) {
        let body = vec![fill; size];
        let bytes = zip_bytes(&[
            ("first.bin", &body, ZipCompression::Stored),
            ("second.bin", &body, ZipCompression::Stored),
        ]);
        let archive = OoxmlArchive::from_bytes(bytes).unwrap();
        let budget = size.saturating_mul(2);

        let first = archive.read_part("first.bin", budget);
        prop_assert!(first.is_ok(), "the first part alone must fit: {size} of {budget}");
        first.unwrap();

        // Exactly twice `size` was budgeted, so the second read consumes the
        // remainder and must succeed rather than be rejected as a fresh cap.
        let second = archive.read_part("second.bin", budget);
        prop_assert!(
            second.is_ok(),
            "two parts of {size} must fit a budget of {budget}: {second:?}"
        );

        // The budget is now exactly spent, so the next read of a non-empty
        // part must fail and say so in terms a caller can act on.
        let third = archive.read_part("first.bin", budget);
        let err = third.expect_err("a third part must exceed an aggregate budget of two");
        prop_assert!(
            err.to_string().contains("max_inflate"),
            "the overflow error must name max_inflate: {err:#}"
        );
    }

    /// A fresh archive view gets a fresh budget; the same bytes read twice in
    /// isolation must both succeed. This is what keeps the aggregate cap from
    /// behaving like a one-shot latch.
    #[test]
    fn a_reopened_archive_starts_a_fresh_budget(size in 0usize..1024, fill in any::<u8>()) {
        let body = vec![fill; size];
        let bytes = zip_bytes(&[("part.bin", &body, ZipCompression::Deflated)]);

        for _ in 0..2 {
            let archive = OoxmlArchive::from_bytes(bytes.clone()).unwrap();
            let part = archive
                .read_part("part.bin", size)
                .unwrap_or_else(|err| panic!("a {size}-byte part must fit a {size} budget: {err:#}"));
            prop_assert_eq!(part.as_slice(), body.as_slice());
        }
    }

    /// Stored parts borrow the archive bytes; `into_owned` round-trips the
    /// content for both stored and deflated parts.
    #[test]
    fn stored_borrows_and_into_owned_round_trips(
        content in prop::collection::vec(any::<u8>(), 0..512)
    ) {
        let bytes = zip_bytes(&[
            ("stored.bin", &content, ZipCompression::Stored),
            ("deflated.bin", &content, ZipCompression::Deflated),
        ]);
        let archive = OoxmlArchive::from_bytes(bytes).unwrap();

        let stored = archive.read_part("stored.bin", usize::MAX).unwrap();
        prop_assert!(stored.is_borrowed(), "stored part must be zero-copy");
        prop_assert_eq!(stored.as_slice(), content.as_slice());
        prop_assert_eq!(stored.into_owned(), content.clone());

        let deflated = archive.read_part("deflated.bin", usize::MAX).unwrap();
        prop_assert!(!deflated.is_borrowed(), "deflated part must own its bytes");
        prop_assert_eq!(deflated.into_owned(), content);
    }

    /// `from_bytes` on arbitrary bytes either errors or yields a store whose
    /// index probes never panic.
    #[test]
    fn from_bytes_never_panics(data in prop::collection::vec(any::<u8>(), 0..256)) {
        if let Ok(store) = OoxmlArchive::from_bytes(data) {
            let names: Vec<String> = store.part_names().take(8).map(str::to_string).collect();
            for name in &names {
                prop_assert!(store.has_part(name), "index/probe mismatch for {name}");
                let _ = store.read_part(name, 1 << 20);
            }
        }
    }
}
