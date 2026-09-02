// sap — the winged aphid, drawn by hand.
//
// A port of the companion's `pip2d.py` fragment shader from GLSL. One
// full-screen triangle strip and pure signed distance fields: no mesh, no
// texture, no depth buffer. What is new here is the pair of wings, which an
// aphid has and a blob did not, and the crossfade between two feelings, which
// the original cut straight between.

struct Params {
    // Seconds since the window opened.
    time: f32,
    // The feeling being drawn, and the one before it.
    emote: u32,
    previous: u32,
    // How far between them: 0 is entirely `previous`, 1 entirely `emote`.
    blend: f32,
};

var<uniform> params: Params;

struct Varying {
    @builtin(position) position: vec4<f32>,
    @location(0) uv: vec2<f32>,
};

// A strip of four corners, made from the vertex index alone.
@vertex
fn vs_main(@builtin(vertex_index) index: u32) -> Varying {
    let uv = vec2<f32>(f32(index & 1u), f32(index >> 1u));
    var out: Varying;
    out.uv = uv;
    out.position = vec4<f32>(uv * 2.0 - 1.0, 0.0, 1.0);
    return out;
}

fn sd_circle(p: vec2<f32>, r: f32) -> f32 {
    return length(p) - r;
}

// An ellipse, near enough: a circle in a squashed space.
fn sd_ellipse(p: vec2<f32>, r: vec2<f32>) -> f32 {
    return length(p / r) - 1.0;
}

fn speed_of(emote: u32) -> f32 {
    if emote == 7u { return 0.7; }   // sleeping
    if emote == 3u { return 3.2; }   // talking
    if emote == 4u { return 3.0; }   // happy
    return 2.0;
}

fn body_color(emote: u32) -> vec3<f32> {
    if emote == 5u { return vec3<f32>(0.56, 0.58, 0.86); }  // sad
    if emote == 2u { return vec3<f32>(0.63, 0.62, 0.95); }  // thinking
    if emote == 4u { return vec3<f32>(0.83, 0.62, 0.95); }  // happy
    if emote == 7u { return vec3<f32>(0.55, 0.47, 0.75); }  // sleeping
    return vec3<f32>(0.73, 0.58, 0.96);
}

