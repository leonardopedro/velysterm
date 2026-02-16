# Delta-SIRK: Analytic Interaction Net

**Delta-SIRK** is a Rust crate implementing a "Pure Spectral Projector" architecture for high-precision physical simulations. It leverages the GPU not for numerical approximation, but as a **Massively Parallel Algebraic Evaluator**.

## Core Philosophy

Traditional methods (FEM, Finite Difference) discretize space into a grid. **Delta-SIRK** works on the continuous manifold. It represents physical states as **Symbolic Parameter Sets** (e.g., width, center, and phase of a Gaussian wavepacket) and computes interactions using **Exact Analytic Inner Products** on the GPU.

### Key Features
1.  **Continuous & Exact**: No spatial grid. All integrals are computed using closed-form algebraic formulas in the shader.
2.  **Certified Error**: Uses the Hashimoto-Zolotarev theory to provide rigorous error bounds based on the subspace size ($m$) and shift density.
3.  **GPU-Accelerated**: Dispatches thousands of "Interaction Agents" to compute the Hamiltonian matrix elements in parallel.

## Architecture

### 1. The Agents (`Agent` struct)
The system is built around "Agents" that live on the GPU:
-   **StateVector**: Holds the index to a `SymbolicParams` entry (the data).
-   **ResolventOp**: Represents the action of $(H + i\gamma)^{-1}$.
-   **MetricScanner**: A mobile agent that "scans" a state vector to compute overlap integrals $\langle v_i | v_j \rangle$ or matrix elements $\langle v_i | H | v_j \rangle$.

### 2. The Data (`SymbolicParams` struct)
A `repr(C)` struct that encodes the ansatz function $f(x)$.
```rust
pub struct SymbolicParams {
    pub type_id: u32,     // e.g. 0=Gaussian
    pub alpha: f32,       // Decay / Width
    pub x0: f32,          // Center
    pub k0: f32,          // Momentum
    pub amp_real: f32,    // Amplitude (Re)
    pub amp_imag: f32,    // Amplitude (Im)
    // ... padding for alignment
}
```

### 3. The Kernel (`shader.wgsl`)
The compute shader contains the **Analytic Library**—a set of functions that return the exact value of integrals like $\int_{-\infty}^{\infty} f^*(x) g(x) dx$ based on the parameters of $f$ and $g$.

## Usage Tutorial

### Prerequisites
-   Rust (stable)
-   A GPU capable of running WGPU (Vulkan, Metal, DX12, or WebGPU).

### Compilation
The crate is part of the `velysterm` workspace. To build it:
```bash
cargo build -p delta_sirk
```

### Running Tests
We have included a verification test that runs the full pipeline (Host -> GPU -> Host) to compute the overlap of two symbolic functions.
```bash
cargo test -p delta_sirk
```
You should see output indicating `test tests::test_overlap ... ok`.

### extending the System
To adapt this for a specific physical problem (e.g., Quantum Chemistry, Fluid Dynamics):
1.  **Modify `SymbolicParams`**: Add fields relevant to your ansatz (e.g., polynomial coefficients, contraction weights).
2.  **Update `shader.wgsl`**: Implement the `analytic_overlap` and `analytic_matrix_element` functions with the specific integrals for your basis functions.
3.  **Implement `apply_symbolic_resolvent`**: In `src/lib.rs`, define how the operator $(H + i\gamma)^{-1}$ transforms your parameters.
