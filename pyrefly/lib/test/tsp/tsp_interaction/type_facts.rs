/*
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 *
 * This source code is licensed under the MIT license found in the
 * LICENSE file in the root directory of this source tree.
 */

//! Integration tests for the `pyrefly/typeFacts` request.

use lsp_types::Uri;
use serde_json::json;
use tempfile::TempDir;

use crate::test::tsp::tsp_interaction::object_model::TspInteraction;
use crate::test::tsp::tsp_interaction::object_model::get_current_snapshot;
use crate::test::tsp::tsp_interaction::object_model::write_pyproject;

const SOURCE: &str = r#"import abc, dataclasses, functools, os.path

class Base: ...
class Child(Base): ...

def make() -> Child:
    return Child()

def take(b: Base) -> None: ...

c = Child()
take(c)
def f(x: int | Child) -> None:
    print(x)
m = os.path
opt: Child | None = None
seq: list[Child] = []
deep: list[list[list[Child]]] = []
class Svc:
    @staticmethod
    def s() -> None: ...
    async def a(self) -> None: ...
    @abc.abstractmethod
    def ab(self) -> None: ...
svc = Svc()
Svc.s
svc.a
svc.ab
@dataclasses.dataclass(frozen=True)
class Point:
    x: int
    @property
    def norm(self) -> int: ...
    @functools.cached_property
    def cached(self) -> int: ...
    @functools.lru_cache
    def memo(self) -> int: ...
    class Inner: ...
    def method(self) -> None: ...
Point
"#;

fn setup(source: &str) -> (TspInteraction, String, i32, TempDir) {
    let temp_dir = TempDir::new().unwrap();
    write_pyproject(temp_dir.path());
    let test_file = temp_dir.path().join("main.py");
    std::fs::write(&test_file, source).unwrap();
    let mut tsp = TspInteraction::new();
    tsp.set_root(temp_dir.path().to_path_buf());
    tsp.initialize(Default::default());
    tsp.server.did_open("main.py");
    tsp.client.expect_any_message();
    let snapshot = get_current_snapshot(&mut tsp, 2);
    let uri = Uri::from_file_path(&test_file).unwrap().to_string();
    (tsp, uri, snapshot, temp_dir)
}

/// Ranges are `(line, start, line, end)` into `SOURCE`, zero-based.
fn facts(queries: &[((u32, u32, u32, u32), bool)]) -> serde_json::Value {
    facts_with_uri(queries).0
}

/// Like `facts`, also returning the URI of `main.py`, which definitions in it name.
fn facts_with_uri(queries: &[((u32, u32, u32, u32), bool)]) -> (serde_json::Value, String) {
    let (mut tsp, uri, snapshot, _dir) = setup(SOURCE);
    tsp.server.type_facts(&uri, queries, snapshot);
    let resp = tsp.client.receive_response_skip_notifications();
    assert!(resp.error.is_none(), "unexpected error: {:?}", resp.error);
    tsp.shutdown();
    (resp.result.expect("result"), uri)
}

/// A definition in `main.py`: the name at `(line, start..end)`, zero-based.
fn main_definition(uri: &str, line: u32, start: u32, end: u32) -> serde_json::Value {
    json!({
        "uri": uri,
        "range": {
            "start": {"line": line, "character": start},
            "end": {"line": line, "character": end},
        },
    })
}

#[test]
fn test_instance_class_module_and_function() {
    let (result, uri) = facts_with_uri(&[
        // `c` on line 10: an instance of Child, whose MRO lists Base.
        ((10, 0, 10, 1), false),
        // `Child` in `class Child(Base)` on line 3: the class object.
        ((3, 6, 3, 11), false),
        // `os.path` in `m = os.path` on line 14: a module.
        ((14, 4, 14, 11), false),
        // `make` on line 5: a function returning Child.
        ((5, 4, 5, 8), false),
    ]);
    // `Child` is named on line 3, `make` on line 5.
    let child = main_definition(&uri, 3, 6, 11);
    let make = main_definition(&uri, 5, 4, 8);
    assert_eq!(
        result,
        json!([
            {"kind": "instance", "qname": "main.Child", "definition": child, "mro": ["main.Base"]},
            {"kind": "class", "qname": "main.Child", "definition": child, "mro": ["main.Base"]},
            {"kind": "module", "name": "os.path"},
            {
                "kind": "function",
                "qname": "main.make",
                "definition": make,
                "returns": {
                    "kind": "instance",
                    "qname": "main.Child",
                    "definition": child,
                    "mro": ["main.Base"],
                },
            },
        ])
    );
}

