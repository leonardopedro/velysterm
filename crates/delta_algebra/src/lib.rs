pub mod types;
pub use types::*;

use std::borrow::Cow;
use wgpu::util::DeviceExt;

/// GPU-accelerated Algebraic Evaluator for Hermite (Fock) basis sets.
pub struct DeltaAlgebraEngine {
    device: wgpu::Device,
    queue: wgpu::Queue,
    expand_pipeline: wgpu::ComputePipeline,
    bind_group_layout: wgpu::BindGroupLayout,
}

impl DeltaAlgebraEngine {
    /// Initialise a new GPU engine.
    pub async fn new() -> Self {
        let instance = wgpu::Instance::default();
        let adapter = instance
            .request_adapter(&wgpu::RequestAdapterOptions::default())
            .await
            .expect("Failed to find a suitable GPU adapter");

        let (device, queue) = adapter
            .request_device(&wgpu::DeviceDescriptor::default(), None)
            .await
            .expect("Failed to create WGPU device");

        let shader = device.create_shader_module(wgpu::ShaderModuleDescriptor {
            label: Some("Delta-Algebra Expansion Shader"),
            source: wgpu::ShaderSource::Wgsl(Cow::Borrowed(include_str!("expand.wgsl"))),
        });

        let bind_group_layout = device.create_bind_group_layout(&wgpu::BindGroupLayoutDescriptor {
            label: Some("Delta-Algebra Bind Group Layout"),
            entries: &[
                wgpu::BindGroupLayoutEntry {
                    binding: 0,
                    visibility: wgpu::ShaderStages::COMPUTE,
                    ty: wgpu::BindingType::Buffer {
                        ty: wgpu::BufferBindingType::Storage { read_only: true },
                        has_dynamic_offset: false,
                        min_binding_size: None,
                    },
                    count: None,
                },
                wgpu::BindGroupLayoutEntry {
                    binding: 1,
                    visibility: wgpu::ShaderStages::COMPUTE,
                    ty: wgpu::BindingType::Buffer {
                        ty: wgpu::BufferBindingType::Storage { read_only: false },
                        has_dynamic_offset: false,
                        min_binding_size: None,
                    },
                    count: None,
                },
                wgpu::BindGroupLayoutEntry {
                    binding: 2,
                    visibility: wgpu::ShaderStages::COMPUTE,
                    ty: wgpu::BindingType::Buffer {
                        ty: wgpu::BufferBindingType::Uniform,
                        has_dynamic_offset: false,
                        min_binding_size: None,
                    },
                    count: None,
                },
            ],
        });

        let pipeline_layout = device.create_pipeline_layout(&wgpu::PipelineLayoutDescriptor {
            label: Some("Delta-Algebra Pipeline Layout"),
            bind_group_layouts: &[&bind_group_layout],
            push_constant_ranges: &[],
        });

        let expand_pipeline = device.create_compute_pipeline(&wgpu::ComputePipelineDescriptor {
            label: Some("Delta-Algebra Expansion Pipeline"),
            layout: Some(&pipeline_layout),
            module: &shader,
            entry_point: "apply_recursion",
        });

        Self {
            device,
            queue,
            expand_pipeline,
            bind_group_layout,
        }
    }

    /// Applies the action of a Hamiltonian string on an initial state.
    ///
    /// If the Hamiltonian consists of multiple terms that should be summed,
    /// each term is computed independently and the results are merged.
    pub async fn apply_operator(
        &self,
        initial_states: &[HermiteState],
        operator_terms: &[OperatorTerm],
    ) -> Vec<HermiteState> {
        if initial_states.is_empty() || operator_terms.is_empty() {
            return initial_states.to_vec();
        }

        let mut all_results = Vec::new();

        // One pass per operator term (addition)
        for &op in operator_terms {
            let result = self.execute_monomial(initial_states, op).await;
            all_results.extend(result);
        }

        // Aggregate identical states (Merge-Sort-Reduce)
        self.aggregate_states(all_results)
    }

