use bytemuck::{Pod, Zeroable};
use std::borrow::Cow;
use wgpu::util::DeviceExt;

// --- Symbolic State Descriptor ---
// Defines a continuous function f(x) via parameters.
// Example: f(x) = c * exp(-alpha * (x - x0)^2 + i * k0 * x)
#[repr(C)]
#[derive(Debug, Clone, Copy, Pod, Zeroable)]
pub struct SymbolicParams {
    pub type_id: u32, // 0=Gaussian, 1=Polynomial-Gaussian, etc.
    pub alpha: f32,   // Width / Decay
    pub x0: f32,      // Center
    pub k0: f32,      // Momentum
    pub amp_real: f32, // Amplitude (Re)
    pub amp_imag: f32, // Amplitude (Im)
    pub padding: [u32; 2], // Alignment
}

// --- The Delta-Net Agent ---
#[repr(u32)]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AgentKind {
    Free = 0,
    // THE GENERATOR: Applies (H + ic)^-1 symbolically
    ResolventOp = 1,
    // THE SCANNER: Computes <v_i | v_j> or <v_i | H | v_j>
    MetricScanner = 2,
    // THE DATA: Holds a SymbolicParams index
    StateVector = 3,
}

#[repr(C)]
#[derive(Debug, Clone, Copy, Pod, Zeroable)]
pub struct Agent {
    pub kind: u32,
    pub principal_port: u32, // Target Agent Index
    pub aux_port: u32,       // Secondary Target

    // Payload
    pub param_index: u32, // Index into SymbolicParams buffer
    pub shift_val: f32,   // The 'c' in (H + ic)

    pub padding: [u32; 2],
}

// --- Helper to apply symbolic resolvent (Host Side) ---
// This is where the "Algebra" happens on the CPU for the ansatz
fn apply_symbolic_resolvent(
    prev: &SymbolicParams,
    shift: f32,
) -> SymbolicParams {
    // Placeholder logic:
    // In a real implementation, this would compute the parameters of (H + i*shift)^-1 * prev
    // For a Gaussian exp(-a x^2), the resolvent might approximate to another Gaussian
    // with modified width and amplitude.
    let mut next = *prev;
    next.alpha *= 1.1; // Dummy modification
    next.amp_real *= 0.9;
    next.amp_imag += 0.1 * shift;
    next
}

