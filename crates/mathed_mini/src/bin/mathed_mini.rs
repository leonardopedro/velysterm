//! Minimal math editor window: `cargo run -p mathed_mini`.
//!
//! Type to append text; Backspace deletes; Enter/Space insert; Esc
//! quits. Markup is Typst-flavored, e.g. `$ E = m c^2 $`.
//!
//! Export modes (C16):
//!   mathed_mini --export-typst <file>   Export to standalone .typ
//!   mathed_mini --export-json <file>    Export SemanticIndex as JSON
//!   mathed_mini --export-md <file>      Export as plain Markdown
//!
//! Durable dashboard (TUI):
//!   mathed_mini --dashboard             Open the durable-store
//! status                                       dashboard (Typst
//! document, citable                                       sections:
//! Ctrl+1..3 to expand)   mathed_mini --dashboard-typst <f>
//! Headless: consult the store and                                   
//! write the dashboard document

fn main() -> Result<(), Box<dyn std::error::Error>> {
    let args: Vec<String> = std::env::args().collect();

    let initial = "= mathed kernel demo\n\n\
         #1 a #2 \\model(#1,#2)\n\n\
         #5 #let translate(body) = { \"{\\\"kind\\\":\\\"vacuum\\\"}\" } \
         #6 \\translator(#5,#6, name: \"ev\")\n\n\
         #3 vacuum #4 \\prob(#3,#4, translator: \"ev\")\n";

    if let Some(flag) = args.get(1) {
        let rest = &args[2..];
        match flag.as_str() {
            "--dashboard-typst" => {
                let path = rest
                    .first()
                    .ok_or("--dashboard-typst requires a file path")?;
                let out = mathed_mini::durable_dashboard::dashboard_document();
                std::fs::write(path, out)?;
                eprintln!("Exported durable dashboard Typst to {path}");
                return Ok(());
            }
            "--dashboard" => {
                // Open the editor on the durable-status dashboard
                // document.
                return mathed_mini::app::run(&mathed_mini::durable_dashboard::dashboard_document());
            }
            "--export-typst" => {
                let path = rest.first().ok_or("--export-typst requires a file path")?;
                let out = mathed_mini::export::export_typst(initial);
                std::fs::write(path, out)?;
                eprintln!("Exported Typst to {path}");
                return Ok(());
            }
            "--export-json" => {
                let path = rest.first().ok_or("--export-json requires a file path")?;
                let out = mathed_mini::export::export_json(initial);
                std::fs::write(path, out)?;
                eprintln!("Exported JSON to {path}");
                return Ok(());
            }
            "--export-md" => {
                let path = rest.first().ok_or("--export-md requires a file path")?;
                let out = mathed_mini::export::export_markdown(initial);
                std::fs::write(path, out)?;
                eprintln!("Exported Markdown to {path}");
                return Ok(());
            }
            _ => {
                eprintln!("Unknown option: {}", args[1]);
                return Ok(());
            }
        }
    }

    mathed_mini::app::run(initial)
}
