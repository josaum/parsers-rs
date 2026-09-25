//! Schema fidelity for `complexContent`/`extension` inheritance and `xs:choice`.
//!
//! The schema model must fold a base type's particles and attributes into the
//! derived type, base first, and must preserve `xs:choice` as a discriminated
//! union rather than flattening it into a sequence. Both are load-bearing for
//! any consumer deriving a typed projection from a schema: flattening a choice
//! turns mutually exclusive branches into co-occurring fields, and losing
//! inheritance drops inherited columns silently.
//!
//! Contributed by a consumer that carried these tests against a fork of this
//! crate. Re-homing them found a real divergence in this crate's favour: for a
//! required element (`<xs:element name="codigo" type="st_codigo"/>`, no
//! `minOccurs`, therefore required) the fork produced an Arrow field with
//! `nullable = true`, contradicting its own schema, while this crate produces
//! `nullable = false`. The snapshot tests below pin that.
//!
//! The three Arrow tests are `#[cfg(feature = "arrow")]` because `arrow` is
//! optional here; a consumer whose dev-dependency always enables it can drop
//! the gates, but upstream they must stay or the file will not compile with
//! default features.

use xsd_fast_core::schema::{ContentModel, ElementRef};
use xsd_fast_core::{ParseOptions, SchemaSet, ValidateOptions, ValidationSeverity};

// ── Helpers ────────────────────────────────────────────────────────────────

/// Local element names of every direct particle in a content model.
fn particle_names(content: &ContentModel) -> Vec<String> {
    let particles = match content {
        ContentModel::Sequence(p) | ContentModel::All(p) | ContentModel::Choice(p) => p,
        _ => return Vec::new(),
    };
    particles
        .iter()
        .filter_map(|particle| match &particle.element {
            ElementRef::Inline(decl) => Some(decl.name.local.clone()),
            ElementRef::Ref(qn) => Some(qn.local.clone()),
            ElementRef::Group(_) => None,
        })
        .collect()
}

/// Error-severity findings only.
fn errors_only(errs: &[xsd_fast_core::ValidationError]) -> Vec<&xsd_fast_core::ValidationError> {
    errs.iter()
        .filter(|e| e.severity == ValidationSeverity::Error)
        .collect()
}

// ── (A) Inheritance resolution ──────────────────────────────────────────────

const INHERIT_XSD: &[u8] = br#"<?xml version="1.0" encoding="UTF-8"?>
<xs:schema xmlns:xs="http://www.w3.org/2001/XMLSchema" targetNamespace="urn:test">
  <xs:complexType name="A">
    <xs:sequence>
      <xs:element name="a1" type="xs:string"/>
      <xs:element name="a2" type="xs:string"/>
    </xs:sequence>
    <xs:attribute name="aAttr" type="xs:string" use="required"/>
  </xs:complexType>

  <xs:complexType name="B">
    <xs:complexContent>
      <xs:extension base="A">
        <xs:sequence>
          <xs:element name="b1" type="xs:string"/>
        </xs:sequence>
      </xs:extension>
    </xs:complexContent>
    <xs:attribute name="bAttr" type="xs:string"/>
  </xs:complexType>

  <xs:element name="bRoot" type="B"/>
</xs:schema>
"#;

#[test]
fn extension_folds_base_particles_base_first() {
    let schema = SchemaSet::load_bytes(INHERIT_XSD, ParseOptions::default()).expect("parse");
    let model = schema.model();
    let b = model.complex_types.get("B").expect("type B");

    // base recorded for provenance.
    assert!(b.inherits(), "B should record its complex base");
    assert_eq!(b.base.as_ref().map(|q| q.local.as_str()), Some("A"));

    // Effective content = base particles (base-first) then B's own.
    let names = particle_names(b.effective_content());
    assert_eq!(
        names,
        vec!["a1", "a2", "b1"],
        "inherited A particles must precede B's own, in declaration order"
    );
}

#[test]
fn extension_folds_base_attributes_base_first() {
    let schema = SchemaSet::load_bytes(INHERIT_XSD, ParseOptions::default()).expect("parse");
    let b = schema.model().complex_types.get("B").expect("type B");
    let attr_names: Vec<&str> = b
        .effective_attributes()
        .iter()
        .map(|a| a.name.as_str())
        .collect();
    assert_eq!(
        attr_names,
        vec!["aAttr", "bAttr"],
        "inherited attribute must precede B's own attribute"
    );
}

#[test]
fn non_inheriting_type_is_left_untouched() {
    let schema = SchemaSet::load_bytes(INHERIT_XSD, ParseOptions::default()).expect("parse");
    let a = schema.model().complex_types.get("A").expect("type A");
    assert!(!a.inherits(), "A has no base");
    assert_eq!(particle_names(a.effective_content()), vec!["a1", "a2"]);
}

