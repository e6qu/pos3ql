#!/usr/bin/env bash
# Keep the production object-store boundary on one direct S3-compatible
# protocol. Test infrastructure may start different implementations, but the
# shipped configuration and client must never select endpoint-specific code or
# resurrect the deleted translation service.

set -u
cd "$(dirname "$0")/.." || exit

fail=0

check_absent() {
  local description=$1
  local pattern=$2
  shift 2
  local hits
  hits=$(rg -n -i "$pattern" "$@" 2>/dev/null || true)
  if [[ -n "$hits" ]]; then
    printf 'FAIL: %s\n%s\n' "$description" "$hits"
    fail=1
  fi
}

check_absent 'deleted transport vocabulary returned' \
  'object_store_(namespace|token)|/v1/objects|authorization:[[:space:]]*bearer' \
  src examples Cargo.toml
check_absent 'a vendor SDK or endpoint-specific implementation entered production' \
  'aws-sdk|google-cloud|gcp[_-]|azure[_-]|minio[_-]|object_store_provider|provider[_-](kind|name)' \
  src Cargo.toml Cargo.lock
check_absent 'the object client contains a provider-selected branch' \
  '(if|match|cfg).*\b(aws|google|gcp|azure|minio)\b' \
  src/config.rs src/object_store.rs src/object_store src/store
check_absent 'provider vocabulary entered the production storage boundary' \
  '\b(google|gcp|azure|minio|seaweedfs)\b' \
  src/config.rs src/object_store.rs src/object_store src/store
check_absent 'the deleted translation service returned' \
  'gateway|translation (proxy|service)|custom (wire|transport|protocol)' \
  src/object_store.rs src/object_store examples

if (( fail != 0 )); then
  exit 1
fi
printf '%s\n' 'object-store boundary: one direct S3-compatible protocol (OK)'
