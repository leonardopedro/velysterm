// --- Analytic Library ---

struct SymbolicParams {
    type_id: u32,
    alpha: f32,
    x0: f32,
    k0: f32,
    amp_re: f32,
    amp_im: f32,
    padding: array<u32, 2>,
}

struct Agent {
    kind: u32,
    principal_port: u32,
    aux_port: u32,
    param_index: u32,
    shift_val: f32,
    padding: array<u32, 2>,
}

@group(0) @binding(0) var<storage, read> param_buffer: array<SymbolicParams>;
@group(0) @binding(1) var<storage, read> agents: array<Agent>;
@group(0) @binding(2) var<storage, read_write> output_matrix: array<vec2<f32>>; // Complex values

// 1. CLOSED-FORM OVERLAP <f | g>
// Computes integral(-inf, inf) of f(x)* g(x) dx analytically
fn analytic_overlap(f: SymbolicParams, g: SymbolicParams) -> vec2<f32> {
    // Example: Gaussian Overlap Formula
    // Integral of exp(-a(x-x1)^2) * exp(-b(x-x2)^2) is a known Gaussian integral.
    
    let a_sum = f.alpha + g.alpha;
    let dist = f.x0 - g.x0; // Assuming real centers for now
    
    // Analytic result (simplified for illustration):
    // Integral exp(-a x^2) = sqrt(pi/a)
    // Product of two Gaussians is a Gaussian.
    
    let prefactor = sqrt(3.14159265 / a_sum); 
    let exponent = - (f.alpha * g.alpha * dist * dist) / a_sum;
    
    let magnitude = prefactor * exp(exponent);
    
    // Complex multiplication of amplitudes
    // (a + bi)(c + di) = (ac - bd) + (ad + bc)i
    // We want <f|g> = integral f*(x) g(x) dx
    // So if params are for the function, we conjugate f's amplitude?
    // Let's assume params store the raw function parameters.
    // If f is the "bra", we should conjugate its amplitude.
    
    let f_re = f.amp_re;
    let f_im = -f.amp_im; // Conjugate for bra
    let g_re = g.amp_re;
    let g_im = g.amp_im;

    let re = (f_re * g_re - f_im * g_im) * magnitude;
    let im = (f_re * g_im + f_im * g_re) * magnitude;
    
    return vec2<f32>(re, im);
}

// 2. CLOSED-FORM HAMILTONIAN ELEMENT <f | H | g>
// H is the symbolic Unbounded Operator (e.g. -d^2/dx^2 + V(x))
fn analytic_matrix_element(f: SymbolicParams, g: SymbolicParams) -> vec2<f32> {
    // Dummy logic for now: just return overlap scaled
    // Real implementation would implement derivative rules
    let overlap = analytic_overlap(f, g);
    let scale = f.alpha * g.alpha; // Placeholder
    return overlap * scale;
}

@compute @workgroup_size(64)
fn main_algebra(@builtin(global_invocation_id) id: vec3<u32>) {
    // The Interaction:
    // A Scanner Agent (Row i) meets a State Agent (Column j)
    let idx = id.x;
    
    if (idx >= arrayLength(&agents)) {
        return;
    }

    let scanner = agents[idx];
    
    if (scanner.kind == 2u) { // MetricScanner
        let target_idx = scanner.principal_port;
        let state_vector = agents[target_idx];
        
        // Safety check indices
        if (scanner.param_index >= arrayLength(&param_buffer) || 
            state_vector.param_index >= arrayLength(&param_buffer)) {
            return;
        }

        let f = param_buffer[scanner.param_index];
        let g = param_buffer[state_vector.param_index];
        
        // Compute Exact Matrix Element
        // For H_m, we want <v_i | v_{j+1}>? Or <v_i | H | v_j>?
        // The resolvent step is implicit. 
        // If we are building H in the Krylov basis, we need <v_i | H | v_j>.
        // Let's assume this kernel computes one element.
        
        let val = analytic_matrix_element(f, g);
        
        // Write to H_m matrix
        let matrix_idx = scanner.aux_port; 
        if (matrix_idx < arrayLength(&output_matrix)) {
            output_matrix[matrix_idx] = val;
        }
    }
}
