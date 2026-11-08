//! The orb's motion: a port of the physics half of `orb.js` from the orbs
//! prototype (eyz/orbs), with its defaults. A hollow liquid cup turns slowly,
//! its surface driven by `liquid`; it sloshes when carried, bulges as it
//! spins, reaches for the pointer, jiggles when poked, and boils in one
//! hotspot. Energy (0..1) says how animated it is; moods set it.
//!
//! Left out (off in the prototype's defaults): drift, bounce, bob, pull, rain,
//! star ripples, `hold`, morphs, rings, ink and vapour.

use std::f64::consts::PI;

use super::liquid::{Flow, Liquid, MODES, Rng};

/// Physics time step.
pub const STEP: f64 = 1.0 / 120.0;
const NORMAL_ENERGY: f64 = 0.3;

pub type Mat = [f64; 9];
pub type V3 = [f64; 3];

/// The prototype's DEFAULTS that this port uses.
#[derive(Clone)]
pub struct Opts {
    pub energy: f64,
    pub gestures: f64,
    pub mood_strength: f64,
    pub move_strength: f64,
    pub spin: f64,
    pub spin_axis: V3,
    pub cone: f64,
    pub phase: f64,
    pub radius: f64,
    pub wall: [f64; 3],
    pub opening: f64,
    pub rim: f64,
    pub tension: f64,
    pub viscosity: f64,
    pub agitation: f64,
    pub wiggle: f64,
    pub boil: f64,
    pub bubble_size: f64,
    pub boil_speed: f64,
    pub hotspot: f64,
    pub hotspot_depth: f64,
    pub hotspot_spread: f64,
    pub hotspot_wander: f64,
    pub bubble_shape: f64,
    pub size: f64,
    pub beat: f64,
    pub beat_strength: f64,
    pub sag: f64,
    pub swirl: f64,
    pub reach: f64,
    pub poke: f64,
    pub max_deform: f64,
    pub follow: f64,
    pub density: f64,
    pub specular: f64,
    pub trail: f64,
    pub step_scale: f64,
    pub seed: u32,
}

impl Default for Opts {
    fn default() -> Self {
        Self {
            energy: 0.3,
            gestures: 0.5,
            mood_strength: 0.6,
            move_strength: 0.6,
            spin: 0.15,
            spin_axis: [-0.45, 0.3, -0.84],
            cone: 0.7,
            phase: 4.71,
            radius: 1.0,
            wall: [0.01, 0.15, 1.0],
            opening: 0.6,
            rim: 0.12,
            tension: 3.0,
            viscosity: 0.2,
            agitation: 0.5,
            wiggle: 0.4,
            boil: 0.06,
            bubble_size: 2.8,
            boil_speed: 0.8,
            hotspot: 120.0,
            hotspot_depth: 0.3,
            hotspot_spread: 0.6,
            hotspot_wander: 0.08,
            bubble_shape: 2.0,
            size: 1.0,
            beat: 0.0,
            beat_strength: 0.8,
            sag: 0.005,
            swirl: 0.002,
            reach: 0.25,
            poke: 5.0,
            max_deform: 0.12,
            follow: 7.0,
            density: 1.4,
            specular: 0.15,
            trail: 0.5,
            step_scale: 0.9,
            seed: 7,
        }
    }
}

pub const MOODS: [(&str, f64); 5] = [("rest", 0.1), ("idle", 0.3), ("thinking", 0.45), ("working", 0.6), ("important", 0.8)];

/// The pointer, on the z = 0 plane through the orb's home (world units).
#[derive(Clone, Copy, Default)]
pub struct Pointer {
    pub over: bool,
    /// Being carried (the window is dragged).
    pub down: bool,
    pub x: f64,
    pub y: f64,
}

struct Look {
    dir: V3,
    until: f64,
}

pub struct Sim {
    pub o: Opts,
    liquid: Liquid,
    flow: Flow,
    pub t: f64,
    pub pos: V3,
    vel: V3,
    pub rot: Mat,
    extra_spin: V3,
    energy: f64,
    pub boil: f64,
    pub boil_phase: f64,
    hot_angle: f64,
    wander: f64,
    look: Option<Look>,
    look_weight: f64,
    next_gesture: f64,
    beat_phase: f64,
    echo_at: f64,
    gesture_rand: Rng,
    pub pointer: Pointer,
}

