    use super::*;

    #[test]
    fn concat_null_semantics() {
        let f = |args: Vec<Option<Value>>| {
            eval_scalar_fn("CONCAT", &args).map(|v| match v {
                Value::Text(s) => s,
                Value::Null => "NULL".into(),
                other => other.to_string(),
            })
        };
        assert_eq!(
            f(vec![Some(Value::Text("a".into())), None, Some(Value::Text("b".into()))]),
            Some("NULL".into()),
            "None arg → NULL"
        );
        assert_eq!(
            f(vec![
                Some(Value::Text("a".into())),
                Some(Value::Null),
                Some(Value::Text("b".into()))
            ]),
            Some("NULL".into()),
            "Some(Null) arg → NULL (MySQL parity)"
        );
        assert_eq!(
            f(vec![
                Some(Value::Text("a".into())),
                Some(Value::Int(1)),
                Some(Value::Float(2.5))
            ]),
            Some("a12.5".into())
        );
    }

    #[test]
    fn fn_eval_through_expr() {
        use std::collections::BTreeMap;
        let r: BTreeMap<String, Value> = BTreeMap::new();
        // CONCAT('днк', '-', 'тест')
        let e = Expr::Fn {
            name: "CONCAT".into(),
            args: vec![
                Expr::Literal(Value::Text("днк".into())),
                Expr::Literal(Value::Text("-".into())),
                Expr::Literal(Value::Text("тест".into())),
            ],
        };
        assert_eq!(e.eval(&r), Some(Value::Text("днк-тест".into())));
        // CONCAT with a NULL literal → NULL
        let e = Expr::Fn {
            name: "CONCAT".into(),
            args: vec![
                Expr::Literal(Value::Text("a".into())),
                Expr::Literal(Value::Null),
            ],
        };
        assert_eq!(e.eval(&r), Some(Value::Null));
        // IFNULL(NULL, 'x') → 'x'; COALESCE(NULL, 'x', 'y') → 'x';
        // IF(1>0, 'yes', 'no') → 'yes'
        let e = Expr::Fn {
            name: "IFNULL".into(),
            args: vec![
                Expr::Literal(Value::Null),
                Expr::Literal(Value::Text("x".into())),
            ],
        };
        assert_eq!(e.eval(&r), Some(Value::Text("x".into())));
        let e = Expr::Fn {
            name: "COALESCE".into(),
            args: vec![
                Expr::Literal(Value::Null),
                Expr::Literal(Value::Text("x".into())),
                Expr::Literal(Value::Text("y".into())),
            ],
        };
        assert_eq!(e.eval(&r), Some(Value::Text("x".into())));
        let e = Expr::Fn {
            name: "IF".into(),
            args: vec![
                Expr::Cmp {
                    op: CmpOp::Gt,
                    left: Box::new(Expr::Literal(Value::Int(1))),
                    right: Box::new(Expr::Literal(Value::Int(0))),
                },
                Expr::Literal(Value::Text("yes".into())),
                Expr::Literal(Value::Text("no".into())),
            ],
        };
        assert_eq!(e.eval(&r), Some(Value::Text("yes".into())));
    }

    #[test]
    fn fn_string_numeric_basics() {
        use std::collections::BTreeMap;
        let r: BTreeMap<String, Value> = BTreeMap::new();
        let ev = |name: &str, args: Vec<Expr>| -> Option<Value> {
            Expr::Fn {
                name: name.into(),
                args,
            }
            .eval(&r)
        };
        let t = |s: &str| Expr::Literal(Value::Text(s.into()));
        let i = |n: i64| Expr::Literal(Value::Int(n));
        assert_eq!(ev("UPPER", vec![t("abc")]), Some(Value::Text("ABC".into())));
        assert_eq!(ev("LOWER", vec![t("ABC")]), Some(Value::Text("abc".into())));
        assert_eq!(ev("CHAR_LENGTH", vec![t("днк")]), Some(Value::Int(3)));
        assert_eq!(ev("LENGTH", vec![t("днк")]), Some(Value::Int(6)));
        assert_eq!(ev("TRIM", vec![t("  x  ")]), Some(Value::Text("x".into())));
        assert_eq!(
            ev("SUBSTRING", vec![t("wordpress"), i(1), i(3)]),
            Some(Value::Text("wor".into()))
        );
        assert_eq!(
            ev("SUBSTRING", vec![t("wordpress"), i(-5)]),
            Some(Value::Text("press".into()))
        );
        assert_eq!(
            ev("REPLACE", vec![t("a-b-c"), t("-"), t("+")]),
            Some(Value::Text("a+b+c".into()))
        );
        assert_eq!(ev("LEFT", vec![t("hello"), i(2)]), Some(Value::Text("he".into())));
        assert_eq!(ev("RIGHT", vec![t("hello"), i(2)]), Some(Value::Text("lo".into())));
        assert_eq!(ev("ABS", vec![i(-5)]), Some(Value::Int(5)));
        assert_eq!(ev("ROUND", vec![Expr::Literal(Value::Float(2.5))]), Some(Value::Float(3.0)));
        assert_eq!(ev("FLOOR", vec![Expr::Literal(Value::Float(2.9))]), Some(Value::Int(2)));
        assert_eq!(ev("CEIL", vec![Expr::Literal(Value::Float(2.1))]), Some(Value::Int(3)));
        assert_eq!(ev("SQRT", vec![i(-1)]), Some(Value::Null));
        assert_eq!(
            ev("CONCAT_WS", vec![t(","), t("a"), Value::Null.into_lit(), t("b")]),
            Some(Value::Text("a,b".into()))
        );
        assert_eq!(
            ev("GREATEST", vec![i(3), i(9), i(1)]),
            Some(Value::Int(9))
        );
        assert_eq!(ev("LEAST", vec![i(3), i(9), i(1)]), Some(Value::Int(1)));
        // NULLIF('a', 'a') → NULL; NULLIF('a', 'b') → 'a'
        assert_eq!(ev("NULLIF", vec![t("a"), t("a")]), Some(Value::Null));
        assert_eq!(ev("NULLIF", vec![t("a"), t("b")]), Some(Value::Text("a".into())));
        assert_eq!(eval_scalar_fn("NOPE", &[]), None);
    }

    trait IntoLit {
        fn into_lit(self) -> Expr;
    }
    impl IntoLit for Value {
        fn into_lit(self) -> Expr {
            Expr::Literal(self)
        }
    }

    use super::*;
    use std::collections::BTreeMap;

    fn row() -> BTreeMap<String, Value> {
        let mut m = BTreeMap::new();
        m.insert("total_usd".to_string(), Value::Float(350.0));
        m.insert("user_id".to_string(), Value::Int(14));
        m.insert("name".to_string(), Value::Text("crick".to_string()));
        m.insert("missing".to_string(), Value::Null);
        m
    }

    #[test]
    fn comparisons() {
        let r = row();
        let e = Expr::Cmp {
            op: CmpOp::Gt,
            left: Box::new(Expr::Column("total_usd".into())),
            right: Box::new(Expr::Literal(Value::Float(300.0))),
        };
        assert!(e.eval_bool(&r));

        let e2 = Expr::Cmp {
            op: CmpOp::Eq,
            left: Box::new(Expr::Column("name".into())),
            right: Box::new(Expr::Literal(Value::Text("crick".into()))),
        };
        assert!(e2.eval_bool(&r));

        let e3 = Expr::Cmp {
            op: CmpOp::Eq,
            left: Box::new(Expr::Column("user_id".into())),
            right: Box::new(Expr::Literal(Value::Int(99))),
        };
        assert!(!e3.eval_bool(&r));
    }

    #[test]
    fn and_or_not() {
        let r = row();
        let e = Expr::And(
            Box::new(Expr::Cmp {
                op: CmpOp::Gt,
                left: Box::new(Expr::Column("total_usd".into())),
                right: Box::new(Expr::Literal(Value::Float(100.0))),
            }),
            Box::new(Expr::Cmp {
                op: CmpOp::Lt,
                left: Box::new(Expr::Column("user_id".into())),
                right: Box::new(Expr::Literal(Value::Int(20))),
            }),
        );
        assert!(e.eval_bool(&r));
        assert!(e.describe().contains("AND"));

        // NULL comparison -> not truthy
        let en = Expr::Cmp {
            op: CmpOp::Eq,
            left: Box::new(Expr::Column("missing".into())),
            right: Box::new(Expr::Literal(Value::Int(0))),
        };
        assert!(!en.eval_bool(&r));
    }

    #[test]
    fn cast_mysql_semantics() {
        let signed = |lit: Value| Expr::Cast {
            inner: Box::new(Expr::Literal(lit)),
            ty: CastTy::Signed,
        };
        let r = BTreeMap::new();
        // Numeric prefix: "150" -> 150, "19.9" -> 20 (rounds), "abc" -> 0.
        assert_eq!(
            signed(Value::Text("150".into())).eval(&r),
            Some(Value::Int(150))
        );
        assert_eq!(
            signed(Value::Text("19.9".into())).eval(&r),
            Some(Value::Int(20))
        );
        assert_eq!(
            signed(Value::Text("abc".into())).eval(&r),
            Some(Value::Int(0))
        );
        // NULL propagates (SQL NULL, not "no value").
        assert_eq!(signed(Value::Null).eval(&r), Some(Value::Null));
        // CHAR stringifies.
        let c = Expr::Cast {
            inner: Box::new(Expr::Literal(Value::Int(42))),
            ty: CastTy::Char,
        };
        assert_eq!(c.eval(&r), Some(Value::Text("42".into())));
    }
