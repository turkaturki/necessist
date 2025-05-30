use crate::{
    AbstractTypes, GenericVisitor, MaybeNamed, Named, OutputAccessors, OutputStrippedOfAnsiScapes,
    ParseLow, Spanned, WalkDirResult,
};
use anyhow::{Result, anyhow};
use if_chain::if_chain;
use log::debug;
use necessist_core::{
    LightContext, LineColumn, SourceFile, Span, WarnFlags, Warning,
    framework::{Postprocess, SpanTestMaps, TestSet},
    source_warn, util,
};
use std::{
    cell::RefCell,
    collections::BTreeMap,
    convert::Infallible,
    ffi::OsStr,
    path::{Path, PathBuf},
    process::Command,
    rc::Rc,
};
use subprocess::{Exec, NullFile};
use swc_core::{
    common::{
        BytePos, Loc, SourceMap, Span as SwcSpan, Spanned as SwcSpanned, source_map::SmallPos,
    },
    ecma::{
        ast::{
            ArrowExpr, AwaitExpr, BlockStmtOrExpr, CallExpr, Callee, EsVersion, Expr, ExprStmt,
            FnDecl, Invalid, Lit, MemberExpr, MemberProp, Module, Stmt, Str,
        },
        atoms::Atom,
        parser::{Parser, StringInput, Syntax, TsSyntax, lexer::Lexer},
    },
};

mod storage;
use storage::Storage;

mod visitor;
use visitor::{collect_local_functions, visit};

static INVALID: Expr = Expr::Invalid(Invalid {
    span: SwcSpan {
        lo: BytePos(0),
        hi: BytePos(0),
    },
});

#[derive(Debug, Eq, PartialEq)]
enum ItMessageState {
    NotFound,
    Found,
    WarningEmitted,
}

impl Default for ItMessageState {
    fn default() -> Self {
        Self::NotFound
    }
}

#[allow(clippy::type_complexity)]
pub struct Inner {
    subdir: PathBuf,
    path_predicate: &'static dyn Fn(&Path) -> bool,
    it_message_extractor: Box<dyn Fn(&[u8]) -> Result<Vec<String>>>,
    source_map: Rc<SourceMap>,
    source_file_it_message_state_map: RefCell<BTreeMap<PathBuf, BTreeMap<String, ItMessageState>>>,
}

impl Inner {
    // smoelius: `path_predicate` and `it_message_extractor` are hacks to support Vitest:
    // - `path_predicate` allows filtering out paths of the form `*.test-d.ts`, which are "type"
    //   tests. See: https://vitest.dev/guide/testing-types.html#testing-types
    // - `it_message_extractor` allows parsing JSON output from Vitest.
    // We should look into producing JSON from Mocha as well, though.
    #[allow(clippy::type_complexity)]
    pub fn new(
        subdir: impl AsRef<Path>,
        path_predicate: Option<&'static dyn Fn(&Path) -> bool>,
        it_message_extractor: Box<dyn Fn(&[u8]) -> Result<Vec<String>>>,
    ) -> Self {
        fn default_path_predicate(_: &Path) -> bool {
            true
        }

        Self {
            subdir: subdir.as_ref().to_path_buf(),
            path_predicate: path_predicate.unwrap_or(&default_path_predicate),
            it_message_extractor,
            source_map: Rc::default(),
            source_file_it_message_state_map: RefCell::new(BTreeMap::new()),
        }
    }

    pub fn dry_run(
        &self,
        _context: &LightContext,
        source_file: &Path,
        mut command: Command,
    ) -> Result<()> {
        debug!("{command:?}");

        let output = command.output_stripped_of_ansi_escapes()?;
        if !output.status().success() {
            return Err(output.into());
        }

        let mut source_file_it_message_state_map =
            self.source_file_it_message_state_map.borrow_mut();
        let it_message_state_map = source_file_it_message_state_map
            .entry(source_file.to_path_buf())
            .or_default();

        for it_message in (self.it_message_extractor)(output.stdout())? {
            it_message_state_map.insert(it_message, ItMessageState::Found);
        }

        Ok(())
    }

    #[allow(clippy::unnecessary_wraps, clippy::unused_self)]
    pub fn statement_prefix_and_suffix(&self, span: &Span) -> Result<(String, String)> {
        Ok((
            format!(
                r#"if (process.env.NECESSIST_REMOVAL != "{}") {{ "#,
                span.id()
            ),
            " }".to_owned(),
        ))
    }

