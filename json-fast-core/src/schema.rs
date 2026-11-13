//! JSON Schema validation — every constraint keyword in draft-07 through
//! 2020-12 (Validation, Applicator and Unevaluated vocabularies), plus local
//! `$ref` and boolean schemas.
//!
//! Supported keywords: `type`, `nullable` (OpenAPI), `enum`, `const`,
//! `multipleOf`, `minimum`, `maximum`, `exclusiveMinimum` (numeric or the
//! OpenAPI boolean form), `exclusiveMaximum`, `minLength`, `maxLength`,
//! `pattern`, `format`, `minItems`, `maxItems`, `uniqueItems`, `contains`,
//! `minContains`, `maxContains`, `items` (schema and tuple forms),
//! `prefixItems`, `additionalItems`, `minProperties`, `maxProperties`,
//! `required`, `properties`, `patternProperties`, `additionalProperties`
//! (boolean or schema), `propertyNames`, `dependencies` (both forms),
//! `dependentRequired`, `dependentSchemas`, `unevaluatedItems`,
//! `unevaluatedProperties`, `allOf`, `anyOf`, `oneOf`, `not`,
//! `if`/`then`/`else`, `$ref` (local: root, JSON-pointer fragments, and
//! `$anchor`/plain-name fragments), `$defs` and `definitions` as containers.
//!
//! Semantics follow 2020-12 where drafts differ (2020-12 `$ref` siblings are
//! evaluated; `prefixItems` + single-schema `items`; `contains` annotates the
//! indices it matches so `unevaluatedItems` can see them). Draft-07 and
//! 2019-09 forms (`items` tuples, `additionalItems`, `dependencies`) are
//! accepted alongside. `format` is asserted for the formats this crate
//! implements; an unknown format name is a warning, never a silent pass.
//!
//! `format` validates only string instances. `contentEncoding`,
//! `contentMediaType` and `contentSchema` are annotations by spec and are
//! ignored. Core-keyword gaps that would change a document's validity
//! (`$dynamicRef`, `$recursiveRef`) and remote `$ref` targets are surfaced as
//! warnings so validation never silently under-checks; genuinely unknown
//! keywords likewise warn.
//!
//! Patterns and `format` regexes compile with the Rust `regex` crate; an
//! ECMA-262 construct Rust `regex` cannot express makes the keyword a warning
//! (the check is skipped) rather than a silent pass.
//!
//! # Performance profile
//!
//! The document and schema are borrowed, never copied. `validate` allocates
//! nothing on the happy path: the JSON pointer is carried as a reused working
//! buffer (a finding materializes its own `String` only when one exists),
//! patterns are compiled once and cached, `propertyNames` reuses one scratch
//! `Value`, `uniqueItems` runs in O(n) with a hash prefilter plus exact
//! re-check, the `unevaluated` tracker stores borrowed property names, and
//! subschema applications under `dependencies` / `dependentSchemas` /
//! `items` / `properties` apply to borrowed slices with no intermediate
//! copies. Pairwise string/byte work rides the `regex` crate's memchr/SIMD
//! fast paths; the JSON parse itself is sonic-rs (SIMD). The `mmap` crate
//! feature adds zero-cpu-copy file entry points ([`crate::parse_file`],
//! [`crate::validate_file`]).
//!
//! `validate` never panics and never allocates outside the findings list and
//! the evaluation tracker.

use std::collections::{HashMap, HashSet};

use serde_json::map::Map;
use serde_json::Value;

/// Deepest array/object nesting [`validate`] accepts in the schema and in the
/// document: the parser's default, [`crate::DEFAULT_MAX_NESTING_DEPTH`].
///
/// Text parsed by this crate already respects it; a `Value` built in code
/// does not, and the validator recurses once per level.
pub const MAX_VALIDATION_NESTING_DEPTH: usize = crate::DEFAULT_MAX_NESTING_DEPTH;

/// Most subschema applications that may be in progress at once.
///
/// The validator recurses once per application, and `$ref`, `allOf` and the
/// other in-place keywords apply schemas without descending the document, so
/// this depth is not bounded by the 64 levels of nesting alone. 2,048 allows
/// 32 nested applications for every one of the 64 levels a document may
/// have; a schema that wraps each level in a handful of `allOf`/`anyOf`/
/// `$ref` layers uses 5 to 10.
///
/// Stack: an application measured 4 to 8 KiB in an unoptimized build. The
/// worst-case depth is worked out before validating, from the longest chain
/// of subschemas and the document's depth. Up to
/// [`INLINE_APPLICATION_DEPTH`] it runs on the caller's stack; beyond that it
/// runs, once, on a thread whose stack is that depth (at most this limit)
/// times `STACK_BYTES_PER_APPLICATION`: at most 64 MiB of address space,
/// touched only as far as the recursion goes.
pub const MAX_APPLICATION_DEPTH: usize = 2_048;

/// Nested applications the caller's own stack is trusted with.
///
/// Measured in an unoptimized build, an application costs 4 to 6 KiB of
/// stack: about 52 of the worst shape fit a 256 KiB stack and about 102 fit
/// 512 KiB. 48 keeps a caller on a 256 KiB stack safe. A validation whose
/// worst case is deeper runs on a dedicated thread from the start.
const INLINE_APPLICATION_DEPTH: usize = 48;

/// Stack reserved per nested application on the dedicated thread: four
/// times the 8 KiB measured in an unoptimized build.
const STACK_BYTES_PER_APPLICATION: usize = 32 * 1024;

/// Most bytes of remembered results per validation; past this, results are
/// recomputed instead of kept.
const MAX_REMEMBERED_BYTES: usize = 64 * 1024 * 1024;

/// Subschema applications allowed per schema node and document value.
///
/// One validation may apply at most
/// `VALIDATION_STEPS_PER_PAIR * (schema values + 1) * (document values + 1)`
/// subschemas. This is a backstop, not a working limit: the result of
/// applying a `$ref` target or an `anyOf`/`oneOf`/`not`/`if`/`contains`
/// candidate to an array or object is remembered for the rest of the
/// validation, so a schema node is fully applied to a document value about
/// once and the work is at most the product of the two sizes. Only schemas
/// that fan out in place on a scalar can reach it.
pub const VALIDATION_STEPS_PER_PAIR: u64 = 64;

/// Most bytes of regular-expression text one validation may compile.
///
/// Counts `pattern`, `patternProperties` keys and, because `format: "regex"`
/// compiles a string taken from the document, every distinct document
/// string checked against that format. Each expression is compiled once and
/// kept for the whole validation, so the bytes of pattern text bound both
/// the compile time and the memory an input can demand. The number of
/// expressions is deliberately not limited: a schema with 5,000 short
/// patterns is ordinary. Beyond this many bytes validation stops with an
/// error.
pub const MAX_PATTERN_BYTES_PER_VALIDATION: usize = 1 << 20;

/// Severity of a schema [`Finding`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Severity {
    /// A concrete schema violation — the document does not match the schema.
    Error,
    /// The schema used a construct this crate does not evaluate; check skipped.
    Warning,
}

impl Severity {
    /// The lowercase name used in JSON/CLI output (`"error"` / `"warning"`).
    pub const fn as_str(self) -> &'static str {
        match self {
            Severity::Error => "error",
            Severity::Warning => "warning",
        }
    }
}

/// One schema-validation finding (a violation or a skipped-check note).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Finding {
    /// Severity of the finding.
    pub severity: Severity,
    /// JSON pointer of the value in the document (`$` for the root).
    pub path: String,
    /// Human-readable message.
    pub message: String,
}

impl Finding {
    fn error(path: &str, message: impl Into<String>) -> Finding {
        Finding {
            severity: Severity::Error,
            path: path.to_string(),
            message: message.into(),
        }
    }

    fn warning(path: &str, message: impl Into<String>) -> Finding {
        Finding {
            severity: Severity::Warning,
            path: path.to_string(),
            message: message.into(),
        }
    }
}

/// Keywords that carry no constraint and are ignored without a warning.
///
/// This includes the Meta-Data vocabulary (`title`, `description`, `default`,
/// `deprecated`, `readOnly`, `writeOnly`, `examples`), Core identifiers
/// (`$schema`, `$id`, `$anchor`, `$dynamicAnchor`, `$recursiveAnchor`,
/// `$vocabulary`, `$comment`), and the Content vocabulary
/// (`contentEncoding`, `contentMediaType`, `contentSchema`), which is
/// annotation-only in every draft from draft-07 on.
const ANNOTATION: &[&str] = &[
    "$schema",
    "$id",
    "$anchor",
    "$dynamicAnchor",
    "$recursiveAnchor",
    "$vocabulary",
    "$comment",
    "title",
    "description",
    "default",
    "deprecated",
    "examples",
    "readOnly",
    "writeOnly",
    "contentEncoding",
    "contentMediaType",
    "contentSchema",
];

/// Container keywords whose members are reached by `$ref`, never directly.
const CONTAINER: &[&str] = &["$defs", "definitions"];

/// Constraint-relevant Core keywords that are not implemented; surfaced as
/// warnings so a schema that relies on them cannot silently under-check.
const UNSUPPORTED: &[&str] = &["$dynamicRef", "$recursiveRef"];

/// Every constraint keyword this validator applies.
const SUPPORTED: &[&str] = &[
    "type",
    "nullable",
    "enum",
    "const",
    "multipleOf",
    "minimum",
    "maximum",
    "exclusiveMinimum",
    "exclusiveMaximum",
    "minLength",
    "maxLength",
    "pattern",
    "format",
    "minItems",
    "maxItems",
    "uniqueItems",
    "contains",
    "minContains",
    "maxContains",
    "items",
    "prefixItems",
    "additionalItems",
    "minProperties",
    "maxProperties",
    "required",
    "properties",
    "patternProperties",
    "additionalProperties",
    "propertyNames",
    "dependencies",
    "dependentRequired",
    "dependentSchemas",
    "unevaluatedItems",
    "unevaluatedProperties",
    "allOf",
    "anyOf",
    "oneOf",
    "not",
    "if",
    "then",
    "else",
    "$ref",
];

/// Which instance properties/items an application of a subschema evaluated
/// (for 2019-09+ `unevaluatedProperties` / `unevaluatedItems`).
///
/// Property names are borrowed from the document, so tracking an evaluated
/// property is a reference insert, not a string copy.
#[derive(Debug, Default, Clone)]
struct Evaluated<'v> {
    props: HashSet<&'v str>,
    items: HashSet<usize>,
}

impl<'v> Evaluated<'v> {
    fn merge(&mut self, other: &Evaluated<'v>) {
        self.props.extend(other.props.iter().copied());
        self.items.extend(other.items.iter().copied());
    }
}

/// Outcome of applying one subschema to a value: did it pass, and what did it
/// evaluate. A subschema that fails does not count as evaluating anything.
struct Applied<'v> {
    passed: bool,
    evaluated: Evaluated<'v>,
}

struct Ctx<'a, 'v> {
    root: &'a Value,
    /// `$anchor`/plain-name fragment name -> JSON pointers that define it.
    anchors: HashMap<String, Vec<String>>,
    /// Compiled patterns, keyed by pattern text, so rows never recompile.
    patterns: HashMap<String, Result<regex::Regex, String>>,
    /// Bytes of pattern text compiled so far.
    pattern_bytes: usize,
    /// Subschema applications in progress.
    depth: usize,
    /// Subschema applications started so far.
    steps: u64,
    /// Subschema applications allowed in total.
    budget: u64,
    /// Set once a limit is hit. Every application after that returns at
    /// once, and `validate` reports the reason itself: a finding pushed where
    /// the limit was hit could be discarded by `anyOf`, or turned into a pass
    /// by `not`.
    halted: Option<&'static str>,
    /// Results of shared applications, by (schema node, document value)
    /// address. Both trees are borrowed for the whole validation, so an
    /// address identifies a node.
    /// `None` is the common outcome, kept in a few bytes: passed, with no
    /// findings and nothing evaluated.
    remembered: HashMap<(usize, usize), Option<Box<Remembered<'v>>>>,
    /// Estimated bytes held by `remembered`.
    remembered_bytes: usize,
    /// Schema nodes that lie on a `$ref` cycle. Only these can be applied
    /// to the same value twice, so only their results are remembered.
    cyclic: &'a HashSet<usize>,
    /// Shared applications currently running; meeting one again means the
    /// schema applies itself to the same value without end.
    running: HashSet<(usize, usize)>,
}

/// The outcome of one subschema application, kept for reuse.
struct Remembered<'v> {
    passed: bool,
    evaluated: Evaluated<'v>,
    /// Findings with `path` relative to the value the schema was applied to.
    findings: Vec<Finding>,
}

/// Why validation stopped at a schema that applies itself forever.
const ENDLESS_SELF_APPLICATION: &str = "validation stopped: the schema applies itself to \
     the same value without end (a `$ref` cycle that never descends into the document); \
     the document was not fully validated";

/// Why validation stopped at the work budget.
const BUDGET_EXHAUSTED: &str = "validation stopped: the schema needs more subschema \
     applications than the work budget allows (a schema that branches into itself); \
     the document was not fully validated";

/// Why validation stopped at the pattern budget.
const PATTERN_BUDGET_EXHAUSTED: &str = "validation stopped: the schema and document ask for \
     more than 1 MiB of regular-expression text; \
     the document was not fully validated";

/// Why validation stopped at the application depth.
const TOO_MANY_NESTED_APPLICATIONS: &str = "validation stopped: more than 2048 subschema \
     applications are nested inside each other; the document was not fully validated";

/// Counts the values in `root` and its deepest array/object nesting, without
/// recursion, so a `Value` nested without limit cannot overflow the stack.
fn measure(root: &Value) -> (u64, usize) {
    let mut values = 0u64;
    let mut deepest = 0usize;
    let mut pending: Vec<(&Value, usize)> = vec![(root, 0)];
    while let Some((value, depth)) = pending.pop() {
        values = values.saturating_add(1);
        match value {
            Value::Array(items) => {
                deepest = deepest.max(depth + 1);
                pending.extend(items.iter().map(|item| (item, depth + 1)));
            }
            Value::Object(map) => {
                deepest = deepest.max(depth + 1);
                pending.extend(map.values().map(|item| (item, depth + 1)));
            }
            _ => {}
        }
    }
    (values, deepest)
}

