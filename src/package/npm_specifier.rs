//! Parse bare import / `require()` string arguments into module-graph edges.
//!
//! Specifiers that are not relative files or npm packages (URI schemes,
//! `node:…`, absolute paths, garbage) are rejected — never coerced into a
//! fake package name like `https:` via `split('/')`.

/// A specifier the Sandpack package graph can act on.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ModuleSpecifier {
    /// `./foo`, `../bar`, `.`, `..`
    Relative(String),
    /// Registry package, optionally with a subpath (`react-dom/client`,
    /// `@swc/helpers/_/_interop_require_default`).
    Package {
        name: String,
        #[allow(dead_code)]
        subpath: Option<String>,
    },
}

/// Classify a static module specifier. `None` = not a graph edge (skip).
pub fn parse_module_specifier(spec: &str) -> Option<ModuleSpecifier> {
    let spec = spec.trim();
    if spec.is_empty() || spec.len() > 2048 {
        return None;
    }

    if is_relative(spec) {
        return Some(ModuleSpecifier::Relative(spec.to_string()));
    }

    // Absolute POSIX paths, Windows paths, URI schemes (`https://…`, `node:fs`,
    // `data:…`, bare `https:`) — never npm package names.
    if spec.starts_with('/') || looks_like_windows_path(spec) || has_uri_scheme(spec) {
        return None;
    }

    parse_package_import(spec).map(|(name, subpath)| ModuleSpecifier::Package { name, subpath })
}

/// Package name only (`react`, `@scope/name`), for `/dep_tree` keys.
pub fn package_name_from_specifier(spec: &str) -> Option<String> {
    match parse_module_specifier(spec)? {
        ModuleSpecifier::Package { name, .. } => Some(name),
        ModuleSpecifier::Relative(_) => None,
    }
}

/// True when `name` is a well-formed npm package name (no subpath).
pub fn is_valid_package_name(name: &str) -> bool {
    matches!(
        parse_module_specifier(name),
        Some(ModuleSpecifier::Package { subpath: None, .. })
    )
}

fn is_relative(spec: &str) -> bool {
    spec == "."
        || spec == ".."
        || spec.starts_with("./")
        || spec.starts_with("../")
}

fn looks_like_windows_path(spec: &str) -> bool {
    // `C:\…` or `C:/…`
    let mut chars = spec.chars();
    matches!(
        (chars.next(), chars.next()),
        (Some(d), Some(':')) if d.is_ascii_alphabetic()
    ) && matches!(chars.next(), Some('\\') | Some('/'))
}

/// RFC 3986 scheme: `ALPHA *(ALPHA / DIGIT / "+" / "-" / ".") ":"`
///
/// Matches `https://…`, `http:`, `node:fs`, `data:text/…`. Does **not** match
/// scoped packages (`@scope/name` — leading `@`) or ordinary names.
fn has_uri_scheme(spec: &str) -> bool {
    let bytes = spec.as_bytes();
    if bytes.is_empty() || !bytes[0].is_ascii_alphabetic() {
        return false;
    }
    let mut i = 1;
    while i < bytes.len() {
        match bytes[i] {
            b':' => return true,
            c if c.is_ascii_alphanumeric() || c == b'+' || c == b'.' || c == b'-' => i += 1,
            _ => return false,
        }
    }
    false
}

/// Split `name`, `name/sub`, `@scope/name`, `@scope/name/sub…` into package + subpath.
fn parse_package_import(spec: &str) -> Option<(String, Option<String>)> {
    if spec.starts_with('@') {
        let rest = &spec[1..];
        let slash = rest.find('/')?;
        let scope = &rest[..slash];
        let after_scope = &rest[slash + 1..];
        if after_scope.is_empty() || !is_valid_name_segment(scope) {
            return None;
        }
        let (pkg, sub) = match after_scope.find('/') {
            Some(i) => (&after_scope[..i], Some(after_scope[i + 1..].to_string())),
            None => (after_scope, None),
        };
        if !is_valid_name_segment(pkg) {
            return None;
        }
        if let Some(ref s) = sub {
            if s.is_empty() {
                return None;
            }
        }
        let name = format!("@{scope}/{pkg}");
        Some((name, sub))
    } else {
        let (pkg, sub) = match spec.find('/') {
            Some(i) => (&spec[..i], Some(spec[i + 1..].to_string())),
            None => (spec, None),
        };
        if !is_valid_name_segment(pkg) {
            return None;
        }
        if let Some(ref s) = sub {
            if s.is_empty() {
                return None;
            }
        }
        Some((pkg.to_string(), sub))
    }
}

