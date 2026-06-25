//! Minimal math editor window: `cargo run -p mathed_mini`.
//!
//! Type to append text; Backspace deletes; Enter/Space insert; Esc quits.
//! Markup is Typst-flavored, e.g. `$ E = m c^2 $`.

fn main() -> Result<(), Box<dyn std::error::Error>> {
    let initial = "= mathed (minimal)\n\nType Typst markup, e.g. \
                   $ E = m c^2 $\n\n";
    mathed_mini::app::run(initial)
}