#[test]
fn transitive_inheritance_chain_resolves() {
    // C extends B extends A. Effective C = a1,a2,b1,c1 (base-first, transitive).
    let xsd = br#"<?xml version="1.0" encoding="UTF-8"?>
<xs:schema xmlns:xs="http://www.w3.org/2001/XMLSchema" targetNamespace="urn:test">
  <xs:complexType name="A">
    <xs:sequence><xs:element name="a1" type="xs:string"/></xs:sequence>
  </xs:complexType>
  <xs:complexType name="B">
    <xs:complexContent><xs:extension base="A">
      <xs:sequence><xs:element name="b1" type="xs:string"/></xs:sequence>
    </xs:extension></xs:complexContent>
  </xs:complexType>
  <xs:complexType name="C">
    <xs:complexContent><xs:extension base="B">
      <xs:sequence><xs:element name="c1" type="xs:string"/></xs:sequence>
    </xs:extension></xs:complexContent>
  </xs:complexType>
</xs:schema>
"#;
    let schema = SchemaSet::load_bytes(xsd, ParseOptions::default()).expect("parse");
    let c = schema.model().complex_types.get("C").expect("type C");
    assert_eq!(
        particle_names(c.effective_content()),
        vec!["a1", "b1", "c1"]
    );
}

#[test]
fn self_referential_base_does_not_loop() {
    // Pathological: a type extends itself. Must not hang; resolution is bounded.
    let xsd = br#"<?xml version="1.0" encoding="UTF-8"?>
<xs:schema xmlns:xs="http://www.w3.org/2001/XMLSchema" targetNamespace="urn:test">
  <xs:complexType name="Loop">
    <xs:complexContent><xs:extension base="Loop">
      <xs:sequence><xs:element name="x" type="xs:string"/></xs:sequence>
    </xs:extension></xs:complexContent>
  </xs:complexType>
</xs:schema>
"#;
    let schema = SchemaSet::load_bytes(xsd, ParseOptions::default()).expect("parse");
    let t = schema.model().complex_types.get("Loop").expect("type Loop");
    // The cycle is broken; only the local particle survives.
    assert_eq!(particle_names(t.effective_content()), vec!["x"]);
}

// ── (B) xs:choice preservation ──────────────────────────────────────────────

const CHOICE_XSD: &[u8] = br#"<?xml version="1.0" encoding="UTF-8"?>
<xs:schema xmlns:xs="http://www.w3.org/2001/XMLSchema" targetNamespace="urn:test"
           elementFormDefault="qualified">
  <xs:complexType name="ct_payload">
    <xs:choice>
      <xs:element name="alpha" type="xs:string"/>
      <xs:element name="beta" type="xs:string"/>
      <xs:element name="gamma" type="xs:string"/>
    </xs:choice>
  </xs:complexType>
  <xs:element name="payload" type="ct_payload"/>
</xs:schema>
"#;

#[test]
fn choice_is_preserved_not_flattened() {
    let schema = SchemaSet::load_bytes(CHOICE_XSD, ParseOptions::default()).expect("parse");
    let ct = schema.model().complex_types.get("ct_payload").expect("ct");
    assert!(
        matches!(ct.effective_content(), ContentModel::Choice(_)),
        "content must stay a Choice, got {:?}",
        ct.content
    );
    let branches = particle_names(ct.effective_content());
    assert_eq!(branches, vec!["alpha", "beta", "gamma"]);
}

#[test]
fn validator_accepts_any_single_choice_branch() {
    let schema = SchemaSet::load_bytes(CHOICE_XSD, ParseOptions::default()).expect("parse");
    for branch in ["alpha", "beta", "gamma"] {
        let xml = format!(
            "<payload xmlns=\"urn:test\"><{branch}>v</{branch}></payload>",
            branch = branch
        );
        let errs = schema
            .validate(xml.as_bytes(), ValidateOptions::default())
            .expect("validate");
        assert!(
            errors_only(&errs).is_empty(),
            "branch {branch} should validate, got: {:#?}",
            errors_only(&errs)
        );
    }
}

