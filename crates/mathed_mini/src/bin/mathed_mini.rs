//! Minimal math editor window: `cargo run -p mathed_mini`.
//!
//! Type to append text; Backspace deletes; Enter/Space insert; Esc quits.
//! Markup is Typst-flavored, e.g. `$ E = m c^2 $`.
//!
//! The seed document demonstrates the probability kernel (P3 #10/#11): a
//! `\model` (built-in translator → a mode-0 number operator, vacuum prior), a
//! `\translator` panel defining an event mapping, and a `\prob` whose value is
//! computed by the kernel and shown in the results panel below the document.

fn main() -> Result<(), Box<dyn std::error::Error>> {
    let initial = "= mathed kernel demo\n\n\
         #1 a #2 \\model(#1,#2)\n\n\
         #5 #let translate(body) = { \"{\\\"kind\\\":\\\"vacuum\\\"}\" } \
         #6 \\translator(#5,#6, name: \"ev\")\n\n\
         #3 vacuum #4 \\prob(#3,#4, translator: \"ev\")\n";
    mathed_mini::app::run(initial)
}
