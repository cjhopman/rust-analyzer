mod never_type;
mod coercion;
mod regression;
mod simple;
mod patterns;
mod traits;
mod method_resolution;
mod macros;

use std::sync::Arc;

use hir_def::{
    body::{BodySourceMap, SyntheticSyntax},
    child_by_source::ChildBySource,
    db::DefDatabase,
    item_scope::ItemScope,
    keys,
    nameres::CrateDefMap,
    AssocItemId, DefWithBodyId, LocalModuleId, Lookup, ModuleDefId,
};
use hir_expand::InFile;
use insta::assert_snapshot;
use ra_db::{fixture::WithFixture, salsa::Database, FilePosition, SourceDatabase};
use ra_syntax::{
    algo,
    ast::{self, AstNode},
};
use stdx::format_to;

use crate::{db::HirDatabase, display::HirDisplay, test_db::TestDB, InferenceResult};

// These tests compare the inference results for all expressions in a file
// against snapshots of the expected results using insta. Use cargo-insta to
// update the snapshots.

fn type_at_pos(db: &TestDB, pos: FilePosition) -> String {
    let file = db.parse(pos.file_id).ok().unwrap();
    let expr = algo::find_node_at_offset::<ast::Expr>(file.syntax(), pos.offset).unwrap();
    let fn_def = expr.syntax().ancestors().find_map(ast::FnDef::cast).unwrap();
    let module = db.module_for_file(pos.file_id);
    let func = *module.child_by_source(db)[keys::FUNCTION]
        .get(&InFile::new(pos.file_id.into(), fn_def))
        .unwrap();

    let (_body, source_map) = db.body_with_source_map(func.into());
    if let Some(expr_id) = source_map.node_expr(InFile::new(pos.file_id.into(), &expr)) {
        let infer = db.infer(func.into());
        let ty = &infer[expr_id];
        return ty.display(db).to_string();
    }
    panic!("Can't find expression")
}

fn type_at(content: &str) -> String {
    let (db, file_pos) = TestDB::with_position(content);
    type_at_pos(&db, file_pos)
}

fn infer(ra_fixture: &str) -> String {
    infer_with_mismatches(ra_fixture, false)
}

fn infer_with_mismatches(content: &str, include_mismatches: bool) -> String {
    let (db, file_id) = TestDB::with_single_file(content);

    let mut buf = String::new();

    let mut infer_def = |inference_result: Arc<InferenceResult>,
                         body_source_map: Arc<BodySourceMap>| {
        let mut types = Vec::new();
        let mut mismatches = Vec::new();

        for (pat, ty) in inference_result.type_of_pat.iter() {
            let syntax_ptr = match body_source_map.pat_syntax(pat) {
                Ok(sp) => {
                    sp.map(|ast| ast.either(|it| it.syntax_node_ptr(), |it| it.syntax_node_ptr()))
                }
                Err(SyntheticSyntax) => continue,
            };
            types.push((syntax_ptr, ty));
        }

        for (expr, ty) in inference_result.type_of_expr.iter() {
            let syntax_ptr = match body_source_map.expr_syntax(expr) {
                Ok(sp) => {
                    sp.map(|ast| ast.either(|it| it.syntax_node_ptr(), |it| it.syntax_node_ptr()))
                }
                Err(SyntheticSyntax) => continue,
            };
            types.push((syntax_ptr, ty));
            if let Some(mismatch) = inference_result.type_mismatch_for_expr(expr) {
                mismatches.push((syntax_ptr, mismatch));
            }
        }

        // sort ranges for consistency
        types.sort_by_key(|(src_ptr, _)| {
            (src_ptr.value.range().start(), src_ptr.value.range().end())
        });
        for (src_ptr, ty) in &types {
            let node = src_ptr.value.to_node(&src_ptr.file_syntax(&db));

            let (range, text) = if let Some(self_param) = ast::SelfParam::cast(node.clone()) {
                (self_param.self_token().unwrap().text_range(), "self".to_string())
            } else {
                (src_ptr.value.range(), node.text().to_string().replace("\n", " "))
            };
            let macro_prefix = if src_ptr.file_id != file_id.into() { "!" } else { "" };
            format_to!(
                buf,
                "{}{} '{}': {}\n",
                macro_prefix,
                range,
                ellipsize(text, 15),
                ty.display(&db)
            );
        }
        if include_mismatches {
            mismatches.sort_by_key(|(src_ptr, _)| {
                (src_ptr.value.range().start(), src_ptr.value.range().end())
            });
            for (src_ptr, mismatch) in &mismatches {
                let range = src_ptr.value.range();
                let macro_prefix = if src_ptr.file_id != file_id.into() { "!" } else { "" };
                format_to!(
                    buf,
                    "{}{}: expected {}, got {}\n",
                    macro_prefix,
                    range,
                    mismatch.expected.display(&db),
                    mismatch.actual.display(&db),
                );
            }
        }
    };

    let module = db.module_for_file(file_id);
    let crate_def_map = db.crate_def_map(module.krate);

    let mut defs: Vec<DefWithBodyId> = Vec::new();
    visit_module(&db, &crate_def_map, module.local_id, &mut |it| defs.push(it));
    defs.sort_by_key(|def| match def {
        DefWithBodyId::FunctionId(it) => {
            it.lookup(&db).ast_id.to_node(&db).syntax().text_range().start()
        }
        DefWithBodyId::ConstId(it) => {
            it.lookup(&db).ast_id.to_node(&db).syntax().text_range().start()
        }
        DefWithBodyId::StaticId(it) => {
            it.lookup(&db).ast_id.to_node(&db).syntax().text_range().start()
        }
    });
    for def in defs {
        let (_body, source_map) = db.body_with_source_map(def);
        let infer = db.infer(def);
        infer_def(infer, source_map);
    }

    buf.truncate(buf.trim_end().len());
    buf
}

