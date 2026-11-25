// The scene's automatic exposure (gpu::AutoExposure): a histogram of the
// HDR target's log luminance, then the exposure it settles on, as Anno
// 117's eye_adaptation.fx builds and reads it (csPerTileHistogram,
// csCalculateExposure). The post pass multiplies by exposure[0].

struct Params {
    size: vec2<u32>,
    // Every stride-th pixel each way is counted.
    stride: u32,
    // 1: take the measured exposure at once, without adapting toward it.
    jump: u32,
    log_min: f32,
    log_range: f32,
    // The share of samples dropped from the dark end, and the share
    // counted after them (cHistogramFilterMin, cHistogramFilterMax).
    low: f32,
    count: f32,
    target_key: f32,
    min_exposure: f32,
    max_exposure: f32,
    tolerance: f32,
    speed_up: f32,
    speed_down: f32,
    // Seconds since the last frame.
    dt: f32,
    pad: f32,
    // Top left of the measured rect in the target; `size` is its extent.
    origin: vec2<u32>,
    pad2: vec2<u32>,
}

@group(0) @binding(0) var<uniform> params: Params;
@group(0) @binding(1) var t_hdr: texture_2d<f32>;
@group(0) @binding(2) var<storage, read_write> bins: array<atomic<u32>, 64>;
@group(0) @binding(3) var<storage, read_write> exposure: array<f32, 4>;

const BINS: u32 = 64u;
// eye_adaptation.fx's GetLuminance: LUMINANCE_CHANNEL_WEIGHTS.
const LUMA: vec3<f32> = vec3<f32>(0.299, 0.587, 0.114);

var<workgroup> local: array<atomic<u32>, 64>;

// Samples per thread along x and y (exposure.rs CELL). Neighbouring samples
// mostly land in one bin, so a thread counts a run and adds it once: at 5K
// one sample per thread spent most of the pass contending on the same few
// shared and global counters. The counts, and so the exposure, are the
// same either way.
const CELL: vec2<u32> = vec2<u32>(4u, 2u);

fn bin_of(c: vec4<f32>) -> u32 {
    let l = max(dot(c.rgb / min(c.a, 1.0), LUMA), 1e-10);
    let at = clamp((log2(l) - params.log_min) / params.log_range, 0.0, 1.0);
    return min(u32(f32(BINS) * at), BINS - 1u);
}

@compute @workgroup_size(16, 16)
fn cs_histogram(
    @builtin(workgroup_id) group: vec3<u32>,
    @builtin(local_invocation_id) lid: vec3<u32>,
    @builtin(local_invocation_index) li: u32,
) {
    if li < BINS { atomicStore(&local[li], 0u); }
    workgroupBarrier();
    // A workgroup covers 16 CELL samples each way; its threads step 16
    // apart, so each row of loads stays side by side.
    let corner = group.xy * 16u * CELL;
    var run = BINS;
    var n = 0u;
    for (var j = 0u; j < CELL.y; j++) {
        for (var i = 0u; i < CELL.x; i++) {
            let at_rect = (corner + lid.xy + 16u * vec2<u32>(i, j)) * params.stride;
            if any(at_rect >= params.size) { continue; }
            let c = textureLoad(t_hdr, params.origin + at_rect, 0);
            // Alpha is coverage: what nothing drew is not the scene's.
            if c.a <= 0.0 { continue; }
            let b = bin_of(c);
            if b != run {
                if n > 0u { atomicAdd(&local[run], n); }
                run = b;
                n = 0u;
            }
            n++;
        }
    }
    if n > 0u { atomicAdd(&local[run], n); }
    workgroupBarrier();
    if li < BINS {
        let n = atomicLoad(&local[li]);
        if n > 0u { atomicAdd(&bins[li], n); }
    }
}

// csCalculateExposure: the mean log luminance of the samples between the
// filter's bounds, the exposure that brings it to the target, clamped,
// held within the tolerance of the last, and approached at the adaptation
// speed. Clears the histogram for the next frame.
@compute @workgroup_size(1)
fn cs_exposure() {
    var total = 0u;
    for (var i = 0u; i < BINS; i++) { total += atomicLoad(&bins[i]); }
    var skip = u32(round(f32(total) * params.low));
    var keep = u32(round(f32(total) * params.count));
    var sum = 0.0;
    var used = 0u;
    let delta = params.log_range / f32(BINS);
    for (var i = 0u; i < BINS; i++) {
        var n = atomicLoad(&bins[i]);
        atomicStore(&bins[i], 0u);
        let dropped = min(n, skip);
        n -= dropped;
        skip -= dropped;
        n = min(n, keep);
        keep -= n;
        used += n;
        sum += (params.log_min + (f32(i) + 0.5) * delta) * f32(n);
    }
    if used == 0u { return; }
    let mean = sum / f32(used);
    var next = clamp(params.target_key / exp2(mean), params.min_exposure, params.max_exposure);
    let last = exposure[0];
    if params.jump == 1u || last <= 0.0 {
        exposure[0] = next;
        return;
    }
    // Within the tolerance of the last exposure nothing changes.
    let change = last / next;
    if change > 1.0 {
        next = last / max(change - params.tolerance, 1.0);
    } else {
        next = last / min(change + params.tolerance, 1.0);
    }
    let speed = select(params.speed_up, params.speed_down, next > last);
    let diff = max(next / last, last / next);
    let fac = 1.0 - exp(-0.6 * (diff - 0.7) * params.dt * speed);
    exposure[0] = last + (next - last) * clamp(fac, 0.0, 1.0);
}
