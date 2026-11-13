//! Sibling Arrow columns never share a name.
//!
//! One test per way two particles of a type reach the same local name, plus
//! a sweep over every schema the crate's tests hold inline (and the Node
//! binding's `note.xsd`) checking that no struct, and no top-level schema,
//! has two children of one name.

#![cfg(feature = "arrow")]

use std::path::{Path, PathBuf};
use std::sync::Arc;

use arrow_schema::{DataType, Field, Fields, Schema};
use xsd_fast_core::arrow::{ArrowOptions, ArrowShape};
use xsd_fast_core::{ParseOptions, SchemaSet};

fn derive(xsd: &str, root: &str) -> Arc<Schema> {
    let set = SchemaSet::load_bytes(xsd.as_bytes(), ParseOptions::default()).expect("schema loads");
    set.to_arrow_schema(root, ArrowOptions::default())
        .expect("schema derives")
}

fn schema(body: &str) -> String {
    format!(r#"<xs:schema xmlns:xs="http://www.w3.org/2001/XMLSchema">{body}</xs:schema>"#)
}

fn names(fields: &Fields) -> Vec<&str> {
    fields.iter().map(|f| f.name().as_str()).collect()
}

fn field<'a>(fields: &'a Fields, name: &str) -> &'a Field {
    fields
        .iter()
        .find(|f| f.name() == name)
        .unwrap_or_else(|| panic!("no column {name} in {:?}", names(fields)))
}

fn list_of(item: DataType, item_nullable: bool) -> DataType {
    DataType::List(Arc::new(Field::new("item", item, item_nullable)))
}

#[test]
fn an_element_repeated_in_a_sequence_is_one_list() {
    let s = derive(
        &schema(
            r#"<xs:element name="r"><xs:complexType><xs:sequence>
                 <xs:element name="a" type="xs:int"/>
                 <xs:element name="b" type="xs:string"/>
                 <xs:element name="a" type="xs:int" minOccurs="0"/>
               </xs:sequence></xs:complexType></xs:element>"#,
        ),
        "r",
    );
    assert_eq!(names(s.fields()), ["a", "b"]);
    let a = field(s.fields(), "a");
    assert_eq!(a.data_type(), &list_of(DataType::Int64, true));
    // The first `a` is required, so the list is always there.
    assert!(!a.is_nullable());
}

#[test]
fn a_repeated_particle_and_another_of_its_name_are_one_list() {
    let s = derive(
        &schema(
            r#"<xs:element name="r"><xs:complexType><xs:sequence>
                 <xs:element name="a" type="xs:int" maxOccurs="unbounded"/>
                 <xs:element name="a" type="xs:int"/>
               </xs:sequence></xs:complexType></xs:element>"#,
        ),
        "r",
    );
    assert_eq!(names(s.fields()), ["a"]);
    // Not a list of lists: the repeated particle contributes its items.
    assert_eq!(
        field(s.fields(), "a").data_type(),
        &list_of(DataType::Int64, false)
    );
}

#[test]
fn choice_branches_sharing_a_name_are_one_scalar() {
    let s = derive(
        &schema(
            r#"<xs:element name="r"><xs:complexType><xs:choice>
                 <xs:sequence>
                   <xs:element name="a" type="xs:int"/>
                   <xs:element name="b" type="xs:string"/>
                 </xs:sequence>
                 <xs:sequence>
                   <xs:element name="a" type="xs:int"/>
                   <xs:element name="c" type="xs:string"/>
                 </xs:sequence>
               </xs:choice></xs:complexType></xs:element>"#,
        ),
        "r",
    );
    assert_eq!(names(s.fields()), ["a", "b", "c"]);
    let a = field(s.fields(), "a");
    // One branch occurs, and each holds one `a`.
    assert_eq!(a.data_type(), &DataType::Int64);
    // Every branch requires `a`; `b` and `c` are in one branch each.
    assert!(!a.is_nullable());
    assert!(field(s.fields(), "b").is_nullable());
    assert!(field(s.fields(), "c").is_nullable());
}

