//! HTML tree construction that stops at a nesting limit.
//!
//! The tree builder walks its stack of open elements for every block-level
//! start tag, so its cost grows with the square of the nesting depth. It has
//! no depth limit and cannot be told to stop, but it can be fed the input in
//! pieces. [`parse_document_within`] wraps the tree sink, keeps the element
//! depth of every node as it is attached, and stops feeding input once an
//! element is deeper than the limit plus a small allowance.
//!
//! The allowance exists because the tree builder sometimes attaches a node
//! and then moves it (formatting elements, content fostered out of tables),
//! so a node can be a few levels deeper for a moment than it ends up. The
//! sink therefore only bounds the work; whether a finished document is
//! within the limit is decided by the caller's exact check of the final
//! tree, which counts element ancestors only.
//!
//! The sink also counts the elements the tree builder creates. Open
//! formatting elements are cloned into every following block, so a small
//! input can ask for millions of elements; the parse stops when that
//! happens.

use std::borrow::Cow;
use std::cell::{Cell, RefCell};
use std::hash::{Hash, Hasher};

use html5ever::tendril::{StrTendril, TendrilSink};
use html5ever::tree_builder::{ElementFlags, NodeOrText, QuirksMode, TreeSink};
use html5ever::{Attribute, QualName};
use scraper::{Html, HtmlTreeSink};

/// Bytes of input handed to the tree builder between checks of the limit.
///
/// One piece can open at most a third as many elements as it has bytes, so
/// the tree is never built more than about 1,400 levels past the limit.
const PIECE_BYTES: usize = 4096;

/// Levels past the limit an element may sit while the tree is being built.
const TRANSIENT_DEPTH_ALLOWANCE: usize = 16;

type Handle = <HtmlTreeSink as TreeSink>::Handle;

/// Why tree construction was stopped early.
pub(crate) enum Stopped {
    /// An element was nested deeper than the limit.
    TooDeep,
    /// The tree builder created more elements than allowed.
    TooManyElements,
}

/// Element depth of every attached node, four bytes per node of the tree.
///
/// The tree stores its nodes in one vector and a handle is a position in
/// it, but exposes that number only through `Hash`. It is read through a
/// hasher that keeps the single integer it is given, and used as an index.
/// A hash map from handle to depth, tried first, roughly doubled the time
/// and added a third to the memory of parsing a 28 MB document.
#[derive(Default)]
struct Depths(Vec<u32>);

/// Keeps the one integer written to it.
#[derive(Default)]
struct Position(u64);

impl Hasher for Position {
    fn finish(&self) -> u64 {
        self.0
    }

    fn write(&mut self, bytes: &[u8]) {
        for byte in bytes {
            self.0 = (self.0 << 8) | u64::from(*byte);
        }
    }

    fn write_usize(&mut self, value: usize) {
        self.0 = value as u64;
    }
}

impl Depths {
    fn position(handle: Handle) -> usize {
        let mut position = Position::default();
        handle.hash(&mut position);
        position.finish() as usize
    }

    fn get(&self, handle: Handle) -> u32 {
        self.0.get(Self::position(handle)).copied().unwrap_or(0)
    }

    fn set(&mut self, handle: Handle, depth: u32) {
        let position = Self::position(handle);
        // Positions are small and dense for any real tree; a handle that
        // hashes to something else is not tracked (its depth reads as 0,
        // and the finished tree is still checked exactly).
        if position > (1 << 32) {
            return;
        }
        if position >= self.0.len() {
            self.0.resize(position + 1, 0);
        }
        self.0[position] = depth;
    }
}

/// A tree sink that records when the tree grows too deep or too large.
struct DepthLimitedSink {
    inner: HtmlTreeSink,
    depth_limit: usize,
    element_limit: usize,
    elements: Cell<usize>,
    /// Number of elements from the document down to and including each
    /// attached node, so attaching a node costs one lookup.
    depths: RefCell<Depths>,
    stopped: Cell<Option<Stopped>>,
}

