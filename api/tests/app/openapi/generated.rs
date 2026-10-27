//! Checks of the spec generated from the annotations (`--features openapi`;
//! run them locally, CI does not).

use super::manifest_dir;
use serde_json::Value;
use std::collections::HashSet;

const SPEC_FILE: &str = "openapi.json";
const METHODS: [&str; 5] = ["get", "post", "put", "patch", "delete"];

fn spec() -> Value {
    serde_json::to_value(api::openapi::spec()).expect("spec serializes")
}
fn has_text(value: Option<&Value>) -> bool {
    value
        .and_then(Value::as_str)
        .is_some_and(|text| !text.trim().is_empty())
}
#[test]
fn spec_file_is_current() {
    let generated = api::openapi::spec()
        .to_pretty_json()
        .expect("spec serializes")
        + "\n";
    let path = manifest_dir().join(SPEC_FILE);
    if std::env::var_os("UPDATE_OPENAPI").is_some() {
        std::fs::write(&path, &generated).expect("write openapi.json");
        return;
    }
    let checked_in = std::fs::read_to_string(&path).unwrap_or_default();
    assert!(
        checked_in == generated,
        "api/openapi.json is stale; run `make api-openapi` and commit the result"
    );
}

#[test]
fn every_operation_is_documented() {
    let spec = spec();
    let mut problems = Vec::new();
    let mut operation_ids = HashSet::new();

    for (path, item) in spec["paths"].as_object().expect("paths") {
        for method in METHODS {
            let Some(op) = item.get(method) else { continue };
            let at = format!("{} {path}", method.to_uppercase());
            // The showroom, signing in and third parties' webhooks (signed
            // by the sender) are public: no session, so no security and no 401.
            let public = path.starts_with("/v1/public/")
                || path.starts_with("/v1/webhooks/")
                || path == "/v1/auth/login"
                || path == "/v1/auth/logout"
                || path.starts_with("/v1/auth/password-reset")
                || path == "/v1/auth/device-sessions"
                || path.starts_with("/v1/auth/invite/");

            match op["operationId"].as_str() {
                // The default is the handler's name (`list`); tools need `list_customers`.
                Some(id) if id.contains('_') => {
                    if !operation_ids.insert(id.to_string()) {
                        problems.push(format!("{at}: duplicate operation_id `{id}`"));
                    }
                }
                _ => problems.push(format!(
                    "{at}: set an explicit verb_noun `operation_id` (e.g. `list_customers`)"
                )),
            }
            if !has_text(op.get("summary")) {
                problems.push(format!("{at}: missing summary (first doc-comment line)"));
            }
            if !has_text(op.get("description")) {
                problems.push(format!(
                    "{at}: missing description (doc-comment paragraph after the summary)"
                ));
            }
            if op["tags"].as_array().is_none_or(Vec::is_empty) {
                problems.push(format!("{at}: missing `tag`"));
            }
            if public {
                if op["security"].as_array().is_some_and(|s| !s.is_empty()) {
                    problems.push(format!("{at}: public routes take no session"));
                }
            } else if op["security"].as_array().is_none_or(Vec::is_empty) {
                problems.push(format!(
                    "{at}: missing `security((\"session_cookie\" = []))`"
                ));
            }
            for param in op["parameters"].as_array().into_iter().flatten() {
                if !has_text(param.get("description")) {
                    problems.push(format!(
                        "{at}: parameter `{}` has no doc comment",
                        param["name"].as_str().unwrap_or("?")
                    ));
                }
            }
            let responses = op["responses"].as_object();
            // A redirect is the answer of routes made for `<img>` (previews).
            if !responses.is_some_and(|r| {
                r.keys()
                    .any(|code| code.starts_with('2') || code.starts_with('3'))
            }) {
                problems.push(format!("{at}: no success response"));
            }
            if !public && !responses.is_some_and(|r| r.contains_key("401")) {
                problems.push(format!("{at}: no 401 response"));
            }
        }
    }
    assert!(problems.is_empty(), "\n{}\n", problems.join("\n"));
}

#[test]
fn every_schema_field_is_documented() {
    let spec = spec();
    let mut problems = Vec::new();
    for (name, schema) in spec["components"]["schemas"].as_object().expect("schemas") {
        if !has_text(schema.get("description")) {
            problems.push(format!("schema `{name}` has no doc comment"));
        }
        check_properties(name, schema, &mut problems);
    }
    assert!(problems.is_empty(), "\n{}\n", problems.join("\n"));
}

/// Every property needs a description, except `$ref`s, whose target schema
/// carries its own. A nullable object (`Option<T>`) is a `oneOf` of the
/// `$ref` and `null`, and utoipa puts the field's doc on the `$ref` member.
fn check_properties(name: &str, schema: &Value, problems: &mut Vec<String>) {
    for (field, property) in schema["properties"].as_object().into_iter().flatten() {
        let nullable_ref_documented = property["oneOf"]
            .as_array()
            .into_iter()
            .flatten()
            .any(|member| member.get("$ref").is_some() && has_text(member.get("description")));
        if property.get("$ref").is_none()
            && !nullable_ref_documented
            && !has_text(property.get("description"))
        {
            problems.push(format!(
                "schema `{name}`: field `{field}` has no doc comment"
            ));
        }
    }
    for key in ["allOf", "oneOf", "anyOf"] {
        for part in schema[key].as_array().into_iter().flatten() {
            check_properties(name, part, problems);
        }
    }
}

#[test]
fn every_ref_resolves() {
    let spec = spec();
    let mut refs = Vec::new();
    collect_refs(&spec, &mut refs);
    let missing: Vec<_> = refs
        .into_iter()
        .filter(|reference| {
            reference
                .strip_prefix('#')
                .is_none_or(|pointer| spec.pointer(pointer).is_none())
        })
        .collect();
    assert!(
        missing.is_empty(),
        "unresolved $refs (add `inline` to the `value_type`, or derive `ToSchema` on a type \
         used in a body): {missing:?}"
    );
}

fn collect_refs(value: &Value, refs: &mut Vec<String>) {
    match value {
        Value::Object(map) => {
            if let Some(Value::String(reference)) = map.get("$ref") {
                refs.push(reference.clone());
            }
            map.values().for_each(|child| collect_refs(child, refs));
        }
        Value::Array(items) => items.iter().for_each(|child| collect_refs(child, refs)),
        _ => {}
    }
}
