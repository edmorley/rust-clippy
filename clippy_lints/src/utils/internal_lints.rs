use crate::utils::internal_lints::metadata_collector::is_deprecated_lint;
use clippy_utils::consts::{constant_simple, Constant};
use clippy_utils::diagnostics::{span_lint, span_lint_and_help, span_lint_and_sugg, span_lint_and_then};
use clippy_utils::macros::root_macro_call_first_node;
use clippy_utils::source::{snippet, snippet_with_applicability};
use clippy_utils::ty::match_type;
use clippy_utils::{
    def_path_res, higher, is_else_clause, is_expn_of, is_expr_path_def_path, is_lint_allowed, match_any_def_paths,
    match_def_path, method_calls, paths, peel_blocks_with_stmt, peel_hir_expr_refs, SpanlessEq,
};
use if_chain::if_chain;
use rustc_ast as ast;
use rustc_ast::ast::{Crate, ItemKind, LitKind, ModKind, NodeId};
use rustc_ast::visit::FnKind;
use rustc_data_structures::fx::{FxHashMap, FxHashSet};
use rustc_errors::Applicability;
use rustc_hir as hir;
use rustc_hir::def::{DefKind, Res};
use rustc_hir::def_id::DefId;
use rustc_hir::hir_id::CRATE_HIR_ID;
use rustc_hir::intravisit::Visitor;
use rustc_hir::{
    BinOpKind, Block, Closure, Expr, ExprKind, HirId, Item, Local, MutTy, Mutability, Node, Path, Stmt, StmtKind,
    TyKind, UnOp,
};
use rustc_hir_analysis::hir_ty_to_ty;
use rustc_lint::{EarlyContext, EarlyLintPass, LateContext, LateLintPass, LintContext};
use rustc_middle::hir::nested_filter;
use rustc_middle::mir::interpret::{Allocation, ConstValue, GlobalAlloc};
use rustc_middle::ty::{
    self, fast_reject::SimplifiedTypeGen, subst::GenericArgKind, AssocKind, DefIdTree, FloatTy, Ty,
};
use rustc_semver::RustcVersion;
use rustc_session::{declare_lint_pass, declare_tool_lint, impl_lint_pass};
use rustc_span::source_map::Spanned;
use rustc_span::symbol::{Ident, Symbol};
use rustc_span::{sym, BytePos, Span};

use std::borrow::{Borrow, Cow};
use std::str;

#[cfg(feature = "internal")]
pub mod metadata_collector;

declare_clippy_lint! {
    /// ### What it does
    /// Checks for various things we like to keep tidy in clippy.
    ///
    /// ### Why is this bad?
    /// We like to pretend we're an example of tidy code.
    ///
    /// ### Example
    /// Wrong ordering of the util::paths constants.
    pub CLIPPY_LINTS_INTERNAL,
    internal,
    "various things that will negatively affect your clippy experience"
}

declare_clippy_lint! {
    /// ### What it does
    /// Ensures every lint is associated to a `LintPass`.
    ///
    /// ### Why is this bad?
    /// The compiler only knows lints via a `LintPass`. Without
    /// putting a lint to a `LintPass::get_lints()`'s return, the compiler will not
    /// know the name of the lint.
    ///
    /// ### Known problems
    /// Only checks for lints associated using the
    /// `declare_lint_pass!`, `impl_lint_pass!`, and `lint_array!` macros.
    ///
    /// ### Example
    /// ```rust,ignore
    /// declare_lint! { pub LINT_1, ... }
    /// declare_lint! { pub LINT_2, ... }
    /// declare_lint! { pub FORGOTTEN_LINT, ... }
    /// // ...
    /// declare_lint_pass!(Pass => [LINT_1, LINT_2]);
    /// // missing FORGOTTEN_LINT
    /// ```
    pub LINT_WITHOUT_LINT_PASS,
    internal,
    "declaring a lint without associating it in a LintPass"
}

declare_clippy_lint! {
    /// ### What it does
    /// Checks for calls to `cx.span_lint*` and suggests to use the `utils::*`
    /// variant of the function.
    ///
    /// ### Why is this bad?
    /// The `utils::*` variants also add a link to the Clippy documentation to the
    /// warning/error messages.
    ///
    /// ### Example
    /// ```rust,ignore
    /// cx.span_lint(LINT_NAME, "message");
    /// ```
    ///
    /// Use instead:
    /// ```rust,ignore
    /// utils::span_lint(cx, LINT_NAME, "message");
    /// ```
    pub COMPILER_LINT_FUNCTIONS,
    internal,
    "usage of the lint functions of the compiler instead of the utils::* variant"
}

declare_clippy_lint! {
    /// ### What it does
    /// Checks for calls to `cx.outer().expn_data()` and suggests to use
    /// the `cx.outer_expn_data()`
    ///
    /// ### Why is this bad?
    /// `cx.outer_expn_data()` is faster and more concise.
    ///
    /// ### Example
    /// ```rust,ignore
    /// expr.span.ctxt().outer().expn_data()
    /// ```
    ///
    /// Use instead:
    /// ```rust,ignore
    /// expr.span.ctxt().outer_expn_data()
    /// ```
    pub OUTER_EXPN_EXPN_DATA,
    internal,
    "using `cx.outer_expn().expn_data()` instead of `cx.outer_expn_data()`"
}

declare_clippy_lint! {
    /// ### What it does
    /// Not an actual lint. This lint is only meant for testing our customized internal compiler
    /// error message by calling `panic`.
    ///
    /// ### Why is this bad?
    /// ICE in large quantities can damage your teeth
    ///
    /// ### Example
    /// ```rust,ignore
    /// 🍦🍦🍦🍦🍦
    /// ```
    pub PRODUCE_ICE,
    internal,
    "this message should not appear anywhere as we ICE before and don't emit the lint"
}

declare_clippy_lint! {
    /// ### What it does
    /// Checks for cases of an auto-generated lint without an updated description,
    /// i.e. `default lint description`.
    ///
    /// ### Why is this bad?
    /// Indicates that the lint is not finished.
    ///
    /// ### Example
    /// ```rust,ignore
    /// declare_lint! { pub COOL_LINT, nursery, "default lint description" }
    /// ```
    ///
    /// Use instead:
    /// ```rust,ignore
    /// declare_lint! { pub COOL_LINT, nursery, "a great new lint" }
    /// ```
    pub DEFAULT_LINT,
    internal,
    "found 'default lint description' in a lint declaration"
}

