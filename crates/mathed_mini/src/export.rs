//! Export and interchange (C16).
//!
//! Three export modes:
//! - Typst: standalone `.typ` file with annotations baked in
//! - JSON: `SemanticIndex` as structured JSON
//! - Markdown: plain markdown with math blocks

use mathed_core::markers::{resolve_segments, scan};
use mathed_core::semantics::SemanticIndex;
use mathed_core::transform::{TransformOptions, to_render_text};

pub fn export_typst(doc_text: &str) -> String {
    let scan = scan(doc_text);
    let segments = resolve_segments(&scan);
    let render = to_render_text(doc_text, &scan, &segments, &TransformOptions::default());
    format!("// Exported from mathed_mini\n{}", render.text)
}

/// ASCII interchange projection (U-series U4): render the document's
/// Typst markup, then map every non-ASCII code point to ASCII:
/// inside math, glyphs with a `\name` form in the U2 completion
/// table become that backslash-name (`α` → `\alpha`, valid Typst
/// math); everything else becomes an explicit `\u{HEX}` literal
/// (valid in markup and math — never a silent drop, never a mangled
/// glyph). The source (ASCII syntax + Unicode glyphs) stays
/// canonical; this export is a lossy-by-design but always-total
/// projection for ASCII-only pipelines.
pub fn export_ascii(doc_text: &str) -> String {
    let scan = scan(doc_text);
    let segments = resolve_segments(&scan);
    let render = to_render_text(doc_text, &scan, &segments, &TransformOptions::default());

    let mut out = String::with_capacity(render.text.len());
    let mut in_math = false;
    let mut escaped = false;
    for c in render.text.chars() {
        if escaped {
            // The char escaped by `\` is emitted verbatim — an
            // escaped `$` must not toggle math.
            out.push(c);
            escaped = false;
            continue;
        }
        if c == '\\' {
            out.push(c);
            escaped = true;
            continue;
        }
        if c == '$' {
            in_math = !in_math;
            out.push(c);
            continue;
        }
        if c.is_ascii() {
            out.push(c);
            continue;
        }
        if in_math && let Some(name) = ascii_math_name(c) {
            out.push_str(name);
        } else {
            // Explicit, round-trippable flag — never a silent drop.
            out.push_str(&format!("\\u{{{:X}}}", c as u32));
        }
    }
    out
}

/// Inverted U2 table: glyph → ASCII backslash-name, restricted to
/// entries whose ASCII form is a valid Typst math escape (starts
/// with `\`). Operator forms (`->` for `→`) are NOT valid Typst
/// math, so those glyphs fall back to `\u{...}`. A glyph with
/// several names maps to the longest (the table is currently
/// injective, so this is the single entry).
fn ascii_math_name(glyph: char) -> Option<&'static str> {
    mathed_core::completion::COMPLETIONS
        .iter()
        .filter(|e| e.ascii.starts_with('\\') && e.glyph.chars().count() == 1)
        .filter(|e| e.glyph.starts_with(glyph))
        .max_by_key(|e| e.ascii.len())
        .map(|e| e.ascii)
}

pub fn export_json(doc_text: &str) -> String {
    export_json_with_runs(doc_text, &[])
}

/// JSON export including the notebook record: `export_json`'s shape
/// plus a `"blocks"` array — per blank-line block: index, range,
/// heading, its kernel statements (offset, kind, body hash), and the
/// run-log slice for its result-bearing statements (N-series N3).
/// The document + its log *is* the reproducibility record;
/// everything here is derived from the doc and the bridge's in-memory
/// log — nothing is persisted into the doc text.
pub fn export_json_with_runs(doc_text: &str, runs: &[crate::kernel_bridge::RunEntry]) -> String {
    use std::hash::{Hash, Hasher};

    let scan = scan(doc_text);
    let segments = resolve_segments(&scan);
    let render = to_render_text(doc_text, &scan, &segments, &TransformOptions::default());
    let mut idx = SemanticIndex::default();
    idx.build_index(doc_text, &segments, &[&render]);

    let block_ranges = mathed_core::blocks::split_blocks(doc_text);
    let block_of = |pos: usize| {
        block_ranges
            .iter()
            .rposition(|r| r.start <= pos)
            .unwrap_or(0)
    };
    let heading_of = |r: &std::ops::Range<usize>| {
        doc_text[r.clone()]
            .lines()
            .next()
            .unwrap_or_default()
            .trim()
            .trim_start_matches('=')
            .trim()
            .chars()
            .take(40)
            .collect::<String>()
    };
    let body_hash = |s: &str| {
        let mut h = std::collections::hash_map::DefaultHasher::new();
        s.hash(&mut h);
        h.finish()
    };

    let blocks: Vec<serde_json::Value> = block_ranges
        .iter()
        .enumerate()
        .map(|(bi, r)| {
            let statements: Vec<serde_json::Value> = idx
                .kernel_statements
                .iter()
                .filter(|s| block_of(s.span.start) == bi)
                .map(|s| {
                    serde_json::json!({
                        "offset": s.span.start,
                        "kind": format!("{:?}", s.kind),
                        "body_hash": body_hash(&s.body_text),
                    })
                })
                .collect();
            let block_runs: Vec<serde_json::Value> = runs
                .iter()
                .filter(|e| e.block == bi)
                .map(|e| {
                    serde_json::json!({
                        "offset": e.offset,
                        "input_hash": e.input_hash,
                        "op": e.op,
                        "timing_ms": e.timing_ms,
                        "result": result_json(&e.result),
                    })
                })
                .collect();
            serde_json::json!({
                "index": bi,
                "start": r.start,
                "len": r.end - r.start,
                "heading": heading_of(r),
                "statements": statements,
                "runs": block_runs,
            })
        })
        .collect();

    serde_json::json!({
        "kernel_statements": idx.kernel_statements.iter().map(|s| {
            serde_json::json!({
                "kind": format!("{:?}", s.kind),
                "name": s.name,
                "body": s.body_text,
                "model_name": s.model_name,
            })
        }).collect::<Vec<_>>(),
        "translators": idx.translators.keys().collect::<Vec<_>>(),
        "biblio_statements": idx.biblio_statements.len(),
        "blocks": blocks,
    })
    .to_string()
}

