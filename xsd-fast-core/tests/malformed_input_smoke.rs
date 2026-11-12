//! Malformed schemas and instances must be rejected with an error.
//!
//! No input may panic, overflow the stack, allocate without bound, or keep the
//! parser busy without consuming input. Every entry point that reads bytes
//! from outside is covered: in-memory and on-disk schema loading (with
//! `xs:include` / `xs:import`), instance validation, and Arrow derivation.

#[path = "../../tools/mutation_smoke.rs"]
mod smoke;

use std::fs;
use std::path::Path;

use xsd_fast_core::{ParseOptions, SchemaSet, ValidateOptions};

#[global_allocator]
static ALLOCATOR: smoke::CappedAllocator = smoke::CappedAllocator;

const XS: &str = "http://www.w3.org/2001/XMLSchema";

/// A schema that uses every construct the parser reads or skips.
fn schema_seed() -> Vec<u8> {
    br#"<?xml version="1.0" encoding="UTF-8"?>
<xs:schema xmlns:xs="http://www.w3.org/2001/XMLSchema" xmlns:t="urn:test"
           targetNamespace="urn:test" elementFormDefault="qualified">
  <xs:annotation><xs:documentation>Orders &amp; parties</xs:documentation></xs:annotation>
  <xs:simpleType name="Code">
    <xs:restriction base="xs:string">
      <xs:minLength value="1"/><xs:maxLength value="10"/><xs:pattern value="[A-Z0-9]+"/>
    </xs:restriction>
  </xs:simpleType>
  <xs:simpleType name="Status">
    <xs:restriction base="xs:string">
      <xs:enumeration value="OPEN"/><xs:enumeration value="CLOSED"/>
    </xs:restriction>
  </xs:simpleType>
  <xs:simpleType name="Amount">
    <xs:restriction base="xs:decimal">
      <xs:minInclusive value="0"/><xs:maxInclusive value="99999.99"/>
      <xs:totalDigits value="7"/><xs:fractionDigits value="2"/>
    </xs:restriction>
  </xs:simpleType>
  <xs:simpleType name="Codes"><xs:list itemType="t:Code"/></xs:simpleType>
  <xs:simpleType name="CodeOrStatus"><xs:union memberTypes="t:Code t:Status"/></xs:simpleType>
  <xs:attributeGroup name="Audit">
    <xs:attribute name="createdBy" type="xs:string"/>
    <xs:attribute name="revision" type="xs:int" default="1"/>
  </xs:attributeGroup>
  <xs:group name="Contact">
    <xs:sequence>
      <xs:element name="email" type="xs:string" minOccurs="0"/>
      <xs:element name="phone" type="xs:string" minOccurs="0" maxOccurs="3"/>
    </xs:sequence>
  </xs:group>
  <xs:complexType name="Party">
    <xs:sequence>
      <xs:element name="name" type="xs:string"/>
      <xs:group ref="t:Contact"/>
    </xs:sequence>
    <xs:attribute name="id" type="xs:ID" use="required"/>
    <xs:attributeGroup ref="t:Audit"/>
  </xs:complexType>
  <xs:complexType name="Company">
    <xs:complexContent>
      <xs:extension base="t:Party">
        <xs:sequence>
          <xs:element name="taxId" type="t:Code"/>
          <xs:choice minOccurs="0">
            <xs:element name="branch" type="xs:string"/>
            <xs:element name="subsidiary" type="t:Company"/>
          </xs:choice>
        </xs:sequence>
        <xs:attribute name="country" type="xs:string" fixed="BR"/>
      </xs:extension>
    </xs:complexContent>
  </xs:complexType>
  <xs:complexType name="Restricted">
    <xs:complexContent>
      <xs:restriction base="t:Party">
        <xs:sequence><xs:element name="name" type="xs:string"/></xs:sequence>
      </xs:restriction>
    </xs:complexContent>
  </xs:complexType>
  <xs:complexType name="Money">
    <xs:simpleContent>
      <xs:extension base="xs:decimal">
        <xs:attribute name="currency" type="xs:string" use="required"/>
      </xs:extension>
    </xs:simpleContent>
  </xs:complexType>
  <xs:complexType name="Tree">
    <xs:sequence>
      <xs:element name="label" type="xs:string"/>
      <xs:element name="node" type="t:Tree" minOccurs="0" maxOccurs="unbounded"/>
    </xs:sequence>
  </xs:complexType>
  <xs:element name="note" type="xs:string" nillable="true"/>
  <xs:element name="order">
    <xs:complexType>
      <xs:sequence>
        <xs:element name="code" type="t:Code"/>
        <xs:element name="status" type="t:Status"/>
        <xs:element name="total" type="t:Money"/>
        <xs:element name="amount" type="t:Amount" minOccurs="0"/>
        <xs:element name="buyer" type="t:Company"/>
        <xs:element name="placed" type="xs:dateTime" minOccurs="0"/>
        <xs:element ref="t:note" minOccurs="0" maxOccurs="unbounded"/>
        <xs:element name="tree" type="t:Tree" minOccurs="0"/>
        <xs:element name="line" maxOccurs="unbounded">
          <xs:complexType>
            <xs:all>
              <xs:element name="sku" type="t:Code"/>
              <xs:element name="quantity" type="xs:positiveInteger"/>
              <xs:element name="options" minOccurs="0">
                <xs:complexType>
                  <xs:choice maxOccurs="unbounded">
                    <xs:element name="color" type="xs:string"/>
                    <xs:element name="size">
                      <xs:simpleType>
                        <xs:restriction base="xs:int">
                          <xs:minInclusive value="1"/><xs:maxInclusive value="60"/>
                        </xs:restriction>
                      </xs:simpleType>
                    </xs:element>
                  </xs:choice>
                </xs:complexType>
              </xs:element>
            </xs:all>
            <xs:attribute name="position" type="xs:int" use="required"/>
          </xs:complexType>
        </xs:element>
      </xs:sequence>
      <xs:attribute name="version" type="xs:string"/>
    </xs:complexType>
  </xs:element>
</xs:schema>
"#
    .to_vec()
}

