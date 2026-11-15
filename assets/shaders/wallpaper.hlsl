// Procedural live wallpapers. One program, several scenes chosen by
// `scene`; each scene takes its colours from c0..c3.
//   0 flow (aurora / dunes / tide)   1 nebula   2 galaxy   3 planet
//   4 northern lights                 5 ocean sunset        6 misty mountains

cbuffer Params : register(b0)
{
    float time;
    float aspect;
    float scene;
    float pad;
    float4 c0;
    float4 c1;
    float4 c2;
    float4 c3;
};

struct VsOut
{
    float4 pos : SV_Position;
    float2 uv : TEXCOORD0;
};

// One triangle that covers the whole viewport; no vertex buffer needed.
VsOut vs_main(uint id : SV_VertexID)
{
    VsOut o;
    o.uv = float2((id << 1) & 2, id & 2);
    o.pos = float4(o.uv * float2(2.0, -2.0) + float2(-1.0, 1.0), 0.0, 1.0);
    return o;
}

// ---------------------------------------------------------------- helpers

float hash21(float2 p)
{
    p = frac(p * float2(123.34, 456.21));
    p += dot(p, p + 45.32);
    return frac(p.x * p.y);
}

float vnoise(float2 p)
{
    float2 i = floor(p);
    float2 f = frac(p);
    float a = hash21(i);
    float b = hash21(i + float2(1.0, 0.0));
    float c = hash21(i + float2(0.0, 1.0));
    float d = hash21(i + float2(1.0, 1.0));
    float2 u = f * f * (3.0 - 2.0 * f);
    return lerp(lerp(a, b, u.x), lerp(c, d, u.x), u.y);
}

float fbm(float2 p)
{
    float v = 0.0;
    float a = 0.5;
    for (int k = 0; k < 5; k++)
    {
        v += a * vnoise(p);
        p = p * 2.03 + float2(17.1, 9.2);
        a *= 0.5;
    }
    return v;
}

float2 rot(float2 p, float a)
{
    float c = cos(a);
    float s = sin(a);
    return float2(c * p.x - s * p.y, s * p.x + c * p.y);
}

// One star per grid cell (in about a quarter of the cells), gently twinkling.
float starLayer(float2 p, float density, float t, float sizeMul)
{
    float2 g = p * density;
    float2 id = floor(g);
    float2 f = frac(g) - 0.5;
    float h = hash21(id);
    float2 off = float2(hash21(id + 3.1), hash21(id + 7.7)) - 0.5;
    float d = length(f - off * 0.6);
    float size = (0.12 + 0.22 * pow(hash21(id + 1.7), 6.0)) * sizeMul;
    float tw = 0.65 + 0.35 * sin(t * (0.8 + h * 2.5) + h * 40.0);
    return smoothstep(size, 0.0, d) * step(0.74, h) * tw;
}

float3 starfield(float2 p, float t)
{
    float s = starLayer(p, 70.0, t, 1.0) + starLayer(p + 7.3, 32.0, t * 0.8, 0.9) * 1.4;
    return s * float3(1.0, 0.96, 0.9);
}

// ----------------------------------------------------------------- scenes

// Slowly flowing colour bands made by domain-warping the screen.
float3 flow(float2 uv, float t)
{
    float2 p = uv;
    p.x *= aspect;
    t *= 0.07;
    float2 q = p * 1.5;
    for (int k = 1; k <= 4; k++)
    {
        q.x += 0.55 / k * sin(k * 1.7 * q.y + t * (0.9 + 0.2 * k) + 1.3 * k);
        q.y += 0.45 / k * cos(k * 1.3 * q.x - t * (0.7 + 0.15 * k) + 0.7 * k);
    }
    float a = 0.5 + 0.5 * sin(q.x * 1.1 + q.y * 0.6);
    float b = 0.5 + 0.5 * sin(q.y * 1.4 - q.x * 0.5 + 2.0);
    float3 col = lerp(c0.rgb, c1.rgb, smoothstep(0.0, 1.0, a));
    col = lerp(col, c2.rgb, smoothstep(0.2, 0.9, b) * 0.75);
    col = lerp(col, c3.rgb, pow(saturate(a * b), 2.5) * 0.8);
    return col;
}

// Coloured gas clouds drifting in front of the stars.
float3 nebula(float2 p, float t)
{
    float tt = t * 0.02;
    float2 q = p * 1.8;
    float2 w = float2(fbm(q + float2(0.0, tt)), fbm(q + float2(5.2, 1.3) - tt));
    float n = fbm(q + 2.2 * w + tt * 0.5);
    float n2 = fbm(q * 1.7 - w + float2(3.0, -tt));
    float3 col = c0.rgb;
    col += starfield(p + float2(t * 0.002, 0.0), t);
    col += c1.rgb * smoothstep(0.3, 0.85, n) * 2.0;
    col += c2.rgb * smoothstep(0.35, 0.9, n2) * 1.7;
    col += c3.rgb * pow(saturate(n * n2 * 2.2), 2.5) * 2.0;
    // Dark dust lanes.
    col *= 0.55 + 0.45 * smoothstep(0.25, 0.65, fbm(q * 3.0 + w * 1.5));
    return col;
}

