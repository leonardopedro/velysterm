use unfer_protocol::{
    HamiltonianSpec, ModelSpec, PriorSpec, SolverSpec,
};
use serde_json::json;

pub fn parse_model(text: &str) -> Result<ModelSpec, String> {
    let trimmed = text.trim();
    if trimmed.starts_with("harmonic_chain") {
        Ok(ModelSpec {
            hamiltonian: HamiltonianSpec::builtin(
                "harmonic_chain",
                json!({ "n_modes": 1, "omega": 1.0 }),
            ),
            prior: PriorSpec::Vacuum,
            solver: SolverSpec::default(),
        })
    } else if let Some(latex) = trimmed.strip_prefix("latex\"") {
        let latex = latex.trim_end_matches('"');
        Ok(ModelSpec {
            hamiltonian: HamiltonianSpec::latex(latex),
            prior: PriorSpec::Vacuum,
            solver: SolverSpec::default(),
        })
    } else {
        Err("Unknown model syntax. Use 'harmonic_chain(...)' or 'latex\"...\"'".into())
    }
}

pub fn parse_event(text: &str) -> Result<String, String> {
    let trimmed = text.trim();
    if trimmed.contains("==") || trimmed.contains(">=") || trimmed.contains("<=") {
        Ok(r#"{"BosonModeTotal":{"mode":0,"cmp":"Eq","value":1}}"#.into())
    } else if trimmed == "vacuum" {
        Ok(r#"{"Vacuum":null}"#.into())
    } else {
        Ok(r#"{"BosonModeTotal":{"mode":0,"cmp":"Eq","value":1}}"#.into())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_harmonic_chain() {
        let spec = parse_model("harmonic_chain(g: 0.5)").unwrap();
        assert!(matches!(
            spec.hamiltonian,
            HamiltonianSpec::Builtin { .. }
        ));
    }

    #[test]
    fn parse_latex() {
        let spec = parse_model(r#"latex"a_dag a""#).unwrap();
        assert!(matches!(spec.hamiltonian, HamiltonianSpec::Latex { .. }));
    }

    #[test]
    fn parse_unknown_fails() {
        assert!(parse_model("unknown_model").is_err());
    }
}
