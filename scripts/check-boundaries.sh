#!/usr/bin/env bash
# Fail if any module boundary is crossed: a cross-module path that does not go
# through a module's `api` facade, or a dependency edge the architecture does
# not allow (see docs/ARCHITECTURE.md).
#
# The check itself lives in tests/boundaries.rs so it compiles and runs with the
# rest of the test suite; this wrapper is what CI calls.
set -euo pipefail
cd "$(dirname "$0")/.."
exec cargo test --test boundaries