// A tilted spiral galaxy turning very slowly.
float3 galaxy(float2 p, float t)
{
    float3 col = c0.rgb + starfield(p, t) * 0.8;
    float2 g = rot(p, 0.35);
    g.y *= 2.2;
    float r = length(g);
    float a = atan2(g.y, g.x);
    float twist = t * 0.015 + 2.6 * log(r + 0.03);
    float arm = pow(saturate(0.5 + 0.5 * sin(2.0 * (a + twist))), 2.5);
    float n = fbm(rot(g, twist) * 6.0);
    float disk = exp(-r * 3.2);
    float core = exp(-r * r * 60.0);
    col += c1.rgb * arm * disk * (0.35 + 0.9 * n) * 1.4;
    col += c3.rgb * pow(arm, 5.0) * disk * smoothstep(0.55, 0.8, n) * 0.9;
    col *= 1.0 - 0.6 * smoothstep(0.55, 0.75, fbm(rot(g, twist) * 10.0 + 4.0)) * arm * disk;
    col += c2.rgb * (core * 1.6 + disk * 0.15);
    return col;
}

// A ringed gas giant lit from the side.
float3 planet(float2 p, float t)
{
    float3 col = c0.rgb + c3.rgb * 0.05 * fbm(p * 2.0 + 10.0);
    col += starfield(p, t);
    float2 c = float2(aspect * 0.16, -0.1);
    float R = 0.34;
    float2 d = (p - c) / R;
    float r2 = dot(d, d);
    float dl = sqrt(r2);

    float2 rd = rot(d, -0.28);
    float rr = length(float2(rd.x, rd.y * 4.2));
    float ringBand = smoothstep(1.3, 1.36, rr) * smoothstep(2.15, 2.05, rr);
    float ringTex = 0.6 + 0.4 * sin(rr * 42.0) * (0.6 + 0.4 * vnoise(float2(rr * 30.0, 0.5)));
    float3 ringCol = lerp(c1.rgb, c2.rgb, 0.5) * ringTex * ringBand * 0.85;
    bool front = rd.y < 0.0;
    if (!front)
        col = col * (1.0 - ringBand * 0.9) + ringCol;

    if (r2 < 1.0)
    {
        float3 nrm = float3(d.x, d.y, sqrt(1.0 - r2));
        float3 L = normalize(float3(-0.7, 0.45, 0.55));
        float diff = saturate(dot(nrm, L));
        float lon = atan2(nrm.x, nrm.z) + t * 0.01;
        float lat = asin(clamp(nrm.y, -1.0, 1.0));
        float b = fbm(float2(lon * 1.5, lat * 9.0 + fbm(float2(lon * 3.0, lat * 20.0)) * 0.6));
        float3 base = lerp(c1.rgb, c2.rgb, smoothstep(0.3, 0.75, b));
        base = lerp(base, c1.rgb * 0.6, smoothstep(0.65, 0.9, fbm(float2(lon * 4.0, lat * 30.0))));
        float3 lit = base * (0.02 + 1.05 * diff);
        lit += c3.rgb * pow(1.0 - nrm.z, 3.0) * (0.2 + diff) * 0.6;
        col = lerp(col, lit, smoothstep(1.0, 0.985, dl));
    }
    // Thin atmosphere glow around the limb.
    col += c3.rgb * 0.25 * exp(-max(dl - 1.0, 0.0) * 18.0) * smoothstep(0.98, 1.0, dl);
    if (front)
        col = col * (1.0 - ringBand * 0.9) + ringCol;
    return col;
}

// Northern lights over a mountain range.
float3 borealis(float2 p, float t)
{
    float y = p.y;
    float3 col = lerp(c1.rgb, c0.rgb, smoothstep(-0.3, 0.5, y));
    // Faint green glow low over the horizon.
    col += c2.rgb * 0.07 * exp(-max(y + 0.2, 0.0) * 5.0);
    col += starfield(p, t) * smoothstep(-0.3, 0.1, y);
    float3 aur = float3(0.0, 0.0, 0.0);
    for (int k = 0; k < 3; k++)
    {
        float fk = k;
        float x = p.x * (0.8 + 0.25 * fk) + fk * 3.7;
        float base = 0.02 + 0.09 * fk + 0.08 * sin(x * 1.3 + t * 0.06 + fk) + 0.12 * (fbm(float2(x * 0.9 + t * 0.02, fk)) - 0.5);
        float h = y - base;
        float curtain = smoothstep(-0.015, 0.01, h) * exp(-max(h, 0.0) * (5.0 + fk * 2.0));
        // Soft light spilling below each curtain's lower edge.
        curtain += 0.22 * exp(-max(-h, 0.0) * 22.0) * step(h, 0.0);
        float rays = 0.55 + 0.45 * vnoise(float2(x * 35.0, t * 0.25 + fk * 10.0));
        float3 tint = lerp(c2.rgb, c3.rgb, saturate(h * 3.5));
        aur += tint * curtain * rays * (0.75 - 0.18 * fk);
    }
    col += aur;
    float ridge = -0.22 + 0.26 * (fbm(float2(p.x * 1.4 + 2.0, 0.5)) - 0.5) + 0.03 * vnoise(float2(p.x * 9.0, 1.0));
    float ridge2 = -0.32 + 0.16 * (fbm(float2(p.x * 2.3 - 4.0, 2.5)) - 0.5);
    col = lerp(col, c1.rgb * 0.6 + aur * 0.05, smoothstep(ridge + 0.002, ridge - 0.002, y));
    col = lerp(col, c0.rgb * 0.5, smoothstep(ridge2 + 0.002, ridge2 - 0.002, y));
    return col;
}

