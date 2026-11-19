// This film's own helpers, over cuddly.js: keys, padlocks, file cards that flip between plain and
// encrypted, and poofs. Seeded and timeline-driven like the rest; nothing here reads the clock.
window.Enc = (function () {
  const C = window.Cuddly;
  const INK = C.INK;
  const ACCENT = C.ACCENT;

  // A padlock, its body centred on (x, y): a shackle over a rounded body with a keyhole.
  function padlock(parent, o) {
    const g = C.el("g", {}, parent);
    if (o.id) g.id = o.id;
    gsap.set(g, { x: o.x, y: o.y, scale: o.s || 1 });
    const shackle = C.el("path", { d: "M-13 -12 V-24 A13 13 0 0 1 13 -24 V-12", fill: "none", stroke: INK, "stroke-width": 6, "stroke-linecap": "round" }, g);
    C.el("rect", { x: -22, y: -14, width: 44, height: 36, rx: 9, fill: o.color, stroke: INK, "stroke-width": 5 }, g);
    C.el("path", { d: "M0 -2 V8", stroke: INK, "stroke-width": 6, "stroke-linecap": "round" }, g);
    return { g, shackle, y: o.y };
  }

  // A padlock drops onto what it locks and settles.
  function lockOn(tl, lock, at) {
    tl.fromTo(lock.g, { opacity: 0, y: lock.y - 60 }, { opacity: 1, y: lock.y, duration: 0.35, ease: "back.out(3)" }, at);
    tl.fromTo(lock.shackle, { y: -8 }, { y: 0, duration: 0.12, ease: "power2.in" }, at + 0.3);
  }

  // A file card: a DOM box with a plain face and an encrypted face (o.show: "plain" or "cipher").
  // o: { x, y, w, h, name, plain: [lines], cipher: [lines], tint, id }
  function card(parent, o) {
    const root = C.div("enc-card", parent, { left: o.x + "px", top: o.y + "px", width: o.w + "px", height: o.h + "px" });
    if (o.id) root.id = o.id;
    const head = C.div("enc-card-head", root, { background: o.tint || "#ffd86e" });
    head.textContent = o.name || "";
    const plain = C.div("enc-card-body enc-plain", root);
    plain.innerHTML = (o.plain || []).map((l) => `<div>${l}</div>`).join("");
    const cipher = C.div("enc-card-body enc-cipher", root);
    cipher.innerHTML = (o.cipher || []).map((l) => `<div>${l}</div>`).join("");
    const showPlain = (o.show || "plain") === "plain";
    gsap.set(plain, { opacity: showPlain ? 1 : 0 });
    gsap.set(cipher, { opacity: showPlain ? 0 : 1 });
    return { root, head, plain, cipher };
  }

  // A card flips to the other face: plain to cipher (toCipher) or back.
  function flip(tl, c, toCipher, at) {
    tl.fromTo(c.root, { scaleX: 1 }, { scaleX: 0, duration: 0.13, ease: "power2.in", immediateRender: false }, at);
    tl.fromTo(c.plain, { opacity: toCipher ? 1 : 0 }, { opacity: toCipher ? 0 : 1, duration: 0.01, immediateRender: false }, at + 0.13);
    tl.fromTo(c.cipher, { opacity: toCipher ? 0 : 1 }, { opacity: toCipher ? 1 : 0, duration: 0.01, immediateRender: false }, at + 0.13);
    tl.fromTo(c.root, { scaleX: 0 }, { scaleX: 1, duration: 0.15, ease: "back.out(2)", immediateRender: false }, at + 0.14);
  }

  // A poof of five puffs at (x, y), hidden until played.
  function poof(parent, x, y, s) {
    const g = C.el("g", {}, parent);
    gsap.set(g, { x, y, scale: s || 1, opacity: 0 });
    [[-30, -10, 22], [0, -30, 26], [30, -8, 22], [-14, 18, 20], [18, 20, 20]].forEach(([px, py, r]) =>
      C.el("circle", { cx: px, cy: py, r, fill: "#ffffff", stroke: "#9a93ad", "stroke-width": 3 }, g),
    );
    return g;
  }

  // A critter goes poof and leaves its dashed ghost (which the caller drew, hidden, at the same seat).
  function goPoof(tl, critter, ghost, puff, at) {
    tl.to(critter.pop, { scale: 0, duration: 0.15, ease: "power2.in" }, at);
    tl.fromTo(puff, { opacity: 1, scale: 0.4, transformOrigin: "50% 50%" }, { opacity: 0, scale: 1.4, duration: 0.5, ease: "power2.out", immediateRender: false }, at + 0.05);
    tl.fromTo(ghost.pop, { scale: 0, opacity: 0.5 }, { scale: 1, opacity: 0.5, duration: 0.35, ease: "back.out(2)", immediateRender: false }, at + 0.25);
  }

  // A dotted link from (x1, y1) to (x2, y2), bowed by bend px.
  function link(parent, x1, y1, x2, y2, color, bend) {
    const b = bend || 0;
    const mx = (x1 + x2) / 2, my = (y1 + y2) / 2 - b;
    return C.el("path", { d: `M${x1} ${y1} Q${mx} ${my} ${x2} ${y2}`, fill: "none", stroke: color || "#c89a1c", "stroke-width": 4, "stroke-dasharray": "2 12", "stroke-linecap": "round" }, parent);
  }

  return { ACCENT, padlock, lockOn, card, flip, poof, goPoof, link };
})();
