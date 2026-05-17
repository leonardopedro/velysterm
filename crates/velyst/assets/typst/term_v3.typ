#set page(fill: none, margin: 0pt)

#let final_terminal_fix(content, focused_btn: "") = {
  // Dark theme typography
  set text(fill: rgb("#ffffff"), size: 20pt, font: "DejaVu Sans Mono")
  set par(leading: 8pt, justify: false)

  // High-contrast button component
  let btn(id, label) = {
    let is_focused = id == focused_btn
    
    // Focus inversion for visibility
    let bg = if is_focused { rgb("#ffffff") } else { rgb("#000000") }
    let fg = if is_focused { rgb("#000000") } else { rgb("#ffffff") }
    let stroke_color = if is_focused { rgb("#00d4ff") } else { rgb("#ffffff") }
    
    link("btn:" + id)[
      #box(fill: bg, stroke: 2pt + stroke_color, inset: 6pt, radius: 2pt)[
        #text(fill: fg, weight: "bold", label)
      ]
    ]
  }

  // Custom styling for raw shell text
  show raw: it => {
    set text(font: "DejaVu Sans Mono")
    it.text
  }

  // Handle active marker chains like #(b,a) - high contrast cyan for visibility in dark mode
  show regex("#\(.*?\)"): it => {
    box(
      stroke: (bottom: 2pt + rgb("#00d4ff")),
      text(fill: rgb("#00d4ff"), it)
    )
  }

  // Whitespace preservation
  show " ": [ ]

  if content != "" {
    eval(content, mode: "markup", scope: (btn: btn))
  } else {
    [#text(fill: rgb("#8888aa"))[_Initializing specialized shell session..._]]
  }
}
