"""Create the application's initial account table."""
from alembic import op
import sqlalchemy as sa

revision = "001"
down_revision = None
branch_labels = None
depends_on = None


def upgrade():
    op.create_table("accounts", sa.Column("id", sa.Integer, primary_key=True),
                    sa.Column("display_name", sa.String(80), nullable=False),
                    sa.UniqueConstraint("display_name", name="uq_accounts_name"),
                    schema="client_alembic")


def downgrade():
    op.drop_table("accounts", schema="client_alembic")
