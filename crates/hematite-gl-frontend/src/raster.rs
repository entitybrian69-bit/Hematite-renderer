//! The software rasterizer: clip in homogeneous space, transform to window coordinates
//! and shade fragments into the context framebuffer.
//!
//! Attributes are interpolated perspective-correctly (each is divided by `w` first), so
//! `glFrustum` projections do not swim. Coverage is analytic per pixel centre; there is no
//! multisampling and no anti-aliasing.

use crate::context::{Context, Vertex};
use crate::gl_types::*;

/// A vertex in clip space, before the perspective divide.
#[derive(Clone, Copy, Debug)]
struct Clip {
    pos: [f32; 4],
    color: [f32; 4],
    tex: [f32; 4],
}

/// A vertex in window coordinates, after the divide and viewport transform.
#[derive(Clone, Copy, Debug)]
struct Frag {
    x: f32,
    y: f32,
    z: f32,
    inv_w: f32,
    color: [f32; 4],
    tex: [f32; 4],
}

fn mix(a: &Clip, b: &Clip, t: f32) -> Clip {
    let lerp4 = |x: [f32; 4], y: [f32; 4]| -> [f32; 4] {
        [
            x[0] + (y[0] - x[0]) * t,
            x[1] + (y[1] - x[1]) * t,
            x[2] + (y[2] - x[2]) * t,
            x[3] + (y[3] - x[3]) * t,
        ]
    };
    Clip { pos: lerp4(a.pos, b.pos), color: lerp4(a.color, b.color), tex: lerp4(a.tex, b.tex) }
}

/// Signed distance of a clip-space vertex from one of the six frustum planes.
fn plane_distance(v: &Clip, plane: usize) -> f32 {
    let [x, y, z, w] = v.pos;
    match plane {
        0 => w + x,
        1 => w - x,
        2 => w + y,
        3 => w - y,
        4 => w + z,
        _ => w - z,
    }
}

/// Sutherland-Hodgman clip of one polygon against the view volume.
fn clip_polygon(input: &[Clip]) -> Vec<Clip> {
    let mut polygon: Vec<Clip> = input.to_vec();
    for plane in 0..6 {
        if polygon.is_empty() {
            break;
        }
        let mut output: Vec<Clip> = Vec::with_capacity(polygon.len() + 1);
        for index in 0..polygon.len() {
            let current = polygon[index];
            let next = polygon[(index + 1) % polygon.len()];
            let current_inside = plane_distance(&current, plane) >= 0.0;
            let next_inside = plane_distance(&next, plane) >= 0.0;
            if current_inside {
                output.push(current);
            }
            if current_inside != next_inside {
                let dc = plane_distance(&current, plane);
                let dn = plane_distance(&next, plane);
                let denominator = dc - dn;
                let t = if denominator.abs() > f32::EPSILON { dc / denominator } else { 0.0 };
                output.push(mix(&current, &next, t.clamp(0.0, 1.0)));
            }
        }
        polygon = output;
    }
    polygon
}

impl Context {
    fn to_window(&self, v: &Clip) -> Option<Frag> {
        let w = v.pos[3];
        if !w.is_finite() || w.abs() <= 1e-9 {
            return None;
        }
        let inv_w = 1.0 / w;
        let ndc_x = v.pos[0] * inv_w;
        let ndc_y = v.pos[1] * inv_w;
        let ndc_z = v.pos[2] * inv_w;
        if ![ndc_x, ndc_y, ndc_z].iter().all(|value| value.is_finite()) {
            return None;
        }
        let [vx, vy, vw, vh] = [
            self.viewport[0] as f32,
            self.viewport[1] as f32,
            self.viewport[2] as f32,
            self.viewport[3] as f32,
        ];
        Some(Frag {
            x: vx + (ndc_x * 0.5 + 0.5) * vw,
            y: vy + (ndc_y * 0.5 + 0.5) * vh,
            z: ndc_z * 0.5 + 0.5,
            inv_w,
            color: v.color,
            tex: v.tex,
        })
    }
}

