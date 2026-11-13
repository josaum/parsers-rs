//! Lossy UTF-8 input for the XML reader.
//!
//! quick-xml 0.42 validates every event as UTF-8 and fails the whole document
//! on the first byte that is not. The parsers built on it read such a byte as
//! U+FFFD and carry on, as they did when quick-xml handed them raw bytes, so
//! their input passes through [`LossyUtf8`] (streams) or [`lossy_utf8`]
//! (buffers) first. Both replace exactly what [`String::from_utf8_lossy`]
//! replaces: each maximal ill-formed subsequence becomes one U+FFFD.

use std::borrow::Cow;
use std::io::{self, BufRead, Read};

/// U+FFFD REPLACEMENT CHARACTER encoded as UTF-8.
const REPLACEMENT: &[u8] = "\u{FFFD}".as_bytes();

/// Returns `bytes` as UTF-8, each ill-formed sequence replaced by U+FFFD.
///
/// Valid input is borrowed unchanged. The result is `Cow::Owned` exactly when
/// a replacement was made.
#[must_use]
pub fn lossy_utf8(bytes: &[u8]) -> Cow<'_, [u8]> {
    match String::from_utf8_lossy(bytes) {
        Cow::Borrowed(text) => Cow::Borrowed(text.as_bytes()),
        Cow::Owned(text) => Cow::Owned(text.into_bytes()),
    }
}

/// A [`BufRead`] that yields its input as UTF-8, replacing ill-formed bytes.
///
/// Valid input is passed through from the inner reader's buffer without a
/// copy. A sequence split across two of the inner reader's buffers is joined
/// first, so a character is never replaced for straddling a boundary.
#[derive(Debug)]
pub struct LossyUtf8<R> {
    inner: R,
    /// Bytes produced here rather than borrowed from `inner`: a replacement
    /// character, or a character joined across two inner buffers.
    pending: [u8; 4],
    pending_start: usize,
    pending_end: usize,
    /// Bytes at the front of the inner buffer already checked as UTF-8, so
    /// each byte is validated once however often the buffer is refilled.
    validated: usize,
    repaired: bool,
}

impl<R: BufRead> LossyUtf8<R> {
    /// Wraps `inner`.
    pub fn new(inner: R) -> Self {
        Self {
            inner,
            pending: [0; 4],
            pending_start: 0,
            pending_end: 0,
            validated: 0,
            repaired: false,
        }
    }

    /// Whether a byte that is not UTF-8 has been replaced so far.
    #[must_use]
    pub fn repaired(&self) -> bool {
        self.repaired
    }

    fn set_pending(&mut self, bytes: &[u8]) {
        self.pending[..bytes.len()].copy_from_slice(bytes);
        self.pending_start = 0;
        self.pending_end = bytes.len();
    }

    fn replace(&mut self) {
        self.repaired = true;
        self.set_pending(REPLACEMENT);
    }

    /// Completes the character whose first `carry_len` bytes, held in
    /// `carry`, ended an inner buffer, peeking at the next bytes one at a
    /// time and consuming only those that belong to it.
    fn join_split_sequence(&mut self, mut carry: [u8; 4], mut carry_len: usize) -> io::Result<()> {
        loop {
            let next = self.inner.fill_buf()?;
            let Some(&byte) = next.first() else {
                // Input ends inside a character.
                self.replace();
                return Ok(());
            };
            carry[carry_len] = byte;
            match std::str::from_utf8(&carry[..=carry_len]) {
                Ok(_) => {
                    self.inner.consume(1);
                    self.set_pending(&carry[..=carry_len]);
                    return Ok(());
                }
                Err(error) if error.error_len().is_none() && carry_len + 1 < 4 => {
                    self.inner.consume(1);
                    carry_len += 1;
                }
                Err(_) => {
                    // `byte` cannot continue the character: the bytes before
                    // it are one ill-formed sequence and `byte` is read next.
                    self.replace();
                    return Ok(());
                }
            }
        }
    }
}

