// Differential driver test for pgJDBC against pos3ql and real PostgreSQL.
//
// Prints a deterministic transcript of a real JDBC session — connection,
// prepared-statement CRUD, transactions, and the DatabaseMetaData
// introspection that ORMs and tools issue — so the CI harness can run it
// against both engines and diff the output. Any divergence (wrong rows,
// wrong column metadata, an exception on one side only) shows up in the diff.
//
// Usage:  java JdbcTest <host> <port>
// The pgJDBC jar must be on the classpath.

import java.sql.*;
import java.util.UUID;
import org.postgresql.util.PGobject;

public class JdbcTest {
    static StringBuilder out = new StringBuilder();

    static void line(String s) { out.append(s).append('\n'); }

    public static void main(String[] args) throws Exception {
        String host = args.length > 0 ? args[0] : "127.0.0.1";
        String port = args.length > 1 ? args[1] : "5432";
        String url = "jdbc:postgresql://" + host + ":" + port + "/postgres?sslmode=disable";

        try (Connection c = DriverManager.getConnection(url, "postgres", "")) {
            ddlAndCrud(c);
            transactions(c);
            typedValuesAndPortalPaging(c);
            tableSampling(c);
            metadata(c);
        } catch (SQLException e) {
            line("FATAL " + sqlstate(e) + " " + firstLine(e.getMessage()));
            System.out.print(out);
            System.exit(1);
        }
        System.out.print(out);
    }

    // Prepared-statement CRUD, the core of any driver's use.
    static void ddlAndCrud(Connection c) throws SQLException {
        try (Statement s = c.createStatement()) {
            s.execute("DROP TABLE IF EXISTS jdbc_drv");
            s.execute("CREATE TABLE jdbc_drv (id int PRIMARY KEY, name text, score numeric(6,2), active boolean)");
        }
        try (PreparedStatement ps = c.prepareStatement(
                "INSERT INTO jdbc_drv (id, name, score, active) VALUES (?, ?, ?, ?)")) {
            Object[][] rows = {
                {1, "ada", new java.math.BigDecimal("9.50"), true},
                {2, "bob", new java.math.BigDecimal("7.25"), false},
                {3, "cyd", null, true},
            };
            for (Object[] r : rows) {
                ps.setInt(1, (Integer) r[0]);
                ps.setString(2, (String) r[1]);
                if (r[2] == null) ps.setNull(3, Types.NUMERIC);
                else ps.setBigDecimal(3, (java.math.BigDecimal) r[2]);
                ps.setBoolean(4, (Boolean) r[3]);
                line("insert rows=" + ps.executeUpdate());
            }
        }
        try (PreparedStatement ps = c.prepareStatement(
                "SELECT id, name, score, active FROM jdbc_drv WHERE id <= ? ORDER BY id")) {
            ps.setInt(1, 2);
            try (ResultSet rs = ps.executeQuery()) {
                while (rs.next()) {
                    line("row " + rs.getInt("id") + "|" + rs.getString("name")
                        + "|" + rs.getBigDecimal("score") + "|" + rs.getBoolean("active"));
                }
            }
        }
        try (PreparedStatement ps = c.prepareStatement("UPDATE jdbc_drv SET score = ? WHERE id = ?")) {
            ps.setBigDecimal(1, new java.math.BigDecimal("10.00"));
            ps.setInt(2, 3);
            line("update rows=" + ps.executeUpdate());
        }
        try (PreparedStatement ps = c.prepareStatement("DELETE FROM jdbc_drv WHERE id = ?")) {
            ps.setInt(1, 2);
            line("delete rows=" + ps.executeUpdate());
        }
        try (Statement s = c.createStatement();
             ResultSet rs = s.executeQuery("SELECT count(*), sum(score) FROM jdbc_drv")) {
            rs.next();
            line("agg " + rs.getInt(1) + "|" + rs.getBigDecimal(2));
        }
    }

    // Explicit transaction control: a rolled-back change must not persist.
    static void transactions(Connection c) throws SQLException {
        c.setAutoCommit(false);
        try (Statement s = c.createStatement()) {
            s.executeUpdate("INSERT INTO jdbc_drv (id, name) VALUES (9, 'tmp')");
            c.rollback();
        }
        try (Statement s = c.createStatement();
             ResultSet rs = s.executeQuery("SELECT count(*) FROM jdbc_drv WHERE id = 9")) {
            rs.next();
            line("after rollback id=9 count=" + rs.getInt(1));
        }
        c.setAutoCommit(true);
    }

