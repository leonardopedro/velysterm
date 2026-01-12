#let cursor_anchor = [#metadata("cursor") <cursor>]

#let render_editor(text_before, text_after) = {
  [#text_before]
  cursor_anchor
  [#text_after]
}