/// An instance that is valid against [`schema_seed`].
fn instance_seed() -> Vec<u8> {
    br#"<?xml version="1.0" encoding="UTF-8"?>
<t:order xmlns:t="urn:test" version="1">
  <t:code>AB12</t:code>
  <t:status>OPEN</t:status>
  <t:total currency="BRL">1234.50</t:total>
  <t:amount>10.5</t:amount>
  <t:buyer id="p1" country="BR">
    <t:name>Acme &amp; Sons</t:name>
    <t:email>a@example.org</t:email>
    <t:taxId>X1</t:taxId>
    <t:subsidiary id="p2"><t:name>Sub</t:name><t:taxId>X2</t:taxId><t:branch>South</t:branch></t:subsidiary>
  </t:buyer>
  <t:placed>2026-01-02T03:04:05Z</t:placed>
  <t:note>first</t:note><t:note/>
  <t:tree><t:label>a</t:label><t:node><t:label>b</t:label><t:node><t:label>c</t:label></t:node></t:node></t:tree>
  <t:line position="1">
    <t:sku>SKU1</t:sku><t:quantity>2</t:quantity>
    <t:options><t:color>red</t:color><t:size>42</t:size></t:options>
  </t:line>
  <!-- trailing comment --><![CDATA[ignored]]>
</t:order>
"#
    .to_vec()
}

/// Runs every consumer of a loaded schema: validation and Arrow derivation.
fn exercise(schema: &SchemaSet, instance: &[u8]) {
    let _ = schema.validate(instance, ValidateOptions::default());
    let _ = schema.validate(
        instance,
        ValidateOptions {
            max_errors: 3,
            check_facets: false,
            check_cardinality: true,
        },
    );
    derive_arrow(schema);
}

#[cfg(feature = "arrow")]
fn derive_arrow(schema: &SchemaSet) {
    use xsd_fast_core::arrow::ArrowOptions;

    // `write_schema_ipc` runs the description batch, which runs the schema
    // derivation, so one call per element covers all three entry points.
    let names: Vec<String> = schema.model().global_elements.keys().cloned().collect();
    for name in names.iter().take(3) {
        let mut sink = Vec::new();
        let _ = schema.write_schema_ipc(name, ArrowOptions::default(), &mut sink);
    }
    let deep = ArrowOptions {
        max_depth: usize::MAX,
        include_attributes: false,
        ..ArrowOptions::default()
    };
    let _ = schema.all_arrow_schemas(deep);
    let _ = schema.to_arrow_schema("missing", ArrowOptions::default());
}

#[cfg(not(feature = "arrow"))]
fn derive_arrow(_schema: &SchemaSet) {}

fn load(bytes: &[u8]) -> anyhow::Result<SchemaSet> {
    SchemaSet::load_bytes(bytes, ParseOptions::default())
}

/// Wraps `body` in an `xs:schema` element.
fn schema_document(body: &str) -> Vec<u8> {
    format!("<xs:schema xmlns:xs=\"{XS}\">{body}</xs:schema>").into_bytes()
}

fn write(dir: &Path, name: &str, body: &[u8]) {
    fs::write(dir.join(name), body).expect("write fixture");
}

// ── Mutation runs ────────────────────────────────────────────────────────────