#[test]
fn a_choice_branch_without_the_name_makes_its_column_nullable() {
    let s = derive(
        &schema(
            r#"<xs:element name="r"><xs:complexType><xs:choice>
                 <xs:sequence>
                   <xs:element name="a" type="xs:int"/>
                   <xs:element name="b" type="xs:string"/>
                 </xs:sequence>
                 <xs:sequence>
                   <xs:element name="a" type="xs:int"/>
                   <xs:element name="c" type="xs:string"/>
                 </xs:sequence>
                 <xs:element name="d" type="xs:string"/>
               </xs:choice></xs:complexType></xs:element>"#,
        ),
        "r",
    );
    assert_eq!(names(s.fields()), ["a", "b", "c", "d"]);
    let a = field(s.fields(), "a");
    assert_eq!(a.data_type(), &DataType::Int64);
    assert!(a.is_nullable());
}

#[test]
fn a_repeated_choice_merges_into_a_list() {
    let s = derive(
        &schema(
            r#"<xs:element name="r"><xs:complexType><xs:choice maxOccurs="unbounded">
                 <xs:sequence>
                   <xs:element name="a" type="xs:int"/>
                   <xs:element name="b" type="xs:string"/>
                 </xs:sequence>
                 <xs:sequence>
                   <xs:element name="a" type="xs:int"/>
                   <xs:element name="c" type="xs:string"/>
                 </xs:sequence>
               </xs:choice></xs:complexType></xs:element>"#,
        ),
        "r",
    );
    assert_eq!(names(s.fields()), ["a", "b", "c"]);
    assert_eq!(
        field(s.fields(), "a").data_type(),
        &list_of(DataType::Int64, false)
    );
}

#[test]
fn a_choice_branch_that_repeats_the_name_makes_a_list() {
    let s = derive(
        &schema(
            r#"<xs:element name="r"><xs:complexType><xs:choice>
                 <xs:element name="a" type="xs:int" maxOccurs="2"/>
                 <xs:sequence>
                   <xs:element name="a" type="xs:int"/>
                   <xs:element name="b" type="xs:string"/>
                 </xs:sequence>
               </xs:choice></xs:complexType></xs:element>"#,
        ),
        "r",
    );
    assert_eq!(names(s.fields()), ["a", "b"]);
    let a = field(s.fields(), "a");
    assert_eq!(a.data_type(), &list_of(DataType::Int64, false));
    assert!(!a.is_nullable());
}

#[test]
fn a_choice_branch_in_a_repeated_sequence_makes_a_list() {
    let s = derive(
        &schema(
            r#"<xs:element name="r"><xs:complexType><xs:choice>
                 <xs:sequence maxOccurs="unbounded">
                   <xs:element name="a" type="xs:int"/>
                   <xs:element name="b" type="xs:string"/>
                 </xs:sequence>
                 <xs:sequence>
                   <xs:element name="a" type="xs:int"/>
                   <xs:element name="c" type="xs:string"/>
                 </xs:sequence>
               </xs:choice></xs:complexType></xs:element>"#,
        ),
        "r",
    );
    assert_eq!(names(s.fields()), ["a", "b", "c"]);
    // The first branch may hold many `a`.
    assert!(matches!(
        field(s.fields(), "a").data_type(),
        DataType::List(_)
    ));
}

