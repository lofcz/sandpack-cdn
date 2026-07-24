use std::collections::HashSet;

use swc_core::common::SyntaxContext;
use swc_core::ecma::ast::{
    CallExpr, Callee, ExportAll, Expr, ImportDecl, Lit, NamedExport, Str,
};
use swc_core::ecma::atoms::Atom;
use swc_core::ecma::visit::{Visit, VisitWith};

use crate::package::npm_specifier::parse_module_specifier;

use super::utils::match_str;

/// Collects static module edges from `require("…")` and ESM `import`/`export … from`.
///
/// Only relative paths and well-formed npm package imports are kept. URI
/// schemes (`https://…`, `node:fs`, …) and other non-graph strings are ignored
/// so they never become fake package names in `/dep_tree`.
pub struct DependencyCollector<'a> {
    pub items: &'a mut HashSet<String>,
    pub decls: &'a HashSet<(Atom, SyntaxContext)>,
}

impl DependencyCollector<'_> {
    fn consider(&mut self, specifier: &str) {
        if parse_module_specifier(specifier).is_some() {
            self.items.insert(specifier.to_string());
        }
    }

    fn consider_str_lit(&mut self, s: &Str) {
        // `Str::value` is Wtf8Atom; prefer UTF-8 view, else lossy.
        let owned = s.value.to_atom_lossy();
        self.consider(owned.as_ref());
    }
}

impl Visit for DependencyCollector<'_> {
    fn visit_call_expr(&mut self, node: &CallExpr) {
        node.visit_children_with(self);

        let Callee::Expr(callee) = &node.callee else {
            return;
        };

        let Expr::Ident(ident) = &**callee else {
            return;
        };

        // Bail if `require` is shadowed by a local declaration.
        if self.decls.contains(&ident.to_id()) {
            return;
        }

        if ident.sym.as_str() != "require" {
            return;
        }

        // Static string argument only — no `require(foo)`, no templates with
        // expressions (those are dynamic and not package-graph edges).
        if let Some(arg) = node.args.first() {
            if let Some((specifier, _)) = match_str(&arg.expr) {
                self.consider(specifier.as_str());
            }
        }
    }

    fn visit_import_decl(&mut self, node: &ImportDecl) {
        self.consider_str_lit(&node.src);
        node.visit_children_with(self);
    }

    fn visit_export_all(&mut self, node: &ExportAll) {
        self.consider_str_lit(&node.src);
        node.visit_children_with(self);
    }

    fn visit_named_export(&mut self, node: &NamedExport) {
        if let Some(src) = &node.src {
            self.consider_str_lit(src);
        }
        node.visit_children_with(self);
    }

    fn visit_lit(&mut self, _node: &Lit) {
        // Don't walk into every literal — deps come from call/import sites.
    }
}