#[test]
fn mutated_schemas_never_panic() {
    let instance = instance_seed();
    smoke::assert_no_panics("xsd-fast-core schema", &[schema_seed()], 4_000, |bytes| {
        if let Ok(schema) = load(bytes) {
            exercise(&schema, &instance);
            let _ = serde_json::to_string(schema.model());
        }
    });
}

#[test]
fn mutated_instances_never_panic() {
    let schema = load(&schema_seed()).expect("seed schema loads");
    let clean = schema
        .validate(&instance_seed(), ValidateOptions::default())
        .expect("seed instance validates");
    assert!(clean.is_empty(), "seed instance must be valid: {clean:?}");
    smoke::assert_no_panics(
        "xsd-fast-core instance",
        &[instance_seed()],
        20_000,
        |bytes| {
            let _ = schema.validate(bytes, ValidateOptions::default());
            let _ = schema.validate(
                bytes,
                ValidateOptions {
                    max_errors: 0,
                    check_facets: true,
                    check_cardinality: true,
                },
            );
        },
    );
}

#[test]
fn mutated_schema_files_with_includes_never_panic() {
    let dir = tempfile::tempdir().expect("tempdir");
    write(dir.path(), "types.xsd", &schema_seed());
    write(
        dir.path(),
        "self.xsd",
        &schema_document("<xs:include schemaLocation=\"self.xsd\"/>"),
    );
    let root = schema_document(concat!(
        "<xs:include schemaLocation=\"types.xsd\"/>",
        "<xs:import namespace=\"urn:other\" schemaLocation=\"self.xsd\"/>",
        "<xs:include schemaLocation=\"root.xsd\"/>",
        "<xs:element name=\"extra\" type=\"Tree\"/>",
    ));
    let instance = instance_seed();
    let root_path = dir.path().join("root.xsd");
    smoke::assert_no_panics("xsd-fast-core include", &[root], 1_500, |bytes| {
        fs::write(&root_path, bytes).expect("write mutated root");
        if let Ok(schema) = SchemaSet::load(&root_path, ParseOptions::default()) {
            exercise(&schema, &instance);
        }
    });
}

// ── Includes and imports ─────────────────────────────────────────────────────

#[test]
fn self_and_mutually_including_schemas_terminate() {
    let dir = tempfile::tempdir().expect("tempdir");
    write(
        dir.path(),
        "a.xsd",
        &schema_document(concat!(
            "<xs:include schemaLocation=\"a.xsd\"/>",
            "<xs:include schemaLocation=\"b.xsd\"/>",
            "<xs:element name=\"a\" type=\"xs:string\"/>",
        )),
    );
    write(
        dir.path(),
        "b.xsd",
        &schema_document(concat!(
            "<xs:import schemaLocation=\"a.xsd\"/>",
            "<xs:include schemaLocation=\"./b.xsd\"/>",
            "<xs:element name=\"b\" type=\"xs:string\"/>",
        )),
    );
    let schema = SchemaSet::load(dir.path().join("a.xsd"), ParseOptions::default())
        .expect("cyclic includes load once each");
    assert!(schema.model().global_elements.contains_key("a"));
    assert!(schema.model().global_elements.contains_key("b"));
}

#[test]
fn schema_locations_cannot_leave_the_schema_directory() {
    let outer = tempfile::tempdir().expect("tempdir");
    let root_dir = outer.path().join("upload");
    fs::create_dir(&root_dir).expect("mkdir");
    write(
        outer.path(),
        "secret.xsd",
        &schema_document("<xs:element name=\"secret\" type=\"xs:string\"/>"),
    );
    let secret = outer
        .path()
        .join("secret.xsd")
        .canonicalize()
        .expect("canonical secret path");
    let secret = secret.to_str().expect("utf-8 temp path");

    let mut locations = vec![
        "../secret.xsd".to_string(),
        "./../secret.xsd".to_string(),
        "sub/../../secret.xsd".to_string(),
        secret.to_string(),
        format!("file://{secret}"),
        "http://example.org/x.xsd".to_string(),
        "/etc/passwd".to_string(),
        "/dev/zero".to_string(),
        "..".to_string(),
        "\u{0}".to_string(),
    ];
    #[cfg(unix)]
    {
        std::os::unix::fs::symlink(outer.path().join("secret.xsd"), root_dir.join("link.xsd"))
            .expect("symlink");
        locations.push("link.xsd".to_string());
    }

    for location in locations {
        let escaped = location.replace('\u{0}', "&#0;");
        for tag in ["include", "import"] {
            let root = schema_document(&format!("<xs:{tag} schemaLocation=\"{escaped}\"/>"));
            write(&root_dir, "root.xsd", &root);
            let loaded = SchemaSet::load(root_dir.join("root.xsd"), ParseOptions::default());
            match loaded {
                Err(_) => {}
                Ok(schema) => assert!(
                    !schema.model().global_elements.contains_key("secret"),
                    "{tag} of {location:?} read a file outside the schema directory"
                ),
            }
            // An in-memory schema has no directory, so it follows nothing.
            assert!(
                load(&root).is_err(),
                "in-memory {tag} of {location:?} must be refused"
            );
        }
    }
}