// Sunset over a calm sea.
float3 ocean(float2 p, float t)
{
    float hz = -0.06;
    float2 sun = float2(0.0, hz + 0.07);
    float3 glow = float3(1.0, 0.75, 0.45);
    float3 col;
    if (p.y > hz)
    {
        float h = p.y - hz;
        col = lerp(c2.rgb, c1.rgb, smoothstep(0.0, 0.18, h));
        col = lerp(col, c0.rgb, smoothstep(0.15, 0.55, h));
        float ds = length(p - sun);
        col += glow * exp(-ds * 7.0) * 0.6;
        col = lerp(col, float3(1.0, 0.92, 0.75), smoothstep(0.065, 0.058, ds));
        float cl = fbm(float2(p.x * 1.2 + t * 0.008, h * 7.0));
        float band = smoothstep(0.08, 0.2, h) * smoothstep(0.55, 0.3, h);
        float cloud = smoothstep(0.5, 0.75, cl) * band;
        col = lerp(col, lerp(c1.rgb * 0.7, float3(1.0, 0.7, 0.55), exp(-ds * 3.0)), cloud * 0.8);
    }
    else
    {
        float depth = hz - p.y;
        float z = 0.06 / (depth + 0.01);
        float w = fbm(float2(p.x * z * 2.0, z * 1.5 - t * 0.12));
        float3 sea = lerp(c3.rgb * 1.6, c3.rgb * 0.6, saturate(depth * 4.0));
        sea += c1.rgb * 0.25 * (1.0 - saturate(depth * 6.0));
        float path = exp(-abs(p.x - sun.x) * (6.0 + 30.0 * depth));
        float glit = smoothstep(0.55, 0.85, fbm(float2(p.x * z * 6.0, z * 5.0 - t * 0.25)));
        sea += glow * path * (0.25 + 1.2 * glit);
        sea *= 0.85 + 0.3 * w;
        col = sea;
    }
    return col;
}

// Layered ridges in morning mist.
float3 mountains(float2 p, float t)
{
    float y = p.y;
    float3 col = lerp(c1.rgb, c0.rgb, smoothstep(-0.1, 0.5, y));
    float2 sun = float2(-aspect * 0.18, 0.08);
    float ds = length(p - sun);
    col += float3(1.0, 0.85, 0.6) * exp(-ds * 5.0) * 0.5;
    col = lerp(col, float3(1.0, 0.95, 0.85), smoothstep(0.05, 0.043, ds));
    for (int k = 0; k < 5; k++)
    {
        float fk = k;
        float x = p.x * (1.0 + fk * 0.45) + fk * 7.3 + t * 0.004 * (fk + 1.0);
        float ridge = 0.12 - fk * 0.1 + 0.32 * (fbm(float2(x * 1.2, fk * 3.1)) - 0.5) * (1.0 - fk * 0.1);
        float3 layer = lerp(lerp(c1.rgb, c2.rgb, 0.6), c3.rgb, fk / 4.0);
        col = lerp(col, layer, smoothstep(ridge + 0.003, ridge - 0.003, y));
        float mist = fbm(float2(p.x * 2.0 + t * 0.01 * (fk + 1.0), y * 6.0 + fk));
        float band = exp(-abs(y - ridge + 0.03) * 18.0);
        col = lerp(col, c1.rgb * 0.95 + 0.05, band * smoothstep(0.4, 0.8, mist) * 0.45 * (1.0 - fk * 0.15));
    }
    return col;
}

// ------------------------------------------------------------------- main

float4 ps_main(VsOut i) : SV_Target
{
    // Centred, y up, 1.0 = viewport height.
    float2 p = (i.uv - 0.5) * float2(aspect, -1.0);
    int sc = (int)(scene + 0.5);
    float3 col;
    if (sc == 1)
        col = nebula(p, time);
    else if (sc == 2)
        col = galaxy(p, time);
    else if (sc == 3)
        col = planet(p, time);
    else if (sc == 4)
        col = borealis(p, time);
    else if (sc == 5)
        col = ocean(p, time);
    else if (sc == 6)
        col = mountains(p, time);
    else
        col = flow(i.uv, time);

    // Gentle vignette.
    float2 v = i.uv - 0.5;
    col *= 1.0 - 0.6 * dot(v, v);

    // A little noise hides banding in the smooth gradients.
    float n = frac(sin(dot(i.pos.xy, float2(12.9898, 78.233))) * 43758.5453);
    col += (n - 0.5) / 128.0;

    return float4(saturate(col), 1.0);
}