fn visit_module(
    db: &TestDB,
    crate_def_map: &CrateDefMap,
    module_id: LocalModuleId,
    cb: &mut dyn FnMut(DefWithBodyId),
) {
    visit_scope(db, crate_def_map, &crate_def_map[module_id].scope, cb);
    for impl_id in crate_def_map[module_id].scope.impls() {
        let impl_data = db.impl_data(impl_id);
        for &item in impl_data.items.iter() {
            match item {
                AssocItemId::FunctionId(it) => {
                    let def = it.into();
                    cb(def);
                    let body = db.body(def);
                    visit_scope(db, crate_def_map, &body.item_scope, cb);
                }
                AssocItemId::ConstId(it) => {
                    let def = it.into();
                    cb(def);
                    let body = db.body(def);
                    visit_scope(db, crate_def_map, &body.item_scope, cb);
                }
                AssocItemId::TypeAliasId(_) => (),
            }
        }
    }

    fn visit_scope(
        db: &TestDB,
        crate_def_map: &CrateDefMap,
        scope: &ItemScope,
        cb: &mut dyn FnMut(DefWithBodyId),
    ) {
        for decl in scope.declarations() {
            match decl {
                ModuleDefId::FunctionId(it) => {
                    let def = it.into();
                    cb(def);
                    let body = db.body(def);
                    visit_scope(db, crate_def_map, &body.item_scope, cb);
                }
                ModuleDefId::ConstId(it) => {
                    let def = it.into();
                    cb(def);
                    let body = db.body(def);
                    visit_scope(db, crate_def_map, &body.item_scope, cb);
                }
                ModuleDefId::StaticId(it) => {
                    let def = it.into();
                    cb(def);
                    let body = db.body(def);
                    visit_scope(db, crate_def_map, &body.item_scope, cb);
                }
                ModuleDefId::TraitId(it) => {
                    let trait_data = db.trait_data(it);
                    for &(_, item) in trait_data.items.iter() {
                        match item {
                            AssocItemId::FunctionId(it) => cb(it.into()),
                            AssocItemId::ConstId(it) => cb(it.into()),
                            AssocItemId::TypeAliasId(_) => (),
                        }
                    }
                }
                ModuleDefId::ModuleId(it) => visit_module(db, crate_def_map, it.local_id, cb),
                _ => (),
            }
        }
    }
}

fn ellipsize(mut text: String, max_len: usize) -> String {
    if text.len() <= max_len {
        return text;
    }
    let ellipsis = "...";
    let e_len = ellipsis.len();
    let mut prefix_len = (max_len - e_len) / 2;
    while !text.is_char_boundary(prefix_len) {
        prefix_len += 1;
    }
    let mut suffix_len = max_len - e_len - prefix_len;
    while !text.is_char_boundary(text.len() - suffix_len) {
        suffix_len += 1;
    }
    text.replace_range(prefix_len..text.len() - suffix_len, ellipsis);
    text
}

