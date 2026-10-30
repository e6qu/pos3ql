"""Real Python client workflows; compare the same transcript with PostgreSQL."""

import argparse
import datetime
from decimal import Decimal
import faulthandler
import io
from pathlib import Path
import sys
import uuid

import psycopg2
from psycopg2 import extensions, extras
import sqlalchemy as sa
from sqlalchemy.dialects import postgresql
from sqlalchemy.orm import DeclarativeBase, Session
from alembic import command
from alembic.autogenerate import compare_metadata
from alembic.config import Config
from alembic.migration import MigrationContext


def psycopg2_workflow(args):
    connection = psycopg2.connect(host=args.host, port=args.port, user=args.user,
                                 dbname=args.database, sslmode="disable")
    extras.register_uuid(conn_or_curs=connection)
    connection.autocommit = True
    try:
        with connection.cursor() as cursor:
            cursor.execute("CREATE SCHEMA client_psycopg2")
            cursor.execute("SET search_path=client_psycopg2,pg_catalog")
            cursor.execute("CREATE TABLE items(id integer PRIMARY KEY,name text UNIQUE,"
                           "amount numeric(12,2),day date,tags integer[],token uuid,payload jsonb)")
        connection.autocommit = False
        token = uuid.UUID("00000000-0000-0000-0000-000000000042")
        with connection.cursor() as cursor:
            extras.execute_values(cursor, "INSERT INTO items VALUES %s", [
                (1, "first", Decimal("12.50"), datetime.date(2026, 10, 31),
                 [1, None, 3], token, extras.Json({"ready": True})),
                (2, "second", None, None, [], None, extras.Json({})),
            ], page_size=1)
            cursor.execute("SELECT amount,day,tags,token,payload FROM items WHERE id=%s", (1,))
            assert cursor.fetchone() == (Decimal("12.50"), datetime.date(2026, 10, 31),
                                         [1, None, 3], token, {"ready": True})
            assert [column.type_code for column in cursor.description] == [1700, 1082, 1007, 2950, 3802]
            cursor.execute("SAVEPOINT duplicate_item")
            try:
                cursor.execute("INSERT INTO items(id,name) VALUES(%s,%s)", (1, "duplicate"))
            except psycopg2.IntegrityError as error:
                assert error.pgcode == "23505"
                assert connection.get_transaction_status() == extensions.TRANSACTION_STATUS_INERROR
            else:
                raise AssertionError("duplicate primary key was accepted")
            cursor.execute("ROLLBACK TO SAVEPOINT duplicate_item")
            cursor.execute("SELECT count(*) FROM items")
            assert cursor.fetchone() == (2,)
        connection.commit()
        print("psycopg2 parameters, types and savepoint recovery: ok")
        with connection.cursor() as cursor:
            cursor.copy_expert("COPY items(id,name) FROM STDIN WITH (FORMAT csv)",
                               io.StringIO('3,"third, quoted"\n4,fourth\n'))
            output = io.StringIO()
            cursor.copy_expert("COPY (SELECT id,name FROM items ORDER BY id) TO STDOUT WITH (FORMAT csv)", output)
            assert output.getvalue() == '1,first\n2,second\n3,"third, quoted"\n4,fourth\n'
        connection.commit()
        print("psycopg2 COPY input and output: ok")
        with connection.cursor(name="client_retained", scrollable=False, withhold=True) as cursor:
            cursor.itersize = 2
            cursor.execute("SELECT id,name FROM items ORDER BY id")
            assert cursor.fetchmany(2) == [(1, "first"), (2, "second")]
            with connection.cursor() as writer:
                writer.execute("UPDATE items SET name=%s WHERE id=%s", ("changed", 3))
            connection.commit()
            assert cursor.fetchmany(2) == [(3, "third, quoted"), (4, "fourth")]
            assert cursor.fetchone() is None
        connection.rollback()
        print("psycopg2 named cursor batches, snapshot and hold: ok")
        connection.autocommit = True
    finally:
        connection.rollback()
        connection.autocommit = True
        with connection.cursor() as cursor:
            cursor.execute("DROP SCHEMA IF EXISTS client_psycopg2 CASCADE")
        connection.close()


