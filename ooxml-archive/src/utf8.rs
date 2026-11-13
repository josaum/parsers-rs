//! Lossy UTF-8 input for the XML reader.
//!
//! The XML reader accepts only UTF-8 and fails a whole document on its first
//! byte that is not. The parsers built on it read such a byte as U+FFFD and
//! carry on, so their input passes through [`LossyUtf8`] first. It replaces
//! exactly what [`String::from_utf8_lossy`] replaces: each maximal
//! ill-formed subsequence becomes one U+FFFD.
//!
//! [`LossyUtf8`] also remembers where it put each replacement, so a parser
//! that used to reject a value that was not UTF-8 can tell a replacement in
//! that value from a U+FFFD the document really holds, or from a
//! replacement elsewhere in the part ([`LossyUtf8::repaired_in`]).

use std::borrow::Cow;
use std::io::{self, BufRead, Read};
use std::ops::RangeInclusive;

/// U+FFFD REPLACEMENT CHARACTER encoded as UTF-8.
const REPLACEMENT: &[u8] = "\u{FFFD}".as_bytes();

/// Most separate repaired spans remembered per stream.
///
/// Past this many, the spans are merged into one that covers them all, so
/// memory stays bounded however many stray bytes a part holds. A value
/// inside the merged span then counts as repaired if it holds U+FFFD.
const MAX_SPANS: usize = 4096;

/// Returns `bytes` as UTF-8, each ill-formed sequence replaced by U+FFFD.
///
/// Valid input is borrowed unchanged.
#[must_use]
pub fn lossy_utf8(bytes: &[u8]) -> Cow<'_, str> {
    String::from_utf8_lossy(bytes)
}

/// A [`BufRead`] that yields its input as UTF-8, replacing ill-formed bytes.
///
/// Valid input is passed through from the inner reader's buffer without a
/// copy, and each byte is validated once. A sequence split across two of the
/// inner reader's buffers is joined first, so a character is never replaced
/// for straddling a boundary, and an [`io::ErrorKind::Interrupted`] from the
/// inner reader in the middle of one loses nothing.
#[derive(Debug)]
pub struct LossyUtf8<R> {
    inner: R,
    /// Bytes produced here rather than borrowed from `inner`: a replacement
    /// character, or a character joined across two inner buffers.
    pending: [u8; 4],
    pending_start: usize,
    pending_end: usize,
    /// The start of a character consumed from `inner` whose remaining bytes
    /// have not been read yet.
    carry: [u8; 4],
    carry_len: usize,
    /// Bytes at the front of the inner buffer already checked as UTF-8.
    validated: usize,
    /// Output bytes consumed so far.
    position: u64,
    /// Most output bytes this stream may produce.
    limit: u64,
    /// Output ranges holding a replacement, ascending and disjoint.
    spans: Vec<(u64, u64)>,
    replacements: u64,
}

impl<R: BufRead> LossyUtf8<R> {
    /// Wraps `inner`, with no limit on the output.
    pub fn new(inner: R) -> Self {
        Self::with_limit(inner, u64::MAX)
    }

    /// Wraps `inner`, refusing to produce more than `limit` bytes.
    ///
    /// Replacing one byte with U+FFFD takes three, so repaired output can be
    /// three times its input. Pass the limit the input was held to, and a
    /// part that only grows past it by being repaired is refused instead of
    /// being read at up to three times the size its limit allowed. Valid
    /// UTF-8 is never longer than its input.
    pub fn with_limit(inner: R, limit: u64) -> Self {
        Self {
            inner,
            pending: [0; 4],
            pending_start: 0,
            pending_end: 0,
            carry: [0; 4],
            carry_len: 0,
            validated: 0,
            position: 0,
            limit,
            spans: Vec::new(),
            replacements: 0,
        }
    }

    /// Whether a byte that is not UTF-8 has been replaced so far.
    #[must_use]
    pub fn repaired(&self) -> bool {
        self.replacements > 0
    }

    /// Output bytes consumed so far.
    #[must_use]
    pub fn position(&self) -> u64 {
        self.position
    }

