//! Lexical estimate of element nesting, taken before the tree is built.
//!
//! The tree builder's cost grows with the square of the nesting depth, so a
//! depth limit that is only checked on the finished tree arrives too late.
//! This scan reads the tags once and keeps a small model of the stack of
//! open elements. It follows the HTML rules that keep ordinary documents
//! flat: void elements, raw-text elements, implied end tags (`<p>`, `<li>`,
//! table parts, options), tags ignored inside `<select>`, self-closing tags
//! and the return from `<svg>`/`<math>` to HTML. Where the rules are subtle
//! it errs toward closing more than the tree builder would, so the estimate
//! leans low and the exact check on the built tree stays the contract.
//!
//! The scan costs one pass over the input plus, per tag, a walk of the model
//! stack, which never grows past the limit.

/// What an open element does to the tags that follow it.
#[derive(Clone, Copy, PartialEq, Eq)]
enum Open {
    P,
    Li,
    DdDt,
    Heading,
    OptionTag,
    Optgroup,
    Select,
    Anchor,
    Button,
    Nobr,
    Form,
    Table,
    Cell,
    Row,
    Section,
    Caption,
    List,
    DescriptionList,
    /// `marquee`, `object`, `applet`, `template`: scope boundaries.
    Scope,
    /// `svg` or `math`.
    Foreign,
    /// `foreignObject`, `desc`, `annotation-xml`: HTML resumes inside these.
    Integration,
    /// Any other element, identified by a hash of its lowercase name.
    Other(u64),
}

/// How one tag name is treated.
enum Class {
    /// Never on the stack: void elements and `html`, `head`, `body`.
    Skip,
    /// Content is text up to the matching end tag.
    RawText,
    /// Everything after the tag is text.
    Plaintext,
    /// An element that stays open until closed.
    Element {
        open: Open,
        closes_p: bool,
        leaves_foreign: bool,
    },
}

fn classify(name: &[u8]) -> Class {
    let mut buffer = [0u8; 16];
    let lower = if name.len() <= buffer.len() {
        let lower = &mut buffer[..name.len()];
        lower.copy_from_slice(name);
        lower.make_ascii_lowercase();
        &*lower
    } else {
        // No HTML element with special parsing rules has a name this long.
        return Class::Element {
            open: Open::Other(name_hash(name)),
            closes_p: false,
            leaves_foreign: false,
        };
    };
    let element = |open, closes_p, leaves_foreign| Class::Element {
        open,
        closes_p,
        leaves_foreign,
    };
    match lower {
        b"html" | b"head" | b"body" | b"frameset" | b"area" | b"base" | b"basefont"
        | b"bgsound" | b"br" | b"col" | b"embed" | b"frame" | b"hr" | b"img" | b"input"
        | b"keygen" | b"link" | b"meta" | b"param" | b"source" | b"track" | b"wbr" => Class::Skip,
        b"script" | b"style" | b"textarea" | b"title" | b"xmp" | b"iframe" | b"noembed"
        | b"noframes" | b"noscript" => Class::RawText,
        b"plaintext" => Class::Plaintext,
        b"p" => element(Open::P, true, true),
        b"li" => element(Open::Li, true, true),
        b"dd" | b"dt" => element(Open::DdDt, true, true),
        b"h1" | b"h2" | b"h3" | b"h4" | b"h5" | b"h6" => element(Open::Heading, true, true),
        b"option" => element(Open::OptionTag, false, false),
        b"optgroup" => element(Open::Optgroup, false, false),
        b"select" => element(Open::Select, false, false),
        b"a" => element(Open::Anchor, false, false),
        b"button" => element(Open::Button, false, false),
        b"nobr" => element(Open::Nobr, false, true),
        b"form" => element(Open::Form, true, false),
        b"table" => element(Open::Table, true, true),
        b"td" | b"th" => element(Open::Cell, false, false),
        b"tr" => element(Open::Row, false, false),
        b"tbody" | b"thead" | b"tfoot" => element(Open::Section, false, false),
        b"caption" | b"colgroup" => element(Open::Caption, false, false),
        b"ul" | b"ol" => element(Open::List, true, true),
        b"dl" => element(Open::DescriptionList, true, true),
        b"marquee" | b"object" | b"applet" | b"template" => element(Open::Scope, false, false),
        b"svg" | b"math" => element(Open::Foreign, false, false),
        b"foreignobject" | b"desc" | b"annotation-xml" => element(Open::Integration, false, false),
        b"address" | b"article" | b"aside" | b"details" | b"dialog" | b"dir" | b"fieldset"
        | b"figcaption" | b"figure" | b"footer" | b"header" | b"hgroup" | b"main" | b"nav"
        | b"search" | b"section" | b"summary" => {
            element(Open::Other(name_hash(lower)), true, false)
        }
        b"blockquote" | b"center" | b"div" | b"listing" | b"menu" | b"pre" => {
            element(Open::Other(name_hash(lower)), true, true)
        }
        b"b" | b"big" | b"code" | b"em" | b"i" | b"ruby" | b"s" | b"small" | b"span"
        | b"strong" | b"strike" | b"sub" | b"sup" | b"tt" | b"u" | b"var" => {
            element(Open::Other(name_hash(lower)), false, true)
        }
        _ => element(Open::Other(name_hash(lower)), false, false),
    }
}

