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
    /// New per-page GPU counters.
    pub vertex_counter_buffer: wgpu::Buffer,
    pub index_counter_buffer: wgpu::Buffer,
}

pub struct DispatchCounts {
    pub x: u32,
    pub y: u32,
    pub z: u32,
}

impl GpuMeshingPipeline {
    #[allow(clippy::too_many_arguments)]
    pub fn dispatch_for_page(
        &self,
        device: &wgpu::Device,
        queue: &wgpu::Queue,
        encoder: &mut wgpu::CommandEncoder,
        buffers: &GpuMeshingBuffers,
        page_id: u32,
        dispatch: DispatchCounts,
    ) {
        // 1) Reset vertex_counter and index_counter before dispatch.
        let zero = 0_u32.to_le_bytes();
        let page_offset = page_id as u64 * std::mem::size_of::<u32>() as u64;
        queue.write_buffer(&buffers.vertex_counter_buffer, page_offset, &zero);
        queue.write_buffer(&buffers.index_counter_buffer, page_offset, &zero);

        // 2) Bind new buffers (vertex_counter_buffer + index_counter_buffer) to compute pipeline.
        let bind_group = device.create_bind_group(&wgpu::BindGroupDescriptor {
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
                wgpu::BindGroupEntry {
                    binding: 6,
                    resource: buffers.vertex_counter_buffer.as_entire_binding(),
                },
                wgpu::BindGroupEntry {
                    binding: 7,
                    resource: buffers.index_counter_buffer.as_entire_binding(),
                },
            ],
        });

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

        // After dispatch: copy per-page counts into mesh_meta_buffer.
        let counts_size = std::mem::size_of::<MeshMetaCounts>() as u64;
        let copy_dst = page_id as u64 * counts_size;
        encoder.copy_buffer_to_buffer(
            &buffers.vertex_counter_buffer,
            page_offset,
            &buffers.mesh_meta_buffer,
            copy_dst,
            std::mem::size_of::<u32>() as u64,
        );
        encoder.copy_buffer_to_buffer(
            &buffers.index_counter_buffer,
            page_offset,
            &buffers.mesh_meta_buffer,
            copy_dst + std::mem::size_of::<u32>() as u64,
            std::mem::size_of::<u32>() as u64,
        );
    }
}