#[test]
fn include_fan_out_and_oversized_locations_are_refused() {
    let dir = tempfile::tempdir().expect("tempdir");
    let mut body = String::new();
    for index in 0..200 {
        write(
            dir.path(),
            &format!("f{index}.xsd"),
            &schema_document(&format!(
                "<xs:element name=\"e{index}\" type=\"xs:string\"/>"
            )),
        );
        body.push_str(&format!("<xs:include schemaLocation=\"f{index}.xsd\"/>"));
    }
    write(dir.path(), "root.xsd", &schema_document(&body));
    let error = SchemaSet::load(dir.path().join("root.xsd"), ParseOptions::default())
        .err()
        .expect("200 includes exceed the default limit of 64");
    assert!(error.to_string().contains("schema_includes"), "{error}");

    let long = "a".repeat(1 << 20);
    write(
        dir.path(),
        "long.xsd",
        &schema_document(&format!("<xs:include schemaLocation=\"{long}\"/>")),
    );
    let error = SchemaSet::load(dir.path().join("long.xsd"), ParseOptions::default())
        .err()
        .expect("a 1 MiB schemaLocation exceeds the default limit");
    assert!(
        error.to_string().contains("schema_location_bytes"),
        "{error}"
    );
}

/// Writes `links` files, each including the next, ending in `last`.
fn write_include_chain(dir: &Path, links: usize, last: &[u8]) {
    for index in 0..links {
        let next = index + 1;
        write(
            dir,
            &format!("c{index}.xsd"),
            &schema_document(&format!("<xs:include schemaLocation=\"c{next}.xsd\"/>")),
        );
    }
    write(dir, &format!("c{links}.xsd"), last);
}

#[test]
fn include_chains_are_bounded_on_a_small_stack() {
    // The caller may raise `max_includes`; the nesting of one include inside
    // another must still not follow that number onto the stack.
    let dir = tempfile::tempdir().expect("tempdir");
    write_include_chain(dir.path(), 2_000, &schema_document(""));
    let root = dir.path().join("c0.xsd");
    smoke::run_on_small_stack(move || {
        let options = ParseOptions {
            max_includes: 1_000_000,
            ..ParseOptions::default()
        };
        let error = SchemaSet::load(&root, options)
            .err()
            .expect("a 2,000 file include chain is refused");
        assert!(
            error.to_string().contains("schema_include_nesting"),
            "{error}"
        );
    });
}

#[test]
fn the_deepest_accepted_schema_set_fits_a_small_stack() {
    // The longest include chain that is accepted, ending in a file nested as
    // deeply as one file may be: the worst case both limits allow together.
    let dir = tempfile::tempdir().expect("tempdir");
    let mut last = format!("<xs:schema xmlns:xs=\"{XS}\"><xs:complexType name=\"T\">").into_bytes();
    last.extend(smoke::nested(
        b"<xs:choice>",
        b"<xs:element name=\"leaf\" type=\"xs:string\"/>",
        b"</xs:choice>",
        MAX_NESTING,
    ));
    last.extend_from_slice(b"</xs:complexType><xs:element name=\"root\" type=\"T\"/></xs:schema>");
    write_include_chain(dir.path(), MAX_INCLUDE_CHAIN, &last);
    let root = dir.path().join("c0.xsd");
    smoke::run_on_small_stack(move || {
        let schema =
            SchemaSet::load(&root, ParseOptions::default()).expect("a schema set at both limits");
        exercise(&schema, b"<root><leaf>x</leaf></root>");
        let _ = serde_json::to_string(schema.model());
        let _ = format!("{:?}", schema.model());
        drop(schema.model().complex_types["T"].clone());
    });

    // One more file in the chain is refused.
    let dir = tempfile::tempdir().expect("tempdir");
    write_include_chain(dir.path(), MAX_INCLUDE_CHAIN + 1, &schema_document(""));
    let error = SchemaSet::load(dir.path().join("c0.xsd"), ParseOptions::default())
        .err()
        .expect("one include beyond the limit is refused");
    assert!(
        error.to_string().contains("schema_include_nesting"),
        "{error}"
    );
}

// ── Deep nesting ─────────────────────────────────────────────────────────────

