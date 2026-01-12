# Velyist Typst Editor

This project implements a simple WYSIWYG (What You See Is What You Get) editor for Typst using the Velyst library. It demonstrates a Mosh-like latency hiding technique to provide a responsive user experience.

The core idea is to have two rendering layers:
1.  A **"fast" prediction layer** that immediately displays raw text input as the user types.
2.  A **"slow" Typst rendering layer** that provides accurate, fully formatted output.

This approach solves the challenge of providing immediate feedback in an editor where the final rendering can have some latency.

## The "Cursor Anchor" Strategy

To build a WYSIWYG Typst Editor with Mosh-like latency hiding, we can't just append text. Users need to be able to edit text anywhere in the document. The "Cursor Anchor" strategy solves this:

1.  **The Anchor (Slow):** We inject an invisible marker (`<cursor>`) into the Typst source code at the user's caret position. When Typst compiles, it calculates the exact X/Y coordinates of that marker.
2.  **The Prediction (Fast):** We render a transparent Bevy UI container at the last known X/Y coordinate of the cursor. As the user types, we immediately fill this container with raw characters.
3.  **The Snap (Reconciliation):** When the slower, full Typst compilation finishes, the new, correctly formatted text appears, and we clear the prediction buffer.

## Implementation

The implementation of this editor is split between a Typst file for rendering and a Rust file for the application logic.

### Typst-side Implementation

The Typst logic for anchoring the cursor and rendering the text can be found in:
- `assets/typst/editor.typ`

### Bevy-side (Rust) Implementation

The Bevy application, which includes state management, fast prediction rendering, and the slow synchronization with the Velyst Typst renderer, is implemented in:
- `examples/editor.rs`

## Compiling and Running the Editor

To ensure a consistent and reproducible development environment, this project uses Nix.

### Using Nix (Recommended)

1.  **Install Nix:** If you don't have Nix, follow the instructions at [https://nixos.org/download.html](https://nixos.org/download.html).
2.  **Enter the Development Shell:** Open your terminal at the project's root and run:
    ```sh
    nix-shell
    ```
    This command will automatically download and configure all the necessary dependencies defined in `dev.nix`.
3.  **Compile and Run:** Once inside the Nix shell, you can compile and run the editor:
    ```sh
    cargo run --release --example editor
    ```

### Manual Setup (Without Nix)

If you prefer not to use Nix, you will need to manually install the required system libraries for your operating system. The dependencies for Linux and macOS are listed in the `dev.nix` file. After installing them, you can run the editor using the same `cargo` command as above.

## Visual Polish

To make it feel like a modern editor, some potential improvements include:

1.  **Text Matching:** Ensure the Bevy UI font for the prediction layer matches the Typst font.
2.  **Ghosting:** Make the "Prediction Layer" text slightly grey or underlined until the "Real" Typst render arrives and replaces it.
3.  **Drift Correction:** If the user types a lot, the "Fast" text might drift from where Typst would actually place it (e.g., due to kerning or line wrapping). A "Hard Sync" could be forced to clear the pending input whenever a new frame arrives from Velyst.
