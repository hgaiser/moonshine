#version 450
layout(location=0) out vec4 o;
layout(binding=0) uniform sampler2D tex;
layout(push_constant) uniform PC { uint iters; uint frame; uint taps; } pc;
void main() {
    vec2 uv = gl_FragCoord.xy / vec2(textureSize(tex, 0));
    vec4 acc = vec4(0.0);
    // Bandwidth: a few dependent texture reads, like a post-process pass.
    for (uint i = 0u; i < pc.taps; i++)
        acc += texture(tex, uv + vec2(float(i) * 0.0013 + float(i & 3u) * 0.21, float(pc.frame & 63u) * 0.0007));
    // ALU: deterministic, frame-invariant cost.
    vec2 z = uv;
    for (uint i = 0u; i < pc.iters; i++)
        z = vec2(z.x * z.x - z.y * z.y, 2.0 * z.x * z.y) * 0.5 + uv * 0.25 + acc.xy * 0.01;
    o = acc * (1.0 / float(max(pc.taps, 1u))) + vec4(z, 0.0, 1.0) * 0.001;
}