const DEPTH: usize = 100_000;

/// The parser's nesting limit, restated so a change to it fails a test here.
const MAX_NESTING: usize = 64;
/// The parser's include-chain limit, restated for the same reason.
const MAX_INCLUDE_CHAIN: usize = 32;

/// A schema whose type `T` holds `depth` copies of `open` ... `close` around
/// one leaf element.
fn schema_nested_to(open: &str, close: &str, depth: usize) -> Vec<u8> {
    let mut text = format!("<xs:schema xmlns:xs=\"{XS}\"><xs:complexType name=\"T\">").into_bytes();
    text.extend(smoke::nested(
        open.as_bytes(),
        b"<xs:element name=\"leaf\" type=\"xs:string\"/>",
        close.as_bytes(),
        depth,
    ));
    text.extend_from_slice(b"</xs:complexType><xs:element name=\"e\" type=\"T\"/></xs:schema>");
    text
}

#[test]
fn nesting_up_to_the_limit_is_accepted_and_fits_a_small_stack() {
    // (open, close, parser levels per repetition, levels before the first).
    let shapes = [
        ("<xs:sequence>", "</xs:sequence>", 1, 0),
        ("<xs:choice>", "</xs:choice>", 1, 0),
        (
            "<xs:sequence><xs:element name=\"e\"><xs:complexType>",
            "</xs:complexType></xs:element></xs:sequence>",
            3,
            0,
        ),
        (
            "<xs:choice><xs:element name=\"e\"><xs:complexType><xs:complexContent>\
             <xs:extension base=\"T\">",
            "</xs:extension></xs:complexContent></xs:complexType></xs:element></xs:choice>",
            5,
            0,
        ),
    ];
    for (open, close, levels, offset) in shapes {
        let deepest = (MAX_NESTING - offset) / levels;
        let accepted = schema_nested_to(open, close, deepest);
        smoke::run_on_small_stack(move || {
            let schema = load(&accepted).expect("nesting at the limit is accepted");
            exercise(&schema, b"<e><e><e><leaf>x</leaf></e></e></e>");
            let _ = serde_json::to_string(schema.model());
            let _ = format!("{:?}", schema.model());
            drop(schema.model().complex_types["T"].clone());
        });

        let error = load(&schema_nested_to(open, close, deepest + 1))
            .err()
            .expect("nesting one past the limit is refused");
        assert!(
            error.to_string().contains("schema_nesting_depth"),
            "{error}"
        );
    }
}

fn nested_schema(prefix: &str, open: &str, body: &str, close: &str, suffix: &str) -> Vec<u8> {
    let mut text = format!("<xs:schema xmlns:xs=\"{XS}\">{prefix}").into_bytes();
    text.extend(smoke::nested(
        open.as_bytes(),
        body.as_bytes(),
        close.as_bytes(),
        DEPTH,
    ));
    text.extend_from_slice(suffix.as_bytes());
    text.extend_from_slice(b"</xs:schema>");
    text
}

#[test]
fn deeply_nested_model_groups_are_bounded_on_a_small_stack() {
    let element = "<xs:element name=\"leaf\" type=\"xs:string\"/>";
    for (open, close) in [
        ("<xs:sequence>", "</xs:sequence>"),
        ("<xs:choice>", "</xs:choice>"),
        ("<xs:sequence><xs:choice>", "</xs:choice></xs:sequence>"),
        ("<xs:all>", "</xs:all>"),
        ("<xs:group>", "</xs:group>"),
    ] {
        let named = nested_schema(
            "<xs:complexType name=\"T\">",
            open,
            element,
            close,
            "</xs:complexType><xs:element name=\"root\" type=\"T\"/>",
        );
        let inline = nested_schema(
            "<xs:element name=\"root\"><xs:complexType>",
            open,
            element,
            close,
            "</xs:complexType></xs:element>",
        );
        for document in [named, inline] {
            smoke::run_on_small_stack(move || {
                if let Ok(schema) = load(&document) {
                    exercise(&schema, b"<root><leaf>x</leaf></root>");
                    let _ = serde_json::to_string(schema.model());
                    let _ = format!("{:?}", schema.model());
                }
            });
        }
    }
}

