//! Defensive `package.json` parsing for real-world npm packages.
//!
//! npm package.json is not a strict schema in practice: `"main": false`,
//! numeric versions, odd `exports` shapes, non-string dependency ranges, etc.
//! We parse as `serde_json::Value` and pick out what we need — unexpected
//! types are skipped, never fatal (except wholly invalid JSON / non-object root).

use serde::{self, Serialize};
use serde_json::{Map, Value};
use std::collections::HashMap;
use tracing::warn;

use crate::app_error::ServerError;

use super::additional_exports::get_additional_exports;

#[derive(Debug, Serialize, PartialEq, Eq, Clone)]
pub enum PackageJSONExport {
    Ignored(Option<bool>),
    Value(String),
    Map(HashMap<String, PackageJSONExport>),
    Vec(Vec<PackageJSONExport>),
}

#[derive(Debug, Serialize, PartialEq, Eq, Clone)]
pub struct PackageJSON {
    pub name: String,
    pub version: String,
    pub main: Option<String>,
    pub module: Option<String>,
    pub js_next_main: Option<String>,
    pub browser: Option<PackageJSONExport>,
    pub exports: Option<PackageJSONExport>,
    pub dependencies: Option<HashMap<String, String>>,
    /// Peers declared by the package. Used imports that only appear here
    /// (e.g. `@swc/helpers` for `@tailwindcss/browser`) must still enter
    /// `/dep_tree` — Sandpack installs a flat map and will not resolve them
    /// otherwise.
    pub peer_dependencies: Option<HashMap<String, String>>,
}

/// Parse package.json content. Only fails when the payload is not JSON or not
/// an object — every optional field is best-effort.
pub fn parse_pkg_json(content: String) -> Result<PackageJSON, ServerError> {
    let trimmed = content.trim_start_matches('\u{feff}').trim();
    let value: Value = serde_json::from_str(trimmed).map_err(|err| {
        warn!("package.json is not valid JSON: {err}");
        ServerError::JSONParseError(err)
    })?;

    let Value::Object(obj) = value else {
        warn!("package.json root is not an object — using empty stub");
        return Ok(PackageJSON {
            name: String::new(),
            version: String::new(),
            main: None,
            module: None,
            js_next_main: None,
            browser: None,
            exports: None,
            dependencies: None,
            peer_dependencies: None,
        });
    };

    Ok(package_json_from_map(&obj))
}

fn package_json_from_map(obj: &Map<String, Value>) -> PackageJSON {
    PackageJSON {
        name: optional_string(obj.get("name")).unwrap_or_default(),
        version: optional_string(obj.get("version")).unwrap_or_default(),
        main: optional_entry_path(obj.get("main")),
        module: optional_entry_path(obj.get("module")),
        js_next_main: optional_entry_path(obj.get("jsnext:main")),
        browser: obj.get("browser").and_then(parse_export),
        exports: obj.get("exports").and_then(parse_export),
        dependencies: obj.get("dependencies").and_then(string_map),
        peer_dependencies: obj.get("peerDependencies").and_then(string_map),
    }
}

/// String fields we use as file paths / package ids. Numbers coerced; bool/null/object → None.
fn optional_string(value: Option<&Value>) -> Option<String> {
    match value? {
        Value::String(s) => {
            let t = s.trim();
            if t.is_empty() {
                None
            } else {
                Some(t.to_string())
            }
        }
        Value::Number(n) => Some(n.to_string()),
        _ => None,
    }
}

/// `main` / `module` / `jsnext:main`: string path only. `"main": false` (exports-only) → None.
fn optional_entry_path(value: Option<&Value>) -> Option<String> {
    match value? {
        Value::String(s) => {
            let t = s.trim();
            if t.is_empty() {
                None
            } else {
                Some(t.to_string())
            }
        }
        _ => None,
    }
}

/// Dependency maps: keep string ranges; coerce numbers; skip objects/bools/null.
fn string_map(value: &Value) -> Option<HashMap<String, String>> {
    let Value::Object(obj) = value else {
        return None;
    };
    let mut out = HashMap::new();
    for (key, val) in obj {
        if key.is_empty() {
            continue;
        }
        if let Some(range) = optional_string(Some(val)) {
            out.insert(key.clone(), range);
        }
    }
    if out.is_empty() {
        None
    } else {
        Some(out)
    }
}