    pub fn exec(
        &self,
        context: &LightContext,
        test_name: &str,
        span: &Span,
        command: &Command,
    ) -> Result<Option<(Exec, Option<Box<Postprocess>>)>> {
        let mut source_file_it_message_state_map =
            self.source_file_it_message_state_map.borrow_mut();
        #[allow(clippy::expect_used)]
        let it_message_state_map = source_file_it_message_state_map
            .get_mut(span.source_file.as_ref())
            .expect("Source file is not in map");

        // smoelius: For Mocha-based frameworks, `test_name` is the `it` message.
        let state = it_message_state_map
            .entry(test_name.to_owned())
            .or_default();
        if *state != ItMessageState::Found {
            if *state == ItMessageState::NotFound {
                source_warn(
                    context,
                    Warning::ItMessageNotFound,
                    span,
                    &format!("`it` message {test_name:?} was not found during dry run"),
                    WarnFlags::empty(),
                )?;
                *state = ItMessageState::WarningEmitted;
            }
            // smoelius: Returning `None` here causes Necessist to associate `Outcome::Nonbuildable`
            // with this span. This is not ideal, but there is no ideal choice for this situation
            // currently.
            return Ok(None);
        }

        let mut exec = util::exec_from_command(command);
        exec = exec.stdout(NullFile);
        exec = exec.stderr(NullFile);

        debug!("{exec:?}");

        Ok(Some((exec, None)))
    }
}

#[derive(Clone, Copy)]
pub struct Test<'ast> {
    it_message: &'ast Atom,
    stmts: &'ast Vec<Stmt>,
}

pub struct SourceMapped<'ast, T> {
    source_map: &'ast Rc<SourceMap>,
    node: &'ast T,
}

impl<T> Clone for SourceMapped<'_, T> {
    fn clone(&self) -> Self {
        *self
    }
}

impl<T> Copy for SourceMapped<'_, T> {}

impl<T: PartialEq> PartialEq for SourceMapped<'_, T> {
    // smoelius: Remove this `#[allow(..)]` once the following pull request appears in nightly:
    // https://github.com/rust-lang/rust-clippy/pull/12137
    #[allow(clippy::unconditional_recursion)]
    fn eq(&self, other: &Self) -> bool {
        self.node.eq(other.node)
    }
}

impl<T: Eq> Eq for SourceMapped<'_, T> {}

impl<T: SwcSpanned> Spanned for SourceMapped<'_, T> {
    fn span(&self, source_file: &SourceFile) -> Span {
        SwcSpanned::span(self.node).to_internal_span(self.source_map, source_file)
    }
}

pub struct Types;

impl AbstractTypes for Types {
    type Storage<'ast> = Storage<'ast>;
    type File = (Rc<SourceMap>, Module);
    type Test<'ast> = Test<'ast>;
    type LocalFunction<'ast> = &'ast FnDecl;
    type Statement<'ast> = SourceMapped<'ast, Stmt>;
    type Expression<'ast> = SourceMapped<'ast, Expr>;
    type Await<'ast> = &'ast AwaitExpr;
    type Field<'ast> = SourceMapped<'ast, MemberExpr>;
    type Call<'ast> = SourceMapped<'ast, CallExpr>;
    type MacroCall<'ast> = Infallible;
}

impl Named for Test<'_> {
    fn name(&self) -> String {
        self.it_message.to_string()
    }
}

impl MaybeNamed for <Types as AbstractTypes>::Expression<'_> {
    fn name(&self) -> Option<String> {
        if let Expr::Ident(ident) = self.node {
            Some(ident.as_ref().to_owned())
        } else {
            None
        }
    }
}

impl MaybeNamed for <Types as AbstractTypes>::Field<'_> {
    fn name(&self) -> Option<String> {
        if let MemberProp::Ident(ident) = &self.node.prop {
            Some(ident.as_ref().to_owned())
        } else {
            None
        }
    }
}

impl MaybeNamed for <Types as AbstractTypes>::Call<'_> {
    fn name(&self) -> Option<String> {
        if_chain! {
            if let Callee::Expr(callee) = &self.node.callee;
            if let Expr::Ident(ident) = &**callee;
            then {
                Some(ident.as_ref().to_owned())
            } else {
                None
            }
        }
    }
}

impl ParseLow for Inner {
    type Types = Types;