impl<R: BufRead> Read for LossyUtf8<R> {
    fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
        let available = self.fill_buf()?;
        let amount = available.len().min(buf.len());
        buf[..amount].copy_from_slice(&available[..amount]);
        self.consume(amount);
        Ok(amount)
    }
}

impl<R: BufRead> BufRead for LossyUtf8<R> {
    fn fill_buf(&mut self) -> io::Result<&[u8]> {
        if self.pending_start == self.pending_end {
            if self.validated > 0 {
                let buf = self.inner.fill_buf()?;
                return Ok(&buf[..self.validated.min(buf.len())]);
            }
            // What to do is decided first and the inner buffer re-borrowed
            // after, so that the passthrough can return it.
            let (valid, invalid, split) = {
                let buf = self.inner.fill_buf()?;
                match std::str::from_utf8(buf) {
                    Ok(_) => (buf.len(), 0, None),
                    Err(error) if error.valid_up_to() > 0 => (error.valid_up_to(), 0, None),
                    Err(error) => match error.error_len() {
                        Some(length) => (0, length, None),
                        None => {
                            let mut carry = [0u8; 4];
                            carry[..buf.len()].copy_from_slice(buf);
                            (0, 0, Some((carry, buf.len())))
                        }
                    },
                }
            };
            if invalid > 0 {
                self.inner.consume(invalid);
                self.replace();
            } else if let Some((carry, carry_len)) = split {
                self.inner.consume(carry_len);
                self.join_split_sequence(carry, carry_len)?;
            } else {
                self.validated = valid;
                return Ok(&self.inner.fill_buf()?[..valid]);
            }
        }
        Ok(&self.pending[self.pending_start..self.pending_end])
    }

    fn consume(&mut self, amount: usize) {
        if self.pending_start < self.pending_end {
            self.pending_start = (self.pending_start + amount).min(self.pending_end);
        } else {
            self.validated = self.validated.saturating_sub(amount);
            self.inner.consume(amount);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::BufReader;

    /// Reads `bytes` through a [`LossyUtf8`] whose inner buffer holds
    /// `capacity` bytes, so sequences straddle inner buffer boundaries.
    fn read_lossy(bytes: &[u8], capacity: usize) -> (Vec<u8>, bool) {
        let mut reader = LossyUtf8::new(BufReader::with_capacity(capacity, bytes));
        let mut out = Vec::new();
        reader.read_to_end(&mut out).expect("read from a slice");
        (out, reader.repaired())
    }

    #[test]
    fn stream_matches_from_utf8_lossy_at_every_buffer_size() {
        let cases: &[&[u8]] = &[
            b"plain ascii",
            "caf\u{e9} \u{1F600} \u{4E2D}".as_bytes(),
            b"Caf\xe9 <a b=\"\xe9\"/>",
            b"\xf0\x9f\x98",
            b"\xf0\x9f\x98<",
            b"\xe2\x82\xac\xe2\x82",
            b"\xc3\x28\xa0\xa1\xe2\x28\xa1\xf0\x28\x8c\xbc",
            b"\xed\xa0\x80 surrogate",
            b"\xff\xfe<\x00?\x00",
        ];
        for case in cases {
            let expected = String::from_utf8_lossy(case).into_owned().into_bytes();
            for capacity in 1..=8 {
                let (out, repaired) = read_lossy(case, capacity);
                assert_eq!(out, expected, "case {case:?}, capacity {capacity}");
                assert_eq!(
                    repaired,
                    std::str::from_utf8(case).is_err(),
                    "case {case:?}"
                );
            }
            assert_eq!(lossy_utf8(case).as_ref(), expected.as_slice());
        }
    }

    #[test]
    fn valid_buffer_is_borrowed() {
        assert!(matches!(
            lossy_utf8("caf\u{e9}".as_bytes()),
            Cow::Borrowed(_)
        ));
        assert!(matches!(lossy_utf8(b"caf\xe9"), Cow::Owned(_)));
    }
}