def engine_for(args, driver):
    return sa.create_engine(sa.URL.create(f"postgresql+{driver}", username=args.user,
                                         host=args.host, port=args.port, database=args.database),
                            connect_args={"sslmode": "disable"}, insertmanyvalues_page_size=2)


def sqlalchemy_workflow(args, driver):
    engine = engine_for(args, driver)

    class Base(DeclarativeBase):
        pass

    class Customer(Base):
        __tablename__ = "customers"
        __table_args__ = (sa.UniqueConstraint("name", name="uq_customers_name"),
                          {"schema": "client_sqlalchemy"})
        id = sa.Column(sa.Integer, sa.Identity(), primary_key=True)
        name = sa.Column(sa.String(80), nullable=False)
        balance = sa.Column(sa.Numeric(12, 2), nullable=False)
        tags = sa.Column(postgresql.ARRAY(sa.Text), nullable=True)
        settings = sa.Column(postgresql.JSONB, nullable=True)

    try:
        with engine.begin() as connection:
            connection.execute(sa.text("CREATE SCHEMA client_sqlalchemy"))
        Base.metadata.create_all(engine)
        with Session(engine, expire_on_commit=False) as session:
            with session.begin():
                customers = [Customer(name=name, balance=Decimal(amount), tags=[name, None],
                                      settings={"ready": True})
                             for name, amount in [("ada", "12.50"), ("bob", "7.25"), ("cyd", "0.00")]]
                session.add_all(customers)
            assert [customer.id for customer in customers] == [1, 2, 3]
            rows = session.execute(sa.select(Customer.name, Customer.balance, Customer.tags, Customer.settings)
                                   .where(Customer.id <= 2).order_by(Customer.id)).all()
            assert [tuple(row) for row in rows] == [
                ("ada", Decimal("12.50"), ["ada", None], {"ready": True}),
                ("bob", Decimal("7.25"), ["bob", None], {"ready": True}),
            ]
            try:
                with session.begin_nested():
                    session.add(Customer(name="ada", balance=Decimal("1.00")))
                    session.flush()
            except sa.exc.IntegrityError as error:
                state = error.orig.pgcode if driver == "psycopg2" else error.orig.sqlstate
                assert state == "23505"
            else:
                raise AssertionError("duplicate unique value was accepted")
            assert session.scalar(sa.select(sa.func.count()).select_from(Customer)) == 3
            session.execute(sa.update(Customer).where(Customer.name == "bob").values(balance=Decimal("8.00")))
            session.commit()
        inspector = sa.inspect(engine)
        assert inspector.get_table_names(schema="client_sqlalchemy") == ["customers"]
        columns = inspector.get_columns("customers", schema="client_sqlalchemy")
        assert [column["name"] for column in columns] == ["id", "name", "balance", "tags", "settings"]
        assert columns[0]["identity"]["always"] is False
        assert str(columns[1]["type"]) == "VARCHAR(80)"
        assert str(columns[2]["type"]) == "NUMERIC(12, 2)"
        assert inspector.get_pk_constraint("customers", schema="client_sqlalchemy")["constrained_columns"] == ["id"]
        assert [(item["name"], item["column_names"]) for item in
                inspector.get_unique_constraints("customers", schema="client_sqlalchemy")] == [("uq_customers_name", ["name"])]
        reflected = sa.Table("customers", sa.MetaData(), schema="client_sqlalchemy", autoload_with=engine)
        with engine.connect() as connection:
            result = connection.execution_options(stream_results=True, yield_per=2).execute(
                sa.select(reflected.c.id, reflected.c.balance).order_by(reflected.c.id))
            assert result.fetchmany(2) == [(1, Decimal("12.50")), (2, Decimal("8.00"))]
            assert result.fetchall() == [(3, Decimal("0.00"))]
            result.close()
        print(f"SQLAlchemy {driver} ORM, returning, savepoints, reflection and streaming: ok")
    finally:
        with engine.begin() as connection:
            connection.execute(sa.text("DROP SCHEMA IF EXISTS client_sqlalchemy CASCADE"))
        engine.dispose()