/// One segment of an npm package name (scope or name). Lenient on case so
/// legacy registry packages still resolve; rejects empty / unsafe chars.
fn is_valid_name_segment(seg: &str) -> bool {
    if seg.is_empty() || seg.len() > 214 {
        return false;
    }
    // Must not look like a URI scheme residue (`https:`) — no colons.
    if seg.contains(':') {
        return false;
    }
    let mut chars = seg.chars();
    let Some(first) = chars.next() else {
        return false;
    };
    // npm: no leading `.` / `_` on unscoped; scopes allow more — we still
    // require a URL-safe body. First char: alnum or `@` already stripped.
    if !(first.is_ascii_alphanumeric() || first == '~') {
        return false;
    }
    chars.all(|c| {
        c.is_ascii_alphanumeric() || matches!(c, '-' | '_' | '.' | '~')
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn relative_specifiers() {
        assert!(matches!(
            parse_module_specifier("./foo.js"),
            Some(ModuleSpecifier::Relative(_))
        ));
        assert!(matches!(
            parse_module_specifier("../bar"),
            Some(ModuleSpecifier::Relative(_))
        ));
    }

    #[test]
    fn bare_and_scoped_packages() {
        assert_eq!(
            parse_module_specifier("react"),
            Some(ModuleSpecifier::Package {
                name: "react".into(),
                subpath: None
            })
        );
        assert_eq!(
            parse_module_specifier("react-dom/client"),
            Some(ModuleSpecifier::Package {
                name: "react-dom".into(),
                subpath: Some("client".into())
            })
        );
        assert_eq!(
            parse_module_specifier("@swc/helpers"),
            Some(ModuleSpecifier::Package {
                name: "@swc/helpers".into(),
                subpath: None
            })
        );
        assert_eq!(
            parse_module_specifier("@swc/helpers/_/_interop_require_default"),
            Some(ModuleSpecifier::Package {
                name: "@swc/helpers".into(),
                subpath: Some("_/_interop_require_default".into())
            })
        );
        assert_eq!(package_name_from_specifier("lucide-react"), Some("lucide-react".into()));
    }

    #[test]
    fn rejects_uri_schemes_and_paths() {
        assert_eq!(parse_module_specifier("https:"), None);
        assert_eq!(parse_module_specifier("https://cdn.example.com/x.js"), None);
        assert_eq!(parse_module_specifier("http://example.com"), None);
        assert_eq!(parse_module_specifier("node:fs"), None);
        assert_eq!(parse_module_specifier("data:text/javascript,1"), None);
        assert_eq!(parse_module_specifier("blob:http://localhost/uuid"), None);
        // Other ecosystems — rejected by the same URI-scheme rule, not a denylist.
        assert_eq!(parse_module_specifier("jsr:@std/fs"), None);
        assert_eq!(parse_module_specifier("npm:react@19"), None);
        assert_eq!(parse_module_specifier("deno:std/path"), None);
        assert_eq!(parse_module_specifier("bun:sqlite"), None);
        assert_eq!(parse_module_specifier("/abs/path"), None);
        assert_eq!(parse_module_specifier("C:\\Windows\\x"), None);
        assert_eq!(parse_module_specifier(""), None);
        // Incomplete scope
        assert_eq!(parse_module_specifier("@scope"), None);
        assert_eq!(parse_module_specifier("@scope/"), None);
    }

    #[test]
    fn url_split_never_becomes_package_https() {
        // The old bug: `"https://x".split('/')[0] == "https:"`
        assert!(package_name_from_specifier("https://esm.sh/react").is_none());
        assert!(!is_valid_package_name("https:"));
    }
}
