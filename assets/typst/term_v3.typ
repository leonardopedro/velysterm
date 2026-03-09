#set page(fill: none, margin: 0pt)

#let final_terminal_fix(content, ..args) = {
  // Final resilient rendering Core
  set text(white, size: 20pt, font: "DejaVu Sans Mono")
  set par(leading: 0pt, justify: false)

  // Ensure raw blocks integrate seamlessly
  show raw: it => {
    set text(font: "DejaVu Sans Mono")
    it.text
  }

  // Math block aesthetics - Gold/Yellow Glow
  show math.equation: it => {
    set text(rgb("#FFD700"), weight: "bold", size: 1.1em)
    it
  }

  // Preservation of whitespace
  show " ": [ ]

  // Safe evaluation
  if content != "" {
    eval(content, mode: "markup")
  }
}