/// What the schema's shape says about the work of validating with it.
struct SchemaShape {
    /// Addresses of the schema nodes that lie on a `$ref` cycle.
    cyclic: HashSet<usize>,
    /// Longest chain of nested subschemas, following `$ref` but not around
    /// a cycle.
    longest_chain: usize,
    /// `$ref`s that lead back into a schema still being applied.
    back_references: usize,
}

/// Finds the `$ref` cycles of a schema and its longest chain of subschemas.
///
/// One depth-first walk over the schema's objects and arrays, with `$ref`
/// as an extra edge, using Tarjan's strongly-connected-components algorithm
/// on an explicit stack.
fn schema_shape(root: &Value, anchors: &HashMap<String, Vec<String>>) -> SchemaShape {
    fn address(value: &Value) -> usize {
        std::ptr::from_ref(value) as usize
    }
    fn children<'a>(
        node: &'a Value,
        root: &'a Value,
        anchors: &HashMap<String, Vec<String>>,
    ) -> Vec<&'a Value> {
        let nested = |value: &&Value| value.is_object() || value.is_array();
        match node {
            Value::Array(items) => items.iter().filter(nested).collect(),
            Value::Object(map) => {
                let mut out: Vec<&Value> = map.values().filter(nested).collect();
                let target = map
                    .get("$ref")
                    .and_then(Value::as_str)
                    .and_then(|reference| resolve_reference(reference, root, anchors));
                out.extend(target.filter(nested));
                out
            }
            _ => Vec::new(),
        }
    }
    struct Frame<'a> {
        node: &'a Value,
        index: usize,
        children: Vec<&'a Value>,
        next: usize,
    }
    let mut shape = SchemaShape {
        cyclic: HashSet::new(),
        longest_chain: 1,
        back_references: 0,
    };
    let mut indexes: HashMap<usize, usize> = HashMap::new();
    let mut low: Vec<usize> = Vec::new();
    let mut chain: Vec<usize> = Vec::new();
    let mut open: Vec<bool> = Vec::new();
    let mut component: Vec<usize> = Vec::new();
    let mut frames: Vec<Frame<'_>> = Vec::new();
    let enter = |node: &'_ Value,
                 indexes: &mut HashMap<usize, usize>,
                 low: &mut Vec<usize>,
                 chain: &mut Vec<usize>,
                 open: &mut Vec<bool>|
     -> usize {
        let index = low.len();
        indexes.insert(address(node), index);
        low.push(index);
        chain.push(usize::from(node.is_object()));
        open.push(true);
        index
    };
    if !(root.is_object() || root.is_array()) {
        return shape;
    }
    let index = enter(root, &mut indexes, &mut low, &mut chain, &mut open);
    component.push(address(root));
    frames.push(Frame {
        node: root,
        index,
        children: children(root, root, anchors),
        next: 0,
    });
    while let Some(frame) = frames.last_mut() {
        if let Some(child) = frame.children.get(frame.next).copied() {
            frame.next += 1;
            let own = usize::from(frame.node.is_object());
            match indexes.get(&address(child)).copied() {
                Some(seen) if open[seen] => {
                    // Back into a schema that is still being walked.
                    low[frame.index] = low[frame.index].min(seen);
                    shape.back_references += 1;
                    if seen == frame.index {
                        shape.cyclic.insert(address(child));
                    }
                }
                Some(seen) => {
                    chain[frame.index] = chain[frame.index].max(own + chain[seen]);
                }
                None => {
                    let index = enter(child, &mut indexes, &mut low, &mut chain, &mut open);
                    component.push(address(child));
                    frames.push(Frame {
                        node: child,
                        index,
                        children: children(child, root, anchors),
                        next: 0,
                    });
                }
            }
            continue;
        }
        let Some(done) = frames.pop() else {
            break;
        };
        shape.longest_chain = shape.longest_chain.max(chain[done.index]);
        if low[done.index] == done.index {
            // `done` roots a component: everything walked since it began.
            let start = component
                .iter()
                .rposition(|member| *member == address(done.node))
                .unwrap_or(0);
            let members = component.split_off(start);
            for member in &members {
                if let Some(index) = indexes.get(member) {
                    open[*index] = false;
                }
            }
            if members.len() > 1 {
                shape.cyclic.extend(members);
            }
        }
        if let Some(parent) = frames.last_mut() {
            let own = usize::from(parent.node.is_object());
            low[parent.index] = low[parent.index].min(low[done.index]);
            chain[parent.index] = chain[parent.index].max(own + chain[done.index]);
        }
    }
    shape
}

/// The result of one validation, with the counters the tests bound work by.
#[cfg_attr(
    not(test),
    expect(dead_code, reason = "the counters are read by tests")
)]
struct Outcome {
    findings: Vec<Finding>,
    /// Subschema applications started.
    steps: u64,
    /// Results kept for reuse.
    remembered: usize,
    /// Whether the validation ran on a dedicated thread.
    spawned: bool,
}

impl Outcome {
    fn stopped(reason: impl Into<String>) -> Self {
        Self {
            findings: vec![Finding::error("$", reason)],
            steps: 0,
            remembered: 0,
            spawned: false,
        }
    }
}

/// Why validation could not start on a dedicated thread.
const NO_VALIDATION_THREAD: &str = "validation stopped: this schema and document need a \
     dedicated stack and the thread for it could not be started; the document was not validated";

/// Why validation on the dedicated thread did not return.
const VALIDATION_THREAD_FAILED: &str = "validation stopped: the validation thread ended \
     abnormally; the document was not validated";

/// Runs one validation. `budget` of `None` applies the documented budget.
fn run(document: &Value, schema: &Value, budget: Option<u64>) -> Outcome {
    let (schema_values, schema_depth) = measure(schema);
    let (document_values, document_depth) = measure(document);
    for (what, depth) in [("schema", schema_depth), ("document", document_depth)] {
        if depth > MAX_VALIDATION_NESTING_DEPTH {
            return Outcome::stopped(format!(
                "the {what} is nested {depth} levels deep; the limit is \
                 {MAX_VALIDATION_NESTING_DEPTH}; the document was not validated"
            ));
        }
    }
    let budget = budget.unwrap_or_else(|| {
        VALIDATION_STEPS_PER_PAIR
            .saturating_mul(schema_values.saturating_add(1))
            .saturating_mul(document_values.saturating_add(1))
    });
    let anchors = collect_anchors(schema);
    let shape = schema_shape(schema, &anchors);
    // Worst-case nesting of applications, decided once. Without a cycle it
    // is the longest chain of subschemas. With one, a chain can restart at
    // every level of the document and once more per reference that leads
    // back into the cycle (the same schema cannot be entered twice on one
    // value).
    let worst_depth = if shape.cyclic.is_empty() {
        shape.longest_chain
    } else {
        shape
            .longest_chain
            .saturating_mul(shape.back_references + 1)
            .saturating_mul(document_depth + 1)
    };
    if worst_depth <= INLINE_APPLICATION_DEPTH {
        return run_on_this_stack(document, schema, budget, anchors, &shape.cyclic);
    }
    let stack_bytes = worst_depth.min(MAX_APPLICATION_DEPTH) * STACK_BYTES_PER_APPLICATION;
    let cyclic = &shape.cyclic;
    std::thread::scope(|scope| {
        let spawned = std::thread::Builder::new()
            .stack_size(stack_bytes)
            .spawn_scoped(scope, move || {
                run_on_this_stack(document, schema, budget, anchors, cyclic)
            });
        match spawned {
            Ok(handle) => match handle.join() {
                Ok(mut outcome) => {
                    outcome.spawned = true;
                    outcome
                }
                Err(_) => Outcome::stopped(VALIDATION_THREAD_FAILED),
            },
            Err(_) => Outcome::stopped(NO_VALIDATION_THREAD),
        }
    })
}

/// Validates on the current thread's stack.
fn run_on_this_stack(
    document: &Value,
    schema: &Value,
    budget: u64,
    anchors: HashMap<String, Vec<String>>,
    cyclic: &HashSet<usize>,
) -> Outcome {
    let mut findings = Vec::new();
    let mut ctx = Ctx {
        root: schema,
        anchors,
        patterns: HashMap::new(),
        pattern_bytes: 0,
        depth: 0,
        steps: 0,
        budget,
        halted: None,
        remembered: HashMap::new(),
        remembered_bytes: 0,
        cyclic,
        running: HashSet::new(),
    };
    let mut path = String::from("$");
    validate_node(document, schema, &mut path, &mut ctx, &mut findings);
    if let Some(reason) = ctx.halted {
        // Findings gathered around an abandoned application are not
        // verdicts, so only the reason is reported.
        findings = vec![Finding::error("$", reason)];
    }
    Outcome {
        findings,
        steps: ctx.steps,
        remembered: ctx.remembered.len(),
        spawned: false,
    }
}

impl<'a> Ctx<'a, '_> {
    /// Hands the shared state to a context for values with another lifetime
    /// (the scratch value of `propertyNames`); [`Ctx::rejoin`] takes it back.
    fn fork<'s>(&mut self) -> Ctx<'a, 's> {
        Ctx {
            root: self.root,
            anchors: std::mem::take(&mut self.anchors),
            patterns: std::mem::take(&mut self.patterns),
            pattern_bytes: self.pattern_bytes,
            depth: self.depth,
            steps: self.steps,
            budget: self.budget,
            halted: self.halted,
            remembered: HashMap::new(),
            remembered_bytes: 0,
            cyclic: self.cyclic,
            running: HashSet::new(),
        }
    }

    fn rejoin(&mut self, inner: Ctx<'a, '_>) {
        self.anchors = inner.anchors;
        self.patterns = inner.patterns;
        self.pattern_bytes = inner.pattern_bytes;
        self.steps = inner.steps;
        self.halted = inner.halted;
    }
}

impl Ctx<'_, '_> {
    /// Compiles `pattern` once per validation, within the pattern budget.
    ///
    /// Returns `None` when the budget is exhausted; validation is then
    /// halted, so the caller only has to stop.
    fn compile_pattern(&mut self, pattern: &str) -> Option<Result<regex::Regex, String>> {
        if let Some(hit) = self.patterns.get(pattern).cloned() {
            return Some(hit);
        }
        self.pattern_bytes = self.pattern_bytes.saturating_add(pattern.len());
        if self.pattern_bytes > MAX_PATTERN_BYTES_PER_VALIDATION {
            self.halted = Some(PATTERN_BUDGET_EXHAUSTED);
            return None;
        }
        let compiled = regex::Regex::new(pattern).map_err(|e| e.to_string());
        self.patterns.insert(pattern.to_string(), compiled.clone());
        Some(compiled)
    }
}

/// Validate a JSON document against a JSON Schema.
///
/// Returns every [`Finding`] (errors and warnings). Use [`has_errors`] to test
/// whether the document violates the schema.
///
/// Validation is bounded. When the schema or the document is nested deeper
/// than [`MAX_VALIDATION_NESTING_DEPTH`], when more than
/// [`MAX_APPLICATION_DEPTH`] subschema applications nest, or when the work
/// budget ([`VALIDATION_STEPS_PER_PAIR`]) runs out, the result is a single
/// [`Severity::Error`] finding at `$` that says so: an unfinished validation
/// is never reported as a clean document.
///
/// One case differs from validators that stop at the first matching
/// alternative. A schema that applies itself to the same value again while
/// that application is still running, with no step into the document in
/// between, is such an error even when another alternative would have
/// matched first: `a = {"anyOf": [{"type": "integer"}, {"$ref": "#/$defs/a"}]}`
/// stops with an error for `1`, where a short-circuiting validator accepts
/// it. Every alternative is evaluated here, and that one never finishes.
pub fn validate(document: &Value, schema: &Value) -> Vec<Finding> {
    run(document, schema, None).findings
}

/// True when any finding is an [`Severity::Error`].
pub fn has_errors(findings: &[Finding]) -> bool {
    findings.iter().any(|f| f.severity == Severity::Error)
}

/// Parse two JSON strings and validate `document` against `schema`.
pub fn validate_str(document: &str, schema: &str) -> crate::Result<Vec<Finding>> {
    let doc: Value = crate::parse(document)?;
    let sch: Value = crate::parse(schema)?;
    Ok(validate(&doc, &sch))
}

/// Evaluate `value` against `schema`, appending the findings to `findings`.
///
/// Returns what the application evaluated. `path` is a reusable buffer that
/// currently holds the JSON pointer of `value`; nested calls push and restore
/// their own segments, so the happy path allocates nothing.
fn validate_node<'v>(
    value: &'v Value,
    schema: &Value,
    path: &mut String,
    ctx: &mut Ctx<'_, 'v>,
    findings: &mut Vec<Finding>,
) -> Evaluated<'v> {
    if ctx.halted.is_some() {
        return Evaluated::default();
    }
    ctx.steps += 1;
    if ctx.steps > ctx.budget {
        ctx.halted = Some(BUDGET_EXHAUSTED);
        return Evaluated::default();
    }
    if ctx.depth >= MAX_APPLICATION_DEPTH {
        ctx.halted = Some(TOO_MANY_NESTED_APPLICATIONS);
        return Evaluated::default();
    }
    ctx.depth += 1;
    let evaluated = apply_schema(value, schema, path, ctx, findings);
    ctx.depth -= 1;
    evaluated
}

fn apply_schema<'v>(
    value: &'v Value,
    schema: &Value,
    path: &mut String,
    ctx: &mut Ctx<'_, 'v>,
    findings: &mut Vec<Finding>,
) -> Evaluated<'v> {
    match schema {
        Value::Bool(true) => Evaluated::default(),
        Value::Bool(false) => {
            findings.push(Finding::error(
                path,
                "boolean schema `false` matches no value",
            ));
            Evaluated::default()
        }
        Value::Object(obj) => apply_object(value, obj, path, ctx, findings),
        _ => {
            findings.push(Finding::warning(
                path,
                "schema node is not an object; skipped",
            ));
            Evaluated::default()
        }
    }
}

/// Run one subschema application whose failures are real findings: every
/// error and warning the subschema produced is surfaced.
fn applying<'v>(
    value: &'v Value,
    schema: &Value,
    path: &mut String,
    ctx: &mut Ctx<'_, 'v>,
    findings: &mut Vec<Finding>,
) -> Applied<'v> {
    let mut local = Vec::new();
    let evaluated = validate_node(value, schema, path, ctx, &mut local);
    let passed = !local.iter().any(|f| f.severity == Severity::Error);
    findings.extend(local);
    Applied { passed, evaluated }
}