#[test]
fn columns_of_one_choice_branch_are_nullable() {
    // XML-DSig `PGPData`: a top-level choice whose first branch requires
    // `PGPKeyID`, which the second branch leaves out.
    let s = derive(
        &schema(
            r#"<xs:element name="PGPData"><xs:complexType><xs:choice>
                 <xs:sequence>
                   <xs:element name="PGPKeyID" type="xs:base64Binary"/>
                   <xs:element name="PGPKeyPacket" type="xs:base64Binary" minOccurs="0"/>
                 </xs:sequence>
                 <xs:sequence>
                   <xs:element name="PGPKeyPacket" type="xs:base64Binary"/>
                 </xs:sequence>
               </xs:choice></xs:complexType></xs:element>"#,
        ),
        "PGPData",
    );
    assert_eq!(names(s.fields()), ["PGPKeyID", "PGPKeyPacket"]);
    assert!(field(s.fields(), "PGPKeyID").is_nullable());
    let packet = field(s.fields(), "PGPKeyPacket");
    assert_eq!(packet.data_type(), &DataType::Utf8);
    assert!(packet.is_nullable());
}

#[test]
fn a_required_element_of_a_top_level_choice_is_nullable() {
    let s = derive(
        &schema(
            r#"<xs:element name="r"><xs:complexType><xs:choice>
                 <xs:element name="x" type="xs:string"/>
                 <xs:element name="y" type="xs:string"/>
               </xs:choice></xs:complexType></xs:element>"#,
        ),
        "r",
    );
    assert!(field(s.fields(), "x").is_nullable());
    assert!(field(s.fields(), "y").is_nullable());
}

#[test]
fn a_nested_choice_and_its_sequence_merge_into_a_list() {
    let s = derive(
        &schema(
            r#"<xs:element name="r"><xs:complexType><xs:sequence>
                 <xs:element name="a" type="xs:string"/>
                 <xs:choice>
                   <xs:element name="a" type="xs:string"/>
                   <xs:element name="b" type="xs:string"/>
                 </xs:choice>
               </xs:sequence></xs:complexType></xs:element>"#,
        ),
        "r",
    );
    assert_eq!(names(s.fields()), ["a", "b"]);
    assert_eq!(
        field(s.fields(), "a").data_type(),
        &list_of(DataType::Utf8, true)
    );
}

#[test]
fn an_inherited_element_and_a_local_one_of_its_name_are_one_list() {
    let s = derive(
        &schema(
            r#"<xs:complexType name="Base"><xs:sequence>
                 <xs:element name="a" type="xs:string"/>
               </xs:sequence></xs:complexType>
               <xs:complexType name="Derived"><xs:complexContent>
                 <xs:extension base="Base"><xs:sequence>
                   <xs:element name="a" type="xs:string"/>
                   <xs:element name="b" type="xs:string"/>
                 </xs:sequence></xs:extension>
               </xs:complexContent></xs:complexType>
               <xs:element name="r" type="Derived"/>"#,
        ),
        "r",
    );
    assert_eq!(names(s.fields()), ["a", "b"]);
    assert_eq!(
        field(s.fields(), "a").data_type(),
        &list_of(DataType::Utf8, false)
    );
}

#[test]
fn structs_of_one_shape_merge_up_to_nullability() {
    let s = derive(
        &schema(
            r#"<xs:element name="r"><xs:complexType><xs:sequence>
                 <xs:element name="p"><xs:complexType><xs:sequence>
                   <xs:element name="x" type="xs:string"/>
                   <xs:element name="y" type="xs:string"/>
                 </xs:sequence></xs:complexType></xs:element>
                 <xs:element name="p"><xs:complexType><xs:sequence>
                   <xs:element name="x" type="xs:string" minOccurs="0"/>
                   <xs:element name="y" type="xs:string"/>
                 </xs:sequence></xs:complexType></xs:element>
               </xs:sequence></xs:complexType></xs:element>"#,
        ),
        "r",
    );
    assert_eq!(names(s.fields()), ["p"]);
    let DataType::List(item) = field(s.fields(), "p").data_type() else {
        panic!("p is not a list");
    };
    let DataType::Struct(children) = item.data_type() else {
        panic!("p is not a list of structs");
    };
    assert_eq!(names(children), ["x", "y"]);
    assert!(field(children, "x").is_nullable());
    assert!(!field(children, "y").is_nullable());
}

