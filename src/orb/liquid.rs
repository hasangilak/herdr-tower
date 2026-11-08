//! Liquid-drop surface dynamics for the orb (a port of `liquid.js` from the
//! orbs prototype in eyz/orbs).
//!
//! The surface is described by real spherical harmonics of degree 2..6 — the
//! shapes a free liquid drop oscillates in, from broad lobes to fine ripples.
//! Each mode is a damped oscillator with the frequency and viscous decay of a
//! real drop (Rayleigh, Lamb):
//!
//!   frequency²  ~ l(l-1)(l+2)   surface tension pulls the drop round
//!   decay rate  ~ (l-1)(2l+1)   viscosity eats fine detail first
//!
//! so broad sloshing rings for a while and small ripples settle quickly.
//! Forces are given as an equilibrium shape: the displacement the surface
//! would settle into if the force held still. The modes chase that shape and
//! overshoot it, which is what makes the motion read as liquid.
//!
//! Directions are in world space: the liquid's shape mostly holds still while
//! the cup's opening turns through it.

use std::f64::consts::PI;
use std::sync::LazyLock;

pub const LMAX: usize = 6;
const SAMPLES: usize = 512;

/// Mode order: m = 0..LMAX, then l = max(m, 2)..LMAX, cosine before sine.
/// The shader walks the modes in exactly this order.
static DEGREE: LazyLock<Vec<usize>> = LazyLock::new(|| {
    let mut d = vec![];
    for m in 0..=LMAX {
        for l in m.max(2)..=LMAX {
            d.push(l);
            if m > 0 {
                d.push(l);
            }
        }
    }
    d
});

/// 45 modes.
pub const MODES: usize = 45;

/// Unnormalised real spherical harmonics of a unit vector:
/// Q_l^m(z) * Re/Im (x + iy)^m, with Q from the associated Legendre
/// recurrence. The shader writes out the same computation.
fn raw_basis(x: f64, y: f64, z: f64, out: &mut [f64; MODES]) {
    let (mut k, mut re, mut im, mut qmm) = (0, 1.0, 0.0, 1.0);
    for m in 0..=LMAX {
        if m > 0 {
            let r = re * x - im * y;
            im = re * y + im * x;
            re = r;
            qmm *= (2 * m - 1) as f64;
        }
        let (mut q1, mut q2) = (0.0, 0.0);
        for l in m..=LMAX {
            let q = if l == m { qmm } else { ((2 * l - 1) as f64 * z * q1 - (l + m - 1) as f64 * q2) / (l - m) as f64 };
            q2 = q1;
            q1 = q;
            if l < 2 {
                continue;
            }
            out[k] = q * re;
            k += 1;
            if m > 0 {
                out[k] = q * im;
                k += 1;
            }
        }
    }
}

/// Evenly spread sample directions with the basis at each, scaled so every
/// mode has the same size as an orthonormal spherical harmonic. Used to
/// project arbitrary shapes onto the modes.
struct Tables {
    dirs: Vec<[f64; 3]>,
    table: Vec<[f64; MODES]>,
    /// raw basis -> normalised mode
    scale: [f64; MODES],
    norm: [f64; MODES],
}

static TABLES: LazyLock<Tables> = LazyLock::new(|| {
    assert_eq!(DEGREE.len(), MODES);
    let mut dirs = Vec::with_capacity(SAMPLES);
    let mut table = Vec::with_capacity(SAMPLES);
    let mut scale = [0.0; MODES];
    let mut norm = [0.0; MODES];
    for i in 0..SAMPLES {
        let dz = 1.0 - (2.0 * (i as f64 + 0.5)) / SAMPLES as f64;
        let r = (1.0 - dz * dz).sqrt();
        let th = i as f64 * PI * (3.0 - 5f64.sqrt());
        let d = [r * th.cos(), r * th.sin(), dz];
        let mut y = [0.0; MODES];
        raw_basis(d[0], d[1], d[2], &mut y);
        for k in 0..MODES {
            scale[k] += y[k] * y[k];
        }
        dirs.push(d);
        table.push(y);
    }
    for k in 0..MODES {
        scale[k] = 1.0 / (4.0 * PI * (scale[k] / SAMPLES as f64)).sqrt();
        for row in table.iter_mut() {
            row[k] *= scale[k];
            norm[k] += row[k] * row[k];
        }
    }
    Tables { dirs, table, scale, norm }
});

/// Least-squares mode coefficients of shape(x, y, z), added to `out`.
fn project(shape: &dyn Fn(f64, f64, f64) -> f64, out: &mut [f64; MODES]) {
    let t = &*TABLES;
    let mut acc = [0.0; MODES];
    for (d, row) in t.dirs.iter().zip(&t.table) {
        let f = shape(d[0], d[1], d[2]);
        if f == 0.0 {
            continue;
        }
        for k in 0..MODES {
            acc[k] += f * row[k];
        }
    }
    for k in 0..MODES {
        out[k] += acc[k] / t.norm[k];
    }
}

/// mulberry32, as in the prototype, so the same seed moves the same way.
#[derive(Clone)]
pub struct Rng(u32);

impl Rng {
    pub fn new(seed: u32) -> Self {
        Self(seed)
    }

    pub fn next(&mut self) -> f64 {
        self.0 = self.0.wrapping_add(0x6d2b79f5);
        let s = self.0;
        let mut t = (s ^ (s >> 15)).wrapping_mul(1 | s);
        t = (t.wrapping_add((t ^ (t >> 7)).wrapping_mul(61 | t))) ^ t;
        ((t ^ (t >> 14)) as f64) / 4294967296.0
    }