// Everything the feeling decides, drawn once for each of the two feelings and
// mixed by the caller. Branching in here rather than picking the colours apart
// is what keeps the mouth and the eyes fading with the rest.
fn draw(uv: vec2<f32>, emote: u32) -> vec3<f32> {
    // `uv.y` grows downwards on the target, and so does the drawing: the head
    // is at the top of the frame because it is at the small end of `uv.y`.
    var p = uv * 2.0 - 1.0;

    let ink = vec3<f32>(0.16, 0.12, 0.24);
    var col = mix(
        vec3<f32>(0.06, 0.08, 0.10),
        vec3<f32>(0.09, 0.11, 0.14),
        uv.y
    );

    let speed = speed_of(emote);
    let t = params.time;
    let bob = sin(t * speed) * 0.035;
    let squash = 1.0 + cos(t * speed * 2.0) * 0.03;

    var bp = p - vec2<f32>(0.0, -0.08 + bob);
    bp.y = bp.y * squash;
    if emote == 5u { bp.y = bp.y * 1.12; }  // droop when sad

    // The wings, behind the body, beating twice as fast as it bobs. A sleeping
    // alate folds them.
    var beat = sin(t * 6.0) * 0.5 + 0.5;
    if emote == 7u { beat = 0.05; }
    let lift = 0.10 + 0.22 * beat;
    let wing_col = vec3<f32>(0.62, 0.72, 0.86);
    for (var i = 0; i < 2; i = i + 1) {
        let side = select(0.40, -0.40, i == 0);
        // Up and back from the body: an aphid's wings are over its abdomen, not
        // beside its face. `bp.y` grows upwards here, as the mouth below shows.
        var wp = bp - vec2<f32>(side, 0.18 + lift * 0.6);
        // Lean each wing away from the body as it rises, which is most of what
        // makes the beat read as a beat rather than as a twitch.
        let lean = select(-0.7, 0.7, i == 0) * lift;
        wp = vec2<f32>(wp.x + wp.y * lean, wp.y);
        let wing = sd_ellipse(wp, vec2<f32>(0.17, 0.26 + lift * 0.5));
        col = mix(col, wing_col, smoothstep(0.02, -0.02, wing) * 0.38);
    }

    let body = sd_circle(bp, 0.52);
    let mask = smoothstep(0.012, -0.012, body);
    let shade = 1.0 - 0.30 * smoothstep(-0.4, 0.6, -bp.y);
    let rim = smoothstep(0.02, -0.05, body) - smoothstep(-0.05, -0.16, body);
    col = mix(col, body_color(emote) * shade + rim * 0.18, mask);

    // Eyes, with a blink on a cycle of 3.7 seconds.
    var blink = 1.0;
    let cycle = fract(t / 3.7);
    if cycle > 0.94 {
        blink = abs(cos((cycle - 0.94) / 0.06 * 3.14159));
    }
    if emote == 7u { blink = 0.05; }
    if emote == 6u { blink = 1.4; }
    for (var i = 0; i < 2; i = i + 1) {
        let side = select(0.18, -0.18, i == 0);
        var ep = bp - vec2<f32>(side, 0.10);
        ep.y = ep.y / max(blink, 0.04);
        col = mix(col, ink, smoothstep(0.008, -0.008, sd_circle(ep, 0.055)) * mask);
        let gp = ep - vec2<f32>(0.02, 0.02);
        col = mix(
            col,
            vec3<f32>(0.95),
            smoothstep(0.004, -0.004, sd_circle(gp, 0.015)) * mask * step(0.2, blink)
        );
    }

    // Cheeks, when pleased or listening.
    if emote == 4u || emote == 1u {
        for (var i = 0; i < 2; i = i + 1) {
            let side = select(0.30, -0.30, i == 0);
            let cp = bp - vec2<f32>(side, -0.05);
            col = mix(
                col,
                vec3<f32>(0.95, 0.62, 0.72),
                smoothstep(0.02, -0.02, sd_circle(cp, 0.06)) * 0.55 * mask
            );
        }
    }

    // The mouth, which is most of what a feeling looks like.
    let mp = bp - vec2<f32>(0.0, -0.14);
    var mouth = 1e9;
    if emote == 4u {
        let d = abs(sd_circle(mp - vec2<f32>(0.0, 0.06), 0.16));
        if mp.y < -0.02 { mouth = d; }
    } else if emote == 5u {
        let d = abs(sd_circle(mp - vec2<f32>(0.0, -0.16), 0.16));
        if mp.y > -0.08 { mouth = d; }
    } else if emote == 6u {
        mouth = sd_circle(mp, 0.07);
    } else if emote == 3u {
        var tp = mp;
        tp.y = tp.y / (0.35 + 0.65 * abs(sin(t * 9.0)));
        mouth = sd_circle(tp, 0.06);
    } else {
        let d = abs(sd_circle(mp - vec2<f32>(0.0, 0.05), 0.11));
        if mp.y < -0.01 { mouth = d; }
    }
    col = mix(col, ink, smoothstep(0.012, -0.012, mouth - 0.012) * mask);

    // Three dots beside the head while it thinks.
    if emote == 2u {
        for (var i = 0; i < 3; i = i + 1) {
            let fi = f32(i);
            let dp = p - vec2<f32>(0.52 + fi * 0.13, 0.42 + sin(t * 2.0 + fi) * 0.03);
            col = mix(
                col,
                vec3<f32>(0.85, 0.82, 0.95),
                smoothstep(0.008, -0.008, sd_circle(dp, 0.022 + fi * 0.006))
            );
        }
    }
    return col;
}

@fragment
fn fs_main(in: Varying) -> @location(0) vec4<f32> {
    var col = draw(in.uv, params.emote);
    if params.blend < 1.0 {
        col = mix(draw(in.uv, params.previous), col, params.blend);
    }
    // Straight RGBA: the target's own `Bgra8Unorm` format is what puts the
    // bytes in the order `RenderImage` reads them, so swizzling here would undo
    // it rather than do it.
    return vec4<f32>(col, 1.0);
}