#[test]
fn elements_of_different_types_are_renamed_by_ordinal() {
    let s = derive(
        &schema(
            r#"<xs:element name="r"><xs:complexType><xs:sequence>
                 <xs:element name="a" type="xs:int"/>
                 <xs:element name="a" type="xs:string"/>
                 <xs:element name="a" type="xs:boolean"/>
               </xs:sequence></xs:complexType></xs:element>"#,
        ),
        "r",
    );
    assert_eq!(names(s.fields()), ["a", "a#2", "a#3"]);
    assert_eq!(field(s.fields(), "a").data_type(), &DataType::Int64);
    assert_eq!(field(s.fields(), "a#2").data_type(), &DataType::Utf8);
    assert_eq!(field(s.fields(), "a#3").data_type(), &DataType::Boolean);
}

#[test]
fn a_rename_skips_a_name_a_nested_group_took() {
    let s = derive(
        &schema(
            r#"<xs:element name="r"><xs:complexType><xs:sequence>
                 <xs:element name="a" type="xs:boolean"/>
                 <xs:choice>
                   <xs:element name="a" type="xs:int"/>
                   <xs:element name="a" type="xs:string"/>
                 </xs:choice>
               </xs:sequence></xs:complexType></xs:element>"#,
        ),
        "r",
    );
    // The choice renames its string `a` to `a#2`; the sequence then renames
    // the choice's int `a`, which must not take `a#2` again.
    assert_eq!(names(s.fields()), ["a", "a#3", "a#2"]);
    assert_eq!(field(s.fields(), "a").data_type(), &DataType::Boolean);
    assert_eq!(field(s.fields(), "a#3").data_type(), &DataType::Int64);
    assert_eq!(field(s.fields(), "a#2").data_type(), &DataType::Utf8);
}

#[test]
fn merged_children_whose_column_is_a_list_make_a_list_of_lists() {
    let s = derive(
        &schema(
            r#"<xs:complexType name="C"><xs:sequence>
                 <xs:element name="x" type="xs:string" maxOccurs="unbounded"/>
               </xs:sequence></xs:complexType>
               <xs:element name="r"><xs:complexType><xs:sequence>
                 <xs:element name="c" type="C"/>
                 <xs:element name="c" type="C"/>
               </xs:sequence></xs:complexType></xs:element>"#,
        ),
        "r",
    );
    assert_eq!(names(s.fields()), ["c"]);
    // Each `c` is the list of its `x`; the two `c` are a list of them.
    assert_eq!(
        field(s.fields(), "c").data_type(),
        &list_of(list_of(DataType::Utf8, false), false)
    );
}

#[test]
fn renamed_columns_keep_their_positions() {
    let s = derive(
        &schema(
            r#"<xs:element name="r"><xs:complexType><xs:sequence>
                 <xs:element name="a" type="xs:int"/>
                 <xs:element name="b" type="xs:string"/>
                 <xs:element name="a" type="xs:string"/>
               </xs:sequence></xs:complexType></xs:element>"#,
        ),
        "r",
    );
    assert_eq!(names(s.fields()), ["a", "b", "a#2"]);
}

#[test]
fn a_rename_of_a_rename_numbers_from_the_base_name() {
    let s = derive(
        &schema(
            r#"<xs:element name="r"><xs:complexType><xs:sequence>
                 <xs:choice>
                   <xs:element name="a" type="xs:int"/>
                   <xs:element name="a" type="xs:string"/>
                 </xs:choice>
                 <xs:choice>
                   <xs:element name="a" type="xs:boolean"/>
                   <xs:element name="a" type="xs:date"/>
                 </xs:choice>
               </xs:sequence></xs:complexType></xs:element>"#,
        ),
        "r",
    );
    // Each choice renames its own second `a` to `a#2`; the sequence then
    // renames the second choice's two from `a`, not to `a#2#2`.
    assert_eq!(names(s.fields()), ["a", "a#2", "a#3", "a#4"]);
    assert_eq!(field(s.fields(), "a#3").data_type(), &DataType::Boolean);
    assert_eq!(field(s.fields(), "a#4").data_type(), &DataType::Date32);
}

