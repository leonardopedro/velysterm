//! CPU reference oracle for the GPU Hermite-recursion evaluator.
//!
//! This is the slow-but-correct reference implementation of the exact
//! WGSL semantics in `expand.wgsl` plus the host-side merge step,
//! kept deliberately simple so the GPU path can be differentially
//! tested against it (the "correctness oracle first" rule: never
//! optimize against an unverified implementation). It mirrors the
//! inner-ladder Hermite recursion of `unfer/nested_fock_algebra`:
//!
//! ```text
//! a_i  |n_i⟩ = sqrt(n_i)   |n_i − 1⟩
//! a†_i |n_i⟩ = sqrt(n_i+1) |n_i + 1⟩
//! ```
//!
//! All arithmetic is `f32` to match the WGSL shader bit-for-bit at
//! the tolerance used by the differential tests.

use crate::types::{HermiteState, OperatorTerm};

/// Apply one [`OperatorTerm`] to one state (the WGSL
/// `apply_recursion` body).
///
/// Returns `None` for a "dead" state (annihilation on the vacuum: the
/// shader zeroes the amplitude and the host filters it out).
fn apply_term_to_state(
    state: HermiteState,
    op: &OperatorTerm,
) -> Option<HermiteState> {
    let mut out = state;
    let dim = op.target_dim as usize;
    if dim >= out.n.len() {
        // Out-of-range dimension: the shader indexes `state.n[dim]`
        // (UB in WGSL for dim >= 4); the reference treats it
        // as identity so the differential test can also cover
        // malformed input.
        return Some(out);
    }
    let n_val = out.n[dim];

    let mut multiplier = 1.0f32;
    let mut alive = true;
    match op.op_type {
        1 => {
            // Annihilation: a |n> = sqrt(n) |n-1>
            if n_val == 0 {
                alive = false;
            } else {
                out.n[dim] = n_val - 1;
                multiplier = (n_val as f32).sqrt();
            }
        }
        2 => {
            // Creation: a† |n> = sqrt(n+1) |n+1>
            out.n[dim] = n_val + 1;
            multiplier = ((n_val + 1) as f32).sqrt();
        }
        _ => {
            // Identity (op_type == 0): pass through unchanged.
        }
    }

    if !alive {
        return None;
    }

    // complex_mul(coeff, factor * multiplier) — mirrors the shader.
    // Both components are computed from the *original*
    // coefficients (the shader reads `state.coeff_re`/`state.
    // coeff_im` before writing `output_states`).
    let c_re = out.coeff_re;
    let c_im = out.coeff_im;
    let f_re = op.factor_re * multiplier;
    let f_im = op.factor_im * multiplier;
    out.coeff_re = c_re * f_re - c_im * f_im;
    out.coeff_im = c_re * f_im + c_im * f_re;
    Some(out)
}

/// The reference `apply_operator`: one pass per term, then
/// merge-sort-reduce.
///
/// Mirrors `DeltaAlgebraEngine::apply_operator` (per-term monomial
/// passes, aggregation of identical states, drop of zero-amplitude
/// states).
pub fn apply_operator_reference(
    initial_states: &[HermiteState],
    operator_terms: &[OperatorTerm],
) -> Vec<HermiteState> {
    if initial_states.is_empty() || operator_terms.is_empty() {
        return initial_states.to_vec();
    }

    let mut all_results = Vec::new();
    for op in operator_terms {
        for &s in initial_states {
            if let Some(applied) = apply_term_to_state(s, op) {
                all_results.push(applied);
            }
        }
    }
    aggregate_states(all_results)
}

/// Merge-sort-reduce: sort by quantum numbers, sum identical states,
/// drop zero-amplitude states. Mirrors
/// `DeltaAlgebraEngine::aggregate_states`.
pub fn aggregate_states(
    mut states: Vec<HermiteState>,
) -> Vec<HermiteState> {
    if states.is_empty() {
        return states;
    }
    states.sort_by_key(|s| s.sort_key());

    let mut merged = Vec::new();
    let mut current = states[0];
    for next in states.into_iter().skip(1) {
        if next.n == current.n {
            current.coeff_re += next.coeff_re;
            current.coeff_im += next.coeff_im;
        } else {
            if current.coeff_re.abs() > 1e-12
                || current.coeff_im.abs() > 1e-12
            {
                merged.push(current);
            }
            current = next;
        }
    }
    if current.coeff_re.abs() > 1e-12
        || current.coeff_im.abs() > 1e-12
    {
        merged.push(current);
    }
    merged
}

