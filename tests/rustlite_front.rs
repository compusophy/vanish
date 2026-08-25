//! rustlite front-end evals: lexer, parser, precedence, and every refusal
//! path. golden tests pin the exact AST (source → tree), so a parser change
//! that silently reorders or re-associates fails here rather than surfacing
//! as wrong wasm bytes three layers downstream. negative controls are as
//! load-bearing as the positives: the language's SMALLNESS is the feature,
//! and anything outside it must be refused loudly at the door.

use vanish::cartridges::rustlite::{lex, parse, BinOp, Expr, Stmt, Tok, Ty};

fn bin(op: BinOp, l: Expr, r: Expr) -> Expr {
    Expr::Binary(op, Box::new(l), Box::new(r))
}

// ---- lexer -----------------------------------------------------------------

#[test]
fn lexes_operators_multi_char_before_single() {
    // `<=` must not lex as `<` then `=`; same for ==, !=, &&, ||, ->
    assert_eq!(
        lex("a <= b").unwrap(),
        vec![
            Tok::Ident("a".into()),
            Tok::Le,
            Tok::Ident("b".into())
        ]
    );
    assert_eq!(lex("a==b").unwrap(), vec![Tok::Ident("a".into()), Tok::EqEq, Tok::Ident("b".into())]);
    assert_eq!(lex("a!=b").unwrap(), vec![Tok::Ident("a".into()), Tok::Ne, Tok::Ident("b".into())]);
    assert_eq!(lex("a&&b").unwrap(), vec![Tok::Ident("a".into()), Tok::AndAnd, Tok::Ident("b".into())]);
    assert_eq!(lex("a||b").unwrap(), vec![Tok::Ident("a".into()), Tok::OrOr, Tok::Ident("b".into())]);
    assert_eq!(lex("f(x)->i32").unwrap()[4], Tok::Arrow);
}

#[test]
fn lexes_comments_and_rejects_unknown_characters() {
    let toks = lex("let x = 1; // trailing comment\nlet y = 2;").unwrap();
    assert_eq!(toks.len(), 10, "comments contribute no tokens");
    let err = lex("let s = ~1;").unwrap_err();
    assert!(err.msg.contains("unexpected character"), "{err:?}");
}

#[test]
fn string_literals_lex_and_parse_to_str_expressions() {
    // a literal is an expression (its value: the packed (ptr, len) of its
    // bytes — typed i64 by the checker, laid out by the emitter).
    let toks = lex("let s = \"hello\";").unwrap();
    assert!(toks.contains(&Tok::Str("hello".into())), "{toks:?}");
    let fns = parse("fn f() -> i64 { return \"hello\"; }").unwrap().fns;
    assert_eq!(
        fns[0].body.stmts[0],
        Stmt::Return(Some(Expr::StrLit("hello".into())))
    );
    let fns = parse("fn f() -> i32 { return unpack_len(\"\"); }").unwrap().fns;
    let Stmt::Return(Some(Expr::Call { args, .. })) = &fns[0].body.stmts[0] else {
        panic!("expected a call");
    };
    assert_eq!(args[0], Expr::StrLit(String::new()), "the empty string is a literal too");
    // no escapes, no unterminated strings — both named.
    let err = lex("\"a\\n\"").unwrap_err();
    assert!(err.msg.contains("escape"), "{err:?}");
    let err = lex("\"open").unwrap_err();
    assert!(err.msg.contains("unterminated"), "{err:?}");
}

#[test]
fn float_requires_digit_after_the_dot() {
    // `1.5` is a float; `1.` has no digit after the dot, so the dot is NOT
    // part of the literal and hits the unknown-character refusal instead.
    let toks = lex("1.5").unwrap();
    assert_eq!(toks, vec![Tok::Float(1.5)]);
    let err = lex("1 .").unwrap_err();
    assert!(err.msg.contains("unexpected character"), "{err:?}");
}

// ---- parsing: goldens --------------------------------------------------------

#[test]
fn parses_a_full_function_with_params_and_return() {
    let fns = parse("fn add(a: i64, b: i64) -> i64 { return a + b; }").unwrap().fns;
    assert_eq!(fns.len(), 1);
    assert_eq!(fns[0].sig.name, "add");
    assert!(!fns[0].is_pub, "a plain fn is not exported");
    assert_eq!(
        fns[0].sig.params,
        vec![("a".to_string(), Ty::I64), ("b".to_string(), Ty::I64)]
    );
    assert_eq!(fns[0].sig.ret, Some(Ty::I64));
    assert_eq!(
        fns[0].body.stmts,
        vec![Stmt::Return(Some(bin(
            BinOp::Add,
            Expr::Var("a".into()),
            Expr::Var("b".into())
        )))]
    );
}