    const IGNORED_FUNCTIONS: Option<&'static [&'static str]> =
        Some(&["assert", "assert.*", "console.*", "expect"]);

    const IGNORED_MACROS: Option<&'static [&'static str]> = None;

    const IGNORED_METHODS: Option<&'static [&'static str]> = Some(&["toNumber", "toString"]);

    fn walk_dir(&self, root: &Path) -> Box<dyn Iterator<Item = WalkDirResult>> {
        let path_predicate = self.path_predicate;
        Box::new(
            walkdir::WalkDir::new(root.join(&self.subdir))
                .into_iter()
                .filter_entry(move |entry| {
                    let path = entry.path();
                    if !path.is_file() {
                        return true;
                    }
                    (path.extension() == Some(OsStr::new("js"))
                        || path.extension() == Some(OsStr::new("ts")))
                        && (path_predicate)(path)
                }),
        )
    }

    fn parse_source_file(
        &self,
        source_file: &Path,
    ) -> Result<<Self::Types as AbstractTypes>::File> {
        let source_file = self.source_map.load_file(source_file)?;
        let lexer = Lexer::new(
            Syntax::Typescript(TsSyntax::default()),
            EsVersion::default(),
            StringInput::from(&*source_file),
            None,
        );
        let mut parser = Parser::new_from(lexer);
        parser
            .parse_typescript_module()
            .map(|module| (self.source_map.clone(), module))
            .map_err(|error| anyhow!(format!("{error:?}")))
    }

    fn storage_from_file<'ast>(
        &self,
        file: &'ast <Self::Types as AbstractTypes>::File,
    ) -> <Self::Types as AbstractTypes>::Storage<'ast> {
        Storage::new(file)
    }

    fn local_functions<'ast>(
        &self,
        _storage: &RefCell<<Self::Types as AbstractTypes>::Storage<'ast>>,
        file: &'ast <Self::Types as AbstractTypes>::File,
    ) -> Result<BTreeMap<String, Vec<<Self::Types as AbstractTypes>::LocalFunction<'ast>>>> {
        Ok(collect_local_functions(&file.1))
    }

    fn visit_file<'ast>(
        generic_visitor: GenericVisitor<'_, '_, '_, 'ast, Self>,
        storage: &RefCell<<Self::Types as AbstractTypes>::Storage<'ast>>,
        file: &'ast <Self::Types as AbstractTypes>::File,
    ) -> Result<(TestSet, SpanTestMaps)> {
        visit(generic_visitor, storage, &file.1)
    }

    fn test_statements<'ast>(
        &self,
        storage: &RefCell<<Self::Types as AbstractTypes>::Storage<'ast>>,
        test: <Self::Types as AbstractTypes>::Test<'ast>,
    ) -> Vec<<Self::Types as AbstractTypes>::Statement<'ast>> {
        test.stmts
            .iter()
            .map(|stmt| SourceMapped {
                source_map: storage.borrow().source_map,
                node: stmt,
            })
            .collect()
    }

    fn statement_is_removable(
        &self,
        _statement: <Self::Types as AbstractTypes>::Statement<'_>,
    ) -> bool {
        true
    }

    fn statement_is_expression<'ast>(
        &self,
        storage: &RefCell<<Self::Types as AbstractTypes>::Storage<'ast>>,
        statement: <Self::Types as AbstractTypes>::Statement<'ast>,
    ) -> Option<<Self::Types as AbstractTypes>::Expression<'ast>> {
        if let Stmt::Expr(ExprStmt { expr, .. }) = statement.node {
            Some(SourceMapped {
                source_map: storage.borrow().source_map,
                node: expr,
            })
        } else {
            None
        }
    }

    fn statement_is_control<'ast>(
        &self,
        _storage: &RefCell<<Self::Types as AbstractTypes>::Storage<'ast>>,
        statement: <Self::Types as AbstractTypes>::Statement<'ast>,
    ) -> bool {
        matches!(
            statement.node,
            Stmt::Break(_) | Stmt::Continue(_) | Stmt::Return(_) | Stmt::Throw(_)
        )
    }

    fn statement_is_declaration<'ast>(
        &self,
        _storage: &RefCell<<Self::Types as AbstractTypes>::Storage<'ast>>,
        statement: <Self::Types as AbstractTypes>::Statement<'ast>,
    ) -> bool {
        matches!(statement.node, Stmt::Decl(_))
    }

    fn expression_is_await<'ast>(
        &self,
        _storage: &RefCell<<Self::Types as AbstractTypes>::Storage<'ast>>,
        expression: <Self::Types as AbstractTypes>::Expression<'ast>,
    ) -> Option<<Self::Types as AbstractTypes>::Await<'ast>> {
        if let Expr::Await(await_) = expression.node {
            Some(await_)
        } else {
            None
        }
    }

    fn expression_is_field<'ast>(
        &self,
        storage: &RefCell<<Self::Types as AbstractTypes>::Storage<'ast>>,
        expression: <Self::Types as AbstractTypes>::Expression<'ast>,
    ) -> Option<<Self::Types as AbstractTypes>::Field<'ast>> {
        if let Expr::Member(member) = expression.node {
            Some(SourceMapped {
                source_map: storage.borrow().source_map,
                node: member,
            })
        } else {
            None
        }
    }

    fn expression_is_call<'ast>(
        &self,
        storage: &RefCell<<Self::Types as AbstractTypes>::Storage<'ast>>,
        expression: <Self::Types as AbstractTypes>::Expression<'ast>,
    ) -> Option<<Self::Types as AbstractTypes>::Call<'ast>> {
        if let Expr::Call(call) = expression.node {
            Some(SourceMapped {
                source_map: storage.borrow().source_map,
                node: call,
            })
        } else {
            None
        }
    }

    fn expression_is_macro_call<'ast>(
        &self,
        _storage: &RefCell<<Self::Types as AbstractTypes>::Storage<'ast>>,
        _expression: <Self::Types as AbstractTypes>::Expression<'ast>,
    ) -> Option<<Self::Types as AbstractTypes>::MacroCall<'ast>> {
        None
    }

    fn await_arg<'ast>(
        &self,
        storage: &RefCell<<Self::Types as AbstractTypes>::Storage<'ast>>,
        await_: <Self::Types as AbstractTypes>::Await<'ast>,
    ) -> <Self::Types as AbstractTypes>::Expression<'ast> {
        SourceMapped {
            source_map: storage.borrow().source_map,
            node: &*await_.arg,
        }
    }

    fn field_base<'ast>(
        &self,
        _storage: &RefCell<<Self::Types as AbstractTypes>::Storage<'ast>>,
        field: <Self::Types as AbstractTypes>::Field<'ast>,
    ) -> <Self::Types as AbstractTypes>::Expression<'ast> {
        SourceMapped {
            source_map: field.source_map,
            node: &*field.node.obj,
        }
    }

    fn call_callee<'ast>(
        &self,
        _storage: &RefCell<<Self::Types as AbstractTypes>::Storage<'ast>>,
        call: <Self::Types as AbstractTypes>::Call<'ast>,
    ) -> <Self::Types as AbstractTypes>::Expression<'ast> {
        if let Callee::Expr(expr) = &call.node.callee {
            SourceMapped {
                source_map: call.source_map,
                node: expr,
            }
        } else {
            SourceMapped {
                source_map: call.source_map,
                node: &INVALID,
            }
        }
    }

    fn macro_call_callee<'ast>(
        &self,
        _storage: &RefCell<<Self::Types as AbstractTypes>::Storage<'ast>>,
        _macro_call: <Self::Types as AbstractTypes>::MacroCall<'ast>,
    ) -> <Self::Types as AbstractTypes>::Expression<'ast> {
        unreachable!()
    }
}

