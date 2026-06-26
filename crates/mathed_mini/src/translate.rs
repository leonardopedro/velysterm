//! The user-defined translator engine (P3 #10).
//!
//! A *translator* is Typst source that defines a `#let translate(body) = …`
//! binding mapping a math source string to a JSON payload for the kernel
//! (`TermSpec[]` for `\model`, an `EventPredicate` for `\event`/`\prob`).
//! This module evaluates that source against a `body` string and returns the
//! JSON string the translator produced.
//!
//! Evaluation strategy: we append `#let __mathed_result = translate(<body>)`
//! to the translator source so Typst invokes the function *during module
//! evaluation*, then read `__mathed_result` from the module scope. This avoids
//! constructing a layout `Vm`/`Args` by hand to call a `Value::Func` from Rust
//! (see `docs/mathed/TRANSLATOR_DESIGN.md` §5 Risk A — this is the resolved
//! "let-binding" path).
//!
//! The returned value is a raw JSON *string*; parsing it into `Vec<TermSpec>`
//! or an `EventPredicate` is the dispatcher's job, keeping the typst-eval
//! boundary free of kernel types.

use crate::world::MiniWorld;
use typst::foundations::Value;

/// The default translator, used when a `\model`/`\event`/`\prob` segment names
/// no translator. It is intentionally minimal — real documents define a
/// model-specific translator — but it is valid Typst returning valid JSON.
pub const BUILTIN_TRANSLATOR: &str =
    include_str!("builtin_translator.typ");

/// The scope binding the engine reads back after evaluation.
const RESULT_BINDING: &str = "__mathed_result";

/// Why a translator failed to produce a JSON string.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum TranslateError {
    /// The Typst source failed to evaluate (syntax/runtime error). Carries
    /// the concatenated Typst diagnostics.
    Eval(String),
    /// Evaluation succeeded but the result binding was absent (defensive: the
    /// appended `translate(body)` call normally surfaces a missing function as
    /// an [`Eval`](Self::Eval) error instead).
    MissingResult,
    /// `translate(body)` returned a non-string value (translators must return
    /// a JSON string, e.g. via `json.encode(...)`).
    NotString,
    /// The translator returned an empty string.
    Empty,
}

impl std::fmt::Display for TranslateError {
    fn fmt(
        &self,
        f: &mut std::fmt::Formatter<'_>,
    ) -> std::fmt::Result {
        match self {
            Self::Eval(msg) => {
                write!(f, "translator eval error: {msg}")
            }
            Self::MissingResult => write!(
                f,
                "translator defined no `translate(body)` function"
            ),
            Self::NotString => {
                write!(f, "translator returned a non-string value")
            }
            Self::Empty => {
                write!(f, "translator returned an empty string")
            }
        }
    }
}

impl std::error::Error for TranslateError {}

/// Evaluates translators, reusing one [`MiniWorld`] (and its loaded fonts)
/// across calls so repeated evaluation does not reload the font set.
pub struct Translator {
    world: MiniWorld,
}

impl Default for Translator {
    fn default() -> Self {
        Self::new()
    }
}

impl Translator {
    /// Create a translator engine with a fresh evaluation world.
    pub fn new() -> Self {
        Self {
            world: MiniWorld::new(""),
        }
    }

    /// Run `translator_src` against `body`, returning the JSON string the
    /// translator's `translate(body)` function produced.
    pub fn run(
        &mut self,
        translator_src: &str,
        body: &str,
    ) -> Result<String, TranslateError> {
        let src = format!(
            "{translator_src}\n#let {RESULT_BINDING} = translate({})\n",
            typst_str_lit(body),
        );
        self.world.set_markup(src);
        match self
            .world
            .eval_binding(RESULT_BINDING)
            .map_err(TranslateError::Eval)?
        {
            None => Err(TranslateError::MissingResult),
            Some(Value::Str(s)) => {
                let s = s.as_str().to_string();
                if s.is_empty() {
                    Err(TranslateError::Empty)
                } else {
                    Ok(s)
                }
            }
            Some(_) => Err(TranslateError::NotString),
        }
    }

