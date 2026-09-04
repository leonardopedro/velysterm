#!/usr/bin/env bash
# Build the authoring-time egison rules binary (T5). Dev-machine only:
# requires the GHC 9.10.3 + sweet-egison env from the unfer flake.
# The env is either cached in the store (the fock_match GHC_ENV path)
# or obtained with `nix develop ../unfer --command ./tools/mathed_rules/build.sh`.
set -euo pipefail
HERE="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"

if [ -n "${GHC_ENV:-}" ]; then
    :
elif [ -d /nix/store/1ir89h874mwag82kkryrrp52f10sc7y9-ghc-9.10.3-with-packages ]; then
    GHC_ENV=/nix/store/1ir89h874mwag82kkryrrp52f10sc7y9-ghc-9.10.3-with-packages
else
    echo ">> GHC env not cached on this machine; run:" >&2
    echo "   nix develop ../unfer --command ./tools/mathed_rules/build.sh" >&2
    exit 1
fi

echo ">> Compiling mathed_rules (sweet-egison TH quasiquoters)"
export PATH="$GHC_ENV/bin:$PATH"
ghc -O2 -o "$HERE/mathed_rules" "$HERE/haskell/MathedRules.hs"
echo ">> built $HERE/mathed_rules"