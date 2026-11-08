// The orb's shader: a Metal port of the fragment shader in orb.js and the
// shared GLSL in orbkit.js (eyz/orbs). A hollow, glass-like cup made of
// liquid is raymarched; colour comes from how much material each view ray
// passes through (thin = background shows, thick = deep navy). A feedback
// texture keeps part of the previous frame for the slight motion trail.
//
// Only what the prototype's defaults use is here: a cup (no ring), no morph,
// no ink or vapour, no refraction (ior 1), no grain.
//
// `present` then turns the opaque picture into colour over transparency for
// the floating window: the look's background (beige, or black for the glow
// look) becomes see-through, so on that background it looks exactly as the
// prototype does.

#include <metal_stdlib>
using namespace metal;

struct U {
  float4 rot0, rot1, rot2;   // columns of the object -> world rotation
  float4 center;             // world position of the orb; w: radius of a sphere holding it
  float4 sh[12];             // liquid surface, spherical harmonics l = 2..6
  float4 boil;               // bubble height, bubble frequency, boil phase
  float4 hot;                // hotspot direction, spread
  float4 shape;              // radius, opening plane, rim rounding
  float4 wall;               // half wall thickness opposite the opening, at the lip; falloff
  float4 p1;                 // density, specular, step scale, focal length
  float4 p2;                 // bubble shape, glow (0 = light look), edge, keep (trail)
  float4 res;                // render size, jitter
  float4 out;                // x: fade (opacity of the whole orb)
  float4 tint;               // colour the material is lifted towards, how far
  float4 halo;               // glow just outside the shell: colour, strength
};

constant float3 CAM = float3(0.0, 0.0, 4.0);

// Colour ramp sampled from the reference video, background -> deep navy.
constant float RAMP_T[8] = {0.0, 0.12, 0.28, 0.48, 0.62, 0.76, 0.88, 1.0};
constant float3 RAMP_C[8] = {
  float3(223.0, 210.0, 194.0) / 255.0,
  float3(193.0, 185.0, 179.0) / 255.0,
  float3(144.0, 161.0, 183.0) / 255.0,
  float3( 54.0, 105.0, 171.0) / 255.0,
  float3( 16.0,  53.0, 145.0) / 255.0,
  float3( 22.0,  41.0, 102.0) / 255.0,
  float3( 28.0,  39.0,  72.0) / 255.0,
  float3( 26.0,  28.0,  36.0) / 255.0,
};

float3 ramp(float x) {
  float3 c = RAMP_C[0];
  for (int i = 1; i < 8; i++) {
    c = mix(c, RAMP_C[i], clamp((x - RAMP_T[i - 1]) / (RAMP_T[i] - RAMP_T[i - 1]), 0.0, 1.0));
  }
  return c;
}

// Glow look (the second reference video): black -> navy faces -> blue
// streaks -> near-white edges, sampled from that video.
constant float GLOW_T[7] = {0.0, 0.2, 0.4, 0.6, 0.8, 0.92, 1.0};
constant float3 GLOW_C[7] = {
  float3(  0.0,   0.0,   0.0) / 255.0,
  float3( 10.0,  16.0,  38.0) / 255.0,
  float3( 24.0,  37.0,  70.0) / 255.0,
  float3( 47.0,  82.0, 145.0) / 255.0,
  float3(104.0, 140.0, 200.0) / 255.0,
  float3(165.0, 185.0, 217.0) / 255.0,
  float3(236.0, 228.0, 240.0) / 255.0,
};

float3 glowRamp(float x) {
  float3 c = GLOW_C[0];
  for (int i = 1; i < 7; i++) {
    c = mix(c, GLOW_C[i], clamp((x - GLOW_T[i - 1]) / (GLOW_T[i] - GLOW_T[i - 1]), 0.0, 1.0));
  }
  return c;
}

float3 hash3(float3 p) {
  p = float3(dot(p, float3(127.1, 311.7, 74.7)),
             dot(p, float3(269.5, 183.3, 246.1)),
             dot(p, float3(113.5, 271.9, 124.6)));
  return -1.0 + 2.0 * fract(sin(p) * 43758.5453123);
}

