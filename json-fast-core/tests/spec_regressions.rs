use json_fast_core::{jsonapi::JsonApiDocument, jsonl, jsonrpc::*, schema};
use serde_json::{json, Value};

#[test]
fn protocol_envelopes_require_json_objects() {
    assert!(JsonApiDocument::parse("[null]").is_err());
    assert!(JsonRpcRequest::parse(r#"["2.0","m"]"#).is_err());
    assert!(JsonRpcResponse::parse(r#"["2.0",1,null,1]"#).is_err());
    assert!(JsonRpcResponse::parse(r#"{"jsonrpc":"2.0","error":[-1,"bad"],"id":1}"#).is_err());
}

#[test]
fn jsonapi_preserves_explicit_null_and_excludes_errors() {
    let doc = JsonApiDocument::parse(r#"{"data":null}"#).unwrap();
    assert_eq!(doc.data, Some(Value::Null));
    assert_eq!(serde_json::to_value(doc).unwrap(), json!({"data": null}));
    assert!(JsonApiDocument::parse(r#"{"data":null,"errors":[]}"#).is_err());
}

#[test]
fn jsonapi_checks_top_level_member_shapes() {
    for input in [
        r#"{"data":1}"#,
        r#"{"data":[null]}"#,
        r#"{"meta":null}"#,
        r#"{"meta":[]}"#,
        r#"{"errors":[1]}"#,
        r#"{"errors":null,"meta":{}}"#,
        r#"{"included":[],"meta":{}}"#,
        r#"{"data":[],"included":null}"#,
    ] {
        assert!(JsonApiDocument::parse(input).is_err(), "accepted {input}");
    }
    assert!(JsonApiDocument::parse(r#"{"meta":{}}"#).is_ok());
    assert!(JsonApiDocument::parse(r#"{"data":[],"included":[]}"#).is_ok());
}

#[test]
fn jsonrpc_null_is_distinct_from_absent() {
    let response = JsonRpcResponse::parse(r#"{"jsonrpc":"2.0","result":null,"id":null}"#).unwrap();
    assert_eq!(response.result, Some(Value::Null));
    assert_eq!(response.id, Some(Value::Null));
    assert_eq!(
        serde_json::to_value(response).unwrap(),
        json!({"jsonrpc":"2.0","result":null,"id":null})
    );
    let request = JsonRpcRequest::parse(r#"{"jsonrpc":"2.0","method":"m","id":null}"#).unwrap();
    assert_eq!(request.id, Some(Value::Null));
    let notification = JsonRpcRequest::parse(r#"{"jsonrpc":"2.0","method":"m"}"#).unwrap();
    assert!(notification.id.is_none());
}

#[test]
fn jsonrpc_rejects_invalid_member_types_and_missing_response_id() {
    for member in [
        r#""params":null"#,
        r#""params":1"#,
        r#""params":"x""#,
        r#""id":true"#,
        r#""id":[]"#,
        r#""id":{}"#,
    ] {
        let input = format!(r#"{{"jsonrpc":"2.0","method":"m",{member}}}"#);
        assert!(JsonRpcRequest::parse(&input).is_err(), "accepted {input}");
    }
    for input in [
        r#"{"jsonrpc":"2.0","result":1}"#,
        r#"{"jsonrpc":"2.0","result":1,"id":true}"#,
        r#"{"jsonrpc":"2.0","result":1,"error":null,"id":1}"#,
        r#"{"jsonrpc":"2.0","result":null,"error":{"code":1,"message":"x"},"id":1}"#,
    ] {
        assert!(JsonRpcResponse::parse(input).is_err(), "accepted {input}");
    }
}

#[test]
fn jsonl_only_accepts_json_whitespace() {
    for whitespace in ['\u{a0}', '\u{b}', '\u{c}', '\u{2003}'] {
        for input in [format!("{whitespace}1\n"), format!("{whitespace}\n")] {
            assert!(
                jsonl::parse_many::<Value>(&input).is_err(),
                "accepted {input:?}"
            );
            assert!(jsonl::iter::<Value>(&input).next().unwrap().is_err());
            assert_eq!(jsonl::count_lines(&input), 1);
        }
    }
    assert_eq!(
        jsonl::parse_many::<Value>(" \t\r\n1\r\n\t2 \n").unwrap(),
        vec![json!(1), json!(2)]
    );
}

#[test]
fn schema_unique_items_ignores_object_key_order() {
    let doc = json!([{"a": 1, "b": {"c":2,"d":3}}, {"b":{"d":3,"c":2}, "a":1.0}]);
    assert!(schema::has_errors(&schema::validate(
        &doc,
        &json!({"uniqueItems":true})
    )));
}

#[test]
fn schema_numeric_equality_does_not_round_integers_to_floats() {
    for (integer, float) in [
        (
            json!(9_007_199_254_740_993u64),
            json!(9_007_199_254_740_992.0),
        ),
        (json!(u64::MAX), json!(18_446_744_073_709_551_616.0)),
        (json!(i64::MIN + 1), json!(-9_223_372_036_854_775_808.0)),
    ] {
        assert!(schema::has_errors(&schema::validate(
            &integer,
            &json!({"const":float})
        )));
        assert!(!schema::has_errors(&schema::validate(
            &json!([integer, float]),
            &json!({"uniqueItems":true})
        )));
    }
    assert!(!schema::has_errors(&schema::validate(
        &json!(-0.0),
        &json!({"const":0})
    )));
}

#[test]
fn schema_anchors_resolve_under_array_applicators() {
    let schema = json!({
        "$defs": {"bundle": {"allOf": [{"$anchor":"positive", "minimum":1}]}},
        "$ref":"#positive"
    });
    assert!(schema::validate(&json!(2), &schema).is_empty());
    assert!(schema::has_errors(&schema::validate(&json!(0), &schema)));
    let root_anchor =
        json!({"$anchor":"root", "type":"integer", "properties":{"value":{"$ref":"#root"}}});
    let findings = schema::validate(&json!({"value":"wrong"}), &root_anchor);
    assert!(findings
        .iter()
        .any(|finding| finding.path == "$/value" && finding.severity == schema::Severity::Error));
}

#[test]
fn schema_anchors_in_instance_data_are_not_registered() {
    let schema = json!({"const":{"$anchor":"fake"}, "$ref":"#fake"});
    assert!(schema::validate(&json!({"$anchor":"fake"}), &schema)
        .iter()
        .any(|finding| finding.severity == schema::Severity::Warning));
}

#[test]
fn schema_pointer_references_validate_escapes_and_array_indices() {
    for reference in ["#/$defs/list/01", "#/$defs/list/+1", "#/$defs/bad~2key"] {
        let schema = json!({
            "$defs": {"list":[true, false], "bad~2key":false},
            "$ref":reference
        });
        let findings = schema::validate(&Value::Null, &schema);
        assert!(
            findings
                .iter()
                .any(|finding| finding.severity == schema::Severity::Warning),
            "{reference}"
        );
        assert!(
            !schema::has_errors(&findings),
            "invalid pointers must not select a schema"
        );
    }
    let schema = json!({"$defs":{"a/b~c":false}, "$ref":"#/$defs/a~1b~0c"});
    assert!(schema::has_errors(&schema::validate(&Value::Null, &schema)));
}
