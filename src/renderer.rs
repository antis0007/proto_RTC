use crate::gpu_compute::{DispatchCounts, GpuMeshingBuffers, GpuMeshingPipeline};

const INDIRECT_DRAW_STRIDE: u64 = std::mem::size_of::<wgpu::util::DrawIndexedIndirectArgs>() as u64;
const MESH_META_STRIDE: u64 = std::mem::size_of::<crate::gpu_compute::MeshMetaCounts>() as u64;

pub struct MeshingPage {
    pub id: u32,
    pub dispatch: DispatchCounts,
}

#[derive(Clone, Copy, Debug)]
pub struct VisibleChunk {
    /// Byte offset of this chunk/page metadata in `mesh_meta_buffer`.
    pub mesh_meta_offset: u64,
    /// Byte offset of this chunk/page indirect draw entry in `draw_indirect_buffer`.
    pub indirect_offset: u64,
}

pub struct Renderer {
    pub gpu_meshing: GpuMeshingPipeline,
    pub gpu_buffers: GpuMeshingBuffers,
}

impl Renderer {
    fn log_meshing_startup_validation_once(&self) {
        static STARTUP_VALIDATION_LOG: std::sync::Once = std::sync::Once::new();
        STARTUP_VALIDATION_LOG.call_once(|| {
            self.gpu_meshing.log_startup_validation();
        });
    }

    pub fn rebuild_chunk_meshes(
        &mut self,
        device: &wgpu::Device,
        queue: &wgpu::Queue,
        encoder: &mut wgpu::CommandEncoder,
        pages: &[MeshingPage],
    ) {
        self.log_meshing_startup_validation_once();

        self.gpu_buffers
            .debug_assert_page_configuration(pages.len() as u32);

        for page in pages {
            if page.id >= self.gpu_buffers.page_capacity {
                eprintln!(
                    "renderer: skipping meshing page_id={} (capacity={})",
                    page.id, self.gpu_buffers.page_capacity
                );
                continue;
            }

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
            debug_assert_eq!(
                chunk.indirect_offset / INDIRECT_DRAW_STRIDE,
                chunk.mesh_meta_offset / MESH_META_STRIDE
            );
            render_pass.draw_indexed_indirect(
                &self.gpu_buffers.draw_indirect_buffer,
                chunk.indirect_offset,
            );
        }
    }
}
