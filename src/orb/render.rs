//! Draws the liquid orb with Metal, under GPUI's view in the orb's window
//! (GPUI has no custom shaders). Its own thread runs the physics (`sim`) and
//! renders up to 60 frames a second, like the prototype's loop: a scene pass
//! into one of two feedback textures (the motion trail), then a pass that
//! turns the opaque picture into colour over transparency for the window.
//!
//! The tower's phases become the orb's moods and moves (see `Drive`).

use std::ffi::c_void;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::thread::JoinHandle;
use std::time::{Duration, Instant};

use metal::foreign_types::ForeignType;
use metal::{
    CommandQueue, CompileOptions, Device, MTLClearColor, MTLLoadAction, MTLPixelFormat, MTLPrimitiveType, MTLStorageMode, MTLStoreAction,
    MTLTextureUsage, MetalLayer, RenderPassDescriptor, RenderPipelineDescriptor, RenderPipelineState, Texture, TextureDescriptor,
};

use super::liquid::LMAX;
use super::mac;
use super::sim::{Opts, Sim};

/// The prototype's camera: focal length and distance (orbkit.js).
const FOCAL: f64 = 1.32;
const CAM_Z: f64 = 4.0;
const MAX_FPS: f64 = 60.0;
/// How each state looks: the material's tint, a halo round it and how fast
/// the orb's clock runs (a share of real time; the prototype's is 1).
/// Listening is icy cyan with a breathing halo, replying periwinkle with a
/// halo that throbs on each heartbeat, everything else a calm pale blue.
#[derive(Clone, Copy)]
struct Cue {
    tint: [f32; 3],
    /// How far the material is lifted towards the tint (0: the prototype's colours).
    tint_amount: f32,
    halo: [f32; 3],
    halo_amount: f32,
    speed: f64,
}

const CALM: Cue = Cue { tint: [0.80, 0.88, 1.0], tint_amount: 0.25, halo: [0.55, 0.75, 1.0], halo_amount: 0.0, speed: 0.5 };
const LISTENING: Cue = Cue { tint: [0.55, 0.92, 1.0], tint_amount: 0.55, halo: [0.35, 0.85, 1.0], halo_amount: 0.55, speed: 0.8 };
const REPLYING: Cue = Cue { tint: [0.64, 0.62, 1.0], tint_amount: 0.5, halo: [0.50, 0.48, 1.0], halo_amount: 0.75, speed: 0.8 };
/// An alert (news nobody asked for), in warm amber, after its bell.
const ANNOUNCING: Cue = Cue { tint: [1.0, 0.80, 0.52], tint_amount: 0.5, halo: [1.0, 0.70, 0.32], halo_amount: 0.8, speed: 0.8 };

impl Cue {
    /// Eases towards `to` by `k` (0..1).
    fn ease(&mut self, to: &Cue, k: f64) {
        let kf = k as f32;
        for i in 0..3 {
            self.tint[i] += (to.tint[i] - self.tint[i]) * kf;
            self.halo[i] += (to.halo[i] - self.halo[i]) * kf;
        }
        self.tint_amount += (to.tint_amount - self.tint_amount) * kf;
        self.halo_amount += (to.halo_amount - self.halo_amount) * kf;
        self.speed += (to.speed - self.speed) * k;
    }
}

enum Cmd {
    Phase(String),
    /// The pointer went down on the orb.
    Press,
    /// Carried (the window is dragged) or let go.
    Carry(bool),
    /// The window moved by this much (points, y up) while carried.
    Moved(f64, f64),
    /// The brain took a step: a small poke somewhere on the rim.
    Nudge,
}

struct Shared {
    cmds: Vec<Cmd>,
    /// The window, in screen points (AppKit, y up).
    frame: mac::Rect,
    dark: bool,
}

/// The orb's drawing; stops when dropped.
pub struct Renderer {
    shared: Arc<Mutex<Shared>>,
    stop: Arc<AtomicBool>,
    thread: Option<JoinHandle<()>>,
}

