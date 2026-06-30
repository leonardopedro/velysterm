#set page(fill: none, margin: 0pt)

#let render_editor(before, active, after) = {
  // Use the exact same font file as Bevy
  set text(white, size: 20pt, font: "DejaVu Sans Mono")

  // Explicitly set paragraph leading to 0 to match Bevy's vertical behavior
  set par(leading: 0pt)

  // Style for math
  show math.equation: it => {
    set text(yellow, weight: "bold")
    it
  }

  // Preserve spaces
  show " ": [ ]

  let render_part(s) = {
    let math_count = s.clusters().filter(c => c == "$").len()
    if math_count > 0 and calc.rem(math_count, 2) == 0 {
      eval(s, mode: "markup")
    } else {
      s
    }
  }

  render_part(before)
  if active != "" {
    set text(fill: white.transparentize(100%))
    render_part(active)
  }
  render_part(after)

  [#metadata("cursor") <cursor>]
}
