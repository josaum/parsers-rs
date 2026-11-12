//! STR-packed R-tree (STRtree) for axis-aligned bbox queries.
//!
//! Packs leaf MBRs with the Sort-Tile-Recursive heuristic (Leutenegger et al.),
//! then builds a balanced hierarchy. Queries are inclusive-edge AABB tests in
//! lon-lat space — call [`crate::types::BBox::split_antimeridian`] on the query
//! (and on indexed boxes) when extents may wrap the dateline.
//!
//! This is a pure-Rust index with no external GIS dependency. It indexes
//! bounding boxes only; exact geometry predicates remain the caller's job.

use crate::types::BBox;

/// Default maximum children per node (STR packing leaf/fanout).
const DEFAULT_NODE_CAPACITY: usize = 16;

#[derive(Debug, Clone)]
struct Entry {
    bbox: BBox,
    /// Payload index into the caller's parallel array, or child node id.
    id: usize,
}

#[derive(Debug, Clone)]
struct Node {
    bbox: BBox,
    /// `true` when `children` are leaf payload ids; `false` when they are node ids.
    is_leaf: bool,
    children: Vec<Entry>,
}

/// Spatial index over axis-aligned bounding boxes.
///
/// Build once with [`SpatialIndex::bulk_load`], then query with
/// [`SpatialIndex::query`] / [`SpatialIndex::query_into`]. Returned values are
/// the `id` integers supplied at load time (typically feature indices).
#[derive(Debug, Clone)]
pub struct SpatialIndex {
    nodes: Vec<Node>,
    root: Option<usize>,
    len: usize,
    capacity: usize,
}

impl SpatialIndex {
    /// Build an empty index (queries always return nothing).
    pub fn empty() -> Self {
        Self {
            nodes: Vec::new(),
            root: None,
            len: 0,
            capacity: DEFAULT_NODE_CAPACITY,
        }
    }

    /// Bulk-load `(bbox, id)` pairs with STR packing.
    ///
    /// Non-finite or empty-seed boxes are skipped. Duplicate `id` values are
    /// allowed (caller-defined). Antimeridian-crossing boxes (`min_lon > max_lon`)
    /// are indexed as-is; prefer splitting them before insert so queries that
    /// do not wrap still hit the correct halves.
    pub fn bulk_load(items: impl IntoIterator<Item = (BBox, usize)>) -> Self {
        Self::bulk_load_with_capacity(items, DEFAULT_NODE_CAPACITY)
    }

    /// Bulk-load with an explicit node capacity (`>= 2`).
    pub fn bulk_load_with_capacity(
        items: impl IntoIterator<Item = (BBox, usize)>,
        capacity: usize,
    ) -> Self {
        let capacity = capacity.max(2);
        let mut leaves: Vec<Entry> = items
            .into_iter()
            .filter(|(b, _)| b.is_finite() && b.is_ordered())
            .map(|(bbox, id)| Entry { bbox, id })
            .collect();

        let len = leaves.len();
        if leaves.is_empty() {
            return Self {
                nodes: Vec::new(),
                root: None,
                len: 0,
                capacity,
            };
        }

        let mut nodes: Vec<Node> = Vec::new();
        let mut level = pack_str_level(&mut leaves, capacity, true, &mut nodes);

        while level.len() > 1 {
            level = pack_str_level(&mut level, capacity, false, &mut nodes);
        }

        let root = level.into_iter().next().map(|e| e.id);
        Self {
            nodes,
            root,
            len,
            capacity,
        }
    }

    /// Number of indexed items (after filtering invalid boxes).
    pub fn len(&self) -> usize {
        self.len
    }

    /// Returns true when no items were indexed.
    pub fn is_empty(&self) -> bool {
        self.len == 0
    }

    /// Node fanout used at build time.
    pub fn capacity(&self) -> usize {
        self.capacity
    }

    /// Collect all payload ids whose MBR intersects `query` (inclusive edges).
    pub fn query(&self, query: &BBox) -> Vec<usize> {
        let mut out = Vec::new();
        self.query_into(query, &mut out);
        out
    }

    /// Append matching payload ids into `out` (does not clear `out`).
    pub fn query_into(&self, query: &BBox, out: &mut Vec<usize>) {
        if !query.is_finite() {
            return;
        }
        let Some(root) = self.root else {
            return;
        };
        self.query_node(root, query, out);
    }

    /// Query that also matches antimeridian-split query boxes.
    ///
    /// Splits `query` with [`BBox::split_antimeridian`] and unions results
    /// (deduplicated, unsorted).
    pub fn query_antimeridian_aware(&self, query: &BBox) -> Vec<usize> {
        let parts = query.split_antimeridian();
        if parts.len() == 1 {
            return self.query(&parts[0]);
        }
        let mut out = Vec::new();
        for part in &parts {
            self.query_into(part, &mut out);
        }
        out.sort_unstable();
        out.dedup();
        out
    }

    fn query_node(&self, node_id: usize, query: &BBox, out: &mut Vec<usize>) {
        let Some(node) = self.nodes.get(node_id) else {
            return;
        };
        if !node.bbox.intersects(query) {
            return;
        }
        if node.is_leaf {
            for child in &node.children {
                if child.bbox.intersects(query) {
                    out.push(child.id);
                }
            }
        } else {
            for child in &node.children {
                if child.bbox.intersects(query) {
                    self.query_node(child.id, query, out);
                }
            }
        }
    }
}