    /// Internal: Run a single operator term on a set of states.
    async fn execute_monomial(
        &self,
        input: &[HermiteState],
        op: OperatorTerm,
    ) -> Vec<HermiteState> {
        if input.is_empty() { return Vec::new(); }

        let input_size = (input.len() * std::mem::size_of::<HermiteState>()) as u64;
        
        let input_buf = self.device.create_buffer_init(&wgpu::util::BufferInitDescriptor {
            label: Some("Delta-Algebra Input Buffer"),
            contents: bytemuck::cast_slice(input),
            usage: wgpu::BufferUsages::STORAGE,
        });

        let output_buf = self.device.create_buffer(&wgpu::BufferDescriptor {
            label: Some("Delta-Algebra Output Buffer"),
            size: input_size,
            usage: wgpu::BufferUsages::STORAGE | wgpu::BufferUsages::COPY_SRC,
            mapped_at_creation: false,
        });

        let uniform_buf = self.device.create_buffer_init(&wgpu::util::BufferInitDescriptor {
            label: Some("Delta-Algebra Uniform Buffer"),
            contents: bytemuck::cast_slice(&[op]),
            usage: wgpu::BufferUsages::UNIFORM,
        });

        let bind_group = self.device.create_bind_group(&wgpu::BindGroupDescriptor {
            label: Some("Delta-Algebra Bind Group"),
            layout: &self.bind_group_layout,
            entries: &[
                wgpu::BindGroupEntry {
                    binding: 0,
                    resource: input_buf.as_entire_binding(),
                },
                wgpu::BindGroupEntry {
                    binding: 1,
                    resource: output_buf.as_entire_binding(),
                },
                wgpu::BindGroupEntry {
                    binding: 2,
                    resource: uniform_buf.as_entire_binding(),
                },
            ],
        });

        let mut encoder = self.device.create_command_encoder(&wgpu::CommandEncoderDescriptor {
            label: Some("Delta-Algebra Encoder"),
        });

        {
            let mut cpass = encoder.begin_compute_pass(&wgpu::ComputePassDescriptor {
                label: Some("Delta-Algebra Compute Pass"),
                timestamp_writes: None,
            });
            cpass.set_pipeline(&self.expand_pipeline);
            cpass.set_bind_group(0, &bind_group, &[]);
            let workgroups = (input.len() as u32 + 255) / 256;
            cpass.dispatch_workgroups(workgroups, 1, 1);
        }

        let staging_buf = self.device.create_buffer(&wgpu::BufferDescriptor {
            label: Some("Delta-Algebra Staging Buffer"),
            size: input_size,
            usage: wgpu::BufferUsages::MAP_READ | wgpu::BufferUsages::COPY_DST,
            mapped_at_creation: false,
        });

        encoder.copy_buffer_to_buffer(&output_buf, 0, &staging_buf, 0, input_size);
        self.queue.submit(Some(encoder.finish()));

        // Readback
        let buffer_slice = staging_buf.slice(..);
        let (sender, receiver) = tokio::sync::oneshot::channel();
        buffer_slice.map_async(wgpu::MapMode::Read, move |v| {
            sender.send(v).expect("Failed to send map result");
        });

        self.device.poll(wgpu::Maintain::Wait);
        receiver.await.expect("Failed to receive map result").expect("Buffer mapping failed");

        let data = buffer_slice.get_mapped_range();
        let result: Vec<HermiteState> = bytemuck::cast_slice(&data).to_vec();
        drop(data);
        staging_buf.unmap();

        // Filter out "dead" states (zero amplitude)
        result.into_iter()
              .filter(|s| s.coeff_re.abs() > 1e-12 || s.coeff_im.abs() > 1e-12)
              .collect()
    }

    /// Aggregate states with identical quantum numbers.
    fn aggregate_states(&self, mut states: Vec<HermiteState>) -> Vec<HermiteState> {
        if states.is_empty() {
            return states;
        }

        // Sort by quantum numbers n
        states.sort_by_key(|s| s.sort_key());

        let mut merged = Vec::new();
        if let Some(first) = states.first() {
            let mut current = *first;
            for next in states.iter().skip(1) {
                if next.n == current.n {
                    current.coeff_re += next.coeff_re;
                    current.coeff_im += next.coeff_im;
                } else {
                    if current.coeff_re.abs() > 1e-12 || current.coeff_im.abs() > 1e-12 {
                        merged.push(current);
                    }
                    current = *next;
                }
            }
            if current.coeff_re.abs() > 1e-12 || current.coeff_im.abs() > 1e-12 {
                merged.push(current);
            }
        }

        merged
    }