declare_clippy_lint! {
    /// ### What it does
    /// Lints `span_lint_and_then` function calls, where the
    /// closure argument has only one statement and that statement is a method
    /// call to `span_suggestion`, `span_help`, `span_note` (using the same
    /// span), `help` or `note`.
    ///
    /// These usages of `span_lint_and_then` should be replaced with one of the
    /// wrapper functions `span_lint_and_sugg`, span_lint_and_help`, or
    /// `span_lint_and_note`.
    ///
    /// ### Why is this bad?
    /// Using the wrapper `span_lint_and_*` functions, is more
    /// convenient, readable and less error prone.
    ///
    /// ### Example
    /// ```rust,ignore
    /// span_lint_and_then(cx, TEST_LINT, expr.span, lint_msg, |diag| {
    ///     diag.span_suggestion(
    ///         expr.span,
    ///         help_msg,
    ///         sugg.to_string(),
    ///         Applicability::MachineApplicable,
    ///     );
    /// });
    /// span_lint_and_then(cx, TEST_LINT, expr.span, lint_msg, |diag| {
    ///     diag.span_help(expr.span, help_msg);
    /// });
    /// span_lint_and_then(cx, TEST_LINT, expr.span, lint_msg, |diag| {
    ///     diag.help(help_msg);
    /// });
    /// span_lint_and_then(cx, TEST_LINT, expr.span, lint_msg, |diag| {
    ///     diag.span_note(expr.span, note_msg);
    /// });
    /// span_lint_and_then(cx, TEST_LINT, expr.span, lint_msg, |diag| {
    ///     diag.note(note_msg);
    /// });
    /// ```
    ///
    /// Use instead:
    /// ```rust,ignore
    /// span_lint_and_sugg(
    ///     cx,
    ///     TEST_LINT,
    ///     expr.span,
    ///     lint_msg,
    ///     help_msg,
    ///     sugg.to_string(),
    ///     Applicability::MachineApplicable,
    /// );
    /// span_lint_and_help(cx, TEST_LINT, expr.span, lint_msg, Some(expr.span), help_msg);
    /// span_lint_and_help(cx, TEST_LINT, expr.span, lint_msg, None, help_msg);
    /// span_lint_and_note(cx, TEST_LINT, expr.span, lint_msg, Some(expr.span), note_msg);
    /// span_lint_and_note(cx, TEST_LINT, expr.span, lint_msg, None, note_msg);
    /// ```
    pub COLLAPSIBLE_SPAN_LINT_CALLS,
    internal,
    "found collapsible `span_lint_and_then` calls"
}

declare_clippy_lint! {
    /// ### What it does
    /// Checks for usages of def paths when a diagnostic item or a `LangItem` could be used.
    ///
    /// ### Why is this bad?
    /// The path for an item is subject to change and is less efficient to look up than a
    /// diagnostic item or a `LangItem`.
    ///
    /// ### Example
    /// ```rust,ignore
    /// utils::match_type(cx, ty, &paths::VEC)
    /// ```
    ///
    /// Use instead:
    /// ```rust,ignore
    /// utils::is_type_diagnostic_item(cx, ty, sym::Vec)
    /// ```
    pub UNNECESSARY_DEF_PATH,
    internal,
    "using a def path when a diagnostic item or a `LangItem` is available"
}

declare_clippy_lint! {
    /// ### What it does
    /// Checks the paths module for invalid paths.
    ///
    /// ### Why is this bad?
    /// It indicates a bug in the code.
    ///
    /// ### Example
    /// None.
    pub INVALID_PATHS,
    internal,
    "invalid path"
}

declare_clippy_lint! {
    /// ### What it does
    /// Checks for interning symbols that have already been pre-interned and defined as constants.
    ///
    /// ### Why is this bad?
    /// It's faster and easier to use the symbol constant.
    ///
    /// ### Example
    /// ```rust,ignore
    /// let _ = sym!(f32);
    /// ```
    ///
    /// Use instead:
    /// ```rust,ignore
    /// let _ = sym::f32;
    /// ```
    pub INTERNING_DEFINED_SYMBOL,
    internal,
    "interning a symbol that is pre-interned and defined as a constant"
}

declare_clippy_lint! {
    /// ### What it does
    /// Checks for unnecessary conversion from Symbol to a string.
    ///
    /// ### Why is this bad?
    /// It's faster use symbols directly instead of strings.
    ///
    /// ### Example
    /// ```rust,ignore
    /// symbol.as_str() == "clippy";
    /// ```
    ///
    /// Use instead:
    /// ```rust,ignore
    /// symbol == sym::clippy;
    /// ```
    pub UNNECESSARY_SYMBOL_STR,
    internal,
    "unnecessary conversion between Symbol and string"
}

declare_clippy_lint! {
    /// Finds unidiomatic usage of `if_chain!`
    pub IF_CHAIN_STYLE,
    internal,
    "non-idiomatic `if_chain!` usage"
}

declare_clippy_lint! {
    /// ### What it does
    /// Checks for invalid `clippy::version` attributes.
    ///
    /// Valid values are:
    /// * "pre 1.29.0"
    /// * any valid semantic version
    pub INVALID_CLIPPY_VERSION_ATTRIBUTE,
    internal,
    "found an invalid `clippy::version` attribute"
}

declare_clippy_lint! {
    /// ### What it does
    /// Checks for declared clippy lints without the `clippy::version` attribute.
    ///
    pub MISSING_CLIPPY_VERSION_ATTRIBUTE,
    internal,
    "found clippy lint without `clippy::version` attribute"
}

declare_clippy_lint! {
    /// ### What it does
    /// Check that the `extract_msrv_attr!` macro is used, when a lint has a MSRV.
    ///
    pub MISSING_MSRV_ATTR_IMPL,
    internal,
    "checking if all necessary steps were taken when adding a MSRV to a lint"
}

declare_clippy_lint! {
    /// ### What it does
    /// Checks for cases of an auto-generated deprecated lint without an updated reason,
    /// i.e. `"default deprecation note"`.
    ///
    /// ### Why is this bad?
    /// Indicates that the documentation is incomplete.
    ///
    /// ### Example
    /// ```rust,ignore
    /// declare_deprecated_lint! {
    ///     /// ### What it does
    ///     /// Nothing. This lint has been deprecated.
    ///     ///
    ///     /// ### Deprecation reason
    ///     /// TODO
    ///     #[clippy::version = "1.63.0"]
    ///     pub COOL_LINT,
    ///     "default deprecation note"
    /// }
    /// ```
    ///
    /// Use instead:
    /// ```rust,ignore
    /// declare_deprecated_lint! {
    ///     /// ### What it does
    ///     /// Nothing. This lint has been deprecated.
    ///     ///
    ///     /// ### Deprecation reason
    ///     /// This lint has been replaced by `cooler_lint`
    ///     #[clippy::version = "1.63.0"]
    ///     pub COOL_LINT,
    ///     "this lint has been replaced by `cooler_lint`"
    /// }
    /// ```
    pub DEFAULT_DEPRECATION_REASON,
    internal,
    "found 'default deprecation note' in a deprecated lint declaration"
}

declare_lint_pass!(ClippyLintsInternal => [CLIPPY_LINTS_INTERNAL]);

impl EarlyLintPass for ClippyLintsInternal {
    fn check_crate(&mut self, cx: &EarlyContext<'_>, krate: &Crate) {
        if let Some(utils) = krate.items.iter().find(|item| item.ident.name.as_str() == "utils") {
            if let ItemKind::Mod(_, ModKind::Loaded(ref items, ..)) = utils.kind {
                if let Some(paths) = items.iter().find(|item| item.ident.name.as_str() == "paths") {
                    if let ItemKind::Mod(_, ModKind::Loaded(ref items, ..)) = paths.kind {
                        let mut last_name: Option<&str> = None;
                        for item in items {
                            let name = item.ident.as_str();
                            if let Some(last_name) = last_name {
                                if *last_name > *name {
                                    span_lint(
                                        cx,
                                        CLIPPY_LINTS_INTERNAL,
                                        item.span,
                                        "this constant should be before the previous constant due to lexical \
                                         ordering",
                                    );
                                }
                            }
                            last_name = Some(name);
                        }
                    }
                }
            }
        }
    }
}

#[derive(Clone, Debug, Default)]
pub struct LintWithoutLintPass {
    declared_lints: FxHashMap<Symbol, Span>,
    registered_lints: FxHashSet<Symbol>,
}