/// Serialize a kernel result into the notebook record.
fn result_json(result: &crate::kernel_bridge::KernelResult) -> serde_json::Value {
    match result {
        crate::kernel_bridge::KernelResult::Value(p) => serde_json::json!({ "value": p }),
        crate::kernel_bridge::KernelResult::StringValue(s) => {
            serde_json::json!({ "string": s })
        }
        crate::kernel_bridge::KernelResult::Error {
            code_name,
            message,
            hints,
        } => serde_json::json!({
            "error": {
                "code_name": code_name,
                "message": message,
                "hints": hints.iter().map(|h| {
                    serde_json::json!({
                        "kind": format!("{:?}", h.kind),
                        "target": h.target,
                        "suggestion": h.suggestion,
                    })
                }).collect::<Vec<_>>(),
            }
        }),
    }
}

pub fn export_markdown(doc_text: &str) -> String {
    let scan = scan(doc_text);
    let segments = resolve_segments(&scan);
    let mut out = String::new();
    let mut last_end = 0;

    for seg in &segments {
        if let Some(span) = &seg.span {
            if span.start > last_end {
                out.push_str(&doc_text[last_end..span.start]);
            }
            let body = doc_text[span.clone()].trim();
            if seg.kind.is_kernel() {
                out.push_str(&format!("$ {body} $"));
            } else {
                out.push_str(body);
            }
            last_end = span.end;
        }
    }
    if last_end < doc_text.len() {
        out.push_str(&doc_text[last_end..]);
    }

    out.lines()
        .filter(|l| !l.trim().starts_with('#') || l.trim().starts_with("# "))
        .collect::<Vec<_>>()
        .join("\n")
}

pub fn export_html(doc_text: &str) -> String {
    let scan = scan(doc_text);
    let segments = resolve_segments(&scan);
    let mut body = String::new();
    let mut last_end = 0;

    for seg in &segments {
        if let Some(span) = &seg.span {
            if span.start > last_end {
                body.push_str(&escape_html(&doc_text[last_end..span.start]));
            }
            let raw = doc_text[span.clone()].trim();
            if seg.kind.is_kernel() {
                body.push_str(&format!(
                    "<span class=\"math-kernel\">{}</span>",
                    escape_html(raw)
                ));
            } else {
                body.push_str(&escape_html(raw));
            }
            last_end = span.end;
        }
    }
    if last_end < doc_text.len() {
        body.push_str(&escape_html(&doc_text[last_end..]));
    }

    format!(
        "<!DOCTYPE html>\n<html>\n<head>\n<meta charset=\"utf-8\">\n<title>mathed export</title>\n<style>\n.math-kernel {{ color: #1a56db; font-family: monospace; }}\n</style>\n</head>\n<body>\n{body}\n</body>\n</html>"
    )
}

pub fn export_tex(doc_text: &str) -> String {
    let scan = scan(doc_text);
    let segments = resolve_segments(&scan);
    let mut out = String::new();
    let mut last_end = 0;

    for seg in &segments {
        if let Some(span) = &seg.span {
            if span.start > last_end {
                out.push_str(&doc_text[last_end..span.start]);
            }
            let raw = doc_text[span.clone()].trim();
            if seg.kind.is_kernel() {
                out.push_str(&format!("${raw}$"));
            } else {
                out.push_str(raw);
            }
            last_end = span.end;
        }
    }
    if last_end < doc_text.len() {
        out.push_str(&doc_text[last_end..]);
    }

    format!(
        "\\documentclass{{article}}\n\\usepackage{{amsmath}}\n\\begin{{document}}\n{out}\n\\end{{document}}"
    )
}
/// Render the document as a Typst template (stage T4 of
/// PLAN_mathed_template_language.md): every `\template` segment's
/// `render(ctx)` body is evaluated against the document's
/// [`mathed_core::DocumentContext`] (overlaid with `extra_ctx_json`,
/// if any — the "template arguments" seam), and the returned markup
/// is spliced after the segment's body via the T3
/// `TransformOptions::template_splices` seam. Template bodies
/// collapse to `▸ template: name` like translator code. A
/// document without `\template` segments renders byte-identically
/// to [`export_typst`] (same transform, no splices).
///
/// The context reaches template code as a **Typst dictionary
/// literal** (not a JSON string to decode — typst 0.15's `json`
/// module has `encode` but no `decode`), so strings stay `Str` and
/// templates build markup strings directly, e.g.
/// `#let render(ctx) = "#strong[" + ctx.at("blocks").at(0).at("heading") + "]"`.
/// T5: apply the authoring-time egison rules binary to a template
/// body. Reads `MATHED_RULES_BIN` (dev-machine only); the binary
/// consumes `{op, body}` JSON on stdin and writes `{markup}` JSON on
/// stdout (see `tools/mathed_rules/README.md`). Returns `None` when
/// the binary is absent, fails, or times out — the caller keeps the
/// body (identity path), so `--render-typst` works with and without
/// it.
pub fn apply_mathed_rules(body: &str, op: &str) -> Option<String> {
    mathed_rules_engine(body, op, None)
}

