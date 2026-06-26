// Built-in default event translator (P3 #10 follow-up).
//
// Used when a `\event`/`\prob` segment names no translator. It ignores the
// math `body` and emits the `Vacuum` predicate — the simplest valid
// EventPredicate — so a document with no event translator still produces a
// well-formed kernel request. Real documents are expected to define an
// event-specific `\translator` segment that maps their notation to a
// predicate over Fock-space occupation numbers.
//
// Output contract: a JSON `EventPredicate` string, i.e. an object with a
// `kind` tag. See `unfer_protocol::EventPredicate`.

#let translate(body) = {
  json.encode((kind: "vacuum"))
}
