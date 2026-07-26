use std::collections::{HashMap, HashSet};

use swc_core::common::comments::SingleThreadedComments;
use swc_core::common::{sync::Lrc, FileName, Globals, Mark, SourceMap, SyntaxContext, GLOBALS};
use swc_core::ecma::ast::{EsVersion, Module, Program};
use swc_core::ecma::atoms::Atom;
use swc_core::ecma::codegen::text_writer::JsWriter;
use swc_core::ecma::codegen::{Config as CodegenConfig, Emitter};
use swc_core::ecma::parser::lexer::Lexer;
use swc_core::ecma::parser::{EsSyntax, Parser, StringInput, Syntax};
use swc_core::ecma::transforms::base::fixer::fixer;
use swc_core::ecma::transforms::base::helpers::{inject_helpers, Helpers, HELPERS};
use swc_core::ecma::transforms::base::hygiene::hygiene;
use swc_core::ecma::transforms::base::resolver;
use swc_core::ecma::transforms::module::common_js::{common_js, Config as CommonJsConfig};
use swc_core::ecma::transforms::module::path::Resolver;
use swc_core::ecma::transforms::optimization::simplify::expr::Config as SimplifyExprConfig;
use swc_core::ecma::transforms::optimization::simplify::{dead_branch_remover, expr_simplifier};
use swc_core::ecma::utils::collect_decls;
use swc_core::ecma::visit::{VisitMutWith, VisitWith};
use tracing::info;

use crate::app_error::ServerError;

use super::dependency_collector::DependencyCollector;
use super::env_replacer::EnvReplacer;

#[derive(Debug)]
pub struct TransformedFile {
    pub content: String,
    pub dependencies: HashSet<String>,
}

fn parse(
    code: &str,
    source_map: &Lrc<SourceMap>,
    comments: &SingleThreadedComments,
) -> Result<Module, ServerError> {
    let source_file = source_map.new_source_file(FileName::Anon.into(), code.to_string());

    let syntax = Syntax::Es(EsSyntax {
        jsx: false,
        export_default_from: true,
        decorators: true,
        ..Default::default()
    });

    let lexer = Lexer::new(
        syntax,
        EsVersion::latest(),
        StringInput::from(&*source_file),
        Some(comments),
    );

    let mut parser = Parser::new_from(lexer);
    parser.parse_module().map_err(|err| ServerError::SWCParseError {
        message: format!("{:?}", err),
    })
}