impl DepthLimitedSink {
    /// Records the depth of a node that was just attached, and of anything
    /// already below it (a moved subtree).
    fn attached(&self, child: Option<Handle>) {
        let Some(child) = child else {
            return;
        };
        let html = self.inner.0.borrow();
        let Some(node) = html.tree.get(child) else {
            return;
        };
        let mut depths = self.depths.borrow_mut();
        let above = node.parent().map_or(0, |parent| depths.get(parent.id()));
        let mut deepest = above + u32::from(node.value().is_element());
        depths.set(child, deepest);
        if node.has_children() {
            // A moved subtree: everything below has a new depth.
            let mut pending: Vec<_> = node.children().map(|below| (below, deepest)).collect();
            while let Some((node, above)) = pending.pop() {
                let depth = above + u32::from(node.value().is_element());
                depths.set(node.id(), depth);
                deepest = deepest.max(depth);
                pending.extend(node.children().map(|below| (below, depth)));
            }
        }
        let deepest = deepest as usize;
        if deepest > self.depth_limit + TRANSIENT_DEPTH_ALLOWANCE {
            self.stopped.set(Some(Stopped::TooDeep));
        }
    }
}

fn appended(child: &NodeOrText<Handle>) -> Option<Handle> {
    match child {
        NodeOrText::AppendNode(handle) => Some(*handle),
        NodeOrText::AppendText(_) => None,
    }
}

impl TreeSink for DepthLimitedSink {
    type Handle = Handle;
    type Output = Html;
    type ElemName<'a> = <HtmlTreeSink as TreeSink>::ElemName<'a>;

    fn finish(self) -> Html {
        self.inner.finish()
    }

    fn parse_error(&self, msg: Cow<'static, str>) {
        self.inner.parse_error(msg);
    }

    fn get_document(&self) -> Handle {
        self.inner.get_document()
    }

    fn elem_name<'a>(&'a self, target: &'a Handle) -> Self::ElemName<'a> {
        self.inner.elem_name(target)
    }

    fn create_element(&self, name: QualName, attrs: Vec<Attribute>, flags: ElementFlags) -> Handle {
        let created = self.elements.get() + 1;
        self.elements.set(created);
        if created > self.element_limit {
            self.stopped.set(Some(Stopped::TooManyElements));
        }
        self.inner.create_element(name, attrs, flags)
    }

    fn create_comment(&self, text: StrTendril) -> Handle {
        self.inner.create_comment(text)
    }

    fn create_pi(&self, target: StrTendril, data: StrTendril) -> Handle {
        self.inner.create_pi(target, data)
    }

    fn append(&self, parent: &Handle, child: NodeOrText<Handle>) {
        let node = appended(&child);
        self.inner.append(parent, child);
        self.attached(node);
    }

    fn append_based_on_parent_node(
        &self,
        element: &Handle,
        prev_element: &Handle,
        child: NodeOrText<Handle>,
    ) {
        let node = appended(&child);
        self.inner
            .append_based_on_parent_node(element, prev_element, child);
        self.attached(node);
    }

    fn append_doctype_to_document(
        &self,
        name: StrTendril,
        public_id: StrTendril,
        system_id: StrTendril,
    ) {
        self.inner
            .append_doctype_to_document(name, public_id, system_id);
    }

    fn mark_script_already_started(&self, node: &Handle) {
        self.inner.mark_script_already_started(node);
    }

    fn pop(&self, node: &Handle) {
        self.inner.pop(node);
    }

    fn get_template_contents(&self, target: &Handle) -> Handle {
        self.inner.get_template_contents(target)
    }

    fn same_node(&self, x: &Handle, y: &Handle) -> bool {
        self.inner.same_node(x, y)
    }

    fn set_quirks_mode(&self, mode: QuirksMode) {
        self.inner.set_quirks_mode(mode);
    }

    fn append_before_sibling(&self, sibling: &Handle, new_node: NodeOrText<Handle>) {
        let node = appended(&new_node);
        self.inner.append_before_sibling(sibling, new_node);
        self.attached(node);
    }

    fn add_attrs_if_missing(&self, target: &Handle, attrs: Vec<Attribute>) {
        self.inner.add_attrs_if_missing(target, attrs);
    }

    fn associate_with_form(
        &self,
        target: &Handle,
        form: &Handle,
        nodes: (&Handle, Option<&Handle>),
    ) {
        self.inner.associate_with_form(target, form, nodes);
    }

