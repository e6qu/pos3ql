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

	exec := func(sql string, args ...any) {
		tag, err := conn.Exec(ctx, sql, args...)
		if err != nil {
			fmt.Printf("ERR %s\n", firstLine(err))
			return
		}
		fmt.Printf("exec %s\n", tag.String())
	}

	conn.Exec(ctx, "DROP TABLE IF EXISTS pgx_drv")
	exec("CREATE TABLE pgx_drv (id int PRIMARY KEY, name text, score float8)")
	// Binary-encoded parameters (pgx default).
	exec("INSERT INTO pgx_drv VALUES ($1,$2,$3)", 1, "ada", 9.5)
	exec("INSERT INTO pgx_drv VALUES ($1,$2,$3)", 2, "bob", 7.25)
	exec("INSERT INTO pgx_drv VALUES ($1,$2,$3)", 3, "cyd", nil)

	// Parameterized select; binary result decoding.
	rows, err := conn.Query(ctx, "SELECT id, name, score FROM pgx_drv WHERE id <= $1 ORDER BY id", 2)
	if err != nil {
		fmt.Println("ERR query:", firstLine(err))
	} else {
		for rows.Next() {
			var id int
			var name string
			var score *float64
			if err := rows.Scan(&id, &name, &score); err != nil {
				fmt.Println("ERR scan:", firstLine(err))
				break
			}
			s := "nil"
			if score != nil {
				s = fmt.Sprintf("%g", *score)
			}
			fmt.Printf("row %d|%s|%s\n", id, name, s)
		}
		rows.Close()
	}

	exec("UPDATE pgx_drv SET score = $1 WHERE id = $2", 10.0, 3)
	exec("DELETE FROM pgx_drv WHERE id = $1", 2)

	// Transaction rollback must not persist.
	tx, _ := conn.Begin(ctx)
	tx.Exec(ctx, "INSERT INTO pgx_drv VALUES ($1,$2,$3)", 9, "tmp", nil)
	tx.Rollback(ctx)
	var n int
	conn.QueryRow(ctx, "SELECT count(*) FROM pgx_drv WHERE id = 9").Scan(&n)
	fmt.Printf("after rollback id=9 count=%d\n", n)

	// Catalog introspection.
	crows, _ := conn.Query(ctx,
		"SELECT attname, format_type(atttypid, atttypmod), attnotnull "+
			"FROM pg_attribute WHERE attrelid = 'pgx_drv'::regclass AND attnum > 0 "+
			"AND NOT attisdropped ORDER BY attnum")
	for crows.Next() {
		var name, typ string
		var notnull bool
		crows.Scan(&name, &typ, &notnull)
		fmt.Printf("col %s|%s|notnull=%t\n", name, typ, notnull)
	}
	crows.Close()
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
