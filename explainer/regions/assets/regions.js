// This film's own helpers, over cuddly.js: the two cloud regions with an ocean between them, pails
// wearing their bucket numbers, request pills that travel on arcs, and counters that step.
window.Regions = (function () {
  const C = window.Cuddly;
  const ACCENT = C.ACCENT;

  // Where things stand, the same in every scene, so a cut changes only what is in the world.
  const AT = {
    west: { x: 100, w: 640, name: "us-east-1" },
    east: { x: 1180, w: 640, name: "europe-west1" },
    top: 250,
    h: 600,
    ocean: { x: 760, y: 300, w: 400, h: 500 },
    pail1: { x: 420, y: 740 },
    pail2: { x: 1630, y: 740 },
    sandbox: { x: 1220, y: 330, w: 210, h: 150 },
    cell: { x: 1330, y: 690 },
  };

  // The two cloud regions as felt patches, into `land` (an empty div first in the scene, so under
  // everything), and the ocean between them, first in `svg` (the scene's svg).
  function world(land, svg) {
    [AT.west, AT.east].forEach((r) => {
      const patch = C.div("rg-patch", land, { left: r.x + "px", top: AT.top + "px", width: r.w + "px", height: AT.h + "px" });
      C.div("rg-patch-name mono", patch).textContent = r.name;
    });
    const o = AT.ocean;
    const sea = C.el("g", {}, svg);
    svg.insertBefore(sea, svg.firstChild);
    C.el("rect", { x: o.x, y: o.y, width: o.w, height: o.h, rx: 40, fill: "#d8ecff" }, sea);
    for (let row = 0; row < 5; row++) {
      const y = o.y + 70 + row * 90;
      const x0 = o.x + 40 + (row % 2) * 30;
      let d = `M${x0} ${y}`;
      for (let i = 0; i < 5; i++) d += ` q15 -16 30 0 q15 16 30 0`;
      C.el("path", { d, fill: "none", stroke: "#8cc8ff", "stroke-width": 5, "stroke-linecap": "round" }, sea);
    }
    return sea;
  }

  // A pail wearing its bucket number on an accent badge.
  function pail(svg, o) {
    const g = C.bucket(svg, { x: o.x, y: o.y, w: o.w || 200, h: o.h || 170, id: o.id });
    const badge = C.el("g", {}, g);
    C.el("circle", { cx: 0, cy: 20, r: 32, fill: ACCENT, stroke: C.INK, "stroke-width": 5 }, badge);
    C.el("text", { x: 0, y: 33, "text-anchor": "middle", "font-family": "Nunito", "font-weight": 900, "font-size": 38, fill: "#ffffff" }, badge).textContent = String(o.number);
    return { g, badge };
  }

  // A request pill travels from (x0, y0) to (x1, y1) on an arc `lift` px high; `back` brings it home.
  // The pill is a div whose top-left is its centre's offset (it sits at left 0, top 0).
  function travel(tl, pill, x0, y0, x1, y1, at, dur, lift) {
    tl.fromTo(pill, { x: x0, y: y0 }, { x: x1, duration: dur, ease: "power1.inOut", immediateRender: false }, at);
    tl.fromTo(pill, { y: y0 }, { y: Math.min(y0, y1) - (lift || 0), duration: dur / 2, ease: "power2.out", immediateRender: false }, at);
    tl.to(pill, { y: y1, duration: dur / 2, ease: "power2.in" }, at + dur / 2);
  }

  // Round trips there and back, `n` of them, each `period` long; `tick(k)` is the time trip k is home.
  function trips(tl, pill, x0, y0, x1, y1, at, n, period, lift) {
    const half = period / 2;
    for (let k = 0; k < n; k++) {
      const t = at + k * period;
      travel(tl, pill, x0, y0, x1, y1, t, half, lift);
      travel(tl, pill, x1, y1, x0, y0, t + half, half, lift);
    }
    return (k) => at + k * period;
  }

  // A counter that steps from `from` to `to`, one step every `dur / |to - from|`.
  function steps(tl, node, from, to, at, dur) {
    const c = { n: from };
    const n = Math.abs(to - from);
    tl.fromTo(c, { n: from }, { n: to, duration: dur, ease: `steps(${n})`, onUpdate: () => { node.textContent = Math.round(c.n); } }, at);
  }

  return { AT, ACCENT, world, pail, travel, trips, steps };
})();
