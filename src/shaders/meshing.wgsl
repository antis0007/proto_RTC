const EMPTY_MATERIAL: u32 = 0u;
const CHUNK_SIZE_X: u32 = 32u;
const CHUNK_SIZE_Y: u32 = 32u;
const CHUNK_SIZE_Z: u32 = 32u;
const CHUNK_VOXEL_COUNT: u32 = CHUNK_SIZE_X * CHUNK_SIZE_Y * CHUNK_SIZE_Z;

const FACE_VERTEX_COUNT: u32 = 4u;
const FACE_INDEX_COUNT: u32 = 6u;
const INVALID_ALLOC: u32 = 0xffffffffu;

struct Vertex {
  position: vec4<f32>,
  normal: vec4<f32>,
  material: u32,
  _padding0: vec3<u32>,
};

struct DrawIndexedIndirect {
  index_count: atomic<u32>,
  instance_count: atomic<u32>,
  first_index: atomic<u32>,
  base_vertex: atomic<u32>,
  first_instance: atomic<u32>,
};

struct MeshMeta {
  vertex_count: atomic<u32>,
  index_count: atomic<u32>,
  dirty: atomic<u32>,

  // Legacy counters are intentionally preserved.
  legacy_face_counter: atomic<u32>,
  legacy_emitted_index_counter: atomic<u32>,

  max_vertices: u32,
  max_indices: u32,
  page_id: u32,
  _padding0: u32,
};

struct MeshingParams {
  chunk_base_voxel: u32,
  chunk_voxel_count: u32,
  page_id: u32,
  _padding0: u32,
};

@group(0) @binding(0)
var<storage, read> voxel_material_buffer: array<u32>;

@group(0) @binding(1)
var<storage, read_write> chunk_vertex_buffer: array<Vertex>;

@group(0) @binding(2)
var<storage, read_write> chunk_index_buffer: array<u32>;

@group(0) @binding(3)
var<storage, read_write> mesh_meta_buffer: array<MeshMeta>;

@group(0) @binding(4)
var<storage, read_write> draw_indirect_buffer: array<DrawIndexedIndirect>;

@group(0) @binding(5)
var<uniform> params: MeshingParams;

fn flatten(pos: vec3<u32>) -> u32 {
  return pos.x + pos.y * CHUNK_SIZE_X + pos.z * CHUNK_SIZE_X * CHUNK_SIZE_Y;
}

fn in_bounds(pos: vec3<i32>) -> bool {
  return pos.x >= 0 && pos.y >= 0 && pos.z >= 0 &&
         pos.x < i32(CHUNK_SIZE_X) && pos.y < i32(CHUNK_SIZE_Y) && pos.z < i32(CHUNK_SIZE_Z);
}

fn try_allocate(counter: ptr<storage, atomic<u32>, read_write>, count: u32, max_count: u32) -> u32 {
  loop {
    let current = atomicLoad(counter);
    if (current + count > max_count) {
      return INVALID_ALLOC;
    }

    let exchange = atomicCompareExchangeWeak(counter, current, current + count);
    if (exchange.exchanged) {
      return current;
    }
  }
}

fn face_corner(dir: u32, corner: u32) -> vec3<f32> {
  switch dir {
    case 0u { return array<vec3<f32>, 4>(vec3<f32>(1.0,0.0,0.0), vec3<f32>(1.0,1.0,0.0), vec3<f32>(1.0,1.0,1.0), vec3<f32>(1.0,0.0,1.0))[corner]; }
    case 1u { return array<vec3<f32>, 4>(vec3<f32>(0.0,0.0,1.0), vec3<f32>(0.0,1.0,1.0), vec3<f32>(0.0,1.0,0.0), vec3<f32>(0.0,0.0,0.0))[corner]; }
    case 2u { return array<vec3<f32>, 4>(vec3<f32>(0.0,1.0,1.0), vec3<f32>(1.0,1.0,1.0), vec3<f32>(1.0,1.0,0.0), vec3<f32>(0.0,1.0,0.0))[corner]; }
    case 3u { return array<vec3<f32>, 4>(vec3<f32>(0.0,0.0,0.0), vec3<f32>(1.0,0.0,0.0), vec3<f32>(1.0,0.0,1.0), vec3<f32>(0.0,0.0,1.0))[corner]; }
    case 4u { return array<vec3<f32>, 4>(vec3<f32>(0.0,0.0,1.0), vec3<f32>(1.0,0.0,1.0), vec3<f32>(1.0,1.0,1.0), vec3<f32>(0.0,1.0,1.0))[corner]; }
    default { return array<vec3<f32>, 4>(vec3<f32>(0.0,1.0,0.0), vec3<f32>(1.0,1.0,0.0), vec3<f32>(1.0,0.0,0.0), vec3<f32>(0.0,0.0,0.0))[corner]; }
  }
}