#[test]
fn precedence_becomes_tree_shape() {
    // * binds tighter than +: a + b * c === a + (b * c)
    let fns = parse("fn f(a: i32, b: i32, c: i32) -> i32 { return a + b * c; }").unwrap().fns;
    let Stmt::Return(Some(expr)) = &fns[0].body.stmts[0] else {
        panic!("expected return");
    };
    let expected = bin(
        BinOp::Add,
        Expr::Var("a".into()),
        bin(BinOp::Mul, Expr::Var("b".into()), Expr::Var("c".into())),
    );
    assert_eq!(*expr, expected);
}

#[test]
fn left_associativity_for_same_precedence() {
    // a - b - c must be (a - b) - c, never a - (b - c)
    let fns = parse("fn f() -> i32 { return 10 - 4 - 3; }").unwrap().fns;
    let Stmt::Return(Some(expr)) = &fns[0].body.stmts[0] else {
        panic!("expected return");
    };
    let expected = bin(
        BinOp::Sub,
        bin(BinOp::Sub, Expr::IntLit(10), Expr::IntLit(4)),
        Expr::IntLit(3),
    );
    assert_eq!(*expr, expected);
}

#[test]
fn comparison_binds_looser_than_arithmetic_and_yields_bool() {
    // a + b < c * d  ===  (a + b) < (c * d)
    let fns = parse("fn f(a: i32, b: i32, c: i32, d: i32) -> bool { return a + b < c * d; }")
        .unwrap()
        .fns;
    let Stmt::Return(Some(expr)) = &fns[0].body.stmts[0] else {
        panic!("expected return");
    };
    let expected = bin(
        BinOp::Lt,
        bin(BinOp::Add, Expr::Var("a".into()), Expr::Var("b".into())),
        bin(BinOp::Mul, Expr::Var("c".into()), Expr::Var("d".into())),
    );
    assert_eq!(*expr, expected);
}

#[test]
fn parens_override_precedence() {
    let fns = parse("fn f() -> i32 { return (1 + 2) * 3; }").unwrap().fns;
    let Stmt::Return(Some(expr)) = &fns[0].body.stmts[0] else {
        panic!("expected return");
    };
    let expected = bin(
        BinOp::Mul,
        bin(BinOp::Add, Expr::IntLit(1), Expr::IntLit(2)),
        Expr::IntLit(3),
    );
    assert_eq!(*expr, expected);
}

#[test]
fn while_loop_let_assign_and_return_parse() {
    let src = r#"
        fn count(n: i32) -> i32 {
            let i: i32 = 0;
            while i < n {
                i = i + 1;
            }
            return i;
        }
    "#;
    let fns = parse(src).unwrap().fns;
    assert_eq!(fns.len(), 1);
    assert_eq!(fns[0].sig.name, "count");
    match &fns[0].body.stmts[0] {
        Stmt::Let { name, ty, init } => {
            assert_eq!(name, "i");
            assert_eq!(*ty, Ty::I32);
            assert_eq!(*init, Expr::IntLit(0));
        }
        other => panic!("expected let, got {other:?}"),
    }
    let Stmt::While { cond, body } = &fns[0].body.stmts[1] else {
        panic!("expected while");
    };
    assert_eq!(
        *cond,
        bin(BinOp::Lt, Expr::Var("i".into()), Expr::Var("n".into()))
    );
    // the loop body carries the assignment: i = i + 1
    match &body.stmts[0] {
        Stmt::Assign { name, value } => {
            assert_eq!(name, "i");
            assert_eq!(
                *value,
                bin(BinOp::Add, Expr::Var("i".into()), Expr::IntLit(1))
            );
        }
        other => panic!("expected assignment in loop body, got {other:?}"),
    }
    match &fns[0].body.stmts[2] {
        Stmt::Return(Some(Expr::Var(v))) => assert_eq!(v, "i"),
        other => panic!("expected `return i;`, got {other:?}"),
    }
}

#[test]
fn if_else_and_else_if_parse_to_nested_ifs() {
    // `else if` is sugar: the else block holds exactly one If, so neither
    // the checker nor the emitter ever meets a third form.
    let src = r#"
        fn sign(x: i64) -> i64 {
            if x < 0 { return -1; } else if x == 0 { return 0; } else { return 1; }
        }
    "#;
    let fns = parse(src).unwrap().fns;
    let Stmt::If { cond, then, els } = &fns[0].body.stmts[0] else {
        panic!("expected if, got {:?}", fns[0].body.stmts[0]);
    };
    assert_eq!(*cond, bin(BinOp::Lt, Expr::Var("x".into()), Expr::IntLit(0)));
    assert_eq!(then.stmts.len(), 1);
    let els = els.as_ref().expect("else present");
    assert_eq!(els.stmts.len(), 1, "else-if is ONE nested If in the else block");
    let Stmt::If { els: inner_els, .. } = &els.stmts[0] else {
        panic!("expected nested if");
    };
    assert!(inner_els.is_some(), "the final else belongs to the inner if");

    // no else at all is also fine.
    let fns = parse("fn f(x: i64) -> i64 { if x > 0 { x = 0; } return x; }").unwrap().fns;
    let Stmt::If { els, .. } = &fns[0].body.stmts[0] else {
        panic!("expected if");
    };
    assert!(els.is_none());
}