    /// Run the built-in default translator against `body`.
    pub fn run_builtin(
        &mut self,
        body: &str,
    ) -> Result<String, TranslateError> {
        self.run(BUILTIN_TRANSLATOR, body)
    }
}

/// Escape `s` into a Typst double-quoted string literal (including the
/// surrounding quotes), so a math source string can be injected verbatim into
/// generated Typst source.
fn typst_str_lit(s: &str) -> String {
    let mut out = String::with_capacity(s.len() + 2);
    out.push('"');
    for c in s.chars() {
        match c {
            '\\' => out.push_str("\\\\"),
            '"' => out.push_str("\\\""),
            '\n' => out.push_str("\\n"),
            '\r' => out.push_str("\\r"),
            '\t' => out.push_str("\\t"),
            _ => out.push(c),
        }
    }
    out.push('"');
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn translate_returns_json() {
        let mut t = Translator::new();
        let out = t
            .run("#let translate(body) = { \"[42]\" }", "ignored")
            .expect("trivial translator");
        assert_eq!(out, "[42]");
    }

    #[test]
    fn translate_receives_body() {
        let mut t = Translator::new();
        let out = t
            .run("#let translate(body) = { body }", "a^\\dagger a")
            .expect("echo translator");
        assert_eq!(out, "a^\\dagger a");
    }

    #[test]
    fn translate_can_build_json_from_body() {
        // A realistic translator: emit a single (create, annihilate) term.
        let src = r#"#let translate(body) = {
            let ops = (
              (kind: "create", level: "inner_boson", mode: 0),
              (kind: "annihilate", level: "inner_boson", mode: 0),
            )
            let term = (coeff_re: 1.0, coeff_im: 0.0, ops: ops)
            json.encode((term,))
        }"#;
        let mut t = Translator::new();
        let out =
            t.run(src, "a^\\dagger a").expect("json translator");
        // `json.encode` pretty-prints by default, so compare whitespace-free.
        let compact: String =
            out.chars().filter(|c| !c.is_whitespace()).collect();
        assert!(
            compact.contains("\"kind\":\"create\""),
            "got: {out}"
        );
        assert!(compact.contains("\"mode\":0"), "got: {out}");
    }

    #[test]
    fn builtin_translator_runs_and_returns_json_array() {
        let mut t = Translator::new();
        let out =
            t.run_builtin("anything").expect("builtin translator");
        // The builtin returns a JSON array (empty by default).
        assert!(out.trim_start().starts_with('['), "got: {out}");
    }

    #[test]
    fn translate_eval_error() {
        let mut t = Translator::new();
        // `#let` with no body is a Typst syntax error.
        let err = t.run("#let translate(body) =", "x").unwrap_err();
        assert!(
            matches!(err, TranslateError::Eval(_)),
            "got: {err:?}"
        );
    }

    #[test]
    fn translate_missing_function_is_eval_error() {
        let mut t = Translator::new();
        // No `translate` binding: the appended `translate(body)` call fails
        // to resolve, so evaluation errors (unknown variable).
        let err = t.run("#let other = 1", "x").unwrap_err();
        assert!(
            matches!(err, TranslateError::Eval(_)),
            "got: {err:?}"
        );
    }

    #[test]
    fn translate_non_string() {
        let mut t = Translator::new();
        let err =
            t.run("#let translate(body) = { 42 }", "x").unwrap_err();
        assert_eq!(err, TranslateError::NotString);
    }

    #[test]
    fn translate_empty_string() {
        let mut t = Translator::new();
        let err = t
            .run("#let translate(body) = { \"\" }", "x")
            .unwrap_err();
        assert_eq!(err, TranslateError::Empty);
    }

    #[test]
    fn typst_str_lit_escapes() {
        assert_eq!(typst_str_lit("a\\b"), "\"a\\\\b\"");
        assert_eq!(typst_str_lit("say \"hi\""), "\"say \\\"hi\\\"\"");
        assert_eq!(typst_str_lit("l1\nl2"), "\"l1\\nl2\"");
    }
}
