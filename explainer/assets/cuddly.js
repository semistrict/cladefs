// Shared drawing and motion helpers for every scene: the cells are small fuzzy critters.
// Everything is seeded, so every render draws the same fur, the same blinks, the same breaths.
window.Cuddly = (function () {
  const NS = "http://www.w3.org/2000/svg";
  const INK = "#2e2440";

  function rng(seed) {
    let a = seed >>> 0;
    return function () {
      a = (a + 0x6d2b79f5) | 0;
      let t = Math.imul(a ^ (a >>> 15), 1 | a);
      t = (t + Math.imul(t ^ (t >>> 7), 61 | t)) ^ t;
      return ((t ^ (t >>> 14)) >>> 0) / 4294967296;
    };
  }

  function el(tag, attrs, parent) {
    const e = document.createElementNS(NS, tag);
    for (const k in attrs) e.setAttribute(k, attrs[k]);
    if (parent) parent.appendChild(e);
    return e;
  }

  function div(cls, parent, style) {
    const d = document.createElement("div");
    if (cls) d.className = cls;
    if (style) Object.assign(d.style, style);
    if (parent) parent.appendChild(d);
    return d;
  }

  // Mix a colour toward black (amt < 0) or white (amt > 0).
  function shade(hex, amt) {
    const n = parseInt(hex.slice(1), 16);
    const ch = [(n >> 16) & 255, (n >> 8) & 255, n & 255].map((c) =>
      Math.round(amt < 0 ? c * (1 + amt) : c + (255 - c) * amt),
    );
    return "#" + ch.map((c) => c.toString(16).padStart(2, "0")).join("");
  }

  // A fluffy outline: tufts around a slightly wide loaf.
  function furPath(r, R, tufts, fluff) {
    const pts = [];
    const step = (Math.PI * 2) / tufts;
    const off = R() * step;
    for (let i = 0; i < tufts; i++) {
      const a = off + i * step + (R() - 0.5) * step * 0.3;
      pts.push({ a, out: r * (1 + fluff * (0.55 + 0.45 * R())) });
    }
    const P = (a, rad) => [Math.cos(a) * rad * 1.06, Math.sin(a) * rad * 0.96];
    let d = "";
    for (let i = 0; i < tufts; i++) {
      const p = pts[i], q = pts[(i + 1) % tufts];
      const aq = q.a + (i + 1 === tufts ? Math.PI * 2 : 0);
      const s = P(p.a, r), e = P(aq, r);
      const c1 = P(p.a + (aq - p.a) * 0.2, p.out), c2 = P(p.a + (aq - p.a) * 0.8, p.out);
      if (i === 0) d += `M${s[0].toFixed(1)} ${s[1].toFixed(1)}`;
      d += `C${c1[0].toFixed(1)} ${c1[1].toFixed(1)} ${c2[0].toFixed(1)} ${c2[1].toFixed(1)} ${e[0].toFixed(1)} ${e[1].toFixed(1)}`;
    }
    return d + "Z";
  }

  // A critter: pos (moves) > pop (enters) > body (breathes) > fur, texture, face.
  // o: { x, y, r, color, seed, ghost, look: [dx, dy], id }
  function critter(parent, o) {
    const R = rng(o.seed || 1);
    const r = o.r;
    const ghost = !!o.ghost;
    const fill = ghost ? "#ffffff" : o.color;
    const pos = el("g", {}, parent);
    if (o.id) pos.id = o.id;
    gsap.set(pos, { x: o.x, y: o.y });
    const pop = el("g", {}, pos);
    const body = el("g", {}, pop);
    const tufts = r > 60 ? 44 : r > 30 ? 28 : 18;
    const fluff = 0.21;
    const sw = Math.max(2.2, r * 0.055);
    const stroke = ghost ? "#9a93ad" : INK;
    const dash = ghost ? `${(r * 0.18).toFixed(1)} ${(r * 0.12).toFixed(1)}` : "none";
    // feet peek out under the fur
    const feet = [-1, 1].map((s) =>
      el("ellipse", { cx: s * r * 0.36, cy: r * 0.9, rx: r * 0.22, ry: r * 0.13, fill: ghost ? "#ffffff" : shade(o.color, -0.25), stroke, "stroke-width": sw, "stroke-dasharray": dash }, body),
    );
    const furA = el("path", { d: furPath(r, R, tufts, fluff), fill, stroke, "stroke-width": sw, "stroke-linejoin": "round", "stroke-dasharray": dash }, body);
    const furB = el("path", { d: furPath(r, R, tufts, fluff), fill, stroke, "stroke-width": sw, "stroke-linejoin": "round", "stroke-dasharray": dash, opacity: 0 }, body);
    // a patch of its own colour, where a fork writes (hidden until then)
    const patch = el("path", { d: furPath(r * 0.42, R, 12, 0.25), fill: o.patchColor || "#ff9ec0", stroke: INK, "stroke-width": sw * 0.8, opacity: 0 }, body);
    gsap.set(patch, { x: -r * 0.38, y: r * 0.34, scale: 0, transformOrigin: "50% 50%" });
    // a few strokes of fur texture
    if (!ghost && r > 20) {
      const tex = el("g", { stroke: shade(o.color, -0.18), "stroke-width": sw * 0.7, "stroke-linecap": "round", fill: "none", opacity: 0.7 }, body);
      for (let i = 0; i < 5; i++) {
        const a = Math.PI * (0.15 + 0.7 * R()) + (R() < 0.5 ? 0 : Math.PI * 0.0);
        const rad = r * (0.55 + 0.25 * R());
        const x = Math.cos(a) * rad * (R() < 0.5 ? -1 : 1), y = Math.sin(a) * rad * 0.8;
        el("path", { d: `M${x.toFixed(1)} ${y.toFixed(1)}q${(r * 0.06).toFixed(1)} ${(-r * 0.12).toFixed(1)} ${(r * 0.12).toFixed(1)} ${(-r * 0.04).toFixed(1)}` }, tex);
      }
    }
    // the face
    const face = el("g", {}, body);
    const look = o.look || [0, 0];
    const eyes = [-1, 1].map((s) => {
      const g = el("g", {}, face);
      gsap.set(g, { x: s * r * 0.3, y: -r * 0.12 });
      el("ellipse", { cx: 0, cy: 0, rx: r * 0.2, ry: r * 0.25, fill: "#ffffff", stroke: ghost ? "#9a93ad" : INK, "stroke-width": sw * 0.8 }, g);
      el("circle", { cx: look[0] * r * 0.07, cy: look[1] * r * 0.07 + r * 0.04, r: r * 0.12, fill: ghost ? "#9a93ad" : INK }, g);
      el("circle", { cx: look[0] * r * 0.07 + r * 0.045, cy: look[1] * r * 0.07 - r * 0.01, r: r * 0.04, fill: "#ffffff" }, g);
      return g;
    });
    if (!ghost) {
      [-1, 1].forEach((s) => el("ellipse", { cx: s * r * 0.55, cy: r * 0.2, rx: r * 0.14, ry: r * 0.08, fill: "#ff7aa8", opacity: 0.55 }, face));
    }
    const smile = el("path", { d: `M${-r * 0.13} ${r * 0.2}q${r * 0.13} ${r * 0.14} ${r * 0.26} 0`, fill: "none", stroke: ghost ? "#9a93ad" : INK, "stroke-width": sw * 0.8, "stroke-linecap": "round" }, face);
    const ooh = el("ellipse", { cx: 0, cy: r * 0.27, rx: r * 0.08, ry: r * 0.1, fill: INK, opacity: 0 }, face);
    return { pos, pop, body, furA, furB, patch, eyes, face, smile, ooh, feet, r };
  }

  // Life: breath, boiling fur (8 redraws a second) and blinks, all between start and end.
  const square = (p) => (p < 0.5 ? 0 : 1);
  const squareInv = (p) => (p < 0.5 ? 1 : 0);
  function life(tl, c, start, end, seed) {
    const R = rng(seed || 7);
    const span = end - start;
    const bd = 0.75 + R() * 0.45;
    const reps = Math.max(1, Math.floor(span / bd) - 1);
    tl.fromTo(c.body, { scaleX: 1, scaleY: 1 }, { scaleX: 0.97, scaleY: 1.05, duration: bd, ease: "sine.inOut", yoyo: true, repeat: reps, transformOrigin: "50% 100%" }, start + R() * 0.3);
    const boils = Math.max(1, Math.floor(span / 0.25) - 1);
    const ph = R() * 0.12;
    tl.fromTo(c.furB, { opacity: 0 }, { opacity: 1, duration: 0.25, ease: square, repeat: boils }, start + ph);
    tl.fromTo(c.furA, { opacity: 1 }, { opacity: 0, duration: 0.25, ease: square, repeat: boils }, start + ph);
    let t = start + 0.6 + R() * 1.5;
    while (t < end - 0.3) {
      blink(tl, c.eyes, t);
      t += 1.6 + R() * 2.4;
    }
  }

  function blink(tl, eyes, at) {
    tl.fromTo(eyes, { scaleY: 1 }, { scaleY: 0.1, duration: 0.06, ease: "power1.in", yoyo: true, repeat: 1, transformOrigin: "50% 50%" }, at);
  }

  // A critter enters: squash, stretch up, settle.
  function popIn(tl, targets, at, opts) {
    const o = Object.assign({ duration: 0.5, stagger: 0 }, opts || {});
    tl.fromTo(targets, { scale: 0, transformOrigin: "50% 100%" }, { scale: 1, duration: o.duration, ease: "back.out(2.4)", stagger: o.stagger }, at);
  }

  // A hop along an arc, with a squash on landing. pop is the critter's pop group.
  function hop(tl, c, toX, toY, at, dur, height) {
    const d = dur || 0.7;
    const h = height || 160;
    tl.to(c.body, { scaleX: 1.15, scaleY: 0.82, duration: 0.12, ease: "power2.out", transformOrigin: "50% 100%" }, at);
    tl.to(c.pos, { x: toX, duration: d, ease: "power1.inOut" }, at + 0.1);
    tl.to(c.pop, { y: -h, duration: d / 2, ease: "power2.out", yoyo: true, repeat: 1 }, at + 0.1);
    tl.to(c.pos, { y: toY, duration: d, ease: "power1.inOut" }, at + 0.1);
    tl.to(c.body, { scaleX: 0.9, scaleY: 1.12, duration: 0.15, ease: "power2.out" }, at + 0.12);
    tl.to(c.body, { scaleX: 1.18, scaleY: 0.8, duration: 0.1, ease: "power2.in" }, at + 0.1 + d - 0.05);
    tl.to(c.body, { scaleX: 1, scaleY: 1, duration: 0.5, ease: "elastic.out(1, 0.4)" }, at + 0.1 + d + 0.05);
  }

  // A VM: a rounded window with a face on its screen.
  function vm(parent, o) {
    const w = o.w, h = o.h;
    const s = w / 108;
    const root = div("vm", parent, { left: o.x + "px", top: o.y + "px", width: w + "px", height: h + "px", borderWidth: Math.max(2.5, 4 * s) + "px", borderRadius: 16 * s + "px" });
    if (o.id) root.id = o.id;
    const bar = div("vm-bar", root, { height: 14 * s + "px" });
    const face = div("vm-face", root, { top: 14 * s + "px" });
    const eyes = [-1, 1].map((d) => div("vm-eye", face, { width: 8 * s + "px", height: 10 * s + "px", left: `calc(50% + ${d * 14 * s - 4 * s}px)`, top: 14 * s + "px" }));
    const mouth = div("vm-mouth", face, { width: 18 * s + "px", height: 9 * s + "px", left: `calc(50% - ${9 * s}px)`, top: 30 * s + "px", borderWidth: Math.max(2, 3 * s) + "px" });
    return { root, bar, face, eyes, mouth };
  }

  // A friendly pail for the bucket.
  function bucket(parent, o) {
    const g = el("g", {}, parent);
    if (o.id) g.id = o.id;
    gsap.set(g, { x: o.x, y: o.y });
    const w = o.w, h = o.h || o.w * 0.8;
    el("path", { d: `M${-w * 0.38} ${-h * 0.42} Q0 ${-h * 1.15} ${w * 0.38} ${-h * 0.42}`, fill: "none", stroke: INK, "stroke-width": 5, "stroke-linecap": "round" }, g);
    el("path", { d: `M${-w / 2} ${-h * 0.35} L${-w * 0.38} ${h / 2 - 14} Q${-w * 0.37} ${h / 2} ${-w * 0.3} ${h / 2} L${w * 0.3} ${h / 2} Q${w * 0.37} ${h / 2} ${w * 0.38} ${h / 2 - 14} L${w / 2} ${-h * 0.35} Z`, fill: "#b9a4ff", stroke: INK, "stroke-width": 5, "stroke-linejoin": "round" }, g);
    el("ellipse", { cx: 0, cy: -h * 0.35, rx: w / 2, ry: h * 0.1, fill: "#d9ccff", stroke: INK, "stroke-width": 5 }, g);
    return g;
  }

  // Words in: each word pops up and lands still.
  function words(tl, sel, at) {
    tl.fromTo(sel + " .w", { opacity: 0, y: 28, scale: 0.85 }, { opacity: 1, y: 0, scale: 1, duration: 0.38, ease: "back.out(2.2)", stagger: 0.06 }, at);
  }
  function wordsOut(tl, sel, at) {
    tl.to(sel + " .w", { opacity: 0, y: -18, duration: 0.2, ease: "power2.in", stagger: 0.03 }, at);
  }

  function count(tl, node, from, to, at, dur) {
    const c = { n: from };
    tl.fromTo(c, { n: from }, { n: to, duration: dur, ease: "none", onUpdate: () => { node.textContent = Math.round(c.n); } }, at);
  }

  return { NS, INK, rng, el, div, shade, critter, life, blink, popIn, hop, vm, bucket, words, wordsOut, count };
})();