impl Sim {
    pub fn new(o: Opts) -> Self {
        let flow = Flow { tension: o.tension, viscosity: o.viscosity, agitation: o.agitation, wiggle: o.wiggle, max_deform: o.max_deform };
        let mut s = Self {
            liquid: Liquid::new(o.seed),
            flow,
            t: 0.0,
            pos: [0.0; 3],
            vel: [0.0; 3],
            rot: start_pose(&o),
            extra_spin: [0.0; 3],
            energy: o.energy.clamp(0.0, 1.0),
            boil: o.boil,
            boil_phase: 0.0,
            hot_angle: o.hotspot,
            wander: 0.0,
            look: None,
            look_weight: 0.0,
            next_gesture: 0.0,
            beat_phase: 0.0,
            echo_at: 0.0,
            gesture_rand: Rng::new(o.seed + 1),
            pointer: Pointer::default(),
            o,
        };
        s.flow.tension = s.o.tension;
        s
    }

    /// Energy multiplier for the current energy and mood intensity.
    fn lively(&self, low: f64, high: f64) -> f64 {
        boost(self.energy, low, high, self.o.mood_strength)
    }

    /// Runs the physics up to time `t` in fixed steps.
    pub fn simulate_to(&mut self, t: f64) {
        while self.t + STEP <= t + 1e-9 {
            self.step(STEP);
            self.t += STEP;
        }
    }