/// Sample a bound 2D texture, honouring the wrap and filter the application set.
fn sample_texture(ctx: &mut Context, s: f32, t: f32) -> Option<[f32; 4]> {
    let name = ctx.bound_texture[ctx.active_unit]?;
    let texture = ctx.textures.get(name as usize - 1)?.as_ref()?;
    if texture.width == 0 || texture.height == 0 {
        return None;
    }
    ctx.diagnostics.texture_fetches += 1;
    let wrap = |value: f32, mode: u32, extent: usize| -> f32 {
        match mode {
            GL_REPEAT => value - value.floor(),
            _ => {
                let half = 0.5 / extent as f32;
                value.clamp(half, 1.0 - half)
            }
        }
    };
    let u = wrap(s, texture.wrap_s, texture.width);
    let v = wrap(t, texture.wrap_t, texture.height);
    let fetch = |x: usize, y: usize| -> [f32; 4] {
        let x = x.min(texture.width - 1);
        let y = y.min(texture.height - 1);
        let offset = (y * texture.width + x) * 4;
        [
            f32::from(texture.data[offset]) / 255.0,
            f32::from(texture.data[offset + 1]) / 255.0,
            f32::from(texture.data[offset + 2]) / 255.0,
            f32::from(texture.data[offset + 3]) / 255.0,
        ]
    };
    let fx = u * texture.width as f32 - 0.5;
    let fy = v * texture.height as f32 - 0.5;
    if texture.mag_filter == GL_LINEAR || texture.min_filter == GL_LINEAR {
        let x0 = fx.floor();
        let y0 = fy.floor();
        let tx = fx - x0;
        let ty = fy - y0;
        let (x0, y0) = (x0 as i64, y0 as i64);
        let at = |x: i64, y: i64| {
            fetch(
                x.rem_euclid(texture.width as i64) as usize,
                y.rem_euclid(texture.height as i64) as usize,
            )
        };
        let top = mix4(&at(x0, y0), &at(x0 + 1, y0), tx);
        let bottom = mix4(&at(x0, y0 + 1), &at(x0 + 1, y0 + 1), tx);
        Some(mix4(&top, &bottom, ty))
    } else {
        Some(fetch(fx.round().max(0.0) as usize, fy.round().max(0.0) as usize))
    }
}

fn mix4(a: &[f32; 4], b: &[f32; 4], t: f32) -> [f32; 4] {
    [
        a[0] + (b[0] - a[0]) * t,
        a[1] + (b[1] - a[1]) * t,
        a[2] + (b[2] - a[2]) * t,
        a[3] + (b[3] - a[3]) * t,
    ]
}

fn blend_factor(factor: u32, source: &[f32; 4], dest: &[f32; 4]) -> [f32; 4] {
    match factor {
        GL_ZERO => [0.0; 4],
        GL_ONE => [1.0; 4],
        GL_SRC_COLOR => *source,
        GL_ONE_MINUS_SRC_COLOR => {
            [1.0 - source[0], 1.0 - source[1], 1.0 - source[2], 1.0 - source[3]]
        }
        GL_SRC_ALPHA => [source[3]; 4],
        GL_ONE_MINUS_SRC_ALPHA => [1.0 - source[3]; 4],
        GL_DST_COLOR => *dest,
        GL_ONE_MINUS_DST_COLOR => [1.0 - dest[0], 1.0 - dest[1], 1.0 - dest[2], 1.0 - dest[3]],
        GL_DST_ALPHA => [dest[3]; 4],
        _ => [1.0 - dest[3]; 4],
    }
}

fn depth_passes(func: u32, incoming: f32, stored: f32) -> bool {
    match func {
        GL_NEVER => false,
        GL_ALWAYS => true,
        GL_LESS => incoming < stored,
        GL_LEQUAL => incoming <= stored,
        GL_EQUAL => incoming == stored,
        GL_NOTEQUAL => incoming != stored,
        GL_GREATER => incoming > stored,
        _ => incoming >= stored,
    }
}

/// Shade and store one fragment. Shared by triangles, lines and points so the depth,
/// scissor, blend and mask rules cannot drift apart between them.
#[allow(clippy::too_many_arguments)]
fn shade(ctx: &mut Context, x: i32, y: i32, z: f32, color: &[f32; 4], tex: &[f32; 4]) {
    if x < 0 || y < 0 || x >= ctx.framebuffer.width as i32 || y >= ctx.framebuffer.height as i32 {
        return;
    }
    if ctx.scissor_test {
        let [sx, sy, sw, sh] = ctx.scissor;
        if x < sx || y < sy || x >= sx + sw || y >= sy + sh {
            return;
        }
    }
    let index = (y as usize * ctx.framebuffer.width + x as usize) * 4;
    if ctx.depth_test && !depth_passes(ctx.depth_func, z, ctx.framebuffer.depth[index / 4]) {
        return;
    }
    let mut shaded = *color;
    if ctx.texture_2d {
        if let Some(sample) = sample_texture(ctx, tex[0], tex[1]) {
            shaded = [
                shaded[0] * sample[0],
                shaded[1] * sample[1],
                shaded[2] * sample[2],
                shaded[3] * sample[3],
            ];
        }
    }
    let stored = [
        f32::from(ctx.framebuffer.color[index]) / 255.0,
        f32::from(ctx.framebuffer.color[index + 1]) / 255.0,
        f32::from(ctx.framebuffer.color[index + 2]) / 255.0,
        f32::from(ctx.framebuffer.color[index + 3]) / 255.0,
    ];
    let out = if ctx.blend {
        let src = blend_factor(ctx.blend_src, &shaded, &stored);
        let dst = blend_factor(ctx.blend_dst, &shaded, &stored);
        [
            shaded[0] * src[0] + stored[0] * dst[0],
            shaded[1] * src[1] + stored[1] * dst[1],
            shaded[2] * src[2] + stored[2] * dst[2],
            shaded[3] * src[3] + stored[3] * dst[3],
        ]
    } else {
        shaded
    };
    if ctx.depth_test && ctx.depth_mask {
        ctx.framebuffer.depth[index / 4] = z;
    }
    for (channel, value) in out.iter().enumerate() {
        if ctx.color_mask[channel] {
            ctx.framebuffer.color[index + channel] = (value.clamp(0.0, 1.0) * 255.0).round() as u8;
        }
    }
    ctx.diagnostics.fragments_written += 1;
}

