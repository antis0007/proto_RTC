use crate::gpu_compute::{DispatchCounts, GpuMeshingBuffers, GpuMeshingPipeline};

const INDIRECT_DRAW_STRIDE: u64 = std::mem::size_of::<wgpu::util::DrawIndexedIndirectArgs>() as u64;

pub struct MeshingPage {
    pub id: u32,
    pub dispatch: DispatchCounts,
}

#[derive(Clone, Copy, Debug)]
pub struct CpuDrawIndexedArgs {
    pub index_count: u32,
    pub base_index: u32,
    pub vertex_offset: i32,
}

#[derive(Clone, Copy, Debug)]
pub struct VisibleChunk {
    /// Meshing page index used to locate this chunk's `DrawIndexedIndirectArgs` entry.
    pub page_index: u32,
    /// CPU draw information, only used when `feature = "cpu_meshing"` is enabled.
    #[cfg(feature = "cpu_meshing")]
    pub cpu_draw: CpuDrawIndexedArgs,
}

pub struct Renderer {
    pub gpu_meshing: GpuMeshingPipeline,
    pub gpu_buffers: GpuMeshingBuffers,
    /// Runtime toggle: defaults to the validated CPU draw path.
    pub use_gpu_indirect_rendering: bool,
}

impl Renderer {
    pub fn set_gpu_indirect_rendering(&mut self, enabled: bool) {
        self.use_gpu_indirect_rendering = enabled;
    }

    pub fn rebuild_chunk_meshes(
        &mut self,
        device: &wgpu::Device,
        queue: &wgpu::Queue,
        encoder: &mut wgpu::CommandEncoder,
        pages: &[MeshingPage],
    ) {
        for page in pages {
            // Keep CPU meshing enabled while GPU path is brought up in parallel.
            #[cfg(feature = "cpu_meshing")]
            self.run_cpu_meshing(page.id);

            self.gpu_meshing.dispatch_for_page(
                device,
                queue,
                encoder,
                &self.gpu_buffers,
                page.id,
                DispatchCounts {
                    x: page.dispatch.x,
                    y: page.dispatch.y,
                    z: page.dispatch.z,
                },
            );

            // Debug verification: log generated counts per page.
            self.gpu_meshing.debug_log_page_counts(device, page.id);
        }
    }

    pub fn draw_visible_chunks<'a>(
        &'a self,
        render_pass: &mut wgpu::RenderPass<'a>,
        visible_chunks: &[VisibleChunk],
    ) {
        // Shared geometry buffers are bound once globally, then each chunk picks draw args.
        render_pass.set_vertex_buffer(0, self.gpu_buffers.chunk_vertex_buffer.slice(..));
        render_pass.set_index_buffer(
            self.gpu_buffers.chunk_index_buffer.slice(..),
            wgpu::IndexFormat::Uint32,
        );

        for chunk in visible_chunks {
            if self.use_gpu_indirect_rendering {
                // One `DrawIndexedIndirectArgs` entry per meshing page.
                let indirect_offset = chunk.page_index as u64 * INDIRECT_DRAW_STRIDE;
                render_pass
                    .draw_indexed_indirect(&self.gpu_buffers.draw_indirect_buffer, indirect_offset);
                continue;
            }

            #[cfg(feature = "cpu_meshing")]
            {
                render_pass.draw_indexed(
                    chunk.cpu_draw.base_index
                        ..(chunk.cpu_draw.base_index + chunk.cpu_draw.index_count),
                    chunk.cpu_draw.vertex_offset,
                    0..1,
                );
            }

            #[cfg(not(feature = "cpu_meshing"))]
            {
                let indirect_offset = chunk.page_index as u64 * INDIRECT_DRAW_STRIDE;
                render_pass
                    .draw_indexed_indirect(&self.gpu_buffers.draw_indirect_buffer, indirect_offset);
            }
        }
    }

    #[cfg(feature = "cpu_meshing")]
    fn run_cpu_meshing(&self, page_id: u32) {
        log::trace!("CPU meshing still active for page={}", page_id);
        // Existing CPU meshing code path remains unchanged and still executes.
    }
}
