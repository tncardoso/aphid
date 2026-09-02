// drift — the same creature as a body that turns and ripples.
//
// A port of the companion's `umbra3d.py`. The original drew a 40x60 UV sphere
// with a vertex shader displacing each vertex along its normal; this ray
// marches the same displaced sphere in the fragment shader instead. The
// displacement is the same three summed sines, the lighting is the same
// Lambert with a rim and a pulse, and the per-feeling parameters are the same
// table — but there is no vertex buffer, no index buffer and no depth
// attachment, which for one shape on one quad is machinery that pays for
// nothing.

struct Params {
    time: f32,
    emote: u32,
    previous: u32,
    blend: f32,
};

var<uniform> params: Params;

struct Varying {
    @builtin(position) position: vec4<f32>,
    @location(0) uv: vec2<f32>,
};

@vertex
fn vs_main(@builtin(vertex_index) index: u32) -> Varying {
    let uv = vec2<f32>(f32(index & 1u), f32(index >> 1u));
    var out: Varying;
    out.uv = uv;
    out.position = vec4<f32>(uv * 2.0 - 1.0, 0.0, 1.0);
    return out;
}

// colour, wobble, speed, pulse — the `EMOTE_PARAMS` table, moved to the GPU
// because there is one shape and the branch costs less than a second binding.
fn look(emote: u32) -> mat2x4<f32> {
    var colour = vec3<f32>(0.42, 0.33, 0.62);
    var wobble = 1.0;
    var speed = 1.0;
    var pulse = 0.4;
    if emote == 1u { colour = vec3<f32>(0.48, 0.38, 0.70); wobble = 1.1; speed = 1.2; pulse = 0.7; }
    if emote == 2u { colour = vec3<f32>(0.40, 0.42, 0.78); wobble = 0.6; speed = 0.5; pulse = 1.0; }
    if emote == 3u { colour = vec3<f32>(0.50, 0.38, 0.75); wobble = 1.7; speed = 2.2; pulse = 0.8; }
    if emote == 4u { colour = vec3<f32>(0.70, 0.40, 0.72); wobble = 1.4; speed = 1.8; pulse = 1.0; }
    if emote == 5u { colour = vec3<f32>(0.28, 0.30, 0.48); wobble = 0.5; speed = 0.5; pulse = 0.2; }
    if emote == 6u { colour = vec3<f32>(0.62, 0.55, 0.90); wobble = 2.2; speed = 3.0; pulse = 1.0; }
    if emote == 7u { colour = vec3<f32>(0.22, 0.20, 0.36); wobble = 0.35; speed = 0.3; pulse = 0.15; }
    return mat2x4<f32>(
        vec4<f32>(colour, wobble),
        vec4<f32>(speed, pulse, 0.0, 0.0)
    );
}

// 0x101419, the page both interfaces are drawn on.
const BACKGROUND: vec3<f32> = vec3<f32>(0.0627, 0.0784, 0.0980);

// Turn a point about Y, which is what makes the body drift past the eye.
fn spin(p: vec3<f32>, angle: f32) -> vec3<f32> {
    let c = cos(angle);
    let s = sin(angle);
    return vec3<f32>(c * p.x - s * p.z, p.y, s * p.x + c * p.z);
}

// The sphere, displaced by three sines in its own frame — the same sum the
// original applied to each vertex.
fn surface(p: vec3<f32>, t: f32, wobble: f32) -> f32 {
    let d = sin(p.y * 5.0 + t * 2.0) * 0.05
          + sin(p.x * 7.0 - t * 1.7) * 0.04
          + sin(p.z * 6.0 + t * 1.3) * 0.04;
    return length(p) - (1.0 - d * wobble);
}

fn normal_at(p: vec3<f32>, t: f32, wobble: f32) -> vec3<f32> {
    let e = vec2<f32>(0.002, 0.0);
    return normalize(vec3<f32>(
        surface(p + e.xyy, t, wobble) - surface(p - e.xyy, t, wobble),
        surface(p + e.yxy, t, wobble) - surface(p - e.yxy, t, wobble),
        surface(p + e.yyx, t, wobble) - surface(p - e.yyx, t, wobble)
    ));
}

fn draw(uv: vec2<f32>, emote: u32) -> vec3<f32> {
    let params_of = look(emote);
    let colour = params_of[0].xyz;
    let wobble = params_of[0].w;
    let speed = params_of[1].x;
    let pulse_amount = params_of[1].y;

    let t = params.time * speed;
    let screen = uv * 2.0 - 1.0;

    // The camera the original set with a perspective matrix: 45 degrees, back
    // along Z by 2.4, and tipped a little forward.
    let eye = vec3<f32>(0.0, 0.0, 2.4);
    let ray = normalize(vec3<f32>(screen * 0.83, -1.0));
    let bob = sin(t * 1.2) * 0.06;

    // Flat, and exactly the background of the window: see `sap.wgsl`.
    var col = BACKGROUND;

    var travelled = 0.0;
    var hit = false;
    var point = vec3<f32>(0.0);
    for (var step = 0; step < 64; step = step + 1) {
        let world = eye + ray * travelled - vec3<f32>(0.0, bob, 0.0);
        // Everything is done in the body's own frame, so turning the body is
        // turning the point that asks about it.
        point = spin(world, -params.time * 0.4 * speed);
        let distance = surface(point, t, wobble);
        if distance < 0.001 {
            hit = true;
            break;
        }
        travelled = travelled + distance;
        if travelled > 6.0 { break; }
    }
    if !hit {
        return col;
    }

    let n = normal_at(point, t, wobble);
    let light = normalize(vec3<f32>(0.4, 0.7, 0.6));
    let view = normalize(vec3<f32>(0.0, 0.0, 2.4) - point);
    let lambert = max(dot(n, light), 0.0);
    let rim = pow(1.0 - max(dot(n, view), 0.0), 2.0);
    let pulse = 0.5 + 0.5 * sin(params.time * 1.6);
    return colour * (0.22 + 0.78 * lambert)
        + rim * vec3<f32>(0.55, 0.40, 0.90) * (0.5 + 0.5 * pulse * pulse_amount);
}

@fragment
fn fs_main(in: Varying) -> @location(0) vec4<f32> {
    var col = draw(in.uv, params.emote);
    if params.blend < 1.0 {
        col = mix(draw(in.uv, params.previous), col, params.blend);
    }
    // Straight RGBA: the `Bgra8Unorm` target is what orders the bytes.
    return vec4<f32>(col, 1.0);
}
