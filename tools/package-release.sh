#!/usr/bin/env bash
set -euo pipefail

ROOT=$(cd "$(dirname "$0")/.." && pwd)
OUTPUT=${1:-"$ROOT/dist"}
BINARY=${POS3QL_RELEASE_BINARY:-"$ROOT/target/release/pos3ql"}
TARGET=${POS3QL_RELEASE_TARGET:-x86_64-unknown-linux-gnu}
VERSION=$(sed -n 's/^version = "\([^"]*\)"/\1/p' "$ROOT/Cargo.toml" | head -n 1)

[[ -n "$VERSION" ]] || { echo "Cargo package version is missing" >&2; exit 1; }
[[ -x "$BINARY" ]] || { echo "release binary is missing: $BINARY" >&2; exit 1; }

NAME="pos3ql-v${VERSION}-${TARGET}"
STAGE=$(mktemp -d "${TMPDIR:-/tmp}/pos3ql-package.XXXXXX")
trap 'rm -rf "$STAGE"' EXIT
mkdir -p "$OUTPUT" "$STAGE/$NAME/bin" "$STAGE/$NAME/etc/pos3ql" \
  "$STAGE/$NAME/lib/systemd/system" "$STAGE/$NAME/libexec/pos3ql" \
  "$STAGE/$NAME/share/doc/pos3ql"

install -m 0755 "$BINARY" "$STAGE/$NAME/bin/pos3ql"
install -m 0755 "$ROOT/packaging/pos3ql-failover-monitor" \
  "$STAGE/$NAME/libexec/pos3ql/failover-monitor"
install -m 0644 "$ROOT/packaging/pos3ql.conf" "$STAGE/$NAME/etc/pos3ql/pos3ql.conf"
install -m 0644 "$ROOT/packaging/pos3ql-failover.conf" \
  "$STAGE/$NAME/etc/pos3ql/pos3ql-failover.conf.example"
install -m 0644 "$ROOT/packaging/pos3ql.service" \
  "$STAGE/$NAME/lib/systemd/system/pos3ql.service"
install -m 0644 "$ROOT/packaging/pos3ql-failover.service" \
  "$STAGE/$NAME/lib/systemd/system/pos3ql-failover.service"
install -m 0644 "$ROOT/packaging/README.md" "$STAGE/$NAME/README.md"
install -m 0644 "$ROOT/LICENSE" "$ROOT/README.md" "$ROOT/PLAN.md" \
  "$ROOT/docs/operations.md" "$ROOT/docs/object-storage.md" \
  "$ROOT/docs/backup-restore.md" "$STAGE/$NAME/share/doc/pos3ql/"

ARCHIVE="$OUTPUT/$NAME.tar.gz"
if [[ $(tar --version) == *"GNU tar"* ]]; then
  tar --sort=name --mtime='UTC 1970-01-01' --owner=0 --group=0 --numeric-owner \
    -C "$STAGE" -cf - "$NAME" | gzip -n > "$ARCHIVE"
else
  # libarchive/bsdtar has no --sort or --mtime override. Normalize the staged
  # tree first and feed it a stable member list.
  find "$STAGE/$NAME" -exec touch -t 198001010000 {} +
  (
    cd "$STAGE"
    find "$NAME" -print | LC_ALL=C sort > member-list
    tar --uid 0 --gid 0 --uname root --gname root \
      -cf - -T member-list | gzip -n > "$ARCHIVE"
  )
fi
(
  cd "$OUTPUT"
  sha256sum "$(basename "$ARCHIVE")" > "$(basename "$ARCHIVE").sha256"
)
printf '%s\n' "$ARCHIVE"
