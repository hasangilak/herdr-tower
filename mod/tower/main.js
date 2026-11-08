// Tower: one orb, floating over the window. It shows what the voice control
// tower is doing by how it moves; hover for what it heard or said.
//
//   click        talk (or start the tower when it's off)
//   right-click  show the tower's own terminal
//   Control      talk from anywhere (the `tower` daemon listens for it)
//
// The work happens in the `tower` program (Rust). This mod reads the state it
// writes (~/.local/state/herder-tower/ui/state.json) and runs `tower <cmd>`.
// Keys (keys.json): "tower.talk", "tower.hush", "tower.mode", "tower.show".

import { View, div, image } from "gpui-kit";
import { exec, hc, home, readTextFile, watchFolder, theme, settings, run, terminals, nextCommand, notify } from "herdr";

const SIZE = 44; // the orb, px
const BOX = 58; // room for it to breathe

// How the orb moves in each phase.
const MOTION = {
  speaking: { grow: 6, period: 420, label: "Speaking" },
  announcing: { grow: 6, period: 420, label: "Heads up" },
  listening: { grow: 8, period: 700, label: "Listening" },
  transcribing: { spin: 700, label: "Understanding" },
  thinking: { spin: 2600, label: "Thinking" },
  armed: { grow: 4, period: 500, label: "Yes?" },
  blocked: { label: "Waiting for your answer" },
  standby: { fade: 2600, label: "Listening for “tower”" },
  idle: { label: "Ready" },
  off: { label: "Off · click to start" },
};

export default class Tower extends View {
  init(_props, cx) {
    this.dir = `${home()}/.local/state/herder-tower/ui`;
    this.bin = `${home()}/.local/bin/tower`;
    this.state = null;
    this.alive = false;
    this.starting = false;
    cx.spawn(async (cx) => {
      await this.read();
      cx.notify();
      // The daemon rewrites one file whenever something changes.
      for (;;) {
        try {
          await watchFolder(this.dir);
        } catch (_e) {
          await exec("mkdir", ["-p", this.dir]).catch(() => null);
        }
        await this.read();
        cx.notify();
      }
    });
    // A daemon that died without saying so.
    cx.timer.every(15000, (cx) => this.checkAlive(cx));
    cx.spawn(async (cx) => {
      for (;;) {
        const c = await nextCommand();
        await this.command(c, cx);
        cx.notify();
      }
    });
  }

  async read() {
    const text = await readTextFile(`${this.dir}/state.json`).catch(() => null);
    try {
      this.state = text ? JSON.parse(text) : null;
    } catch (_e) {
      return;
    }
    this.alive = Boolean(this.state && this.state.running !== false);
    if (this.alive) this.starting = false;
  }

  checkAlive(cx) {
    const pid = this.state?.pid;
    if (!pid || !this.alive) return;
    cx.spawn(async (cx) => {
      const r = await exec("kill", ["-0", String(pid)]).catch(() => ({ code: 1 }));
      if ((r.code === 0) !== this.alive) {
        this.alive = r.code === 0;
        cx.notify();
      }
    });
  }

  async tower(args) {
    const r = await exec(this.bin, args).catch((e) => ({ code: 1, stderr: String(e) }));
    if (r.code !== 0) notify({ title: "Tower", body: (r.stderr || r.stdout || "failed").trim().replace(/^tower: /, ""), level: "error" });
    return r;
  }

  async command(c, cx) {
    switch (c) {
      case "talk":
        return this.alive ? this.tower(["talk"]) : this.start(cx);
      case "hush":
        return this.tower(["hush"]);
      case "cancel":
        return this.tower(["cancel"]);
      case "mode": {
        const modes = ["ptt", "handsfree", "off"];
        const next = modes[(modes.indexOf(this.state?.mode ?? "ptt") + 1) % modes.length];
        return this.tower(["mode", next]);
      }
      case "show":
        return this.show();
    }
    return null;
  }

  show() {
    const brain = terminals().find((t) => t.name === "tower" && t.state === "live");
    if (!brain) return this.tower(["show"]);
    if (brain.labels?.["herdr.space"]) run("agent.focus", { terminal: brain.address });
    else run("terminal.keep", { terminal: brain.address, place: "tab" });
    return null;
  }

  // `tower up` waits for the models: run it in a Herdr terminal so it may take
  // as long as a first download needs.
  start(cx) {
    if (this.starting) return null;
    this.starting = true;
    cx.notify();
    cx.spawn(async (cx) => {
      const r = await hc(["run", "--bg", "--no-parent", "--no-notify", "--rm", "--name", "tower-up", "--", this.bin, "up"]);
      if (r.code !== 0) {
        this.starting = false;
        notify({ title: "Tower", body: (r.stderr || r.stdout).trim(), level: "error" });
      }
      cx.notify();
    });
    return null;
  }

  phase() {
    if (this.starting) return "thinking";
    if (!this.alive) return "off";
    return this.state?.phase ?? "idle";
  }

  // What hovering says: the phase, then the latest line either way.
  caption(phase) {
    const s = this.state ?? {};
    const parts = [this.starting ? "Starting…" : MOTION[phase]?.label ?? phase];
    if (this.alive) {
      const last = (s.said_ms ?? 0) >= (s.heard_ms ?? 0) ? s.said : s.heard && `“${s.heard}”`;
      if (s.error) parts.push(s.error);
      else if (last) parts.push(last.length > 140 ? `${last.slice(0, 140)}…` : last);
      if (s.talk_key === "needs-permission") parts.push("Allow Input Monitoring for Herdr Core to talk with Control");
      const blocked = (s.agents ?? []).filter((a) => a.status === "blocked").length;
      if (blocked) parts.push(`${blocked} agent${blocked === 1 ? "" : "s"} need${blocked === 1 ? "s" : ""} you`);
    }
    return parts.join(" · ");
  }

  // The icon. Swap this for another drawing; everything else stays.
  orb(phase) {
    const dark = settings()?.dark ?? true;
    const m = MOTION[phase] ?? MOTION.idle;
    let el = image(`mods/tower/orb-${dark ? "dark" : "light"}.png`).w(SIZE).h(SIZE).flex_none();
    if (phase === "off") el = el.opacity(0.35);
    if (m.spin) el = el.spin(m.spin);
    if (m.grow) {
      el = el.loop("width", { from: SIZE - 2, to: SIZE + m.grow, period: m.period }).loop("height", { from: SIZE - 2, to: SIZE + m.grow, period: m.period });
    }
    if (m.fade) el = el.loop("opacity", { from: 0.55, to: 1, period: m.fade });
    return el;
  }

  render(_cx) {
    const t = theme();
    const phase = this.phase();
    const needsYou = this.alive && (phase === "blocked" || (this.state?.agents ?? []).some((a) => a.status === "blocked"));
    return div()
      .id("tower-orb")
      .relative()
      .w(BOX)
      .h(BOX)
      .flex()
      .items_center()
      .justify_center()
      .cursor_pointer()
      .tooltip(this.caption(phase))
      .on_click((_e, cx) => this.command("talk", cx))
      .on_mouse_down("right", (_e, cx) => {
        cx.stop_propagation();
        this.show();
      })
      .child(this.orb(phase))
      .when(needsYou, (el) =>
        el.child(div().absolute().top(7).right(7).size(9).rounded(5).bg(t.blocked).border(2).border_color(t.background)),
      );
  }
}