/// Reference inner product `<bra | ket>` (mirrors
/// `DeltaAlgebraEngine::inner_product`, but on the CPU).
pub fn inner_product_reference(
    bra: &[HermiteState],
    ket: &[HermiteState],
) -> (f32, f32) {
    let mut bra_sorted = bra.to_vec();
    let mut ket_sorted = ket.to_vec();
    bra_sorted.sort_by_key(|s| s.sort_key());
    ket_sorted.sort_by_key(|s| s.sort_key());

    let mut b_idx = 0;
    let mut k_idx = 0;
    let mut total_re = 0.0f32;
    let mut total_im = 0.0f32;

    while b_idx < bra_sorted.len() && k_idx < ket_sorted.len() {
        let b = bra_sorted[b_idx];
        let k = ket_sorted[k_idx];
        let b_key = b.sort_key();
        let k_key = k.sort_key();
        if b_key == k_key {
            total_re +=
                b.coeff_re * k.coeff_re + b.coeff_im * k.coeff_im;
            total_im +=
                b.coeff_re * k.coeff_im - b.coeff_im * k.coeff_re;
            b_idx += 1;
            k_idx += 1;
        } else if b_key < k_key {
            b_idx += 1;
        } else {
            k_idx += 1;
        }
    }
    (total_re, total_im)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::types::{HermiteState, OpType};

    #[test]
    fn reference_creation_annihilation_roundtrip() {
        let vac = vec![HermiteState::vacuum()];
        let create =
            [OperatorTerm::new(OpType::Creation, 0, 1.0, 0.0)];
        let one = apply_operator_reference(&vac, &create);
        assert_eq!(one.len(), 1);
        assert_eq!(one[0].n, [1, 0, 0, 0]);
        assert!((one[0].coeff_re - 1.0).abs() < 1e-6);

        let annihilate =
            [OperatorTerm::new(OpType::Annihilation, 0, 1.0, 0.0)];
        let zero = apply_operator_reference(&one, &annihilate);
        assert_eq!(zero.len(), 1);
        assert_eq!(zero[0].n, [0, 0, 0, 0]);

        // Annihilating the vacuum yields nothing.
        let dead = apply_operator_reference(&vac, &annihilate);
        assert!(dead.is_empty());
    }

    #[test]
    fn reference_merges_identical_states() {
        // (a†_0 + a†_1) |0> = |1,0> + |0,1>  (two distinct states)
        let vac = vec![HermiteState::vacuum()];
        let terms = [
            OperatorTerm::new(OpType::Creation, 0, 1.0, 0.0),
            OperatorTerm::new(OpType::Creation, 1, 1.0, 0.0),
        ];
        let out = apply_operator_reference(&vac, &terms);
        assert_eq!(out.len(), 2);

        // 2·a†_0 |0> = 2|1,0> — both passes hit the same state,
        // amplitudes sum.
        let double = [
            OperatorTerm::new(OpType::Creation, 0, 1.0, 0.0),
            OperatorTerm::new(OpType::Creation, 0, 1.0, 0.0),
        ];
        let out = apply_operator_reference(&vac, &double);
        assert_eq!(out.len(), 1);
        assert!((out[0].coeff_re - 2.0).abs() < 1e-6);
    }

    #[test]
    fn reference_sqrt_factors_match_boson_ladder() {
        // a† |2> = sqrt(3) |3>; a |2> = sqrt(2) |1>
        let two = vec![HermiteState::new([2, 0, 0, 0], 1.0, 0.0)];
        let create =
            [OperatorTerm::new(OpType::Creation, 0, 1.0, 0.0)];
        let three = apply_operator_reference(&two, &create);
        assert!((three[0].coeff_re - 3.0f32.sqrt()).abs() < 1e-6);

        let annihilate =
            [OperatorTerm::new(OpType::Annihilation, 0, 1.0, 0.0)];
        let one = apply_operator_reference(&two, &annihilate);
        assert!((one[0].coeff_re - 2.0f32.sqrt()).abs() < 1e-6);
    }

    #[test]
    fn reference_complex_prefactor() {
        // (i·a†) |0> = i |1>
        let vac = vec![HermiteState::vacuum()];
        let terms =
            [OperatorTerm::new(OpType::Creation, 0, 0.0, 1.0)];
        let out = apply_operator_reference(&vac, &terms);
        assert!((out[0].coeff_re).abs() < 1e-6);
        assert!((out[0].coeff_im - 1.0).abs() < 1e-6);
    }

    #[test]
    fn reference_inner_product_matches_dot() {
        let psi = vec![
            HermiteState::new([1, 0, 0, 0], 1.0, 0.0),
            HermiteState::new([0, 1, 0, 0], 0.0, 2.0),
        ];
        let (re, im) = inner_product_reference(&psi, &psi);
        assert!((re - 5.0).abs() < 1e-6);
        assert!(im.abs() < 1e-6);
    }
}
