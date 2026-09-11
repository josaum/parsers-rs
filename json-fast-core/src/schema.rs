//! JSON Schema validation — a focused, fail-loud subset.
//!
//! Implements the commonly used draft-07 / OpenAPI keywords: `type`,
//! `nullable`, `enum`, `properties`, `required`, `items`,
//! `additionalProperties` (boolean only), `minimum` / `maximum`,
//! `minLength` / `maxLength`, `minItems` / `maxItems`,
//! `minProperties` / `maxProperties`, and `pattern` (Rust regex).
//!
//! Unsupported keywords (`$ref`, `oneOf`, `anyOf`, `allOf`, `format`, ...) are
//! surfaced as warnings so validation never silently under-checks. Metadata
//! keywords (`title`, `description`, `default`, ...) are ignored.
//!
//! `validate` never panics and never allocates outside the findings list.

use std::collections::HashSet;

use serde_json::Value;

/// Severity of a schema [`Finding`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Severity {
    /// A concrete schema violation — the document does not match the schema.
    Error,
    /// The schema used a keyword this subset does not implement; check skipped.
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

/// One schema-validation finding (a violation or an unsupported-keyword note).
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
    fn error(path: impl Into<String>, message: impl Into<String>) -> Finding {
        Finding {
            severity: Severity::Error,
            path: path.into(),
            message: message.into(),
        }
    }

    fn warning(path: impl Into<String>, message: impl Into<String>) -> Finding {
        Finding {
            severity: Severity::Warning,
            path: path.into(),
            message: message.into(),
        }
    }
}

/// Keywords this subset implements.
const KNOWN: &[&str] = &[
    "type",
    "nullable",
    "enum",
    "properties",
    "required",
    "additionalProperties",
    "items",
    "minimum",
    "maximum",
    "minLength",
    "maxLength",
    "minItems",
    "maxItems",
    "minProperties",
    "maxProperties",
    "pattern",
];

/// Metadata keywords that carry no constraint; ignored without a warning.
const METADATA: &[&str] = &[
    "$schema",
    "$id",
    "$anchor",
    "title",
    "description",
    "default",
    "examples",
];

/// Constraint keywords that exist in draft-07+ but are not implemented here.
const UNSUPPORTED: &[&str] = &[
    "$ref",
    "$defs",
    "definitions",
    "oneOf",
    "anyOf",
    "allOf",
    "not",
    "if",
    "then",
    "else",
    "format",
    "multipleOf",
    "uniqueItems",
    "contains",
    "minContains",
    "maxContains",
    "patternProperties",
    "propertyNames",
    "dependencies",
    "dependentRequired",
    "dependentSchemas",
];

