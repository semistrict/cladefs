// This film's helpers, on top of Cuddly (shared/cuddly.js). Seeded and timeline-driven, as Cuddly is.
window.SV = (function () {
  const C = window.Cuddly;
  const INK = C.INK;
  const ACCENT = C.ACCENT;

  // A flag worn like a party hat, on a critter's body (the overview's s9 "v1" flag, with any text).
  function flag(c, text) {
    const r = c.r;
    const g = C.el("g", {}, c.body);
    const x = r * 0.2, top = -r * 2.05, w = Math.max(r * 1.1, 26 + text.length * 17);
    C.el("path", { d: `M${x} ${-r * 1.02} V${top}`, stroke: INK, "stroke-width": 6, "stroke-linecap": "round" }, g);
    C.el("path", { d: `M${x} ${top} L${x + w} ${top + 14} L${x + w} ${top + 46} L${x} ${top + 52} Z`, fill: ACCENT, stroke: INK, "stroke-width": 5, "stroke-linejoin": "round" }, g);
    const t = C.el("text", { x: x + 12, y: top + 37, "font-family": "Nunito", "font-weight": 900, "font-size": 28, fill: "#ffffff" }, g);
    t.textContent = text;
    return g;
  }

  // A path drawn in by its dash, as every scene of the series draws its threads.
  function thread(svg, d, o) {
    const p = Object.assign({ stroke: INK, width: 5, dots: false }, o || {});
    const attrs = { d, fill: "none", stroke: p.stroke, "stroke-width": p.width, "stroke-linecap": "round" };
    if (p.dots) attrs["stroke-dasharray"] = "1 14";
    else attrs.pathLength = 100;
    return C.el("path", attrs, svg);
  }

  // Timeline verbs every scene uses.
  function verbs(tl) {
    return {
      pop: (t, at, st) => tl.fromTo(t, { scale: 0, transformOrigin: "50% 100%" }, { scale: 1, duration: 0.45, ease: "back.out(2.4)", stagger: st || 0 }, at),
      fade: (t, at, st) => tl.fromTo(t, { opacity: 0, y: 10 }, { opacity: 1, y: 0, duration: 0.3, ease: "back.out(2)", stagger: st || 0 }, at),
      out: (t, at, st) => tl.to(t, { opacity: 0, duration: 0.3, ease: "power2.in", stagger: st || 0 }, at),
      dots: (t, at, st) => tl.fromTo(t, { opacity: 0 }, { opacity: 1, duration: 0.3, ease: "power2.out", stagger: st || 0 }, at),
      // A chip that says one step after another, in one place: each word pops in as the last goes.
      steps: (els, ats) => els.forEach((e, i) => {
        tl.fromTo(e, { scale: 0, transformOrigin: "50% 50%" }, { scale: 1, duration: 0.35, ease: "back.out(2.6)" }, ats[i]);
        if (i + 1 < els.length) tl.to(e, { scale: 0, duration: 0.15, ease: "back.in(2)" }, ats[i + 1] - 0.15);
      }),
      flash: (t, color, at, st) => {
        tl.fromTo(t, { backgroundColor: "#f6f2ff", y: 0 }, { backgroundColor: color, y: -8, duration: 0.2, ease: "power2.out", stagger: st || 0 }, at);
        tl.to(t, { y: 0, duration: 0.4, ease: "bounce.out", stagger: st || 0 }, at + 0.2);
      },
    };
  }

  return { ACCENT, flag, thread, verbs };
})();
