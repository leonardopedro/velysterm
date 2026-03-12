use crate::{Hamiltonian, InnerBosonicState, InnerFermionicState, Operator};
use num_complex::Complex64;
use pest::Parser;
use pest_derive::Parser;

#[derive(Parser)]
#[grammar_inline = r#"
WHITESPACE = _{ " " | "\t" | "\r" | "\n" }
expression = { term ~ (add_op ~ term)* }
add_op = { "+" | "-" }
term = { factor ~ (mult_op ~ factor)* }
mult_op = { "*" | "" }
factor = { primary ~ (power_op ~ exponent)? }
power_op = _{ "^" }
exponent = @{ ASCII_DIGIT+ }
primary = { number | variable | "(" ~ expression ~ ")" }
number = @{ ("+" | "-")? ~ ASCII_DIGIT+ ~ ("." ~ ASCII_DIGIT*)? }
variable = @{ (ASCII_ALPHA | "_") ~ (ASCII_ALPHANUMERIC | "_")* }
"#]
struct SymbolicParser;

/// A simple symbolic representation that can expand products and collect terms
#[derive(Debug, Clone)]
pub enum SymbolicExpr {
    Number(f64),
    Variable(String),
    Add(Vec<SymbolicExpr>),
    Mul(Vec<SymbolicExpr>),
    Pow(Box<SymbolicExpr>, u32),
}

impl SymbolicExpr {
    /// Expand out all products and powers into a sum of monomials: Sum[coeff * Var_i * Var_j ...]
    pub fn expand(&self) -> Vec<(f64, Vec<String>)> {
        match self {
            SymbolicExpr::Number(n) => vec![(*n, vec![])],
            SymbolicExpr::Variable(v) => vec![(1.0, vec![v.clone()])],
            SymbolicExpr::Add(terms) => {
                let mut result = Vec::new();
                for t in terms {
                    result.extend(t.expand());
                }
                result
            }
            SymbolicExpr::Mul(factors) => {
                let mut current = vec![(1.0, vec![])];
                for f in factors {
                    let expanded_f = f.expand();
                    let mut next = Vec::new();
                    for (c1, v1) in &current {
                        for (c2, v2) in &expanded_f {
                            let mut combined_v = v1.clone();
                            combined_v.extend(v2.iter().cloned());
                            next.push((c1 * c2, combined_v));
                        }
                    }
                    current = next;
                }
                current
            }
            SymbolicExpr::Pow(base, exp) => {
                let mut current = vec![(1.0, vec![])];
                let expanded_base = base.expand();
                for _ in 0..*exp {
                    let mut next = Vec::new();
                    for (c1, v1) in &current {
                        for (c2, v2) in &expanded_base {
                            let mut combined_v = v1.clone();
                            combined_v.extend(v2.iter().cloned());
                            next.push((c1 * c2, combined_v));
                        }
                    }
                    current = next;
                }
                current
            }
        }
    }
}

pub fn compile_to_fock(input: &str) -> Hamiltonian {
    // 1. Symbolic Parsing
    let pairs = SymbolicParser::parse(Rule::expression, input)
        .expect("Failed to parse expression")
        .next()
        .unwrap();
    let expr = parse_pairs(pairs);

    // 2. Algebraic Expansion
    let expanded = expr.expand();

    // 3. Mapping variables to Fock Operators
    let mut terms = Vec::new();
    for (coeff, vars) in expanded {
        let mut ops = Vec::new();
        // Standard Right-to-Left application: 1 * 2 * 3 -> ops = [3, 2, 1] applied sequentially.
        // Wait, the order in expanded list is Left-to-Right.
        // We will push them and apply from end to start in lib.rs.
        for v in vars {
            if let Some(op) = map_variable_to_op(&v) {
                ops.push(op);
            }
        }
        terms.push((Complex64::new(coeff, 0.0), ops));
    }

    Hamiltonian { terms }
}