#[test]
fn nested_choice_in_sequence_becomes_group() {
    // A choice nested inside a sequence must survive as a Group particle, not be
    // flattened into the parent sequence (the parser.rs ~489-492 fix).
    let xsd = br#"<?xml version="1.0" encoding="UTF-8"?>
<xs:schema xmlns:xs="http://www.w3.org/2001/XMLSchema" targetNamespace="urn:test"
           elementFormDefault="qualified">
  <xs:complexType name="ct_mixed">
    <xs:sequence>
      <xs:element name="header" type="xs:string"/>
      <xs:choice>
        <xs:element name="byCode" type="xs:string"/>
        <xs:element name="byName" type="xs:string"/>
      </xs:choice>
    </xs:sequence>
  </xs:complexType>
  <xs:element name="mixed" type="ct_mixed"/>
</xs:schema>
"#;
    let schema = SchemaSet::load_bytes(xsd, ParseOptions::default()).expect("parse");
    let ct = schema.model().complex_types.get("ct_mixed").expect("ct");
    let particles = match ct.effective_content() {
        ContentModel::Sequence(p) => p,
        other => panic!("expected Sequence, got {other:?}"),
    };
    // header element + one Group particle wrapping the nested choice.
    assert_eq!(particles.len(), 2, "header + choice group");
    let group = particles
        .iter()
        .find_map(|p| match &p.element {
            ElementRef::Group(g) => Some(g.as_ref()),
            _ => None,
        })
        .expect("nested choice must be preserved as a Group, not flattened");
    assert!(
        matches!(group, ContentModel::Choice(_)),
        "the group must be a Choice"
    );
    assert_eq!(particle_names(group), vec!["byCode", "byName"]);

    // Validator accepts either branch.
    for branch in ["byCode", "byName"] {
        let xml = format!(
            "<mixed xmlns=\"urn:test\"><header>h</header><{b}>v</{b}></mixed>",
            b = branch
        );
        let errs = schema
            .validate(xml.as_bytes(), ValidateOptions::default())
            .expect("validate");
        assert!(
            errors_only(&errs).is_empty(),
            "nested choice branch {branch} should validate, got {:#?}",
            errors_only(&errs)
        );
    }
}

// ── Backward-compatibility snapshot ─────────────────────────────────────────

const SNAPSHOT_XSD: &[u8] = br#"<?xml version="1.0" encoding="UTF-8"?>
<xs:schema xmlns:xs="http://www.w3.org/2001/XMLSchema"
           targetNamespace="urn:test" elementFormDefault="qualified">
  <xs:simpleType name="st_codigo">
    <xs:restriction base="xs:string">
      <xs:minLength value="1"/><xs:maxLength value="10"/>
    </xs:restriction>
  </xs:simpleType>
  <xs:complexType name="ct_plain">
    <xs:sequence>
      <xs:element name="codigo" type="st_codigo"/>
      <xs:element name="nascimento" type="xs:date" minOccurs="0"/>
      <xs:element name="valor" type="xs:decimal" minOccurs="0"/>
    </xs:sequence>
    <xs:attribute name="registroANS" type="xs:string" use="required"/>
  </xs:complexType>
  <xs:element name="plain" type="ct_plain"/>
</xs:schema>
"#;

#[cfg(feature = "arrow")]
#[test]
fn backward_compat_non_inheriting_arrow_snapshot() {
    use xsd_fast_core::arrow::ArrowOptions;

    let schema = SchemaSet::load_bytes(SNAPSHOT_XSD, ParseOptions::default()).expect("parse");
    let arrow = schema
        .to_arrow_schema("plain", ArrowOptions::default())
        .expect("arrow");

    // Exact field list + types + nullability for a type that neither inherits
    // nor uses a choice. This pins the byte-for-byte output against the changes.
    let snapshot: Vec<(String, String, bool)> = arrow
        .fields()
        .iter()
        .map(|f| {
            (
                f.name().clone(),
                format!("{:?}", f.data_type()),
                f.is_nullable(),
            )
        })
        .collect();

    // Nullability follows the schema, not the writer's convenience:
    //   @registroANS  use="required"   -> not nullable
    //   codigo        no minOccurs, so minOccurs="1" -> NOT nullable
    //   nascimento    minOccurs="0"    -> nullable
    //   valor         minOccurs="0"    -> nullable
    //
    // `codigo` is the one that moved. The deleted fork emitted it nullable, which
    // contradicted its own schema; the sovereign crate emits it non-nullable. That
    // is a fix, not a regression, and it is stricter on the shredder surface: an
    // Arrow consumer that fed a guia with no `codigo` through the fork got a null
    // column and now gets a schema that forbids one.
    let expected: Vec<(String, String, bool)> = vec![
        ("@registroANS".into(), "Utf8".into(), false),
        ("codigo".into(), "Utf8".into(), false),
        ("nascimento".into(), "Date32".into(), true),
        ("valor".into(), "Float64".into(), true),
    ];
    assert_eq!(
        snapshot, expected,
        "arrow schema for a non-inheriting, non-choice type must be unchanged"
    );
}

#[cfg(feature = "arrow")]
#[test]
fn backward_compat_description_batch_unchanged() {
    use xsd_fast_core::arrow::ArrowOptions;

    let schema = SchemaSet::load_bytes(SNAPSHOT_XSD, ParseOptions::default()).expect("parse");
    let batch = schema
        .to_schema_description_batch("plain", ArrowOptions::default())
        .expect("describe");
    assert_eq!(batch.num_columns(), 3);
    assert_eq!(batch.num_rows(), 4, "one row per leaf field/attribute");
}
