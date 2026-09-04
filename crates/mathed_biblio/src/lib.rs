//! `mathed_biblio` — bibliography & citation backend for velysterm's
//! math editor (P11.21), wrapping `../hayagriva::Library` + its CSL
//! formatting machinery. Both velysterm and `hayagriva` are `MIT OR
//! Apache-2.0`, so — unlike the MPL/AGPL `pattern_unfer` bridge or
//! the arms-length `arctic_authority` sibling crate — hayagriva is a
//! direct Cargo dependency, not an arms-length protocol bridge.
//!
//! Authors attach a bibliography (YAML or BibTeX) via a
//! `\bibliography(#1,#2, name, format: "yaml", style: "apa")` segment
//! and insert in-text citations via `\cite(#1,#2, "key-a", "key-b",
//! bib: "name", style: "apa")`. Per the P3.10 translator pivot, these
//! markers are emitted by a translator, never hand-written Typst-math
//! directly. `resolve_citations` is the bridge from
//! `mathed_core::semantics::SemanticIndex.biblio_statements` to
//! rendered strings.

use hayagriva::archive::ArchivedStyle;
use hayagriva::citationberg::{IndependentStyle, Style};
use hayagriva::{
    BibliographyDriver, BibliographyRequest, BufWriteFormat, CitationItem, CitationRequest, Library,
};
use mathed_core::markers::PropKind;
use mathed_core::semantics::BiblioStatement;
use std::collections::HashMap;

#[derive(Debug, thiserror::Error)]
pub enum BiblioError {
    #[error("bibliography parse error: {0}")]
    Parse(String),
    #[error("unknown citation style: {0}")]
    UnknownStyle(String),
    #[error("style '{0}' is a dependent CSL style; mathed_biblio only supports independent styles")]
    DependentStyle(String),
    #[error("unknown bibliography entry key: {0}")]
    UnknownKey(String),
    #[error("no \\bibliography is bound for this \\cite (bib: \"{0}\")")]
    UnknownBibliography(String),
    #[error("citation render error: {0}")]
    Render(String),
}

/// The CSL style used when a `\bibliography`/`\cite` segment does not
/// name one via `style:`.
pub const DEFAULT_STYLE: &str = "apa";

/// Parse a YAML (Hayagriva-native) bibliography source.
pub fn load_yaml(s: &str) -> Result<Library, BiblioError> {
    hayagriva::io::from_yaml_str(s).map_err(|e| BiblioError::Parse(e.to_string()))
}

/// Parse a BibTeX/BibLaTeX bibliography source.
pub fn load_bibtex(s: &str) -> Result<Library, BiblioError> {
    hayagriva::io::from_biblatex_str(s).map_err(|errs| {
        BiblioError::Parse(
            errs.iter()
                .map(|e| e.to_string())
                .collect::<Vec<_>>()
                .join("; "),
        )
    })
}

/// A bundled CSL style, resolved to the `IndependentStyle`
/// `hayagriva`'s driver needs (dependent styles, which only override
/// locale/terms, are rejected — pass the underlying independent
/// style's name instead).
pub struct CitationStyle(IndependentStyle);

impl CitationStyle {
    pub fn by_name(name: &str) -> Result<Self, BiblioError> {
        let archived = ArchivedStyle::by_name(name)
            .ok_or_else(|| BiblioError::UnknownStyle(name.to_string()))?;
        match archived.get() {
            Style::Independent(style) => Ok(Self(style)),
            Style::Dependent(_) => Err(BiblioError::DependentStyle(name.to_string())),
        }
    }
}

/// A loaded bibliography paired with the CSL style used to format it.
pub struct Bibliography {
    library: Library,
    style: CitationStyle,
}

impl Bibliography {
    pub fn new(library: Library, style: CitationStyle) -> Self {
        Self { library, style }
    }

    pub fn library(&self) -> &Library {
        &self.library
    }

    /// Render a single in-text citation covering `keys`, in the order
    /// given (multiple keys produce one grouped citation, e.g.
    /// `(Doe 2020; Roe 2021)`).
    pub fn cite(&self, keys: &[String]) -> Result<String, BiblioError> {
        let mut entries = Vec::with_capacity(keys.len());
        for k in keys {
            entries.push(
                self.library
                    .get(k)
                    .ok_or_else(|| BiblioError::UnknownKey(k.clone()))?,
            );
        }
        let items: Vec<_> = entries.into_iter().map(CitationItem::with_entry).collect();
        let req = CitationRequest::new(items, &self.style.0, None, &[], None);
        let elems = hayagriva::standalone_citation(req);
        let mut out = String::new();
        elems
            .write_buf(&mut out, BufWriteFormat::Plain)
            .map_err(|e| BiblioError::Render(e.to_string()))?;
        Ok(out)
    }