impl_lint_pass!(LintWithoutLintPass => [DEFAULT_LINT, LINT_WITHOUT_LINT_PASS, INVALID_CLIPPY_VERSION_ATTRIBUTE, MISSING_CLIPPY_VERSION_ATTRIBUTE, DEFAULT_DEPRECATION_REASON]);

impl<'tcx> LateLintPass<'tcx> for LintWithoutLintPass {
    fn check_item(&mut self, cx: &LateContext<'tcx>, item: &'tcx Item<'_>) {
        if is_lint_allowed(cx, DEFAULT_LINT, item.hir_id())
            || is_lint_allowed(cx, DEFAULT_DEPRECATION_REASON, item.hir_id())
        {
            return;
        }

        if let hir::ItemKind::Static(ty, Mutability::Not, body_id) = item.kind {
            let is_lint_ref_ty = is_lint_ref_type(cx, ty);
            if is_deprecated_lint(cx, ty) || is_lint_ref_ty {
                check_invalid_clippy_version_attribute(cx, item);

                let expr = &cx.tcx.hir().body(body_id).value;
                let fields;
                if is_lint_ref_ty {
                    if let ExprKind::AddrOf(_, _, inner_exp) = expr.kind
                        && let ExprKind::Struct(_, struct_fields, _) = inner_exp.kind {
                            fields = struct_fields;
                    } else {
                        return;
                    }
                } else if let ExprKind::Struct(_, struct_fields, _) = expr.kind {
                    fields = struct_fields;
                } else {
                    return;
                }

                let field = fields
                    .iter()
                    .find(|f| f.ident.as_str() == "desc")
                    .expect("lints must have a description field");

                if let ExprKind::Lit(Spanned {
                    node: LitKind::Str(ref sym, _),
                    ..
                }) = field.expr.kind
                {
                    let sym_str = sym.as_str();
                    if is_lint_ref_ty {
                        if sym_str == "default lint description" {
                            span_lint(
                                cx,
                                DEFAULT_LINT,
                                item.span,
                                &format!("the lint `{}` has the default lint description", item.ident.name),
                            );
                        }

                        self.declared_lints.insert(item.ident.name, item.span);
                    } else if sym_str == "default deprecation note" {
                        span_lint(
                            cx,
                            DEFAULT_DEPRECATION_REASON,
                            item.span,
                            &format!("the lint `{}` has the default deprecation reason", item.ident.name),
                        );
                    }
                }
            }
        } else if let Some(macro_call) = root_macro_call_first_node(cx, item) {
            if !matches!(
                cx.tcx.item_name(macro_call.def_id).as_str(),
                "impl_lint_pass" | "declare_lint_pass"
            ) {
                return;
            }
            if let hir::ItemKind::Impl(hir::Impl {
                of_trait: None,
                items: impl_item_refs,
                ..
            }) = item.kind
            {
                let mut collector = LintCollector {
                    output: &mut self.registered_lints,
                    cx,
                };
                let body_id = cx.tcx.hir().body_owned_by(
                    cx.tcx.hir().local_def_id(
                        impl_item_refs
                            .iter()
                            .find(|iiref| iiref.ident.as_str() == "get_lints")
                            .expect("LintPass needs to implement get_lints")
                            .id
                            .hir_id(),
                    ),
                );
                collector.visit_expr(cx.tcx.hir().body(body_id).value);
            }
        }
    }

    fn check_crate_post(&mut self, cx: &LateContext<'tcx>) {
        if is_lint_allowed(cx, LINT_WITHOUT_LINT_PASS, CRATE_HIR_ID) {
            return;
        }

        for (lint_name, &lint_span) in &self.declared_lints {
            // When using the `declare_tool_lint!` macro, the original `lint_span`'s
            // file points to "<rustc macros>".
            // `compiletest-rs` thinks that's an error in a different file and
            // just ignores it. This causes the test in compile-fail/lint_pass
            // not able to capture the error.
            // Therefore, we need to climb the macro expansion tree and find the
            // actual span that invoked `declare_tool_lint!`:
            let lint_span = lint_span.ctxt().outer_expn_data().call_site;

            if !self.registered_lints.contains(lint_name) {
                span_lint(
                    cx,
                    LINT_WITHOUT_LINT_PASS,
                    lint_span,
                    &format!("the lint `{lint_name}` is not added to any `LintPass`"),
                );
            }
        }
    }
}

fn is_lint_ref_type<'tcx>(cx: &LateContext<'tcx>, ty: &hir::Ty<'_>) -> bool {
    if let TyKind::Rptr(
        _,
        MutTy {
            ty: inner,
            mutbl: Mutability::Not,
        },
    ) = ty.kind
    {
        if let TyKind::Path(ref path) = inner.kind {
            if let Res::Def(DefKind::Struct, def_id) = cx.qpath_res(path, inner.hir_id) {
                return match_def_path(cx, def_id, &paths::LINT);
            }
        }
    }

    false
}

fn check_invalid_clippy_version_attribute(cx: &LateContext<'_>, item: &'_ Item<'_>) {
    if let Some(value) = extract_clippy_version_value(cx, item) {
        // The `sym!` macro doesn't work as it only expects a single token.
        // It's better to keep it this way and have a direct `Symbol::intern` call here.
        if value == Symbol::intern("pre 1.29.0") {
            return;
        }

        if RustcVersion::parse(value.as_str()).is_err() {
            span_lint_and_help(
                cx,
                INVALID_CLIPPY_VERSION_ATTRIBUTE,
                item.span,
                "this item has an invalid `clippy::version` attribute",
                None,
                "please use a valid semantic version, see `doc/adding_lints.md`",
            );
        }
    } else {
        span_lint_and_help(
            cx,
            MISSING_CLIPPY_VERSION_ATTRIBUTE,
            item.span,
            "this lint is missing the `clippy::version` attribute or version value",
            None,
            "please use a `clippy::version` attribute, see `doc/adding_lints.md`",
        );
    }
}

/// This function extracts the version value of a `clippy::version` attribute if the given value has
/// one
fn extract_clippy_version_value(cx: &LateContext<'_>, item: &'_ Item<'_>) -> Option<Symbol> {
    let attrs = cx.tcx.hir().attrs(item.hir_id());
    attrs.iter().find_map(|attr| {
        if_chain! {
            // Identify attribute
            if let ast::AttrKind::Normal(ref attr_kind) = &attr.kind;
            if let [tool_name, attr_name] = &attr_kind.item.path.segments[..];
            if tool_name.ident.name == sym::clippy;
            if attr_name.ident.name == sym::version;
            if let Some(version) = attr.value_str();
            then {
                Some(version)
            } else {
                None
            }
        }
    })
}

struct LintCollector<'a, 'tcx> {
    output: &'a mut FxHashSet<Symbol>,
    cx: &'a LateContext<'tcx>,
}

impl<'a, 'tcx> Visitor<'tcx> for LintCollector<'a, 'tcx> {
    type NestedFilter = nested_filter::All;

    fn visit_path(&mut self, path: &'tcx Path<'_>, _: HirId) {
        if path.segments.len() == 1 {
            self.output.insert(path.segments[0].ident.name);
        }
    }

    fn nested_visit_map(&mut self) -> Self::Map {
        self.cx.tcx.hir()
    }
}

#[derive(Clone, Default)]
pub struct CompilerLintFunctions {
    map: FxHashMap<&'static str, &'static str>,
}

