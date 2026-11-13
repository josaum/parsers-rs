//! HTML tree construction that stops at a nesting limit.
//!
//! The tree builder walks its stack of open elements for every block-level
//! start tag, so its cost grows with the square of the nesting depth. It has
//! no depth limit and cannot be told to stop, but it can be fed the input in
//! pieces. [`parse_document_within`] wraps the tree sink, measures the depth
//! of every element as it is attached, and stops feeding input as soon as an
//! element is deeper than the limit. The depth is that of the real tree, so
//! every parsing rule (implied end tags, raw text, foreign content, table
//! fixes) is accounted for exactly; no document within the limit is refused.

use std::borrow::Cow;
use std::cell::Cell;

use html5ever::tendril::{StrTendril, TendrilSink};
use html5ever::tree_builder::{ElementFlags, NodeOrText, QuirksMode, TreeSink};
use html5ever::{Attribute, QualName};
use scraper::{Html, HtmlTreeSink};

/// Bytes of input handed to the tree builder between checks of the limit.
///
/// One piece can open at most a third as many elements as it has bytes, so
/// the tree is never built more than about 1,400 levels past the limit.
const PIECE_BYTES: usize = 4096;

type Handle = <HtmlTreeSink as TreeSink>::Handle;

/// A tree sink that records when an element is attached too deep.
struct DepthLimitedSink {
    inner: HtmlTreeSink,
    limit: usize,
    exceeded: Cell<bool>,
}

impl DepthLimitedSink {
    /// Notes `child` if it is an element that now sits deeper than the limit.
    fn check(&self, child: Option<Handle>) {
        let Some(child) = child else {
            return;
        };
        let html = self.inner.0.borrow();
        let Some(node) = html.tree.get(child) else {
            return;
        };
        if !node.value().is_element() {
            return;
        }
        // Ancestors are the elements above plus the document node, which
        // stands in for the element itself in the count.
        if node.ancestors().take(self.limit + 1).count() > self.limit {
            self.exceeded.set(true);
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
        self.check(node);
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
        self.check(node);
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
        self.check(node);
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
        // Moved children sit one level below their new parent.
        let first = {
            let html = self.inner.0.borrow();
            html.tree
                .get(*new_parent)
                .and_then(|parent| parent.last_child())
                .map(|child| child.id())
        };
        self.check(first);
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

/// Parses `html` as a document, or returns `None` as soon as an element is
/// nested deeper than `limit`.
///
/// The result is the tree `scraper::Html::parse_document` would build.
pub(crate) fn parse_document_within(html: &str, limit: usize) -> Option<Html> {
    let sink = DepthLimitedSink {
        inner: HtmlTreeSink::new(Html::new_document()),
        limit,
        exceeded: Cell::new(false),
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
        if parser.tokenizer.sink.sink.exceeded.get() {
            return None;
        }
        rest = tail;
    }
    let exceeded = parser.tokenizer.sink.sink.exceeded.get();
    let document = parser.finish();
    (!exceeded).then_some(document)
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
            let pieces = parse_document_within(html, 512).expect("within the limit");
            assert_eq!(pieces.html(), whole.html());
        }
    }

    #[test]
    fn stops_at_the_limit() {
        let html = "<div>".repeat(100);
        assert!(parse_document_within(&html, 102).is_some());
        assert!(parse_document_within(&html, 101).is_none());
    }
}
