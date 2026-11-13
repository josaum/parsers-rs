//! Deterministic malformed-input smoke harness shared by the parser crates.
//!
//! A parser that reads untrusted files must return an error on bad input; it
//! must not panic, overflow the stack, or allocate without bound. This module
//! mutates valid seed inputs with a fixed-seed generator and feeds them to a
//! parse closure, so each crate can assert that property in its own tests.
//!
//! It is included by path rather than built as a crate, so it adds no
//! workspace member and no dependency:
//!
//! ```ignore
//! #[path = "../../tools/mutation_smoke.rs"]
//! mod smoke;
//!
//! #[global_allocator]
//! static ALLOCATOR: smoke::CappedAllocator = smoke::CappedAllocator;
//!
//! #[test]
//! fn malformed_inputs_never_panic() {
//!     smoke::assert_no_panics("my-format", &[seed()], 20_000, |bytes| {
//!         let _ = my_crate::parse(bytes);
//!     });
//! }
//! ```
//!
//! Everything is deterministic: a fixed seed and a fixed iteration count, never
//! wall-clock time, so a failure reproduces on any machine.

// Each including test binary uses a different subset of this module.
#![allow(dead_code)]

use std::alloc::{GlobalAlloc, Layout, System};
use std::panic::{catch_unwind, AssertUnwindSafe};
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::Mutex;

/// Most memory one test binary may hold before it is aborted.
///
/// Far above anything a parser needs for the kilobyte-sized inputs used here,
/// and far below what would exhaust a CI runner. A parser that sizes an
/// allocation from an attacker-controlled field, or loops without consuming
/// input, trips this and aborts the test binary with a message instead of
/// taking the machine down with it.
pub const ALLOCATION_CAP_BYTES: usize = 1 << 30;

static LIVE_BYTES: AtomicUsize = AtomicUsize::new(0);

/// Set once the cap has tripped and its message is being written.
///
/// Writing to standard error can itself allocate (on macOS the first write
/// allocates the stream's lock). Without this flag that allocation would trip
/// the cap again from inside the report, and the process would die of a stack
/// overflow with no message instead of naming the cap.
static REPORTING_CAP: AtomicBool = AtomicBool::new(false);

/// A global allocator that aborts the process once live memory passes
/// [`ALLOCATION_CAP_BYTES`].
///
/// Install it in a test binary with `#[global_allocator]`.
pub struct CappedAllocator;

impl CappedAllocator {
    fn charge(size: usize) {
        let live = LIVE_BYTES
            .fetch_add(size, Ordering::Relaxed)
            .saturating_add(size);
        // The first thread to pass the cap reports and aborts. An allocation
        // made while it does so, by the report itself or by another thread, is
        // let through: the process is about to end.
        if live > ALLOCATION_CAP_BYTES && !REPORTING_CAP.swap(true, Ordering::SeqCst) {
            // `abort` does not allocate, so this is safe to call from inside
            // the allocator; a panic here would try to allocate its message.
            eprintln!(
                "mutation_smoke: allocation cap exceeded ({size} byte request, {live} bytes live); \
                 a parser allocated without bound on malformed input"
            );
            std::process::abort();
        }
    }
}

// SAFETY: every method forwards to `System` with the caller's unchanged
// pointer and layout; the only added behavior is byte accounting and an abort.
unsafe impl GlobalAlloc for CappedAllocator {
    unsafe fn alloc(&self, layout: Layout) -> *mut u8 {
        Self::charge(layout.size());
        // SAFETY: forwarding the unchanged layout to the system allocator.
        unsafe { System.alloc(layout) }
    }

    unsafe fn alloc_zeroed(&self, layout: Layout) -> *mut u8 {
        Self::charge(layout.size());
        // SAFETY: forwarding the unchanged layout to the system allocator.
        unsafe { System.alloc_zeroed(layout) }
    }