#[test]
fn test_expected_type_at_call_argument() {
    // `c` in `take(c)` on line 11: computed Child, expected Base (the parameter),
    // which is named on line 2.
    let (result, uri) = facts_with_uri(&[((11, 5, 11, 6), false), ((11, 5, 11, 6), true)]);
    assert_eq!(result[0]["qname"], json!("main.Child"));
    assert_eq!(
        result[1],
        json!({
            "kind": "instance",
            "qname": "main.Base",
            "definition": main_definition(&uri, 2, 6, 10),
            "mro": [],
        })
    );
}

#[test]
fn test_expected_type_absent_without_context() {
    // `c = Child()` on line 10: nothing expects a type for `c`, and unlike
    // `typeServer/getExpectedType` there is no fallback to the computed type.
    let result = facts(&[((10, 0, 10, 1), true)]);
    assert_eq!(result, json!([null]));
}

#[test]
fn test_inverted_range_gets_no_fact_and_keeps_the_batch() {
    // An end before the start would panic in `TextRange::new`; the other query in
    // the batch must still be answered.
    let result = facts(&[((10, 1, 10, 0), false), ((10, 0, 10, 1), false)]);
    assert_eq!(result[0], json!(null));
    assert_eq!(result[1]["qname"], json!("main.Child"));
}

#[test]
fn test_union_members() {
    // `x` in `print(x)` on line 13 is the parameter declared `int | Child`.
    let result = facts(&[((13, 10, 13, 11), false)]);
    let members = result[0]["members"].as_array().expect("union members");
    let mut qnames: Vec<&str> = members.iter().filter_map(|m| m["qname"].as_str()).collect();
    qnames.sort_unstable();
    assert_eq!(result[0]["kind"], json!("union"));
    assert_eq!(qnames, vec!["builtins.int", "main.Child"]);
}

#[test]
fn test_definition_in_another_module() {
    // `x` in `print(x)` on line 13 is `int | Child`; `int` is defined in the bundled
    // `builtins` stub.
    let result = facts(&[((13, 10, 13, 11), false)]);
    let members = result[0]["members"].as_array().expect("union members");
    let int = members
        .iter()
        .find(|m| m["qname"] == json!("builtins.int"))
        .expect("builtins.int among the members");
    let uri = int["definition"]["uri"].as_str().expect("a definition URI");
    assert!(uri.ends_with("builtins.pyi"), "got {uri}");
}

#[test]
fn test_function_flags() {
    // `Svc.s`, `svc.a` and `svc.ab` on lines 25-27; `make` on line 5 has no flags.
    let result = facts(&[
        ((25, 0, 25, 5), false),
        ((26, 0, 26, 5), false),
        ((27, 0, 27, 6), false),
        ((5, 4, 5, 8), false),
    ]);
    assert_eq!(result[0]["flags"], json!(["staticmethod"]), "got {result}");
    assert_eq!(result[1]["flags"], json!(["async"]), "got {result}");
    assert_eq!(result[2]["flags"], json!(["abstract"]), "got {result}");
    assert!(result[3].get("flags").is_none(), "got {result}");
}