#[test]
fn deeply_nested_inline_types_are_bounded_on_a_small_stack() {
    let cases = [
        (
            "<xs:element name=\"e\"><xs:complexType><xs:sequence>",
            "</xs:sequence></xs:complexType></xs:element>",
        ),
        (
            "<xs:element name=\"e\"><xs:complexType><xs:choice>",
            "</xs:choice></xs:complexType></xs:element>",
        ),
        (
            "<xs:element name=\"e\"><xs:complexType><xs:complexContent><xs:extension base=\"B\"><xs:sequence>",
            "</xs:sequence></xs:extension></xs:complexContent></xs:complexType></xs:element>",
        ),
        ("<xs:element name=\"e\">", "</xs:element>"),
        ("<xs:complexType>", "</xs:complexType>"),
        ("<xs:simpleType><xs:restriction>", "</xs:restriction></xs:simpleType>"),
        ("<xs:annotation>", "</xs:annotation>"),
        ("<xs:unknown>", "</xs:unknown>"),
    ];
    for (open, close) in cases {
        let document = nested_schema(
            "<xs:complexType name=\"B\"><xs:sequence><xs:element name=\"b\" type=\"xs:int\"/></xs:sequence></xs:complexType>",
            open,
            "<xs:element name=\"leaf\" type=\"xs:string\"/>",
            close,
            "",
        );
        smoke::run_on_small_stack(move || {
            if let Ok(schema) = load(&document) {
                exercise(&schema, b"<e><e><e/></e></e>");
                let _ = serde_json::to_string(schema.model());
            }
        });
    }
}

#[test]
fn unclosed_deep_nesting_is_bounded_on_a_small_stack() {
    for open in [
        "<xs:sequence>",
        "<xs:choice>",
        "<xs:element name=\"e\"><xs:complexType><xs:sequence>",
        "<xs:complexType><xs:complexContent><xs:extension base=\"T\">",
    ] {
        let mut document =
            format!("<xs:schema xmlns:xs=\"{XS}\"><xs:complexType name=\"T\">").into_bytes();
        document.extend(smoke::nested(open.as_bytes(), b"", b"", DEPTH));
        smoke::run_on_small_stack(move || {
            if let Ok(schema) = load(&document) {
                exercise(&schema, b"<e/>");
            }
        });
    }
}

#[test]
fn deeply_nested_instances_are_bounded_on_a_small_stack() {
    let schema = std::sync::Arc::new(load(&schema_seed()).expect("seed schema loads"));
    let cases: [(&[u8], &[u8], &[u8]); 4] = [
        // Recursive type: every level resolves to `Tree` again.
        (b"<node><label>x</label>", b"", b"</node>"),
        // Unknown elements: no type on any level.
        (b"<a>", b"text", b"</a>"),
        // Unclosed: the document ends at the deepest level.
        (b"<node>", b"", b""),
        // Prefixed names and attributes on every level.
        (
            b"<t:node xmlns:t=\"urn:test\" a=\"1\">",
            b"<t:label/>",
            b"</t:node>",
        ),
    ];
    for (open, body, close) in cases {
        let mut document = b"<order><tree><label>root</label>".to_vec();
        document.extend(smoke::nested(open, body, close, DEPTH));
        document.extend_from_slice(b"</tree></order>");
        let schema = std::sync::Arc::clone(&schema);
        smoke::run_on_small_stack(move || {
            let errors = schema
                .validate(&document, ValidateOptions::default())
                .expect("validation reports findings, not a failure");
            assert!(errors.len() <= ValidateOptions::default().max_errors);
        });
    }
}

// ── Recursive and self-referential definitions ───────────────────────────────

#[test]
fn self_extending_and_mutually_recursive_definitions_terminate() {
    let extension = |name: &str, base: &str, child: &str| {
        format!(
            "<xs:complexType name=\"{name}\"><xs:complexContent><xs:extension base=\"{base}\">\
             <xs:sequence><xs:element name=\"{child}\" type=\"{name}\" minOccurs=\"0\"/>\
             </xs:sequence></xs:extension></xs:complexContent></xs:complexType>"
        )
    };
    let mut body = String::new();
    // A type that extends itself, a two-cycle, and a longer ring.
    body.push_str(&extension("Self", "Self", "self"));
    body.push_str(&extension("A", "B", "a"));
    body.push_str(&extension("B", "A", "b"));
    for index in 0..40 {
        let next = (index + 1) % 40;
        body.push_str(&extension(
            &format!("R{index}"),
            &format!("R{next}"),
            &format!("r{index}"),
        ));
    }
    // Groups that reference each other, and an element that references itself.
    body.push_str(concat!(
        "<xs:group name=\"G1\"><xs:sequence><xs:group ref=\"G2\"/></xs:sequence></xs:group>",
        "<xs:group name=\"G2\"><xs:choice><xs:group ref=\"G1\"/></xs:choice></xs:group>",
        "<xs:attributeGroup name=\"AG\"><xs:attributeGroup ref=\"AG\"/></xs:attributeGroup>",
        "<xs:complexType name=\"UsesGroups\"><xs:sequence><xs:group ref=\"G1\"/>",
        "<xs:element ref=\"loop\"/></xs:sequence><xs:attributeGroup ref=\"AG\"/></xs:complexType>",
        "<xs:element name=\"loop\"><xs:complexType><xs:sequence>",
        "<xs:element ref=\"loop\" minOccurs=\"0\"/></xs:sequence></xs:complexType></xs:element>",
        "<xs:element name=\"self\" type=\"Self\"/><xs:element name=\"a\" type=\"A\"/>",
        "<xs:element name=\"ring\" type=\"R0\"/><xs:element name=\"groups\" type=\"UsesGroups\"/>",
    ));
    let document = schema_document(&body);
    smoke::run_on_small_stack(move || {
        let schema = load(&document).expect("recursive definitions are legal XSD");
        for instance in [
            &b"<self><self><self/></self></self>"[..],
            b"<a><a><b><a/></b></a></a>",
            b"<ring><r0><r1/></r0></ring>",
            b"<loop><loop><loop/></loop></loop>",
            b"<groups><loop/></groups>",
        ] {
            exercise(&schema, instance);
        }
    });
}