    unsafe fn dealloc(&self, pointer: *mut u8, layout: Layout) {
        LIVE_BYTES.fetch_sub(layout.size(), Ordering::Relaxed);
        // SAFETY: `pointer` came from this allocator, which delegates to the
        // system allocator with the same layout.
        unsafe { System.dealloc(pointer, layout) }
    }

    unsafe fn realloc(&self, pointer: *mut u8, layout: Layout, new_size: usize) -> *mut u8 {
        if new_size > layout.size() {
            Self::charge(new_size - layout.size());
        } else {
            LIVE_BYTES.fetch_sub(layout.size() - new_size, Ordering::Relaxed);
        }
        // SAFETY: forwarding the original pointer and layout, and the requested
        // size, to the system allocator that created the allocation.
        unsafe { System.realloc(pointer, layout, new_size) }
    }
}

/// A small deterministic generator (xorshift64); no external crate, no clock.
pub struct Rng(u64);

impl Rng {
    /// Creates a generator from a non-zero seed.
    pub fn new(seed: u64) -> Self {
        Self(seed.max(1))
    }

    /// Returns the next 64 pseudo-random bits.
    pub fn next_u64(&mut self) -> u64 {
        self.0 ^= self.0 << 13;
        self.0 ^= self.0 >> 7;
        self.0 ^= self.0 << 17;
        self.0
    }

    /// Returns a value in `0..bound`, or 0 when `bound` is 0.
    pub fn below(&mut self, bound: usize) -> usize {
        if bound == 0 {
            0
        } else {
            (self.next_u64() % bound as u64) as usize
        }
    }
}

/// Byte strings that tend to reach untested branches of text and binary
/// parsers: delimiters, numeric extremes, and format keywords.
const TOKENS: &[&[u8]] = &[
    b"<<",
    b">>",
    b"[",
    b"]",
    b"(",
    b")",
    b"{",
    b"}",
    b"<",
    b">",
    b"/",
    b"\\",
    b"&",
    b"&#",
    b";",
    b"\"",
    b"'",
    b"=",
    b":",
    b"@",
    b"%",
    b",",
    b"\x00",
    b"\xff\xff\xff\xff",
    b"\r\n",
    b"--",
    b"-1",
    b"0",
    b"4294967295",
    b"4294967296",
    b"99999999999999999999",
    b"1e999",
    b"1e-999",
    b"<![CDATA[",
    b"]]>",
    b"<!--",
    b"-->",
    b"<?",
    b"?>",
    b"=?",
    b"?=",
    b"stream",
    b"endstream",
    b"obj",
    b"endobj",
    b" 0 R",
    b"BT",
    b"ET",
    b"Tj",
    b"TJ",
    b"BI",
    b"ID",
    b"EI",
    b"/Length 999999999",
    b"/Count -5",
    b"/Kids",
    b"/Parent",
    b"/Columns 4294967295",
    b"/Predictor 15",
    b"/Filter /FlateDecode",
    b"/Filter /LZWDecode",
];

/// Returns `seed` with one to four random edits applied.
///
/// Edits are byte replacement, bit flips, truncation, token insertion, range
/// deletion, range duplication, digit inflation and random overwrites.
pub fn mutate(seed: &[u8], rng: &mut Rng) -> Vec<u8> {
    let mut data = seed.to_vec();
    for _ in 0..1 + rng.below(4) {
        if data.is_empty() {
            data.push(0);
        }
        match rng.below(8) {
            0 => {
                let at = rng.below(data.len());
                data[at] = rng.next_u64() as u8;
            }
            1 => {
                let at = rng.below(data.len());
                data[at] ^= 1 << rng.below(8);
            }
            2 => {
                let at = rng.below(data.len());
                data.truncate(at);
            }
            3 => {
                let at = rng.below(data.len() + 1);
                let token = TOKENS[rng.below(TOKENS.len())];
                data.splice(at..at, token.iter().copied());
            }
            4 => {
                let at = rng.below(data.len());
                let len = rng.below(64.min(data.len() - at) + 1);
                data.drain(at..at + len);
            }
            5 => {
                let at = rng.below(data.len());
                let len = rng.below(128.min(data.len() - at) + 1);
                let chunk = data[at..at + len].to_vec();
                let to = rng.below(data.len() + 1);
                data.splice(to..to, chunk);
            }
            6 => {
                let at = rng.below(data.len());
                if data[at].is_ascii_digit() {
                    data.splice(at..at, b"99999999".iter().copied());
                } else {
                    data[at] = b"0-9 /<>([\n"[rng.below(10)];
                }
            }
            _ => {
                let at = rng.below(data.len());
                let len = rng.below(16.min(data.len() - at) + 1);
                for byte in &mut data[at..at + len] {
                    *byte = rng.next_u64() as u8;
                }
            }
        }
    }
    data
}

