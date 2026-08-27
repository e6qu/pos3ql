// Differential driver test for jackc/pgx against pos3ql and real PostgreSQL.
//
// pgx speaks the extended protocol with binary parameter and result formats,
// so it exercises pos3ql's binary codecs. Prints a deterministic transcript
// (prepared-statement CRUD, transaction rollback, catalog introspection) so
// the CI harness can diff pos3ql's output against PostgreSQL's.
//
// Usage:  go run pgx_test.go <host> <port>
package main

import (
	"context"
	"fmt"
	"os"

	"github.com/jackc/pgx/v5"
)

func main() {
	host, port := "127.0.0.1", "5432"
	if len(os.Args) > 1 {
		host = os.Args[1]
	}
	if len(os.Args) > 2 {
		port = os.Args[2]
	}
	ctx := context.Background()
	url := fmt.Sprintf("postgres://postgres@%s:%s/postgres?sslmode=disable", host, port)
	conn, err := pgx.Connect(ctx, url)
	if err != nil {
		fmt.Println("FATAL connect:", err)
		os.Exit(1)
	}
	defer conn.Close(ctx)
	must := func(label string, err error) {
		if err != nil {
			fmt.Printf("FATAL %s: %s\n", label, firstLine(err))
			os.Exit(1)
		}
	}

	exec := func(sql string, args ...any) {
		tag, err := conn.Exec(ctx, sql, args...)
		must("exec", err)
		fmt.Printf("exec %s\n", tag.String())
	}

	_, err = conn.Exec(ctx, "DROP TABLE IF EXISTS pgx_drv")
	must("drop pgx_drv", err)
	_, err = conn.Exec(ctx, "DROP TABLE IF EXISTS pgx_types")
	must("drop pgx_types", err)
	_, err = conn.Exec(ctx, "DROP TABLE IF EXISTS pgx_sample_source")
	must("drop pgx_sample_source", err)
	exec("CREATE TABLE pgx_drv (id int PRIMARY KEY, name text, score float8)")
	// Binary-encoded parameters (pgx default).
	exec("INSERT INTO pgx_drv VALUES ($1,$2,$3)", 1, "ada", 9.5)
	exec("INSERT INTO pgx_drv VALUES ($1,$2,$3)", 2, "bob", 7.25)
	exec("INSERT INTO pgx_drv VALUES ($1,$2,$3)", 3, "cyd", nil)

	// Parameterized select; binary result decoding.
	rows, err := conn.Query(ctx, "SELECT id, name, score FROM pgx_drv WHERE id <= $1 ORDER BY id", 2)
	must("query rows", err)
	for rows.Next() {
		var id int
		var name string
		var score *float64
		must("scan row", rows.Scan(&id, &name, &score))
		s := "nil"
		if score != nil {
			s = fmt.Sprintf("%g", *score)
		}
		fmt.Printf("row %d|%s|%s\n", id, name, s)
	}
	rows.Close()
	must("iterate rows", rows.Err())

	exec("UPDATE pgx_drv SET score = $1 WHERE id = $2", 10.0, 3)
	exec("DELETE FROM pgx_drv WHERE id = $1", 2)

	exec("CREATE TABLE pgx_types (id int PRIMARY KEY, label varchar(4), amount numeric(7,2), ids integer[], note jsonb, span int4range, key uuid)")
	exec("INSERT INTO pgx_types VALUES (1,$1,12.345,$2,'{\"ready\": true}','[1,5)','00112233-4455-6677-8899-aabbccddeeff')",
		"wide", []int32{1, 2, 3})
	var label, amount, note, span, key string
	var ids []int32
	err = conn.QueryRow(ctx,
		"SELECT label, amount::text, ids, note::text, span::text, key::text FROM pgx_types").
		Scan(&label, &amount, &ids, &note, &span, &key)
	must("scan typed row", err)
	fmt.Printf("typed %s|%s|%v|%s|%s|%s\n", label, amount, ids, note, span, key)

	exec("SET application_name TO 'outside'")
	exec("DROP TABLE IF EXISTS pgx_routine_log")
	exec("CREATE TABLE pgx_routine_log(value integer)")
	exec("CREATE OR REPLACE FUNCTION pgx_standard(value integer) RETURNS integer LANGUAGE SQL IMMUTABLE STRICT SET application_name TO 'driver' RETURN value + 1")
	exec("CREATE OR REPLACE PROCEDURE pgx_standard_proc(value integer) LANGUAGE SQL AS 'INSERT INTO pgx_routine_log VALUES (value)'")
	var routineValue int
	var applicationName string
	must("standard routine", conn.QueryRow(ctx,
		"SELECT pgx_standard($1), current_setting('application_name')", 41).
		Scan(&routineValue, &applicationName))
	fmt.Printf("standard routine %d|%s\n", routineValue, applicationName)
	exec("CALL pgx_standard_proc($1)", 43)
	must("standard procedure", conn.QueryRow(ctx, "SELECT value FROM pgx_routine_log").Scan(&routineValue))
	fmt.Printf("standard procedure %d\n", routineValue)
	exec("CREATE OR REPLACE PROCEDURE pgx_plpgsql(value integer) LANGUAGE plpgsql AS 'BEGIN INSERT INTO pgx_routine_log VALUES (value + 1); END'")
	exec("CALL pgx_plpgsql($1)", 44)
	must("plpgsql procedure", conn.QueryRow(ctx, "SELECT value FROM pgx_routine_log WHERE value = 45").Scan(&routineValue))
	fmt.Printf("plpgsql procedure %d\n", routineValue)

	// Transaction rollback must not persist.
	tx, err := conn.Begin(ctx)
	must("begin", err)
	_, err = tx.Exec(ctx, "INSERT INTO pgx_drv VALUES ($1,$2,$3)", 9, "tmp", nil)
	must("transaction insert", err)
	must("rollback", tx.Rollback(ctx))
	var n int
	must("rollback count", conn.QueryRow(ctx, "SELECT count(*) FROM pgx_drv WHERE id = 9").Scan(&n))
	fmt.Printf("after rollback id=9 count=%d\n", n)

	// Catalog introspection.
	crows, err := conn.Query(ctx,
		"SELECT attname, format_type(atttypid, atttypmod), attnotnull "+
			"FROM pg_attribute WHERE attrelid = 'pgx_drv'::regclass AND attnum > 0 "+
			"AND NOT attisdropped ORDER BY attnum")
	must("query columns", err)
	for crows.Next() {
		var name, typ string
		var notnull bool
		must("scan column", crows.Scan(&name, &typ, &notnull))
		fmt.Printf("col %s|%s|notnull=%t\n", name, typ, notnull)
	}
	crows.Close()
	must("iterate columns", crows.Err())
	trows, err := conn.Query(ctx,
		"SELECT attname, format_type(atttypid, atttypmod) "+
			"FROM pg_attribute WHERE attrelid = 'pgx_types'::regclass AND attnum > 0 "+
			"AND NOT attisdropped ORDER BY attnum")
	must("query typed columns", err)
	for trows.Next() {
		var name, typ string
		must("scan typed column", trows.Scan(&name, &typ))
		fmt.Printf("typed-col %s|%s\n", name, typ)
	}
	trows.Close()
	must("iterate typed columns", trows.Err())

	exec("CREATE TABLE pgx_sample_source (id integer PRIMARY KEY)")
	exec("INSERT INTO pgx_sample_source SELECT value FROM generate_series(1,20) value")
	var sampled int
	must("bernoulli sample", conn.QueryRow(ctx,
		"SELECT count(*) FROM pgx_sample_source TABLESAMPLE BERNOULLI ($1) REPEATABLE ($2)",
		100.0, 42.0).Scan(&sampled))
	fmt.Printf("sample rows=%d\n", sampled)
	must("system sample", conn.QueryRow(ctx,
		"SELECT count(*) FROM pgx_sample_source TABLESAMPLE SYSTEM ($1) REPEATABLE ($2)",
		0.0, 42.0).Scan(&sampled))
	fmt.Printf("system sample rows=%d\n", sampled)
}

func firstLine(err error) string {
	s := err.Error()
	for i, r := range s {
		if r == '\n' {
			return s[:i]
		}
	}
	return s
}
