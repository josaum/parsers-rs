#![no_main]

use libfuzzer_sys::fuzz_target;
use ooxml_archive::{
    normalize_part, parse_relationships, parse_root_rels, part_dir, relationship_matches,
    relationship_target, resolve_part, REL_OFFICE_DOCUMENT,
};

// Any bytes as a relationships part (`.rels`); every target and type that
// parses goes through the path helpers the format readers call.
fuzz_target!(|data: &[u8]| {
    let _ = parse_root_rels(data);
    if let Ok(rels) = parse_relationships(data) {
        let _ = relationship_target(&rels, REL_OFFICE_DOCUMENT);
        for entry in rels.entries.iter().take(16) {
            let _ = normalize_part(&entry.target);
            let _ = part_dir(&entry.target);
            let _ = resolve_part("word/document.xml", &entry.target);
            let _ = relationship_matches(REL_OFFICE_DOCUMENT, &entry.rel_type);
        }
    }
});