    fn remove_from_parent(&self, target: &Handle) {
        self.inner.remove_from_parent(target);
    }

    fn reparent_children(&self, node: &Handle, new_parent: &Handle) {
        self.inner.reparent_children(node, new_parent);
        // The moved children, and everything below them, have new depths.
        let moved: Vec<Handle> = {
            let html = self.inner.0.borrow();
            html.tree
                .get(*new_parent)
                .map(|parent| parent.children().map(|child| child.id()).collect())
                .unwrap_or_default()
        };
        for child in moved {
            self.attached(Some(child));
        }
    }

    fn is_mathml_annotation_xml_integration_point(&self, handle: &Handle) -> bool {
        self.inner
            .is_mathml_annotation_xml_integration_point(handle)
    }

    fn set_current_line(&self, line_number: u64) {
        self.inner.set_current_line(line_number);
    }

    fn allow_declarative_shadow_roots(&self, intended_parent: &Handle) -> bool {
        self.inner.allow_declarative_shadow_roots(intended_parent)
    }

    fn attach_declarative_shadow(
        &self,
        location: &Handle,
        template: &Handle,
        attrs: &[Attribute],
    ) -> bool {
        self.inner
            .attach_declarative_shadow(location, template, attrs)
    }

    fn maybe_clone_an_option_into_selectedcontent(&self, option: &Handle) {
        self.inner
            .maybe_clone_an_option_into_selectedcontent(option);
    }
}

/// Parses `html` as a document, stopping early when the tree grows past
/// `depth_limit` (plus the transient allowance) or `element_limit`.
///
/// The result is the tree `scraper::Html::parse_document` would build. The
/// caller still checks the finished tree against the exact depth limit.
pub(crate) fn parse_document_within(
    html: &str,
    depth_limit: usize,
    element_limit: usize,
) -> Result<Html, Stopped> {
    let sink = DepthLimitedSink {
        inner: HtmlTreeSink::new(Html::new_document()),
        depth_limit,
        element_limit,
        elements: Cell::new(0),
        depths: RefCell::new(Depths::default()),
        stopped: Cell::new(None),
    };
    let mut parser = html5ever::parse_document(sink, html5ever::ParseOpts::default());
    let mut rest = html;
    while !rest.is_empty() {
        let mut end = rest.len().min(PIECE_BYTES);
        while !rest.is_char_boundary(end) {
            end += 1;
        }
        let (piece, tail) = rest.split_at(end);
        parser.process(StrTendril::from(piece));
        if let Some(stopped) = parser.tokenizer.sink.sink.stopped.take() {
            return Err(stopped);
        }
        rest = tail;
    }
    let stopped = parser.tokenizer.sink.sink.stopped.take();
    let document = parser.finish();
    stopped.map_or(Ok(document), Err)
}

#[cfg(test)]
mod tests {
    use super::parse_document_within;
    use scraper::Html;

    /// Feeding the input in pieces must build the same tree as one call.
    #[test]
    fn piecewise_parsing_builds_the_same_tree() {
        let page = "<!doctype html><title>t &amp; é</title><p>α<b>β<table><tr><td>c<p>d</table>\
                    <svg><path d=\"M0 0\"/></svg><script>if (a < b) {}</script><!-- c -->";
        let long = page.repeat(200);
        for html in [page, long.as_str(), "", "plain text"] {
            let whole = Html::parse_document(html);
            let Ok(pieces) = parse_document_within(html, 512, usize::MAX) else {
                panic!("within the limits");
            };
            assert_eq!(pieces.html(), whole.html());
        }
    }

    #[test]
    fn stops_past_the_limit_and_its_allowance() {
        let html = "<div>".repeat(100);
        assert!(parse_document_within(&html, 102, usize::MAX).is_ok());
        // html, body and 100 divs: 102 deep, 16 past a limit of 86.
        assert!(parse_document_within(&html, 86, usize::MAX).is_ok());
        assert!(parse_document_within(&html, 85, usize::MAX).is_err());
        assert!(parse_document_within(&html, 102, 50).is_err());
    }
}