/// Like [`applying`], but for subschemas whose failure is an expected outcome
/// (`anyOf` / `oneOf` / `contains` candidates, the `if` condition): errors
/// feed the verdict only and are not surfaced. Warnings still are — a skipped
/// check is always worth seeing.
fn applying_soft<'v>(
    value: &'v Value,
    schema: &Value,
    path: &mut String,
    ctx: &mut Ctx<'_, 'v>,
    findings: &mut Vec<Finding>,
) -> Applied<'v> {
    applying_shared(value, schema, path, ctx, findings, true)
}

/// Applies a subschema whose outcome can be asked for again: a `$ref`
/// target, or a candidate of `anyOf`/`oneOf`/`not`/`if`/`contains`.
///
/// The outcome for an array or object is remembered by (schema node,
/// document value), so a recursive schema that offers several alternatives
/// per level does each piece of work once instead of once per path to it.
/// Meeting an application that is still running means the schema applies
/// itself to the same value forever; that stops the validation.
///
/// With `soft`, errors feed the verdict only and warnings are surfaced.
fn applying_shared<'v>(
    value: &'v Value,
    schema: &Value,
    path: &mut String,
    ctx: &mut Ctx<'_, 'v>,
    findings: &mut Vec<Finding>,
    soft: bool,
) -> Applied<'v> {
    let schema_address = std::ptr::from_ref(schema) as usize;
    if !ctx.cyclic.contains(&schema_address) {
        // Not on a `$ref` cycle: this pair cannot come up again.
        let mut local = Vec::new();
        let evaluated = validate_node(value, schema, path, ctx, &mut local);
        let passed = !local.iter().any(|f| f.severity == Severity::Error);
        if soft {
            local.retain(|f| f.severity == Severity::Warning);
        }
        findings.append(&mut local);
        return Applied { passed, evaluated };
    }
    let key = (schema_address, std::ptr::from_ref(value) as usize);
    let surfaced = |finding: &Finding| !soft || finding.severity == Severity::Warning;
    match ctx.remembered.get(&key) {
        Some(None) => {
            return Applied {
                passed: true,
                evaluated: Evaluated::default(),
            };
        }
        Some(Some(known)) => {
            findings.extend(
                known
                    .findings
                    .iter()
                    .filter(|finding| surfaced(finding))
                    .map(|finding| Finding {
                        severity: finding.severity,
                        path: format!("{path}{}", finding.path),
                        message: finding.message.clone(),
                    }),
            );
            return Applied {
                passed: known.passed,
                evaluated: known.evaluated.clone(),
            };
        }
        None => {}
    }
    if !ctx.running.insert(key) {
        ctx.halted = Some(ENDLESS_SELF_APPLICATION);
        return Applied {
            passed: true,
            evaluated: Evaluated::default(),
        };
    }
    let base = path.len();
    let mut local = Vec::new();
    let evaluated = validate_node(value, schema, path, ctx, &mut local);
    ctx.running.remove(&key);
    let passed = !local.iter().any(|f| f.severity == Severity::Error);
    let container = value.is_array() || value.is_object();
    if container && ctx.halted.is_none() && ctx.remembered_bytes < MAX_REMEMBERED_BYTES {
        let plain = local.is_empty() && evaluated.props.is_empty() && evaluated.items.is_empty();
        // A map slot is about 48 bytes; the rest is what the entry owns.
        let mut bytes = 48;
        let entry = (!plain).then(|| {
            let relative: Vec<Finding> = local
                .iter()
                .map(|finding| Finding {
                    severity: finding.severity,
                    path: finding.path.get(base..).unwrap_or_default().to_owned(),
                    message: finding.message.clone(),
                })
                .collect();
            bytes += 128
                + 24 * (evaluated.props.len() + evaluated.items.len())
                + relative
                    .iter()
                    .map(|f| 56 + f.path.len() + f.message.len())
                    .sum::<usize>();
            Box::new(Remembered {
                passed,
                evaluated: evaluated.clone(),
                findings: relative,
            })
        });
        ctx.remembered_bytes += bytes;
        ctx.remembered.insert(key, entry);
    }
    findings.extend(local.into_iter().filter(surfaced));
    Applied { passed, evaluated }
}

/// Reads a keyword whose value is a count. The specification allows any
/// number with an integral value, so `1.0` is 1.
fn count_keyword(obj: &Map<String, Value>, key: &str) -> Option<u64> {
    let value = obj.get(key)?;
    value.as_u64().or_else(|| {
        let float = value.as_f64()?;
        (float >= 0.0 && float.fract() == 0.0 && float < 1.8e19).then_some(float as u64)
    })
}

/// Concat `<path>/<n>` into the buffer without allocating.
fn push_index(path: &mut String, index: usize) {
    let mut digits = [0u8; 20];
    let mut n = index;
    let mut len = 0;
    loop {
        digits[len] = b'0' + (n % 10) as u8;
        len += 1;
        n /= 10;
        if n == 0 {
            break;
        }
    }
    for &d in digits[..len].iter().rev() {
        path.push(d as char);
    }
}

fn apply_object<'v>(
    value: &'v Value,
    obj: &Map<String, Value>,
    path: &mut String,
    ctx: &mut Ctx<'_, 'v>,
    findings: &mut Vec<Finding>,
) -> Evaluated<'v> {
    // Classify every keyword first: anything this validator neither applies
    // nor knows to ignore is a warning, so an unsupported construct can never
    // silently pass.
    for key in obj.keys() {
        let k = key.as_str();
        if ANNOTATION.contains(&k) || CONTAINER.contains(&k) {
            continue;
        }
        if UNSUPPORTED.contains(&k) {
            findings.push(Finding::warning(
                path,
                format!("unsupported schema keyword {key:?} ignored"),
            ));
        } else if !SUPPORTED.contains(&k) {
            findings.push(Finding::warning(
                path,
                format!("unknown schema keyword {key:?} ignored"),
            ));
        }
    }

    let mut evaluated = Evaluated::default();

    // OpenAPI `nullable: true` short-circuits null values.
    if obj.get("nullable").and_then(|n| n.as_bool()) == Some(true) && value.is_null() {
        return evaluated;
    }

    if let Some(reference) = obj.get("$ref").and_then(|r| r.as_str()) {
        match resolve_ref(reference, ctx) {
            Some(target) => {
                let applied = applying_shared(value, target, path, ctx, findings, false);
                if applied.passed {
                    evaluated.merge(&applied.evaluated);
                }
            }
            None => findings.push(Finding::warning(
                path,
                format!("$ref {reference:?} does not resolve in this document; skipped"),
            )),
        }
    }

    check_type(value, obj, path, findings);
    check_enum_const(value, obj, path, findings);

    if let Some(n) = value.as_f64() {
        check_numeric(n, obj, path, findings);
    }

    if let Some(s) = value.as_str() {
        check_string(s, obj, path, ctx, findings);
    }

    if let Some(arr) = value.as_array() {
        evaluated.merge(&check_array(arr, obj, path, ctx, findings));
    }

    if let Some(map) = value.as_object() {
        evaluated.merge(&check_object(value, map, obj, path, ctx, findings));
    }

    evaluated.merge(&check_combinators(value, obj, path, ctx, findings));

    check_unevaluated(value, obj, path, ctx, &mut evaluated, findings);

    evaluated
}

fn check_type(value: &Value, obj: &Map<String, Value>, path: &str, findings: &mut Vec<Finding>) {
    if let Some(t) = obj.get("type") {
        if !value_matches_type(value, t) {
            findings.push(Finding::error(
                path,
                format!(
                    "expected type {}, got {}",
                    t,
                    crate::value_type(value).as_str()
                ),
            ));
        }
    }
}

fn check_enum_const(
    value: &Value,
    obj: &Map<String, Value>,
    path: &str,
    findings: &mut Vec<Finding>,
) {
    if let Some(en) = obj.get("enum").and_then(|e| e.as_array()) {
        if !en.iter().any(|candidate| json_equal(candidate, value)) {
            findings.push(Finding::error(
                path,
                format!("value is not one of the {} enum entries", en.len()),
            ));
        }
    }
    if let Some(expected) = obj.get("const") {
        if !json_equal(value, expected) {
            findings.push(Finding::error(
                path,
                format!("value does not equal const {expected}"),
            ));
        }
    }
}

fn check_numeric(n: f64, obj: &Map<String, Value>, path: &str, findings: &mut Vec<Finding>) {
    if let Some(min) = obj.get("minimum").and_then(|m| m.as_f64()) {
        if n < min {
            findings.push(Finding::error(path, format!("{n} < minimum {min}")));
        }
    }
    if let Some(max) = obj.get("maximum").and_then(|m| m.as_f64()) {
        if n > max {
            findings.push(Finding::error(path, format!("{n} > maximum {max}")));
        }
    }

    // draft-07+ numeric form, and the OpenAPI boolean form (`exclusiveMinimum:
    // true` next to `minimum`), which the crate has always accepted.
    match obj.get("exclusiveMinimum") {
        Some(Value::Number(num)) => {
            if let Some(min) = num.as_f64() {
                if n <= min {
                    findings.push(Finding::error(
                        path,
                        format!("{n} <= exclusiveMinimum {min}"),
                    ));
                }
            } else {
                findings.push(Finding::warning(
                    path,
                    "exclusiveMinimum must be a number; ignored",
                ));
            }
        }
        Some(Value::Bool(true)) => {
            if let Some(min) = obj.get("minimum").and_then(|m| m.as_f64()) {
                if n <= min {
                    findings.push(Finding::error(
                        path,
                        format!("{n} <= minimum {min} (exclusive)"),
                    ));
                }
            } else {
                findings.push(Finding::warning(
                    path,
                    "exclusiveMinimum: true without minimum; ignored",
                ));
            }
        }
        Some(Value::Bool(false)) => {}
        Some(_) => findings.push(Finding::warning(
            path,
            "exclusiveMinimum must be a number or boolean; ignored",
        )),
        None => {}
    }

    match obj.get("exclusiveMaximum") {
        Some(Value::Number(num)) => {
            if let Some(max) = num.as_f64() {
                if n >= max {
                    findings.push(Finding::error(
                        path,
                        format!("{n} >= exclusiveMaximum {max}"),
                    ));
                }
            } else {
                findings.push(Finding::warning(
                    path,
                    "exclusiveMaximum must be a number; ignored",
                ));
            }
        }
        Some(Value::Bool(true)) => {
            if let Some(max) = obj.get("maximum").and_then(|m| m.as_f64()) {
                if n >= max {
                    findings.push(Finding::error(
                        path,
                        format!("{n} >= maximum {max} (exclusive)"),
                    ));
                }
            } else {
                findings.push(Finding::warning(
                    path,
                    "exclusiveMaximum: true without maximum; ignored",
                ));
            }
        }
        Some(Value::Bool(false)) => {}
        Some(_) => findings.push(Finding::warning(
            path,
            "exclusiveMaximum must be a number or boolean; ignored",
        )),
        None => {}
    }

    if let Some(m) = obj.get("multipleOf").and_then(|m| m.as_f64()) {
        if m > 0.0 && m.is_finite() {
            let quotient = n / m;
            let epsilon = 1e-9 * quotient.abs().max(1.0);
            let off = if quotient.is_finite() {
                (quotient - quotient.round()).abs() > epsilon
            } else {
                // The division overflowed; the remainder is still exact.
                let remainder = (n % m).abs();
                remainder > 1e-9 * m && m - remainder > 1e-9 * m
            };
            if off {
                findings.push(Finding::error(
                    path,
                    format!("{n} is not a multiple of {m}"),
                ));
            }
        } else {
            findings.push(Finding::warning(
                path,
                "multipleOf must be a number strictly greater than 0; ignored",
            ));
        }
    }
}

fn check_string(
    s: &str,
    obj: &Map<String, Value>,
    path: &str,
    ctx: &mut Ctx<'_, '_>,
    findings: &mut Vec<Finding>,
) {
    let len = s.chars().count() as u64;
    if let Some(min) = count_keyword(obj, "minLength") {
        if len < min {
            findings.push(Finding::error(
                path,
                format!("string length {len} < minLength {min}"),
            ));
        }
    }
    if let Some(max) = count_keyword(obj, "maxLength") {
        if len > max {
            findings.push(Finding::error(
                path,
                format!("string length {len} > maxLength {max}"),
            ));
        }
    }
    if let Some(p) = obj.get("pattern").and_then(|p| p.as_str()) {
        match ctx.compile_pattern(p) {
            Some(Ok(re)) => {
                if !re.is_match(s) {
                    findings.push(Finding::error(
                        path,
                        format!("string does not match pattern {p:?}"),
                    ));
                }
            }
            Some(Err(e)) => findings.push(Finding::warning(
                path,
                format!("invalid pattern {p:?}: {e}"),
            )),
            None => return,
        }
    }
    if let Some(f) = obj.get("format").and_then(|f| f.as_str()) {
        // `regex` compiles a string from the document: it goes through the
        // same cache and budget as the schema's own patterns.
        let checked = if f == "regex" {
            match ctx.compile_pattern(s) {
                Some(Ok(_)) => Ok(()),
                Some(Err(e)) => Err(FormatError::Violation(format!(
                    "not a valid regular expression: {e}"
                ))),
                None => return,
            }
        } else {
            check_format(f, s)
        };
        match checked {
            Ok(()) => {}
            Err(FormatError::Unimplemented) => findings.push(Finding::warning(
                path,
                format!("format {f:?} is not implemented by this crate; check skipped"),
            )),
            Err(FormatError::Violation(reason)) => findings.push(Finding::error(
                path,
                format!("string does not match format {f:?}: {reason}"),
            )),
        }
    }
}

/// One element of an array under `items` / `prefixItems` / `contains`: run the
/// subschema, mark the index evaluated on success, descend the path.
fn check_item<'v>(
    item: &'v Value,
    subschema: &Value,
    index: usize,
    path: &mut String,
    ctx: &mut Ctx<'_, 'v>,
    findings: &mut Vec<Finding>,
    evaluated: &mut Evaluated<'v>,
) {
    let saved = path.len();
    path.push('/');
    push_index(path, index);
    let applied = applying(item, subschema, path, ctx, findings);
    path.truncate(saved);
    // The index is covered by this keyword whether or not the item is valid
    // (an invalid item fails the array on its own); what the item evaluated
    // inside itself belongs to the item, not to this array.
    let _ = applied;
    evaluated.items.insert(index);
}

