#!/usr/bin/env bash
# The front door. A person types this.
#
# It holds no logic. Every verb goes to a Python module under tools/, which is
# the same code the Reactorcide jobs call, so a local run and a CI run cannot
# drift. See docs/CI-CD.md and D55.
#
# If you are about to add an `if` to this file, add it to the Python instead.

set -euo pipefail

ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"

if ! command -v uv >/dev/null 2>&1; then
    echo "tools.sh needs uv, and it is not on the path." >&2
    echo "Install it from https://docs.astral.sh/uv/ and run this again." >&2
    exit 1
fi

exec uv run --quiet --project "$ROOT/tools" python -m tallyowl_tools "$@"