    /// Computes the exact inner product <Bra | Ket>.
    pub fn inner_product(&self, bra: &[HermiteState], ket: &[HermiteState]) -> (f32, f32) {
        // Simple sparse dot product: sum(bra_coeff* * ket_coeff)
        // Since basis is sorted, we can use dual pointers.
        let mut bra_sorted = bra.to_vec();
        let mut ket_sorted = ket.to_vec();
        bra_sorted.sort_by_key(|s| s.sort_key());
        ket_sorted.sort_by_key(|s| s.sort_key());

        let mut b_idx = 0;
        let mut k_idx = 0;
        let mut total_re = 0.0;
        let mut total_im = 0.0;

        while b_idx < bra_sorted.len() && k_idx < ket_sorted.len() {
            let b = &bra_sorted[b_idx];
            let k = &ket_sorted[k_idx];
            let b_key = b.sort_key();
            let k_key = k.sort_key();

            if b_key == k_key {
                // (br - i*bi) * (kr + i*ki) = (br*kr + bi*ki) + i*(br*ki - bi*kr)
                total_re += b.coeff_re * k.coeff_re + b.coeff_im * k.coeff_im;
                total_im += b.coeff_re * k.coeff_im - b.coeff_im * k.coeff_re;
                b_idx += 1;
                k_idx += 1;
            } else if b_key < k_key {
                b_idx += 1;
            } else {
                k_idx += 1;
            }
        }

        (total_re, total_im)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn test_creation_annihilation() {
        let engine = DeltaAlgebraEngine::new().await;
        
        // Initial state |0,0,0,0>
        let vac = vec![HermiteState::vacuum()];
        
        // 1. Apply a†_0: |0> -> |1>
        let op_create = vec![OperatorTerm::new(OpType::Creation, 0, 1.0, 0.0)];
        let state_1 = engine.apply_operator(&vac, &op_create).await;
        
        assert_eq!(state_1.len(), 1);
        assert_eq!(state_1[0].n, [1, 0, 0, 0]);
        // amp = sqrt(0+1) * 1.0 = 1.0
        assert!((state_1[0].coeff_re - 1.0).abs() < 1e-6);

        // 2. Apply a_0 on |1>: |1> -> |0>
        let op_annihilate = vec![OperatorTerm::new(OpType::Annihilation, 0, 1.0, 0.0)];
        let state_0 = engine.apply_operator(&state_1, &op_annihilate).await;
        
        assert_eq!(state_0.len(), 1);
        assert_eq!(state_0[0].n, [0, 0, 0, 0]);
        // amp = 1.0 * sqrt(1) * 1.0 = 1.0
        assert!((state_0[0].coeff_re - 1.0).abs() < 1e-6);
        
        // 3. Apply a_0 on |0>: should be annihilated (empty result)
        let state_null = engine.apply_operator(&state_0, &op_annihilate).await;
        assert!(state_null.is_empty());
    }

    #[tokio::test]
    async fn test_superposition_and_inner_product() {
        let engine = DeltaAlgebraEngine::new().await;
        
        // |psi> = 1*|1,0,0,0> + 2i*|0,1,0,0>
        let psi = vec![
            HermiteState::new([1, 0, 0, 0], 1.0, 0.0),
            HermiteState::new([0, 1, 0, 0], 0.0, 2.0),
        ];
        
        // <psi|psi> = 1*1 + 2*2 = 5
        let (norm_re, norm_im) = engine.inner_product(&psi, &psi);
        assert!((norm_re - 5.0).abs() < 1e-6);
        assert!(norm_im.abs() < 1e-6);
        
        // Test position operator x_0 = (1/sqrt(2))(a_0 + a_0†)
        // x_0 |0,0,0,0> = (1/sqrt(2)) |1,0,0,0>
        let vac = vec![HermiteState::vacuum()];
        let x0_terms = OperatorTerm::position(0, 1.0);
        let x_vac = engine.apply_operator(&vac, &x0_terms).await;
        
        assert_eq!(x_vac.len(), 1);
        assert_eq!(x_vac[0].n, [1, 0, 0, 0]);
        let expected_amp = 1.0 / 2.0_f32.sqrt();
        assert!((x_vac[0].coeff_re - expected_amp).abs() < 1e-6);
    }
}