/// Recursively normalize `exports` / `browser`. Unknown shapes are dropped, not errors.
fn parse_export(value: &Value) -> Option<PackageJSONExport> {
    match value {
        Value::Null => Some(PackageJSONExport::Ignored(None)),
        Value::Bool(b) => Some(PackageJSONExport::Ignored(Some(*b))),
        Value::String(s) => {
            if s.is_empty() {
                None
            } else {
                Some(PackageJSONExport::Value(s.clone()))
            }
        }
        Value::Array(items) => {
            let parsed: Vec<PackageJSONExport> = items.iter().filter_map(parse_export).collect();
            if parsed.is_empty() {
                None
            } else {
                Some(PackageJSONExport::Vec(parsed))
            }
        }
        Value::Object(map) => {
            let mut out = HashMap::new();
            for (key, val) in map {
                if let Some(export) = parse_export(val) {
                    out.insert(key.clone(), export);
                }
            }
            Some(PackageJSONExport::Map(out))
        }
        Value::Number(_) => None,
    }
}

// exports key order: 'browser', 'development', 'default', 'require', 'import'
// Surprisingly good documentation of exports: https://webpack.js.org/guides/package-exports/
pub fn get_export_entry(exports: &PackageJSONExport) -> Option<String> {
    match exports {
        PackageJSONExport::Value(s) => Some(s.clone()),
        PackageJSONExport::Map(nested_exports_value) => {
            for key in ["browser", "development", "default", "require", "import"] {
                if let Some(v) = nested_exports_value.get(key) {
                    return get_export_entry(v);
                }
            }

            None
        }
        PackageJSONExport::Vec(vector_exports) => {
            for export in vector_exports {
                if let Some(found_export) = get_export_entry(export) {
                    return Some(found_export);
                }
            }

            None
        }
        // Fallback to none
        _ => None,
    }
}

// main fields order: 'exports#.', 'module', 'browser', 'main', 'jsnext:main'
fn get_main_entry(pkg_json: &PackageJSON) -> String {
    if let Some(module_export) = pkg_json.module.clone() {
        return module_export;
    }

    if let Some(PackageJSONExport::Value(val)) = pkg_json.browser.clone() {
        return val;
    }

    if let Some(main_export) = pkg_json.main.clone() {
        return main_export;
    }

    if let Some(js_next_main_export) = pkg_json.js_next_main.clone() {
        return js_next_main_export;
    }

    String::from("index")
}

pub fn collect_pkg_entries(pkg_json: PackageJSON) -> Result<Vec<String>, ServerError> {
    let mut entries: Vec<String> = Vec::new();
    let mut has_main_export = false;

    if let Some(exports_field) = pkg_json.exports.clone() {
        match &exports_field {
            PackageJSONExport::Map(exports_map) => {
                for (key, value) in exports_map.iter() {
                    // Skip things with .node or .server as we don't care about node things in the browser
                    if key.contains(".node") || key.contains(".server") {
                        continue;
                    }

                    // If an export does not start with a dot it is a conditional group, handle it differently.
                    // Whoever invented this really does not respect tooling developers time
                    if !key.starts_with('.') {
                        let new_export_value = PackageJSONExport::Map(exports_map.clone());
                        if let Some(main_export) = get_export_entry(&new_export_value) {
                            has_main_export = true;
                            entries.push(main_export);
                        }
                        break;
                    }

                    // Export starts with a dot, now we have relative exports
                    if let Some(export_val) = get_export_entry(value) {
                        entries.push(export_val);

                        if key.eq(".") {
                            has_main_export = true;
                        }
                    }
                }
            }
            PackageJSONExport::Value(export_val) => {
                has_main_export = true;
                entries.push(export_val.clone());
            }
            PackageJSONExport::Vec(_) => {
                has_main_export = true;
                if let Some(found_export) = get_export_entry(&exports_field) {
                    entries.push(found_export);
                }
            }
            _ => {}
        }
    }

    // Fallback when there is no `exports["."]` (and no subpath exports collected).
    // Exports-only packages (`"main": false` + only `./foo` keys) must not gain a
    // phantom `index` entry.
    if !has_main_export && entries.is_empty() {
        entries.push(get_main_entry(&pkg_json));
    }

    let mut additional_exports = get_additional_exports(pkg_json.name.as_str());
    entries.append(&mut additional_exports);

    // Sort and deduplicate...
    entries.sort();
    entries.dedup();

    Ok(entries)
}