fn check_array<'v>(
    arr: &'v [Value],
    obj: &Map<String, Value>,
    path: &mut String,
    ctx: &mut Ctx<'_, 'v>,
    findings: &mut Vec<Finding>,
) -> Evaluated<'v> {
    let mut evaluated = Evaluated::default();
    let len = arr.len() as u64;

    if let Some(min) = count_keyword(obj, "minItems") {
        if len < min {
            findings.push(Finding::error(
                path,
                format!("array length {} < minItems {min}", arr.len()),
            ));
        }
    }
    if let Some(max) = count_keyword(obj, "maxItems") {
        if len > max {
            findings.push(Finding::error(
                path,
                format!("array length {} > maxItems {max}", arr.len()),
            ));
        }
    }

    if obj.get("uniqueItems").and_then(Value::as_bool) == Some(true) {
        check_unique_items(arr, path, findings);
    }

    // items / prefixItems / additionalItems cover the array positions; every
    // subschema that passes marks its position as evaluated.
    let prefix = obj.get("prefixItems").and_then(|v| v.as_array());
    let items = obj.get("items");
    if let Some(prefix) = prefix {
        for (i, subschema) in prefix.iter().enumerate() {
            if let Some(item) = arr.get(i) {
                check_item(item, subschema, i, path, ctx, findings, &mut evaluated);
            }
        }
        if let Some(subschema) = items.filter(|v| matches!(v, Value::Object(_) | Value::Bool(_))) {
            for (i, item) in arr.iter().enumerate().skip(prefix.len()) {
                check_item(item, subschema, i, path, ctx, findings, &mut evaluated);
            }
        }
    } else if let Some(Value::Array(tuple)) = items {
        // draft-07 / 2019-09 tuple form; `additionalItems` covers the rest.
        for (i, subschema) in tuple.iter().enumerate() {
            if let Some(item) = arr.get(i) {
                check_item(item, subschema, i, path, ctx, findings, &mut evaluated);
            }
        }
        for (i, item) in arr.iter().enumerate().skip(tuple.len()) {
            match obj.get("additionalItems") {
                Some(Value::Bool(false)) => findings.push(Finding::error(
                    path,
                    format!("additional array item at index {i} not allowed"),
                )),
                Some(other) => check_item(item, other, i, path, ctx, findings, &mut evaluated),
                None => {}
            }
        }
    } else if let Some(subschema) = items.filter(|v| matches!(v, Value::Object(_) | Value::Bool(_)))
    {
        for (i, item) in arr.iter().enumerate() {
            check_item(item, subschema, i, path, ctx, findings, &mut evaluated);
        }
    }
    // `additionalItems` alone (no tuple/prefix `items`) is ignored, per spec.

    check_contains(arr, obj, path, ctx, findings, &mut evaluated);

    evaluated
}

/// `uniqueItems` in O(n): a canonical hash prefilter plus exact `Value`
/// equality inside the bucket, so distinct numbers that collide in f64 never
/// produce a false duplicate.
fn check_unique_items(arr: &[Value], path: &str, findings: &mut Vec<Finding>) {
    let mut buckets: HashMap<u64, Vec<usize>> = HashMap::with_capacity(64);
    'outer: for (i, item) in arr.iter().enumerate() {
        let hash = hash_value(item);
        let bucket = buckets.entry(hash).or_default();
        for &previous in bucket.iter() {
            if json_equal(&arr[previous], item) {
                findings.push(Finding::error(
                    path,
                    format!("array items at indexes {i} are not unique"),
                ));
                break 'outer;
            }
        }
        bucket.push(i);
    }
}

/// FNV-1a over a canonical byte form that mirrors `serde_json::Value`
/// equality: numbers hash by their f64 value (so `1` and `1.0` collide, as
/// they compare equal) and `-0.0` normalizes to `0.0`. Buckets re-check with
/// exact equality, so hash collisions can never change the verdict.
fn hash_value(value: &Value) -> u64 {
    const FNV_OFFSET: u64 = 0xcbf2_9ce4_8422_2325;
    const FNV_PRIME: u64 = 0x0000_0100_0000_01b3;
    let mut h = FNV_OFFSET;
    fn mix_u64(h: &mut u64, byte: u8) {
        *h ^= u64::from(byte);
        *h = h.wrapping_mul(FNV_PRIME);
    }
    fn mix(h: &mut u64, word: u64) {
        for shift in (0..64).step_by(8) {
            mix_u64(h, (word >> shift) as u8);
        }
    }
    fn walk(h: &mut u64, value: &Value) {
        match value {
            Value::Null => mix_u64(h, 0x00),
            Value::Bool(true) => mix_u64(h, 0x01),
            Value::Bool(false) => mix_u64(h, 0x02),
            Value::Number(n) => {
                mix_u64(h, 0x03);
                // Integers (and floats holding one) hash by their exact
                // value, as they compare: through f64, the integers near
                // 2^64 would all land in a few buckets.
                if let Some(integer) = exact_integer(n) {
                    mix(h, integer as u64);
                    mix(h, (integer >> 64) as u64);
                } else {
                    mix(h, n.as_f64().unwrap_or(f64::NAN).to_bits());
                }
            }
            Value::String(s) => {
                mix_u64(h, 0x04);
                for &b in s.as_bytes() {
                    mix_u64(h, b);
                }
                mix_u64(h, 0xff);
            }
            Value::Array(entries) => {
                mix_u64(h, 0x05);
                for entry in entries {
                    walk(h, entry);
                    mix_u64(h, 0xfe);
                }
            }
            Value::Object(map) => {
                mix_u64(h, 0x06);
                // Members are hashed one by one and summed, so two objects
                // that differ only in key order hash alike, as they compare.
                let mut members = 0u64;
                for (key, entry) in map {
                    let mut member = FNV_OFFSET;
                    for &b in key.as_bytes() {
                        mix_u64(&mut member, b);
                    }
                    mix_u64(&mut member, 0xfd);
                    walk(&mut member, entry);
                    members = members.wrapping_add(member);
                }
                mix(h, members);
            }
        }
    }
    walk(&mut h, value);
    h
}

/// `contains` (draft-07), plus `minContains` / `maxContains` (2019-09+).
/// Matched indices are marked evaluated for `unevaluatedItems`. Items that do
/// not match are the expected outcome — their errors stay local.
fn check_contains<'v>(
    arr: &'v [Value],
    obj: &Map<String, Value>,
    path: &mut String,
    ctx: &mut Ctx<'_, 'v>,
    findings: &mut Vec<Finding>,
    evaluated: &mut Evaluated<'v>,
) {
    let Some(subschema) = obj.get("contains") else {
        return; // minContains / maxContains have no effect without `contains`
    };
    let mut matched: Vec<usize> = Vec::new();
    for (i, item) in arr.iter().enumerate() {
        let saved = path.len();
        path.push('/');
        push_index(path, i);
        let applied = applying_soft(item, subschema, path, ctx, findings);
        path.truncate(saved);
        if applied.passed {
            matched.push(i);
            evaluated.items.insert(i);
        }
    }

    let min_contains = count_keyword(obj, "minContains").unwrap_or(1);
    let max_contains = count_keyword(obj, "maxContains");

    // minContains: 0 makes `contains` itself always pass.
    let enough = min_contains == 0 || matched.len() as u64 >= min_contains;
    if !enough {
        findings.push(Finding::error(
            path,
            format!(
                "array does not contain at least {min_contains} item(s) matching the contains schema"
            ),
        ));
    }
    if let Some(max) = max_contains {
        if matched.len() as u64 > max {
            findings.push(Finding::error(
                path,
                format!(
                    "array contains {} item(s) matching the contains schema; maxContains is {max}",
                    matched.len()
                ),
            ));
        }
    }
}

/// One property under `properties` / `patternProperties` /
/// `additionalProperties`: apply the subschema, mark the name evaluated on
/// success, descend the path.
fn check_prop<'v>(
    key: &'v str,
    value: &'v Value,
    subschema: &Value,
    path: &mut String,
    ctx: &mut Ctx<'_, 'v>,
    findings: &mut Vec<Finding>,
    evaluated: &mut Evaluated<'v>,
) {
    let saved = path.len();
    path.push('/');
    path.push_str(key);
    let applied = applying(value, subschema, path, ctx, findings);
    path.truncate(saved);
    // The name is covered by this keyword whether or not its value is valid
    // (an invalid value fails the object on its own); what the value
    // evaluated inside itself belongs to the value, not to this object.
    let _ = applied;
    evaluated.props.insert(key);
}

fn check_object<'v>(
    value: &'v Value,
    map: &'v Map<String, Value>,
    obj: &Map<String, Value>,
    path: &mut String,
    ctx: &mut Ctx<'_, 'v>,
    findings: &mut Vec<Finding>,
) -> Evaluated<'v> {
    let mut evaluated = Evaluated::default();
    let size = map.len() as u64;

    if let Some(min) = count_keyword(obj, "minProperties") {
        if size < min {
            findings.push(Finding::error(
                path,
                format!("object size {} < minProperties {min}", map.len()),
            ));
        }
    }
    if let Some(max) = count_keyword(obj, "maxProperties") {
        if size > max {
            findings.push(Finding::error(
                path,
                format!("object size {} > maxProperties {max}", map.len()),
            ));
        }
    }

    if let Some(req) = obj.get("required").and_then(|r| r.as_array()) {
        for entry in req {
            if let Some(key) = entry.as_str() {
                if !map.contains_key(key) {
                    findings.push(Finding::error(
                        path,
                        format!("missing required property {key:?}"),
                    ));
                }
            }
        }
    }

    // A property is evaluated when `properties`, `patternProperties` or
    // `additionalProperties` applies a schema to it successfully; the name is
    // recorded in `evaluated.props` (borrowed, zero-copy).
    if let Some(props) = obj.get("properties").and_then(|p| p.as_object()) {
        for (schema_key, subschema) in props {
            // The evaluated name must be the document's own key, so the
            // tracker stays borrowed from the document, never from the schema.
            if let Some((key, v)) = map.get_key_value(schema_key) {
                check_prop(key, v, subschema, path, ctx, findings, &mut evaluated);
            }
        }
    }

    let mut patterns: Vec<(regex::Regex, &Value)> = Vec::new();
    if let Some(pp) = obj.get("patternProperties").and_then(|p| p.as_object()) {
        for (pattern, subschema) in pp {
            match ctx.compile_pattern(pattern) {
                Some(Ok(re)) => patterns.push((re, subschema)),
                Some(Err(e)) => {
                    findings.push(Finding::warning(
                        path,
                        format!("invalid patternProperty pattern {pattern:?}: {e}"),
                    ));
                }
                None => return evaluated,
            }
        }
        for key in map.keys() {
            for (re, subschema) in &patterns {
                if re.is_match(key) {
                    check_prop(
                        key,
                        &map[key],
                        subschema,
                        path,
                        ctx,
                        findings,
                        &mut evaluated,
                    );
                }
            }
        }
    }

    let declared = obj.get("properties").and_then(|p| p.as_object());
    let unmatched = || -> Vec<&String> {
        map.keys()
            .filter(|key| {
                // A declared property is matched whether or not its value
                // is valid; only undeclared names are "additional".
                !declared.is_some_and(|names| names.contains_key(key.as_str()))
                    && !patterns.iter().any(|(re, _)| re.is_match(key))
            })
            .collect()
    };

    match obj.get("additionalProperties") {
        Some(Value::Bool(false)) => {
            for key in unmatched() {
                findings.push(Finding::error(
                    path,
                    format!("additional property {key:?} not allowed"),
                ));
            }
        }
        Some(Value::Bool(true)) | Some(Value::Object(_)) => {
            let subschema = obj.get("additionalProperties").unwrap();
            for key in unmatched() {
                check_prop(
                    key,
                    &map[key],
                    subschema,
                    path,
                    ctx,
                    findings,
                    &mut evaluated,
                );
            }
        }
        Some(_) => findings.push(Finding::warning(
            path,
            "additionalProperties must be a boolean or a schema; ignored",
        )),
        None => {}
    }

    if let Some(names) = obj.get("propertyNames") {
        // One scratch `Value` reused for every property name: validated names
        // are strings, so a single buffer is all the allocation the happy
        // path needs.
        let mut scratch = Value::String(String::new());
        for key in map.keys() {
            let saved = path.len();
            path.push('/');
            path.push_str(key);
            if let Value::String(slot) = &mut scratch {
                slot.clear();
                slot.push_str(key);
            }
            // The scratch value does not live as long as the document, so
            // it is validated through a context of its own lifetime.
            let mut inner = ctx.fork();
            applying(&scratch, names, path, &mut inner, findings);
            ctx.rejoin(inner);
            path.truncate(saved);
        }
    }

    // draft-07 `dependencies` (both forms) and the 2019-09+ replacements.
    if let Some(deps) = obj.get("dependencies").and_then(|d| d.as_object()) {
        for (trigger, dep) in deps {
            if !map.contains_key(trigger) {
                continue;
            }
            match dep {
                Value::Array(props) => {
                    for required in props {
                        if let Some(required) = required.as_str() {
                            if !map.contains_key(required) {
                                findings.push(Finding::error(
                                    path,
                                    format!("property {trigger:?} requires property {required:?}"),
                                ));
                            }
                        }
                    }
                }
                Value::Object(_) | Value::Bool(_) => {
                    // The subschema applies to the whole object; `value`
                    // already is that object, so nothing is copied.
                    let applied = applying(value, dep, path, ctx, findings);
                    if applied.passed {
                        evaluated.merge(&applied.evaluated);
                    }
                }
                _ => findings.push(Finding::warning(
                    path,
                    format!("dependency for {trigger:?} must be an array or a schema; ignored"),
                )),
            }
        }
    }

    if let Some(dep_req) = obj.get("dependentRequired").and_then(|d| d.as_object()) {
        for (trigger, props) in dep_req {
            if !map.contains_key(trigger) || !props.is_array() {
                continue;
            }
            for required in props.as_array().unwrap() {
                if let Some(required) = required.as_str() {
                    if !map.contains_key(required) {
                        findings.push(Finding::error(
                            path,
                            format!("property {trigger:?} requires property {required:?}"),
                        ));
                    }
                }
            }
        }
    }

    if let Some(dep_schemas) = obj.get("dependentSchemas").and_then(|d| d.as_object()) {
        for (trigger, schema) in dep_schemas {
            if map.contains_key(trigger) {
                let applied = applying(value, schema, path, ctx, findings);
                if applied.passed {
                    evaluated.merge(&applied.evaluated);
                }
            }
        }
    }

    evaluated
}