/// FNV-1a over the lowercase name; a collision only merges two element names.
fn name_hash(name: &[u8]) -> u64 {
    let mut hash = 0xcbf2_9ce4_8422_2325u64;
    for byte in name {
        hash ^= u64::from(byte.to_ascii_lowercase());
        hash = hash.wrapping_mul(0x0000_0100_0000_01b3);
    }
    hash
}

/// Elements an implied end tag never reaches past.
fn is_boundary(open: Open) -> bool {
    matches!(
        open,
        Open::Table | Open::Cell | Open::Caption | Open::Scope | Open::Button | Open::Integration
    )
}

/// Closes the nearest element `matches` accepts, with everything above it,
/// unless an element `stops` accepts is met first.
fn close_through(
    stack: &mut Vec<Open>,
    matches: impl Fn(Open) -> bool,
    stops: impl Fn(Open) -> bool,
) {
    for index in (0..stack.len()).rev() {
        if matches(stack[index]) {
            stack.truncate(index);
            return;
        }
        if stops(stack[index]) {
            return;
        }
    }
}

/// Applies a start tag to the model; returns `false` when the tag is ignored.
fn open_element(stack: &mut Vec<Open>, open: Open, closes_p: bool, leaves_foreign: bool) -> bool {
    let has = |stack: &[Open], wanted: Open| stack.contains(&wanted);
    if has(stack, Open::Select)
        && !matches!(
            open,
            Open::OptionTag | Open::Optgroup | Open::Select | Open::Scope
        )
    {
        return false;
    }
    if leaves_foreign {
        close_through(stack, |entry| entry == Open::Foreign, is_boundary);
    }
    if closes_p {
        close_through(stack, |entry| entry == Open::P, is_boundary);
    }
    match open {
        Open::Li => close_through(
            stack,
            |entry| entry == Open::Li,
            |entry| is_boundary(entry) || entry == Open::List,
        ),
        Open::DdDt => close_through(
            stack,
            |entry| entry == Open::DdDt,
            |entry| is_boundary(entry) || entry == Open::DescriptionList,
        ),
        Open::Heading => {
            if stack.last() == Some(&Open::Heading) {
                stack.pop();
            }
        }
        Open::OptionTag => close_through(
            stack,
            |entry| entry == Open::OptionTag,
            |entry| is_boundary(entry) || entry == Open::Select,
        ),
        Open::Optgroup => close_through(
            stack,
            |entry| matches!(entry, Open::OptionTag | Open::Optgroup),
            |entry| is_boundary(entry) || entry == Open::Select,
        ),
        Open::Anchor | Open::Button | Open::Nobr | Open::Select => {
            close_through(stack, |entry| entry == open, is_boundary);
        }
        Open::Form => {
            if has(stack, Open::Form) {
                return false;
            }
        }
        Open::Row | Open::Cell | Open::Section | Open::Caption => {
            // Table parts outside a table are dropped by the tree builder.
            if !has(stack, Open::Table) {
                return false;
            }
            let closes = move |entry: Open| match open {
                Open::Cell => entry == Open::Cell,
                Open::Row => entry == Open::Row,
                Open::Section => matches!(entry, Open::Section | Open::Caption),
                _ => matches!(entry, Open::Section | Open::Caption | Open::Row),
            };
            close_through(stack, closes, |entry| entry == Open::Table);
        }
        Open::Table => {
            // A table directly inside a table (not inside a cell) closes it.
            close_through(
                stack,
                |entry| entry == Open::Table,
                |entry| {
                    matches!(
                        entry,
                        Open::Cell | Open::Caption | Open::Scope | Open::Integration
                    )
                },
            );
        }
        _ => {}
    }
    stack.push(open);
    true
}

/// Applies an end tag to the model.
fn close_element(stack: &mut Vec<Open>, open: Open) {
    let table_part = matches!(
        open,
        Open::Table | Open::Cell | Open::Row | Open::Section | Open::Caption
    );
    close_through(
        stack,
        |entry| entry == open,
        |entry| {
            entry == Open::Table
                || (!table_part
                    && matches!(
                        entry,
                        Open::Cell | Open::Caption | Open::Scope | Open::Integration
                    ))
        },
    );
}

fn find(bytes: &[u8], from: usize, needle: u8) -> Option<usize> {
    bytes
        .get(from..)?
        .iter()
        .position(|byte| *byte == needle)
        .map(|offset| from + offset)
}

/// Returns the index just past the `>` that ends the tag starting before
/// `from`, and whether the tag is self-closing; quoted attribute values may
/// contain `>`.
fn tag_end(bytes: &[u8], from: usize) -> (usize, bool) {
    let mut index = from;
    let mut after_equals = false;
    let mut last = 0u8;
    while let Some(&byte) = bytes.get(index) {
        match byte {
            b'>' => return (index + 1, last == b'/'),
            b'"' | b'\'' if after_equals => {
                index = find(bytes, index + 1, byte).unwrap_or(bytes.len());
                after_equals = false;
            }
            b'=' => after_equals = true,
            _ if byte.is_ascii_whitespace() => {}
            _ => after_equals = false,
        }
        if !byte.is_ascii_whitespace() {
            last = byte;
        }
        index += 1;
    }
    (bytes.len(), false)
}