/// Validate a JSON document against a JSON Schema (focused subset).
///
/// Returns every [`Finding`] (errors and warnings). Use [`has_errors`] to test
/// whether the document violates the schema.
pub fn validate(document: &Value, schema: &Value) -> Vec<Finding> {
    let mut findings = Vec::new();
    validate_node(document, schema, "$", &mut findings);
    findings
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

fn validate_node(value: &Value, schema: &Value, path: &str, findings: &mut Vec<Finding>) {
    let Some(obj) = schema.as_object() else {
        findings.push(Finding::warning(
            path,
            "schema node is not an object; skipped",
        ));
        return;
    };

    for key in obj.keys() {
        if METADATA.contains(&key.as_str()) {
            continue;
        }
        if UNSUPPORTED.contains(&key.as_str()) {
            findings.push(Finding::warning(
                path,
                format!("unsupported schema keyword {key:?} ignored"),
            ));
        } else if !KNOWN.contains(&key.as_str()) {
            findings.push(Finding::warning(
                path,
                format!("unknown schema keyword {key:?} ignored"),
            ));
        }
    }

    // OpenAPI `nullable: true` short-circuits null values.
    if obj.get("nullable").and_then(|n| n.as_bool()) == Some(true) && value.is_null() {
        return;
    }

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

    if let Some(en) = obj.get("enum").and_then(|e| e.as_array()) {
        if !en.iter().any(|candidate| candidate == value) {
            findings.push(Finding::error(
                path,
                format!("value is not one of the {} enum entries", en.len()),
            ));
        }
    }

    if let Some(n) = value.as_f64() {
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
    }

    if let Some(s) = value.as_str() {
        let len = s.chars().count() as u64;
        if let Some(min) = obj.get("minLength").and_then(|m| m.as_u64()) {
            if len < min {
                findings.push(Finding::error(
                    path,
                    format!("string length {len} < minLength {min}"),
                ));
            }
        }
        if let Some(max) = obj.get("maxLength").and_then(|m| m.as_u64()) {
            if len > max {
                findings.push(Finding::error(
                    path,
                    format!("string length {len} > maxLength {max}"),
                ));
            }
        }
        if let Some(p) = obj.get("pattern").and_then(|p| p.as_str()) {
            match regex::Regex::new(p) {
                Ok(re) => {
                    if !re.is_match(s) {
                        findings.push(Finding::error(
                            path,
                            format!("string does not match pattern {p:?}"),
                        ));
                    }
                }
                Err(e) => findings.push(Finding::warning(
                    path,
                    format!("invalid pattern {p:?}: {e}"),
                )),
            }
        }
    }

    if let Some(arr) = value.as_array() {
        if let Some(min) = obj.get("minItems").and_then(|m| m.as_u64()) {
            if (arr.len() as u64) < min {
                findings.push(Finding::error(
                    path,
                    format!("array length {} < minItems {min}", arr.len()),
                ));
            }
        }
        if let Some(max) = obj.get("maxItems").and_then(|m| m.as_u64()) {
            if (arr.len() as u64) > max {
                findings.push(Finding::error(
                    path,
                    format!("array length {} > maxItems {max}", arr.len()),
                ));
            }
        }
        if let Some(items) = obj.get("items") {
            for (i, item) in arr.iter().enumerate() {
                validate_node(item, items, &format!("{path}/{i}"), findings);
            }
        }
    }

    if let Some(map) = value.as_object() {
        if let Some(min) = obj.get("minProperties").and_then(|m| m.as_u64()) {
            if (map.len() as u64) < min {
                findings.push(Finding::error(
                    path,
                    format!("object size {} < minProperties {min}", map.len()),
                ));
            }
        }
        if let Some(max) = obj.get("maxProperties").and_then(|m| m.as_u64()) {
            if (map.len() as u64) > max {
                findings.push(Finding::error(
                    path,
                    format!("object size {} > maxProperties {max}", map.len()),
                ));
            }
        }

        if let Some(props) = obj.get("properties").and_then(|p| p.as_object()) {
            for (key, subschema) in props {
                if let Some(v) = map.get(key) {
                    validate_node(v, subschema, &format!("{path}/{key}"), findings);
                }
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

        if obj.get("additionalProperties").and_then(|a| a.as_bool()) == Some(false) {
            let allowed: HashSet<&String> = obj
                .get("properties")
                .and_then(|p| p.as_object())
                .map(|p| p.keys().collect())
                .unwrap_or_default();
            for key in map.keys() {
                if !allowed.contains(key) {
                    findings.push(Finding::error(
                        path,
                        format!("additional property {key:?} not allowed"),
                    ));
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

fn type_matches(value: &Value, type_name: &str) -> bool {
    match type_name {
        "string" => value.is_string(),
        "number" => value.is_number(),
        "integer" => value.as_i64().is_some() || value.as_u64().is_some(),
        "boolean" => value.is_boolean(),
        "null" => value.is_null(),
        "array" => value.is_array(),
        "object" => value.is_object(),
        _ => false,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

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
    fn flags_unsupported_keyword() {
        let doc = json!({"a": 1});
        let schema = json!({"oneOf": [{"type": "object"}]});
        let findings = validate(&doc, &schema);
        assert!(findings.iter().any(|f| f.severity == Severity::Warning));
    }
}