    fn step(&mut self, dt: f64) {
        let o = self.o.clone();
        // Energy: how animated the orb is, eased so changes never jump.
        self.energy += (o.energy.clamp(0.0, 1.0) - self.energy) * (1.0 - (-dt / 2.5).exp());
        let e = self.energy;
        self.flow.tension = o.tension * self.lively(0.8, 1.35);
        self.flow.agitation = o.agitation * self.lively(0.4, 1.5);
        self.flow.wiggle = o.wiggle * self.lively(0.5, 1.5);
        self.flow.viscosity = o.viscosity;
        self.flow.max_deform = o.max_deform;
        self.boil = o.boil * self.lively(0.5, 1.4);
        self.boil_phase += o.boil_speed * self.lively(0.6, 1.6) * dt;
        // The hotspot eases to where it's sent and drifts around it.
        let turn_to = ((o.hotspot - self.hot_angle) % 360.0 + 540.0) % 360.0 - 180.0;
        self.hot_angle += turn_to * (1.0 - (-dt / 0.5).exp());
        self.wander += o.hotspot_wander * dt;

        // Now and then it moves on its own; the livelier, the more often.
        if o.gestures > 0.0 && self.t >= self.next_gesture {
            if self.next_gesture > 0.0 {
                self.gesture(e);
            }
            self.next_gesture = self.t + ((0.5 + self.gesture_rand.next()) * (9.0 - 4.0 * e)) / o.gestures;
        }

        // Heartbeat: a pulse, then a softer echo a moment later.
        if o.beat > 0.0 {
            self.beat_phase += o.beat * dt;
            if self.beat_phase >= 1.0 {
                self.beat_phase -= 1.0;
                self.swell(1.2 * o.beat_strength);
                self.echo_at = self.t + 0.22;
            }
        }
        if self.echo_at > 0.0 && self.t >= self.echo_at {
            self.swell(0.66 * o.beat_strength);
            self.echo_at = 0.0;
        }

        // The orb hangs on a damped spring: to its home, or to the pointer while held.
        let (tx, ty) = (0.0, 0.0);
        let k = o.follow * o.follow;
        let c = 2.0 * 0.45 * o.follow;
        let acc = [k * (tx - self.pos[0]) - c * self.vel[0], k * (ty - self.pos[1]) - c * self.vel[1], 0.0];
        for i in 0..3 {
            self.vel[i] += acc[i] * dt;
            self.pos[i] += self.vel[i] * dt;
        }

        // Being carried rolls it like a ball; let go and the extra spin fades.
        if self.pointer.down {
            let roll = cross([0.0, 0.0, 1.0], self.vel);
            for i in 0..3 {
                self.extra_spin[i] += (roll[i] * 0.6 - self.extra_spin[i]) * (dt * 6.0).min(1.0);
            }
        } else {
            let fade = (-dt / 1.2).exp();
            for v in &mut self.extra_spin {
                *v *= fade;
            }
        }
        // Looking at something pauses the tumble and swings the opening round.
        let looking = if self.look.as_ref().is_some_and(|l| self.t < l.until) { 1.0 } else { 0.0 };
        self.look_weight += (looking - self.look_weight) * (1.0 - (-dt / 0.6).exp());
        if looking == 0.0 && self.look_weight < 1e-3 {
            self.look = None;
        }
        let axis = normalize(o.spin_axis);
        let base = 2.0 * PI * o.spin * self.lively(0.4, 2.0) * (1.0 - self.look_weight);
        let mut spin = [0, 1, 2].map(|i| axis[i] * base + self.extra_spin[i]);
        if let Some(look) = &self.look {
            let facing = [self.rot[2], self.rot[5], self.rot[8]]; // where the opening points
            let mut turn = cross(facing, look.dir);
            let sn = len(turn);
            let cs = dot(facing, look.dir);
            if sn < 1e-6 {
                turn = if cs < 0.0 { cross(facing, [0.0, 1.0, 0.0]) } else { [0.0; 3] };
            }
            let l = len(turn);
            let gain = (2.5 * self.look_weight * sn.atan2(cs)) / if l == 0.0 { 1.0 } else { l };
            for i in 0..3 {
                spin[i] += turn[i] * gain;
            }
        }
        let rate = len(spin);
        if rate > 1e-6 {
            self.rot = orthonormalize(mul(axis_angle(spin, rate * dt), self.rot));
        }

        // Shape the liquid would settle into under the current forces.
        // Inertia: in the orb's frame the liquid feels gravity opposite to the
        // acceleration, so it stretches along it and pools on the trailing side.
        let g = acc[0].hypot(acc[1]);
        let gn = if g == 0.0 { 1.0 } else { g };
        let (gx, gy, gz) = (-acc[0] / gn, -acc[1] / gn, 0.0);
        let sag = (g * o.sag).min(o.max_deform);
        // Spin: centrifugal bulge around the equator.
        let rn = if rate == 0.0 { 1.0 } else { rate };
        let (sx, sy, sz) = (spin[0] / rn, spin[1] / rn, spin[2] / rn);
        let swirl = o.swirl * rate * rate;
        // Pointer: surface tension reaches out towards a nearby cursor.
        let (mut reach, mut r) = (0.0, [0.0, 0.0, 1.0]);
        if self.pointer.over && !self.pointer.down {
            let (dx, dy) = (self.pointer.x - self.pos[0], self.pointer.y - self.pos[1]);
            let dist = dx.hypot(dy);
            reach = o.reach * ((2.4 - dist) / 1.4).clamp(0.0, 1.0);
            r = normalize([dx, dy, (1.0 - dist * dist).max(0.0).sqrt() + 0.3]);
        }
        let shape = move |x: f64, y: f64, z: f64| {
            let mut s = 0.0;
            if sag > 1e-4 {
                let u = x * gx + y * gy + z * gz;
                s += sag * (0.75 * (1.5 * u * u - 0.5) + 0.25 * (2.5 * u * u * u - 1.5 * u));
            }
            let v = x * sx + y * sy + z * sz;
            s += swirl * (1.0 / 3.0 - v * v);
            if reach > 0.0 {
                s += reach * (6.0 * (x * r[0] + y * r[1] + z * r[2] - 1.0)).exp();
            }
            s
        };
        self.liquid.step(dt, &self.flow, Some(&shape));
    }

    // ---- Moves ----------------------------------------------------------

    pub fn set_energy(&mut self, v: f64) {
        self.o.energy = v.clamp(0.0, 1.0);
    }

    pub fn mood(&mut self, name: &str) {
        let Some(&(_, e)) = MOODS.iter().find(|(n, _)| *n == name) else { return };
        self.o.energy = e;
        if name == "thinking" {
            self.look(Some(90.0), 2.5);
        }
        if name == "important" {
            self.pulse(0.35);
        }
    }

    /// Tap the surface at an edge (degrees round the outline; None: random).
    pub fn poke(&mut self, edge: Option<f64>, strength: f64) {
        let deg = self.edge(edge);
        self.dent(edge_dir(deg, 0.35), self.o.poke * strength * self.o.move_strength);
    }