#[test]
fn typing_whitespace_inside_a_function_should_not_invalidate_types() {
    let (mut db, pos) = TestDB::with_position(
        "
        //- /lib.rs
        fn foo() -> i32 {
            <|>1 + 1
        }
    ",
    );
    {
        let events = db.log_executed(|| {
            let module = db.module_for_file(pos.file_id);
            let crate_def_map = db.crate_def_map(module.krate);
            visit_module(&db, &crate_def_map, module.local_id, &mut |def| {
                db.infer(def);
            });
        });
        assert!(format!("{:?}", events).contains("infer"))
    }

    let new_text = "
        fn foo() -> i32 {
            1
            +
            1
        }
    "
    .to_string();

    db.query_mut(ra_db::FileTextQuery).set(pos.file_id, Arc::new(new_text));

    {
        let events = db.log_executed(|| {
            let module = db.module_for_file(pos.file_id);
            let crate_def_map = db.crate_def_map(module.krate);
            visit_module(&db, &crate_def_map, module.local_id, &mut |def| {
                db.infer(def);
            });
        });
        assert!(!format!("{:?}", events).contains("infer"), "{:#?}", events)
    }
}

#[test]
fn no_such_field_diagnostics() {
    let diagnostics = TestDB::with_files(
        r"
        //- /lib.rs
        struct S { foo: i32, bar: () }
        impl S {
            fn new() -> S {
                S {
                    foo: 92,
                    baz: 62,
                }
            }
        }
        ",
    )
    .diagnostics()
    .0;

    assert_snapshot!(diagnostics, @r###"
    "baz: 62": no such field
    "{\n            foo: 92,\n            baz: 62,\n        }": Missing structure fields:
    - bar
    "###
    );
}

#[test]
fn no_such_field_with_feature_flag_diagnostics() {
    let diagnostics = TestDB::with_files(
        r#"
        //- /lib.rs crate:foo cfg:feature=foo
        struct MyStruct {
            my_val: usize,
            #[cfg(feature = "foo")]
            bar: bool,
        }

        impl MyStruct {
            #[cfg(feature = "foo")]
            pub(crate) fn new(my_val: usize, bar: bool) -> Self {
                Self { my_val, bar }
            }

            #[cfg(not(feature = "foo"))]
            pub(crate) fn new(my_val: usize, _bar: bool) -> Self {
                Self { my_val }
            }
        }
        "#,
    )
    .diagnostics()
    .0;

    assert_snapshot!(diagnostics, @r###""###);
}

#[test]
fn no_such_field_with_feature_flag_diagnostics_on_struct_lit() {
    let diagnostics = TestDB::with_files(
        r#"
        //- /lib.rs crate:foo cfg:feature=foo
        struct S {
            #[cfg(feature = "foo")]
            foo: u32,
            #[cfg(not(feature = "foo"))]
            bar: u32,
        }

        impl S {
            #[cfg(feature = "foo")]
            fn new(foo: u32) -> Self {
                Self { foo }
            }
            #[cfg(not(feature = "foo"))]
            fn new(bar: u32) -> Self {
                Self { bar }
            }
        }
        "#,
    )
    .diagnostics()
    .0;

    assert_snapshot!(diagnostics, @r###""###);
}

#[test]
fn no_such_field_with_feature_flag_diagnostics_on_struct_fields() {
    let diagnostics = TestDB::with_files(
        r#"
        //- /lib.rs crate:foo cfg:feature=foo
        struct S {
            #[cfg(feature = "foo")]
            foo: u32,
            #[cfg(not(feature = "foo"))]
            bar: u32,
        }

        impl S {
            fn new(val: u32) -> Self {
                Self {
                    #[cfg(feature = "foo")]
                    foo: val,
                    #[cfg(not(feature = "foo"))]
                    bar: val,
                }
            }
        }
        "#,
    )
    .diagnostics()
    .0;

    assert_snapshot!(diagnostics, @r###""###);
}

#[test]
fn missing_record_pat_field_diagnostic() {
    let diagnostics = TestDB::with_files(
        r"
        //- /lib.rs
        struct S { foo: i32, bar: () }
        fn baz(s: S) {
            let S { foo: _ } = s;
        }
        ",
    )
    .diagnostics()
    .0;

    assert_snapshot!(diagnostics, @r###"
    "{ foo: _ }": Missing structure fields:
    - bar
    "###
    );
}

#[test]
fn missing_record_pat_field_no_diagnostic_if_not_exhaustive() {
    let diagnostics = TestDB::with_files(
        r"
        //- /lib.rs
        struct S { foo: i32, bar: () }
        fn baz(s: S) -> i32 {
            match s {
                S { foo, .. } => foo,
            }
        }
        ",
    )
    .diagnostics()
    .0;

    assert_snapshot!(diagnostics, @"");
}
