// Built-in default translator (P3 #10).
//
// Used when a `\model`/`\event`/`\prob` segment names no translator. It is a
// placeholder: it ignores the math `body` and emits a single mode-0 number
// operator a^dagger_0 a_0 — a valid, non-empty Hamiltonian so a document with
// no translator still produces a real model out of the box. Real documents are
// expected to define a model-specific `\translator` segment that maps their
// notation to operator strings.
//
// Output contract: a JSON `TermSpec[]` string, i.e. an array of
// { coeff_re, coeff_im, ops: [{ kind, level, mode }] }.

#let translate(body) = {
  let ops = (
    (kind: "create", level: "inner_boson", mode: 0),
    (kind: "annihilate", level: "inner_boson", mode: 0),
  )
  let term = (coeff_re: 1.0, coeff_im: 0.0, ops: ops)
  json.encode((term,))
}
