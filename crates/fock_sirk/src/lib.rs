use nested_fock_algebra::{QuantumState, Hamiltonian};
use num_complex::Complex64;

pub trait MatrixFreeOperator: Sync {
    fn apply(&self, x: &QuantumState) -> QuantumState;
    
    fn inner_product(a: &QuantumState, b: &QuantumState) -> Complex64 {
        QuantumState::inner_product(a, b)
    }

    fn scale_and_add(a: &mut QuantumState, b: &QuantumState, scale: Complex64) {
        a.scale_and_add(b, scale);
    }
    
    fn norm(a: &QuantumState) -> f64 {
        QuantumState::inner_product(a, a).re.sqrt()
    }
}

impl MatrixFreeOperator for Hamiltonian {
    fn apply(&self, x: &QuantumState) -> QuantumState {
        self.apply(x)
    }
}

pub fn solve_shift_invert(
    operator: &impl MatrixFreeOperator,
    v: &QuantumState,
    shift: Complex64,
    tol: f64
) -> QuantumState {
    let apply_a = |x: &QuantumState| {
        let mut h_x = operator.apply(x);
        <Hamiltonian as MatrixFreeOperator>::scale_and_add(&mut h_x, x, shift);
        h_x
    };

    // BiCGSTAB implementation
    let mut x = QuantumState { components: Default::default() };
    let mut r = v.clone(); 
    let r_hat = r.clone();
    
    let mut rho = Complex64::new(1.0, 0.0);
    let mut alpha = Complex64::new(1.0, 0.0);
    let mut omega = Complex64::new(1.0, 0.0);
    
    let mut v_vec = QuantumState { components: Default::default() };
    let mut p = QuantumState { components: Default::default() };
    
    let b_norm = <Hamiltonian as MatrixFreeOperator>::norm(v);
    if b_norm < 1e-18 {
        return x;
    }

    for _i in 0..1000 {
        let rho_prev = rho;
        rho = <Hamiltonian as MatrixFreeOperator>::inner_product(&r_hat, &r);
        
        if rho.norm() < 1e-24 { break; }
        
        let beta = (rho / rho_prev) * (alpha / omega);
        
        // p = r + beta * (p - omega * v)
        let mut next_p = r.clone();
        let mut p_term = p.clone();
        <Hamiltonian as MatrixFreeOperator>::scale_and_add(&mut p_term, &v_vec, -omega);
        <Hamiltonian as MatrixFreeOperator>::scale_and_add(&mut next_p, &p_term, beta);
        p = next_p;
        
        v_vec = apply_a(&p);
        
        let dot_rv = <Hamiltonian as MatrixFreeOperator>::inner_product(&r_hat, &v_vec);
        alpha = rho / dot_rv;
        
        // s = r - alpha * v
        let mut s = r.clone();
        <Hamiltonian as MatrixFreeOperator>::scale_and_add(&mut s, &v_vec, -alpha);
        
        if <Hamiltonian as MatrixFreeOperator>::norm(&s) < tol * b_norm {
            <Hamiltonian as MatrixFreeOperator>::scale_and_add(&mut x, &p, alpha);
            return x;
        }
        
        let t = apply_a(&s);
        
        let t_dot_s = <Hamiltonian as MatrixFreeOperator>::inner_product(&t, &s);
        let t_dot_t = <Hamiltonian as MatrixFreeOperator>::inner_product(&t, &t);
        omega = t_dot_s / t_dot_t;
        
        // x = x + alpha * p + omega * s
        <Hamiltonian as MatrixFreeOperator>::scale_and_add(&mut x, &p, alpha);
        <Hamiltonian as MatrixFreeOperator>::scale_and_add(&mut x, &s, omega);
        
        // r = s - omega * t
        let mut next_r = s;
        <Hamiltonian as MatrixFreeOperator>::scale_and_add(&mut next_r, &t, -omega);
        r = next_r;
        
        if <Hamiltonian as MatrixFreeOperator>::norm(&r) < tol * b_norm {
            return x;
        }
    }
    
    x
}

pub struct SirkResult {
    pub h_matrix: nalgebra::DMatrix<Complex64>,
    pub basis: Vec<QuantumState>,
    pub error_bound: f64,
}

impl SirkResult {
    pub fn time_evolve(&self, _t: f64) -> QuantumState {
        // Exponentiate the small m x m reduced Hamiltonian matrix
        // (Simplified for now - returns a linear combination of basis vectors)
        let mut res = QuantumState { components: Default::default() };
        if let Some(first) = self.basis.first() {
            res.scale_and_add(first, Complex64::new(1.0, 0.0));
        }
        res
    }
}

pub fn build_hashimoto_subspace(
    hamiltonian: &impl MatrixFreeOperator,
    v_real: QuantumState,
    m_dim: usize,
    base_shift: f64,
    step: f64
) -> SirkResult {
    let shifts: Vec<Complex64> = (1..=m_dim)
        .map(|j| Complex64::new(0.0, base_shift - step * (j as f64)))
        .collect();

    let mut basis = Vec::with_capacity(m_dim);
    let mut h_matrix = nalgebra::DMatrix::zeros(m_dim, m_dim);
    
    // Normalize initial vector
    let norm_v = <Hamiltonian as MatrixFreeOperator>::norm(&v_real);
    let mut v_curr = v_real.clone();
    for val in v_curr.components.values_mut() {
        *val /= norm_v;
    }
    basis.push(v_curr);

    for k in 0..m_dim {
        let current_shift = shifts[k];
        let v_k = &basis[k];

        // x = (H + shift)^-1 v_k
        let mut v_next = solve_shift_invert(hamiltonian, v_k, current_shift, 1e-9);

        // Modified Gram-Schmidt
        for j in 0..=k {
            let h_jk = <Hamiltonian as MatrixFreeOperator>::inner_product(&basis[j], &v_next);
            h_matrix[(j, k)] = h_jk;
            <Hamiltonian as MatrixFreeOperator>::scale_and_add(&mut v_next, &basis[j], -h_jk);
        }

        let norm = <Hamiltonian as MatrixFreeOperator>::norm(&v_next);
        if k + 1 < m_dim {
            h_matrix[(k + 1, k)] = Complex64::new(norm, 0.0);
            for val in v_next.components.values_mut() {
                *val /= norm;
            }
            basis.push(v_next);
        }
    }

    let error_bound = 2.0 * 11.08 * f64::exp(-step * m_dim as f64);

    SirkResult { h_matrix, basis, error_bound }
}
