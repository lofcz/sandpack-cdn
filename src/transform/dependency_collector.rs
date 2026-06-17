use std::collections::HashSet;

use swc_core::common::SyntaxContext;
use swc_core::ecma::ast::{Callee, CallExpr, Expr};
use swc_core::ecma::atoms::Atom;
use swc_core::ecma::visit::{Visit, VisitWith};

use super::utils::match_str;

/// Collects `require("specifier")` calls so the CDN knows which packages a file
/// depends on. Calls to a locally-declared `require` are ignored.
pub struct DependencyCollector<'a> {
    pub items: &'a mut HashSet<String>,
    pub decls: &'a HashSet<(Atom, SyntaxContext)>,
}

impl Visit for DependencyCollector<'_> {
    fn visit_call_expr(&mut self, node: &CallExpr) {
        node.visit_children_with(self);

        let Callee::Expr(callee) = &node.callee else {
            return;
        };

        if let Expr::Ident(ident) = &**callee {
            // Bail if `require` is shadowed by a local declaration.
            if self.decls.contains(&ident.to_id()) {
                return;
            }

            if ident.sym.as_str() != "require" {
                return;
            }

            if let Some(arg) = node.args.first() {
                if let Some((specifier, _)) = match_str(&arg.expr) {
                    self.items.insert(specifier.to_string());
                }
            }
        }
    }
}