fn check_combinators<'v>(
    value: &'v Value,
    obj: &Map<String, Value>,
    path: &mut String,
    ctx: &mut Ctx<'_, 'v>,
    findings: &mut Vec<Finding>,
) -> Evaluated<'v> {
    let mut evaluated = Evaluated::default();

    if let Some(branches) = obj.get("allOf").and_then(|b| b.as_array()) {
        for (i, subschema) in branches.iter().enumerate() {
            let saved = path.len();
            path.push_str("/allOf/");
            push_index(path, i);
            let applied = applying(value, subschema, path, ctx, findings);
            path.truncate(saved);
            if applied.passed {
                evaluated.merge(&applied.evaluated);
            }
        }
    }

    if let Some(branches) = obj.get("anyOf").and_then(|b| b.as_array()) {
        let mut passes = 0usize;
        let mut branch_evaluated = Evaluated::default();
        for (i, subschema) in branches.iter().enumerate() {
            let saved = path.len();
            path.push_str("/anyOf/");
            push_index(path, i);
            let applied = applying_soft(value, subschema, path, ctx, findings);
            path.truncate(saved);
            if applied.passed {
                passes += 1;
            }
            // anyOf collects annotations from every branch that passed.
            if applied.passed {
                branch_evaluated.merge(&applied.evaluated);
            }
        }
        if passes == 0 {
            findings.push(Finding::error(
                path,
                format!("value matches none of the {} anyOf schemas", branches.len()),
            ));
        }
        evaluated.merge(&branch_evaluated);
    }

    if let Some(branches) = obj.get("oneOf").and_then(|b| b.as_array()) {
        let mut passes = 0usize;
        let mut branch_evaluated = Evaluated::default();
        for (i, subschema) in branches.iter().enumerate() {
            let saved = path.len();
            path.push_str("/oneOf/");
            push_index(path, i);
            let applied = applying_soft(value, subschema, path, ctx, findings);
            path.truncate(saved);
            if applied.passed {
                passes += 1;
                // oneOf collects annotations from the branches that passed.
                branch_evaluated.merge(&applied.evaluated);
            }
        }
        if passes != 1 {
            findings.push(Finding::error(
                path,
                format!(
                    "value matches {passes} of the {} oneOf schemas; must match exactly one",
                    branches.len()
                ),
            ));
        }
        evaluated.merge(&branch_evaluated);
    }

    if let Some(not_schema) = obj.get("not") {
        // `not` contributes no annotations; a skipped check inside still
        // warns, because the verdict is then uncertain.
        let passed = applying_soft(value, not_schema, path, ctx, findings).passed;
        if passed {
            findings.push(Finding::error(path, "value matches the `not` schema"));
        }
    }

    if let Some(if_schema) = obj.get("if") {
        let applied_if = applying_soft(value, if_schema, path, ctx, findings);
        if applied_if.passed {
            evaluated.merge(&applied_if.evaluated);
            if let Some(then_schema) = obj.get("then") {
                let applied = applying(value, then_schema, path, ctx, findings);
                if applied.passed {
                    evaluated.merge(&applied.evaluated);
                }
            }
        } else if let Some(else_schema) = obj.get("else") {
            let applied = applying(value, else_schema, path, ctx, findings);
            if applied.passed {
                evaluated.merge(&applied.evaluated);
            }
        }
    }

    evaluated
}

/// 2019-09+ `unevaluatedProperties` / `unevaluatedItems`: every property or
/// item index not evaluated by any other keyword must validate against these.
fn check_unevaluated<'v>(
    value: &'v Value,
    obj: &Map<String, Value>,
    path: &mut String,
    ctx: &mut Ctx<'_, 'v>,
    evaluated_total: &mut Evaluated<'v>,
    findings: &mut Vec<Finding>,
) {
    if let Some(subschema) = obj.get("unevaluatedProperties") {
        if let Some(map) = value.as_object() {
            for (key, member) in map {
                if !evaluated_total.props.contains(key.as_str()) {
                    let saved = path.len();
                    path.push('/');
                    path.push_str(key);
                    applying(member, subschema, path, ctx, findings);
                    path.truncate(saved);
                    // This keyword has now evaluated the property, which
                    // an enclosing `unevaluatedProperties` must see.
                    evaluated_total.props.insert(key.as_str());
                }
            }
        }
    }

    if let Some(subschema) = obj.get("unevaluatedItems") {
        if let Some(arr) = value.as_array() {
            for (i, item) in arr.iter().enumerate() {
                if !evaluated_total.items.contains(&i) {
                    let saved = path.len();
                    path.push('/');
                    push_index(path, i);
                    applying(item, subschema, path, ctx, findings);
                    path.truncate(saved);
                    evaluated_total.items.insert(i);
                }
            }
        }
    }
}

fn value_matches_type(value: &Value, t: &Value) -> bool {
    match t {
        Value::String(name) => type_matches(value, name),
        Value::Array(names) => names
            .iter()
            .filter_map(|n| n.as_str())
            .any(|name| type_matches(value, name)),
        _ => false,
    }
}

/// JSON value equality per the JSON Schema specification: two numbers are
/// equal when they have the same mathematical value (`1` equals `1.0`), which
/// `serde_json`'s `PartialEq` does not implement. Used by `const`, `enum` and
/// `uniqueItems`, which all compare by JSON equality.
fn json_equal(a: &Value, b: &Value) -> bool {
    match (a, b) {
        (Value::Number(x), Value::Number(y)) => numbers_equal(x, y),
        (Value::Array(x), Value::Array(y)) => {
            x.len() == y.len() && x.iter().zip(y).all(|(l, r)| json_equal(l, r))
        }
        (Value::Object(x), Value::Object(y)) => {
            x.len() == y.len()
                && x.iter()
                    .all(|(k, v)| y.get(k).is_some_and(|o| json_equal(v, o)))
        }
        _ => a == b,
    }
}

fn numbers_equal(a: &serde_json::Number, b: &serde_json::Number) -> bool {
    // Integers, and floats that hold an integer, are compared exactly:
    // 2^53 + 1 and 2^53 are different numbers that share one f64.
    match (exact_integer(a), exact_integer(b)) {
        (Some(x), Some(y)) => x == y,
        (None, None) => a.as_f64() == b.as_f64(),
        _ => false,
    }
}

/// The value of `n` as an integer, when it is one.
fn exact_integer(n: &serde_json::Number) -> Option<i128> {
    if let Some(value) = n.as_i64() {
        return Some(i128::from(value));
    }
    if let Some(value) = n.as_u64() {
        return Some(i128::from(value));
    }
    let float = n.as_f64()?;
    // Every f64 of this magnitude with no fraction converts exactly.
    (float.is_finite() && float.fract() == 0.0 && float.abs() < 1e38).then_some(float as i128)
}

fn type_matches(value: &Value, type_name: &str) -> bool {
    match type_name {
        "string" => value.is_string(),
        "number" => value.is_number(),
        "integer" => {
            value.as_i64().is_some()
                || value.as_u64().is_some()
                || value
                    .as_f64()
                    .is_some_and(|f| f.is_finite() && f.fract() == 0.0)
        }
        "boolean" => value.is_boolean(),
        "null" => value.is_null(),
        "array" => value.is_array(),
        "object" => value.is_object(),
        _ => false,
    }
}

/// Walk the root schema and record every `$anchor` (and `$id` in plain-name
/// fragment form) so `$ref: "#name"` resolves. Runs once per `validate`.
fn collect_anchors(root: &Value) -> HashMap<String, Vec<String>> {
    let mut anchors: HashMap<String, Vec<String>> = HashMap::new();
    let mut stack: Vec<(&Value, String)> = vec![(root, String::new())];
    while let Some((node, pointer)) = stack.pop() {
        if let Some(obj) = node.as_object() {
            if let Some(name) = obj
                .get("$anchor")
                .and_then(|a| a.as_str())
                .filter(|a| !a.is_empty())
            {
                anchors
                    .entry(name.to_string())
                    .or_default()
                    .push(pointer.clone());
            }
            if let Some(name) = obj
                .get("$id")
                .and_then(|i| i.as_str())
                .and_then(|i| i.strip_prefix('#'))
                .filter(|n| !n.is_empty())
            {
                anchors
                    .entry(name.to_string())
                    .or_default()
                    .push(pointer.clone());
            }
            for (key, child) in obj {
                let mut child_pointer = pointer.clone();
                child_pointer.push('/');
                child_pointer.push_str(&key.replace('~', "~0").replace('/', "~1"));
                child_pointer.shrink_to_fit();
                stack.push((child, child_pointer));
            }
        }
    }
    anchors
}

fn resolve_ref<'a>(reference: &str, ctx: &Ctx<'a, '_>) -> Option<&'a Value> {
    resolve_reference(reference, ctx.root, &ctx.anchors)
}

fn resolve_reference<'a>(
    reference: &str,
    root: &'a Value,
    anchors: &HashMap<String, Vec<String>>,
) -> Option<&'a Value> {
    if reference == "#" {
        return Some(root);
    }
    // A fragment is percent-encoded (RFC 3986); the JSON Pointer inside it
    // then has its own `~0`/`~1` escapes (RFC 6901).
    let fragment = percent_decode(reference.strip_prefix('#')?);
    if let Some(pointer) = fragment.strip_prefix('/') {
        return pointer_get(root, pointer);
    }
    {
        let name = fragment.as_ref();
        if let Some(pointers) = anchors.get(name) {
            for pointer in pointers {
                if let Some(target) = pointer_get(root, pointer) {
                    return Some(target);
                }
            }
        }
    }
    None
}

/// Decodes `%XX` escapes; text that is not valid UTF-8 afterwards, or has
/// no escapes, is returned unchanged.
fn percent_decode(text: &str) -> std::borrow::Cow<'_, str> {
    if !text.contains('%') {
        return std::borrow::Cow::Borrowed(text);
    }
    let bytes = text.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut index = 0;
    while let Some(&byte) = bytes.get(index) {
        let hex = |at: usize| bytes.get(at).and_then(|b| char::from(*b).to_digit(16));
        match (byte, hex(index + 1), hex(index + 2)) {
            (b'%', Some(high), Some(low)) => {
                out.push((high * 16 + low) as u8);
                index += 3;
            }
            _ => {
                out.push(byte);
                index += 1;
            }
        }
    }
    String::from_utf8(out).map_or(std::borrow::Cow::Borrowed(text), std::borrow::Cow::Owned)
}

/// RFC 6901 JSON-pointer get (document-local references only).
fn pointer_get<'a>(root: &'a Value, pointer: &str) -> Option<&'a Value> {
    let mut node = root;
    for raw in pointer.split('/') {
        let token = raw.replace("~1", "/").replace("~0", "~");
        node = match node {
            Value::Object(map) => map.get(&token)?,
            Value::Array(arr) => arr.get(token.parse::<usize>().ok()?)?,
            _ => return None,
        };
    }
    Some(node)
}

/// Why a `format` check did not pass.
enum FormatError {
    /// The format name is not implemented by this crate; the check is a
    /// warning and does not fail the instance.
    Unimplemented,
    /// The value violates a format this crate implements.
    Violation(String),
}

/// Best-effort `format` assertions for the format set this crate implements.
///
/// Each check is a bounded, simple validator — a full RFC parser is out of
/// scope.
fn check_format(format: &str, s: &str) -> Result<(), FormatError> {
    match format {
        "date-time" => validate_rfc3339(s, true).map_err(FormatError::Violation),
        "date" => validate_rfc3339(s, false).map_err(FormatError::Violation),
        "time" => validate_full_time(s).map_err(FormatError::Violation),
        "duration" => validate_duration(s).map_err(FormatError::Violation),
        "email" | "idn-email" => validate_email(s).map_err(FormatError::Violation),
        "hostname" => validate_hostname(s).map_err(FormatError::Violation),
        "idn-hostname" => validate_idn_hostname(s).map_err(FormatError::Violation),
        "ipv4" => validate_ipv4(s).map_err(FormatError::Violation),
        "ipv6" => validate_ipv6(s).map_err(FormatError::Violation),
        "uri" => validate_uri(s, false).map_err(FormatError::Violation),
        "uri-reference" => validate_uri(s, true).map_err(FormatError::Violation),
        "iri" | "iri-reference" => validate_iri(s).map_err(FormatError::Violation),
        "uri-template" => validate_uri_template(s).map_err(FormatError::Violation),
        "uuid" => validate_uuid(s).map_err(FormatError::Violation),
        "regex" => regex::Regex::new(s)
            .map(|_| ())
            .map_err(|e| FormatError::Violation(format!("not a valid regular expression: {e}"))),
        "json-pointer" => validate_json_pointer(s).map_err(FormatError::Violation),
        "relative-json-pointer" => {
            validate_relative_json_pointer(s).map_err(FormatError::Violation)
        }
        _ => Err(FormatError::Unimplemented),
    }
}