/// T8: the generalized rules seam — [`apply_mathed_rules`] plus a
/// `slice` argument for the selection ops: for `select` /
/// `select/pattern` the caller pre-slices `DocumentContext` rows and
/// passes them here, and the slice (not the raw body) is what the
/// binary matches over (the `tools/mathed_rules` contract). Other
/// ops ignore the slice. Returns `None` when the binary is absent,
/// fails, or times out — the caller keeps the body (identity path).
pub fn mathed_rules_engine(body: &str, op: &str, slice: Option<&str>) -> Option<String> {
    let bin = std::env::var("MATHED_RULES_BIN").ok()?;
    mathed_rules_engine_with_bin(&bin, body, op, slice)
}

/// The env-free core of [`apply_mathed_rules`] — testable without
/// touching process env. Bounded at 5 s so a hung rules binary can
/// never block an export indefinitely.
pub fn apply_mathed_rules_with_bin(bin: &str, body: &str, op: &str) -> Option<String> {
    mathed_rules_engine_with_bin(bin, body, op, None)
}

/// The env-free core of [`mathed_rules_engine`]. For selection ops
/// the pre-sliced rows win over the raw body.
pub fn mathed_rules_engine_with_bin(
    bin: &str,
    body: &str,
    op: &str,
    slice: Option<&str>,
) -> Option<String> {
    use std::io::{Read, Write};
    use std::process::{Command, Stdio};
    use std::time::{Duration, Instant};

    let payload = if op.starts_with("select") {
        slice.unwrap_or(body)
    } else {
        body
    };
    let input = serde_json::json!({ "op": op, "body": payload }).to_string();
    let mut child = Command::new(bin)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()
        .ok()?;
    let stdin = child.stdin.take()?;
    // Write stdin on a thread so a full pipe cannot deadlock.
    let writer = std::thread::spawn(move || {
        let mut stdin = stdin;
        let _ = stdin.write_all(input.as_bytes());
    });
    // Drain stdout on a thread; wait for exit with a deadline.
    let stdout = child.stdout.take()?;
    let reader = std::thread::spawn(move || {
        let mut buf = Vec::new();
        let mut stdout = stdout;
        let _ = stdout.read_to_end(&mut buf);
        buf
    });
    let deadline = Instant::now() + Duration::from_secs(5);
    let status = loop {
        if let Some(s) = child.try_wait().ok()? {
            break s;
        }
        if Instant::now() > deadline {
            let _ = child.kill();
            return None;
        }
        std::thread::sleep(Duration::from_millis(10));
    };
    let _ = writer.join();
    let out_buf = reader.join().ok()?;
    if !status.success() {
        return None;
    }
    serde_json::from_slice::<serde_json::Value>(&out_buf)
        .ok()?
        .get("markup")?
        .as_str()
        .map(str::to_string)
}

pub fn export_typst_template(
    doc_text: &str,
    extra_ctx_json: Option<&str>,
) -> Result<String, String> {
    render_doc(doc_text, extra_ctx_json, std::collections::HashMap::new())
}

/// N5: `--export-typst --with-outputs` — render the document with each
/// block's computed output region spliced beneath its content (the
/// printable notebook page). Regions are **derived state**: the bridge
/// runs the live kernel statements to completion, then each region's
/// markup is spliced at its block's end offset via
/// `TransformOptions.block_splices` — never written into the doc text.
/// A document without kernel statements renders byte-identically to
/// plain `--export-typst` (no regions to splice).
pub fn export_typst_with_outputs(doc_text: &str) -> Result<String, String> {
    let block_ranges = mathed_core::blocks::split_blocks(doc_text);
    let mut bridge = crate::kernel_bridge::KernelBridge::new();
    bridge.refresh(doc_text);
    // Settle best-effort: a hung worker must not hang the export, so
    // the wait is bounded and partial results still render.
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(30);
    while !bridge.is_idle() {
        bridge.poll();
        if std::time::Instant::now() > deadline {
            break;
        }
        std::thread::sleep(std::time::Duration::from_millis(10));
    }
    let mut block_splices = std::collections::HashMap::new();
    for (bi, r) in block_ranges.iter().enumerate() {
        let outputs = bridge.block_outputs(bi);
        if outputs.is_empty() {
            continue;
        }
        let timings: std::collections::HashMap<usize, u64> = outputs
            .iter()
            .filter_map(|(off, _)| bridge.timing_of(*off).map(|t| (*off, t)))
            .collect();
        let markup = crate::output_region::region_markup_with_timings(&outputs, &timings);
        block_splices.insert(
            r.end,
            format!("// ---- block {bi} output region ----\n{markup}\n"),
        );
    }
    render_doc(doc_text, None, block_splices)
}

