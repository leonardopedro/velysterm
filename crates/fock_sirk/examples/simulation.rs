use fock_sirk::build_hashimoto_subspace;
use nested_fock_algebra::{
    compile_expression, Expression, InnerBosonicState, InnerFermionicState, Operator, QuantumState,
    symengine::quantum::operators::{annihilation_mode, creation_mode},
};

fn main() {
    // 1. FRONTEND: Define Physics using the Symbolic Engine
    // We can now build the Hamiltonian programmatically using the symengine.
    let mut sum_n = Expression::zero();
    for i in 0..3 {
        // Build sum(c_i * a_i)
        sum_n = sum_n + creation_mode(i) * annihilation_mode(i);
    }

    // Outer space operators for the interaction
    let c_f0 = Expression::symbol("C_f0");
    let a_f0 = Expression::symbol("A_f0");

    let h_expr = c_f0 * sum_n * a_f0;
    
    println!("Hamiltonian Expression: {}", h_expr);
    let hamiltonian = compile_expression(h_expr);
    
    // 2. INITIAL STATE: vacuum in the multiverse then populate
    let initial_state = QuantumState::vacuum()
        .apply(&Operator::OuterBosonCreate(InnerBosonicState::vacuum()))
        .apply(&Operator::OuterFermionCreate(InnerFermionicState::vacuum()));

    println!("Initial State components: {}", initial_state.components.len());

    // 3. BACKEND: SIRK Spectral Projector
    let m_dim = 15;
    let sirk_result = build_hashimoto_subspace(
        &hamiltonian,
        initial_state,
        m_dim,
        100.0,
        2.0
    );

    println!("Krylov subspace built. Reduced matrix size: {}x{}", 
        sirk_result.h_matrix.nrows(), sirk_result.h_matrix.ncols());
    println!("Certified Spectral Error Bound: {:.3e}", sirk_result.error_bound);

    // 4. EVOLUTION
    let t = 1.0;
    let evolved = sirk_result.time_evolve(t);
    println!("Evolved state components: {}", evolved.components.len());
}
