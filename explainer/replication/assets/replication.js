// This film's stage: four hosts in a row over the bucket, each with its disk, and the beads of
// cell 9's log on those disks. Every scene draws the same stage at the same places, so a seam
// between two scenes keeps the picture still. Loaded after cuddly.js.
window.Rep = (function () {
  const C = window.Cuddly;
  const X = { A: 120, B: 540, C: 960, D: 1380 };
  const TOP = 250;
  const PAIL = [960, 900];
  const INTO_PAIL = [960, 850];
  const COLORS = { peach: "#ffb088", rose: "#ff9ec0", lilac: "#b9a4ff", mint: "#8fddc2", butter: "#ffd86e", sky: "#8cc8ff", accent: C.ACCENT };
  const OLD = [COLORS.mint, COLORS.sky, COLORS.lilac];

  // Where commit i of a host's disk sits.
  const slot = (h, i) => [X[h] + 64 + i * 44, TOP + 454];
  // The sandbox's middle, where a commit leaves from.
  const fromSandbox = (h) => [X[h] + 94, TOP + 112];

  // A host: its dashed outline, its name, its disk, and a role tag (hidden until a scene pops it).
  function host(root, h, p, roles) {
    const box = C.div("rp-host", root, { left: X[h] + "px", top: TOP + "px" });
    box.id = `${p}-h${h}`;
    C.div("rp-host-name", box).textContent = "host " + h;
    const disk = C.div("rp-disk", box);
    disk.id = `${p}-d${h}`;
    C.div("rp-disk-name", disk).textContent = "disk";
    const tags = {};
    (roles || []).forEach((r) => {
      const t = C.div("rp-tag rp-role", box);
      t.id = `${p}-${h}-${r}`;
      t.textContent = r;
      tags[r] = t;
    });
    return { box, disk, tags };
  }

  // A tag at a place in the scene, hidden until popped.
  function tag(root, id, text, x, y, cls) {
    const t = C.div("rp-tag" + (cls ? " " + cls : ""), root, { left: x + "px", top: y + "px" });
    t.id = id;
    t.textContent = text;
    return t;
  }

  // A tag set right-aligned against the pail's left side.
  function beside(t) {
    Object.assign(t.style, { left: "auto", right: "1060px" });
    return t;
  }

  function sandbox(root, h, p) {
    return C.vm(root, { x: X[h] + 34, y: TOP + 64, w: 120, h: 96, id: `${p}-sb${h}` });
  }

  function cell(svg, h, look) {
    return C.critter(svg, { x: X[h] + 310, y: TOP + 300, r: 54, color: COLORS.peach, seed: 909, look: look || [-0.4, 0.2] });
  }

  // A bead: one commit, at a point.
  function bead(svg, at, color, r) {
    const g = C.el("g", {}, svg);
    gsap.set(g, { x: at[0], y: at[1] });
    const rad = r || 15;
    C.el("circle", { cx: 0, cy: 0, r: rad, fill: color, stroke: C.INK, "stroke-width": rad > 10 ? 5 : 4 }, g);
    return g;
  }

  // The bucket, its name and its head (hidden until popped).
  function pail(root, svg, p, heads) {
    const g = C.bucket(svg, { x: PAIL[0], y: PAIL[1], w: 170, h: 130, id: `${p}-pail` });
    // Left of the pail, so that uploads from the right pass under no words.
    const name = C.div("label", root, { right: "1060px", top: "838px" });
    name.id = `${p}-bucket-name`;
    name.textContent = "bucket";
    const hs = (heads || []).map((text, i) => beside(tag(root, `${p}-head${i}`, "head: " + text, 0, 886, "mono")));
    return { g, name, heads: hs };
  }

  // A quadratic arc from a to b, rising `lift` px above the higher end (a negative lift dips
  // below the lower one, to pass under a host): the path, for a trail.
  function arcPath(a, b, lift) {
    const c = [(a[0] + b[0]) / 2, lift >= 0 ? Math.min(a[1], b[1]) - lift : Math.max(a[1], b[1]) - lift];
    return { c, d: `M${a[0]} ${a[1]} Q${c[0]} ${c[1]} ${b[0]} ${b[1]}` };
  }

  // Flies g along the arc from a to b. g is a copy of its own, shown only from `at` on.
  function fly(tl, g, a, b, lift, at, dur, ease) {
    const { c } = arcPath(a, b, lift);
    const o = { t: 0 };
    gsap.set(g, { x: a[0], y: a[1], opacity: 0 });
    tl.fromTo(g, { opacity: 0 }, { opacity: 1, duration: 0.08, immediateRender: false }, at);
    tl.fromTo(o, { t: 0 }, {
      t: 1, duration: dur, ease: ease || "power1.inOut", immediateRender: false,
      onUpdate: () => {
        const t = o.t, u = 1 - t;
        gsap.set(g, { x: u * u * a[0] + 2 * u * t * c[0] + t * t * b[0], y: u * u * a[1] + 2 * u * t * c[1] + t * t * b[1] });
      },
    }, at);
  }

  // Drifts g in a straight line from a to b at a steady pace, from progress p0 at `at` to p1.
  function drift(tl, g, a, b, p0, p1, at, dur) {
    const o = { t: p0 };
    const put = () => gsap.set(g, { x: a[0] + (b[0] - a[0]) * o.t, y: a[1] + (b[1] - a[1]) * o.t });
    put();
    tl.fromTo(o, { t: p0 }, { t: p1, duration: dur, ease: "none", immediateRender: false, onUpdate: put }, at);
  }

  // A trail along the arc, drawn as the copy flies, then faded.
  function trail(svg, a, b, lift) {
    return C.el("path", { d: arcPath(a, b, lift).d, fill: "none", stroke: "#9a93ad", "stroke-width": 4, "stroke-dasharray": "2 12", "stroke-linecap": "round", opacity: 0 }, svg);
  }

  // A tag goes; a pair swaps.
  function out(tl, t, at) {
    tl.to(t, { opacity: 0, duration: 0.15, ease: "power2.in" }, at);
  }
  function swap(tl, a, b, at) {
    out(tl, a, at);
    C.tagIn(tl, b, at + 0.1, { immediateRender: false });
  }

  // The pail takes something in.
  function wiggle(tl, g, at) {
    tl.fromTo(g, { rotation: 0 }, { rotation: 6, duration: 0.12, ease: "sine.inOut", yoyo: true, repeat: 3, transformOrigin: "50% 100%", immediateRender: false }, at);
  }

  // A picket fence: a member refusing every older epoch.
  function fence(svg, x, y) {
    const g = C.el("g", {}, svg);
    gsap.set(g, { x, y });
    for (let i = 0; i < 4; i++) {
      const px = i * 30;
      C.el("path", { d: `M${px} 80 V14 L${px + 11} 0 L${px + 22} 14 V80 Z`, fill: "#ffffff", stroke: C.INK, "stroke-width": 4, "stroke-linejoin": "round" }, g);
    }
    C.el("path", { d: "M-10 32 H120 M-10 64 H120", stroke: C.INK, "stroke-width": 5, "stroke-linecap": "round" }, g);
    return g;
  }

  return { X, TOP, PAIL, INTO_PAIL, COLORS, OLD, slot, fromSandbox, host, tag, beside, sandbox, cell, bead, pail, arcPath, fly, drift, trail, out, swap, wiggle, fence };
})();