/// Shared template render: evaluate every `\template` against the
/// derived context (with the caller's overlay), splice the results at
/// the T3 seam, and — for `--with-outputs` — splice each block's
/// computed region at its end offset. `block_splices` is empty for
/// the plain template export, so that path stays byte-identical to
/// the T4 fixture.
fn render_doc(
    doc_text: &str,
    extra_ctx_json: Option<&str>,
    block_splices: std::collections::HashMap<usize, String>,
) -> Result<String, String> {
    let (scan, segments, _, idx) = crate::kernel_bridge::scan_pipeline(doc_text);

    // DocumentContext → value, with the caller's overlay merged at
    // the top level (overlay keys win over derived keys), then
    // lowered to a Typst literal expression.
    let ctx = mathed_core::DocumentContext::from_index(
        doc_text,
        &scan,
        &idx,
        &std::collections::HashMap::new(),
    );
    let mut ctx_value =
        serde_json::to_value(&ctx).map_err(|e| format!("serialize context: {e}"))?;
    if let Some(extra) = extra_ctx_json {
        let extra: serde_json::Value =
            serde_json::from_str(extra).map_err(|e| format!("--ctx is not valid JSON: {e}"))?;
        merge_overlay(&mut ctx_value, extra);
    }
    let ctx_literal = ctx_to_typst_literal(&ctx_value)?;

    // Evaluate each template against the shared context; a failing
    // template fails the export loudly (never a silent partial
    // render).
    let mut splices = std::collections::HashMap::new();
    let mut template_outputs: std::collections::HashMap<String, String> =
        std::collections::HashMap::new();
    let mut engine = crate::translate::Translator::new();
    for (name, def) in &idx.templates {
        // T5/T8: when the authoring-time egison rules binary is
        // present (MATHED_RULES_BIN), the body may be
        // notation-rewritten first (rewrite, then right-assoc);
        // absent or failing, the body runs unchanged (identity
        // path).
        let mut body = def.body_text.clone();
        if let Some(r) = apply_mathed_rules(&body, "rewrite") {
            body = r;
        }
        if let Some(r) = apply_mathed_rules(&body, "rewrite/assoc") {
            body = r;
        }
        match engine.run_template(&body, &ctx_literal) {
            Ok(markup) => {
                template_outputs.insert(name.clone(), markup.clone());
                splices.insert(def.span.start, markup);
            }
            Err(e) => {
                return Err(format!("\\template `{name}` failed to render: {e}"));
            }
        }
    }

    // T7: base-template composition. When a `\base` segment exists,
    // its `render(ctx)` output *is* the exported document: `ctx.body`
    // carries the doc-body markup (without template splices — the
    // templates reach the base through `ctx.templates`), so the base
    // wraps rather than duplicates. No base → the plain path below
    // (templates spliced inline), byte-identical to the T4 fixture.
    if let Some(base) = &idx.base {
        let body_render = to_render_text(
            doc_text,
            &scan,
            &segments,
            &TransformOptions {
                block_splices: block_splices.clone(),
                ..Default::default()
            },
        );
        let mut base_ctx = ctx_value.clone();
        base_ctx["body"] = serde_json::Value::String(body_render.text);
        let mut tmpls = serde_json::Map::new();
        for (name, markup) in &template_outputs {
            if is_typst_ident(name) {
                tmpls.insert(name.clone(), serde_json::Value::String(markup.clone()));
            }
        }
        base_ctx["templates"] = serde_json::Value::Object(tmpls);
        let base_literal = ctx_to_typst_literal(&base_ctx)?;
        let mut body = base.body_text.clone();
        if let Some(r) = apply_mathed_rules(&body, "rewrite") {
            body = r;
        }
        if let Some(r) = apply_mathed_rules(&body, "rewrite/assoc") {
            body = r;
        }
        let out = match engine.run_base(&body, &base_literal) {
            Ok(markup) => markup,
            Err(e) => return Err(format!("\\base `{}` failed to render: {e}", base.name)),
        };
        // The base output *is* the exported file — it must parse.
        let parsed = typst::syntax::parse(&out);
        let (errors, _) = parsed.errors_and_warnings();
        if !errors.is_empty() {
            return Err(format!(
                "\\base output does not parse as Typst ({} error(s)): {out}",
                errors.len()
            ));
        }
        return Ok(format!(
            "// Exported from mathed_mini (rendered template)\n{out}"
        ));
    }

    let render = to_render_text(
        doc_text,
        &scan,
        &segments,
        &TransformOptions {
            template_splices: splices,
            block_splices,
            ..Default::default()
        },
    );
    Ok(format!(
        "// Exported from mathed_mini (rendered template)\n{}",
        render.text
    ))
}

/// Merge `extra` into `base` (both JSON objects merge key-wise,
/// overlay wins; a non-object overlay replaces the whole context).
fn merge_overlay(base: &mut serde_json::Value, extra: serde_json::Value) {
    match (base, extra) {
        (serde_json::Value::Object(b), serde_json::Value::Object(x)) => {
            for (k, v) in x {
                b.insert(k, v);
            }
        }
        (b, x) => *b = x,
    }
}

