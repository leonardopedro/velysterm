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
//! Template rendering (T4):
//!   mathed_mini --render-typst <doc> [--ctx <ctx.json>] [--out <out.typ>]
//!     Renders the doc's \\template segments against the document
//!     context (overlaid with --ctx) and writes the resulting .typ
//!     to --out or stdout.
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
                // N5: `--with-outputs` renders each block's computed
                // output region beneath its content (the printable
                // notebook page).
                let with_outputs = rest.iter().any(|a| a == "--with-outputs");
                let out = if with_outputs {
                    mathed_mini::export::export_typst_with_outputs(initial).map_err(|e| {
                        std::io::Error::other(format!("with-outputs export failed: {e}"))
                    })?
                } else {
                    mathed_mini::export::export_typst(initial)
                };
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
            // N8: headless notebook record — run every block and
            // write (or print) the reproducible record JSON.
            "--run-all" => {
                let path = rest.first().ok_or("--run-all requires a file path")?;
                let mut grants: Vec<&str> = Vec::new();
                let mut out: Option<&str> = None;
                let mut i = 1;
                while i < rest.len() {
                    match rest[i].as_str() {
                        "--grants" => {
                            grants = rest
                                .get(i + 1)
                                .ok_or("--grants requires a comma-separated list")?
                                .split(',')
                                .collect();
                            i += 2;
                        }
                        "--out" => {
                            out = Some(rest.get(i + 1).ok_or("--out requires a path")?);
                            i += 2;
                        }
                        other => {
                            return Err(format!("unexpected --run-all argument `{other}`").into());
                        }
                    }
                }
                let doc = std::fs::read_to_string(path)?;
                let record = mathed_mini::export::run_all_record(&doc, &grants)?;
                if let Some(o) = out {
                    std::fs::write(o, &record)?;
                    eprintln!("Recorded run-all for {path} -> {o}");
                } else {
                    print!("{record}");
                }
                return Ok(());
            }
            // N10: one-way .ipynb projection — run every block (like
            // --run-all) and write the notebook with the live run
            // outputs; the doc stays the source of truth.
            "--export-ipynb" => {
                let path = rest
                    .first()
                    .ok_or("--export-ipynb requires a doc file path")?;
                let doc = std::fs::read_to_string(path)?;
                let mut grants: Vec<&str> = Vec::new();
                let mut out: Option<&str> = None;
                let mut i = 1;
                while i < rest.len() {
                    match rest[i].as_str() {
                        "--grants" => {
                            grants = rest
                                .get(i + 1)
                                .ok_or("--grants requires a comma-separated list")?
                                .split(',')
                                .collect();
                            i += 2;
                        }
                        "--out" => {
                            out = Some(rest.get(i + 1).ok_or("--out requires a path")?);
                            i += 2;
                        }
                        other => {
                            return Err(
                                format!("unexpected --export-ipynb argument: {other}").into()
                            );
                        }
                    }
                }
                let runs = mathed_mini::export::run_all_runs(&doc, &grants);
                let nb = mathed_mini::export::export_ipynb_with_runs(&doc, &runs)
                    .map_err(|e| std::io::Error::other(format!("ipynb export failed: {e}")))?;
                if let Some(o) = out {
                    std::fs::write(o, &nb)?;
                    eprintln!("Projected {path} -> {o} (ipynb, one-way)");
                } else {
                    print!("{nb}");
                }
                return Ok(());
            }
            // N8: open-doc staleness check against a record.
            "--check-record" => {
                let doc_path = rest.first().ok_or("--check-record requires a doc path")?;
                let rec_path = rest.get(1).ok_or("--check-record requires a record path")?;
                let doc = std::fs::read_to_string(doc_path)?;
                let rec = std::fs::read_to_string(rec_path)?;
                let stale = mathed_mini::export::record_stale_blocks(&doc, &rec)?;
                if stale.is_empty() {
                    eprintln!("record is current for {doc_path}");
                } else {
                    eprintln!("stale blocks for {doc_path}: {stale:?}");
                }
                return Ok(());
            }
            "--export-md" => {
                let path = rest.first().ok_or("--export-md requires a file path")?;
                let out = mathed_mini::export::export_markdown(initial);
                std::fs::write(path, out)?;
                eprintln!("Exported Markdown to {path}");
                return Ok(());
            }
            "--export-ascii" => {
                let path = rest.first().ok_or("--export-ascii requires a file path")?;
                // U7: optional `--mappings <json>` — per-document
                // `glyph → ascii form` overrides for the export
                // (glyph keys must be single chars).
                let mut mappings = std::collections::HashMap::new();
                if let Some(i) = rest.iter().position(|a| a == "--mappings") {
                    let json = rest.get(i + 1).ok_or("--mappings requires a JSON object")?;
                    let v: serde_json::Value = serde_json::from_str(json)
                        .map_err(|e| format!("--mappings is not valid JSON: {e}"))?;
                    let serde_json::Value::Object(map) = v else {
                        return Err("--mappings must be a JSON object".into());
                    };
                    for (k, val) in map {
                        let mut cs = k.chars();
                        let g = cs
                            .next()
                            .filter(|_| cs.next().is_none())
                            .ok_or(format!("--mappings key `{k}` must be a single char"))?;
                        let ascii_form = val
                            .as_str()
                            .ok_or(format!("--mappings value for `{k}` must be a string"))?;
                        mappings.insert(g, ascii_form.to_string());
                    }
                }
                let out = mathed_mini::export::export_ascii_with_mappings(initial, &mappings);
                std::fs::write(path, out)?;
                eprintln!("Exported ASCII-only Typst to {path}");
                return Ok(());
            }
            "--render-typst" => {
                let path = rest
                    .first()
                    .ok_or("--render-typst requires a doc file path")?;
                let doc = std::fs::read_to_string(path)?;
                let mut ctx_json: Option<String> = None;
                let mut out_path: Option<String> = None;
                let mut i = 1;
                while i < rest.len() {
                    match rest[i].as_str() {
                        "--ctx" => {
                            i += 1;
                            let p = rest.get(i).ok_or("--ctx requires a JSON file path")?;
                            ctx_json = Some(std::fs::read_to_string(p)?);
                        }
                        "--out" => {
                            i += 1;
                            let p = rest.get(i).ok_or("--out requires an output file path")?;
                            out_path = Some(p.clone());
                        }
                        other => {
                            return Err(
                                format!("unexpected --render-typst argument: {other}").into()
                            );
                        }
                    }
                    i += 1;
                }
                let out = mathed_mini::export::export_typst_template(&doc, ctx_json.as_deref())
                    .map_err(std::io::Error::other)?;
                match out_path {
                    Some(p) => {
                        std::fs::write(&p, &out)?;
                        eprintln!("Rendered Typst to {p}");
                    }
                    None => {
                        print!("{out}");
                    }
                }
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