impl Renderer {
    /// Puts a Metal layer under GPUI's view in `win` and starts drawing.
    pub fn attach(win: mac::Win, dark: bool, phase: &str) -> Option<Self> {
        let device = Device::system_default()?;
        let layer = MetalLayer::new();
        layer.set_device(&device);
        layer.set_pixel_format(MTLPixelFormat::BGRA8Unorm);
        layer.set_opaque(false);
        layer.set_framebuffer_only(true);
        let scale = win.attach_layer_below(layer.as_ptr() as *mut c_void);
        let frame = win.frame();
        layer.set_contents_scale(scale);
        let (w, h) = ((frame.w * scale).round() as u64, (frame.h * scale).round() as u64);
        layer.set_drawable_size(core_graphics_types::geometry::CGSize::new(w as f64, h as f64));

        let shared = Arc::new(Mutex::new(Shared { cmds: vec![Cmd::Phase(phase.to_string())], frame, dark }));
        let stop = Arc::new(AtomicBool::new(false));
        let thread = {
            let (shared, stop) = (shared.clone(), stop.clone());
            std::thread::Builder::new()
                .name("orb-render".into())
                .spawn(move || {
                    if let Err(e) = run(device, layer, (w, h), shared, stop) {
                        eprintln!("[orb] drawing stopped: {e}");
                    }
                })
                .ok()?
        };
        Some(Self { shared, stop, thread: Some(thread) })
    }

    fn send(&self, cmd: Cmd) {
        self.shared.lock().unwrap().cmds.push(cmd);
    }

    pub fn phase(&self, phase: &str) {
        self.send(Cmd::Phase(phase.to_string()));
    }

    pub fn press(&self) {
        self.send(Cmd::Press);
    }

    pub fn nudge(&self) {
        self.send(Cmd::Nudge);
    }

    pub fn carry(&self, on: bool) {
        self.send(Cmd::Carry(on));
    }

    /// The window moved to `frame`; `carried`: by a drag.
    pub fn moved(&self, frame: mac::Rect, carried: bool) {
        let mut s = self.shared.lock().unwrap();
        if carried {
            let (dx, dy) = (frame.x - s.frame.x, frame.y - s.frame.y);
            s.cmds.push(Cmd::Moved(dx, dy));
        }
        s.frame = frame;
    }

    pub fn set_dark(&self, dark: bool) {
        self.shared.lock().unwrap().dark = dark;
    }
}

impl Drop for Renderer {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::Relaxed);
        if let Some(t) = self.thread.take() {
            let _ = t.join();
        }
    }
}

/// The shader's uniforms (`struct U` in orb.metal).
#[repr(C)]
#[derive(Clone, Copy)]
struct Uniforms {
    rot: [[f32; 4]; 3],
    center: [f32; 4],
    sh: [f32; 48],
    boil: [f32; 4],
    hot: [f32; 4],
    shape: [f32; 4],
    wall: [f32; 4],
    p1: [f32; 4],
    p2: [f32; 4],
    res: [f32; 4],
    out: [f32; 4],
    tint: [f32; 4],
    halo: [f32; 4],
}

struct Gpu {
    queue: CommandQueue,
    scene: RenderPipelineState,
    present: RenderPipelineState,
    /// The motion trail's feedback textures: last frame and this one.
    frames: [Texture; 2],
}

fn setup(device: &Device, (w, h): (u64, u64)) -> Result<Gpu, String> {
    let src = include_str!("orb.metal").replace("{{LIQUID}}", &liquid_msl());
    let opts = CompileOptions::new();
    // The noise hashes large sines; keep them as precise as WebGL's.
    opts.set_fast_math_enabled(false);
    let lib = device.new_library_with_source(&src, &opts)?;
    let vertex = lib.get_function("fullscreen", None)?;
    let pipeline = |frag: &str, format: MTLPixelFormat| -> Result<RenderPipelineState, String> {
        let d = RenderPipelineDescriptor::new();
        d.set_vertex_function(Some(&vertex));
        let f = lib.get_function(frag, None)?;
        d.set_fragment_function(Some(&f));
        d.color_attachments().object_at(0).ok_or("no color attachment")?.set_pixel_format(format);
        device.new_render_pipeline_state(&d)
    };
    let scene = pipeline("scene", MTLPixelFormat::RGBA16Float)?;
    let present = pipeline("present", MTLPixelFormat::BGRA8Unorm)?;
    let texture = || {
        let d = TextureDescriptor::new();
        d.set_pixel_format(MTLPixelFormat::RGBA16Float);
        d.set_width(w);
        d.set_height(h);
        d.set_storage_mode(MTLStorageMode::Private);
        d.set_usage(MTLTextureUsage::RenderTarget | MTLTextureUsage::ShaderRead);
        device.new_texture(&d)
    };
    Ok(Gpu { queue: device.new_command_queue(), scene, present, frames: [texture(), texture()] })
}

