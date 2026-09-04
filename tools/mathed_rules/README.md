# mathed_rules — authoring-time Egison pattern engine (T5)

A small Haskell binary that gives `--render-typst` templates the
XSLT-style pattern-matching role, using the **Egison Template-Haskell
matchers already staged in the ecosystem** (australVM `fock_match`
precedent, GHC 9.10.3 + sweet-egison env from the unfer flake) —
nothing new invented, per the "improve, don't build new" constraint.

**It is a dev-machine convenience, never required.** `--render-typst`
degrades to the identity path when the binary is absent (the Rust side
checks `MATHED_RULES_BIN`; unset or failing → template bodies run
unchanged). No Haskell ever runs in the editor or on the keystroke
path.

## Wire contract

stdin: `{"op": "<op>", "body": "<body>"}` (JSON)
stdout: `{"markup": "<markup>"}` (JSON)

| op | body | markup |
|---|---|---|
| `rewrite` | comma-separated token list (`a†, a, b`; `†` = adjoint) | tokens after contracting every adjacent same-name `x†, x` pair to `⟨x⟩`, joined by spaces |
| `select` | `name:value;name:value;…` (Rust pre-slices `DocumentContext.statements`) | names whose value equals `self(<name>)` (Eql-bound), joined by `;` |

> As-built deviation from the plan's `{ctx, body}` sketch: the full
> `DocumentContext` is not shipped to the binary in v1. The Rust side
> pre-slices the statements into the `select` body, so the Haskell
> side needs no JSON object parser beyond the minimal field extractor
> below. The two contract forms (serde JSON for the out-of-band
> binary vs the lowered Typst dict literal inside `render(ctx)`)
> coexist without coupling.

## Build & test (dev machine)

```bash
# With the GHC env cached (the fock_match store path):
./tools/mathed_rules/build.sh
./tools/mathed_rules/test.sh

# Or through the unfer flake (read-only use — the cross-repo rule):
nix develop ../unfer --command ./tools/mathed_rules/build.sh
nix develop ../unfer --command ./tools/mathed_rules/test.sh
```

`test.sh` runs every `tests/golden_*.json` fixture (stdin → stdout
byte comparison) and skips with a clear message when the GHC env is
unavailable — the golden data is committed either way.

## Wiring into `--render-typst`

`mathed_mini::export::apply_mathed_rules` (env-gated on
`MATHED_RULES_BIN`) rewrites each template body with `op = rewrite`
before evaluation when the binary is present; absent or failing, the
body runs through the identity path (existing byte-identical export
tests pin this).