#[cfg(test)]
mod test {
    use crate::package::package_json::{
        collect_pkg_entries, parse_pkg_json, PackageJSONExport,
    };
    use crate::utils::test_utils;

    #[test]
    fn pkg_json_parse_test() {
        let content = test_utils::read_fixture("fixtures/pkg-json/parse-test.json").unwrap();
        let parsed = parse_pkg_json(content.clone()).unwrap();

        assert_eq!(parsed.name, "react");
        assert_eq!(parsed.version, "17.0.2");
        assert_eq!(parsed.js_next_main.unwrap(), "index.next.js");
        assert_eq!(parsed.main.unwrap(), "index.cjs");
        assert_eq!(parsed.module.unwrap(), "index.mjs");
        assert_eq!(
            match parsed.browser.unwrap() {
                PackageJSONExport::Value(v) => v,
                _ => panic!("incorrect browser value"),
            },
            "index.browser.js"
        );
    }

    #[test]
    fn pkg_json_accepts_main_false() {
        let content = r#"{
            "name": "math-intrinsics",
            "version": "1.1.0",
            "main": false,
            "exports": {
                "./abs": "./abs.js",
                "./package.json": "./package.json"
            }
        }"#;
        let parsed = parse_pkg_json(content.to_string()).unwrap();
        assert_eq!(parsed.name, "math-intrinsics");
        assert_eq!(parsed.main, None);
        let entries = collect_pkg_entries(parsed).unwrap();
        assert!(entries.iter().any(|e| e.ends_with("abs.js")));
        assert!(!entries.iter().any(|e| e == "index"));
    }

    #[test]
    fn pkg_json_tolerates_real_world_quirks() {
        let content = r#"{
            "name": "weird-pkg",
            "version": 2,
            "main": false,
            "module": null,
            "jsnext:main": "",
            "browser": { "./index.js": false, ".": "./browser.js" },
            "exports": [
                { "import": "./esm.js", "require": "./cjs.js", "default": "./esm.js" },
                null,
                true
            ],
            "dependencies": {
                "ok": "^1.0.0",
                "num": 1,
                "obj": { "version": "1.0.0" },
                "nil": null,
                "": "skip-empty-key"
            },
            "peerDependencies": {
                "react": ">=17",
                "bad": false
            },
            "unexpected": { "totally": "fine" }
        }"#;
        let parsed = parse_pkg_json(content.to_string()).unwrap();
        assert_eq!(parsed.name, "weird-pkg");
        assert_eq!(parsed.version, "2");
        assert_eq!(parsed.main, None);
        assert_eq!(parsed.module, None);
        assert_eq!(parsed.js_next_main, None);
        assert!(matches!(
            parsed.browser,
            Some(PackageJSONExport::Map(_))
        ));
        assert!(matches!(
            parsed.exports,
            Some(PackageJSONExport::Vec(_))
        ));
        let deps = parsed.dependencies.unwrap();
        assert_eq!(deps.get("ok").map(String::as_str), Some("^1.0.0"));
        assert_eq!(deps.get("num").map(String::as_str), Some("1"));
        assert!(!deps.contains_key("obj"));
        assert!(!deps.contains_key("nil"));
        let peers = parsed.peer_dependencies.unwrap();
        assert_eq!(peers.get("react").map(String::as_str), Some(">=17"));
        assert!(!peers.contains_key("bad"));
    }

    #[test]
    fn pkg_json_non_object_root_is_stub_not_error() {
        let parsed = parse_pkg_json("[]".to_string()).unwrap();
        assert_eq!(parsed.name, "");
        assert_eq!(parsed.main, None);
    }

    #[test]
    fn pkg_json_strips_bom() {
        let content = "\u{feff}{\"name\":\"x\",\"version\":\"1.0.0\",\"main\":\"index.js\"}";
        let parsed = parse_pkg_json(content.to_string()).unwrap();
        assert_eq!(parsed.name, "x");
        assert_eq!(parsed.main.as_deref(), Some("index.js"));
    }

    #[test]
    fn pkg_json_invalid_json_still_errors() {
        assert!(parse_pkg_json("{not json".to_string()).is_err());
    }
}
