use nested_fock_algebra::{QuantumState, compile_to_fock, InnerBosonicState, InnerFermionicState, Operator};
use fock_sirk::build_hashimoto_subspace;

fn main() {
    // 1. Define Physics using Symbolic Engine (CAS)
    // Hamiltonian: c_0_dag * c_0 + 0.5 * (A_phi_dag * A_phi)^2
    let h_str = "c_0_dag * c_0 + 0.5 * (A_phi_dag * A_phi)^2";
    println!("Symbolic Expression: {}", h_str);
    
    // 2. Compile to Nested Fock Operators
    let hamiltonian = compile_to_fock(h_str);
    
    // Define initial state (Vacuum + 1 Bosonic universe + 1 Fermionic universe)
    let initial_state = QuantumState::vacuum()
        .apply(&Operator::OuterBosonCreate(InnerBosonicState::vacuum()))
        .apply(&Operator::OuterFermionCreate(InnerFermionicState::vacuum()));

    // 3. Run the Hashimoto SIRK Solver
    let m_dim = 5; // Krylov dimension
    println!("Building Hashimoto subspace with m = {}", m_dim);
    
    let sirk_result = build_hashimoto_subspace(
        &hamiltonian, 
        initial_state, 
        m_dim, 
        50.0, // gamma base
        1.0   // step
    );

    println!("Certified Spectral Error Bound: {:.3e}", sirk_result.error_bound);
    
    // 4. Time Evolution
    let t = 2.0;
    let _final_state = sirk_result.time_evolve(t);
    println!("Evolution for t = {} computed.", t);
}