/// Lower a JSON value to a Typst expression: objects become
/// `(key: value, …)` dictionaries, arrays `(value, …)`, strings
/// quoted (so they stay `Str`), numbers/bools literal, null `none`.
/// Object keys must be Typst identifiers (the DocumentContext keys
/// are; the `--ctx` overlay is validated here).
fn ctx_to_typst_literal(v: &serde_json::Value) -> Result<String, String> {
    fn go(v: &serde_json::Value) -> Result<String, String> {
        Ok(match v {
            serde_json::Value::Null => "none".to_string(),
            serde_json::Value::Bool(b) => b.to_string(),
            serde_json::Value::Number(n) => n.to_string(),
            serde_json::Value::String(s) => crate::translate::typst_str_lit(s),
            serde_json::Value::Array(items) => {
                let inner: Result<Vec<String>, String> = items.iter().map(go).collect();
                match inner?.as_slice() {
                    [] => "()".to_string(),
                    // A single element needs the trailing comma,
                    // otherwise `(x)` parses as a grouped value,
                    // not a one-element array.
                    [one] => format!("({one},)"),
                    many => format!("({})", many.join(", ")),
                }
            }
            serde_json::Value::Object(map) => {
                let mut parts = Vec::new();
                for (k, val) in map {
                    if !is_typst_ident(k) {
                        return Err(format!("ctx key `{k}` is not a valid Typst identifier"));
                    }
                    parts.push(format!("{k}: {}", go(val)?));
                }
                if parts.is_empty() {
                    "()".to_string()
                } else {
                    format!("({})", parts.join(", "))
                }
            }
        })
    }
    go(v)
}

/// Whether `k` can be a Typst identifier (a dictionary-literal key in
/// the lowered ctx expression).
fn is_typst_ident(k: &str) -> bool {
    let mut cs = k.chars();
    matches!(cs.next(), Some(c) if c.is_ascii_alphabetic() || c == '_')
        && cs.all(|c| c.is_ascii_alphanumeric() || c == '_')
}

