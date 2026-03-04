use crate::gpu_compute::{DispatchCounts, GpuMeshingBuffers, GpuMeshingPipeline};

pub struct MeshingPage {
    pub id: u32,
    pub dispatch: DispatchCounts,
}

pub struct Renderer {
    pub gpu_meshing: GpuMeshingPipeline,
    pub gpu_buffers: GpuMeshingBuffers,
}

impl Renderer {
    pub fn rebuild_chunk_meshes(
        &mut self,
        device: &wgpu::Device,
        queue: &wgpu::Queue,
        encoder: &mut wgpu::CommandEncoder,
        pages: &[MeshingPage],
    ) {
        for page in pages {
            // Keep CPU meshing enabled while GPU path is brought up in parallel.
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

    fn run_cpu_meshing(&self, page_id: u32) {
        log::trace!("CPU meshing still active for page={}", page_id);
        // Existing CPU meshing code path remains unchanged and still executes.
    }
}