fn validate_rfc3339(s: &str, full_date_time: bool) -> Result<(), String> {
    let bytes = s.as_bytes();
    let digits = |bytes: &[u8], start: usize, count: usize| -> Result<u64, String> {
        let slice = bytes
            .get(start..start + count)
            .ok_or("field out of range")?;
        let mut value: u64 = 0;
        for &b in slice {
            if !b.is_ascii_digit() {
                return Err("field contains a non-digit".into());
            }
            value = value * 10 + (b - b'0') as u64;
        }
        Ok(value)
    };

    // `YYYY-MM-DD`
    let (year, month, day) = (
        digits(bytes, 0, 4)?,
        digits(bytes, 5, 2)?,
        digits(bytes, 8, 2)?,
    );
    if bytes.get(4) != Some(&b'-') || bytes.get(7) != Some(&b'-') {
        return Err("not an RFC 3339 date".into());
    }
    let leap = year.is_multiple_of(4) && (!year.is_multiple_of(100) || year.is_multiple_of(400));
    let days_in_month: u64 = match month {
        1 | 3 | 5 | 7 | 8 | 10 | 12 => 31,
        4 | 6 | 9 | 11 => 30,
        2 if leap => 29,
        2 => 28,
        _ => return Err("month out of range".into()),
    };
    if day < 1 || day > days_in_month {
        return Err("day out of range".into());
    }

    if !full_date_time {
        return if bytes.len() == 10 {
            Ok(())
        } else {
            Err("trailing characters after date".into())
        };
    }

    // `T` separator
    if bytes.get(10) != Some(&b'T') && bytes.get(10) != Some(&b't') {
        return Err("missing T separator".into());
    }
    let (hour, minute, second) = (
        digits(bytes, 11, 2)?,
        digits(bytes, 14, 2)?,
        digits(bytes, 17, 2)?,
    );
    if bytes.get(13) != Some(&b':') || bytes.get(16) != Some(&b':') {
        return Err("invalid time separator".into());
    }
    if hour > 23 || minute > 59 || second > 60 {
        return Err("time out of range".into());
    }

    let mut index = 19;
    if bytes.get(index) == Some(&b'.') || bytes.get(index) == Some(&b',') {
        index += 1;
        let mut fraction = false;
        while index < bytes.len() && bytes[index].is_ascii_digit() {
            fraction = true;
            index += 1;
        }
        if !fraction {
            return Err("empty fractional second".into());
        }
    }

    // `Z` or `±HH:MM`
    match bytes.get(index) {
        Some(b'Z') | Some(b'z') => {
            if index + 1 == bytes.len() {
                Ok(())
            } else {
                Err("trailing characters after offset".into())
            }
        }
        Some(b'+') | Some(b'-') => {
            let (oh, om) = (digits(bytes, index + 1, 2)?, digits(bytes, index + 4, 2)?);
            if bytes.get(index + 3) != Some(&b':') {
                return Err("invalid offset".into());
            }
            if oh > 23 || om > 59 {
                return Err("offset out of range".into());
            }
            if index + 6 == bytes.len() {
                Ok(())
            } else {
                Err("trailing characters after offset".into())
            }
        }
        _ => Err("missing timezone offset".into()),
    }
}

fn validate_full_time(s: &str) -> Result<(), String> {
    // RFC 3339 full-time; the offset is accepted but not required.
    let bytes = s.as_bytes();
    let digits = |bytes: &[u8], start: usize, count: usize| -> Result<u64, String> {
        let slice = bytes
            .get(start..start + count)
            .ok_or("field out of range")?;
        let mut value: u64 = 0;
        for &b in slice {
            if !b.is_ascii_digit() {
                return Err("field contains a non-digit".into());
            }
            value = value * 10 + (b - b'0') as u64;
        }
        Ok(value)
    };
    let (hour, minute, second) = (
        digits(bytes, 0, 2)?,
        digits(bytes, 3, 2)?,
        digits(bytes, 6, 2)?,
    );
    if bytes.get(2) != Some(&b':') || bytes.get(5) != Some(&b':') {
        return Err("invalid time separator".into());
    }
    if hour > 23 || minute > 59 || second > 60 {
        return Err("time out of range".into());
    }
    let mut index = 8;
    if bytes.get(index) == Some(&b'.') || bytes.get(index) == Some(&b',') {
        index += 1;
        let mut fraction = false;
        while index < bytes.len() && bytes[index].is_ascii_digit() {
            fraction = true;
            index += 1;
        }
        if !fraction {
            return Err("empty fractional second".into());
        }
    }
    match bytes.get(index) {
        None => Ok(()),
        Some(b'Z') | Some(b'z') if index + 1 == bytes.len() => Ok(()),
        Some(b'+') | Some(b'-') => {
            let (oh, om) = (digits(bytes, index + 1, 2)?, digits(bytes, index + 4, 2)?);
            if bytes.get(index + 3) != Some(&b':') || oh > 23 || om > 59 {
                return Err("invalid offset".into());
            }
            if index + 6 == bytes.len() {
                Ok(())
            } else {
                Err("trailing characters after offset".into())
            }
        }
        _ => Err("invalid timezone offset".into()),
    }
}

fn validate_duration(s: &str) -> Result<(), String> {
    // ISO 8601 duration, restricted to the commonly used form
    // `P[n]Y[n]M[n]DT[n]H[n]M[n]S` (and week form `P[n]W`).
    let bytes = s.as_bytes();
    if bytes.first() != Some(&b'P') {
        return Err("duration must start with P".into());
    }
    let mut index = 1;
    let mut any = false;
    let mut in_time = false;
    while index < bytes.len() {
        let b = bytes[index];
        if b == b'T' {
            if in_time {
                return Err("duplicate T".into());
            }
            in_time = true;
            index += 1;
            continue;
        }
        let start = index;
        while index < bytes.len() && bytes[index].is_ascii_digit() {
            index += 1;
        }
        if index == start {
            return Err("missing number in duration".into());
        }
        let unit = bytes
            .get(index)
            .copied()
            .ok_or("missing unit in duration")?;
        index += 1;
        match (unit, in_time) {
            (b'Y' | b'M' | b'D' | b'W', false) => any = true,
            (b'H' | b'M' | b'S', true) => any = true,
            _ => return Err("invalid duration unit".into()),
        }
    }
    if any {
        Ok(())
    } else {
        Err("empty duration".into())
    }
}

fn validate_email(s: &str) -> Result<(), String> {
    // RFC 5322 §3.4.1, kept bounded: exactly one @, non-empty halves, no
    // whitespace or control bytes, no leading/trailing dot on the domain.
    if s.contains(|c: char| c.is_whitespace() || c.is_control()) {
        return Err("email must not contain whitespace".into());
    }
    let mut parts = s.split('@');
    let local = parts.next().unwrap_or("");
    let domain = parts.next().unwrap_or("");
    if parts.next().is_some() || local.is_empty() || domain.is_empty() {
        return Err("email must have exactly one @".into());
    }
    if local.starts_with('.') || local.ends_with('.') {
        return Err("local part must not start or end with a dot".into());
    }
    if domain.starts_with('.') || domain.ends_with('.') {
        return Err("domain must not start or end with a dot".into());
    }
    Ok(())
}

fn validate_hostname(s: &str) -> Result<(), String> {
    if s.is_empty() || s.len() > 253 {
        return Err("hostname length out of range".into());
    }
    for label in s.split('.') {
        if label.is_empty() || label.len() > 63 {
            return Err("hostname label length out of range".into());
        }
        if label.starts_with('-') || label.ends_with('-') {
            return Err("hostname label must not start or end with a hyphen".into());
        }
        if !label
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b == b'-')
        {
            return Err("hostname label has invalid characters".into());
        }
    }
    Ok(())
}

fn validate_idn_hostname(s: &str) -> Result<(), String> {
    if s.is_empty() || s.len() > 253 {
        return Err("hostname length out of range".into());
    }
    for label in s.split('.') {
        if label.is_empty() || label.len() > 63 {
            return Err("hostname label length out of range".into());
        }
        if label.starts_with('-') || label.ends_with('-') {
            return Err("hostname label must not start or end with a hyphen".into());
        }
        if !label
            .chars()
            .all(|c| c.is_alphanumeric() || c == '-' || c == '_')
        {
            return Err("hostname label has invalid characters".into());
        }
    }
    Ok(())
}

fn validate_ipv4(s: &str) -> Result<(), String> {
    let mut count = 0;
    for part in s.split('.') {
        count += 1;
        if part.is_empty()
            || part.len() > 3
            || !part.bytes().all(|b| b.is_ascii_digit())
            || (part.len() > 1 && part.starts_with('0'))
        {
            return Err("invalid IPv4 octet".into());
        }
        let octet: u16 = part.parse().map_err(|_| "invalid IPv4 octet")?;
        if octet > 255 {
            return Err("IPv4 octet out of range".into());
        }
    }
    if count != 4 {
        return Err("IPv4 must have four octets".into());
    }
    Ok(())
}

fn validate_ipv6(s: &str) -> Result<(), String> {
    // RFC 4291 §2.2 textual form, including `::` compression and the
    // trailing embedded IPv4 form.
    if s.contains(":::")
        || s.starts_with(':') && !s.starts_with("::")
        || s.ends_with(':') && !s.ends_with("::")
    {
        return Err("invalid IPv6 compression".into());
    }

    let hex_group = |g: &str| -> bool {
        !g.is_empty() && g.len() <= 4 && g.bytes().all(|b| b.is_ascii_hexdigit())
    };

    let (head, tail): (&str, &str) = match s.split_once("::") {
        Some((h, t)) => (h, t),
        None => (s, ""),
    };

    if tail.contains('.') {
        validate_ipv4(tail)?;
        let head_groups = if head.is_empty() {
            0
        } else {
            head.split(':').filter(|g| !g.is_empty()).count()
        };
        if head_groups as i64 + 2 > 8 {
            return Err("IPv6 has too many groups".into());
        }
        return Ok(());
    }

    let groups: Vec<&str> = s.split(':').filter(|g| !g.is_empty()).collect();
    if groups.is_empty() || groups.len() > 8 {
        return Err("IPv6 group count out of range".into());
    }
    let all_hex = groups.iter().all(|g| hex_group(g));
    if all_hex {
        Ok(())
    } else {
        Err("invalid IPv6 group".into())
    }
}

fn validate_uri(s: &str, allow_relative: bool) -> Result<(), String> {
    if s.is_empty() || s.contains(|c: char| c.is_whitespace() || c.is_control()) {
        return Err("URI must not be empty or contain whitespace".into());
    }
    if let Some(colon) = s.find(':') {
        let scheme = &s[..colon];
        if !scheme.is_empty()
            && scheme
                .chars()
                .next()
                .is_some_and(|c| c.is_ascii_alphabetic())
            && scheme
                .bytes()
                .all(|b| b.is_ascii_alphanumeric() || b == b'+' || b == b'-' || b == b'.')
        {
            return Ok(());
        }
    }
    if allow_relative {
        Ok(())
    } else {
        Err("URI must have a scheme".into())
    }
}

fn validate_iri(s: &str) -> Result<(), String> {
    if s.is_empty() || s.contains(|c: char| c.is_whitespace() || c.is_control()) {
        return Err("IRI must not be empty or contain whitespace".into());
    }
    Ok(())
}

fn validate_uri_template(s: &str) -> Result<(), String> {
    if s.contains(|c: char| c.is_whitespace() || c.is_control()) {
        return Err("URI template must not contain whitespace".into());
    }
    let mut depth = 0usize;
    for b in s.bytes() {
        match b {
            b'{' => depth += 1,
            b'}' => {
                if depth == 0 {
                    return Err("unbalanced braces in URI template".into());
                }
                depth -= 1;
            }
            _ => {}
        }
    }
    if depth == 0 {
        Ok(())
    } else {
        Err("unbalanced braces in URI template".into())
    }
}

fn validate_uuid(s: &str) -> Result<(), String> {
    let bytes = s.as_bytes();
    if bytes.len() != 36 {
        return Err("UUID must be 36 characters".into());
    }
    let hex = |range: std::ops::Range<usize>| bytes[range].iter().all(|b| b.is_ascii_hexdigit());
    if bytes[8] == b'-'
        && bytes[13] == b'-'
        && bytes[18] == b'-'
        && bytes[23] == b'-'
        && hex(0..8)
        && hex(9..13)
        && hex(14..18)
        && hex(19..23)
        && hex(24..36)
    {
        Ok(())
    } else {
        Err("UUID must be 8-4-4-4-12 hex digits".into())
    }
}

fn validate_json_pointer(s: &str) -> Result<(), String> {
    if s.is_empty() {
        return Ok(());
    }
    if !s.starts_with('/') {
        return Err("JSON pointer must be empty or start with /".into());
    }
    let mut chars = s.chars().peekable();
    while chars.peek().is_some() {
        let c = chars.next().unwrap();
        if c == '~' {
            match chars.peek() {
                Some('0') | Some('1') => {
                    chars.next();
                }
                _ => return Err("~ in a JSON pointer must be followed by 0 or 1".into()),
            }
        }
    }
    Ok(())
}

