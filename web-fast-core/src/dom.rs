//! Iterative DOM indexing and bounded text extraction.
use crate::{decoding::ascii_space, Budget, ParseOptions, WebParseError};
use ego_tree::{iter::Edge, NodeId};
use scraper::{ElementRef, Html, Node};
use std::collections::HashMap;

pub(crate) struct ElementInfo {
    pub hidden: bool,
    pub inert: bool,
    position: usize,
}

pub(crate) struct DomIndex<'a> {
    pub elements: Vec<ElementRef<'a>>,
    pub info: HashMap<NodeId, ElementInfo>,
}

struct Frame<'a> {
    counts: HashMap<&'a str, usize>,
    hidden: bool,
    inert: bool,
    table: Option<NodeId>,
    row: Option<NodeId>,
}

impl<'a> DomIndex<'a> {
    pub fn new(document: &'a Html, options: &ParseOptions) -> Result<Self, WebParseError> {
        if document.tree.nodes().count() > options.max_dom_nodes {
            return Err(WebParseError::NodeLimitExceeded);
        }
        let mut index = Self {
            elements: Vec::new(),
            info: HashMap::new(),
        };
        let mut stack = vec![Frame {
            counts: HashMap::new(),
            hidden: false,
            inert: false,
            table: None,
            row: None,
        }];
        for edge in document.tree.root().traverse() {
            match edge {
                Edge::Open(node) => {
                    let Some(element) = ElementRef::wrap(node) else {
                        continue;
                    };
                    if stack.len() > options.max_dom_depth {
                        return Err(if options.max_dom_depth >= crate::MAX_NESTING_DEPTH {
                            WebParseError::NestingLimitExceeded
                        } else {
                            WebParseError::DepthLimitExceeded
                        });
                    }
                    let parent = stack.last_mut().unwrap();
                    let name = element.value().name();
                    let position = parent.counts.entry(name).or_default();
                    *position += 1;
                    let hidden = parent.hidden || is_hidden(element);
                    let inert = parent.inert || name == "template";
                    let table = if name == "table" {
                        Some(node.id())
                    } else {
                        parent.table
                    };
                    let row = if name == "table" {
                        None
                    } else if name == "tr" {
                        Some(node.id())
                    } else {
                        parent.row
                    };
                    index.info.insert(
                        node.id(),
                        ElementInfo {
                            hidden,
                            inert,
                            position: *position,
                        },
                    );
                    index.elements.push(element);
                    stack.push(Frame {
                        counts: HashMap::new(),
                        hidden,
                        inert,
                        table,
                        row,
                    });
                }
                Edge::Close(node) if node.value().is_element() => {
                    stack.pop();
                }
                _ => {}
            }
        }
        Ok(index)
    }

    pub fn path(
        &self,
        element: ElementRef<'_>,
        budget: &mut Budget,
    ) -> Result<String, WebParseError> {
        let mut parts = std::iter::once(element)
            .chain(element.ancestors().filter_map(ElementRef::wrap))
            .collect::<Vec<_>>();
        parts.reverse();
        let mut path = String::new();
        for part in parts {
            let segment = format!(
                "/{}[{}]",
                part.value().name(),
                self.info[&part.id()].position
            );
            budget.add_path(&segment)?;
            path.push_str(&segment);
        }
        Ok(path)
    }

