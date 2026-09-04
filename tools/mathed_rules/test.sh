#!/usr/bin/env bash
# Golden tests for mathed_rules (T5). Builds the binary (skips with a
# clear message when the GHC env is unavailable) and runs each
# tests/golden_*.json fixture: stdin = `input`, stdout must equal
# `expected`.
set -euo pipefail
HERE="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"

if ! "$HERE/build.sh" 2>/dev/null; then
    echo ">> SKIP: mathed_rules golden tests (GHC env unavailable — dev-machine only)" >&2
    exit 0
fi

pass=0
fail=0
for fixture in "$HERE"/tests/golden_*.json; do
    name="$(grep -o '"name": *"[^"]*"' "$fixture" | head -1 | sed 's/.*": *"//; s/"$//')"
    input="$(grep -o '"input": *"[^"]*"' "$fixture" | head -1 | sed 's/.*": *"//; s/"$//')"
    expected="$(grep -o '"expected": *"[^"]*"' "$fixture" | head -1 | sed 's/.*": *"//; s/"$//')"
    got="$(printf '%s' "$input" | "$HERE/mathed_rules")"
    if [ "$got" = "$expected" ]; then
        echo ">> PASS: $name"
        pass=$((pass + 1))
    else
        echo ">> FAIL: $name" >&2
        echo "   expected: $expected" >&2
        echo "   got:      $got" >&2
        fail=$((fail + 1))
    fi
done

echo "============================================================"
echo " mathed_rules: $pass passed, $fail failed"
echo "============================================================"
[ "$fail" -eq 0 ]