#[tracing::instrument(name = "transform_file", skip(code))]
pub fn transform_file(filename: &str, code: &str) -> Result<TransformedFile, ServerError> {
    info!("Transforming file: {}", filename);

    // Error early if filename does not end in js: js, cjs, mjs, ...
    let ext = &filename[filename.len() - 2..];
    if ext.ne("js") {
        return Err(ServerError::SWCParseError {
            message: format!("File {} is not JavaScript", filename),
        });
    }

    let source_map = Lrc::new(SourceMap::default());
    let comments = SingleThreadedComments::default();
    let module = parse(code, &source_map, &comments)?;

    // SWC's visit/fold passes recurse deeply (common_js, simplify, ...). The
    // required depth easily exceeds the 2 MB default Windows thread stack (Linux
    // defaults to 8 MB, which is why this only bit on Windows). Grow the stack on
    // demand instead of relying on the OS-provided size so debug builds, tests
    // and the server all behave identically. `maybe_grow` runs on the same thread
    // so thread-locals (GLOBALS/HELPERS) stay intact.
    stacker::maybe_grow(2 * 1024 * 1024, 64 * 1024 * 1024, move || {
        GLOBALS.set(&Globals::new(), || {
            // Inline helpers (requires swc_core feature `ecma_helpers_inline`).
            // Sandpack evals each file as a CJS function body — pulling
            // `@swc/helpers` as a runtime dep is the wrong model (extra packages,
            // cache keys, and import/require ordering traps). Order is the SWC
            // contract: transforms that *use* helpers first, then inject_helpers
            // to materialize their bodies in-file.
            HELPERS.set(&Helpers::new(/* external */ false), || {
                    let unresolved_mark = Mark::new();
                    let top_level_mark = Mark::new();

                    let mut program =
                        Program::Module(module).apply(resolver(unresolved_mark, top_level_mark, false));

                    // Inline process.env.NODE_ENV / process.browser so that the
                    // simplifier can drop dead branches (and we don't collect deps
                    // that live inside always-false conditionals).
                    let mut env: HashMap<Atom, Atom> = HashMap::new();
                    env.insert("NODE_ENV".into(), "development".into());

                    let decls: HashSet<(Atom, SyntaxContext)> =
                        collect_decls(&program).into_iter().collect();
                    program.visit_mut_with(&mut EnvReplacer {
                        env: &env,
                        is_browser: true,
                        decls: &decls,
                    });

                    let program = program
                        .apply(expr_simplifier(unresolved_mark, SimplifyExprConfig::default()))
                        .apply(dead_branch_remover(unresolved_mark))
                        .apply(common_js(
                            Resolver::Default,
                            unresolved_mark,
                            CommonJsConfig::default(),
                            Default::default(),
                        ))
                        .apply(inject_helpers(unresolved_mark));

                    // Collect dependencies after CJS so helper/import edges are
                    // visible as `require("…")` calls.
                    let decls: HashSet<(Atom, SyntaxContext)> =
                        collect_decls(&program).into_iter().collect();
                    let mut dependencies: HashSet<String> = HashSet::new();
                    program.visit_with(&mut DependencyCollector {
                        items: &mut dependencies,
                        decls: &decls,
                    });

                    let program = program.apply(hygiene()).apply(fixer(Some(&comments)));

                    // Remove sourcemap comment
                    {
                        let (mut _leading_comments, mut trailing_comments) =
                            comments.borrow_all_mut();
                        for (_key, value) in trailing_comments.iter_mut() {
                            if let Some(index) = value.iter().position(|comment| {
                                comment.text.starts_with("# sourceMappingURL")
                            }) {
                                value.remove(index);
                            }
                        }
                    }

                    // Print code...
                    let mut buf = vec![];
                    {
                        let mut emitter = Emitter {
                            cfg: CodegenConfig::default().with_minify(true),
                            comments: Some(&comments),
                            cm: source_map.clone(),
                            wr: JsWriter::new(source_map.clone(), "\n", &mut buf, None),
                        };
                        emitter.emit_program(&program)?;
                    }

                    let output = String::from(std::str::from_utf8(&buf).unwrap_or(""));

                    Ok(TransformedFile {
                        content: output,
                        dependencies,
                    })
                },
            )
        })
    })
}

#[cfg(test)]
mod test {
    use crate::transform::transformer::transform_file;

    #[test]
    fn inlines_env_variables() {
        assert_eq!(
            transform_file("index.js", "module.exports = process.env.NODE_ENV;")
                .unwrap()
                .content,
            String::from("\"use strict\";module.exports=\"development\";")
        );
    }

    #[test]
    fn collects_conditional_require_deps() {
        let code = "'use strict';\n\nif (process.env.NODE_ENV === 'production') {\n  module.exports = require('./cjs/react.production.min.js');\n} else {\n  module.exports = require('./cjs/react.development.js');\n}\n";
        let res = transform_file("index.js", code).unwrap();
        println!("CONTENT >>>{}<<<", res.content);
        println!("DEPS >>>{:?}<<<", res.dependencies);
        assert!(
            res.dependencies.contains("./cjs/react.development.js"),
            "expected development require to be collected, got {:?}",
            res.dependencies
        );
    }

    #[test]
    fn remove_sourcemap_comment() {
        // TODO: Allow inline sourcemaps?
        assert_eq!(
            transform_file(
                "index.js",
                "module.exports = \"hello world\";\n//other-comment\n//# sourceMappingURL=index.js.map"
            )
            .unwrap()
            .content,
            String::from("\"use strict\";module.exports=\"hello world\";//other-comment\n")
        );
    }

    #[test]
    fn esm_star_import_inlines_interop_helpers() {
        let res = transform_file(
            "index.js",
            "import * as icons from './icons.js';\nexport default icons;\n",
        )
        .unwrap();
        assert!(
            !res.content.contains("import "),
            "expected no residual ESM import, got {}",
            res.content
        );
        assert!(
            !res.content.contains("@swc/helpers"),
            "helpers must be inlined, not external: {}",
            res.content
        );
        assert!(
            res.dependencies.iter().all(|d| !d.starts_with("@swc/helpers")),
            "expected no @swc/helpers dep edge, got {:?}",
            res.dependencies
        );
        if res.content.contains("_interop_require_wildcard") {
            assert!(
                res.content.contains("function _interop_require_wildcard"),
                "helper call without inlined definition: {}",
                res.content
            );
        }
    }

}
