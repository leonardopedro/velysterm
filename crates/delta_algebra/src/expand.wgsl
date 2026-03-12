// === expand.wgsl — Pass 1: Hermite Recursion Expansion ===
//
// Each GPU invocation processes one HermiteState from `input_states` and
// applies a single OperatorTerm to it, writing the (possibly mutated or
// zeroed) result to `output_states`.
//
// Supported operators (op_type):
//   0 = Identity:    output ← input  (no mutation)
//   1 = Annihilation: a_i |n_i⟩ = sqrt(n_i)   |n_i-1⟩
//   2 = Creation:    a†_i|n_i⟩ = sqrt(n_i+1) |n_i+1⟩

struct HermiteState {
    n: array<u32, 4>,  // occupation numbers
    coeff_re: f32,
    coeff_im: f32,
    pad0: u32,
    pad1: u32,
    // total: 32 bytes
}

struct OperatorTerm {
    op_type:    u32,
    target_dim: u32,
    factor_re:  f32,
    factor_im:  f32,
}

@group(0) @binding(0) var<storage, read>       input_states: array<HermiteState>;
@group(0) @binding(1) var<storage, read_write> output_states: array<HermiteState>;
@group(0) @binding(2) var<uniform>             current_op: OperatorTerm;

fn complex_mul(a_re: f32, a_im: f32, b_re: f32, b_im: f32) -> vec2<f32> {
    return vec2<f32>(a_re * b_re - a_im * b_im, a_re * b_im + a_im * b_re);
}

@compute @workgroup_size(256)
fn apply_recursion(@builtin(global_invocation_id) id: vec3<u32>) {
    let idx = id.x;
    if (idx >= arrayLength(&input_states)) { return; }

    var state = input_states[idx];
    let dim   = current_op.target_dim;
    let n_val = state.n[dim];

    var multiplier = 1.0;
    var alive      = true;

    if (current_op.op_type == 1u) {
        // Annihilation: a |n> = sqrt(n) |n-1>
        if (n_val == 0u) {
            alive = false;
        } else {
            state.n[dim] = n_val - 1u;
            multiplier   = sqrt(f32(n_val));
        }
    } else if (current_op.op_type == 2u) {
        // Creation: a† |n> = sqrt(n+1) |n+1>
        state.n[dim] = n_val + 1u;
        multiplier   = sqrt(f32(n_val + 1u));
    }
    // op_type == 0: Identity — pass through unchanged (multiplier stays 1.0)

    if (alive) {
        let new_coeff = complex_mul(
            state.coeff_re, state.coeff_im,
            current_op.factor_re * multiplier,
            current_op.factor_im * multiplier,
        );
        state.coeff_re = new_coeff.x;
        state.coeff_im = new_coeff.y;
    } else {
        // Dead state: zero amplitude, keep n for the sort step
        state.coeff_re = 0.0;
        state.coeff_im = 0.0;
    }

    output_states[idx] = state;
}