fn face_normal(dir: u32) -> vec3<f32> {
  switch dir {
    case 0u { return vec3<f32>(1.0, 0.0, 0.0); }
    case 1u { return vec3<f32>(-1.0, 0.0, 0.0); }
    case 2u { return vec3<f32>(0.0, 1.0, 0.0); }
    case 3u { return vec3<f32>(0.0, -1.0, 0.0); }
    case 4u { return vec3<f32>(0.0, 0.0, 1.0); }
    default { return vec3<f32>(0.0, 0.0, -1.0); }
  }
}

fn dir_offset(dir: u32) -> vec3<i32> {
  switch dir {
    case 0u { return vec3<i32>(1, 0, 0); }
    case 1u { return vec3<i32>(-1, 0, 0); }
    case 2u { return vec3<i32>(0, 1, 0); }
    case 3u { return vec3<i32>(0, -1, 0); }
    case 4u { return vec3<i32>(0, 0, 1); }
    default { return vec3<i32>(0, 0, -1); }
  }
}

@compute @workgroup_size(8, 8, 8)
fn main(@builtin(global_invocation_id) gid: vec3<u32>) {
  if (gid.x >= CHUNK_SIZE_X || gid.y >= CHUNK_SIZE_Y || gid.z >= CHUNK_SIZE_Z) { return; }

  let local_idx = flatten(gid);
  if (local_idx >= params.chunk_voxel_count || local_idx >= CHUNK_VOXEL_COUNT) { return; }

  let page_id = params.page_id;
  let voxel_idx = params.chunk_base_voxel + local_idx;
  let material = voxel_material_buffer[voxel_idx];
  if (material == EMPTY_MATERIAL) { return; }

  for (var dir: u32 = 0u; dir < 6u; dir = dir + 1u) {
    let npos_i = vec3<i32>(i32(gid.x), i32(gid.y), i32(gid.z)) + dir_offset(dir);

    var neighbor_empty = true;
    if (in_bounds(npos_i)) {
      let npos = vec3<u32>(u32(npos_i.x), u32(npos_i.y), u32(npos_i.z));
      let nidx = params.chunk_base_voxel + flatten(npos);
      neighbor_empty = voxel_material_buffer[nidx] == EMPTY_MATERIAL;
    }
    if (!neighbor_empty) { continue; }

    // Preserve legacy counter flow.
    atomicAdd(&mesh_meta_buffer[page_id].legacy_face_counter, 1u);
    atomicAdd(&mesh_meta_buffer[page_id].legacy_emitted_index_counter, FACE_INDEX_COUNT);
    atomicStore(&mesh_meta_buffer[page_id].dirty, 1u);

    let vertex_base = try_allocate(
      &mesh_meta_buffer[page_id].vertex_count,
      FACE_VERTEX_COUNT,
      mesh_meta_buffer[page_id].max_vertices,
    );
    if (vertex_base == INVALID_ALLOC) { continue; }

    let index_base = try_allocate(
      &mesh_meta_buffer[page_id].index_count,
      FACE_INDEX_COUNT,
      mesh_meta_buffer[page_id].max_indices,
    );
    if (index_base == INVALID_ALLOC) { continue; }

    let voxel_origin = vec3<f32>(f32(gid.x), f32(gid.y), f32(gid.z));
    let normal = face_normal(dir);

    for (var i: u32 = 0u; i < FACE_VERTEX_COUNT; i = i + 1u) {
      chunk_vertex_buffer[vertex_base + i].position = vec4<f32>(voxel_origin + face_corner(dir, i), 1.0);
      chunk_vertex_buffer[vertex_base + i].normal = vec4<f32>(normal, 0.0);
      chunk_vertex_buffer[vertex_base + i].material = material;
    }

    chunk_index_buffer[index_base + 0u] = vertex_base + 0u;
    chunk_index_buffer[index_base + 1u] = vertex_base + 1u;
    chunk_index_buffer[index_base + 2u] = vertex_base + 2u;
    chunk_index_buffer[index_base + 3u] = vertex_base + 0u;
    chunk_index_buffer[index_base + 4u] = vertex_base + 2u;
    chunk_index_buffer[index_base + 5u] = vertex_base + 3u;

    atomicStore(&draw_indirect_buffer[page_id].index_count, atomicLoad(&mesh_meta_buffer[page_id].index_count));
    atomicStore(&draw_indirect_buffer[page_id].instance_count, 1u);
    atomicStore(&draw_indirect_buffer[page_id].first_index, 0u);
    atomicStore(&draw_indirect_buffer[page_id].base_vertex, 0u);
    atomicStore(&draw_indirect_buffer[page_id].first_instance, 0u);
  }
}
