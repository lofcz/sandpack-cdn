use std::collections::HashSet;

use swc_core::common::{Span, SyntaxContext};
use swc_core::ecma::ast::{Expr, Lit, MemberExpr, MemberProp};
use swc_core::ecma::atoms::Atom;

pub fn match_member_expr(
    expr: &MemberExpr,
    idents: Vec<&str>,
    decls: &HashSet<(Atom, SyntaxContext)>,
) -> bool {
    let mut member = expr;
    let mut idents = idents;
    while idents.len() > 1 {
        let expected = idents.pop().unwrap();
        let prop = match &member.prop {
            MemberProp::Ident(ident_name) => &ident_name.sym,
            _ => return false,
        };

        if prop.as_str() != expected {
            return false;
        }

        match &*member.obj {
            Expr::Member(m) => member = m,
            Expr::Ident(ident) => {
                return idents.len() == 1
                    && ident.sym.as_str() == idents.pop().unwrap()
                    && !decls.contains(&(ident.sym.clone(), ident.ctxt));
            }
            _ => return false,
        }
    }

    false
}

pub fn match_str(node: &Expr) -> Option<(Atom, Span)> {
    match node {
        // "string" or 'string'
        Expr::Lit(Lit::Str(s)) => Some((s.value.to_atom_lossy().into_owned(), s.span)),
        // `string` — prefer cooked (decoded) over raw (escape sequences).
        Expr::Tpl(tpl) if tpl.quasis.len() == 1 && tpl.exprs.is_empty() => {
            let quasi = &tpl.quasis[0];
            let value = quasi
                .cooked
                .as_ref()
                .map(|c| c.to_atom_lossy().into_owned())
                .unwrap_or_else(|| quasi.raw.clone());
            Some((value, tpl.span))
        }
        _ => None,
    }
}