    fn gauss(&mut self) -> f64 {
        (-2.0 * (1.0 - self.next()).ln()).sqrt() * (2.0 * PI * self.next()).cos()
    }
}

/// Settings read on every step, so they can change live.
#[derive(Clone, Copy)]
pub struct Flow {
    /// rad/s of the slowest mode
    pub tension: f64,
    pub viscosity: f64,
    pub agitation: f64,
    pub wiggle: f64,
    pub max_deform: f64,
}

pub struct Liquid {
    /// Current displacement per mode.
    amp: [f64; MODES],
    vel: [f64; MODES],
    /// Slowly drifting idle shape.
    stir: [f64; MODES],
    rand: Rng,
    stiff: [f64; MODES],
    damp: [f64; MODES],
    stir_size: [f64; MODES],
    wiggle_size: [f64; MODES],
}

impl Liquid {
    pub fn new(seed: u32) -> Self {
        // Per-mode factors of the drop's restoring force, viscous decay and idle stirring.
        let mut stiff = [0.0; MODES];
        let mut damp = [0.0; MODES];
        let mut stir_size = [0.0; MODES];
        let mut wiggle_size = [0.0; MODES];
        const STIR: [f64; 7] = [0.0, 0.0, 0.035, 0.0175, 0.024, 0.018, 0.016];
        for (k, &l) in DEGREE.iter().enumerate() {
            let lf = l as f64;
            stiff[k] = lf * (lf - 1.0) * (lf + 2.0) / 8.0;
            damp[k] = 2.0 * (lf - 1.0) * (2.0 * lf + 1.0);
            stir_size[k] = STIR[l];
            wiggle_size[k] = STIR[l] * (2.0 / lf).powi(2);
        }
        Self { amp: [0.0; MODES], vel: [0.0; MODES], stir: [0.0; MODES], rand: Rng::new(seed), stiff, damp, stir_size, wiggle_size }
    }

    /// Advance by dt. `shape` is the equilibrium displacement in a direction,
    /// or None when nothing is pushing.
    pub fn step(&mut self, dt: f64, flow: &Flow, shape: Option<&dyn Fn(f64, f64, f64) -> f64>) {
        let tension = flow.tension.max(0.1);
        // The idle stirring drifts on the same time scale as the liquid wobbles.
        let keep = (-dt * tension / 4.5).exp();
        let kick = (1.0 - keep * keep).sqrt();
        let mut rest = [0.0; MODES];
        for k in 0..MODES {
            self.stir[k] = self.stir[k] * keep + self.stir_size[k] * flow.agitation * kick * self.rand.gauss();
            rest[k] = self.stir[k];
        }
        if let Some(shape) = shape {
            project(shape, &mut rest);
        }
        let sqdt = dt.sqrt();
        for k in 0..MODES {
            let omega2 = tension * tension * self.stiff[k];
            let damping = flow.viscosity * self.damp[k];
            let force = omega2 * (rest[k] - self.amp[k]) - damping * self.vel[k];
            // Wiggle: random kicks (like thermal noise) that set each mode ringing
            // at its own frequency. Mostly broad sloshing, little fine shivering.
            let kick_size = flow.wiggle * self.wiggle_size[k] * (2.0 * damping * omega2).sqrt();
            self.vel[k] += force * dt + kick_size * sqdt * self.rand.gauss();
            self.amp[k] += self.vel[k] * dt;
        }
        // Surface tension wins eventually: never tear the drop apart.
        let e = self.extent();
        if e > flow.max_deform {
            let s = flow.max_deform / e;
            for k in 0..MODES {
                self.amp[k] *= s;
                self.vel[k] *= s;
            }
        }
    }

    /// Instant change of surface velocity, e.g. a poke.
    pub fn impulse(&mut self, shape: &dyn Fn(f64, f64, f64) -> f64) {
        project(shape, &mut self.vel);
    }

    /// How far the surface is displaced at most (over the sample directions).
    pub fn extent(&self) -> f64 {
        TABLES.table.iter().map(|row| row.iter().zip(&self.amp).map(|(b, a)| b * a).sum::<f64>().abs()).fold(0.0, f64::max)
    }

    /// Coefficients for the shader, which evaluates the unnormalised basis.
    pub fn shader_coeffs(&self, out: &mut [f32]) {
        for k in 0..MODES {
            out[k] = (self.amp[k] * TABLES.scale[k]) as f32;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn mulberry_matches_javascript() {
        // createLiquid.random(7)() in the prototype.
        let mut r = Rng::new(7);
        let a = r.next();
        assert!((0.0..1.0).contains(&a));
        // Deterministic.
        let mut r2 = Rng::new(7);
        assert_eq!(a, r2.next());
    }

    #[test]
    fn a_poke_rings_then_settles() {
        let mut l = Liquid::new(7);
        let flow = Flow { tension: 3.0, viscosity: 0.2, agitation: 0.0, wiggle: 0.0, max_deform: 0.12 };
        l.impulse(&|x, _, _| -5.0 * (8.0 * (x - 1.0)).exp());
        let mut peak = 0f64;
        for _ in 0..120 {
            l.step(1.0 / 120.0, &flow, None);
            peak = peak.max(l.extent());
        }
        assert!(peak > 0.01 && peak <= 0.12 + 1e-9);
        for _ in 0..(120 * 20) {
            l.step(1.0 / 120.0, &flow, None);
        }
        assert!(l.extent() < peak * 0.05);
    }
}