/// Returns the index of the `</name` that ends a raw-text element.
fn raw_text_end(bytes: &[u8], from: usize, name: &[u8]) -> usize {
    let mut index = from;
    while let Some(start) = find(bytes, index, b'<') {
        let candidate = bytes.get(start + 2..start + 2 + name.len());
        if bytes.get(start + 1) == Some(&b'/')
            && candidate.is_some_and(|candidate| candidate.eq_ignore_ascii_case(name))
        {
            return start;
        }
        index = start + 1;
    }
    bytes.len()
}

/// Reports whether the tags in `html` appear to nest deeper than `limit`.
pub(crate) fn lexical_depth_exceeds(html: &[u8], limit: usize) -> bool {
    let mut stack: Vec<Open> = Vec::new();
    let mut index = 0usize;
    while let Some(start) = find(html, index, b'<') {
        let rest = &html[start + 1..];
        if rest.starts_with(b"!--") {
            // A comment ends at `-->` (`<!-->` is a whole comment); an
            // unterminated one runs to the end of the input.
            index = html[start + 2..]
                .windows(3)
                .position(|window| window == b"-->")
                .map_or(html.len(), |offset| start + 2 + offset + 3);
            continue;
        }
        let closing = rest.first() == Some(&b'/');
        let name_start = start + 1 + usize::from(closing);
        if !html.get(name_start).is_some_and(u8::is_ascii_alphabetic) {
            // `<!doctype`, `<?...`, or a bare `<` in text.
            index = match rest.first() {
                Some(b'!' | b'?' | b'/') => {
                    find(html, start, b'>').map_or(html.len(), |end| end + 1)
                }
                _ => start + 1,
            };
            continue;
        }
        let name_end = (name_start..html.len())
            .find(|at| html[*at].is_ascii_whitespace() || matches!(html[*at], b'/' | b'>'))
            .unwrap_or(html.len());
        let name = &html[name_start..name_end];
        let (after, self_closing) = tag_end(html, name_end);
        index = after;
        match classify(name) {
            Class::Skip => {}
            Class::Plaintext => {
                if !closing {
                    return false;
                }
            }
            Class::RawText => {
                if !closing && !self_closing {
                    index = raw_text_end(html, after, name);
                }
            }
            Class::Element { open, .. } if closing => close_element(&mut stack, open),
            Class::Element {
                open,
                closes_p,
                leaves_foreign,
            } => {
                // A self-closing tag opens nothing in `svg`/`math`; in HTML it
                // does, but skipping it keeps the estimate low.
                if !self_closing
                    && open_element(&mut stack, open, closes_p, leaves_foreign)
                    && stack.len() > limit
                {
                    return true;
                }
            }
        }
    }
    false
}

#[cfg(test)]
mod tests {
    use super::lexical_depth_exceeds;

    fn depth_of(html: &str) -> usize {
        (0..10_000)
            .find(|limit| !lexical_depth_exceeds(html.as_bytes(), *limit))
            .expect("depth below 10,000")
    }

    #[test]
    fn counts_plain_nesting() {
        assert_eq!(depth_of("<div><div><span>x</span></div></div>"), 3);
        assert_eq!(depth_of("<DIV><Div></div><div></DIV>"), 2);
    }

    #[test]
    fn implied_end_tags_keep_documents_flat() {
        assert_eq!(depth_of(&"<p>x".repeat(1_000)), 1);
        assert_eq!(depth_of(&format!("<ul>{}</ul>", "<li>x".repeat(1_000))), 2);
        assert_eq!(depth_of(&"<br><img src=a><hr>".repeat(1_000)), 0);
        assert_eq!(depth_of(&"<p><font size=2>x".repeat(1_000)), 2);
        let rows = "<tr><td>a<td>b".repeat(1_000);
        assert_eq!(depth_of(&format!("<table>{rows}</table>")), 3);
        assert_eq!(
            depth_of(&format!("<select>{}</select>", "<option>x".repeat(100))),
            2
        );
    }

    #[test]
    fn text_that_looks_like_tags_is_not_counted() {
        let fake = "<div>".repeat(1_000);
        assert_eq!(depth_of(&format!("<script>{fake}</script>")), 0);
        assert_eq!(depth_of(&format!("<!-- {fake} -->")), 0);
        assert_eq!(depth_of(&format!("<a title=\"{fake}\">x</a>")), 1);
        assert_eq!(depth_of(&format!("<plaintext>{fake}")), 0);
        let paths = "<path d=\"M0 0\"/>".repeat(1_000);
        assert_eq!(depth_of(&format!("<svg>{paths}</svg>")), 1);
    }

    #[test]
    fn nested_tables_count_every_level() {
        let html = "<table><tr><td>".repeat(10);
        assert_eq!(depth_of(&html), 30);
    }
}