def alembic_metadata(version):
    metadata = sa.MetaData(schema="client_alembic")
    columns = [sa.Column("id", sa.Integer, primary_key=True),
               sa.Column("display_name" if version == 1 else "name", sa.String(80), nullable=False)]
    if version == 2:
        columns.append(sa.Column("email", sa.String(160), nullable=True, server_default=sa.text("'unknown'")))
    table = sa.Table("accounts", metadata, *columns,
                     sa.UniqueConstraint(columns[1].name, name="uq_accounts_name"))
    if version == 2:
        sa.Index("ix_accounts_email", table.c.email)
    return metadata


def alembic_workflow(args, driver):
    engine = engine_for(args, driver)
    schema = "client_alembic"
    config = Config()
    config.set_main_option("script_location", str(Path(__file__).with_name("alembic")))
    config.attributes["target_metadata"] = alembic_metadata(2)
    try:
        with engine.begin() as connection:
            connection.execute(sa.text(f"CREATE SCHEMA {schema}"))
        with engine.begin() as connection:
            config.attributes["connection"] = connection
            command.upgrade(config, "head")
            command.check(config)
            assert connection.scalar(sa.text(f"SELECT version_num FROM {schema}.alembic_version")) == "002"
            connection.execute(sa.text(f"INSERT INTO {schema}.accounts(id,name) VALUES(:id,:name)"),
                               {"id": 1, "name": "first"})
            assert connection.execute(sa.text(f"SELECT name,email FROM {schema}.accounts")).all() == [("first", "unknown")]
            changed = alembic_metadata(2)
            changed.tables[f"{schema}.accounts"].append_column(sa.Column("note", sa.Text))
            context = MigrationContext.configure(connection, opts={
                "include_schemas": True,
                "include_name": lambda name, kind, parents: name == schema if kind == "schema" else True,
                "compare_type": True, "compare_server_default": True,
                "version_table_schema": schema,
            })
            differences = compare_metadata(context, changed)
            assert len(differences) == 1 and differences[0][0] == "add_column", differences
            assert differences[0][3].name == "note"
            command.downgrade(config, "001")
            assert connection.execute(sa.text(f"SELECT display_name FROM {schema}.accounts")).all() == [("first",)]
            config.attributes["target_metadata"] = alembic_metadata(1)
            command.check(config)
            command.downgrade(config, "base")
            assert "accounts" not in sa.inspect(connection).get_table_names(schema=schema)
        print(f"Alembic {driver} upgrade, autogeneration, drift and downgrade: ok")
    finally:
        with engine.begin() as connection:
            connection.execute(sa.text(f"DROP SCHEMA IF EXISTS {schema} CASCADE"))
        engine.dispose()


def main():
    parser = argparse.ArgumentParser()
    parser.add_argument("--suite", choices=["psycopg2", "sqlalchemy", "alembic"], required=True)
    parser.add_argument("--host", required=True)
    parser.add_argument("--port", type=int, required=True)
    parser.add_argument("--user", required=True)
    parser.add_argument("--database", required=True)
    args = parser.parse_args()
    sys.stdout.reconfigure(line_buffering=True)
    faulthandler.dump_traceback_later(120, exit=True)
    try:
        if args.suite == "psycopg2":
            psycopg2_workflow(args)
        else:
            for driver in ("psycopg2", "psycopg"):
                if args.suite == "sqlalchemy":
                    sqlalchemy_workflow(args, driver)
                else:
                    alembic_workflow(args, driver)
    finally:
        faulthandler.cancel_dump_traceback_later()


if __name__ == "__main__":
    main()
