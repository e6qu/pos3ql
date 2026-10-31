"""Rename the displayed name and add a searchable email address."""
from alembic import op
import sqlalchemy as sa

revision = "002"
down_revision = "001"
branch_labels = None
depends_on = None


def upgrade():
    op.alter_column("accounts", "display_name", new_column_name="name",
                    existing_type=sa.String(80), existing_nullable=False,
                    schema="client_alembic")
    op.add_column("accounts", sa.Column("email", sa.String(160), nullable=True,
                                       server_default=sa.text("'unknown'")), schema="client_alembic")
    op.create_index("ix_accounts_email", "accounts", ["email"], schema="client_alembic")


def downgrade():
    op.drop_index("ix_accounts_email", table_name="accounts", schema="client_alembic")
    op.drop_column("accounts", "email", schema="client_alembic")
    op.alter_column("accounts", "name", new_column_name="display_name",
                    existing_type=sa.String(80), existing_nullable=False,
                    schema="client_alembic")
