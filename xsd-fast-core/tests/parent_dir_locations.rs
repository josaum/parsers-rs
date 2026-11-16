//! `schemaLocation` values that go up a directory.
//!
//! Schema packages keep shared files in sibling directories and name them
//! with `../`. Such a location is followed when it stays inside the directory
//! of the root schema, and refused when it leaves it.

use std::fs;
use std::path::Path;

use xsd_fast_core::{ParseOptions, SchemaSet};

fn write(dir: &Path, name: &str, body: &str) {
    let path = dir.join(name);
    fs::create_dir_all(path.parent().expect("parent")).expect("mkdir");
    fs::write(
        path,
        format!("<xs:schema xmlns:xs=\"http://www.w3.org/2001/XMLSchema\">{body}</xs:schema>"),
    )
    .expect("write");
}

/// A package whose `content/package.xsd` names `location`, under `<outer>/pkg`.
fn package(outer: &Path, location: &str) -> std::path::PathBuf {
    let root = outer.join("pkg");
    write(
        outer,
        "secret.xsd",
        "<xs:element name=\"secret\" type=\"xs:string\"/>",
    );
    write(
        &root,
        "root.xsd",
        "<xs:include schemaLocation=\"content/package.xsd\"/>",
    );
    write(
        &root,
        "content/package.xsd",
        &format!("<xs:include schemaLocation=\"{location}\"/><xs:element name=\"package\" type=\"xs:string\"/>"),
    );
    write(
        &root,
        "lom/lom.xsd",
        "<xs:element name=\"lom\" type=\"xs:string\"/>",
    );
    root.join("root.xsd")
}

#[test]
fn a_parent_directory_inside_the_schema_root_is_followed() {
    for location in [
        "../lom/lom.xsd",
        "./../lom/./lom.xsd",
        "../content/../lom/lom.xsd",
    ] {
        let outer = tempfile::tempdir().expect("tempdir");
        let schema = SchemaSet::load(package(outer.path(), location), ParseOptions::default())
            .unwrap_or_else(|error| panic!("{location} must load: {error:#}"));
        let names: Vec<&str> = schema
            .model()
            .global_elements
            .keys()
            .map(String::as_str)
            .collect();
        assert_eq!(names, ["lom", "package"], "{location}");
    }
}

#[test]
fn a_parent_directory_that_leaves_the_schema_root_is_refused() {
    for location in [
        "../../secret.xsd",
        "../../pkg/lom/lom.xsd",
        "../lom/../../secret.xsd",
        "../..",
    ] {
        let outer = tempfile::tempdir().expect("tempdir");
        let error = match SchemaSet::load(package(outer.path(), location), ParseOptions::default())
        {
            Ok(_) => panic!("{location} must be refused"),
            Err(error) => error,
        };
        assert!(
            error.to_string().contains("rejected schemaLocation"),
            "{location}: {error:#}"
        );
    }
}

#[cfg(unix)]
#[test]
fn a_parent_directory_reached_through_a_link_out_of_the_root_is_refused() {
    let outer = tempfile::tempdir().expect("tempdir");
    let entry = package(outer.path(), "../out/secret.xsd");
    std::os::unix::fs::symlink(outer.path(), outer.path().join("pkg/out")).expect("symlink");
    let error = match SchemaSet::load(entry, ParseOptions::default()) {
        Ok(_) => panic!("a link out of the schema root must be refused"),
        Err(error) => error,
    };
    assert!(
        error.to_string().contains("rejected schemaLocation"),
        "{error:#}"
    );
}
