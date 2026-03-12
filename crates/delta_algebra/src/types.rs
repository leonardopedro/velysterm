use bytemuck::{Pod, Zeroable};

// ---------------------------------------------------------------------------
// HermiteState
// A single basis state in the multi-dimensional Hermite (Fock) basis:
//   |n0, n1, n2, n3⟩  with complex amplitude (coeff_re + i*coeff_im).
//
// Layout: 4×u32 + 2×f32 + 2×u32-pad = 32 bytes (16-byte aligned for WGSL).
// ---------------------------------------------------------------------------
#[repr(C)]
#[derive(Clone, Copy, Pod, Zeroable, Debug, PartialEq)]
pub struct HermiteState {
    /// Occupation numbers for each dimension.
    pub n: [u32; 4],
    /// Real part of the amplitude.
    pub coeff_re: f32,
    /// Imaginary part of the amplitude.
    pub coeff_im: f32,
    pub _pad0: u32,
    pub _pad1: u32,
}

impl HermiteState {
    /// Construct a state |n0,n1,n2,n3⟩ with a real amplitude.
    pub fn new(n: [u32; 4], coeff_re: f32, coeff_im: f32) -> Self {
        Self {
            n,
            coeff_re,
            coeff_im,
            _pad0: 0,
            _pad1: 0,
        }
    }

    /// Vacuum state |0,0,0,0⟩ with amplitude 1.
    pub fn vacuum() -> Self {
        Self::new([0; 4], 1.0, 0.0)
    }

    /// Returns a canonical sort key built from the four quantum numbers.
    /// Used for the CPU-side reduction step.
    #[inline]
    pub fn sort_key(&self) -> u128 {
        (self.n[0] as u128)
            | ((self.n[1] as u128) << 32)
            | ((self.n[2] as u128) << 64)
            | ((self.n[3] as u128) << 96)
    }
}

// ---------------------------------------------------------------------------
// OpType — the elementary operation type
// ---------------------------------------------------------------------------
#[repr(u32)]
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum OpType {
    Identity = 0,
    Annihilation = 1, // a_i   |n_i⟩ = sqrt(n_i)   |n_i-1⟩
    Creation = 2,     // a†_i  |n_i⟩ = sqrt(n_i+1) |n_i+1⟩
}

// ---------------------------------------------------------------------------
// OperatorTerm — one monomial in the Hamiltonian, e.g. α·a†_0·a_1
//
// The full Hamiltonian is a Vec<OperatorTerm>; they are applied sequentially
// (each term is a *separate* linear pass over the state buffer).
// ---------------------------------------------------------------------------
#[repr(C)]
#[derive(Clone, Copy, Pod, Zeroable, Debug)]
pub struct OperatorTerm {
    /// Which elementary operation (OpType as u32).
    pub op_type: u32,
    /// Which Fock-space dimension (0–3).
    pub target_dim: u32,
    /// Complex prefactor of this term.
    pub factor_re: f32,
    pub factor_im: f32,
}

impl OperatorTerm {
    pub fn new(
        op: OpType,
        dim: u32,
        factor_re: f32,
        factor_im: f32,
    ) -> Self {
        Self {
            op_type: op as u32,
            target_dim: dim,
            factor_re,
            factor_im,
        }
    }

    // -----------------------------------------------------------------------
    // Convenience constructors for common QHO operators
    // -----------------------------------------------------------------------

    /// Number operator N_i = a†_i a_i  →  represented as two sequential terms.
    /// Returns (annihilation, creation) to be applied in order.
    pub fn number_op(dim: u32) -> [Self; 2] {
        [
            Self::new(OpType::Annihilation, dim, 1.0, 0.0),
            Self::new(OpType::Creation, dim, 1.0, 0.0),
        ]
    }

    /// Position operator x_i = (1/√2)(a_i + a†_i).
    pub fn position(dim: u32, scale: f32) -> [Self; 2] {
        let f = scale / std::f32::consts::SQRT_2;
        [
            Self::new(OpType::Annihilation, dim, f, 0.0),
            Self::new(OpType::Creation, dim, f, 0.0),
        ]
    }

    /// Momentum operator p_i = (-i/√2)(a_i - a†_i).
    pub fn momentum(dim: u32, scale: f32) -> [Self; 4] {
        let f = scale / std::f32::consts::SQRT_2;
        [
            Self::new(OpType::Annihilation, dim, 0.0, -f), //  -i * a
            Self::new(OpType::Creation, dim, 0.0, f),      //  +i * a†
            // padding duplicates (identity, zero weight) to keep array len = 4
            Self::new(OpType::Identity, dim, 0.0, 0.0),
            Self::new(OpType::Identity, dim, 0.0, 0.0),
        ]
    }
}
