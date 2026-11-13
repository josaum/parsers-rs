//! The vectorized digit scan against a byte-by-byte scan, sized for Miri.
//!
//! The unit test next to `find_first_digit` covers far more inputs, and takes
//! the interpreter the better part of an hour. This one keeps to the lengths
//! around the 16- and 32-byte block sizes, so that
//! `cargo +nightly miri test -p xlsx-fast-core --test digit_scan` checks every
//! `unsafe` load and pointer offset of the scan in about a minute.

use xlsx_fast_core::find_first_digit;

fn scalar(bytes: &[u8]) -> Option<usize> {
    bytes.iter().position(u8::is_ascii_digit)
}

#[test]
fn the_vectorized_scan_agrees_with_a_scalar_scan_at_every_length() {
    // `/` and `:` are the neighbours of the digits in ASCII; 0xB5 is a digit
    // with the high bit set, which a signed comparison could take for one.
    for len in 0..=70usize {
        for filler in [b'/', b':', 0xB5] {
            let mut bytes = vec![filler; len];
            assert_eq!(find_first_digit(&bytes), None, "len {len}");
            for position in 0..len {
                bytes[position] = b"059"[position % 3];
                assert_eq!(
                    find_first_digit(&bytes),
                    Some(position),
                    "len {len}, digit at {position}"
                );
                bytes[position] = filler;
            }
        }
    }
}

#[test]
fn a_slice_that_does_not_start_on_an_alignment_boundary_is_scanned() {
    let buffer: Vec<u8> = b"$AB$:/XFD"
        .iter()
        .copied()
        .cycle()
        .take(95)
        .chain(*b"7")
        .collect();
    for start in 0..33 {
        for end in start..=buffer.len() {
            let bytes = &buffer[start..end];
            assert_eq!(find_first_digit(bytes), scalar(bytes), "{start}..{end}");
        }
    }
}