float gnoise(float3 p) {
  float3 i = floor(p), f = fract(p);
  float3 u = f * f * (3.0 - 2.0 * f);
  return mix(mix(mix(dot(hash3(i), f),
                     dot(hash3(i + float3(1, 0, 0)), f - float3(1, 0, 0)), u.x),
                 mix(dot(hash3(i + float3(0, 1, 0)), f - float3(0, 1, 0)),
                     dot(hash3(i + float3(1, 1, 0)), f - float3(1, 1, 0)), u.x), u.y),
             mix(mix(dot(hash3(i + float3(0, 0, 1)), f - float3(0, 0, 1)),
                     dot(hash3(i + float3(1, 0, 1)), f - float3(1, 0, 1)), u.x),
                 mix(dot(hash3(i + float3(0, 1, 1)), f - float3(0, 1, 1)),
                     dot(hash3(i + float3(1, 1, 1)), f - float3(1, 1, 1)), u.x), u.y), u.z);
}

float smax(float a, float b, float k) {
  float h = max(k - abs(a - b), 0.0) / k;
  return max(a, b) + h * h * k * 0.25;
}

float highlight(float3 rd, float3 n) {
  return pow(max(dot(reflect(rd, n), normalize(float3(0.7, -0.45, 0.6))), 0.0), 40.0);
}

// Radial displacement of the liquid surface in unit direction n (world space).
{{LIQUID}}

// Boiling: smooth noise drifting through itself. Wherever it rises above zero
// a round bubble swells, then sinks back as the noise moves on. It only
// happens around the hotspot and fades out with distance from it.
float boil(float3 n, constant U& u) {
  float heat = exp(-(1.0 - dot(n, u.hot.xyz)) / (u.hot.w * u.hot.w));
  if (heat < 0.01) return 0.0;
  float3 p = n * u.boil.y;
  float t = u.boil.z;
  float a = min(max(gnoise(p + float3(0.0, 0.0, t)), 0.0) / 0.6, 1.0);
  float b = min(max(gnoise(p * 2.1 + float3(t * 1.3, -t * 0.7, 3.1)), 0.0) / 0.6, 1.0);
  // Raising to a higher power keeps only the tops of the bumps: spikes.
  return (pow(a, u.p2.x) + 0.6 * pow(b, u.p2.x)) * 1.44 * u.boil.x * heat;
}

float map(float3 p, constant U& u) {
  float3 w = p - u.center.xyz;
  float3 q = float3(dot(u.rot0.xyz, w), dot(u.rot1.xyz, w), dot(u.rot2.xyz, w));   // world -> object
  float r = max(length(w), 1e-5);
  float3 n = w / r;
  // Bubbles swell only the outside, so they add material and show as darker blobs.
  float bubble = boil(n, u);
  float wall = mix(u.wall.x, u.wall.y, pow(smoothstep(-u.shape.x, u.shape.y, q.z), u.wall.z));
  float shell = abs(r - u.shape.x - liquid(n, u) - 0.5 * bubble) - wall - 0.5 * bubble;
  return smax(shell, q.z - u.shape.y, u.shape.z);
}

float3 calcNormal(float3 p, constant U& u) {
  const float2 e = float2(0.002, -0.002);
  return normalize(e.xyy * map(p + e.xyy, u) + e.yyx * map(p + e.yyx, u) +
                   e.yxy * map(p + e.yxy, u) + e.xxx * map(p + e.xxx, u));
}

// Without refraction the ray goes straight through, so a single march that
// adds up the distance spent inside the material is enough. Glow look: how
// much the walls are seen edge-on where the ray crosses them (gEdge), which
// is what makes their outlines shine.
float straightPath(float3 p, float3 rd, thread float& spec, thread float& gEdge, constant U& u) {
  float len = 0.0, t = 0.0;
  bool hit = false, wasIn = false;
  spec = 0.0;
  for (int i = 0; i < 160; i++) {
    float d = map(p + rd * t, u);
    float s = clamp(abs(d) * u.p1.z, 0.004, 0.25);
    float inside = clamp(0.5 - d / 0.008, 0.0, 1.0);   // soft edge, no stair-stepping
    if (inside > 0.0 && !hit) {
      hit = true;
      if (u.p1.y > 0.0) spec = highlight(rd, calcNormal(p + rd * t, u));
    }
    if (u.p2.z > 0.0 && (inside > 0.5) != wasIn) {
      wasIn = inside > 0.5;
      gEdge += pow(1.0 - abs(dot(calcNormal(p + rd * t, u), rd)), 6.0);
    }
    len += s * inside;
    t += s;
    float3 w = p + rd * t - u.center.xyz;
    if (dot(w, rd) > 0.0 && length(w) > u.center.w) break;
  }
  return len;
}

