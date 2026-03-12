use fock_sirk::{build_hashimoto_subspace};
use nested_fock_algebra::{QuantumState, compile_to_fock, InnerBosonicState, InnerFermionicState, Operator};

fn main() {
    // 1. FRONTEND: Define Physics using the Symbolic CAS
    // Hamiltonian: H = (\sum a_dag_i * a_i) * C_phi_dag * C_phi
    // Represents the integral of quantum harmonic oscillators (inner Fock space) times a number operator (outer Fock space).
    let h_str = "(a_dag_0 * a_0 + a_dag_1 * a_1 + a_dag_2 * a_2) * C_phi_dag * C_phi";
    let hamiltonian = compile_to_fock(h_str);
    
    // 2. INITIAL STATE: Shifted vacuum in the multiverse
    // One universe in the vacuum state, plus one fermion at the vacuum.
    let initial_state = QuantumState::vacuum()
        .apply(&Operator::OuterBosonCreate(InnerBosonicState::vacuum()))
        .apply(&Operator::OuterFermionCreate(InnerFermionicState::vacuum()));

    println!("Initial State components: {}", initial_state.components.len());

    // 3. BACKEND: SIRK Spectral Projector
    let m_dim = 10;
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
    let t = 2.0;
    let evolved = sirk_result.time_evolve(t);
    println!("Evolved state components: {}", evolved.components.len());
}
