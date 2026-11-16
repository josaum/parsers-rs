//! The vectorized `obj` marker scan against a byte-by-byte scan.
//!
//! Small enough to run under Miri (`cargo +nightly miri test -p pdf-fast-core
//! --test obj_marker_scan`), which checks the `unsafe` SIMD loads in
//! `find_obj_markers_simd` for out-of-bounds reads at every length around the
//! 16-byte block size.

use pdf_fast_core::find_obj_markers_simd;

/// Every offset where the three bytes `obj` start.
fn scalar(bytes: &[u8]) -> Vec<usize> {
    (0..bytes.len().saturating_sub(2))
        .filter(|&at| &bytes[at..at + 3] == b"obj")
        .collect()
}

#[test]
fn the_vectorized_scan_agrees_with_a_scalar_scan_at_every_length() {
    // Filler that contains the marker's first byte without the marker, so a
    // candidate is found and then rejected, including at a block's last byte.
    let filler = b"1 0 o ob oo endo bj\n";
    for len in 0..=40 {
        let base: Vec<u8> = filler.iter().copied().cycle().take(len).collect();
        assert_eq!(find_obj_markers_simd(&base), scalar(&base), "len {len}");
        for at in 0..len.saturating_sub(2) {
            let mut bytes = base.clone();
            bytes[at..at + 3].copy_from_slice(b"obj");
            assert_eq!(
                find_obj_markers_simd(&bytes),
                scalar(&bytes),
                "len {len}, marker at {at}"
            );
        }
    }
}

#[test]
fn a_slice_that_does_not_start_on_an_alignment_boundary_is_scanned() {
    let buffer: Vec<u8> = b"xx1 0 obj<<>>endobj 2 0 obj<<>>endobj 3 0 obj"
        .iter()
        .copied()
        .cycle()
        .take(96)
        .collect();
    for start in 0..17 {
        let bytes = &buffer[start..];
        assert_eq!(find_obj_markers_simd(bytes), scalar(bytes), "start {start}");
    }
}