fn run(device: Device, layer: MetalLayer, size: (u64, u64), shared: Arc<Mutex<Shared>>, stop: Arc<AtomicBool>) -> Result<(), String> {
    let gpu = setup(&device, size)?;
    let mut sim = Sim::new(Opts::default());
    let mut drive = Drive::default();
    let mut rng = super::liquid::Rng::new(11);
    let (mut cur, mut first) = (0usize, true);
    let start = Instant::now();
    let mut last = start;
    let mut time = 0.0;
    let mut fade = 1.0;
    let mut cue = CALM;

    while !stop.load(Ordering::Relaxed) {
        let now = Instant::now();
        // A pause (the Mac asleep) waits; it doesn't fast-forward.
        let dt = (now - last).as_secs_f64().min(0.1);
        last = now;
        time += dt * cue.speed;

        let (cmds, frame, dark) = {
            let mut s = shared.lock().unwrap();
            (std::mem::take(&mut s.cmds), s.frame, s.dark)
        };
        // The pointer, on the plane through the orb's home (world units, y up).
        let per_point = CAM_Z / (FOCAL * sim.o.size) / frame.w.min(frame.h);
        let (mx, my) = mac::mouse();
        sim.pointer.x = (mx - (frame.x + frame.w / 2.0)) * per_point;
        sim.pointer.y = (my - (frame.y + frame.h / 2.0)) * per_point;
        sim.pointer.over = true;
        for cmd in cmds {
            match cmd {
                Cmd::Phase(p) => drive.enter(&p, &mut sim),
                Cmd::Press => {
                    let (x, y) = (sim.pointer.x - sim.pos[0], sim.pointer.y - sim.pos[1]);
                    if x.hypot(y) < sim.o.radius * 1.1 {
                        sim.press(sim.pointer.x, sim.pointer.y);
                    }
                }
                Cmd::Carry(on) => sim.pointer.down = on,
                Cmd::Moved(dx, dy) => sim.carried(dx * per_point, dy * per_point),
                Cmd::Nudge => sim.poke(None, 0.12),
            }
        }
        drive.tick(&mut sim);
        sim.simulate_to(time);
        fade += (drive.fade(sim.t) - fade) * (1.0 - (-dt / 0.25).exp());
        cue.ease(&drive.cue(), 1.0 - (-dt / 0.3).exp());
        let mut shown = cue;
        shown.halo_amount *= drive.halo_pulse(&sim, (now - start).as_secs_f64()) as f32;

        let keep = if first { 0.0 } else { sim.o.trail.powf(dt * 60.0) };
        let u = uniforms(&sim, size, [rng.next() as f32 - 0.5, rng.next() as f32 - 0.5], keep as f32, dark, fade as f32, &shown);
        objc::rc::autoreleasepool(|| draw(&gpu, &layer, &u, cur));
        cur = 1 - cur;
        first = false;

        let spent = now.elapsed();
        let frame_time = Duration::from_secs_f64(1.0 / MAX_FPS);
        if spent < frame_time {
            std::thread::sleep(frame_time - spent);
        }
    }
    Ok(())
}

fn uniforms(sim: &Sim, (w, h): (u64, u64), jitter: [f32; 2], keep: f32, dark: bool, fade: f32, cue: &Cue) -> Uniforms {
    let m = sim.rot;
    let col = |j: usize| [m[j] as f32, m[3 + j] as f32, m[6 + j] as f32, 0.0];
    let o = &sim.o;
    let hot = sim.hotspot_dir();
    // Dark screens get the prototype's glow look (black backdrop), light ones its
    // light look (beige). HERDER_TOWER_ORB_LOOK=light|glow picks one regardless.
    let dark = match std::env::var("HERDER_TOWER_ORB_LOOK").as_deref() {
        Ok("light") => false,
        Ok("glow") => true,
        _ => dark,
    };
    let glow = if dark { 1.0 } else { 0.0 };
    let edge = if dark { 2.0 } else { 0.0 };
    Uniforms {
        rot: [col(0), col(1), col(2)],
        center: [sim.pos[0] as f32, sim.pos[1] as f32, sim.pos[2] as f32, sim.bound() as f32],
        sh: sim.coeffs(),
        boil: [sim.boil as f32, (7.0 / o.bubble_size.max(0.1)) as f32, sim.boil_phase as f32, 0.0],
        hot: [hot[0] as f32, hot[1] as f32, hot[2] as f32, sim.hotspot_spread() as f32],
        shape: [o.radius as f32, o.opening as f32, o.rim as f32, 0.0],
        wall: [o.wall[0] as f32, o.wall[1] as f32, o.wall[2] as f32, 0.0],
        p1: [o.density as f32, o.specular as f32, o.step_scale as f32, (FOCAL * o.size) as f32],
        p2: [o.bubble_shape.max(0.5) as f32, glow, edge, keep],
        res: [w as f32, h as f32, jitter[0], jitter[1]],
        out: [fade, 0.0, 0.0, 0.0],
        tint: [cue.tint[0], cue.tint[1], cue.tint[2], cue.tint_amount],
        halo: [cue.halo[0], cue.halo[1], cue.halo[2], cue.halo_amount],
    }
}