    /// Push the surface in where the pointer pressed (x, y on the orb, radii).
    pub fn press(&mut self, x: f64, y: f64) {
        let (hx, hy) = (x - self.pos[0], y - self.pos[1]);
        let h = normalize([hx, hy, (1.0 - hx * hx - hy * hy).max(0.0).sqrt()]);
        let size = self.o.poke;
        self.liquid.impulse(&|x, y, z| -size * (8.0 * (x * h[0] + y * h[1] + z * h[2] - 1.0)).exp());
    }

    /// Flick it so its front rolls towards an edge.
    pub fn spin(&mut self, edge: Option<f64>, turns: f64) {
        let deg = self.edge(edge);
        let roll = cross([0.0, 0.0, 1.0], edge_dir(deg, 0.0));
        let rate = (turns * self.o.move_strength * 2.0 * PI) / 1.2;
        for i in 0..3 {
            self.extra_spin[i] += roll[i] * rate;
        }
    }

    /// Turn the opening towards an edge (None: the front) and hold.
    pub fn look(&mut self, edge: Option<f64>, seconds: f64) {
        let dir = match edge {
            None => [0.0, 0.0, 1.0],
            Some(d) => edge_dir(d, 0.6),
        };
        self.look = Some(Look { dir, until: self.t + seconds });
    }

    /// A swell that ripples through it, for attention.
    pub fn pulse(&mut self, strength: f64) {
        self.swell(1.2 * strength * self.o.move_strength);
    }

    /// Move the boiling hotspot to an edge (None: the front).
    pub fn focus(&mut self, edge: Option<f64>, depth: f64) {
        match edge {
            None => self.o.hotspot_depth = 1.0,
            Some(d) => {
                self.o.hotspot = d;
                self.o.hotspot_depth = depth;
            }
        }
    }

    /// The window was moved by (dx, dy) world units while carried: the orb
    /// stays behind for a moment and the spring brings it back.
    pub fn carried(&mut self, dx: f64, dy: f64) {
        self.pos[0] = (self.pos[0] - dx).clamp(-0.45, 0.45);
        self.pos[1] = (self.pos[1] - dy).clamp(-0.45, 0.45);
    }

    fn edge(&mut self, edge: Option<f64>) -> f64 {
        edge.unwrap_or_else(|| self.gesture_rand.next() * 360.0)
    }

    /// Push the surface in around direction d.
    fn dent(&mut self, d: V3, size: f64) {
        self.liquid.impulse(&|x, y, z| -size * (8.0 * (x * d[0] + y * d[1] + z * d[2] - 1.0)).exp());
    }

    /// Squeeze it front-to-back so the outline swells, then rings.
    fn swell(&mut self, size: f64) {
        self.liquid.impulse(&|_, _, z| -size * (1.5 * z * z - 0.5));
    }

    /// A small spontaneous move: mostly soft pokes, sometimes a glance.
    fn gesture(&mut self, e: f64) {
        let pick = self.gesture_rand.next();
        let deg = self.gesture_rand.next() * 360.0;
        if pick < 0.7 {
            self.poke(Some(deg), 0.08 + 0.2 * e);
        } else if pick < 0.9 {
            self.look(Some(deg), 1.2);
        } else {
            self.spin(Some(deg), 0.1 + 0.15 * e);
        }
    }

    // ---- What the shader needs --------------------------------------------

    /// Where the boiling is centred, including its slow drift.
    pub fn hotspot_dir(&self) -> V3 {
        let w = self.wander;
        let deg = self.hot_angle + 35.0 * (w * 2.1).sin() + 20.0 * (w * 3.7 + 1.0).sin();
        let depth = (self.o.hotspot_depth + 0.15 * (w * 2.9 + 2.0).sin()).clamp(0.0, 1.0);
        let a = deg.to_radians();
        let ring = (depth * PI / 2.0).cos();
        [a.cos() * ring, a.sin() * ring, (depth * PI / 2.0).sin()]
    }

    pub fn hotspot_spread(&self) -> f64 {
        self.o.hotspot_spread * self.lively(0.85, 1.2)
    }