fn is_it_call_stmt(stmt: &Stmt) -> Option<Test<'_>> {
    if let Stmt::Expr(ExprStmt { expr, .. }) = stmt {
        is_it_call_expr(expr)
    } else {
        None
    }
}

fn is_it_call_expr(expr: &Expr) -> Option<Test<'_>> {
    if_chain! {
        if let Expr::Call(CallExpr {
            callee: Callee::Expr(callee),
            args,
            ..
        }) = expr;
        if let Expr::Ident(ident) = &**callee;
        if ident.as_ref() == "it";
        if let [arg0, arg1] = args.as_slice();
        if let Expr::Lit(Lit::Str(Str { value, .. })) = &*arg0.expr;
        if let Expr::Arrow(ArrowExpr { body, .. }) = &*arg1.expr;
        if let BlockStmtOrExpr::BlockStmt(block) = &**body;
        then {
            Some(Test {
                it_message: value,
                stmts: &block.stmts,
            })
        } else {
            None
        }
    }
}

trait ToInternalSpan {
    fn to_internal_span(&self, source_map: &SourceMap, source_file: &SourceFile) -> Span;
}

impl ToInternalSpan for SwcSpan {
    fn to_internal_span(&self, source_map: &SourceMap, source_file: &SourceFile) -> Span {
        Span {
            source_file: source_file.clone(),
            start: self.lo.to_line_column(source_map),
            end: self.hi.to_line_column(source_map),
        }
    }
}

trait ToLineColumn {
    fn to_line_column(&self, source_map: &SourceMap) -> LineColumn;
}

impl ToLineColumn for BytePos {
    fn to_line_column(&self, source_map: &SourceMap) -> LineColumn {
        let Loc { line, col, .. } = source_map.lookup_char_pos(*self);
        LineColumn {
            line,
            column: col.to_usize(),
        }
    }
}
