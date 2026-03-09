#set page(fill: none, margin: 0pt)

#let render_terminal(content) = {
  // Use the exact same font file as Bevy
  set text(white, size: 20pt, font: "DejaVu Sans Mono")

  // Explicitly set paragraph leading to 0 to match Bevy's vertical behavior
  set par(leading: 0pt)

  // Style for math
  show math.equation: it => {
    set text(yellow, weight: "bold")
    it
  }

  // Preserve spaces and newlines
  show " ": [ ]

  // Terminal content should be parsed as markup to support math $...$
  // We'll escape everything except what looks like math blocks if needed,
  // but for now let's try raw eval.

  eval(content, mode: "markup")

  [#metadata("cursor") <cursor>]
}