fn edge(a: &Frag, b: &Frag, x: f32, y: f32) -> f32 {
    (b.x - a.x) * (y - a.y) - (b.y - a.y) * (x - a.x)
}

impl Context {
    /// Rasterize one triangle, clipping it first.
    fn rasterize(&mut self, triangle: [Clip; 3]) {
        let clipped = clip_polygon(&triangle);
        if clipped.len() < 3 {
            return;
        }
        let mut window: Vec<Frag> = Vec::with_capacity(clipped.len());
        for vertex in &clipped {
            match self.to_window(vertex) {
                Some(fragment) => window.push(fragment),
                None => return,
            }
        }
        for index in 1..window.len() - 1 {
            self.shade_triangle(window[0], window[index], window[index + 1]);
        }
    }

    fn shade_triangle(&mut self, a: Frag, b: Frag, c: Frag) {
        let area = edge(&a, &b, c.x, c.y);
        if !area.is_finite() || area.abs() < 1e-9 {
            return;
        }
        let front = if area > 0.0 { self.front_face == GL_CCW } else { self.front_face == GL_CW };
        if self.cull_face {
            let drop = match self.cull_mode {
                GL_FRONT => front,
                GL_BACK => !front,
                _ => true,
            };
            if drop {
                return;
            }
        }
        let min_x = a.x.min(b.x).min(c.x).floor() as i32;
        let max_x = a.x.max(b.x).max(c.x).ceil() as i32;
        let min_y = a.y.min(b.y).min(c.y).floor() as i32;
        let max_y = a.y.max(b.y).max(c.y).ceil() as i32;
        let inverse = 1.0 / area;
        self.diagnostics.triangles += 1;
        let inv_w = [a.inv_w, b.inv_w, c.inv_w];
        for y in min_y..=max_y {
            for x in min_x..=max_x {
                let px = x as f32 + 0.5;
                let py = y as f32 + 0.5;
                let w0 = edge(&b, &c, px, py) * inverse;
                let w1 = edge(&c, &a, px, py) * inverse;
                let w2 = 1.0 - w0 - w1;
                if w0 < 0.0 || w1 < 0.0 || w2 < 0.0 {
                    continue;
                }
                let denominator = w0 * inv_w[0] + w1 * inv_w[1] + w2 * inv_w[2];
                let scale = if denominator.abs() > 1e-12 { 1.0 / denominator } else { 0.0 };
                let weights = [w0 * scale, w1 * scale, w2 * scale];
                let color: [f32; 4] = std::array::from_fn(|channel| {
                    let values = [a.color[channel], b.color[channel], c.color[channel]];
                    (0..3).map(|i| weights[i] * inv_w[i] * values[i]).sum()
                });
                let tex: [f32; 4] = std::array::from_fn(|channel| {
                    let values = [a.tex[channel], b.tex[channel], c.tex[channel]];
                    (0..3).map(|i| weights[i] * inv_w[i] * values[i]).sum()
                });
                let depth = (0..3).map(|i| weights[i] * inv_w[i] * [a.z, b.z, c.z][i]).sum();
                shade(self, x, y, depth, &color, &tex);
            }
        }
    }

    fn shade_line(&mut self, from: Frag, to: Frag) {
        let steps = ((to.x - from.x).abs().max((to.y - from.y).abs()).ceil() as i32).max(1);
        for step in 0..=steps {
            let t = step as f32 / steps as f32;
            let x = (from.x + (to.x - from.x) * t).round() as i32;
            let y = (from.y + (to.y - from.y) * t).round() as i32;
            let z = from.z + (to.z - from.z) * t;
            let color = mix4(&from.color, &to.color, t);
            let tex = mix4(&from.tex, &to.tex, t);
            shade(self, x, y, z, &color, &tex);
        }
    }