pub async fn run_symbolic_delta_sirk(
    initial_params: SymbolicParams,
    shifts: Vec<f32>,
) -> (Vec<f32>, f32) {
    let m = shifts.len();

    // --- 1. Setup WGPU ---
    let instance = wgpu::Instance::default();
    let adapter = instance
        .request_adapter(&wgpu::RequestAdapterOptions::default())
        .await
        .unwrap();
    let (device, queue) = adapter
        .request_device(&wgpu::DeviceDescriptor::default(), None)
        .await
        .unwrap();

    let shader =
        device.create_shader_module(wgpu::ShaderModuleDescriptor {
            label: Some("Delta-SIRK Shader"),
            source: wgpu::ShaderSource::Wgsl(Cow::Borrowed(
                include_str!("shader.wgsl"),
            )),
        });

    // --- 2. Prepare Data ---
    let mut basis_params = vec![initial_params];
    // Buffers that will be resized/updated
    // We need a bind group layout
    let bind_group_layout = device.create_bind_group_layout(
        &wgpu::BindGroupLayoutDescriptor {
            label: Some("Bind Group Layout"),
            entries: &[
                // Binding 0: Param Buffer (ReadOnly)
                wgpu::BindGroupLayoutEntry {
                    binding: 0,
                    visibility: wgpu::ShaderStages::COMPUTE,
                    ty: wgpu::BindingType::Buffer {
                        ty: wgpu::BufferBindingType::Storage {
                            read_only: true,
                        },
                        has_dynamic_offset: false,
                        min_binding_size: None,
                    },
                    count: None,
                },
                // Binding 1: Agent Buffer (ReadOnly)
                wgpu::BindGroupLayoutEntry {
                    binding: 1,
                    visibility: wgpu::ShaderStages::COMPUTE,
                    ty: wgpu::BindingType::Buffer {
                        ty: wgpu::BufferBindingType::Storage {
                            read_only: true,
                        },
                        has_dynamic_offset: false,
                        min_binding_size: None,
                    },
                    count: None,
                },
                // Binding 2: Output Matrix (ReadWrite)
                wgpu::BindGroupLayoutEntry {
                    binding: 2,
                    visibility: wgpu::ShaderStages::COMPUTE,
                    ty: wgpu::BindingType::Buffer {
                        ty: wgpu::BufferBindingType::Storage {
                            read_only: false,
                        },
                        has_dynamic_offset: false,
                        min_binding_size: None,
                    },
                    count: None,
                },
            ],
        },
    );

    let pipeline_layout = device.create_pipeline_layout(
        &wgpu::PipelineLayoutDescriptor {
            label: Some("Pipeline Layout"),
            bind_group_layouts: &[&bind_group_layout],
            push_constant_ranges: &[],
        },
    );

    let compute_pipeline = device.create_compute_pipeline(
        &wgpu::ComputePipelineDescriptor {
            label: Some("Compute Pipeline"),
            layout: Some(&pipeline_layout),
            module: &shader,
            entry_point: "main_algebra",
        },
    );

    // Output buffer (fixed size for H_m)
    // Size: m * m elements * 2 floats * 4 bytes
    let output_buffer_size = (m * m * 2 * 4) as u64;
    let output_buffer =
        device.create_buffer(&wgpu::BufferDescriptor {
            label: Some("Output Matrix Buffer"),
            size: output_buffer_size,
            usage: wgpu::BufferUsages::STORAGE
                | wgpu::BufferUsages::COPY_SRC,
            mapped_at_creation: false,
        });

    // Staging buffer for reading back
    let staging_buffer =
        device.create_buffer(&wgpu::BufferDescriptor {
            label: Some("Staging Buffer"),
            size: output_buffer_size,
            usage: wgpu::BufferUsages::MAP_READ
                | wgpu::BufferUsages::COPY_DST,
            mapped_at_creation: false,
        });

    // --- 3. Krylov Loop ---
    for k in 0..m {
        // A. Symbolic Step (Host)
        if k > 0 {
            let prev_param = basis_params.last().unwrap();
            let next_param =
                apply_symbolic_resolvent(prev_param, shifts[k - 1]);
            basis_params.push(next_param);
        }

        // B. Generate Agents for this column
        // We want to compute <v_i | v_k> for all i <= k (or all i)
        // Let's compute the whole column k.
        // Agents will be: MetricScanner for each row i.

        let mut agents = Vec::new();
        for i in 0..basis_params.len() {
            agents.push(Agent {
                kind: AgentKind::MetricScanner as u32,
                principal_port: k as u32, // Target: v_k (current column)
                aux_port: (i * m + k) as u32, // Matrix index (row i, col k)
                param_index: i as u32,        // Source: v_i
                shift_val: 0.0,
                padding: [0; 2],
            });
        }

        // C. Upload Buffers
        let param_buffer = device.create_buffer_init(
            &wgpu::util::BufferInitDescriptor {
                label: Some("Param Buffer"),
                contents: bytemuck::cast_slice(&basis_params),
                usage: wgpu::BufferUsages::STORAGE,
            },
        );

        // Make sure agents reference valid indices.
        // Note: agents refer to 'agents' array indices in WGSL logic?
        // Wait, main_algebra says:
        // let scanner = agents[idx];
        // let target_idx = scanner.principal_port;
        // let state_vector = agents[target_idx];
        // So principal_port must point to an Agent in the agents array that represents the state vector.

        // We need to add "StateVector" agents to the agents list so Scanners can reference them!
        // Re-design agent list:
        // [0..N]: StateVector agents for v_0...v_k
        // [N..]: Scanner agents

        let mut gpu_agents = Vec::new();
        // Add State Vectors
        for (idx, _) in basis_params.iter().enumerate() {
            gpu_agents.push(Agent {
                kind: AgentKind::StateVector as u32,
                principal_port: 0,
                aux_port: 0,
                param_index: idx as u32,
                shift_val: 0.0,
                padding: [0; 2],
            });
        }
        let state_vec_offset = 0;

        // Add Scanners
        for i in 0..basis_params.len() {
            gpu_agents.push(Agent {
                kind: AgentKind::MetricScanner as u32,
                principal_port: (state_vec_offset + k) as u32, // Index of v_k agent
                aux_port: (i * m + k) as u32,
                param_index: i as u32, // Params for v_i
                shift_val: 0.0,
                padding: [0; 2],
            });
        }

        let agent_buffer = device.create_buffer_init(
            &wgpu::util::BufferInitDescriptor {
                label: Some("Agent Buffer"),
                contents: bytemuck::cast_slice(&gpu_agents),
                usage: wgpu::BufferUsages::STORAGE,
            },
        );

        // D. Bind Group
        let bind_group =
            device.create_bind_group(&wgpu::BindGroupDescriptor {
                label: Some("Bind Group"),
                layout: &bind_group_layout,
                entries: &[
                    wgpu::BindGroupEntry {
                        binding: 0,
                        resource: param_buffer.as_entire_binding(),
                    },
                    wgpu::BindGroupEntry {
                        binding: 1,
                        resource: agent_buffer.as_entire_binding(),
                    },
                    wgpu::BindGroupEntry {
                        binding: 2,
                        resource: output_buffer.as_entire_binding(),
                    },
                ],
            });

        // E. Dispatch
        let mut encoder = device.create_command_encoder(
            &wgpu::CommandEncoderDescriptor { label: None },
        );
        {
            let mut cpass = encoder.begin_compute_pass(
                &wgpu::ComputePassDescriptor {
                    label: None,
                    timestamp_writes: None,
                },
            );
            cpass.set_pipeline(&compute_pipeline);
            cpass.set_bind_group(0, &bind_group, &[]);
            let workgroup_count = (gpu_agents.len() as u32 + 63) / 64;
            cpass.dispatch_workgroups(workgroup_count, 1, 1);
        }
        queue.submit(Some(encoder.finish()));
    }

    // --- 4. Readback ---
    let mut encoder = device.create_command_encoder(
        &wgpu::CommandEncoderDescriptor { label: None },
    );
    encoder.copy_buffer_to_buffer(
        &output_buffer,
        0,
        &staging_buffer,
        0,
        output_buffer_size,
    );
    queue.submit(Some(encoder.finish()));

    let buffer_slice = staging_buffer.slice(..);
    let (sender, receiver) = tokio::sync::oneshot::channel();
    buffer_slice.map_async(wgpu::MapMode::Read, move |v| {
        sender.send(v).unwrap()
    });
    device.poll(wgpu::Maintain::Wait);
    receiver.await.unwrap().unwrap();

    let data = buffer_slice.get_mapped_range();
    let result: Vec<f32> = bytemuck::cast_slice(&data).to_vec();
    drop(data);
    staging_buffer.unmap();

    // --- 5. Error Bound ---
    let h_step = if m > 1 { shifts[0] - shifts[1] } else { 1.0 };
    let spectral_error = 2.0 * 11.08 * (-h_step * m as f32).exp();

    (result, spectral_error)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn test_overlap() {
        let p1 = SymbolicParams {
            type_id: 0,
            alpha: 1.0,
            x0: 0.0,
            k0: 0.0,
            amp_real: 1.0,
            amp_imag: 0.0,
            padding: [0; 2],
        };
        // Just a dummy test for now
        let (_matrix, _error) =
            run_symbolic_delta_sirk(p1, vec![10.0, 9.0]).await;
        assert_eq!(_matrix.len(), 2 * 2 * 2);
    }
}