impl CompilerLintFunctions {
    #[must_use]
    pub fn new() -> Self {
        let mut map = FxHashMap::default();
        map.insert("span_lint", "utils::span_lint");
        map.insert("struct_span_lint", "utils::span_lint");
        map.insert("lint", "utils::span_lint");
        map.insert("span_lint_note", "utils::span_lint_and_note");
        map.insert("span_lint_help", "utils::span_lint_and_help");
        Self { map }
    }
}

impl_lint_pass!(CompilerLintFunctions => [COMPILER_LINT_FUNCTIONS]);

impl<'tcx> LateLintPass<'tcx> for CompilerLintFunctions {
    fn check_expr(&mut self, cx: &LateContext<'tcx>, expr: &'tcx Expr<'_>) {
        if is_lint_allowed(cx, COMPILER_LINT_FUNCTIONS, expr.hir_id) {
            return;
        }

        if_chain! {
            if let ExprKind::MethodCall(path, self_arg, _, _) = &expr.kind;
            let fn_name = path.ident;
            if let Some(sugg) = self.map.get(fn_name.as_str());
            let ty = cx.typeck_results().expr_ty(self_arg).peel_refs();
            if match_type(cx, ty, &paths::EARLY_CONTEXT)
                || match_type(cx, ty, &paths::LATE_CONTEXT);
            then {
                span_lint_and_help(
                    cx,
                    COMPILER_LINT_FUNCTIONS,
                    path.ident.span,
                    "usage of a compiler lint function",
                    None,
                    &format!("please use the Clippy variant of this function: `{sugg}`"),
                );
            }
        }
    }
}

declare_lint_pass!(OuterExpnDataPass => [OUTER_EXPN_EXPN_DATA]);

impl<'tcx> LateLintPass<'tcx> for OuterExpnDataPass {
    fn check_expr(&mut self, cx: &LateContext<'tcx>, expr: &'tcx hir::Expr<'_>) {
        if is_lint_allowed(cx, OUTER_EXPN_EXPN_DATA, expr.hir_id) {
            return;
        }

        let (method_names, arg_lists, spans) = method_calls(expr, 2);
        let method_names: Vec<&str> = method_names.iter().map(Symbol::as_str).collect();
        if_chain! {
            if let ["expn_data", "outer_expn"] = method_names.as_slice();
            let (self_arg, args)= arg_lists[1];
            if args.is_empty();
            let self_ty = cx.typeck_results().expr_ty(self_arg).peel_refs();
            if match_type(cx, self_ty, &paths::SYNTAX_CONTEXT);
            then {
                span_lint_and_sugg(
                    cx,
                    OUTER_EXPN_EXPN_DATA,
                    spans[1].with_hi(expr.span.hi()),
                    "usage of `outer_expn().expn_data()`",
                    "try",
                    "outer_expn_data()".to_string(),
                    Applicability::MachineApplicable,
                );
            }
        }
    }
}

declare_lint_pass!(ProduceIce => [PRODUCE_ICE]);

impl EarlyLintPass for ProduceIce {
    fn check_fn(&mut self, _: &EarlyContext<'_>, fn_kind: FnKind<'_>, _: Span, _: NodeId) {
        assert!(!is_trigger_fn(fn_kind), "Would you like some help with that?");
    }
}

fn is_trigger_fn(fn_kind: FnKind<'_>) -> bool {
    match fn_kind {
        FnKind::Fn(_, ident, ..) => ident.name.as_str() == "it_looks_like_you_are_trying_to_kill_clippy",
        FnKind::Closure(..) => false,
    }
}

declare_lint_pass!(CollapsibleCalls => [COLLAPSIBLE_SPAN_LINT_CALLS]);

