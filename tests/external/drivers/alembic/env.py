"""Use the test's actual SQLAlchemy connection and schema comparison settings."""
from alembic import context

schema = "client_alembic"
context.configure(
    connection=context.config.attributes["connection"],
    target_metadata=context.config.attributes["target_metadata"],
    version_table_schema=schema,
    include_schemas=True,
    include_name=lambda name, kind, parents: name == schema if kind == "schema" else True,
    compare_type=True,
    compare_server_default=True,
)
with context.begin_transaction():
    context.run_migrations()