    pub fn text(
        &self,
        element: ElementRef<'_>,
        mode: TextMode,
        skip_tables: bool,
        budget: &mut Budget,
    ) -> Result<String, WebParseError> {
        let mut text = Text::new(budget, mode);
        let mut suppressed = 0usize;
        let mut preformatted = usize::from(matches!(mode, TextMode::Preformatted));
        for edge in element.traverse() {
            match edge {
                Edge::Open(node) => match node.value() {
                    Node::Element(value) => {
                        let hidden = self.info.get(&node.id()).is_some_and(|info| info.hidden)
                            || (skip_tables
                                && node.id() != element.id()
                                && value.name() == "table");
                        if suppressed > 0 || hidden {
                            suppressed += 1;
                            continue;
                        }
                        if value.name() == "pre" {
                            preformatted += 1;
                        }
                        if is_boundary(value.name()) {
                            text.boundary();
                        }
                    }
                    Node::Text(value) if suppressed == 0 => text.push(value, preformatted > 0)?,
                    _ => {}
                },
                Edge::Close(node) => {
                    if let Node::Element(value) = node.value() {
                        if suppressed > 0 {
                            suppressed -= 1;
                            continue;
                        }
                        if value.name() == "pre" {
                            preformatted -= 1;
                        }
                        if is_boundary(value.name()) {
                            text.boundary();
                        }
                    }
                }
            }
        }
        Ok(text.output)
    }
}

#[derive(Clone, Copy)]
pub(crate) enum TextMode {
    Normalized,
    Preformatted,
    Document,
}

struct Text<'a> {
    output: String,
    pending: Option<char>,
    budget: &'a mut Budget,
    mode: TextMode,
}

impl<'a> Text<'a> {
    fn new(budget: &'a mut Budget, mode: TextMode) -> Self {
        Self {
            output: String::new(),
            pending: None,
            budget,
            mode,
        }
    }
    fn boundary(&mut self) {
        if !self.output.is_empty() {
            self.pending = Some(
                if matches!(self.mode, TextMode::Document | TextMode::Preformatted) {
                    '\n'
                } else {
                    ' '
                },
            );
        }
    }
    fn append(&mut self, value: &str) -> Result<(), WebParseError> {
        self.budget.add_text(value)?;
        self.output.push_str(value);
        Ok(())
    }
    fn flush_space(&mut self) -> Result<(), WebParseError> {
        if let Some(space) = self.pending.take() {
            if !self.output.is_empty() && !self.output.ends_with('\n') {
                self.append(if space == '\n' { "\n" } else { " " })?;
            }
        }
        Ok(())
    }
    fn push(&mut self, value: &str, preformatted: bool) -> Result<(), WebParseError> {
        if preformatted {
            if !value.is_empty() {
                self.flush_space()?;
                self.append(value)?;
            }
            return Ok(());
        }
        // Append whole non-whitespace runs without allocating a Vec of words.
        let mut start = 0;
        for (offset, c) in value.char_indices() {
            if ascii_space(c) {
                if start < offset {
                    self.flush_space()?;
                    self.append(&value[start..offset])?;
                }
                if self.pending.is_none() && !self.output.is_empty() {
                    self.pending = Some(' ');
                }
                start = offset + c.len_utf8();
            }
        }
        if start < value.len() {
            self.flush_space()?;
            self.append(&value[start..])?;
        }
        Ok(())
    }
}

fn is_hidden(element: ElementRef<'_>) -> bool {
    element.value().attr("hidden").is_some()
        || element
            .value()
            .attr("aria-hidden")
            .is_some_and(|value| value.eq_ignore_ascii_case("true"))
        || matches!(
            element.value().name(),
            "script" | "style" | "noscript" | "template" | "svg" | "canvas"
        )
}

fn is_boundary(tag: &str) -> bool {
    matches!(
        tag,
        "address"
            | "article"
            | "aside"
            | "blockquote"
            | "br"
            | "caption"
            | "dd"
            | "div"
            | "dl"
            | "dt"
            | "fieldset"
            | "figcaption"
            | "figure"
            | "footer"
            | "form"
            | "h1"
            | "h2"
            | "h3"
            | "h4"
            | "h5"
            | "h6"
            | "header"
            | "hr"
            | "li"
            | "main"
            | "nav"
            | "ol"
            | "p"
            | "pre"
            | "section"
            | "table"
            | "tbody"
            | "td"
            | "th"
            | "thead"
            | "tr"
            | "ul"
    )
}
