use bytemuck::{Pod, Zeroable};

#[repr(C)]
#[derive(Clone, Copy, Debug, Default, Pod, Zeroable)]
pub struct MeshMetaCounts {
    pub vertex_count: u32,
    pub index_count: u32,
}

#[repr(C)]
#[derive(Clone, Copy, Debug, Default, Pod, Zeroable)]
pub struct MeshingCounters {
    pub vertex_counter: u32,
    pub index_counter: u32,
}

pub struct GpuMeshingPipeline {
    pub pipeline: wgpu::ComputePipeline,
    pub bind_group_layout: wgpu::BindGroupLayout,
}

pub struct GpuMeshingBuffers {
    pub voxel_material_buffer: wgpu::Buffer,
    pub chunk_vertex_buffer: wgpu::Buffer,
    pub chunk_index_buffer: wgpu::Buffer,
    pub mesh_meta_buffer: wgpu::Buffer,
    pub draw_indirect_buffer: wgpu::Buffer,
    pub meshing_params_buffer: wgpu::Buffer,
    /// Number of logical pages allocated across all page-indexed GPU buffers.
    pub page_capacity: u32,
    /// Capacity (in pages) for `mesh_meta_buffer`.
    pub mesh_meta_page_capacity: u32,
    /// Capacity (in pages) for `draw_indirect_buffer`.
    pub draw_indirect_page_capacity: u32,
}

pub struct DispatchCounts {
    pub x: u32,
    pub y: u32,
    pub z: u32,
}

impl GpuMeshingPipeline {
    pub const MESHING_SHADER_PATH: &'static str = "src/shaders/meshing.wgsl";
    pub const REQUIRED_BINDINGS: [u32; 6] = [0, 1, 2, 3, 4, 5];

    pub fn bind_group_layout_entries() -> [wgpu::BindGroupLayoutEntry; 6] {
        [
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
                    ty: wgpu::BufferBindingType::Storage { read_only: false },
                    has_dynamic_offset: false,
                    min_binding_size: None,
                },
                count: None,
            },
            wgpu::BindGroupLayoutEntry {
                binding: 3,
                visibility: wgpu::ShaderStages::COMPUTE,
                ty: wgpu::BindingType::Buffer {
                    ty: wgpu::BufferBindingType::Storage { read_only: false },
                    has_dynamic_offset: false,
                    min_binding_size: None,
                },
                count: None,
            },
            wgpu::BindGroupLayoutEntry {
                binding: 4,
                visibility: wgpu::ShaderStages::COMPUTE,
                ty: wgpu::BindingType::Buffer {
                    ty: wgpu::BufferBindingType::Storage { read_only: false },
                    has_dynamic_offset: false,
                    min_binding_size: None,
                },
                count: None,
            },
            wgpu::BindGroupLayoutEntry {
                binding: 5,
                visibility: wgpu::ShaderStages::COMPUTE,
                ty: wgpu::BindingType::Buffer {
                    ty: wgpu::BufferBindingType::Uniform,
                    has_dynamic_offset: false,
                    min_binding_size: None,
                },
                count: None,
            },
        ]
    }

    pub fn create_bind_group_layout(device: &wgpu::Device) -> wgpu::BindGroupLayout {
        let entries = Self::bind_group_layout_entries();
        device.create_bind_group_layout(&wgpu::BindGroupLayoutDescriptor {
            label: Some("gpu_meshing.bind_group_layout"),
            entries: &entries,
        })
    }

    fn create_bind_group(
        &self,
        device: &wgpu::Device,
        buffers: &GpuMeshingBuffers,
    ) -> wgpu::BindGroup {
        device.create_bind_group(&wgpu::BindGroupDescriptor {
            label: Some("gpu_meshing.bind_group"),
            layout: &self.bind_group_layout,
            entries: &[
                wgpu::BindGroupEntry {
                    binding: 0,
                    resource: buffers.voxel_material_buffer.as_entire_binding(),
                },
                wgpu::BindGroupEntry {
                    binding: 1,
                    resource: buffers.chunk_vertex_buffer.as_entire_binding(),
                },
                wgpu::BindGroupEntry {
                    binding: 2,
                    resource: buffers.chunk_index_buffer.as_entire_binding(),
                },
                wgpu::BindGroupEntry {
                    binding: 3,
                    resource: buffers.mesh_meta_buffer.as_entire_binding(),
                },
                wgpu::BindGroupEntry {
                    binding: 4,
                    resource: buffers.draw_indirect_buffer.as_entire_binding(),
                },
                wgpu::BindGroupEntry {
                    binding: 5,
                    resource: buffers.meshing_params_buffer.as_entire_binding(),
                },
            ],
        })
    }

    pub fn log_startup_validation(&self) {
        eprintln!(
            "gpu_meshing: pipeline created with shader '{}' and compatible bind-group bindings {:?}",
            Self::MESHING_SHADER_PATH,
            Self::REQUIRED_BINDINGS
        );
    }

    #[allow(clippy::too_many_arguments)]
    pub fn dispatch_for_page(
        &self,
        device: &wgpu::Device,
        _queue: &wgpu::Queue,
        encoder: &mut wgpu::CommandEncoder,
        buffers: &GpuMeshingBuffers,
        page_id: u32,
        dispatch: DispatchCounts,
    ) {
        if page_id >= buffers.page_capacity {
            eprintln!(
                "gpu_meshing: rejecting dispatch for page_id={} (capacity={})",
                page_id, buffers.page_capacity
            );
            return;
        }

        debug_assert_eq!(
            buffers.page_capacity, buffers.mesh_meta_page_capacity,
            "mesh meta page capacity must match configured page capacity"
        );
        debug_assert_eq!(
            buffers.page_capacity, buffers.draw_indirect_page_capacity,
            "draw indirect page capacity must match configured page capacity"
        );

        // Bind the same resources required by src/shaders/meshing.wgsl (@binding 0..=5).
        let bind_group = self.create_bind_group(device, buffers);

        // 3) Dispatch meshing shader.
        {
            let mut pass = encoder.begin_compute_pass(&wgpu::ComputePassDescriptor {
                label: Some("gpu_meshing.dispatch"),
                timestamp_writes: None,
            });
            pass.set_pipeline(&self.pipeline);
            pass.set_bind_group(0, &bind_group, &[]);
            pass.dispatch_workgroups(dispatch.x, dispatch.y, dispatch.z);
        }
    }
}

impl GpuMeshingBuffers {
    #[inline]
    pub fn debug_assert_page_configuration(&self, configured_page_count: u32) {
        debug_assert_eq!(
            configured_page_count, self.page_capacity,
            "configured GPU page count must match allocated page capacity"
        );
        debug_assert_eq!(
            self.page_capacity, self.mesh_meta_page_capacity,
            "mesh_meta_page_capacity must equal page_capacity"
        );
        debug_assert_eq!(
            self.page_capacity, self.draw_indirect_page_capacity,
            "draw_indirect_page_capacity must equal page_capacity"
        );
    }
}