#[test]
fn extern_block_declares_host_imports_and_pub_marks_exports() {
    let src = r#"
        extern "C" {
            fn now_ms() -> i64;
            fn log(level: i32, ptr: i32, len: i32);
        }
        pub fn cart_alloc(size: i32) -> i32 { return 8; }
        fn helper() -> i64 { return now_ms(); }
    "#;
    let p = parse(src).unwrap();
    assert_eq!(p.externs.len(), 2);
    assert_eq!(p.externs[0].name, "now_ms");
    assert_eq!(p.externs[0].ret, Some(Ty::I64));
    assert_eq!(p.externs[1].name, "log");
    assert_eq!(p.externs[1].params.len(), 3);
    assert_eq!(p.externs[1].ret, None);
    assert_eq!(p.fns.len(), 2);
    assert!(p.fns[0].is_pub, "pub fn is exported");
    assert!(!p.fns[1].is_pub);
}

#[test]
fn extern_with_any_abi_but_c_is_refused() {
    let err = parse("extern \"system\" { fn f(); } fn g() -> i32 { return 0; }").unwrap_err();
    assert!(err.msg.contains("extern \"C\""), "{err:?}");
    // an extern block alone is not a program — nothing to run.
    let err = parse("extern \"C\" { fn now_ms() -> i64; }").unwrap_err();
    assert!(err.msg.contains("no functions"), "{err:?}");
    // a prototype with a body is a fn, not an extern.
    let err = parse("extern \"C\" { fn f() { } } fn g() -> i32 { return 0; }").unwrap_err();
    assert!(err.msg.contains("Semi"), "{err:?}");
}

#[test]
fn untyped_let_is_refused_the_annotation_is_required_in_v1() {
    let err = parse("fn f() -> i32 { let x = 1; return x; }").unwrap_err();
    assert!(
        err.msg.contains("type name") || err.msg.contains("found"),
        "{err:?}"
    );
}

#[test]
fn unary_minus_binds_tightest_and_nests() {
    let fns = parse("fn f() -> i32 { return -(-5); }").unwrap().fns;
    let Stmt::Return(Some(expr)) = &fns[0].body.stmts[0] else {
        panic!("expected return");
    };
    assert_eq!(
        *expr,
        Expr::Unary(
            vanish::cartridges::rustlite::UnOp::Neg,
            Box::new(Expr::Unary(vanish::cartridges::rustlite::UnOp::Neg, Box::new(Expr::IntLit(5))))
        )
    );
}

// ---- refusals: the language stays small, loudly ------------------------------

#[test]
fn unknown_types_are_refused_with_the_closed_set_named() {
    let err = parse("fn f(s: string) -> i32 { return 0; }").unwrap_err();
    assert!(err.msg.contains("i32") && err.msg.contains("bool"), "{err:?}");
}

#[test]
fn missing_semicolon_is_refused_not_swallowed() {
    let err = parse("fn f() -> i32 { return 1 }").unwrap_err();
    assert!(err.msg.contains("Semi"), "{err:?}");
}

#[test]
fn unclosed_block_is_refused() {
    let err = parse("fn f() -> i32 { return 1; ").unwrap_err();
    assert!(err.msg.contains("closed"), "{err:?}");
}

#[test]
fn empty_source_is_refused() {
    let err = parse("").unwrap_err();
    assert!(err.msg.contains("no functions"), "{err:?}");
    let err = parse("// just a comment\n").unwrap_err();
    assert!(err.msg.contains("no functions"), "{err:?}");
}

#[test]
fn garbage_after_a_complete_fn_is_refused() {
    // trailing junk must not be silently dropped by a lenient parser.
    let err = parse("fn f() -> i32 { return 1; } ~~~").unwrap_err();
    assert!(err.msg.contains("unexpected character"), "{err:?}");
}

#[test]
fn trailing_comma_in_params_parses_like_rust() {
    let fns = parse("fn f(a: i32,) -> i32 { return 0; }").unwrap().fns;
    assert_eq!(fns[0].sig.params.len(), 1);
}

#[test]
fn malformed_parameter_list_is_refused() {
    let err = parse("fn f(a i32) -> i32 { return 0; }").unwrap_err();
    assert!(err.msg.contains("':'") || err.msg.contains("Colon"), "{err:?}");
}
