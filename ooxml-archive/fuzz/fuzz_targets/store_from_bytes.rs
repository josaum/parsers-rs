#![no_main]

use libfuzzer_sys::fuzz_target;
use ooxml_archive::{rels_path_for_part, source_part_from_rels, OoxmlArchive};

/// Keep inflation bounded so malformed size hints cannot exhaust memory.
const MAX_INFLATE: usize = 1 << 20; // 1 MiB
/// Bound per-input work: probe at most this many central-directory entries.
const MAX_PARTS: usize = 8;

fuzz_target!(|data: &[u8]| {
    let Ok(store) = OoxmlArchive::from_bytes(data.to_vec()) else {
        return;
    };
    let names: Vec<String> = store
        .part_names()
        .take(MAX_PARTS)
        .map(str::to_string)
        .collect();
    for name in &names {
        let _ = store.has_part(name);
        let _ = store.read_part(name, MAX_INFLATE);
        let rels = rels_path_for_part(name);
        let _ = source_part_from_rels(&rels);
    }
});