impl<'tcx> LateLintPass<'tcx> for CollapsibleCalls {
    fn check_expr(&mut self, cx: &LateContext<'tcx>, expr: &'tcx hir::Expr<'_>) {
        if is_lint_allowed(cx, COLLAPSIBLE_SPAN_LINT_CALLS, expr.hir_id) {
            return;
        }

        if_chain! {
            if let ExprKind::Call(func, and_then_args) = expr.kind;
            if is_expr_path_def_path(cx, func, &["clippy_utils", "diagnostics", "span_lint_and_then"]);
            if and_then_args.len() == 5;
            if let ExprKind::Closure(&Closure { body, .. }) = &and_then_args[4].kind;
            let body = cx.tcx.hir().body(body);
            let only_expr = peel_blocks_with_stmt(body.value);
            if let ExprKind::MethodCall(ps, recv, span_call_args, _) = &only_expr.kind;
            if let ExprKind::Path(..) = recv.kind;
            then {
                let and_then_snippets = get_and_then_snippets(cx, and_then_args);
                let mut sle = SpanlessEq::new(cx).deny_side_effects();
                match ps.ident.as_str() {
                    "span_suggestion" if sle.eq_expr(&and_then_args[2], &span_call_args[0]) => {
                        suggest_suggestion(cx, expr, &and_then_snippets, &span_suggestion_snippets(cx, span_call_args));
                    },
                    "span_help" if sle.eq_expr(&and_then_args[2], &span_call_args[0]) => {
                        let help_snippet = snippet(cx, span_call_args[1].span, r#""...""#);
                        suggest_help(cx, expr, &and_then_snippets, help_snippet.borrow(), true);
                    },
                    "span_note" if sle.eq_expr(&and_then_args[2], &span_call_args[0]) => {
                        let note_snippet = snippet(cx, span_call_args[1].span, r#""...""#);
                        suggest_note(cx, expr, &and_then_snippets, note_snippet.borrow(), true);
                    },
                    "help" => {
                        let help_snippet = snippet(cx, span_call_args[0].span, r#""...""#);
                        suggest_help(cx, expr, &and_then_snippets, help_snippet.borrow(), false);
                    }
                    "note" => {
                        let note_snippet = snippet(cx, span_call_args[0].span, r#""...""#);
                        suggest_note(cx, expr, &and_then_snippets, note_snippet.borrow(), false);
                    }
                    _  => (),
                }
            }
        }
    }
}

struct AndThenSnippets<'a> {
    cx: Cow<'a, str>,
    lint: Cow<'a, str>,
    span: Cow<'a, str>,
    msg: Cow<'a, str>,
}

fn get_and_then_snippets<'a, 'hir>(cx: &LateContext<'_>, and_then_snippets: &'hir [Expr<'hir>]) -> AndThenSnippets<'a> {
    let cx_snippet = snippet(cx, and_then_snippets[0].span, "cx");
    let lint_snippet = snippet(cx, and_then_snippets[1].span, "..");
    let span_snippet = snippet(cx, and_then_snippets[2].span, "span");
    let msg_snippet = snippet(cx, and_then_snippets[3].span, r#""...""#);

    AndThenSnippets {
        cx: cx_snippet,
        lint: lint_snippet,
        span: span_snippet,
        msg: msg_snippet,
    }
}

struct SpanSuggestionSnippets<'a> {
    help: Cow<'a, str>,
    sugg: Cow<'a, str>,
    applicability: Cow<'a, str>,
}

fn span_suggestion_snippets<'a, 'hir>(
    cx: &LateContext<'_>,
    span_call_args: &'hir [Expr<'hir>],
) -> SpanSuggestionSnippets<'a> {
    let help_snippet = snippet(cx, span_call_args[1].span, r#""...""#);
    let sugg_snippet = snippet(cx, span_call_args[2].span, "..");
    let applicability_snippet = snippet(cx, span_call_args[3].span, "Applicability::MachineApplicable");

    SpanSuggestionSnippets {
        help: help_snippet,
        sugg: sugg_snippet,
        applicability: applicability_snippet,
    }
}

fn suggest_suggestion(
    cx: &LateContext<'_>,
    expr: &Expr<'_>,
    and_then_snippets: &AndThenSnippets<'_>,
    span_suggestion_snippets: &SpanSuggestionSnippets<'_>,
) {
    span_lint_and_sugg(
        cx,
        COLLAPSIBLE_SPAN_LINT_CALLS,
        expr.span,
        "this call is collapsible",
        "collapse into",
        format!(
            "span_lint_and_sugg({}, {}, {}, {}, {}, {}, {})",
            and_then_snippets.cx,
            and_then_snippets.lint,
            and_then_snippets.span,
            and_then_snippets.msg,
            span_suggestion_snippets.help,
            span_suggestion_snippets.sugg,
            span_suggestion_snippets.applicability
        ),
        Applicability::MachineApplicable,
    );
}

fn suggest_help(
    cx: &LateContext<'_>,
    expr: &Expr<'_>,
    and_then_snippets: &AndThenSnippets<'_>,
    help: &str,
    with_span: bool,
) {
    let option_span = if with_span {
        format!("Some({})", and_then_snippets.span)
    } else {
        "None".to_string()
    };

    span_lint_and_sugg(
        cx,
        COLLAPSIBLE_SPAN_LINT_CALLS,
        expr.span,
        "this call is collapsible",
        "collapse into",
        format!(
            "span_lint_and_help({}, {}, {}, {}, {}, {help})",
            and_then_snippets.cx, and_then_snippets.lint, and_then_snippets.span, and_then_snippets.msg, &option_span,
        ),
        Applicability::MachineApplicable,
    );
}

fn suggest_note(
    cx: &LateContext<'_>,
    expr: &Expr<'_>,
    and_then_snippets: &AndThenSnippets<'_>,
    note: &str,
    with_span: bool,
) {
    let note_span = if with_span {
        format!("Some({})", and_then_snippets.span)
    } else {
        "None".to_string()
    };

    span_lint_and_sugg(
        cx,
        COLLAPSIBLE_SPAN_LINT_CALLS,
        expr.span,
        "this call is collapsible",
        "collapse into",
        format!(
            "span_lint_and_note({}, {}, {}, {}, {note_span}, {note})",
            and_then_snippets.cx, and_then_snippets.lint, and_then_snippets.span, and_then_snippets.msg,
        ),
        Applicability::MachineApplicable,
    );
}

declare_lint_pass!(UnnecessaryDefPath => [UNNECESSARY_DEF_PATH]);

#[allow(clippy::too_many_lines)]
impl<'tcx> LateLintPass<'tcx> for UnnecessaryDefPath {
    fn check_expr(&mut self, cx: &LateContext<'tcx>, expr: &'tcx hir::Expr<'_>) {
        enum Item {
            LangItem(Symbol),
            DiagnosticItem(Symbol),
        }
        static PATHS: &[&[&str]] = &[
            &["clippy_utils", "match_def_path"],
            &["clippy_utils", "match_trait_method"],
            &["clippy_utils", "ty", "match_type"],
            &["clippy_utils", "is_expr_path_def_path"],
        ];

        if is_lint_allowed(cx, UNNECESSARY_DEF_PATH, expr.hir_id) {
            return;
        }

        if_chain! {
            if let ExprKind::Call(func, [cx_arg, def_arg, args@..]) = expr.kind;
            if let ExprKind::Path(path) = &func.kind;
            if let Some(id) = cx.qpath_res(path, func.hir_id).opt_def_id();
            if let Some(which_path) = match_any_def_paths(cx, id, PATHS);
            let item_arg = if which_path == 4 { &args[1] } else { &args[0] };
            // Extract the path to the matched type
            if let Some(segments) = path_to_matched_type(cx, item_arg);
            let segments: Vec<&str> = segments.iter().map(|sym| &**sym).collect();
            if let Some(def_id) = def_path_res(cx, &segments[..]).opt_def_id();
            then {
                // def_path_res will match field names before anything else, but for this we want to match
                // inherent functions first.
                let def_id = if cx.tcx.def_kind(def_id) == DefKind::Field {
                    let method_name = *segments.last().unwrap();
                    cx.tcx.def_key(def_id).parent
                        .and_then(|parent_idx|
                            cx.tcx.inherent_impls(DefId { index: parent_idx, krate: def_id.krate }).iter()
                                .find_map(|impl_id| cx.tcx.associated_items(*impl_id)
                                    .find_by_name_and_kind(
                                        cx.tcx,
                                        Ident::from_str(method_name),
                                        AssocKind::Fn,
                                        *impl_id,
                                    )
                                )
                        )
                        .map_or(def_id, |item| item.def_id)
                } else {
                    def_id
                };

                // Check if the target item is a diagnostic item or LangItem.
                let (msg, item) = if let Some(item_name)
                    = cx.tcx.diagnostic_items(def_id.krate).id_to_name.get(&def_id)
                {
                    (
                        "use of a def path to a diagnostic item",
                        Item::DiagnosticItem(*item_name),
                    )
                } else if let Some(lang_item) = cx.tcx.lang_items().items().iter().position(|id| *id == Some(def_id)) {
                    let lang_items = def_path_res(cx, &["rustc_hir", "lang_items", "LangItem"]).def_id();
                    let item_name = cx.tcx.adt_def(lang_items).variants().iter().nth(lang_item).unwrap().name;
                    (
                        "use of a def path to a `LangItem`",
                        Item::LangItem(item_name),
                    )
                } else {
                    return;
                };

                let has_ctor = match cx.tcx.def_kind(def_id) {
                    DefKind::Struct => {
                        let variant = cx.tcx.adt_def(def_id).non_enum_variant();
                        variant.ctor_def_id.is_some() && variant.fields.iter().all(|f| f.vis.is_public())
                    }
                    DefKind::Variant => {
                        let variant = cx.tcx.adt_def(cx.tcx.parent(def_id)).variant_with_id(def_id);
                        variant.ctor_def_id.is_some() && variant.fields.iter().all(|f| f.vis.is_public())
                    }
                    _ => false,
                };

                let mut app = Applicability::MachineApplicable;
                let cx_snip = snippet_with_applicability(cx, cx_arg.span, "..", &mut app);
                let def_snip = snippet_with_applicability(cx, def_arg.span, "..", &mut app);
                let (sugg, with_note) = match (which_path, item) {
                    // match_def_path
                    (0, Item::DiagnosticItem(item)) =>
                        (format!("{cx_snip}.tcx.is_diagnostic_item(sym::{item}, {def_snip})"), has_ctor),
                    (0, Item::LangItem(item)) => (
                        format!("{cx_snip}.tcx.lang_items().require(LangItem::{item}).ok() == Some({def_snip})"),
                        has_ctor
                    ),
                    // match_trait_method
                    (1, Item::DiagnosticItem(item)) =>
                        (format!("is_trait_method({cx_snip}, {def_snip}, sym::{item})"), false),
                    // match_type
                    (2, Item::DiagnosticItem(item)) =>
                        (format!("is_type_diagnostic_item({cx_snip}, {def_snip}, sym::{item})"), false),
                    (2, Item::LangItem(item)) =>
                        (format!("is_type_lang_item({cx_snip}, {def_snip}, LangItem::{item})"), false),
                    // is_expr_path_def_path
                    (3, Item::DiagnosticItem(item)) if has_ctor => (
                        format!(
                            "is_res_diag_ctor({cx_snip}, path_res({cx_snip}, {def_snip}), sym::{item})",
                        ),
                        false,
                    ),
                    (3, Item::LangItem(item)) if has_ctor => (
                        format!(
                            "is_res_lang_ctor({cx_snip}, path_res({cx_snip}, {def_snip}), LangItem::{item})",
                        ),
                        false,
                    ),
                    (3, Item::DiagnosticItem(item)) =>
                        (format!("is_path_diagnostic_item({cx_snip}, {def_snip}, sym::{item})"), false),
                    (3, Item::LangItem(item)) => (
                        format!(
                            "path_res({cx_snip}, {def_snip}).opt_def_id()\
                                .map_or(false, |id| {cx_snip}.tcx.lang_items().require(LangItem::{item}).ok() == Some(id))",
                        ),
                        false,
                    ),
                    _ => return,
                };

                span_lint_and_then(
                    cx,
                    UNNECESSARY_DEF_PATH,
                    expr.span,
                    msg,
                    |diag| {
                        diag.span_suggestion(expr.span, "try", sugg, app);
                        if with_note {
                            diag.help(
                                "if this `DefId` came from a constructor expression or pattern then the \
                                    parent `DefId` should be used instead"
                            );
                        }
                    },
                );
            }
        }
    }
}

fn path_to_matched_type(cx: &LateContext<'_>, expr: &hir::Expr<'_>) -> Option<Vec<String>> {
    match peel_hir_expr_refs(expr).0.kind {
        ExprKind::Path(ref qpath) => match cx.qpath_res(qpath, expr.hir_id) {
            Res::Local(hir_id) => {
                let parent_id = cx.tcx.hir().get_parent_node(hir_id);
                if let Some(Node::Local(Local { init: Some(init), .. })) = cx.tcx.hir().find(parent_id) {
                    path_to_matched_type(cx, init)
                } else {
                    None
                }
            },
            Res::Def(DefKind::Static(_), def_id) => read_mir_alloc_def_path(
                cx,
                cx.tcx.eval_static_initializer(def_id).ok()?.inner(),
                cx.tcx.type_of(def_id),
            ),
            Res::Def(DefKind::Const, def_id) => match cx.tcx.const_eval_poly(def_id).ok()? {
                ConstValue::ByRef { alloc, offset } if offset.bytes() == 0 => {
                    read_mir_alloc_def_path(cx, alloc.inner(), cx.tcx.type_of(def_id))
                },
                _ => None,
            },
            _ => None,
        },
        ExprKind::Array(exprs) => exprs
            .iter()
            .map(|expr| {
                if let ExprKind::Lit(lit) = &expr.kind {
                    if let LitKind::Str(sym, _) = lit.node {
                        return Some((*sym.as_str()).to_owned());
                    }
                }

                None
            })
            .collect(),
        _ => None,
    }
}

fn read_mir_alloc_def_path<'tcx>(cx: &LateContext<'tcx>, alloc: &'tcx Allocation, ty: Ty<'_>) -> Option<Vec<String>> {
    let (alloc, ty) = if let ty::Ref(_, ty, Mutability::Not) = *ty.kind() {
        let &alloc = alloc.provenance().values().next()?;
        if let GlobalAlloc::Memory(alloc) = cx.tcx.global_alloc(alloc) {
            (alloc.inner(), ty)
        } else {
            return None;
        }
    } else {
        (alloc, ty)
    };

    if let ty::Array(ty, _) | ty::Slice(ty) = *ty.kind()
        && let ty::Ref(_, ty, Mutability::Not) = *ty.kind()
        && ty.is_str()
    {
        alloc
            .provenance()
            .values()
            .map(|&alloc| {
                if let GlobalAlloc::Memory(alloc) = cx.tcx.global_alloc(alloc) {
                    let alloc = alloc.inner();
                    str::from_utf8(alloc.inspect_with_uninit_and_ptr_outside_interpreter(0..alloc.len()))
                        .ok().map(ToOwned::to_owned)
                } else {
                    None
                }
            })
            .collect()
    } else {
        None
    }
}

// This is not a complete resolver for paths. It works on all the paths currently used in the paths
// module.  That's all it does and all it needs to do.
pub fn check_path(cx: &LateContext<'_>, path: &[&str]) -> bool {
    if def_path_res(cx, path) != Res::Err {
        return true;
    }

    // Some implementations can't be found by `path_to_res`, particularly inherent
    // implementations of native types. Check lang items.
    let path_syms: Vec<_> = path.iter().map(|p| Symbol::intern(p)).collect();
    let lang_items = cx.tcx.lang_items();
    // This list isn't complete, but good enough for our current list of paths.
    let incoherent_impls = [
        SimplifiedTypeGen::FloatSimplifiedType(FloatTy::F32),
        SimplifiedTypeGen::FloatSimplifiedType(FloatTy::F64),
        SimplifiedTypeGen::SliceSimplifiedType,
        SimplifiedTypeGen::StrSimplifiedType,
    ]
    .iter()
    .flat_map(|&ty| cx.tcx.incoherent_impls(ty));
    for item_def_id in lang_items.items().iter().flatten().chain(incoherent_impls) {
        let lang_item_path = cx.get_def_path(*item_def_id);
        if path_syms.starts_with(&lang_item_path) {
            if let [item] = &path_syms[lang_item_path.len()..] {
                if matches!(
                    cx.tcx.def_kind(*item_def_id),
                    DefKind::Mod | DefKind::Enum | DefKind::Trait
                ) {
                    for child in cx.tcx.module_children(*item_def_id) {
                        if child.ident.name == *item {
                            return true;
                        }
                    }
                } else {
                    for child in cx.tcx.associated_item_def_ids(*item_def_id) {
                        if cx.tcx.item_name(*child) == *item {
                            return true;
                        }
                    }
                }
            }
        }
    }

    false
}

declare_lint_pass!(InvalidPaths => [INVALID_PATHS]);

impl<'tcx> LateLintPass<'tcx> for InvalidPaths {
    fn check_item(&mut self, cx: &LateContext<'tcx>, item: &'tcx Item<'_>) {
        let local_def_id = &cx.tcx.parent_module(item.hir_id());
        let mod_name = &cx.tcx.item_name(local_def_id.to_def_id());
        if_chain! {
            if mod_name.as_str() == "paths";
            if let hir::ItemKind::Const(ty, body_id) = item.kind;
            let ty = hir_ty_to_ty(cx.tcx, ty);
            if let ty::Array(el_ty, _) = &ty.kind();
            if let ty::Ref(_, el_ty, _) = &el_ty.kind();
            if el_ty.is_str();
            let body = cx.tcx.hir().body(body_id);
            let typeck_results = cx.tcx.typeck_body(body_id);
            if let Some(Constant::Vec(path)) = constant_simple(cx, typeck_results, body.value);
            let path: Vec<&str> = path.iter().map(|x| {
                    if let Constant::Str(s) = x {
                        s.as_str()
                    } else {
                        // We checked the type of the constant above
                        unreachable!()
                    }
                }).collect();
            if !check_path(cx, &path[..]);
            then {
                span_lint(cx, INVALID_PATHS, item.span, "invalid path");
            }
        }
    }
}

#[derive(Default)]
pub struct InterningDefinedSymbol {
    // Maps the symbol value to the constant DefId.
    symbol_map: FxHashMap<u32, DefId>,
}

impl_lint_pass!(InterningDefinedSymbol => [INTERNING_DEFINED_SYMBOL, UNNECESSARY_SYMBOL_STR]);

impl<'tcx> LateLintPass<'tcx> for InterningDefinedSymbol {
    fn check_crate(&mut self, cx: &LateContext<'_>) {
        if !self.symbol_map.is_empty() {
            return;
        }

        for &module in &[&paths::KW_MODULE, &paths::SYM_MODULE] {
            if let Some(def_id) = def_path_res(cx, module).opt_def_id() {
                for item in cx.tcx.module_children(def_id).iter() {
                    if_chain! {
                        if let Res::Def(DefKind::Const, item_def_id) = item.res;
                        let ty = cx.tcx.type_of(item_def_id);
                        if match_type(cx, ty, &paths::SYMBOL);
                        if let Ok(ConstValue::Scalar(value)) = cx.tcx.const_eval_poly(item_def_id);
                        if let Ok(value) = value.to_u32();
                        then {
                            self.symbol_map.insert(value, item_def_id);
                        }
                    }
                }
            }
        }
    }

    fn check_expr(&mut self, cx: &LateContext<'tcx>, expr: &'tcx Expr<'_>) {
        if_chain! {
            if let ExprKind::Call(func, [arg]) = &expr.kind;
            if let ty::FnDef(def_id, _) = cx.typeck_results().expr_ty(func).kind();
            if match_def_path(cx, *def_id, &paths::SYMBOL_INTERN);
            if let Some(Constant::Str(arg)) = constant_simple(cx, cx.typeck_results(), arg);
            let value = Symbol::intern(&arg).as_u32();
            if let Some(&def_id) = self.symbol_map.get(&value);
            then {
                span_lint_and_sugg(
                    cx,
                    INTERNING_DEFINED_SYMBOL,
                    is_expn_of(expr.span, "sym").unwrap_or(expr.span),
                    "interning a defined symbol",
                    "try",
                    cx.tcx.def_path_str(def_id),
                    Applicability::MachineApplicable,
                );
            }
        }
        if let ExprKind::Binary(op, left, right) = expr.kind {
            if matches!(op.node, BinOpKind::Eq | BinOpKind::Ne) {
                let data = [
                    (left, self.symbol_str_expr(left, cx)),
                    (right, self.symbol_str_expr(right, cx)),
                ];
                match data {
                    // both operands are a symbol string
                    [(_, Some(left)), (_, Some(right))] => {
                        span_lint_and_sugg(
                            cx,
                            UNNECESSARY_SYMBOL_STR,
                            expr.span,
                            "unnecessary `Symbol` to string conversion",
                            "try",
                            format!(
                                "{} {} {}",
                                left.as_symbol_snippet(cx),
                                op.node.as_str(),
                                right.as_symbol_snippet(cx),
                            ),
                            Applicability::MachineApplicable,
                        );
                    },
                    // one of the operands is a symbol string
                    [(expr, Some(symbol)), _] | [_, (expr, Some(symbol))] => {
                        // creating an owned string for comparison
                        if matches!(symbol, SymbolStrExpr::Expr { is_to_owned: true, .. }) {
                            span_lint_and_sugg(
                                cx,
                                UNNECESSARY_SYMBOL_STR,
                                expr.span,
                                "unnecessary string allocation",
                                "try",
                                format!("{}.as_str()", symbol.as_symbol_snippet(cx)),
                                Applicability::MachineApplicable,
                            );
                        }
                    },
                    // nothing found
                    [(_, None), (_, None)] => {},
                }
            }
        }
    }
}

impl InterningDefinedSymbol {
    fn symbol_str_expr<'tcx>(&self, expr: &'tcx Expr<'tcx>, cx: &LateContext<'tcx>) -> Option<SymbolStrExpr<'tcx>> {
        static IDENT_STR_PATHS: &[&[&str]] = &[&paths::IDENT_AS_STR, &paths::TO_STRING_METHOD];
        static SYMBOL_STR_PATHS: &[&[&str]] = &[
            &paths::SYMBOL_AS_STR,
            &paths::SYMBOL_TO_IDENT_STRING,
            &paths::TO_STRING_METHOD,
        ];
        let call = if_chain! {
            if let ExprKind::AddrOf(_, _, e) = expr.kind;
            if let ExprKind::Unary(UnOp::Deref, e) = e.kind;
            then { e } else { expr }
        };
        if_chain! {
            // is a method call
            if let ExprKind::MethodCall(_, item, [], _) = call.kind;
            if let Some(did) = cx.typeck_results().type_dependent_def_id(call.hir_id);
            let ty = cx.typeck_results().expr_ty(item);
            // ...on either an Ident or a Symbol
            if let Some(is_ident) = if match_type(cx, ty, &paths::SYMBOL) {
                Some(false)
            } else if match_type(cx, ty, &paths::IDENT) {
                Some(true)
            } else {
                None
            };
            // ...which converts it to a string
            let paths = if is_ident { IDENT_STR_PATHS } else { SYMBOL_STR_PATHS };
            if let Some(path) = paths.iter().find(|path| match_def_path(cx, did, path));
            then {
                let is_to_owned = path.last().unwrap().ends_with("string");
                return Some(SymbolStrExpr::Expr {
                    item,
                    is_ident,
                    is_to_owned,
                });
            }
        }
        // is a string constant
        if let Some(Constant::Str(s)) = constant_simple(cx, cx.typeck_results(), expr) {
            let value = Symbol::intern(&s).as_u32();
            // ...which matches a symbol constant
            if let Some(&def_id) = self.symbol_map.get(&value) {
                return Some(SymbolStrExpr::Const(def_id));
            }
        }
        None
    }
}

enum SymbolStrExpr<'tcx> {
    /// a string constant with a corresponding symbol constant
    Const(DefId),
    /// a "symbol to string" expression like `symbol.as_str()`
    Expr {
        /// part that evaluates to `Symbol` or `Ident`
        item: &'tcx Expr<'tcx>,
        is_ident: bool,
        /// whether an owned `String` is created like `to_ident_string()`
        is_to_owned: bool,
    },
}