#[test]
fn nested_extensions_cannot_multiply_the_schema() {
    // Every inline type extends the type that contains it. Folding the base
    // into each one copies the base, extensions included, so each level of
    // folding multiplies the model by the number of inline extensions.
    let inline = |name: &str| {
        format!(
            "<xs:element name=\"{name}\"><xs:complexType><xs:complexContent>\
             <xs:extension base=\"B\"></xs:extension></xs:complexContent>\
             </xs:complexType></xs:element>"
        )
    };
    let mut body = String::from("<xs:complexType name=\"B\"><xs:sequence>");
    for index in 0..6 {
        body.push_str(&inline(&format!("x{index}")));
    }
    body.push_str("</xs:sequence></xs:complexType><xs:element name=\"root\" type=\"B\"/>");
    let document = schema_document(&body);
    assert!(document.len() < 2_048);
    if let Ok(schema) = load(&document) {
        exercise(&schema, b"<root><x0><x1/></x0></root>");
    }

    // The same multiplication without a cycle: each type holds inline
    // extensions of the previous one.
    let mut body = String::from(
        "<xs:complexType name=\"T0\"><xs:sequence><xs:element name=\"leaf\" type=\"xs:int\"/>\
         </xs:sequence></xs:complexType>",
    );
    for level in 1..=15 {
        body.push_str(&format!("<xs:complexType name=\"T{level}\"><xs:sequence>"));
        for index in 0..6 {
            body.push_str(&format!(
                "<xs:element name=\"x{index}\"><xs:complexType><xs:complexContent>\
                 <xs:extension base=\"T{}\"></xs:extension></xs:complexContent>\
                 </xs:complexType></xs:element>",
                level - 1
            ));
        }
        body.push_str("</xs:sequence></xs:complexType>");
    }
    body.push_str("<xs:element name=\"root\" type=\"T15\"/>");
    let document = schema_document(&body);
    assert!(document.len() < 32 * 1024);
    if let Ok(schema) = load(&document) {
        exercise(&schema, b"<root><x0><x1/></x0></root>");
    }
}

#[test]
fn many_derived_types_cannot_multiply_a_large_base() {
    // 2,000 particles in the base and 2,000 types that extend it would hold
    // four million inherited particles if each were folded in full.
    let mut body = String::from("<xs:complexType name=\"B\"><xs:sequence>");
    for index in 0..2_000 {
        body.push_str(&format!("<xs:element name=\"e{index}\" type=\"xs:int\"/>"));
    }
    body.push_str("</xs:sequence></xs:complexType>");
    for index in 0..2_000 {
        body.push_str(&format!(
            "<xs:complexType name=\"D{index}\"><xs:complexContent><xs:extension base=\"B\">\
             </xs:extension></xs:complexContent></xs:complexType>"
        ));
    }
    let document = schema_document(&body);
    assert!(document.len() < 512 * 1024);
    let _ = load(&document);
}