#[test]
fn an_attribute_and_an_element_of_one_name_stay_apart() {
    let s = derive(
        &schema(
            r#"<xs:element name="r"><xs:complexType><xs:sequence>
                 <xs:element name="a" type="xs:int"/>
               </xs:sequence><xs:attribute name="a" type="xs:string"/>
               </xs:complexType></xs:element>"#,
        ),
        "r",
    );
    assert_eq!(names(s.fields()), ["@a", "a"]);
}

#[test]
fn a_child_whose_columns_merged_stays_a_struct() {
    let s = derive(
        &schema(
            r#"<xs:attribute name="ref" type="xs:string"/>
               <xs:element name="r"><xs:complexType><xs:sequence>
                 <xs:element name="c"><xs:complexType>
                   <xs:sequence>
                     <xs:element name="a" type="xs:string"/>
                     <xs:element name="a" type="xs:string"/>
                   </xs:sequence>
                   <xs:attribute ref="ref"/>
                 </xs:complexType></xs:element>
               </xs:sequence></xs:complexType></xs:element>"#,
        ),
        "r",
    );
    let DataType::Struct(children) = field(s.fields(), "c").data_type() else {
        panic!("c is not a struct");
    };
    // `c/a` and `c/@ref` both survive the merge of its two `a` columns.
    assert_eq!(names(children), ["@ref", "a"]);
    assert_eq!(
        field(children, "a").data_type(),
        &list_of(DataType::Utf8, false)
    );
}

#[test]
fn merged_columns_of_referenced_attributes_keep_a_simple_child_one_column() {
    // Each `m` derives only a referenced attribute, so neither is a column
    // of `c`'s own; merged, they are still not, and `c` stays its one
    // column of text by default, as it was before the merge.
    let s = derive(
        &schema(
            r#"<xs:attribute name="ref" type="xs:string"/>
               <xs:complexType name="M"><xs:attribute ref="ref"/></xs:complexType>
               <xs:element name="r"><xs:complexType><xs:sequence>
                 <xs:element name="c"><xs:complexType><xs:sequence>
                   <xs:element name="x" type="xs:string"/>
                   <xs:element name="m" type="M"/>
                   <xs:element name="m" type="M"/>
                 </xs:sequence></xs:complexType></xs:element>
               </xs:sequence></xs:complexType></xs:element>"#,
        ),
        "r",
    );
    assert_eq!(names(s.fields()), ["c"]);
    assert_eq!(field(s.fields(), "c").data_type(), &DataType::Utf8);
}

#[test]
fn a_referenced_attribute_column_merged_with_an_own_one_keeps_the_child_a_column() {
    // `M` derives only a referenced attribute, so the first `m` is not a
    // column of `c`'s own; the second is. Merged, `c` keeps its one column
    // as before, now a list of both.
    let s = derive(
        &schema(
            r#"<xs:attribute name="ref" type="xs:string"/>
               <xs:complexType name="M"><xs:attribute ref="ref"/></xs:complexType>
               <xs:element name="r"><xs:complexType><xs:sequence>
                 <xs:element name="c"><xs:complexType><xs:sequence>
                   <xs:element name="m" type="M"/>
                   <xs:element name="m" type="xs:string"/>
                 </xs:sequence></xs:complexType></xs:element>
               </xs:sequence></xs:complexType></xs:element>"#,
        ),
        "r",
    );
    assert_eq!(names(s.fields()), ["c"]);
    assert_eq!(
        field(s.fields(), "c").data_type(),
        &list_of(DataType::Utf8, false)
    );
}