    /// Whether `value` holds a U+FFFD this reader put there.
    ///
    /// `event` is the content of the event the XML reader returned last
    /// (what a `BytesStart`, `BytesText`, `BytesCData` or `BytesRef`
    /// dereferences to) and `value` a slice of it: an attribute's raw value,
    /// a name, or the text itself. `after` is how many bytes the reader
    /// consumed after the event's content: `1..=2` for a start or empty tag
    /// (`>` or `/>`), `1..=1` for an end tag or a reference (`>`, `;`),
    /// `3..=3` for CDATA (`]]>`), and `0..=0` for text the reader does not
    /// trim. A range covers both cases at once; that is exact for values
    /// and names, whose neighbors (a quote, `<`, `>`, `/` or white space)
    /// are never a replacement.
    ///
    /// A U+FFFD the input itself held, or a replacement outside `value`,
    /// does not count. A `value` that is not a slice of `event` counts as
    /// repaired when any byte of `event` was.
    #[must_use]
    pub fn repaired_in(&self, event: &[u8], after: RangeInclusive<u64>, value: &[u8]) -> bool {
        if self.replacements == 0 || !contains_replacement(value) {
            return false;
        }
        let event_start = event.as_ptr() as usize;
        let event_end = event_start + event.len();
        let value_start = value.as_ptr() as usize;
        let value = if value_start >= event_start && value_start + value.len() <= event_end {
            value
        } else {
            event
        };
        let value_start = value.as_ptr() as usize;
        let to_value_start = (event_end - value_start) as u64;
        let to_value_end = (event_end - value_start - value.len()) as u64;
        let start = self
            .position
            .saturating_sub(after.end().saturating_add(to_value_start));
        let end = self
            .position
            .saturating_sub(after.start().saturating_add(to_value_end));
        self.repaired_between(start, end)
    }

    /// Whether a replacement overlaps output bytes `start..end`.
    fn repaired_between(&self, start: u64, end: u64) -> bool {
        let first = self
            .spans
            .partition_point(|&(_, span_end)| span_end <= start);
        self.spans
            .get(first)
            .is_some_and(|&(span_start, _)| span_start < end)
    }

    fn set_pending(&mut self, bytes: &[u8]) {
        self.pending[..bytes.len()].copy_from_slice(bytes);
        self.pending_start = 0;
        self.pending_end = bytes.len();
    }

    /// Queues one U+FFFD and records where it lands in the output.
    fn replace(&mut self) {
        self.replacements += 1;
        let start = self.position;
        let end = start + REPLACEMENT.len() as u64;
        match self.spans.last_mut() {
            Some(last) if last.1 == start => last.1 = end,
            _ => self.spans.push((start, end)),
        }
        if self.spans.len() > MAX_SPANS {
            let merged = (self.spans[0].0, end);
            self.spans.clear();
            self.spans.push(merged);
        }
        self.set_pending(REPLACEMENT);
    }

    /// Continues the character held in `carry` with the next input byte.
    ///
    /// Returns `true` once the character is complete or replaced (queued in
    /// `pending`), `false` when it needs another byte. Only bytes that
    /// belong to the character are consumed.
    fn continue_carry(&mut self) -> io::Result<bool> {
        let next = self.inner.fill_buf()?;
        let Some(&byte) = next.first() else {
            // Input ends inside a character.
            self.carry_len = 0;
            self.replace();
            return Ok(true);
        };
        let length = self.carry_len;
        self.carry[length] = byte;
        match std::str::from_utf8(&self.carry[..=length]) {
            Ok(_) => {
                self.inner.consume(1);
                let carry = self.carry;
                self.set_pending(&carry[..=length]);
                self.carry_len = 0;
                Ok(true)
            }
            Err(error) if error.error_len().is_none() && length + 1 < 4 => {
                self.inner.consume(1);
                self.carry_len += 1;
                Ok(false)
            }
            Err(_) => {
                // `byte` cannot continue the character: the bytes before it
                // are one ill-formed sequence and `byte` is read next.
                self.carry_len = 0;
                self.replace();
                Ok(true)
            }
        }
    }
}

/// Whether `bytes` holds the UTF-8 encoding of U+FFFD.
fn contains_replacement(bytes: &[u8]) -> bool {
    bytes
        .windows(REPLACEMENT.len())
        .any(|window| window == REPLACEMENT)
}

