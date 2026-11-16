//! `LossyUtf8` gives what `String::from_utf8_lossy` gives, whatever the inner
//! reader's buffer boundaries, and through `Interrupted` errors.
use ooxml_archive::utf8::{lossy_utf8, LossyUtf8};
use std::io::{self, BufRead, BufReader, Read};

struct Rng(u64);
impl Rng {
    fn next(&mut self) -> u64 {
        // xorshift64*
        let mut x = self.0;
        x ^= x >> 12;
        x ^= x << 25;
        x ^= x >> 27;
        self.0 = x;
        x.wrapping_mul(0x2545_F491_4F6C_DD1D)
    }
    fn below(&mut self, n: usize) -> usize {
        (self.next() % n as u64) as usize
    }
}

fn gen_bytes(rng: &mut Rng) -> Vec<u8> {
    let tokens: &[&[u8]] = &[
        b"a",
        b"<",
        b">",
        b" ",
        b"\r",
        b"\n",
        "\u{e9}".as_bytes(),
        "\u{20ac}".as_bytes(),
        "\u{1F600}".as_bytes(),
        "\u{FFFD}".as_bytes(),
        "\u{10FFFF}".as_bytes(),
        "\u{D7FF}".as_bytes(),
        "\u{E000}".as_bytes(),
        "\u{80}".as_bytes(),
        "\u{7FF}".as_bytes(),
        "\u{800}".as_bytes(),
        "\u{10000}".as_bytes(),
        // truncated prefixes
        b"\xc3",
        b"\xe2",
        b"\xe2\x82",
        b"\xf0",
        b"\xf0\x9f",
        b"\xf0\x9f\x98",
        b"\xf4\x8f\xbf",
        // overlongs
        b"\xc0\x80",
        b"\xc1\xbf",
        b"\xe0\x80\x80",
        b"\xe0\x9f\xbf",
        b"\xf0\x80\x80\x80",
        b"\xf0\x8f\xbf\xbf",
        // surrogates
        b"\xed\xa0\x80",
        b"\xed\xbf\xbf",
        b"\xed\xa0",
        b"\xed\x9f\xbf",
        // above U+10FFFF
        b"\xf4\x90\x80\x80",
        b"\xf4\x90",
        // F5..FF and lone continuation
        b"\xf5",
        b"\xf8",
        b"\xfe",
        b"\xff",
        b"\x80",
        b"\xbf",
        b"\xa0",
    ];
    let len = rng.below(48);
    let mut out = Vec::new();
    for _ in 0..len {
        if rng.below(6) == 0 {
            out.push(rng.next() as u8);
        } else {
            out.extend_from_slice(tokens[rng.below(tokens.len())]);
        }
    }
    out
}

/// A BufRead whose buffer windows have random lengths; optionally the window
/// grows after a partial consume, and fill_buf may return Interrupted.
struct ChunkyReader<'a> {
    data: &'a [u8],
    pos: usize,
    end: usize,
    rng: Rng,
    max_chunk: usize,
    grow: bool,
    interrupt: bool,
}
impl<'a> ChunkyReader<'a> {
    fn new(data: &'a [u8], seed: u64, max_chunk: usize, grow: bool, interrupt: bool) -> Self {
        Self {
            data,
            pos: 0,
            end: 0,
            rng: Rng(seed | 1),
            max_chunk,
            grow,
            interrupt,
        }
    }
}
impl Read for ChunkyReader<'_> {
    fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
        let avail = self.fill_buf()?;
        let n = avail.len().min(buf.len());
        buf[..n].copy_from_slice(&avail[..n]);
        self.consume(n);
        Ok(n)
    }
}
impl BufRead for ChunkyReader<'_> {
    fn fill_buf(&mut self) -> io::Result<&[u8]> {
        if self.interrupt && self.rng.below(3) == 0 {
            return Err(io::Error::from(io::ErrorKind::Interrupted));
        }
        if self.pos == self.end || (self.grow && self.rng.below(2) == 0) {
            let chunk = 1 + self.rng.below(self.max_chunk);
            self.end = (self.end.max(self.pos) + chunk).min(self.data.len());
        }
        Ok(&self.data[self.pos..self.end])
    }
    fn consume(&mut self, amt: usize) {
        self.pos = (self.pos + amt).min(self.end);
    }
}

/// Reads with random outer buffer sizes 0..=64 (0 included to exercise the
/// zero-length read; it never counts as EOF here).
fn drain<R: Read>(mut r: R, rng: &mut Rng) -> io::Result<Vec<u8>> {
    let mut out = Vec::new();
    let mut zeros_in_a_row = 0;
    loop {
        let size = rng.below(65);
        let mut buf = vec![0u8; size];
        match r.read(&mut buf) {
            Ok(0) if size > 0 => {
                // EOF must repeat.
                let mut again = [0u8; 8];
                assert_eq!(r.read(&mut again)?, 0, "EOF did not repeat");
                return Ok(out);
            }
            Ok(0) => {
                zeros_in_a_row += 1;
                assert!(zeros_in_a_row < 10_000);
            }
            Ok(n) => {
                zeros_in_a_row = 0;
                out.extend_from_slice(&buf[..n]);
            }
            Err(e) if e.kind() == io::ErrorKind::Interrupted => continue,
            Err(e) => return Err(e),
        }
    }
}

