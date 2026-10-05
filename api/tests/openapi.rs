//! The OpenAPI document is how AI agents (and people) learn what each route
//! does, so these tests keep it complete and keep `api/openapi.json` current.
//! None of them need a database.
//!
//! After changing a route or schema, regenerate the checked-in file with
//! `make api-openapi`.

use api::app;
use api::state::AppState;
use axum_test::TestServer;
use serde_json::Value;
use sqlx::MySqlPool;
use std::collections::HashSet;
use std::path::{Path, PathBuf};

const SPEC_FILE: &str = "openapi.json";
const METHODS: [&str; 5] = ["get", "post", "put", "patch", "delete"];

fn spec() -> Value {
    serde_json::to_value(api::openapi::spec()).expect("spec serializes")
}

fn manifest_dir() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
}

fn has_text(value: Option<&Value>) -> bool {
    value
        .and_then(Value::as_str)
        .is_some_and(|text| !text.trim().is_empty())
}

#[tokio::test]
async fn spec_is_served_without_a_session() {
    // Never connects: the route reads no session and no database.
    let pool = MySqlPool::connect_lazy("mysql://unused@127.0.0.1/unused").expect("lazy pool");
    let server = TestServer::new(app(AppState::new(pool, "secret", "http://localhost:5173")));
    let res = server.get("/openapi.json").await;
    res.assert_status_ok();
    assert_eq!(res.json::<Value>(), spec());
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
            if op["security"].as_array().is_none_or(Vec::is_empty) {
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
            if !responses.is_some_and(|r| r.keys().any(|code| code.starts_with('2'))) {
                problems.push(format!("{at}: no success response"));
            }
            if !responses.is_some_and(|r| r.contains_key("401")) {
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
/// carries its own.
fn check_properties(name: &str, schema: &Value, problems: &mut Vec<String>) {
    for (field, property) in schema["properties"].as_object().into_iter().flatten() {
        if property.get("$ref").is_none() && !has_text(property.get("description")) {
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

/// A plain axum `.route(...)` would serve the endpoint but leave it out of the
/// spec; domain routers must register handlers with `routes!` instead.
#[test]
fn routes_register_through_openapi_router() {
    let mut offenders = Vec::new();
    scan_for_plain_routes(&manifest_dir().join("src/routes"), &mut offenders);
    assert!(
        offenders.is_empty(),
        "use `.routes(routes!(handler))` instead of `.route(...)` in: {offenders:?}"
    );
}

fn scan_for_plain_routes(dir: &Path, offenders: &mut Vec<PathBuf>) {
    for entry in std::fs::read_dir(dir).expect("read src/routes") {
        let path = entry.expect("dir entry").path();
        if path.is_dir() {
            scan_for_plain_routes(&path, offenders);
        } else if path.extension().is_some_and(|ext| ext == "rs")
            && std::fs::read_to_string(&path)
                .expect("read route file")
                .contains(".route(")
        {
            offenders.push(path);
        }
    }
}