    fn shade_point(&mut self, vertex: Frag) {
        shade(
            self,
            vertex.x.round() as i32,
            vertex.y.round() as i32,
            vertex.z,
            &vertex.color,
            &vertex.tex,
        );
    }

    /// Transform, expand the primitive topology and rasterize.
    pub fn draw_vertices(&mut self, mode: u32, vertices: &[Vertex]) {
        if vertices.len() < 2 {
            return;
        }
        let mvp = self.mvp();
        let mut clip: Vec<Clip> = Vec::with_capacity(vertices.len());
        for vertex in vertices {
            let position = mvp.transform([
                f64::from(vertex.pos[0]),
                f64::from(vertex.pos[1]),
                f64::from(vertex.pos[2]),
                f64::from(vertex.pos[3]),
            ]);
            clip.push(Clip {
                pos: [
                    position[0] as f32,
                    position[1] as f32,
                    position[2] as f32,
                    position[3] as f32,
                ],
                color: vertex.color,
                tex: vertex.tex,
            });
        }
        self.diagnostics.draw_calls += 1;
        match mode {
            GL_POINTS => {
                for vertex in &clip {
                    if let Some(fragment) = self.to_window(vertex) {
                        self.shade_point(fragment);
                    }
                }
            }
            GL_LINES => {
                for pair in clip.chunks_exact(2) {
                    self.draw_line_pair(pair[0], pair[1]);
                }
            }
            GL_LINE_STRIP => {
                for pair in clip.windows(2) {
                    self.draw_line_pair(pair[0], pair[1]);
                }
            }
            GL_LINE_LOOP => {
                for pair in clip.windows(2) {
                    self.draw_line_pair(pair[0], pair[1]);
                }
                if let (Some(first), Some(last)) = (clip.first(), clip.last()) {
                    self.draw_line_pair(*last, *first);
                }
            }
            GL_TRIANGLES => {
                for triangle in clip.chunks_exact(3) {
                    self.rasterize([triangle[0], triangle[1], triangle[2]]);
                }
            }
            GL_TRIANGLE_STRIP => {
                for index in 0..clip.len().saturating_sub(2) {
                    let triangle = if index % 2 == 0 {
                        [clip[index], clip[index + 1], clip[index + 2]]
                    } else {
                        [clip[index + 1], clip[index], clip[index + 2]]
                    };
                    self.rasterize(triangle);
                }
            }
            GL_TRIANGLE_FAN => {
                for index in 1..clip.len().saturating_sub(1) {
                    self.rasterize([clip[0], clip[index], clip[index + 1]]);
                }
            }
            GL_QUADS => {
                for quad in clip.chunks_exact(4) {
                    self.rasterize([quad[0], quad[1], quad[2]]);
                    self.rasterize([quad[0], quad[2], quad[3]]);
                }
            }
            _ => self.set_error(GL_INVALID_ENUM),
        }
    }

    fn draw_line_pair(&mut self, first: Clip, second: Clip) {
        if let (Some(a), Some(b)) = (self.to_window(&first), self.to_window(&second)) {
            self.shade_line(a, b);
        }
    }

    /// Clear the buffers named by `mask`, honouring the scissor test and the write masks.
    pub fn clear(&mut self, mask: u32) {
        let scissor = if self.scissor_test { Some(self.scissor) } else { None };
        let (width, height) = (self.framebuffer.width, self.framebuffer.height);
        let (x0, y0, x1, y1) = match scissor {
            Some([x, y, w, h]) => (
                x.max(0) as usize,
                y.max(0) as usize,
                ((x + w).max(0) as usize).min(width),
                ((y + h).max(0) as usize).min(height),
            ),
            None => (0, 0, width, height),
        };
        let clear = [
            (self.clear_color[0].clamp(0.0, 1.0) * 255.0).round() as u8,
            (self.clear_color[1].clamp(0.0, 1.0) * 255.0).round() as u8,
            (self.clear_color[2].clamp(0.0, 1.0) * 255.0).round() as u8,
            (self.clear_color[3].clamp(0.0, 1.0) * 255.0).round() as u8,
        ];
        for y in y0..y1 {
            for x in x0..x1 {
                let offset = (y * width + x) * 4;
                if mask & GL_COLOR_BUFFER_BIT != 0 {
                    for (channel, value) in clear.iter().enumerate() {
                        if self.color_mask[channel] {
                            self.framebuffer.color[offset + channel] = *value;
                        }
                    }
                }
                if mask & GL_DEPTH_BUFFER_BIT != 0 && self.depth_mask {
                    self.framebuffer.depth[offset / 4] = self.clear_depth;
                }
            }
        }
    }
}