fn map_variable_to_op(name: &str) -> Option<Operator> {
    let parse_coord = |prefix: &str, s: &str| -> Option<u32> {
        let suffix = s.trim_start_matches(prefix);
        suffix.parse().ok()
    };

    if name.starts_with("a_dag_") {
        let idx = name.trim_start_matches("a_dag_").parse().ok()?;
        Some(Operator::InnerBosonCreate(idx))
    } else if name.starts_with("a_") {
        let idx = name.trim_start_matches("a_").parse().ok()?;
        Some(Operator::InnerBosonAnnihilate(idx))
    } else if name.starts_with("c_dag_") {
        let idx = name.trim_start_matches("c_dag_").parse().ok()?;
        Some(Operator::InnerFermionCreate(idx))
    } else if name.starts_with("c_") {
        let idx = name.trim_start_matches("c_").parse().ok()?;
        Some(Operator::InnerFermionAnnihilate(idx))
    } else if name.starts_with("A_phi_dag_") {
        let mut modes = BTreeMap::new();
        modes.insert(parse_coord("A_phi_dag_", name)?, 1);
        Some(Operator::OuterBosonCreate(InnerBosonicState { modes }))
    } else if name == "A_phi_dag" {
        Some(Operator::OuterBosonCreate(InnerBosonicState::vacuum()))
    } else if name.starts_with("A_phi_") {
        let mut modes = BTreeMap::new();
        modes.insert(parse_coord("A_phi_", name)?, 1);
        Some(Operator::OuterBosonAnnihilate(InnerBosonicState { modes }))
    } else if name == "A_phi" {
        Some(Operator::OuterBosonAnnihilate(InnerBosonicState::vacuum()))
    } else if name.starts_with("C_phi_dag_") {
        let mut modes = BTreeSet::new();
        modes.insert(parse_coord("C_phi_dag_", name)?);
        Some(Operator::OuterFermionCreate(InnerFermionicState { modes }))
    } else if name == "C_phi_dag" {
        Some(Operator::OuterFermionCreate(InnerFermionicState::vacuum()))
    } else if name.starts_with("C_phi_") {
        let mut modes = BTreeSet::new();
        modes.insert(parse_coord("C_phi_", name)?);
        Some(Operator::OuterFermionAnnihilate(InnerFermionicState { modes }))
    } else if name == "C_phi" {
        Some(Operator::OuterFermionAnnihilate(InnerFermionicState::vacuum()))
    } else {
        None
    }
}

fn parse_pairs(pair: pest::iterators::Pair<Rule>) -> SymbolicExpr {
    match pair.as_rule() {
        Rule::expression => {
            let mut terms = Vec::new();
            let mut it = pair.into_inner();
            if let Some(first_term) = it.next() {
                terms.push(parse_pairs(first_term));
                while let (Some(op), Some(next_term)) =
                    (it.next(), it.next())
                {
                    let t = parse_pairs(next_term);
                    if op.as_str() == "-" {
                        terms.push(SymbolicExpr::Mul(vec![
                            SymbolicExpr::Number(-1.0),
                            t,
                        ]));
                    } else {
                        terms.push(t);
                    }
                }
            }
            if terms.len() == 1 {
                terms.pop().unwrap()
            } else {
                SymbolicExpr::Add(terms)
            }
        }
        Rule::term => {
            let mut factors = Vec::new();
            for part in pair.into_inner() {
                match part.as_rule() {
                    Rule::factor => factors.push(parse_pairs(part)),
                    _ => {}
                }
            }
            if factors.len() == 1 {
                factors.pop().unwrap()
            } else {
                SymbolicExpr::Mul(factors)
            }
        }
        Rule::factor => {
            let mut it = pair.into_inner();
            let primary = it.next().unwrap();
            let exponent = it
                .next()
                .map(|p| p.as_str().parse::<u32>().unwrap())
                .unwrap_or(1);
            let expr = parse_pairs(primary);
            if exponent == 1 {
                expr
            } else {
                SymbolicExpr::Pow(Box::new(expr), exponent)
            }
        }
        Rule::primary => {
            let inner = pair.into_inner().next().unwrap();
            match inner.as_rule() {
                Rule::number => SymbolicExpr::Number(
                    inner.as_str().parse().unwrap(),
                ),
                Rule::variable => {
                    SymbolicExpr::Variable(inner.as_str().to_string())
                }
                Rule::expression => parse_pairs(inner),
                _ => unreachable!(),
            }
        }
        _ => unreachable!("{:?}", pair.as_rule()),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_cas_expansion() {
        let h = compile_to_fock(
            "(A_phi_dag + C_phi_dag) * (A_phi + C_phi)",
        );
        // Should expand to: A_dag*A + A_dag*C + C_dag*A + C_dag*C
        assert_eq!(h.terms.len(), 4);
    }
}