/// The part of `available` that fits under `limit` at `position`.
///
/// # Errors
///
/// Fails with [`io::ErrorKind::InvalidData`] when output remains but the
/// limit is reached.
fn within_limit(available: &[u8], position: u64, limit: u64) -> io::Result<&[u8]> {
    if available.is_empty() {
        return Ok(available);
    }
    let room = limit.saturating_sub(position);
    if room == 0 {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            format!("part is larger than {limit} bytes once repaired to UTF-8"),
        ));
    }
    let length = usize::try_from(room).map_or(available.len(), |room| room.min(available.len()));
    Ok(&available[..length])
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
        while self.pending_start == self.pending_end {
            if self.carry_len > 0 {
                self.continue_carry()?;
                continue;
            }
            if self.validated > 0 {
                let buf = self.inner.fill_buf()?;
                let valid = self.validated.min(buf.len());
                return within_limit(&buf[..valid], self.position, self.limit);
            }
            // What to do is decided first and the inner buffer re-borrowed
            // after, so that the passthrough can return it.
            let (valid, invalid, split) = {
                let buf = self.inner.fill_buf()?;
                match std::str::from_utf8(buf) {
                    Ok(_) => (buf.len(), 0, 0),
                    Err(error) if error.valid_up_to() > 0 => (error.valid_up_to(), 0, 0),
                    Err(error) => match error.error_len() {
                        Some(length) => (0, length, 0),
                        None => {
                            // An incomplete character ends the buffer.
                            self.carry[..buf.len()].copy_from_slice(buf);
                            (0, 0, buf.len())
                        }
                    },
                }
            };
            if invalid > 0 {
                self.inner.consume(invalid);
                self.replace();
            } else if split > 0 {
                self.inner.consume(split);
                self.carry_len = split;
            } else {
                self.validated = valid;
                let buf = self.inner.fill_buf()?;
                let valid = valid.min(buf.len());
                return within_limit(&buf[..valid], self.position, self.limit);
            }
        }
        within_limit(
            &self.pending[self.pending_start..self.pending_end],
            self.position,
            self.limit,
        )
    }

    fn consume(&mut self, amount: usize) {
        if self.pending_start < self.pending_end {
            let amount = amount.min(self.pending_end - self.pending_start);
            self.pending_start += amount;
            self.position += amount as u64;
        } else {
            let amount = amount.min(self.validated);
            self.validated -= amount;
            self.inner.consume(amount);
            self.position += amount as u64;
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
            assert_eq!(lossy_utf8(case).as_bytes(), expected.as_slice());
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

    #[test]
    fn repaired_output_past_the_limit_is_refused() {
        let mut reader = LossyUtf8::with_limit(&b"ab\xff\xff"[..], 4);
        let mut out = Vec::new();
        let error = reader
            .read_to_end(&mut out)
            .expect_err("8 bytes once repaired");
        assert_eq!(error.kind(), io::ErrorKind::InvalidData);
        let mut reader = LossyUtf8::with_limit(&b"abcd"[..], 4);
        let mut out = Vec::new();
        reader
            .read_to_end(&mut out)
            .expect("valid input at the limit");
        assert_eq!(out, b"abcd");
    }

    #[test]
    fn replacements_are_told_apart_from_a_real_replacement_character() {
        // Text holding a real U+FFFD, then a replacement: the reader has
        // consumed the whole text, so it ends where the stream does.
        let input = b"\xef\xbf\xbd \xff";
        let mut reader = LossyUtf8::new(&input[..]);
        let mut out = Vec::new();
        reader.read_to_end(&mut out).expect("read from a slice");
        assert_eq!(out, "\u{FFFD} \u{FFFD}".as_bytes());
        assert!(!reader.repaired_in(&out, 0..=0, &out[..3]));
        assert!(reader.repaired_in(&out, 0..=0, &out[4..]));
        assert!(reader.repaired_in(&out, 0..=0, &out));
        // An empty tag: its content ends two bytes before the stream does.
        let input = b"<a x=\"\xef\xbf\xbd\" y=\"\xff\"/>";
        let mut reader = LossyUtf8::new(&input[..]);
        let mut out = Vec::new();
        reader.read_to_end(&mut out).expect("read from a slice");
        let tag = &out[1..out.len() - 2];
        let x = &tag[5..8];
        let y = &tag[13..16];
        assert_eq!(x, "\u{FFFD}".as_bytes());
        assert_eq!(y, "\u{FFFD}".as_bytes());
        assert!(!reader.repaired_in(tag, 1..=2, x));
        assert!(reader.repaired_in(tag, 1..=2, y));
    }
}