    /// Radius of a sphere round the orb holding all of it.
    pub fn bound(&self) -> f64 {
        self.o.radius + self.o.wall[1] + self.liquid.extent() + 2.4 * self.boil + 0.1
    }

    pub fn coeffs(&self) -> [f32; 48] {
        let mut c = [0.0; 48];
        self.liquid.shader_coeffs(&mut c[..MODES]);
        c
    }
}

/// Multiplier for energy e: 1 at the normal level, `low` at 0, `high` at 1.
fn boost(e: f64, low: f64, high: f64, strength: f64) -> f64 {
    if e < NORMAL_ENERGY {
        low.powf(strength * (NORMAL_ENERGY - e) / NORMAL_ENERGY)
    } else {
        high.powf(strength * (e - NORMAL_ENERGY) / (1.0 - NORMAL_ENERGY))
    }
}

/// Direction out through an edge, tipped towards the viewer by z.
fn edge_dir(deg: f64, z: f64) -> V3 {
    let a = deg.to_radians();
    normalize([a.cos(), a.sin(), z])
}

/// Rotation taking the opening (object +z) to its starting direction:
/// `cone` radians away from the spin axis, `phase` radians around it.
fn start_pose(o: &Opts) -> Mat {
    let a = normalize(o.spin_axis);
    let u = normalize(cross(a, if a[1].abs() < 0.9 { [0.0, 1.0, 0.0] } else { [1.0, 0.0, 0.0] }));
    let v = cross(a, u);
    let (cs, sn) = (o.cone.cos(), o.cone.sin());
    let d = [0, 1, 2].map(|i| a[i] * cs + sn * (o.phase.cos() * u[i] + o.phase.sin() * v[i]));
    let mut axis = cross([0.0, 0.0, 1.0], d);
    if len(axis) < 1e-6 {
        axis[0] = 1.0;
    }
    axis_angle(axis, d[2].clamp(-1.0, 1.0).acos())
}

fn dot(a: V3, b: V3) -> f64 {
    a[0] * b[0] + a[1] * b[1] + a[2] * b[2]
}

fn len(a: V3) -> f64 {
    dot(a, a).sqrt()
}

fn normalize(v: V3) -> V3 {
    let l = len(v);
    [v[0] / l, v[1] / l, v[2] / l]
}

fn cross(a: V3, b: V3) -> V3 {
    [a[1] * b[2] - a[2] * b[1], a[2] * b[0] - a[0] * b[2], a[0] * b[1] - a[1] * b[0]]
}

/// Row-major 3x3 rotation about an arbitrary axis (Rodrigues).
fn axis_angle(axis: V3, a: f64) -> Mat {
    let [x, y, z] = normalize(axis);
    let (c, s) = (a.cos(), a.sin());
    let k = 1.0 - c;
    [
        c + x * x * k,
        x * y * k - z * s,
        x * z * k + y * s,
        y * x * k + z * s,
        c + y * y * k,
        y * z * k - x * s,
        z * x * k - y * s,
        z * y * k + x * s,
        c + z * z * k,
    ]
}

fn mul(a: Mat, b: Mat) -> Mat {
    let mut r = [0.0; 9];
    for i in 0..3 {
        for j in 0..3 {
            r[i * 3 + j] = a[i * 3] * b[j] + a[i * 3 + 1] * b[3 + j] + a[i * 3 + 2] * b[6 + j];
        }
    }
    r
}

/// Re-orthonormalise a rotation matrix (columns) so integration doesn't drift.
fn orthonormalize(m: Mat) -> Mat {
    let x = normalize([m[0], m[3], m[6]]);
    let y0 = [m[1], m[4], m[7]];
    let d = dot(x, y0);
    let y = normalize([y0[0] - d * x[0], y0[1] - d * x[1], y0[2] - d * x[2]]);
    let z = cross(x, y);
    [x[0], y[0], z[0], x[1], y[1], z[1], x[2], y[2], z[2]]
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn turns_and_stays_a_rotation() {
        let mut s = Sim::new(Opts::default());
        let r0 = s.rot;
        s.simulate_to(3.0);
        assert!(s.rot != r0);
        let c0 = [s.rot[0], s.rot[3], s.rot[6]];
        assert!((len(c0) - 1.0).abs() < 1e-9);
        assert!(s.bound() > 1.2 && s.bound() < 1.6);
    }
}