#[test]
fn prop_lossy_matches_from_utf8_lossy() {
    let mut rng = Rng(0x9E37_79B9_7F4A_7C15);
    let mut checked = 0u64;
    let mut failures = Vec::new();
    for iter in 0..20_000u64 {
        let input = gen_bytes(&mut rng);
        let expected = String::from_utf8_lossy(&input).into_owned().into_bytes();
        let invalid = std::str::from_utf8(&input).is_err();
        assert_eq!(lossy_utf8(&input).as_bytes(), expected.as_slice());
        // 1. std BufReader with random capacity 1..=64, random outer read sizes.
        let cap = 1 + rng.below(64);
        let mut lossy = LossyUtf8::new(BufReader::with_capacity(cap, input.as_slice()));
        let out = drain(&mut lossy, &mut rng).unwrap();
        if out != expected || lossy.repaired() != invalid {
            failures.push(format!("BufReader cap={cap} input={input:02x?}"));
        }
        // 2. Chunky reader (random windows), with and without growth.
        for grow in [false, true] {
            let seed = rng.next();
            let max_chunk = 1 + rng.below(64);
            let mut lossy = LossyUtf8::new(ChunkyReader::new(&input, seed, max_chunk, grow, false));
            let out = drain(&mut lossy, &mut rng).unwrap();
            if out != expected || lossy.repaired() != invalid {
                failures.push(format!(
                    "Chunky grow={grow} max={max_chunk} seed={seed} input={input:02x?} got={:02x?}",
                    out
                ));
            }
        }
        checked += 3;
        let _ = iter;
    }
    eprintln!("PROP checked={checked} failures={}", failures.len());
    for f in failures.iter().take(5) {
        eprintln!("PROP FAIL {f}");
    }
    assert!(failures.is_empty());
}

/// Interrupted from the inner reader is retryable (read_to_end retries it):
/// the output must still equal from_utf8_lossy.
#[test]
fn prop_lossy_survives_interrupted() {
    let mut rng = Rng(0xDEAD_BEEF_CAFE_F00D);
    let mut failures = Vec::new();
    let mut checked = 0;
    for _ in 0..5_000 {
        let input = gen_bytes(&mut rng);
        let expected = String::from_utf8_lossy(&input).into_owned().into_bytes();
        let seed = rng.next();
        let max_chunk = 1 + rng.below(8);
        let mut lossy = LossyUtf8::new(ChunkyReader::new(&input, seed, max_chunk, false, true));
        let mut out = Vec::new();
        lossy.read_to_end(&mut out).unwrap();
        checked += 1;
        if out != expected {
            failures.push(format!("max={max_chunk} seed={seed} input={input:02x?}\n  got={out:02x?}\n  exp={expected:02x?}"));
        }
    }
    eprintln!("INTR checked={checked} failures={}", failures.len());
    for f in failures.iter().take(3) {
        eprintln!("INTR FAIL {f}");
    }
    assert!(failures.is_empty());
}

#[test]
fn split_four_byte_across_one_byte_buffers_minimal() {
    // Explicit hand cases.
    for case in [
        &b"\xf0\x9f\x98\x80"[..],
        b"\xf0\x9f\x98",
        b"\xe0\x80\x80",
        b"\xed\xa0\x80",
        b"\xf5\xff",
        b"\xf4\x90\x80\x80",
    ] {
        for cap in 1..=4 {
            let mut r = LossyUtf8::new(BufReader::with_capacity(cap, case));
            let mut out = Vec::new();
            r.read_to_end(&mut out).unwrap();
            assert_eq!(
                out,
                String::from_utf8_lossy(case).as_bytes(),
                "{case:02x?} cap {cap}"
            );
        }
    }
}

/// Yields "\xC3", then one Interrupted, then "\xA9" ("é" split in two).
struct SplitThenInterrupt {
    step: usize,
    pos: usize,
}
impl Read for SplitThenInterrupt {
    fn read(&mut self, _: &mut [u8]) -> io::Result<usize> {
        unreachable!()
    }
}
impl BufRead for SplitThenInterrupt {
    fn fill_buf(&mut self) -> io::Result<&[u8]> {
        const DATA: &[u8] = b"\xc3\xa9";
        if self.pos == 1 && self.step == 0 {
            self.step = 1;
            return Err(io::Error::from(io::ErrorKind::Interrupted));
        }
        Ok(&DATA[self.pos..(self.pos + 1).min(2)])
    }
    fn consume(&mut self, amt: usize) {
        self.pos += amt;
    }
}

#[test]
fn minimal_interrupted_counterexample() {
    let mut r = LossyUtf8::new(SplitThenInterrupt { step: 0, pos: 0 });
    let mut out = Vec::new();
    r.read_to_end(&mut out).unwrap();
    eprintln!(
        "MIN got={:02x?} expected={:02x?} repaired={}",
        out,
        "\u{e9}".as_bytes(),
        r.repaired()
    );
    assert_eq!(out, "\u{e9}".as_bytes());
}
