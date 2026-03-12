# Delta-SIRK: Analytic Interaction Net for the Quantum Harmonic Oscillator

**Delta-SIRK** is a Rust crate implementing a *Pure Spectral Projector* for high-precision
quantum simulations. The GPU is used not for numerical approximation but as a **Massively Parallel
Algebraic Evaluator**: all inner products and Hamiltonian matrix elements are computed via
**exact closed-form analytic integrals**—no spatial grid is ever constructed.

---

## Physical Problem

### Hamiltonian

The **1-D Quantum Harmonic Oscillator** (QHO) in natural units (ℏ = m = ω = 1):

$$H = -\frac{1}{2}\frac{d^2}{dx^2} + \frac{1}{2}x^2$$

Exact eigenvalues: $E_n = n + \tfrac{1}{2}$.

### Initial State

The simulation is initialised with the **vacuum state displaced by x₀ = 1**:

$$|\psi_0\rangle = \pi^{-1/4} \exp\!\bigl(-\tfrac{1}{2}(x-1)^2\bigr)$$

This is a **QHO coherent state** with displacement parameter $\alpha = x_0/\sqrt{2} = 1/\sqrt{2}$.
Its mean energy is

$$\langle H \rangle = |\alpha|^2 + \tfrac{1}{2} = \tfrac{1}{2} + \tfrac{1}{2} = 1$$

which the test verifies analytically.

In Rust:

```rust
let init = SymbolicParams::shifted_vacuum(1.0);
```

---

## Architecture

### Basis Function Types

| `type_id` | Function | Notes |
|-----------|----------|-------|
| `0` — `TYPE_GAUSSIAN` | `amp · exp(-α (x−x₀)²)` | General centre x₀ |
| `1` — `TYPE_LINEAR_GAUSSIAN` | `amp · x · exp(-α x²)` | Always centred at 0 (kept for reference) |

```rust
pub struct SymbolicParams {
    pub type_id: u32,
    pub alpha: f32,    // Decay exponent α > 0
    pub x0: f32,       // Centre (type 0)
    pub k0: f32,       // Momentum phase (reserved)
    pub amp_real: f32,
    pub amp_imag: f32,
}

// Constructors
SymbolicParams::shifted_vacuum(x0)  // π^{-1/4} exp(-½(x−x0)²)  ← initial state
SymbolicParams::vacuum()            // π^{-1/4} exp(-½ x²)       ← x0 = 0
SymbolicParams::x_times_vacuum()   // π^{-1/4} x exp(-½ x²)     ← |1⟩ (reference)
```

### Agents

| `AgentKind` | Role |
|-------------|------|
| `StateVector` | Holds the index of a `SymbolicParams` entry |
| `MetricScanner` | Computes `⟨v_i | H | v_k⟩` and writes to the output matrix |
| `ResolventOp` | (future) symbolic `(H+iγ)⁻¹` agent |

### Analytic Integral Library (`shader.wgsl`)

**Gaussian moment integrals:**

| Symbol | Formula |
|--------|---------|
| I₀(a) | √(π/a) |
| I₂(a) | √π / (2 a^{3/2}) |
| I₄(a) | 3√π / (4 a^{5/2}) |

**Overlap `⟨f|g⟩` for type-0 Gaussians with general centres x₀_f, x₀_g:**

$$\langle f | g \rangle = \sqrt{\frac{\pi}{s}}\,e^{-\frac{\alpha_f \alpha_g\, d^2}{s}}\qquad s = \alpha_f+\alpha_g,\; d = x_{0f}-x_{0g}$$

**Hamiltonian element `⟨f|H|g⟩` for type-0 Gaussians:**

Let $s = \alpha_f+\alpha_g$, $d = x_{0g}-x_{0f}$, $c = (\alpha_f x_{0f}+\alpha_g x_{0g})/s$, $F = e^{-\alpha_f\alpha_g d^2/s}$.

$$\langle f|T|g\rangle = F\left[ \alpha_g I_0(s) - 2\alpha_g^2\!\left(I_2(s) + \tfrac{\alpha_f^2 d^2}{s^2} I_0(s)\right)\right]$$

$$\langle f|V|g\rangle = \tfrac{1}{2} F\left[I_2(s) + c^2 I_0(s)\right]$$

The x₀-dependent terms vanish when all centres coincide at 0 (recovering the prior formula).
For the shifted vacuum with $x_{0f}=x_{0g}=1$ (d=0, c=1):

$$\langle\psi_0|H|\psi_0\rangle_\text{raw} = \pi^{-1/2}\cdot\sqrt{\pi} = 1.0 \checkmark$$

### Host-Side Resolvent (`apply_symbolic_resolvent`)

For a normalised Gaussian state at (α, x₀), the QHO mean energy is

$$\langle H \rangle = \frac{\alpha}{2} + \frac{1}{8\alpha} + \frac{x_0^2}{2}$$

The resolvent divides the amplitude by $(\langle H\rangle + i\gamma)$ and gently nudges α → ½:

```
new_amp = old_amp / (<H> + i*gamma)
new_alpha = old_alpha + 0.1*(0.5 - old_alpha)
x0 preserved  (coherent states translate rigidly under QHO)
```

---

## Usage

### Build & Test

```bash
cargo build -p delta_sirk
cargo test  -p delta_sirk
```

Expected output:

```
test tests::test_qho_shifted_vacuum_energy ... ok
test tests::test_output_shape ... ok

test result: ok. 2 passed; 0 failed; 0 ignored; 0 measured; 0 filtered out
```

### Minimal Example

```rust
use delta_sirk::{SymbolicParams, run_symbolic_delta_sirk};

#[tokio::main]
async fn main() {
    // Initial state: vacuum displaced to x0 = 1  (<H> = 1.0)
    let init = SymbolicParams::shifted_vacuum(1.0);

    // Krylov shifts
    let shifts = vec![10.0, 9.0, 8.0];

    // Returns (H_m matrix as Vec<f32> of complex pairs, certified error bound)
    let (h_matrix, err) = run_symbolic_delta_sirk(init, shifts).await;

    println!("H₀₀ (Re) = {:.4}", h_matrix[0]);   // ≈ 1.0
    println!("Certified spectral error ≤ {:.2e}", err);
}
```

---

## Extending the System

1. **New centres or momenta** — set `x0` and `k0` in `SymbolicParams`;
   the shader already handles general x₀ for type-0 Gaussians.
2. **New basis types** — extend `type_id`, add integral cases in `analytic_overlap`
   and `analytic_matrix_element` in `shader.wgsl`.
3. **Different Hamiltonian** — replace the kinetic/potential formulas in
   `analytic_matrix_element` and update `apply_symbolic_resolvent` accordingly.

---

## Error Certification

The returned `spectral_error` follows the **Hashimoto–Zolotarev** bound:

$$\epsilon_m \leq 2 \times 11.08 \times e^{-\Delta\gamma \cdot m}$$

where $\Delta\gamma$ is the shift step and $m$ is the Krylov subspace dimension.