struct VOut { float4 pos [[position]]; };

vertex VOut fullscreen(uint vid [[vertex_id]]) {
  float2 p[3] = {float2(-1.0, -1.0), float2(3.0, -1.0), float2(-1.0, 3.0)};
  VOut o;
  o.pos = float4(p[vid], 0.0, 1.0);
  return o;
}

fragment float4 scene(VOut in [[stage_in]], constant U& u [[buffer(0)]], texture2d<float> prev [[texture(0)]]) {
  // Metal's pixel origin is the top left; the prototype's (GL) is the bottom left.
  float2 frag = float2(in.pos.x, u.res.y - in.pos.y);
  float2 uv = (frag + u.res.zw - 0.5 * u.res.xy) / min(u.res.x, u.res.y);
  float3 rd = normalize(float3(uv, -u.p1.w));
  float glow = u.p2.y;
  float3 col = glow > 0.0 ? float3(0.0) : RAMP_C[0];
  float3 oc = CAM - u.center.xyz;
  float b = dot(oc, rd);
  float h = b * b - dot(oc, oc) + u.center.w * u.center.w;
  if (h > 0.0) {
    float spec, gEdge = 0.0;
    float3 start = CAM + rd * max(-b - sqrt(h), 0.0);
    float len = straightPath(start, rd, spec, gEdge, u);
    if (glow > 0.0) {
      col = glowRamp(1.0 - exp(-len * u.p1.x * glow - gEdge * u.p2.z));
    } else {
      col = ramp(1.0 - exp(-len * u.p1.x));
    }
    col += spec * u.p1.y * float3(0.82, 0.88, 1.0);
    // Lifted towards the state's tint (a pale blue at rest), more where there
    // is more material; the background (no material) stays as it is, so it
    // still turns transparent.
    float x = 1.0 - exp(-len * u.p1.x);
    col = mix(col, u.tint.rgb, u.tint.w * smoothstep(0.0, 0.6, x));
  }
  // The halo: light round the outline, fading out over a few tenths of a
  // radius; none over the orb itself. It becomes translucent colour too.
  if (u.halo.w > 0.0) {
    float dist = length(oc - rd * dot(oc, rd));   // how close the ray passes the centre
    float rim = u.shape.x + u.wall.y;
    float g = u.halo.w * exp(-max(dist - rim, 0.0) * 6.0) * smoothstep(rim - 0.2, rim + 0.02, dist);
    col = mix(col, u.halo.rgb, clamp(g, 0.0, 1.0));
  }
  // Blend this frame with the previous one for the motion trail.
  float3 before = prev.read(uint2(in.pos.xy)).rgb;
  return float4(mix(col, before, u.p2.w), 1.0);
}

fragment float4 present(VOut in [[stage_in]], constant U& u [[buffer(0)]], texture2d<float> frame [[texture(0)]]) {
  float3 col = frame.read(uint2(in.pos.xy)).rgb;
  // col = bg * (1 - a) + c * a, with the least a that keeps c within 0..1.
  float3 bg = u.p2.y > 0.0 ? float3(0.0) : RAMP_C[0];
  float a = 0.0;
  for (int i = 0; i < 3; i++) {
    a = max(a, bg[i] > 0.0 ? 1.0 - col[i] / bg[i] : 0.0);
    a = max(a, bg[i] < 1.0 ? (col[i] - bg[i]) / (1.0 - bg[i]) : 0.0);
  }
  a = clamp(a, 0.0, 1.0);
  float3 premul = max(col - bg * (1.0 - a), 0.0);
  return float4(premul, a) * u.out.x;
}
