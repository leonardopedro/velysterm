// Built-in template helpers (T7, PLAN_mathed_full_vision.md).
//
// Prepended to every \template / \base body before evaluation — the
// filters role of a template language (Jinja filters / XSLT
// functions) without a second language: ordinary Typst `#let`
// bindings at module scope, callable from render(ctx). User code may
// shadow any of them (later bindings win).

// Join an array of strings with a separator.
#let join(items, sep: ", ") = items.join(sep)

// One table row from an array of cell values.
#let table_row(values) = table.row(..values)

// A value rounded to `digits` decimals (shortest repr).
#let sigfig(x, digits: 3) = {
  if type(x) in (int, float) {
    let scale = calc.pow(10, digits)
    repr((x * scale).round() / scale)
  } else {
    str(x)
  }
}

// A probability as a percentage with one decimal.
#let fmt_p(x) = if type(x) in (int, float) {
  repr((x * 1000.0).round() / 10.0) + "%"
} else {
  str(x)
}

// A link to a heading whose text is the heading name.
#let heading_ref(name) = link("#" + name)[#name]