fn draw(gpu: &Gpu, layer: &MetalLayer, u: &Uniforms, cur: usize) {
    let Some(drawable) = layer.next_drawable() else { return };
    let (src, dst) = (&gpu.frames[1 - cur], &gpu.frames[cur]);
    let bytes = std::mem::size_of::<Uniforms>() as u64;
    let ptr = u as *const Uniforms as *const c_void;
    let cb = gpu.queue.new_command_buffer();

    let pass = RenderPassDescriptor::new();
    let att = pass.color_attachments().object_at(0).unwrap();
    att.set_texture(Some(dst));
    att.set_load_action(MTLLoadAction::DontCare);
    att.set_store_action(MTLStoreAction::Store);
    let enc = cb.new_render_command_encoder(pass);
    enc.set_render_pipeline_state(&gpu.scene);
    enc.set_fragment_bytes(0, bytes, ptr);
    enc.set_fragment_texture(0, Some(src));
    enc.draw_primitives(MTLPrimitiveType::Triangle, 0, 3);
    enc.end_encoding();

    let pass = RenderPassDescriptor::new();
    let att = pass.color_attachments().object_at(0).unwrap();
    att.set_texture(Some(drawable.texture()));
    att.set_load_action(MTLLoadAction::Clear);
    att.set_clear_color(MTLClearColor::new(0.0, 0.0, 0.0, 0.0));
    att.set_store_action(MTLStoreAction::Store);
    let enc = cb.new_render_command_encoder(pass);
    enc.set_render_pipeline_state(&gpu.present);
    enc.set_fragment_bytes(0, bytes, ptr);
    enc.set_fragment_texture(0, Some(dst));
    enc.draw_primitives(MTLPrimitiveType::Triangle, 0, 3);
    enc.end_encoding();

    cb.present_drawable(drawable);
    cb.commit();
}

/// Metal for `float liquid(float3 n, constant U& u)`: the displacement of the
/// surface in unit direction n. Written out as straight-line code (no loops,
/// constant indices), as the prototype's liquid.js does for GLSL.
fn liquid_msl() -> String {
    let num = |v: f64| if v.fract() == 0.0 { format!("{v:.1}") } else { format!("{v}") };
    let coeff = |k: usize| format!("u.sh[{}].{}", k >> 2, ['x', 'y', 'z', 'w'][k & 3]);
    let mut lines =
        vec!["float liquid(float3 n, constant U& u) {".to_string(), "  float h = 0.0, re = 1.0, im = 0.0, t, q, q1, q2;".into()];
    let (mut k, mut qmm) = (0usize, 1.0f64);
    for m in 0..=LMAX {
        if m > 0 {
            lines.push("  t = re * n.x - im * n.y; im = re * n.y + im * n.x; re = t;".into());
            qmm *= (2 * m - 1) as f64;
        }
        lines.push("  q1 = 0.0; q2 = 0.0;".into());
        for l in m..=LMAX {
            lines.push(if l == m {
                format!("  q = {};", num(qmm))
            } else {
                let (a, b) = ((2 * l - 1) as f64 / (l - m) as f64, (l + m - 1) as f64 / (l - m) as f64);
                format!("  q = {} * n.z * q1 - {} * q2;", num(a), num(b))
            });
            lines.push("  q2 = q1; q1 = q;".into());
            if l < 2 {
                continue;
            }
            if m == 0 {
                lines.push(format!("  h += {} * q;", coeff(k)));
                k += 1;
            } else {
                lines.push(format!("  h += q * ({} * re + {} * im);", coeff(k), coeff(k + 1)));
                k += 2;
            }
        }
    }
    lines.push("  return h;".into());
    lines.push("}".into());
    lines.join("\n")
}

