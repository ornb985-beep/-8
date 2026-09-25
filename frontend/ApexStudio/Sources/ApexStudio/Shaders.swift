/// Metal shaders, compiled at launch (keeps the SwiftPM build free of a
/// separate .metal compilation step).
enum Shaders {
    static let source = """
    #include <metal_stdlib>
    using namespace metal;

    struct VOut {
        float4 position [[position]];
        float2 uv;
    };

    // One oversized triangle covering the viewport.
    vertex VOut apex_vertex(uint vid [[vertex_id]]) {
        float2 p = float2((vid << 1) & 2, vid & 2);
        VOut o;
        o.position = float4(p * float2(2.0, -2.0) + float2(-1.0, 1.0), 0.0, 1.0);
        o.uv = p;
        return o;
    }

    struct FrameUniforms {
        uint layout;   // APEX_PIXEL_*: byte order of the guest pixel
        uint opaque;
    };

    // The guest buffer is bound as RGBA8 (bytes in memory order), so the
    // channel order of the guest format is fixed up here.
    fragment float4 apex_fragment(VOut in [[stage_in]],
                                  texture2d<float> frame [[texture(0)]],
                                  constant FrameUniforms &u [[buffer(0)]]) {
        constexpr sampler s(address::clamp_to_edge, filter::linear);
        float4 t = frame.sample(s, in.uv);
        float4 c;
        switch (u.layout) {
            case 0: c = t.bgra; break;  // B,G,R,A
            case 1: c = t;      break;  // R,G,B,A
            case 2: c = t.gbar; break;  // A,R,G,B
            default: c = t.abgr; break; // A,B,G,R
        }
        if (u.opaque != 0) { c.a = 1.0; }
        return c;
    }
    """
}
