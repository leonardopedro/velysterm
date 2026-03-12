use fock_sirk::build_hashimoto_subspace;
use nested_fock_algebra::{
    compile_expression, Expression, InnerBosonicState, Operator, QuantumState,
    symengine::quantum::operators::{position_operator, momentum_operator},
};

fn main() {
    // 1. Define Physics: Anharmonic Oscillator
    // H = p^2/2 + x^2/2 + 0.1 * x^4
    let x_lib = position_operator();
    let p_lib = momentum_operator();

    // Map library internal symbols "a"/"a_dag" to our "a_0"/"c_0"
    let x = x_lib
        .substitute(&Expression::symbol("a"), &Expression::symbol("a_0"))
        .substitute(&Expression::symbol("a_dag"), &Expression::symbol("c_0"));
    let p = p_lib
        .substitute(&Expression::symbol("a"), &Expression::symbol("a_0"))
        .substitute(&Expression::symbol("a_dag"), &Expression::symbol("c_0"));

    let lambda = 0.1;
    let h_expr = (p.clone().pow(&Expression::int(2)) / Expression::from(2.0)) 
               + (x.clone().pow(&Expression::int(2)) / Expression::from(2.0)) 
               + (Expression::from(lambda) * x.pow(&Expression::int(4)));

    println!("Anharmonic Oscillator Hamiltonian (Symbolic):");
    println!("  {}", h_expr);
    
    // 2. Compile to Nested Fock Operators
    let hamiltonian = compile_expression(h_expr);
    println!("Compiled results in {} terms.", hamiltonian.terms.len());
    for (i, (coeff, ops)) in hamiltonian.terms.iter().enumerate() {
        println!("  Term {}: coeff={:?}, ops={:?}", i, coeff, ops);
    }
    
    // Define initial state (Vacuum + 1 Bosonic universe)
    let initial_state = QuantumState::vacuum()
        .apply(&Operator::OuterBosonCreate(InnerBosonicState::vacuum()));

    // 3. Run the Hashimoto SIRK Solver
    let m_dim = 10;
    println!("Building Hashimoto subspace with m = {}", m_dim);
    
    let sirk_result = build_hashimoto_subspace(
        &hamiltonian, 
        initial_state, 
        m_dim, 
        150.0, // High enough gamma for spectral convergence
        2.0    // step
    );

    println!("Certified Spectral Error Bound: {:.3e}", sirk_result.error_bound);
    
    // 4. Time Evolution to check if particles were created
    let t = 0.5;
    let final_state = sirk_result.time_evolve(t);
    println!("Evolution for t = {} computed. Components: {}", t, final_state.components.len());
}