/// The tower's phases, played on the orb: its moods (rest 0.1, idle 0.3,
/// thinking 0.45, working 0.6, important 0.8) and moves.
///
/// - off, standby: at rest; faded (off), or breathing in and out (standby)
/// - idle: idle
/// - listening: working, boiling on the face towards you, a heartbeat each 0.7 s
/// - transcribing: very lively and spinning
/// - thinking: thinking, the opening turned up and kept there
/// - speaking: working, a quicker, softer heartbeat (0.42 s)
/// - announcing (saying an alert): as speaking, in amber, with a swell first
/// - armed: important (a swell for attention)
/// - blocked: thinking, with a swell every 3 s (and GPUI's red dot)
#[derive(Default)]
struct Drive {
    phase: String,
    since: f64,
    next: f64,
}

impl Drive {
    fn enter(&mut self, phase: &str, sim: &mut Sim) {
        if phase == self.phase {
            return;
        }
        self.phase = phase.to_string();
        self.since = sim.t;
        self.next = sim.t;
        let rest = Opts::default();
        sim.o.beat = 0.0;
        sim.o.hotspot = rest.hotspot;
        sim.o.hotspot_depth = rest.hotspot_depth;
        match phase {
            "off" | "standby" => sim.mood("rest"),
            "listening" => {
                sim.mood("working");
                sim.focus(None, 1.0);
                sim.o.beat = 1.0 / 0.7;
                sim.o.beat_strength = 0.5;
            }
            "transcribing" => {
                sim.set_energy(0.9);
                sim.spin(None, 0.5);
            }
            "thinking" => sim.mood("thinking"),
            "speaking" | "announcing" => {
                sim.mood("working");
                sim.o.beat = 1.0 / 0.42;
                sim.o.beat_strength = 0.35;
                if phase == "announcing" {
                    sim.pulse(0.35);
                }
            }
            "armed" => sim.mood("important"),
            "blocked" => sim.mood("thinking"),
            _ => sim.mood("idle"),
        }
    }

    /// Moves that repeat while a phase lasts.
    fn tick(&mut self, sim: &mut Sim) {
        if sim.t < self.next {
            return;
        }
        match self.phase.as_str() {
            "thinking" => {
                sim.look(Some(90.0), 2.5);
                self.next = sim.t + 2.4;
            }
            "blocked" => {
                if sim.t > self.since {
                    sim.pulse(0.35);
                }
                self.next = sim.t + 3.0;
            }
            "transcribing" => {
                if sim.t > self.since {
                    sim.spin(None, 0.3);
                }
                self.next = sim.t + 0.7;
            }
            _ => self.next = f64::INFINITY,
        }
    }

    /// How the orb looks in this phase.
    fn cue(&self) -> Cue {
        match self.phase.as_str() {
            "listening" | "armed" => LISTENING,
            "speaking" => REPLYING,
            "announcing" => ANNOUNCING,
            _ => CALM,
        }
    }

    /// The halo's strength now (0..1, times the cue's): listening breathes in
    /// and out once a second; replying flares on each heartbeat and fades.
    fn halo_pulse(&self, sim: &Sim, real_t: f64) -> f64 {
        match self.phase.as_str() {
            "listening" | "armed" => 0.55 + 0.45 * (real_t * std::f64::consts::TAU / 1.1).sin(),
            "speaking" | "announcing" if sim.o.beat > 0.0 => 0.35 + 0.65 * (-(sim.t * sim.o.beat).fract() * 4.0).exp(),
            _ => 1.0,
        }
    }

    /// How opaque the orb is.
    fn fade(&self, t: f64) -> f64 {
        match self.phase.as_str() {
            "off" => 0.35,
            "standby" => {
                let x = ((t - self.since) / 2.6).fract();
                let x = if x < 0.5 { x * 2.0 } else { (1.0 - x) * 2.0 };
                let e = if x < 0.5 { 4.0 * x * x * x } else { 1.0 - (-2.0 * x + 2.0).powi(3) / 2.0 };
                1.0 - 0.45 * e
            }
            _ => 1.0,
        }
    }
}