impl<'tcx> SymbolStrExpr<'tcx> {
    /// Returns a snippet that evaluates to a `Symbol` and is const if possible
    fn as_symbol_snippet(&self, cx: &LateContext<'_>) -> Cow<'tcx, str> {
        match *self {
            Self::Const(def_id) => cx.tcx.def_path_str(def_id).into(),
            Self::Expr { item, is_ident, .. } => {
                let mut snip = snippet(cx, item.span.source_callsite(), "..");
                if is_ident {
                    // get `Ident.name`
                    snip.to_mut().push_str(".name");
                }
                snip
            },
        }
    }
}

declare_lint_pass!(IfChainStyle => [IF_CHAIN_STYLE]);

impl<'tcx> LateLintPass<'tcx> for IfChainStyle {
    fn check_block(&mut self, cx: &LateContext<'tcx>, block: &'tcx hir::Block<'_>) {
        let (local, after, if_chain_span) = if_chain! {
            if let [Stmt { kind: StmtKind::Local(local), .. }, after @ ..] = block.stmts;
            if let Some(if_chain_span) = is_expn_of(block.span, "if_chain");
            then { (local, after, if_chain_span) } else { return }
        };
        if is_first_if_chain_expr(cx, block.hir_id, if_chain_span) {
            span_lint(
                cx,
                IF_CHAIN_STYLE,
                if_chain_local_span(cx, local, if_chain_span),
                "`let` expression should be above the `if_chain!`",
            );
        } else if local.span.ctxt() == block.span.ctxt() && is_if_chain_then(after, block.expr, if_chain_span) {
            span_lint(
                cx,
                IF_CHAIN_STYLE,
                if_chain_local_span(cx, local, if_chain_span),
                "`let` expression should be inside `then { .. }`",
            );
        }
    }