/// Returns `depth` copies of `open`, then `body`, then `depth` copies of `close`.
///
/// Random mutation almost never produces deep nesting, so recursion limits
/// need an explicit input.
pub fn nested(open: &[u8], body: &[u8], close: &[u8], depth: usize) -> Vec<u8> {
    let mut out = Vec::with_capacity(depth * (open.len() + close.len()) + body.len());
    for _ in 0..depth {
        out.extend_from_slice(open);
    }
    out.extend_from_slice(body);
    for _ in 0..depth {
        out.extend_from_slice(close);
    }
    out
}

/// Stack size of the thread [`run_on_small_stack`] uses.
///
/// Matches the smallest stack a consumer realistically runs a parser on (a
/// worker thread with a reduced stack), well below the 8 MiB main thread, so a
/// recursion limit that only holds on the main thread is caught.
pub const SMALL_STACK_BYTES: usize = 256 * 1024;

/// Runs `parse` on a thread with a [`SMALL_STACK_BYTES`] stack.
///
/// A parser whose recursion is bounded returns normally. One whose recursion
/// follows input depth overflows the stack, which aborts the test binary.
///
/// # Panics
///
/// Panics if the thread cannot be spawned or `parse` panics.
pub fn run_on_small_stack<F>(parse: F)
where
    F: FnOnce() + Send + 'static,
{
    std::thread::Builder::new()
        .stack_size(SMALL_STACK_BYTES)
        .spawn(parse)
        .expect("spawn small-stack thread")
        .join()
        .expect("parser panicked on a small stack");
}

/// Feeds `iterations` mutated seeds to `parse` and returns every panic.
///
/// Each entry is the mutated input and the panic's `file:line`. Inputs are
/// chosen and mutated by a generator seeded from `name`, so a given name,
/// seed set and iteration count always produce the same inputs.
pub fn collect_panics<F>(
    name: &str,
    seeds: &[Vec<u8>],
    iterations: usize,
    parse: F,
) -> Vec<(Vec<u8>, String)>
where
    F: Fn(&[u8]),
{
    static LAST_PANIC_LOCATION: Mutex<Option<String>> = Mutex::new(None);

    assert!(!seeds.is_empty(), "mutation smoke needs at least one seed");
    let mut seed_bits = 0x9E37_79B9_7F4A_7C15u64;
    for byte in name.bytes() {
        seed_bits = seed_bits.rotate_left(5) ^ u64::from(byte);
    }
    let mut rng = Rng::new(seed_bits);

    let previous_hook = std::panic::take_hook();
    std::panic::set_hook(Box::new(|info| {
        let location = info
            .location()
            .map(|location| format!("{}:{}", location.file(), location.line()))
            .unwrap_or_else(|| "unknown location".to_string());
        if let Ok(mut slot) = LAST_PANIC_LOCATION.lock() {
            *slot = Some(location);
        }
    }));

    let mut panics = Vec::new();
    for _ in 0..iterations {
        let input = mutate(&seeds[rng.below(seeds.len())], &mut rng);
        if catch_unwind(AssertUnwindSafe(|| parse(&input))).is_err() {
            let location = LAST_PANIC_LOCATION
                .lock()
                .ok()
                .and_then(|mut slot| slot.take())
                .unwrap_or_else(|| "unknown location".to_string());
            panics.push((input, location));
        }
    }

    std::panic::set_hook(previous_hook);
    panics
}