fn escape_html(s: &str) -> String {
    s.replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn typst_export_produces_valid_output() {
        let doc = "= Title\n\nSome text with $ E = m c^2 $ math.";
        let out = export_typst(doc);
        assert!(out.contains("mathed_mini"), "header: {out}");
        assert!(out.contains("Title"), "title preserved: {out}");
    }

    #[test]
    fn json_export_parses() {
        let doc = "#1 a #2 \\model(#1,#2)\n\n#3 vac #4 \\prob(#3,#4)";
        let out = export_json(doc);
        let parsed: serde_json::Value = serde_json::from_str(&out).expect("valid JSON");
        assert!(parsed.get("kernel_statements").is_some());
        let stmts = parsed["kernel_statements"].as_array().unwrap();
        assert!(
            stmts.len() >= 2,
            "expected 2+ statements, got {}",
            stmts.len()
        );
    }

    // ── N3: --export-json gains the notebook record ────────────

    #[test]
    fn json_export_record_includes_blocks_and_runs() {
        use crate::kernel_bridge::{KernelResult, RunEntry};
        let doc = "= Cell\n\
                   #1 a #2 \\model(#1,#2)\n\n\
                   #3 vac #4 \\prob(#3,#4)";
        let runs = vec![RunEntry {
            block: 1,
            offset: 99,
            input_hash: 7,
            op: "probability".to_string(),
            timing_ms: 12,
            result: KernelResult::Value(0.5),
        }];
        let out = export_json_with_runs(doc, &runs);
        let parsed: serde_json::Value = serde_json::from_str(&out).expect("valid JSON");
        let blocks = parsed["blocks"].as_array().expect("blocks array");
        assert_eq!(blocks.len(), 2, "two blank-line blocks");
        // Block 1 owns the prob statement and its run.
        let b1 = &blocks[1];
        assert_eq!(b1["index"], 1);
        let stmts = b1["statements"].as_array().unwrap();
        assert_eq!(stmts.len(), 1, "prob statement");
        assert!(stmts[0]["offset"].as_u64().is_some());
        assert!(stmts[0]["body_hash"].as_u64().is_some());
        let block_runs = b1["runs"].as_array().unwrap();
        assert_eq!(block_runs.len(), 1);
        assert_eq!(block_runs[0]["op"], "probability");
        assert_eq!(block_runs[0]["timing_ms"], 12);
        assert_eq!(block_runs[0]["input_hash"], 7);
        assert_eq!(block_runs[0]["result"]["value"], 0.5);
    }

    #[test]
    fn json_export_record_includes_exec_runs() {
        // N4: a granted `\exec` completes and its run lands in the
        // notebook record like any other op.
        let doc = "= Cell\n\
                   #1 echo hello #2 \\exec(#1,#2, grants: \"readonly\")";
        let mut bridge = crate::kernel_bridge::KernelBridge::with_exec_grants(&["readonly"]);
        bridge.refresh(doc);
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(15);
        loop {
            bridge.poll();
            if bridge.run_log().iter().any(|e| e.op == "exec") {
                break;
            }
            assert!(
                std::time::Instant::now() < deadline,
                "exec run never completed"
            );
            std::thread::sleep(std::time::Duration::from_millis(10));
        }
        let out = export_json_with_runs(doc, bridge.run_log());
        let parsed: serde_json::Value = serde_json::from_str(&out).expect("valid JSON");
        let blocks = parsed["blocks"].as_array().unwrap();
        let runs = blocks[0]["runs"].as_array().unwrap();
        assert_eq!(runs.len(), 1, "one exec run recorded: {runs:?}");
        assert_eq!(runs[0]["op"], "exec");
        assert_eq!(runs[0]["result"]["string"], "hello\n");
    }

    #[test]
    fn json_export_without_runs_keeps_shape_with_empty_runs() {
        let doc = "#1 a #2 \\model(#1,#2)\n\n#3 vac #4 \\prob(#3,#4)";
        let out = export_json(doc);
        let parsed: serde_json::Value = serde_json::from_str(&out).expect("valid JSON");
        assert!(
            parsed.get("kernel_statements").is_some(),
            "existing keys kept"
        );
        let blocks = parsed["blocks"].as_array().expect("blocks array");
        assert_eq!(blocks.len(), 2);
        for b in blocks {
            assert!(b["runs"].as_array().unwrap().is_empty());
            assert!(b["statements"].as_array().is_some());
        }
    }

    #[test]
    fn json_export_record_is_stable_for_fixed_trace() {
        use crate::kernel_bridge::{KernelResult, RunEntry};
        let doc = "= Cell\n\
                   #1 a #2 \\model(#1,#2)\n\n\
                   #3 vac #4 \\prob(#3,#4)";
        let runs = vec![RunEntry {
            block: 1,
            offset: 99,
            input_hash: 7,
            op: "probability".to_string(),
            timing_ms: 12,
            result: KernelResult::Value(0.5),
        }];
        // The same document + log must export to the same bytes
        // (deterministic record — reproducibility needs it).
        assert_eq!(
            export_json_with_runs(doc, &runs),
            export_json_with_runs(doc, &runs)
        );
    }

    // ── U4: --export-ascii interchange projection ──────────────

    #[test]
    fn ascii_export_is_total_and_ascii_only() {
        // Math glyphs, CJK prose, emoji: everything must project to
        // ASCII bytes and the projection must be deterministic.
        let doc = "= 数学\n\n$ αβ + π ≤ 1 $\n\nA crab 🦀 and an arrow →";
        let out = export_ascii(doc);
        assert!(out.is_ascii(), "output must be ASCII-only: {out}");
        // Total + deterministic: same input → same output.
        assert_eq!(out, export_ascii(doc));
    }

    #[test]
    fn ascii_export_uses_math_names_inside_math() {
        let doc = "$ αβ $ and plain text";
        let out = export_ascii(doc);
        assert!(out.contains("$ \\alpha\\beta $"), "math names: {out}");
        // Idempotent: already-ASCII input round-trips unchanged.
        let again = export_ascii(&out);
        assert_eq!(out, again);
        // An escaped `$` must not close the math fence.
        let out2 = export_ascii("$ a \\$ α $");
        assert!(
            out2.contains("\\alpha"),
            "still math after escaped $: {out2}"
        );
    }

    // ── T5: egison rules binary seam ────────────────────────────

    /// A stub rules binary that answers the golden rewrite contract.
    fn stub_rules_bin(script: &str) -> std::path::PathBuf {
        use std::io::Write as _;
        let dir = std::env::temp_dir();
        // Content-hashed name: tests run in parallel, and two stubs
        // with equal script lengths used to collide (overwrite each
        // other mid-run).
        use std::hash::{Hash as _, Hasher as _};
        let mut h = std::collections::hash_map::DefaultHasher::new();
        script.hash(&mut h);
        let path = dir.join(format!(
            "mathed_rules_stub_{}_{:x}.sh",
            std::process::id(),
            h.finish()
        ));
        let mut f = std::fs::File::create(&path).expect("create stub");
        f.write_all(script.as_bytes()).expect("write stub");
        use std::os::unix::fs::PermissionsExt;
        let _ = std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o755));
        path
    }

    #[test]
    fn rules_binary_json_contract_roundtrips() {
        // The stub answers with the golden rewrite output; the seam
        // must parse the `{markup: …}` JSON and return the string.
        let stub = stub_rules_bin("#!/bin/sh\ncat\n");
        // `cat` echoes the input — make the stub emit the contract
        // output directly instead.
        let stub2 =
            stub_rules_bin("#!/bin/sh\nread -r _input\nprintf '%s' '{\"markup\":\"⟨a⟩ b\"}'\n");
        let got = apply_mathed_rules_with_bin(stub2.to_str().unwrap(), "a†, a, b", "rewrite");
        assert_eq!(got.as_deref(), Some("⟨a⟩ b"));
        let _ = std::fs::remove_file(&stub);
        let _ = std::fs::remove_file(&stub2);
    }

    #[test]
    fn rules_engine_new_ops_roundtrip() {
        // T8: the generalized seam passes the new op names through;
        // a stub answers for rewrite/assoc.
        let stub = stub_rules_bin(
            "#!/bin/sh\nread -r _input\nprintf '%s' '{\"markup\":\"a + ( b + c )\"}'\n",
        );
        let got = mathed_rules_engine_with_bin(
            stub.to_str().unwrap(),
            "a, +, b, +, c",
            "rewrite/assoc",
            None,
        );
        assert_eq!(got.as_deref(), Some("a + ( b + c )"));
    }

    #[test]
    fn rules_engine_select_ops_use_the_slice() {
        // T8: for select ops the pre-sliced rows win over the raw
        // body — the slice argument is part of the contract.
        let stub = stub_rules_bin(
            "#!/bin/sh\nread -r _input\nprintf '%s' '{\"markup\":\"x;z\"}'\n",
        );
        let got = mathed_rules_engine_with_bin(
            stub.to_str().unwrap(),
            "unused raw body",
            "select/pattern",
            Some("x:compute(x);y:7;z:compute(z)"),
        );
        assert_eq!(got.as_deref(), Some("x;z"));
    }

    #[test]
    fn rules_binary_failure_degrades_to_identity() {
        // Missing binary: None (caller keeps the body).
        assert!(apply_mathed_rules_with_bin("/nonexistent/mathed_rules", "x", "rewrite").is_none());
        // Non-zero exit: None.
        let stub = stub_rules_bin("#!/bin/sh\nexit 1\n");
        assert!(apply_mathed_rules_with_bin(stub.to_str().unwrap(), "x", "rewrite").is_none());
        let _ = std::fs::remove_file(&stub);
        // Malformed stdout: None.
        let stub2 = stub_rules_bin("#!/bin/sh\nprintf '%s' 'not json'\n");
        assert!(apply_mathed_rules_with_bin(stub2.to_str().unwrap(), "x", "rewrite").is_none());
        let _ = std::fs::remove_file(&stub2);
    }

    #[test]
    fn ascii_export_flags_exotic_glyphs() {
        // Arrow has no `\name` form (only `->`, not valid Typst):
        // explicit `\u{...}`. Exotic chars outside math: `\u{...}`.
        let out = export_ascii("$ → $ 🦀");
        assert!(out.contains("\\u{2192}"), "arrow flagged: {out}");
        assert!(out.contains("\\u{1F980}"), "crab flagged: {out}");
        assert!(out.is_ascii());
        assert!(!out.contains('→'));
        assert!(!out.contains('🦀'));
    }

    #[test]
    fn markdown_export_strips_markers() {
        let doc = "= Title\n\n#1 a #2 \\model(#1,#2)\n\nPlain text.";
        let out = export_markdown(doc);
        assert!(out.contains("Title"), "title: {out}");
        assert!(out.contains("Plain text"), "plain text: {out}");
    }

    #[test]
    fn html_export_produces_valid_structure() {
        let doc = "= Title\n\n#1 a #2 \\model(#1,#2)\n\nSome <text> & more.";
        let out = export_html(doc);
        assert!(out.contains("<!DOCTYPE html>"), "doctype: {out}");
        assert!(out.contains("&lt;text&gt;"), "escaped: {out}");
        assert!(out.contains("&amp;"), "ampersand escaped: {out}");
        assert!(out.contains("math-kernel"), "kernel span: {out}");
    }

    #[test]
    fn tex_export_wraps_document() {
        let doc = "#1 a #2 \\model(#1,#2)\n\nEuler: $ e^{i\\pi} + 1 = 0 $";
        let out = export_tex(doc);
        assert!(out.contains("\\documentclass{article}"), "preamble: {out}");
        assert!(out.contains("\\begin{document}"), "begin: {out}");
        assert!(out.contains("\\end{document}"), "end: {out}");
        assert!(out.contains("$"), "math mode: {out}");
    }

    // ── T4: --render-typst (template rendering) ───────────────

    #[test]
    fn checked_in_rendered_fixture_parses() {
        // T6: the companion example's rendered `.typ` is checked in;
        // it must stay parseable Typst (T4 acceptance: zero syntax
        // errors in the existing Typst world).
        let src = std::fs::read_to_string(concat!(
            env!("CARGO_MANIFEST_DIR"),
            "/fixtures/template_report.rendered.typ"
        ))
        .expect("rendered fixture");
        let parsed = typst::syntax::parse(&src);
        let (errors, _) = parsed.errors_and_warnings();
        assert!(
            errors.is_empty(),
            "rendered fixture must parse cleanly: {errors:?}"
        );
    }

    #[test]
    fn template_render_splices_markup_and_hides_code() {
        let doc = concat!(
            "= Report\n\n",
            "#1 #let render(ctx) = \"#emph[expanded]\" #2 ",
            "\\template(#1,#2, name: rep)\n\n",
            "Body text.\n",
        );
        let out = export_typst_template(doc, None).expect("template export");
        assert!(out.contains("#emph[expanded]"), "spliced markup: {out}");
        assert!(out.contains("Body text."), "body preserved: {out}");
        assert!(out.contains("template: rep"), "collapsed title: {out}");
        assert!(
            !out.contains("#let render"),
            "template code body must be hidden (collapsed): {out}"
        );
    }

    #[test]
    fn template_free_doc_renders_byte_identical_to_plain_export() {
        let doc = "= Title\n\n#1 a #2 \\model(#1,#2)\n\nPlain $E=mc^2$ text.\n";
        let plain = export_typst(doc);
        let templated = export_typst_template(doc, None).expect("no-template export");
        let expected = plain.replace(
            "// Exported from mathed_mini",
            "// Exported from mathed_mini (rendered template)",
        );
        assert_eq!(templated, expected, "template-free export must not change");
    }

    #[test]
    fn with_outputs_splices_regions_under_blocks() {
        // N5: a model + prob document exports with each block's
        // computed region beneath its content.
        let doc = "= Cell\n\
                   #1 a #2 \\model(#1,#2)\n\
                   #3 vac #4 \\prob(#3,#4)";
        let out = export_typst_with_outputs(doc).expect("with-outputs export");
        assert!(
            out.contains("// ---- block 0 output region ----"),
            "region banner spliced under the block: {out}"
        );
        assert!(out.contains("#138000"), "green value tint: {out}");
        assert!(
            out.contains("\\= 1.0000"),
            "computed value in region: {out}"
        );
        assert!(out.contains("· "), "timing annotation present: {out}");
    }

    #[test]
    fn with_outputs_matches_plain_export_without_kernel_statements() {
        // No kernel statements → no regions → byte-identical to the
        // plain export (the T4 fixture pin holds for the report path).
        let doc = "= Title\n\nPlain $E=mc^2$ text.\n";
        let plain = export_typst(doc);
        let with_outputs = export_typst_with_outputs(doc).expect("no-kernel export");
        let expected = plain.replace(
            "// Exported from mathed_mini",
            "// Exported from mathed_mini (rendered template)",
        );
        assert_eq!(
            with_outputs, expected,
            "without kernel statements the report path is the plain export"
        );
    }

    #[test]
    fn template_failure_is_loud_and_named() {
        let doc = "#1 not valid typst code #2 \\template(#1,#2, name: bad)";
        let err = export_typst_template(doc, None).unwrap_err();
        assert!(err.contains("bad"), "names the failing template: {err}");
    }
    #[test]
    fn template_ctx_overlay_reaches_render() {
        // The template reads an overlaid key out of the ctx literal,
        // proving --ctx flowed through the overlay merge.
        let doc = concat!(
            "#1 #let render(ctx) = \"author: \" + ctx.at(\"author\") ",
            "#2 \\template(#1,#2, name: byline)",
        );
        let out = export_typst_template(doc, Some(r#"{"author": "leo"}"#)).expect("byline export");
        assert!(out.contains("author: leo"), "overlay value spliced: {out}");
    }

    #[test]
    fn base_wraps_body_and_keeps_it_out_of_the_plain_splice() {
        // T7: with a \base, its output *is* the exported file: the
        // body reaches the base through ctx.body, and the doc text
        // itself is not re-spliced.
        let doc = concat!(
            "= Report\n\n",
            "#1 #let render(ctx) = \"#box[HEAD]\\n\" + ctx.at(\"body\") + \"\\n#box[TAIL]\" #2 ",
            "\\base(#1,#2, name: wrap)\n\n",
            "Body text.\n",
        );
        let out = export_typst_template(doc, None).expect("base export");
        assert!(out.contains("#box[HEAD]"), "base head: {out}");
        assert!(out.contains("#box[TAIL]"), "base tail: {out}");
        assert!(out.contains("Body text."), "base wraps the doc body: {out}");
        assert!(
            !out.contains("template: "),
            "no template splice in base mode: {out}"
        );
    }

    #[test]
    fn base_ctx_templates_carries_subtemplate_output() {
        // The base reads a plain template's rendered output out of
        // ctx.templates (the Jinja include role).
        let doc = concat!(
            "#1 #let render(ctx) = \"#emph[t1]\" #2 ",
            "\\template(#1,#2, name: t1)\n",
            "#3 #let render(ctx) = \"#box[WRAP]\" + ctx.at(\"templates\").at(\"t1\") + \"#box[END]\" #4 ",
            "\\base(#3,#4, name: wrap)",
        );
        let out = export_typst_template(doc, None).expect("base export");
        assert!(out.contains("#box[WRAP]"), "base prefix: {out}");
        assert!(
            out.contains("#emph[t1]"),
            "sub-template output reached the base via ctx.templates: {out}"
        );
        assert!(out.contains("#box[END]"), "base suffix: {out}");
    }

    #[test]
    fn base_output_must_parse_as_typst() {
        // The base output *is* the exported file: unparseable markup
        // fails loudly instead of producing a broken .typ.
        let doc = concat!(
            "#1 #let render(ctx) = \"#emph[unclosed\" #2 ",
            "\\base(#1,#2, name: bad)",
        );
        let err = export_typst_template(doc, None).unwrap_err();
        assert!(err.contains("does not parse"), "parse failure surfaced: {err}");
    }

    #[test]
    fn template_filter_helpers_evaluate() {
        // T7: the builtin_template.typ helpers (filters role) are
        // prepended to every template body and callable from
        // render(ctx).
        let doc = concat!(
            "#1 #let render(ctx) = \"sum: \" + join((\"a\", \"b\", \"c\"), sep: \"+\") #2 ",
            "\\template(#1,#2, name: joined)",
        );
        let out = export_typst_template(doc, None).expect("filter export");
        assert!(out.contains("sum: a+b+c"), "helper `join` evaluated: {out}");
    }

    #[test]
    fn ctx_literal_lowers_typed_context_fields() {
        // Strings stay Str (so markup strings build directly), ints
        // stay literal, arrays become (...) with trailing commas for
        // single elements.
        assert_eq!(
            ctx_to_typst_literal(&serde_json::json!({
                "title": "hello",
                "n": 2,
                "list": ["a", "b"],
                "one": ["x"],
                "empty": []
            }))
            .unwrap(),
            // serde_json maps sort keys (no preserve_order feature),
            // so the literal is emitted in sorted key order.
            "(empty: (), list: (\"a\", \"b\"), n: 2, one: (\"x\",), title: \"hello\")"
        );
        // Overlay keys that are not Typst identifiers are rejected.
        assert!(ctx_to_typst_literal(&serde_json::json!({ "my key": 1 })).is_err());
    }
}