#[test]
fn derivation_marks_never_reach_the_schema() {
    let s = derive(
        &schema(
            r#"<xs:element name="r"><xs:complexType><xs:sequence>
                 <xs:element name="a" type="xs:int"/>
                 <xs:element name="a" type="xs:int"/>
                 <xs:element name="b" type="xs:int" maxOccurs="2"/>
               </xs:sequence></xs:complexType></xs:element>"#,
        ),
        "r",
    );
    let mut stack: Vec<&Field> = s.fields().iter().map(AsRef::as_ref).collect();
    while let Some(f) = stack.pop() {
        assert!(
            f.metadata().is_empty(),
            "{} keeps {:?}",
            f.name(),
            f.metadata()
        );
        match f.data_type() {
            DataType::Struct(children) => stack.extend(children.iter().map(AsRef::as_ref)),
            DataType::List(item) => stack.push(item),
            _ => {}
        }
    }
}

// ── Every fixture schema ───────────────────────────────────────────────────

/// Paths of sibling columns that share a name, anywhere under `fields`.
fn duplicates(at: &str, fields: &Fields, out: &mut Vec<String>) {
    let mut seen = std::collections::HashSet::new();
    for f in fields {
        if !seen.insert(f.name()) {
            out.push(format!("{at}/{}", f.name()));
        }
        let mut data_type = f.data_type();
        while let DataType::List(item) = data_type {
            data_type = item.data_type();
        }
        if let DataType::Struct(children) = data_type {
            duplicates(&format!("{at}/{}", f.name()), children, out);
        }
    }
}

/// Every `<xs:schema>...</xs:schema>` written out in `text`.
fn inline_schemas(text: &str) -> Vec<String> {
    let mut found = Vec::new();
    let mut rest = text;
    while let Some(open) = rest.find("<xs:schema") {
        let Some(len) = rest[open..].find("</xs:schema>") else {
            break;
        };
        let end = open + len + "</xs:schema>".len();
        found.push(rest[open..end].to_string());
        rest = &rest[end..];
    }
    found
}

fn fixture_schemas() -> Vec<(String, String)> {
    let crate_dir = Path::new(env!("CARGO_MANIFEST_DIR"));
    let mut sources: Vec<PathBuf> = std::fs::read_dir(crate_dir.join("tests"))
        .expect("tests directory")
        .filter_map(|entry| entry.ok().map(|entry| entry.path()))
        .filter(|path| path.extension().is_some_and(|ext| ext == "rs"))
        .collect();
    sources.push(crate_dir.join("src/arrow.rs"));
    sources.push(crate_dir.join("src/validator.rs"));
    sources.sort();
    let mut schemas = Vec::new();
    for source in sources {
        let text = std::fs::read_to_string(&source).expect("test source reads");
        for (index, xsd) in inline_schemas(&text).into_iter().enumerate() {
            let label = format!("{}#{index}", source.display());
            schemas.push((label, xsd));
        }
    }
    let note = crate_dir.join("../xsd-fast-node/__fixtures__/note.xsd");
    let text = std::fs::read_to_string(&note).expect("note.xsd reads");
    schemas.push((note.display().to_string(), text));
    schemas
}

#[test]
fn no_fixture_schema_derives_sibling_columns_of_one_name() {
    let mut derived = 0;
    let mut found = Vec::new();
    for (label, xsd) in fixture_schemas() {
        // Some inline texts are templates or deliberately broken.
        let Ok(set) = SchemaSet::load_bytes(xsd.as_bytes(), ParseOptions::default()) else {
            continue;
        };
        for shape in [
            ArrowShape::new(),
            ArrowShape::new().with_attributes_on_simple_children(true),
        ] {
            let Ok(all) = set.try_all_arrow_schemas_with_shape(ArrowOptions::default(), shape)
            else {
                continue;
            };
            for (name, schema) in all {
                derived += 1;
                duplicates(&format!("{label}:{name}"), schema.fields(), &mut found);
            }
        }
    }
    assert!(derived >= 100, "only {derived} fixture schemas derived");
    assert!(found.is_empty(), "duplicate sibling columns: {found:#?}");
}