#[cfg(feature = "arrow")]
#[test]
fn recursive_types_cannot_multiply_the_arrow_schema() {
    use xsd_fast_core::arrow::ArrowOptions;

    // Twelve children of the element's own type: flattening to the default
    // depth of 8 would emit 12^8 columns.
    let mut body = String::from("<xs:complexType name=\"T\"><xs:sequence>");
    for index in 0..12 {
        body.push_str(&format!(
            "<xs:element name=\"c{index}\" type=\"T\" minOccurs=\"0\"/>"
        ));
    }
    body.push_str("</xs:sequence></xs:complexType><xs:element name=\"root\" type=\"T\"/>");
    let document = schema_document(&body);
    assert!(document.len() < 1_024);
    let schema = load(&document).expect("a recursive type is legal XSD");
    let derived = schema.to_arrow_schema("root", ArrowOptions::default());
    assert!(
        derived.is_err(),
        "a recursive type must not flatten into {} columns",
        derived.map(|schema| schema.fields().len()).unwrap_or(0)
    );
    assert!(schema
        .to_schema_description_batch("root", ArrowOptions::default())
        .is_err());
    assert!(schema.all_arrow_schemas(ArrowOptions::default()).is_empty());

    // A caller-chosen depth far beyond the default must not reach the stack.
    let chain = schema_document(
        "<xs:complexType name=\"T\"><xs:sequence>\
         <xs:element name=\"next\" type=\"T\" minOccurs=\"0\"/></xs:sequence></xs:complexType>\
         <xs:element name=\"root\" type=\"T\"/>",
    );
    smoke::run_on_small_stack(move || {
        let schema = load(&chain).expect("a recursive type is legal XSD");
        let options = ArrowOptions {
            max_depth: usize::MAX,
            ..ArrowOptions::default()
        };
        let _ = schema.to_arrow_schema("root", options);
    });
}

// ── Validation output ────────────────────────────────────────────────────────

#[test]
fn validation_findings_are_bounded_by_max_errors() {
    // One element of a type with 50,000 required children: every missing
    // child is a finding, and each finding carries the element path.
    let mut body = String::from("<xs:complexType name=\"T\"><xs:sequence>");
    for index in 0..50_000 {
        body.push_str(&format!("<xs:element name=\"r{index}\" type=\"xs:int\"/>"));
    }
    body.push_str("</xs:sequence></xs:complexType><xs:element name=\"t\" type=\"T\"/>");
    let schema = load(&schema_document(&body)).expect("schema loads");
    for max_errors in [0, 1, 100] {
        for instance in [&b"<t/>"[..], b"<t></t>", b"<t><t/><u></u></t>"] {
            let errors = schema
                .validate(
                    instance,
                    ValidateOptions {
                        max_errors,
                        ..ValidateOptions::default()
                    },
                )
                .expect("validation reports findings");
            assert!(
                errors.len() <= max_errors,
                "{} findings for max_errors = {max_errors}",
                errors.len()
            );
        }
    }
}

#[test]
fn enumeration_findings_do_not_copy_the_schema_per_error() {
    // 100 invalid values against a 4 MiB enumeration: listing every allowed
    // value in every finding would hold 400 MiB of messages.
    let mut body = String::from("<xs:simpleType name=\"E\"><xs:restriction base=\"xs:string\">");
    let value = "v".repeat(4_000);
    for index in 0..1_000 {
        body.push_str(&format!("<xs:enumeration value=\"{value}{index}\"/>"));
    }
    body.push_str(
        "</xs:restriction></xs:simpleType><xs:element name=\"root\"><xs:complexType><xs:sequence>\
         <xs:element name=\"e\" type=\"E\" maxOccurs=\"unbounded\"/></xs:sequence></xs:complexType>\
         </xs:element>",
    );
    let schema = load(&schema_document(&body)).expect("schema loads");
    let mut instance = b"<root>".to_vec();
    for _ in 0..500 {
        instance.extend_from_slice(b"<e>nope</e>");
    }
    instance.extend_from_slice(b"</root>");
    let errors = schema
        .validate(&instance, ValidateOptions::default())
        .expect("validation reports findings");
    assert_eq!(errors.len(), 100);
    let total: usize = errors.iter().map(|error| error.message.len()).sum();
    assert!(
        total < 1 << 20,
        "100 findings hold {total} bytes of message text"
    );
}

#[test]
fn large_recursive_types_are_not_copied_per_instance_element() {
    // A recursive type with 2,000 particles, and 2,000 nested instance
    // elements of it. Copying the type for every open element would hold
    // 2,000 x 2,000 particles.
    let mut body = String::from(
        "<xs:complexType name=\"T\"><xs:sequence><xs:element name=\"t\" type=\"T\" minOccurs=\"0\"/>",
    );
    for index in 0..2_000 {
        body.push_str(&format!(
            "<xs:element name=\"o{index}\" type=\"xs:int\" minOccurs=\"0\"/>"
        ));
    }
    body.push_str("</xs:sequence></xs:complexType><xs:element name=\"t\" type=\"T\"/>");
    let schema = load(&schema_document(&body)).expect("schema loads");
    let instance = smoke::nested(b"<t>", b"", b"</t>", 2_000);
    let errors = schema
        .validate(&instance, ValidateOptions::default())
        .expect("validation reports findings");
    assert!(errors.is_empty(), "{:?}", errors.first());
}