/// Asserts that no mutated input makes `parse` panic.
///
/// # Panics
///
/// Panics with each distinct panic location, its count, and one reproducing
/// input (as escaped bytes) when any mutated input panics.
pub fn assert_no_panics<F>(name: &str, seeds: &[Vec<u8>], iterations: usize, parse: F)
where
    F: Fn(&[u8]),
{
    let panics = collect_panics(name, seeds, iterations, parse);
    if panics.is_empty() {
        return;
    }
    let mut by_location: std::collections::BTreeMap<&str, (usize, &[u8])> =
        std::collections::BTreeMap::new();
    for (input, location) in &panics {
        let entry = by_location.entry(location).or_insert((0, input));
        entry.0 += 1;
        if input.len() < entry.1.len() {
            entry.1 = input;
        }
    }
    let mut report = format!(
        "{name}: {} of {iterations} malformed inputs panicked at {} location(s)\n",
        panics.len(),
        by_location.len()
    );
    for (location, (count, input)) in &by_location {
        let shown: String = input
            .iter()
            .take(400)
            .flat_map(|byte| std::ascii::escape_default(*byte))
            .map(char::from)
            .collect();
        report.push_str(&format!(
            "  {count}x {location}\n    smallest input ({} bytes): b\"{shown}\"\n",
            input.len()
        ));
    }
    panic!("{report}");
}

/// Set to anything but `0` or the empty string to make [`fuzz_seeds`] write
/// the seed files instead of checking them.
pub const WRITE_FUZZ_SEEDS: &str = "WRITE_FUZZ_SEEDS";

/// Keeps the starting corpus of a crate's fuzz targets identical to the seeds
/// its smoke tests use.
///
/// Each entry is `(target, file name, bytes)` and lives at
/// `<crate_dir>/fuzz/seeds/<target>/<file name>`. By default every file must
/// hold exactly those bytes, and a seed directory may hold no other file, so
/// a seed builder cannot change while the corpus stays behind. With
/// [`WRITE_FUZZ_SEEDS`] set, the files are written and the others removed.
///
/// # Panics
///
/// Panics if a seed is empty, missing, different from its file, or cannot be
/// written, or if a seed directory holds a file the list does not name.
pub fn fuzz_seeds(crate_dir: &str, seeds: &[(&str, &str, Vec<u8>)]) {
    let write =
        std::env::var(WRITE_FUZZ_SEEDS).is_ok_and(|value| !value.is_empty() && value != "0");
    let root = std::path::Path::new(crate_dir).join("fuzz/seeds");
    let mut named: std::collections::BTreeMap<&str, Vec<&str>> = std::collections::BTreeMap::new();
    for (target, name, bytes) in seeds {
        assert!(!bytes.is_empty(), "the seed {target}/{name} is empty");
        named.entry(target).or_default().push(name);
        let dir = root.join(target);
        let path = dir.join(name);
        if write {
            std::fs::create_dir_all(&dir).expect("create the seed directory");
            std::fs::write(&path, bytes).expect("write the seed");
        }
        let stored = std::fs::read(&path).unwrap_or_default();
        assert!(
            stored == *bytes,
            "{} is missing or differs from the seed the smoke test builds; \
             run this test with {WRITE_FUZZ_SEEDS}=1 and commit the result",
            path.display()
        );
    }
    for (target, names) in &named {
        let entries = std::fs::read_dir(root.join(target)).expect("read the seed directory");
        for entry in entries {
            let entry = entry.expect("read a seed directory entry");
            let file = entry.file_name();
            if names.iter().any(|name| file == std::ffi::OsStr::new(name)) {
                continue;
            }
            if write {
                std::fs::remove_file(entry.path()).expect("remove a stale seed");
            } else {
                panic!(
                    "{} is not a seed the smoke test builds any more; \
                     run this test with {WRITE_FUZZ_SEEDS}=1 and commit the result",
                    entry.path().display()
                );
            }
        }
    }
}