fn validate_relative_json_pointer(s: &str) -> Result<(), String> {
    let bytes = s.as_bytes();
    let mut index = 0;
    while index < bytes.len() && bytes[index].is_ascii_digit() {
        index += 1;
    }
    if index == 0 {
        return Err("relative JSON pointer must start with a non-negative integer".into());
    }
    let rest = &s[index..];
    if rest.is_empty() {
        return Ok(());
    }
    match rest {
        "#" => Ok(()),
        _ => validate_json_pointer(rest),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn errors(document: &str, schema: &Value) -> Vec<Finding> {
        let doc: Value = serde_json::from_str(document).unwrap();
        validate(&doc, schema)
            .into_iter()
            .filter(|f| f.severity == Severity::Error)
            .collect()
    }

    fn fails(document: &str, schema: &Value) -> bool {
        !errors(document, schema).is_empty()
    }

    fn passes(document: &str, schema: &Value) -> bool {
        errors(document, schema).is_empty()
    }

    #[test]
    fn branching_reference_cycles_are_stopped_at_once() {
        let schema = json!({"allOf": [{"$ref": "#"}, {"$ref": "#"}]});
        let document = json!(1);
        // Left alone this doubled per reference hop (93, 1,533, 24,573 and
        // 393,213 applications at 4, 8, 12 and 16 hops). The schema applies
        // itself to the same value, which is now recognized at once.
        let Outcome {
            findings, steps, ..
        } = run(&document, &schema, None);
        assert!(steps < 10, "{steps} applications");
        assert_eq!(findings.len(), 1);
        assert_eq!(findings[0].severity, Severity::Error);
        assert!(findings[0].message.contains("without end"));
        // The exhaustion survives `not` and `anyOf`, which drop inner errors.
        let hidden = json!({"not": {"anyOf": [{"allOf": [{"$ref": "#"}, {"$ref": "#"}]}]}});
        assert!(has_errors(&validate(&document, &hidden)));
    }

    #[test]
    fn large_documents_stay_inside_the_budget() {
        let schema = json!({
            "type": "array",
            "items": {"$ref": "#/$defs/row"},
            "$defs": {"row": {
                "type": "object",
                "required": ["id"],
                "properties": {"id": {"type": "integer"}, "tags": {"items": {"type": "string"}}},
                "propertyNames": {"pattern": "^[a-z]+$"},
                "additionalProperties": false
            }}
        });
        let rows = (0..20_000)
            .map(|id| json!({"id": id, "tags": ["a", "b"]}))
            .collect::<Vec<_>>();
        let mut document = Value::Array(rows);
        assert_eq!(validate(&document, &schema), []);
        document[19_999]["id"] = json!("x");
        let findings = validate(&document, &schema);
        assert!(!findings.is_empty());
        assert!(findings.iter().all(|f| f.path.starts_with("$/19999")));
    }

    #[test]
    fn in_place_reference_chains_are_reported_as_endless() {
        // 32 hops through 60 nested `not` would recurse 1,920 levels.
        let mut schema = json!({"$ref": "#"});
        for _ in 0..60 {
            schema = json!({"not": schema});
        }
        let findings = validate(&json!(1), &schema);
        assert_eq!(findings.len(), 1);
        assert!(findings[0].message.contains("without end"));
    }

    #[test]
    fn pattern_compilations_are_bounded_per_validation() {
        // The same document string is compiled once, however often it occurs.
        let schema = json!({"items": {"format": "regex"}});
        let repeated = Value::Array(vec![json!("^a+$"); 50_000]);
        assert_eq!(validate(&repeated, &schema), []);

        // Distinct document strings each need a compilation: capped by bytes.
        let distinct = |count: usize| {
            Value::Array(
                (0..count)
                    .map(|i| Value::String(format!("^{i:0>1000}$")))
                    .collect(),
            )
        };
        assert_eq!(validate(&distinct(1_000), &schema), []);
        let findings = validate(&distinct(1_100), &schema);
        assert_eq!(findings.len(), 1);
        assert!(findings[0].message.contains("regular-expression text"));

        // Capped by total pattern bytes, for schema patterns as well.
        let long = "a".repeat(MAX_PATTERN_BYTES_PER_VALIDATION / 2);
        let schema = json!({"patternProperties": {long.clone(): true, format!("{long}b"): true, format!("{long}c"): true}});
        let findings = validate(&json!({"k": 1}), &schema);
        assert_eq!(findings.len(), 1);
        assert!(has_errors(&findings));
    }

    fn stopped(findings: &[Finding]) -> bool {
        findings
            .iter()
            .any(|f| f.message.contains("validation stopped"))
    }

    /// `node = oneOf[v0..v7]`, each variant an object whose `child` is a node.
    fn union_schema(variants: usize) -> Value {
        let alternatives = (0..variants)
            .map(|i| {
                json!({
                    "type": "object",
                    "properties": {"kind": {"const": format!("k{i}")}, "child": {"$ref": "#/$defs/node"}},
                    "required": ["kind"]
                })
            })
            .collect::<Vec<_>>();
        json!({"$ref": "#/$defs/node", "$defs": {"node": {"oneOf": alternatives}}})
    }

    fn union_chain(depth: usize, last_kind: &str) -> Value {
        let mut chain = json!({ "kind": last_kind });
        for level in 1..depth {
            chain = json!({"kind": format!("k{}", level % 8), "child": chain});
        }
        chain
    }

    #[test]
    fn recursive_discriminated_unions_validate_in_polynomial_work() {
        let schema = union_schema(8);
        let Outcome {
            findings, steps, ..
        } = run(&union_chain(40, "k3"), &schema, None);
        assert_eq!(findings, []);
        assert!(steps < 10_000, "{steps} applications");
        let findings = validate(&union_chain(40, "nope"), &schema);
        assert!(has_errors(&findings));
        assert!(!stopped(&findings));
    }

    #[test]
    fn linked_lists_deeper_than_32_are_fully_validated() {
        let schema = json!({
            "type": "object",
            "properties": {"v": {"type": "integer"}, "next": {"$ref": "#"}}
        });
        let list = |last: Value| {
            (0..60).fold(
                json!({ "v": last }),
                |next, _| json!({"v": 1, "next": next}),
            )
        };
        assert_eq!(validate(&list(json!(1)), &schema), []);
        let findings = validate(&list(json!("x")), &schema);
        assert_eq!(findings.len(), 1, "{findings:?}");
        assert_eq!(findings[0].severity, Severity::Error);
        assert!(findings[0].path.ends_with("/next/v"));
    }

    #[test]
    fn wrapped_recursive_schemas_validate_to_the_nesting_limit() {
        let schema = json!({
            "$ref": "#/$defs/node",
            "$defs": {
                "base": {"type": "object"},
                "node": {"allOf": [
                    {"$ref": "#/$defs/base"},
                    {"properties": {"children": {"type": "array", "items": {
                        "anyOf": [{"$ref": "#/$defs/node"}, {"type": "null"}]
                    }}}}
                ]}
            }
        });
        // 31 tree levels are 62 levels of nesting and 155 nested applications.
        let tree = (0..30).fold(
            json!({"children": [null]}),
            |inner, _| json!({"children": [inner]}),
        );
        assert_eq!(measure(&tree).1, 62);
        assert_eq!(validate(&tree, &schema), []);
    }

    #[test]
    fn many_pattern_properties_are_not_a_limit() {
        let mut properties = Map::new();
        let mut document = Map::new();
        for i in 0..5_000 {
            properties.insert(format!("p{i}"), json!({"pattern": format!("^v{i}$")}));
            document.insert(format!("p{i}"), json!(format!("v{i}")));
        }
        let schema = json!({ "properties": properties });
        assert_eq!(validate(&Value::Object(document), &schema), []);
    }

    #[test]
    fn unique_items_ignores_key_order_and_compares_integers_exactly() {
        let unique = json!({"uniqueItems": true});
        assert!(fails(r#"[{"a":1,"b":2},{"b":2,"a":1}]"#, &unique));
        assert!(fails(
            r#"[{"a":{"x":1,"y":[2]}},{"a":{"y":[2],"x":1.0}}]"#,
            &unique
        ));
        assert!(passes(r#"[{"a":1,"b":2},{"b":1,"a":2}]"#, &unique));
        // 2^53 + 1 is not 2^53, although both round to the same f64.
        assert!(passes("[9007199254740993, 9007199254740992.0]", &unique));
        assert!(passes("[9007199254740993, 9007199254740992]", &unique));
        assert!(fails("[9007199254740992, 9007199254740992.0]", &unique));
        assert!(fails("[-3, -3.0]", &unique));
    }

    #[test]
    fn results_are_remembered_only_where_the_schema_can_repeat() {
        // No `$ref` cycle: nothing can be asked twice, so nothing is kept.
        let schema = json!({"items": {"anyOf": [{"type": "object"}, {"type": "string"}, {"type": "array"}]},
            "contains": {"not": {"type": "null"}}, "if": {"type": "array"}, "then": {"$ref": "#/$defs/x"},
            "$defs": {"x": {"minItems": 1}}});
        let document = Value::Array(vec![json!([]); 5_000]);
        let outcome = run(&document, &schema, None);
        assert_eq!(outcome.findings, []);
        assert_eq!(outcome.remembered, 0);
        assert!(!outcome.spawned);
        // A recursive union needs them, and stays polynomial with them.
        let outcome = run(&union_chain(40, "k3"), &union_schema(8), None);
        assert_eq!(outcome.findings, []);
        assert!(outcome.remembered > 0);
        assert!(outcome.steps < 10_000, "{} applications", outcome.steps);
    }

    #[test]
    fn deep_validations_run_once_on_a_stack_chosen_up_front() {
        let list_schema = json!({"properties": {"next": {"$ref": "#"}, "v": {"type": "integer"}}});
        let list = |depth: usize| {
            (0..depth).fold(json!({"v": 1}), |next, _| json!({"v": 1, "next": next}))
        };
        // Shallow recursive documents stay on the caller's stack.
        let shallow = run(&list(3), &list_schema, None);
        assert!(!shallow.spawned);
        assert_eq!(shallow.findings, []);
        // Deep ones go straight to the large stack: the work is done once.
        let deep = run(&list(60), &list_schema, None);
        assert!(deep.spawned);
        assert_eq!(deep.findings, []);
        // Three applications per list node, less one: linear, and not doubled.
        assert_eq!((shallow.steps, deep.steps), (11, 182));
    }

    #[test]
    fn annotations_come_only_from_passing_schemas_at_the_same_level() {
        // A failed `anyOf` branch evaluates nothing.
        let schema = json!({"anyOf": [{"properties": {"a": true}, "required": ["zzz"]}, {"properties": {"b": true}}],
            "unevaluatedProperties": false});
        assert!(fails(r#"{"a":1,"b":2}"#, &schema));
        assert!(passes(r#"{"b":2}"#, &schema));
        // What a child evaluates inside itself says nothing about its parent.
        let schema = json!({"properties": {"foo": {"properties": {"bar": true}}}, "unevaluatedProperties": false});
        assert!(fails(r#"{"foo":{"bar":1},"bar":2}"#, &schema));
        assert!(passes(r#"{"foo":{"bar":1}}"#, &schema));
        let schema =
            json!({"prefixItems": [{"prefixItems": [true, true]}], "unevaluatedItems": false});
        assert!(fails("[[1,2],3]", &schema));
        assert!(passes("[[1,2]]", &schema));
    }

    #[test]
    fn a_failing_declared_property_is_reported_once() {
        let leaf = json!({"type": "object", "properties": {"v": {"type": "integer"}}, "additionalProperties": false});
        let schema = (0..25).fold(leaf, |inner, _| {
            json!({"type": "object", "properties": {"c": inner}, "additionalProperties": false, "unevaluatedProperties": false})
        });
        let document = (0..25).fold(json!({"v": "x"}), |inner, _| json!({ "c": inner }));
        let findings = validate(&document, &schema);
        assert_eq!(findings.len(), 1, "{findings:?}");
        assert!(findings[0].path.ends_with("/c/v"));
    }

    #[test]
    fn count_keywords_accept_integral_floats() {
        assert!(fails("[1,2]", &json!({"maxItems": 1.0})));
        assert!(fails("[]", &json!({"minItems": 1.0})));
        assert!(fails(r#""ab""#, &json!({"maxLength": 1.0})));
        assert!(fails(r#""""#, &json!({"minLength": 1.0})));
        assert!(fails(r#"{"a":1,"b":2}"#, &json!({"maxProperties": 1.0})));
        assert!(fails("{}", &json!({"minProperties": 1.0})));
        assert!(fails(
            "[1,1]",
            &json!({"contains": {"const": 1}, "maxContains": 1.0})
        ));
        assert!(fails(
            "[1]",
            &json!({"contains": {"const": 1}, "minContains": 2.0})
        ));
        assert!(passes("[1,2]", &json!({"maxItems": 2.0, "minItems": 2.0})));
        // Not a count: ignored, as before.
        assert!(passes("[1,2]", &json!({"maxItems": 1.5})));
    }

    #[test]
    fn references_decode_percent_and_pointer_escapes() {
        let schema = json!({
            "$defs": {"tilde~field": {"type": "integer"}, "slash/field": {"type": "integer"},
                      "percent%field": {"type": "integer"}, "a b": {"type": "integer"}},
            "properties": {
                "tilde": {"$ref": "#/$defs/tilde~0field"},
                "slash": {"$ref": "#/$defs/slash~1field"},
                "percent": {"$ref": "#/$defs/percent%25field"},
                "space": {"$ref": "#/$defs/a%20b"}
            }
        });
        for key in ["tilde", "slash", "percent", "space"] {
            let findings = validate(&json!({ key: "x" }), &schema);
            assert_eq!(findings.len(), 1, "{key}: {findings:?}");
            assert_eq!(findings[0].severity, Severity::Error, "{key}");
            assert_eq!(validate(&json!({ key: 1 }), &schema), [], "{key}");
        }
    }

    #[test]
    fn distinct_large_integers_hash_apart() {
        let near_max = |i: u64| Value::from(u64::MAX - i);
        assert_ne!(hash_value(&near_max(0)), hash_value(&near_max(1)));
        assert_eq!(hash_value(&json!(3)), hash_value(&json!(3.0)));
        assert_eq!(hash_value(&json!(-0.0)), hash_value(&json!(0)));
        let items = Value::Array((0..20_000).map(near_max).collect());
        assert_eq!(validate(&items, &json!({"uniqueItems": true})), []);
    }

    /// Ported from the official JSON Schema test suite (`unevaluatedProperties`
    /// and `unevaluatedItems`, the "nested unevaluated..." cases).
    #[test]
    fn nested_unevaluated_keywords_mark_what_they_accept() {
        let cases = [
            json!({"type": "object", "properties": {"foo": {"type": "string"}},
                "allOf": [{"unevaluatedProperties": true}],
                "unevaluatedProperties": {"type": "string", "maxLength": 2}}),
            json!({"type": "object", "properties": {"foo": {"type": "string"}},
                "allOf": [{"unevaluatedProperties": true}], "unevaluatedProperties": false}),
            json!({"type": "object",
                "allOf": [{"properties": {"foo": {"type": "string"}}, "unevaluatedProperties": true}],
                "unevaluatedProperties": false}),
        ];
        for schema in &cases {
            assert!(passes(r#"{"foo":"foo"}"#, schema), "{schema}");
            assert!(passes(r#"{"foo":"foo","bar":"bar"}"#, schema), "{schema}");
        }
        let items = json!({"allOf": [{"prefixItems": [{"type": "string"}]}, {"unevaluatedItems": true}],
            "unevaluatedItems": false});
        assert!(passes(r#"["foo"]"#, &items));
        assert!(passes(r#"["foo", 42, true]"#, &items));
        // The inner keyword rejecting is still a failure.
        let inner_false = json!({"properties": {"foo": true},
            "allOf": [{"unevaluatedProperties": false}], "unevaluatedProperties": true});
        assert!(fails(r#"{"foo":"foo"}"#, &inner_false));
    }

    #[test]
    fn multiple_of_survives_a_quotient_that_overflows() {
        let schema = json!({"type": "integer", "multipleOf": 0.123456789});
        assert!(fails("1e308", &schema));
        assert!(passes("1e308", &json!({"multipleOf": 1e300})));
        assert!(passes("7.5", &json!({"multipleOf": 2.5})));
    }

    #[test]
    fn validates_a_clean_document() {
        let doc = json!({
            "nome": "Carlos Souza",
            "idade": 28,
            "ativo": true,
            "historico": null,
            "telefones": ["(11) 99999-1111"],
            "configuracoes": {"tema": "escuro"}
        });
        let schema = json!({
            "type": "object",
            "required": ["nome", "idade", "ativo"],
            "properties": {
                "nome": {"type": "string", "minLength": 3},
                "idade": {"type": "integer", "minimum": 0},
                "ativo": {"type": "boolean"},
                "historico": {"type": ["null", "string"]},
                "telefones": {"type": "array", "items": {"type": "string"}},
                "configuracoes": {"type": "object"}
            }
        });
        assert!(!has_errors(&validate(&doc, &schema)));
    }

    #[test]
    fn catches_missing_required_and_bad_type() {
        let doc = json!({"idade": "x"});
        let schema = json!({
            "type": "object",
            "required": ["nome"],
            "properties": {"idade": {"type": "integer"}}
        });
        let findings = validate(&doc, &schema);
        assert!(has_errors(&findings));
    }

    #[test]
    fn boolean_schemas_assert() {
        assert!(passes("1", &json!(true)));
        assert!(fails("1", &json!(false)));
    }

    #[test]
    fn const_must_deep_equal() {
        assert!(passes(r#"{"a":1}"#, &json!({"const": {"a": 1}})));
        assert!(fails(r#"{"a":2}"#, &json!({"const": {"a": 1}})));
    }

    #[test]
    fn integer_type_accepts_whole_float() {
        assert!(passes("1.0", &json!({"type": "integer"})));
        assert!(fails("1.5", &json!({"type": "integer"})));
    }

    #[test]
    fn multiple_of_with_float_divisor() {
        assert!(passes("3.1", &json!({"multipleOf": 0.1})));
        assert!(fails("3.15", &json!({"multipleOf": 0.1})));
        assert!(fails("10", &json!({"multipleOf": 3})));
    }

    #[test]
    fn exclusive_bounds_numeric_and_openapi_boolean_forms() {
        assert!(passes("5", &json!({"exclusiveMinimum": 0})));
        assert!(fails("0", &json!({"exclusiveMinimum": 0})));
        assert!(fails("5", &json!({"exclusiveMaximum": 5})));
        assert!(fails("0", &json!({"minimum": 0, "exclusiveMinimum": true})));
        assert!(passes(
            "1",
            &json!({"minimum": 0, "exclusiveMinimum": true})
        ));
        assert!(passes(
            "0",
            &json!({"minimum": 0, "exclusiveMinimum": false})
        ));
    }

    #[test]
    fn unique_items_and_budget_keywords() {
        assert!(fails("[1, 2, 1]", &json!({"uniqueItems": true})));
        // Duplicates are only an error when the schema asks for uniqueness.
        assert!(passes("[1, 2, 1]", &json!({})));
        assert!(passes("[1, 2, 1]", &json!({"uniqueItems": false})));
        assert!(passes("[[1, 1]]", &json!({"items": {"type": "array"}})));
        assert!(passes("[1, 2, 3]", &json!({"uniqueItems": true})));
        assert!(fails("[1, 2]", &json!({"minItems": 3})));
        assert!(fails("[1, 2, 3, 4]", &json!({"maxItems": 3})));
    }

    #[test]
    fn unique_items_cross_type_numbers() {
        // 1 and 1.0 compare equal under JSON value equality.
        assert!(fails("[1, 1.0]", &json!({"uniqueItems": true})));
        // Distinct integers that collide in f64 are not duplicates.
        assert!(passes(
            "[9007199254740992, 9007199254740993]",
            &json!({"uniqueItems": true})
        ));
        // Strings and numbers never collide.
        assert!(passes("[1, \"1\"]", &json!({"uniqueItems": true})));
    }

    #[test]
    fn contains_with_min_and_max_contains() {
        let schema = json!({"contains": {"type": "integer"}});
        assert!(passes("[1, \"a\"]", &schema));
        assert!(fails("[\"a\", \"b\"]", &schema));
        let schema = json!({"contains": {"type": "integer"}, "minContains": 2});
        assert!(passes("[1, 2, \"a\"]", &schema));
        assert!(fails("[1, \"a\"]", &schema));
        let schema = json!({"contains": {"type": "integer"}, "maxContains": 1});
        assert!(fails("[1, 2]", &schema));
        assert!(passes("[1, \"a\"]", &schema));
        // minContains 0: contains itself always passes.
        let schema = json!({"contains": {"type": "integer"}, "minContains": 0});
        assert!(passes("[\"a\", \"b\"]", &schema));
        // Neither has an effect without `contains`.
        assert!(passes("[\"a\"]", &json!({"minContains": 1})));
    }

    #[test]
    fn item_tuple_form_and_additional_items() {
        let tuple = json!({"items": [{"type": "string"}, {"type": "integer"}]});
        assert!(passes("[\"a\", 1]", &tuple));
        assert!(fails("[1, 1]", &tuple));
        let closed = json!({"items": [{"type": "string"}], "additionalItems": false});
        assert!(fails("[\"a\", \"b\"]", &closed));
        let open = json!({"items": [{"type": "string"}], "additionalItems": {"type": "integer"}});
        assert!(passes("[\"a\", 1]", &open));
        assert!(fails("[\"a\", \"b\"]", &open));
    }

    #[test]
    fn prefix_items_then_single_items_schema() {
        let schema = json!({"prefixItems": [{"type": "integer"}], "items": {"type": "string"}});
        assert!(passes("[1, \"a\", \"b\"]", &schema));
        assert!(fails("[1, 2]", &schema));
        assert!(fails("[\"a\"]", &schema));
    }

    #[test]
    fn pattern_properties_and_additional_properties_schema() {
        let schema = json!({
            "patternProperties": {"^s_": {"type": "string"}},
            "additionalProperties": {"type": "integer"}
        });
        assert!(passes(r#"{"s_name": "x", "count": 1}"#, &schema));
        assert!(fails(r#"{"s_name": 1}"#, &schema));
        assert!(fails(r#"{"count": "x"}"#, &schema));
    }

    #[test]
    fn property_names_validate_each_key() {
        assert!(passes(
            r#"{"a": 1}"#,
            &json!({"propertyNames": {"maxLength": 1}})
        ));
        assert!(fails(
            r#"{"ab": 1}"#,
            &json!({"propertyNames": {"maxLength": 1}})
        ));
    }

    #[test]
    fn dependencies_both_forms() {
        let schema = json!({"dependencies": {"a": ["b"], "c": {"required": ["d"]}}});
        assert!(passes(r#"{"a": 1, "b": 2}"#, &schema));
        assert!(fails(r#"{"a": 1}"#, &schema));
        assert!(passes(r#"{"c": 1, "d": 2}"#, &schema));
        assert!(fails(r#"{"c": 1}"#, &schema));
    }

    #[test]
    fn dependent_required_and_dependent_schemas() {
        let schema = json!({
            "dependentRequired": {"cc": ["billing"]},
            "dependentSchemas": {"cc": {"properties": {"amount": {"type": "integer"}}}}
        });
        assert!(fails(r#"{"cc": 1}"#, &schema));
        assert!(passes(r#"{"cc": 1, "billing": 2, "amount": 3}"#, &schema));
        assert!(fails(r#"{"cc": 1, "billing": 2, "amount": "x"}"#, &schema));
    }

    #[test]
    fn all_of_any_of_one_of_not() {
        let all = json!({"allOf": [{"type": "number"}, {"minimum": 3}]});
        assert!(passes("4", &all));
        assert!(fails("2", &all));
        let any = json!({"anyOf": [{"type": "string"}, {"type": "boolean"}]});
        assert!(passes("\"x\"", &any));
        assert!(passes("false", &any));
        assert!(fails("1", &any));
        let one = json!({"oneOf": [{"type": "number"}, {"maximum": 10}]});
        assert!(passes("20", &one));
        assert!(fails("5", &one));
        // {"maximum": 10} does not constrain non-numbers, so "x" still
        // matches exactly one branch; a type-only oneOf rejects it.
        assert!(passes("\"x\"", &one));
        assert!(fails(
            "\"x\"",
            &json!({"oneOf": [{"type": "number"}, {"type": "boolean"}]})
        ));
        let not = json!({"not": {"type": "string"}});
        assert!(fails("\"x\"", &not));
        assert!(passes("1", &not));
    }

    #[test]
    fn if_then_else_choose_the_branch() {
        let schema = json!({
            "if": {"properties": {"kind": {"const": "a"}}},
            "then": {"properties": {"a_value": {"type": "string"}}},
            "else": {"properties": {"b_value": {"type": "integer"}}}
        });
        assert!(passes(r#"{"kind": "a", "a_value": "x"}"#, &schema));
        assert!(fails(r#"{"kind": "a", "a_value": 1}"#, &schema));
        assert!(passes(r#"{"kind": "b", "b_value": 1}"#, &schema));
    }

    #[test]
    fn local_ref_pointer_anchor_and_definitions() {
        let schema = json!({
            "definitions": {"positive": {"type": "integer", "minimum": 1}},
            "$defs": {
                "bound": {"exclusiveMaximum": 3, "$anchor": "bound-anchor"},
                "typed": {"$anchor": "root-bound", "type": "number"}
            },
            "allOf": [
                {"$ref": "#/definitions/positive"},
                {"$ref": "#/$defs/bound"},
                {"$ref": "#root-bound"},
                {"$ref": "#bound-anchor"}
            ]
        });
        assert!(passes("2", &schema));
        assert!(fails("-1", &schema));
        assert!(fails("3", &schema));
    }

    #[test]
    fn ref_cycle_is_capped_not_infinite() {
        let schema = json!({
            "$defs": {"loopA": {"$ref": "#/$defs/loopB"}, "loopB": {"$ref": "#/$defs/loopA"}},
            "$ref": "#/$defs/loopA"
        });
        let findings = validate(&json!(1), &schema);
        // A cycle that never reaches the document is reported, not skipped.
        assert_eq!(findings.len(), 1);
        assert!(has_errors(&findings));
    }

    #[test]
    fn unresolved_ref_is_a_warning() {
        let schema = json!({"$ref": "#/$defs/missing"});
        let findings = validate(&json!(1), &schema);
        assert!(findings.iter().any(|f| f.severity == Severity::Warning));
        assert!(!findings.iter().any(|f| f.severity == Severity::Error));
    }

    #[test]
    fn unevaluated_properties_see_through_applicators() {
        let schema = json!({
            "properties": {"bar": {"type": "integer"}},
            "anyOf": [{"properties": {"foo": {"type": "string"}}}],
            "unevaluatedProperties": false
        });
        assert!(passes(r#"{"bar": 1}"#, &schema));
        assert!(passes(r#"{"foo": "x"}"#, &schema));
        assert!(passes(r#"{"bar": 1, "foo": "x"}"#, &schema));
        assert!(fails(r#"{"baz": 1}"#, &schema));
    }

    #[test]
    fn failed_subschemas_do_not_evaluate() {
        let schema = json!({
            "anyOf": [
                {"properties": {"foo": {"type": "string"}}},
                {"properties": {"bar": {"type": "integer"}}}
            ],
            "unevaluatedProperties": false
        });
        // anyOf still collects annotations from both branches, so `bar` is
        // evaluated even when the first branch fails.
        assert!(passes(r#"{"bar": 1}"#, &schema));
        assert!(fails(r#"{"qux": 1}"#, &schema));
    }

    #[test]
    fn unevaluated_items_after_prefix_and_contains() {
        let schema = json!({
            "prefixItems": [{"type": "integer"}],
            "unevaluatedItems": {"type": "string"}
        });
        assert!(passes("[1, \"a\", \"b\"]", &schema));
        assert!(fails("[1, 2]", &schema));

        let schema = json!({
            "contains": {"type": "integer"},
            "unevaluatedItems": {"type": "string"}
        });
        assert!(passes("[1, \"a\"]", &schema));
        // Every integer matches `contains` and is evaluated; the boolean is
        // the only unevaluated index and fails the string check.
        assert!(passes("[3, 4]", &schema));
        assert!(fails("[1, 2, true]", &schema));
    }

    #[test]
    fn formats_assert_known_names_and_warn_on_unknown() {
        assert!(passes(
            r#""2026-09-25T18:00:00Z""#,
            &json!({"format": "date-time"})
        ));
        assert!(fails(r#""2026-13-01""#, &json!({"format": "date"})));
        assert!(passes(r#""user@example.com""#, &json!({"format": "email"})));
        assert!(fails(r#""not-an-email""#, &json!({"format": "email"})));
        assert!(passes(r#""192.168.0.1""#, &json!({"format": "ipv4"})));
        assert!(fails(r#""256.1.1.1""#, &json!({"format": "ipv4"})));
        assert!(passes(r#""::1""#, &json!({"format": "ipv6"})));
        assert!(fails(r#""1:2:3:4:5:6:7:8:9""#, &json!({"format": "ipv6"})));
        assert!(passes(
            r#""550e8400-e29b-41d4-a716-446655440000""#,
            &json!({"format": "uuid"})
        ));
        let unknown = validate(&json!("x"), &json!({"format": "made-up"}));
        assert!(unknown.iter().any(|f| f.severity == Severity::Warning));
        assert!(!unknown.iter().any(|f| f.severity == Severity::Error));
    }

    #[test]
    fn flags_unsupported_and_unknown_keywords() {
        let dynamic = validate(&json!(1), &json!({"$dynamicRef": "#recursive"}));
        assert!(dynamic.iter().any(|f| f.severity == Severity::Warning));
        let unknown = validate(&json!(1), &json!({"madeUpKeyword": 1}));
        assert!(unknown.iter().any(|f| f.severity == Severity::Warning));
    }

    #[test]
    fn content_and_metadata_keywords_are_ignored_silently() {
        let schema = json!({
            "title": "x",
            "description": "y",
            "contentEncoding": "base64",
            "contentMediaType": "application/json",
            "default": 1,
            "type": "number"
        });
        let findings = validate(&json!(2), &schema);
        assert!(findings.is_empty());
    }
}