    /// Render the full reference list, in the citation style's
    /// bibliography order, as `(key, formatted_text)` pairs.
    pub fn reference_list(&self) -> Result<Vec<(String, String)>, BiblioError> {
        let mut driver = BibliographyDriver::new();
        for entry in self.library.iter() {
            driver.citation(CitationRequest::new(
                vec![CitationItem::with_entry(entry)],
                &self.style.0,
                None,
                &[],
                None,
            ));
        }
        let rendered = driver.finish(BibliographyRequest {
            style: &self.style.0,
            locale: None,
            locale_files: &[],
        });
        let bib = rendered.bibliography.ok_or_else(|| {
            BiblioError::Render("this CSL style produces no bibliography section".to_string())
        })?;
        bib.items
            .into_iter()
            .map(|item| {
                let mut out = String::new();
                item.content
                    .write_buf(&mut out, BufWriteFormat::Plain)
                    .map_err(|e| BiblioError::Render(e.to_string()))?;
                Ok((item.key, out))
            })
            .collect()
    }
}

/// Resolve a document's `\bibliography`/`\cite` statements (as
/// collected by `mathed_core::semantics::SemanticIndex::build_index`
/// into `SemanticIndex.biblio_statements`) into rendered in-text
/// citation strings, keyed by each `\cite` statement's document span
/// start.
///
/// Each `\bibliography` is parsed once per call (its `format:` picks
/// YAML vs. BibTeX, defaulting to YAML); each `\cite` resolves
/// against the `\bibliography` named by its `bib:` binding, or — when
/// exactly one bibliography is in scope — that one implicitly.
pub fn resolve_citations(
    statements: &[BiblioStatement],
) -> HashMap<usize, Result<String, BiblioError>> {
    let mut bibliographies: HashMap<String, Result<Bibliography, BiblioError>> = HashMap::new();

    for stmt in statements {
        if stmt.kind != PropKind::Bibliography {
            continue;
        }
        let key = stmt.name.clone().unwrap_or_default();
        let built = (|| {
            let library = match stmt.format.as_deref() {
                Some("bibtex") => load_bibtex(&stmt.body_text)?,
                _ => load_yaml(&stmt.body_text)?,
            };
            let style_name = stmt.style.as_deref().unwrap_or(DEFAULT_STYLE);
            let style = CitationStyle::by_name(style_name)?;
            Ok(Bibliography::new(library, style))
        })();
        bibliographies.insert(key, built);
    }

    let mut out = HashMap::new();
    for stmt in statements {
        if stmt.kind != PropKind::Cite {
            continue;
        }
        let bib_key = stmt.bib_name.clone().unwrap_or_else(|| {
            if bibliographies.len() == 1 {
                bibliographies.keys().next().cloned().unwrap_or_default()
            } else {
                String::new()
            }
        });

        let result = match bibliographies.get(&bib_key) {
            Some(Ok(bib)) => bib.cite(&stmt.keys),
            Some(Err(e)) => Err(BiblioError::Parse(e.to_string())),
            None => Err(BiblioError::UnknownBibliography(bib_key)),
        };
        out.insert(stmt.span.start, result);
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    const CRAZY_RICH_YAML: &str = r#"
crazy-rich:
    type: Book
    title: Crazy Rich Asians
    author: Kwan, Kevin
    date: 2014
    publisher: Anchor Books
    location: New York, NY, US
"#;

    #[test]
    fn load_yaml_parses_entries() {
        let lib = load_yaml(CRAZY_RICH_YAML).unwrap();
        assert_eq!(
            lib.get("crazy-rich")
                .unwrap()
                .title()
                .unwrap()
                .value
                .to_string(),
            "Crazy Rich Asians"
        );
    }

    #[test]
    fn load_yaml_rejects_garbage() {
        assert!(load_yaml("not: [valid, yaml: :::").is_err());
    }

    #[test]
    fn unknown_style_name_errors() {
        assert!(matches!(
            CitationStyle::by_name("definitely-not-a-real-style"),
            Err(BiblioError::UnknownStyle(_))
        ));
    }

    #[test]
    fn apa_style_resolves() {
        assert!(CitationStyle::by_name(DEFAULT_STYLE).is_ok());
    }

    #[test]
    fn cite_renders_known_entry() {
        let lib = load_yaml(CRAZY_RICH_YAML).unwrap();
        let style = CitationStyle::by_name(DEFAULT_STYLE).unwrap();
        let bib = Bibliography::new(lib, style);
        let rendered = bib.cite(&["crazy-rich".to_string()]).unwrap();
        assert!(
            rendered.contains("Kwan") || rendered.contains("2014"),
            "got: {rendered}"
        );
    }

    #[test]
    fn cite_unknown_key_errors() {
        let lib = load_yaml(CRAZY_RICH_YAML).unwrap();
        let style = CitationStyle::by_name(DEFAULT_STYLE).unwrap();
        let bib = Bibliography::new(lib, style);
        assert!(matches!(
            bib.cite(&["nope".to_string()]),
            Err(BiblioError::UnknownKey(_))
        ));
    }

    #[test]
    fn reference_list_contains_the_entry() {
        let lib = load_yaml(CRAZY_RICH_YAML).unwrap();
        let style = CitationStyle::by_name(DEFAULT_STYLE).unwrap();
        let bib = Bibliography::new(lib, style);
        let list = bib.reference_list().unwrap();
        assert_eq!(list.len(), 1);
        assert_eq!(list[0].0, "crazy-rich");
        assert!(list[0].1.contains("Crazy Rich Asians"));
    }

    #[allow(clippy::too_many_arguments)]
    fn stmt(
        kind: PropKind,
        name: Option<&str>,
        keys: &[&str],
        format: Option<&str>,
        style: Option<&str>,
        bib_name: Option<&str>,
        body_text: &str,
        span_start: usize,
    ) -> BiblioStatement {
        BiblioStatement {
            kind,
            block: 0,
            name: name.map(str::to_string),
            keys: keys.iter().map(|s| s.to_string()).collect(),
            format: format.map(str::to_string),
            style: style.map(str::to_string),
            bib_name: bib_name.map(str::to_string),
            body_text: body_text.to_string(),
            span: span_start..span_start + body_text.len(),
        }
    }

    #[test]
    fn resolve_citations_single_bibliography_implicit_binding() {
        let statements = vec![
            stmt(
                PropKind::Bibliography,
                Some("refs"),
                &[],
                None,
                None,
                None,
                CRAZY_RICH_YAML,
                0,
            ),
            stmt(
                PropKind::Cite,
                None,
                &["crazy-rich"],
                None,
                None,
                None,
                "",
                500,
            ),
        ];
        let results = resolve_citations(&statements);
        assert_eq!(results.len(), 1);
        let rendered = results.get(&500).unwrap().as_ref().unwrap();
        assert!(!rendered.is_empty());
    }

    #[test]
    fn resolve_citations_explicit_bib_binding() {
        let statements = vec![
            stmt(
                PropKind::Bibliography,
                Some("refs"),
                &[],
                None,
                None,
                None,
                CRAZY_RICH_YAML,
                0,
            ),
            stmt(
                PropKind::Cite,
                None,
                &["crazy-rich"],
                None,
                None,
                Some("refs"),
                "",
                500,
            ),
        ];
        let results = resolve_citations(&statements);
        assert!(results.get(&500).unwrap().is_ok());
    }

    #[test]
    fn resolve_citations_unbound_bib_name_errors() {
        let statements = vec![
            stmt(
                PropKind::Bibliography,
                Some("refs"),
                &[],
                None,
                None,
                None,
                CRAZY_RICH_YAML,
                0,
            ),
            stmt(
                PropKind::Cite,
                None,
                &["crazy-rich"],
                None,
                None,
                Some("other"),
                "",
                500,
            ),
        ];
        let results = resolve_citations(&statements);
        assert!(matches!(
            results.get(&500).unwrap(),
            Err(BiblioError::UnknownBibliography(_))
        ));
    }

    #[test]
    fn resolve_citations_unknown_key_propagates_error() {
        let statements = vec![
            stmt(
                PropKind::Bibliography,
                Some("refs"),
                &[],
                None,
                None,
                None,
                CRAZY_RICH_YAML,
                0,
            ),
            stmt(PropKind::Cite, None, &["nope"], None, None, None, "", 500),
        ];
        let results = resolve_citations(&statements);
        assert!(matches!(
            results.get(&500).unwrap(),
            Err(BiblioError::UnknownKey(_))
        ));
    }
}
