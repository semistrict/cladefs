// This film's own helpers, beside the shared critters (cuddly.js): a host shell that types its
// commands and prints their output, and accent threads that draw themselves. Seek-safe: what a
// line shows is a function of its tween's progress alone.
window.Images = (function () {
  // A command types itself out: the element's data-text, a character at a time, at `cps`.
  function type(tl, sel, at, cps) {
    const node = document.querySelector(sel);
    const text = node.dataset.text;
    node.textContent = "";
    const c = { n: 0 };
    const dur = text.length / (cps || 30);
    tl.fromTo(c, { n: 0 }, { n: text.length, duration: dur, ease: "none", onUpdate: () => { node.textContent = text.slice(0, Math.round(c.n)); } }, at);
    return at + dur;
  }

  // Output lines (or a new prompt) appear one after another, `gap` apart.
  function print(tl, sels, at, gap) {
    tl.fromTo(sels, { opacity: 0, x: -10 }, { opacity: 1, x: 0, duration: 0.18, ease: "power2.out", stagger: gap || 0.18 }, at);
  }

  // A thread from one point to another, bowed across (or, vertical, up and down), that draws
  // itself (pathLength 100).
  function thread(parent, x1, y1, x2, y2, o) {
    const opt = Object.assign({ width: 4, opacity: 1, bow: 0.5, vertical: false }, o || {});
    let d;
    if (opt.vertical) {
      const my = y1 + (y2 - y1) * opt.bow;
      d = `M${x1} ${y1} C${x1} ${my} ${x2} ${my} ${x2} ${y2}`;
    } else {
      const mx = x1 + (x2 - x1) * opt.bow;
      d = `M${x1} ${y1} C${mx} ${y1} ${mx} ${y2} ${x2} ${y2}`;
    }
    return Cuddly.el("path", { d, fill: "none", stroke: Cuddly.ACCENT, "stroke-width": opt.width, "stroke-opacity": opt.opacity, "stroke-linecap": "round", pathLength: 100 }, parent);
  }

  return { type, print, thread };
})();