#[test]
fn test_dataclass_and_members() {
    // `Point` on line 39, asking for members, and `Child` on line 3, which is not a
    // dataclass.
    let (mut tsp, uri, snapshot, _dir) = setup(SOURCE);
    let range = |line: u32, start: u32, end: u32| {
        json!({
            "start": {"line": line, "character": start},
            "end": {"line": line, "character": end},
        })
    };
    tsp.server.type_facts_raw(
        &uri,
        vec![
            json!({"range": range(39, 0, 5), "members": true}),
            json!({"range": range(3, 6, 11)}),
        ],
        snapshot,
    );
    let resp = tsp.client.receive_response_skip_notifications();
    tsp.shutdown();
    let result = resp.result.expect("result");
    assert_eq!(
        result[0]["dataclass"],
        json!({"frozen": true}),
        "got {result}"
    );
    // Each member as (name, kind of its type, its flags or its qualified name).
    let members: Vec<(String, String, serde_json::Value)> = result[0]["members"]
        .as_array()
        .expect("members")
        .iter()
        .map(|m| {
            let ty = &m["type"];
            let detail = match ty["kind"].as_str() {
                Some("function") => ty.get("flags").cloned().unwrap_or(json!([])),
                _ => ty["qname"].clone(),
            };
            (
                m["name"].as_str().unwrap().to_owned(),
                ty["kind"].as_str().unwrap().to_owned(),
                detail,
            )
        })
        .collect();
    let expected = [
        ("x", "instance", json!("builtins.int")),
        ("norm", "function", json!(["property"])),
        ("cached", "function", json!(["property", "cachedProperty"])),
        // The decorator returns a wrapper object, so the member has its type.
        ("memo", "instance", json!("functools._lru_cache_wrapper")),
        ("Inner", "class", json!("main.Inner")),
        ("method", "function", json!([])),
    ]
    .map(|(name, kind, detail)| (name.to_owned(), kind.to_owned(), detail));
    assert_eq!(members, expected, "got {result}");
    assert!(result[1].get("dataclass").is_none(), "got {result}");
    assert!(result[1].get("members").is_none(), "got {result}");
}

#[test]
fn test_stale_snapshot_is_rejected() {
    let (mut tsp, uri, _snapshot, _dir) = setup(SOURCE);
    tsp.server
        .type_facts(&uri, &[((10, 0, 10, 1), false)], 9999);
    let resp = tsp.client.receive_response_skip_notifications();
    assert!(
        resp.error.is_some(),
        "expected an error for a stale snapshot"
    );
    tsp.shutdown();
}

#[test]
fn test_union_type_form_lists_its_classes() {
    // The annotation `Child | None` on line 15 evaluates to `type[Child | None]`.
    let (result, uri) = facts_with_uri(&[((15, 5, 15, 17), false)]);
    assert_eq!(result[0]["kind"], json!("union"), "got {result}");
    let members = result[0]["members"].as_array().expect("union members");
    let child = json!({
        "kind": "class",
        "qname": "main.Child",
        "definition": main_definition(&uri, 3, 6, 11),
        "mro": ["main.Base"],
    });
    assert!(
        members.contains(&child),
        "expected class main.Child among {members:?}"
    );
}

#[test]
fn test_specialized_class_object_carries_type_arguments() {
    // The annotation `list[Child]` on line 16.
    let result = facts(&[((16, 5, 16, 16), false)]);
    assert_eq!(result[0]["kind"], json!("class"), "got {result}");
    assert_eq!(result[0]["qname"], json!("builtins.list"));
    assert_eq!(
        result[0]["typeArgs"][0]["qname"],
        json!("main.Child"),
        "got {result}"
    );
}

#[test]
fn test_nesting_stops_two_levels_below_the_queried_type() {
    // `deep` on line 17 is `list[list[list[Child]]]`: the outer list and the two
    // lists nested inside it are described, but not the third level's arguments.
    let result = facts(&[((17, 0, 17, 4), false)]);
    let inner = &result[0]["typeArgs"][0]["typeArgs"][0];
    assert_eq!(inner["qname"], json!("builtins.list"), "got {result}");
    assert!(
        inner.get("typeArgs").is_none(),
        "expected nesting to stop at the third list, got {result}"
    );
}
