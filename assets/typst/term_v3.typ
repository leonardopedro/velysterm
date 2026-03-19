#set page(fill: none, margin: 0pt)

#let final_terminal_fix(content, ..args) = {
  // --- Premium Terminal Design ---
  
  // High-fidelity typography
  set text(
    fill: rgb("#e0e0e0"), 
    size: 16pt, 
    font: "DejaVu Sans Mono",
    tracking: 0.5pt
  )
  
  // Paragraph styling for 1D flow
  set par(
    leading: 8pt, 
    justify: false,
    linebreaks: "optimized"
  )

  // Custom styling for raw shell text
  show raw: it => {
    set text(font: "DejaVu Sans Mono")
    it.text
  }

  // Math block - Vibrant Gold with slight glow effect simulated by weight
  show math.equation: it => {
    set text(rgb("#FFD700"), weight: "bold", size: 1.1em)
    h(2pt)
    it
    h(2pt)
  }

  // Handle markers (solidified IDs)
  // Markers like #Abe, #Ace are rendered as beautiful badges
  show regex("#[a-zA-Z]+"): it => {
    box(
      fill: rgb("#3a3a4a"),
      stroke: 1pt + rgb("#5a5a6a"),
      radius: 4pt,
      outset: (y: 3pt, x: 2pt),
      text(fill: rgb("#00d4ff"), size: 0.8em, weight: "bold", it)
    )
  }

  // Handle active marker chains like #(b,a)
  show regex("#\(.*?\)"): it => {
    box(
      fill: rgb(0, 212, 255, 10%),
      stroke: (bottom: 2pt + rgb("#00d4ff")),
      radius: 2pt,
      text(fill: rgb("#00d4ff"), it)
    )
  }

  // Whitespace preservation with non-breaking spaces for alignment
  show " ": [ ]

  // Premium Container with Title Bar and Glassmorphism-lite
  stack(
    dir: ttb,
    block(
      fill: rgb("#252535"),
      width: 100%,
      inset: (x: 12pt, y: 8pt),
      radius: (top: 10pt),
      text(fill: rgb("#8888aa"), weight: "bold", size: 11pt, "VELYST SPECIALIZED SHELL")
    ),
    block(
      width: 100%,
      fill: rgb(20, 20, 30, 70%),
      stroke: (
        left: 2pt + rgb("#00d4ff"), 
        right: 1pt + rgb("#252535"), 
        bottom: 1pt + rgb("#252535")
      ),
      inset: (left: 15pt, right: 15pt, top: 15pt, bottom: 30pt), // Extra bottom room for cursor
      radius: (bottom: 10pt),
      if content != "" {
        eval(content, mode: "markup")
      } else {
        [_Initializing specialized shell session..._]
      }
    )
  )
}