    static void typedValuesAndPortalPaging(Connection c) throws SQLException {
        try (Statement s = c.createStatement()) {
            s.execute("DROP TABLE IF EXISTS jdbc_types");
            s.execute("CREATE TABLE jdbc_types (id int PRIMARY KEY, label varchar(4), "
                + "amount numeric(7,2), ids integer[], note jsonb, span int4range, key uuid)");
        }
        PGobject json = new PGobject();
        json.setType("jsonb");
        json.setValue("{\"ready\": true}");
        PGobject range = new PGobject();
        range.setType("int4range");
        range.setValue("[1,5)");
        try (PreparedStatement ps = c.prepareStatement(
                "INSERT INTO jdbc_types VALUES (?, ?, ?, ?, ?, ?, ?)")) {
            ps.setInt(1, 1);
            ps.setString(2, "wide");
            ps.setBigDecimal(3, new java.math.BigDecimal("12.345"));
            ps.setArray(4, c.createArrayOf("integer", new Object[]{1, null, 3}));
            ps.setObject(5, json);
            ps.setObject(6, range);
            ps.setObject(7, UUID.fromString("00112233-4455-6677-8899-aabbccddeeff"));
            line("typed insert rows=" + ps.executeUpdate());
        }
        try (Statement s = c.createStatement();
             ResultSet rs = s.executeQuery(
                 "SELECT label, amount, ids::text, note::text, span::text, key::text FROM jdbc_types")) {
            rs.next();
            line("typed " + rs.getString(1) + "|" + rs.getBigDecimal(2) + "|"
                + rs.getString(3) + "|" + rs.getString(4) + "|" + rs.getString(5)
                + "|" + rs.getString(6));
        }
        try (Statement s = c.createStatement();
             ResultSet rs = s.executeQuery("SELECT label, amount, ids, note, span, key FROM jdbc_types")) {
            ResultSetMetaData md = rs.getMetaData();
            for (int column = 1; column <= md.getColumnCount(); column++) {
                line("typed-meta " + md.getColumnName(column) + "|" + md.getColumnTypeName(column)
                    + "|precision=" + md.getPrecision(column) + "|scale=" + md.getScale(column));
            }
        }

        c.setAutoCommit(false);
        try (PreparedStatement ps = c.prepareStatement("SELECT generate_series(1,3) AS value")) {
            ps.setFetchSize(1);
            try (ResultSet rs = ps.executeQuery()) {
                while (rs.next()) line("portal-page " + rs.getInt(1));
            }
        }
        c.rollback();
        c.setAutoCommit(true);
    }

    static void tableSampling(Connection c) throws SQLException {
        try (Statement s = c.createStatement()) {
            s.execute("CREATE TABLE jdbc_sample_source (id integer PRIMARY KEY)");
            s.execute("INSERT INTO jdbc_sample_source SELECT value FROM generate_series(1,20) value");
        }
        try (PreparedStatement ps = c.prepareStatement(
                "SELECT count(*) FROM jdbc_sample_source "
                + "TABLESAMPLE BERNOULLI (?) REPEATABLE (?)")) {
            ps.setDouble(1, 100.0);
            ps.setDouble(2, 42.0);
            try (ResultSet rs = ps.executeQuery()) {
                rs.next();
                line("sample rows=" + rs.getInt(1));
            }
        }
        try (PreparedStatement ps = c.prepareStatement(
                "SELECT count(*) FROM jdbc_sample_source "
                + "TABLESAMPLE SYSTEM (?) REPEATABLE (?)")) {
            ps.setDouble(1, 0.0);
            ps.setDouble(2, 42.0);
            try (ResultSet rs = ps.executeQuery()) {
                rs.next();
                line("system sample rows=" + rs.getInt(1));
            }
        }
    }

    // DatabaseMetaData — what ORMs/tools issue to introspect the schema.
    static void metadata(Connection c) throws SQLException {
        DatabaseMetaData md = c.getMetaData();
        try (ResultSet rs = md.getColumns(null, "public", "jdbc_drv", null)) {
            while (rs.next()) {
                line("col " + rs.getString("COLUMN_NAME") + "|" + rs.getString("TYPE_NAME")
                    + "|nullable=" + rs.getInt("NULLABLE"));
            }
        }
        try (ResultSet rs = md.getPrimaryKeys(null, "public", "jdbc_drv")) {
            while (rs.next()) {
                line("pk " + rs.getString("COLUMN_NAME") + " seq=" + rs.getShort("KEY_SEQ"));
            }
        }
    }

    static String sqlstate(SQLException e) {
        String s = e.getSQLState();
        return s == null ? "?????" : s;
    }

    static String firstLine(String m) {
        if (m == null) return "";
        int nl = m.indexOf('\n');
        return (nl < 0 ? m : m.substring(0, nl)).trim();
    }
}
