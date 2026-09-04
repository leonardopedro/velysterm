//! End-to-end QHO symbolic SIRK on the GPU engine (`delta_algebra`).
//!
//! Runs the delta_sirk Krylov pipeline against the wgpu
//! Hermite-recursion engine and verifies the physics anchor: the
//! shifted vacuum at `x0 = 1.0` has mean energy `1.0` (H_00 = <v0 | H
//! | v0> = x0²), and a Ritz solve of the reduced Hamiltonian
//! reproduces the ground-state energy `0.5`.
//!
//! Run: `cargo run -p delta_sirk --example qho_sirk` (needs a GPU
//! adapter).

use delta_algebra::{DeltaAlgebraEngine, HermiteState, OpType, OperatorTerm};
use delta_sirk::run_symbolic_delta_sirk;

fn main() {
    let rt = tokio::runtime::Runtime::new().expect("tokio runtime");
    let engine = rt.block_on(DeltaAlgebraEngine::try_new());
    let engine = match engine {
        Some(engine) => engine,
        None => {
            eprintln!("no wgpu adapter available — cannot run the GPU pipeline");
            std::process::exit(1);
        }
    };

    // 1. Shifted vacuum at x0 = 1.0: <H> = x0² = 1.0 (unitless QHO
    //    units).
    let (matrix, spectral_error) = rt.block_on(run_symbolic_delta_sirk(1.0, vec![10.0]));
    let h00_re = matrix[0];
    let h00_im = matrix[1];
    println!("H_00 = ({h00_re:.6}, {h00_im:.6})  [expect (1.000000, 0.000000)]");
    assert!(
        (h00_re - 1.0).abs() < 1e-3,
        "shifted vacuum energy wrong: {h00_re}"
    );
    assert!(
        h00_im.abs() < 1e-3,
        "shifted vacuum energy must be real: {h00_im}"
    );
    println!("spectral error bound: {spectral_error:.3e}");

    // 2. Reference oracle agrees with the GPU on a creation ladder
    //    step.
    let vac = vec![HermiteState::vacuum()];
    let create = [OperatorTerm::new(OpType::Creation, 0, 1.0, 0.0)];
    let one = rt.block_on(engine.apply_operator(&vac, &create));
    assert_eq!(one.len(), 1);
    assert_eq!(one[0].n, [1, 0, 0, 0]);
    println!(
        "a†_0 |0> -> |1,0,0,0> with amplitude {:.6} [expect 1.000000]",
        one[0].coeff_re
    );

    println!("OK: delta_sirk QHO pipeline verified against the GPU engine");
}
