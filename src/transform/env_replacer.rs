use std::collections::{HashMap, HashSet};

use swc_core::common::{SyntaxContext, DUMMY_SP};
use swc_core::ecma::ast::{Bool, Expr, Ident, IdentName, Lit, MemberProp, Str};
use swc_core::ecma::atoms::Atom;
use swc_core::ecma::visit::{VisitMut, VisitMutWith};

use super::utils::match_member_expr;

/// Inlines `process.env.<NAME>` and `process.browser` so the simplifier can
/// fold the resulting constants and drop dead branches.
pub struct EnvReplacer<'a> {
    pub is_browser: bool,
    pub env: &'a HashMap<Atom, Atom>,
    pub decls: &'a HashSet<(Atom, SyntaxContext)>,
}

impl VisitMut for EnvReplacer<'_> {
    fn visit_mut_expr(&mut self, node: &mut Expr) {
        if let Expr::Member(member) = node {
            // process.browser -> true
            if self.is_browser
                && match_member_expr(member, vec!["process", "browser"], self.decls)
            {
                *node = Expr::Lit(Lit::Bool(Bool {
                    span: DUMMY_SP,
                    value: true,
                }));
                return;
            }

            // process.env.<NAME> -> "value" | undefined
            if let Expr::Member(inner) = &*member.obj {
                if match_member_expr(inner, vec!["process", "env"], self.decls) {
                    if let MemberProp::Ident(IdentName { sym, .. }) = &member.prop {
                        if let Some(replacement) = self.replace(sym, true) {
                            *node = replacement;
                            return;
                        }
                    }
                }
            }
        }

        node.visit_mut_children_with(self);
    }
}

impl EnvReplacer<'_> {
    fn replace(&self, sym: &Atom, fallback_undefined: bool) -> Option<Expr> {
        if let Some(val) = self.env.get(sym) {
            return Some(Expr::Lit(Lit::Str(Str {
                span: DUMMY_SP,
                value: val.clone().into(),
                raw: None,
            })));
        } else if fallback_undefined {
            match &**sym {
                // don't replace process.env.hasOwnProperty etc. with undefined
                "hasOwnProperty"
                | "isPrototypeOf"
                | "propertyIsEnumerable"
                | "toLocaleString"
                | "toSource"
                | "toString"
                | "valueOf" => {}
                _ => {
                    return Some(Expr::Ident(Ident::new_no_ctxt("undefined".into(), DUMMY_SP)));
                }
            };
        }
        None
    }
}