    fn check_expr(&mut self, cx: &LateContext<'tcx>, expr: &'tcx hir::Expr<'_>) {
        let (cond, then, els) = if let Some(higher::IfOrIfLet { cond, r#else, then }) = higher::IfOrIfLet::hir(expr) {
            (cond, then, r#else.is_some())
        } else {
            return;
        };
        let then_block = match then.kind {
            ExprKind::Block(block, _) => block,
            _ => return,
        };
        let if_chain_span = is_expn_of(expr.span, "if_chain");
        if !els {
            check_nested_if_chains(cx, expr, then_block, if_chain_span);
        }
        let if_chain_span = match if_chain_span {
            None => return,
            Some(span) => span,
        };
        // check for `if a && b;`
        if_chain! {
            if let ExprKind::Binary(op, _, _) = cond.kind;
            if op.node == BinOpKind::And;
            if cx.sess().source_map().is_multiline(cond.span);
            then {
                span_lint(cx, IF_CHAIN_STYLE, cond.span, "`if a && b;` should be `if a; if b;`");
            }
        }
        if is_first_if_chain_expr(cx, expr.hir_id, if_chain_span)
            && is_if_chain_then(then_block.stmts, then_block.expr, if_chain_span)
        {
            span_lint(cx, IF_CHAIN_STYLE, expr.span, "`if_chain!` only has one `if`");
        }
    }
}

fn check_nested_if_chains(
    cx: &LateContext<'_>,
    if_expr: &Expr<'_>,
    then_block: &Block<'_>,
    if_chain_span: Option<Span>,
) {
    #[rustfmt::skip]
    let (head, tail) = match *then_block {
        Block { stmts, expr: Some(tail), .. } => (stmts, tail),
        Block {
            stmts: &[
                ref head @ ..,
                Stmt { kind: StmtKind::Expr(tail) | StmtKind::Semi(tail), .. }
            ],
            ..
        } => (head, tail),
        _ => return,
    };
    if_chain! {
        if let Some(higher::IfOrIfLet { r#else: None, .. }) = higher::IfOrIfLet::hir(tail);
        let sm = cx.sess().source_map();
        if head
            .iter()
            .all(|stmt| matches!(stmt.kind, StmtKind::Local(..)) && !sm.is_multiline(stmt.span));
        if if_chain_span.is_some() || !is_else_clause(cx.tcx, if_expr);
        then {} else { return }
    }
    let (span, msg) = match (if_chain_span, is_expn_of(tail.span, "if_chain")) {
        (None, Some(_)) => (if_expr.span, "this `if` can be part of the inner `if_chain!`"),
        (Some(_), None) => (tail.span, "this `if` can be part of the outer `if_chain!`"),
        (Some(a), Some(b)) if a != b => (b, "this `if_chain!` can be merged with the outer `if_chain!`"),
        _ => return,
    };
    span_lint_and_then(cx, IF_CHAIN_STYLE, span, msg, |diag| {
        let (span, msg) = match head {
            [] => return,
            [stmt] => (stmt.span, "this `let` statement can also be in the `if_chain!`"),
            [a, .., b] => (
                a.span.to(b.span),
                "these `let` statements can also be in the `if_chain!`",
            ),
        };
        diag.span_help(span, msg);
    });
}

fn is_first_if_chain_expr(cx: &LateContext<'_>, hir_id: HirId, if_chain_span: Span) -> bool {
    cx.tcx
        .hir()
        .parent_iter(hir_id)
        .find(|(_, node)| {
            #[rustfmt::skip]
            !matches!(node, Node::Expr(Expr { kind: ExprKind::Block(..), .. }) | Node::Stmt(_))
        })
        .map_or(false, |(id, _)| {
            is_expn_of(cx.tcx.hir().span(id), "if_chain") != Some(if_chain_span)
        })
}

/// Checks a trailing slice of statements and expression of a `Block` to see if they are part
/// of the `then {..}` portion of an `if_chain!`
fn is_if_chain_then(stmts: &[Stmt<'_>], expr: Option<&Expr<'_>>, if_chain_span: Span) -> bool {
    let span = if let [stmt, ..] = stmts {
        stmt.span
    } else if let Some(expr) = expr {
        expr.span
    } else {
        // empty `then {}`
        return true;
    };
    is_expn_of(span, "if_chain").map_or(true, |span| span != if_chain_span)
}

/// Creates a `Span` for `let x = ..;` in an `if_chain!` call.
fn if_chain_local_span(cx: &LateContext<'_>, local: &Local<'_>, if_chain_span: Span) -> Span {
    let mut span = local.pat.span;
    if let Some(init) = local.init {
        span = span.to(init.span);
    }
    span.adjust(if_chain_span.ctxt().outer_expn());
    let sm = cx.sess().source_map();
    let span = sm.span_extend_to_prev_str(span, "let", false, true).unwrap_or(span);
    let span = sm.span_extend_to_next_char(span, ';', false);
    Span::new(
        span.lo() - BytePos(3),
        span.hi() + BytePos(1),
        span.ctxt(),
        span.parent(),
    )
}

declare_lint_pass!(MsrvAttrImpl => [MISSING_MSRV_ATTR_IMPL]);

impl LateLintPass<'_> for MsrvAttrImpl {
    fn check_item(&mut self, cx: &LateContext<'_>, item: &hir::Item<'_>) {
        if_chain! {
            if let hir::ItemKind::Impl(hir::Impl {
                of_trait: Some(lint_pass_trait_ref),
                self_ty,
                items,
                ..
            }) = &item.kind;
            if let Some(lint_pass_trait_def_id) = lint_pass_trait_ref.trait_def_id();
            let is_late_pass = match_def_path(cx, lint_pass_trait_def_id, &paths::LATE_LINT_PASS);
            if is_late_pass || match_def_path(cx, lint_pass_trait_def_id, &paths::EARLY_LINT_PASS);
            let self_ty = hir_ty_to_ty(cx.tcx, self_ty);
            if let ty::Adt(self_ty_def, _) = self_ty.kind();
            if self_ty_def.is_struct();
            if self_ty_def.all_fields().any(|f| {
                cx.tcx
                    .type_of(f.did)
                    .walk()
                    .filter(|t| matches!(t.unpack(), GenericArgKind::Type(_)))
                    .any(|t| match_type(cx, t.expect_ty(), &paths::RUSTC_VERSION))
            });
            if !items.iter().any(|item| item.ident.name == sym!(enter_lint_attrs));
            then {
                let context = if is_late_pass { "LateContext" } else { "EarlyContext" };
                let lint_pass = if is_late_pass { "LateLintPass" } else { "EarlyLintPass" };
                let span = cx.sess().source_map().span_through_char(item.span, '{');
                span_lint_and_sugg(
                    cx,
                    MISSING_MSRV_ATTR_IMPL,
                    span,
                    &format!("`extract_msrv_attr!` macro missing from `{lint_pass}` implementation"),
                    &format!("add `extract_msrv_attr!({context})` to the `{lint_pass}` implementation"),
                    format!("{}\n    extract_msrv_attr!({context});", snippet(cx, span, "..")),
                    Applicability::MachineApplicable,
                );
            }
        }
    }
}
