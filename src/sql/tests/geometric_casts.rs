use super::*;

#[test]
fn geometric_casts_share_function_results_and_postgresql_contexts() {
    for mode in [
        crate::config::TransactionMode::Mvcc,
        crate::config::TransactionMode::Locking,
    ] {
        let mut config = test_config("geometric-cast-contracts");
        config.transaction_mode = mode;
        let mut budget = test_engine_budget(&config, 1 << 29);
        let mut engine = Engine::new(&config, &mut budget).unwrap();
        let mut session = ConfiguredTransactionSession::new(&config, &mut budget);
        for (source, target, literal, expected) in [
            ("point", "box", "(1,2)", "(1,2),(1,2)"),
            ("lseg", "point", "[(0,0),(2,4)]", "(1,2)"),
            (
                "path",
                "polygon",
                "((0,0),(2,0),(2,2))",
                "((0,0),(2,0),(2,2))",
            ),
            ("box", "point", "(2,4),(0,0)", "(1,2)"),
            ("box", "lseg", "(2,4),(0,0)", "[(2,4),(0,0)]"),
            ("box", "polygon", "(2,4),(0,0)", "((0,0),(0,4),(2,4),(2,0))"),
            ("box", "circle", "(2,4),(0,0)", "<(1,2),2.23606797749979>"),
            (
                "polygon",
                "point",
                "((0,0),(2,0),(2,2))",
                "(1.3333333333333333,0.6666666666666666)",
            ),
            (
                "polygon",
                "path",
                "((0,0),(2,0),(2,2))",
                "((0,0),(2,0),(2,2))",
            ),
            ("polygon", "box", "((0,0),(2,0),(2,2))", "(2,2),(0,0)"),
            (
                "polygon",
                "circle",
                "((0,0),(2,0),(2,2))",
                "<(1.3333333333333333,0.6666666666666666),1.308077670527261>",
            ),
            ("circle", "point", "<(1,1),2>", "(1,1)"),
            (
                "circle",
                "box",
                "<(1,1),2>",
                "(2.414213562373095,2.414213562373095),(-0.4142135623730949,-0.4142135623730949)",
            ),
        ] {
            for sql in [
                format!("SELECT ('{literal}'::{source})::{target}"),
                format!("SELECT pg_catalog.{target}('{literal}'::{source})"),
            ] {
                assert_eq!(
                    data_rows(&session.success(&mut engine, &sql, true)),
                    [expected],
                    "{sql}"
                );
            }
        }
        for conversion in [
            "'<(1,1),2>'::circle::polygon",
            "polygon('<(1,1),2>'::circle)",
        ] {
            let sql = format!(
                "SELECT npoints(value),round(area(value::path)::numeric,8),box(value) FROM (SELECT {conversion} AS value) converted"
            );
            assert_eq!(
                data_rows(&session.success(&mut engine, &sql, true)),
                ["12|12.00000000|(3,3),(-1,-1)"]
            );
        }
        assert_eq!(
            error_sqlstates(&session.execute(&mut engine, "SELECT area(NULL::polygon)", true)),
            ["42883"]
        );
        assert_eq!(data_rows(&session.success(&mut engine,
            "SELECT count(*),count(DISTINCT oid),count(*) FILTER(WHERE castcontext='a'),count(*) FILTER(WHERE castcontext='i') \
               FROM pg_cast WHERE castsource IN(600,601,602,603,604,628,718) AND casttarget IN(600,601,602,603,604,628,718); \
             SELECT '((1,2))'::path::polygon, NULL::path::polygon IS NULL; \
             SELECT array_dims(value),value[0]::text,value[1] IS NULL \
               FROM (SELECT '[0:1]={\"((1,2))\",NULL}'::path[]::polygon[] AS value) converted", true)),
            ["14|14|4|0", "((1,2))|t", "[0:1]|((1,2))|t"]);
        for source in crate::sql::types::GeometryKind::ALL {
            for target in crate::sql::types::GeometryKind::ALL {
                if source == target
                    || crate::sql::catalog::builtin_cast_context(source.oid(), target.oid())
                        .is_some()
                {
                    continue;
                }
                let sql = format!("SELECT NULL::{}::{}", source.name(), target.name());
                assert_eq!(
                    error_sqlstates(&session.execute(&mut engine, &sql, true)),
                    ["42846"],
                    "{sql}"
                );
            }
        }
        for sql in [
            "SELECT '[(0,0),(1,1)]'::path::polygon",
            "SELECT polygon('[(0,0),(1,1)]'::path)",
        ] {
            assert_eq!(
                error_sqlstates(&session.execute(&mut engine, sql, true)),
                ["22023"]
            );
        }
        for sql in [
            "SELECT '<(0,0),0>'::circle::polygon",
            "SELECT polygon('<(0,0),0>'::circle)",
        ] {
            assert_eq!(
                error_sqlstates(&session.execute(&mut engine, sql, true)),
                ["0A000"]
            );
        }
        for sql in [
            "SELECT NULL::circle[]::line[]",
            "SELECT ARRAY[]::circle[]::line[]",
        ] {
            assert_eq!(
                error_sqlstates(&session.execute(&mut engine, sql, true)),
                ["42846"]
            );
        }
        session.success(&mut engine,
            "CREATE SEQUENCE geometric_effects; \
             CREATE FUNCTION takes_geometric_box(value box) RETURNS integer LANGUAGE sql AS 'SELECT 1'", true);
        let implicit = session.execute(
            &mut engine,
            "SELECT takes_geometric_box(point(nextval('geometric_effects')::double precision,0))",
            true,
        );
        assert_eq!(error_sqlstates(&implicit), ["42883"]);
        assert_eq!(data_rows(&session.success(&mut engine,
            "SELECT nextval('geometric_effects'); ALTER SEQUENCE geometric_effects RESTART WITH 1; \
             SELECT point(nextval('geometric_effects')::double precision,0)::box; SELECT nextval('geometric_effects')", true)),
            ["1", "(1,0),(1,0)", "2"]);
    }
}