fn pack_str_level(
    entries: &mut [Entry],
    capacity: usize,
    is_leaf: bool,
    nodes: &mut Vec<Node>,
) -> Vec<Entry> {
    if entries.is_empty() {
        return Vec::new();
    }

    // Sort by center longitude (X), then tile into vertical slices.
    entries.sort_by(|a, b| {
        center_lon(&a.bbox)
            .partial_cmp(&center_lon(&b.bbox))
            .unwrap_or(std::cmp::Ordering::Equal)
    });

    let n = entries.len();
    let leaf_count = n.div_ceil(capacity);
    let slice_count = (leaf_count as f64).sqrt().ceil() as usize;
    let slice_count = slice_count.max(1);
    let per_slice = n.div_ceil(slice_count);

    let mut parent_entries = Vec::with_capacity(leaf_count);
    let mut i = 0;
    while i < n {
        let slice_end = i.saturating_add(per_slice).min(n);
        // Within each vertical slice, sort by center latitude (Y).
        entries[i..slice_end].sort_by(|a, b| {
            center_lat(&a.bbox)
                .partial_cmp(&center_lat(&b.bbox))
                .unwrap_or(std::cmp::Ordering::Equal)
        });

        let mut j = i;
        while j < slice_end {
            // `capacity` is the caller's; it may be as large as `usize::MAX`.
            let node_end = j.saturating_add(capacity).min(slice_end);
            let children: Vec<Entry> = entries[j..node_end].to_vec();
            let bbox = union_entries(&children);
            let node_id = nodes.len();
            nodes.push(Node {
                bbox,
                is_leaf,
                children,
            });
            parent_entries.push(Entry { bbox, id: node_id });
            j = node_end;
        }
        i = slice_end;
    }

    parent_entries
}

fn center_lon(b: &BBox) -> f64 {
    (b.min_lon + b.max_lon) * 0.5
}

fn center_lat(b: &BBox) -> f64 {
    (b.min_lat + b.max_lat) * 0.5
}

fn union_entries(entries: &[Entry]) -> BBox {
    let mut bbox = BBox::empty();
    for e in entries {
        bbox.merge(&e.bbox);
    }
    bbox
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::types::Coordinate;

    #[test]
    fn empty_index_queries_nothing() {
        let idx = SpatialIndex::empty();
        assert!(idx.is_empty());
        assert!(idx.query(&BBox::new(0.0, 0.0, 1.0, 1.0)).is_empty());
    }

    #[test]
    fn bulk_load_finds_overlapping() {
        // WHY: STRtree must return only MBRs that intersect the query under
        // closed-edge semantics — the GIS observation filter path.
        let items = vec![
            (BBox::new(0.0, 0.0, 1.0, 1.0), 0),
            (BBox::new(2.0, 2.0, 3.0, 3.0), 1),
            (BBox::new(0.5, 0.5, 2.5, 2.5), 2),
            (BBox::new(10.0, 10.0, 11.0, 11.0), 3),
        ];
        let idx = SpatialIndex::bulk_load(items);
        assert_eq!(idx.len(), 4);

        let mut hits = idx.query(&BBox::new(0.0, 0.0, 1.0, 1.0));
        hits.sort_unstable();
        assert_eq!(hits, vec![0, 2]);

        // Touching edge still hits (inclusive).
        let mut edge = idx.query(&BBox::new(1.0, 0.0, 1.0, 1.0));
        edge.sort_unstable();
        assert_eq!(edge, vec![0, 2]);

        assert!(idx.query(&BBox::new(20.0, 20.0, 21.0, 21.0)).is_empty());
    }

    #[test]
    fn bulk_load_skips_nan() {
        let items = vec![
            (BBox::new(f64::NAN, 0.0, 1.0, 1.0), 0),
            (BBox::new(0.0, 0.0, 1.0, 1.0), 1),
        ];
        let idx = SpatialIndex::bulk_load(items);
        assert_eq!(idx.len(), 1);
        assert_eq!(idx.query(&BBox::new(0.0, 0.0, 1.0, 1.0)), vec![1]);
    }

    #[test]
    fn large_bulk_load_is_complete() {
        let mut items = Vec::new();
        for i in 0..200 {
            let x = (i % 20) as f64;
            let y = (i / 20) as f64;
            items.push((BBox::new(x, y, x + 0.5, y + 0.5), i));
        }
        let idx = SpatialIndex::bulk_load(items);
        assert_eq!(idx.len(), 200);
        let hits = idx.query(&BBox::new(0.0, 0.0, 0.6, 0.6));
        assert!(hits.contains(&0));
        assert!(!hits.contains(&199));
    }

    #[test]
    fn antimeridian_aware_query() {
        let west = BBox::new(170.0, -10.0, 180.0, 10.0);
        let east = BBox::new(-180.0, -10.0, -170.0, 10.0);
        let idx = SpatialIndex::bulk_load(vec![(west, 1), (east, 2)]);
        let wrap = BBox::new(175.0, -5.0, -175.0, 5.0); // crosses dateline
        assert!(wrap.crosses_antimeridian());
        let mut hits = idx.query_antimeridian_aware(&wrap);
        hits.sort_unstable();
        assert_eq!(hits, vec![1, 2]);
        // Point inside the wrap should also work via Coordinate helpers.
        assert!(west.contains_point(&Coordinate::new(176.0, 0.0)));
    }
}
