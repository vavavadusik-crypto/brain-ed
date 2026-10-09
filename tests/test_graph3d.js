// node tests/test_graph3d.js — layout and state rules of the bounded graph view,
// run without a browser: the view's drawing hooks are stubbed, its data rules are not.
// The input-routing tests dispatch real event objects through the actual installed
// listeners; they never set details.open programmatically.
"use strict";
const assert = require("assert");
const fs = require("fs");
const path = require("path");
globalThis.window = {};
new Function(fs.readFileSync(path.join(__dirname, "../brain-cli/assets/graph3d.js"), "utf8"))();
const { GraphView, project, Camera, ballOffset, anchorsFor, INTERACTIVE_SELECTOR } = window.BrainGraph;

function bareView(layout) {
  const v = Object.create(GraphView.prototype);
  Object.assign(v, { camera: new Camera(), nodes: [], index: new Map(), edges: [], relations: [], mode: "empty",
    layout, by: "group", anchors: new Map(), timeRange: null, selected: null, hover: null, expanded: new Set(),
    dirty: {}, metrics: { layoutMs: 0 }, host: {} });
  v.request = () => {}; v.fit = () => {};
  return v;
}
const clusterDoc = { by: "group", clusters: [
  { key: "a", count: 300, first_created_at: 100, last_created_at: 900 },
  { key: "b", count: 20, first_created_at: 200, last_created_at: 800 },
  { key: null, rest_clusters: 4, count: 9, first_created_at: 100, last_created_at: 900 }] };
const doc = { relations: ["calls"], nodes: Array.from({ length: 30 }, (_, i) => ({ id: "n" + i, label: "n" + i, kind: "code",
  group: i < 20 ? "a" : i < 27 ? "b" : "tiny", links: 2, created_at: 100 + i * 10 })),
  edges: Array.from({ length: 29 }, (_, i) => [i, i + 1, 0]) };

// Deterministic: the same data gives the same picture, in every layout.
for (const layout of ["group", "links", "time", "brain"]) {
  const one = bareView(layout), two = bareView(layout);
  one.setAnchors(clusterDoc); two.setAnchors(clusterDoc);
  one.showNodes(doc); two.showNodes(doc);
  assert.deepStrictEqual(one.nodes.map((n) => n.pos), two.nodes.map((n) => n.pos), layout);
  assert.ok(one.nodes.every((n) => n.pos.every(Number.isFinite)), layout);
}
// A group not among the listed clusters sits with the summed rest, not at random.
{
  const v = bareView("time"); v.setAnchors(clusterDoc); v.layout = "group";
  const rest = v.anchors.get("\u0000rest").pos, p = v.groupPosition({ id: "x", group: "tiny" });
  assert.ok(Math.hypot(p[0] - rest[0], p[1] - rest[1], p[2] - rest[2]) <= v.anchors.get("\u0000rest").r + 1e-9);
}
// Expanding never moves what is already drawn, and respects the budget.
for (const layout of ["group", "links", "brain"]) {
  const v = bareView(layout); v.setAnchors(clusterDoc); v.showNodes(doc);
  const before = v.nodes.map((n) => n.pos.slice());
  const rows = Array.from({ length: 12 }, (_, i) => ({ id: "m" + i, label: "m", kind: "memory", group: "b", degree: 1,
    created_at: 500, relation: "relates_to", direction: i % 2 ? "in" : "out" }));
  const result = v.addNeighbours({ id: "n3", label: "n3", kind: "code", group: "a", links: 2, created_at: 130 }, rows, 36);
  assert.deepStrictEqual(v.nodes.slice(0, 30).map((n) => n.pos), before, layout + " moved old nodes");
  assert.deepStrictEqual([result.added, result.skipped, v.nodes.length], [6, 6, 36]);
  assert.ok(v.expanded.has("n3"));
}
// Exact links replace edges by id; ids another space owns are dropped and the rest reindexed.
{
  const v = bareView("group"); v.setAnchors(clusterDoc); v.showNodes(doc);
  const ids = v.visibleIds();
  v.selected = "n29";
  v.dropMissing(ids, [0, 29]);
  assert.strictEqual(v.nodes.length, 28);
  assert.ok(!v.index.has("n0") && v.selected === null);
  assert.ok(v.edges.every((e) => e.a < 28 && e.b < 28));
  const kept = v.visibleIds();
  assert.strictEqual(v.applyLinks(kept, { relations: ["calls"], edges: [[0, 1, 0], [0, 1, 0], [5, 99, 0]] }), 1);
  v.clear();
  assert.deepStrictEqual([v.nodes.length, v.edges.length, v.index.size, v.mode, v.selected], [0, 0, 0, "empty", null]);
}
// Projection: the target lands mid-screen; a point behind the eye is not drawn.
{
  const cam = new Camera(); cam.target = [10, 20, 30]; cam.dist = 500; cam.ref = 500;
  const frame = cam.frame(800, 600), p = project(frame, 10, 20, 30, 4);
  assert.ok(Math.abs(p.x - 400) < 1e-6 && Math.abs(p.y - 300) < 1e-6 && Math.abs(p.r - 4) < 1e-6);
  const eye = cam.eye(), behind = [2 * eye[0] - 10, 2 * eye[1] - 20, 2 * eye[2] - 30];
  assert.strictEqual(project(frame, ...behind, 4), null);
  cam.flat = true; const flat = cam.frame(800, 600);
  assert.ok(Math.abs(project(flat, 10, 20, 30, 4).x - 400) < 1e-6);
}
for (const id of ["a", "решение_длинное", "x".repeat(2000)]) {
  const o = ballOffset(id, 50); assert.ok(Math.hypot(...o) <= 50 + 1e-9);
  assert.deepStrictEqual(o, ballOffset(id, 50));
}
assert.strictEqual(anchorsFor(clusterDoc.clusters, "month").get("a").pos[1], 0);

/* ---- input routing: interactive overlays must not be captured, graph input
   stays untouched. Real dispatch through the listeners bindInput installs. ---- */
{
  // A tiny DOM stub: just enough for closest(), focus identity and listener
  // registration; no layout, no drawing.
  function fakeEl(name, inOverlay) {
    return {
      name, inOverlay,
      closest(sel) { return this.inOverlay ? this : null; },
      addEventListener(type, fn) { (this.listeners[type] = this.listeners[type] || []).push(fn); },
      setPointerCapture(id) { this.captured = id; },
      releasePointerCapture(id) { this.released = id; },
      hasPointerCapture(id) { return this.captured === id; },
      getBoundingClientRect() { return { left: 0, top: 0, width: 800, height: 600 }; },
      style: {},
      listeners: {},
    };
  }
  const stage = fakeEl("stage", false);
  const summaryEl = fakeEl("summary", true); // a click on "Замеры" resolves here
  const view = Object.create(GraphView.prototype);
  Object.assign(view, {
    camera: new Camera(), nodes: [], index: new Map(), edges: [], relations: [],
    selected: null, hover: null, expanded: new Set(), mode: "empty", layout: "group", by: "group",
    anchors: new Map(), timeRange: null, clusterDoc: null, ratioCap: 1.5,
    dirty: {}, metrics: { frameScheduled: 0, contextLosses: 0 }, reducedMotion: true,
    pending: false, animation: null,
    host: { wrap: stage, onSelect: (id) => { view.selectedByHandler = id; },
      onExpand: (id) => { view.expandedByHandler = id; },
      onCluster: () => { view.clusteredByHandler = true; }, onHoverText: (n) => { view.hoverTextSeen = n; } },
  });
  view.request = () => {}; view.fit = () => {}; view.resize = () => {};
  GraphView.prototype.bindInput.call(view);
  const fire = (type, event) => { for (const fn of stage.listeners[type] || []) fn(event); };
  const ev = (target, x = 100, y = 100, extra = {}) => Object.assign(
    { target, clientX: x, clientY: y, button: 0, pointerId: 7, deltaY: -3, key: "",
      shiftKey: false, preventDefault() { this.defaultPrevented = true; } }, extra);

  // The bug: pointerdown on SUMMARY must NOT capture the pointer — capture is
  // what retargeted pointerup/click to the stage and swallowed the native toggle.
  const downOnSummary = ev(summaryEl);
  fire("pointerdown", downOnSummary);
  assert.strictEqual(stage.captured, undefined, "summary pointerdown must not setPointerCapture");
  fire("pointerup", ev(summaryEl));
  assert.strictEqual(view.selectedByHandler, undefined, "summary click must not open a node");
  assert.strictEqual(view.clusteredByHandler, undefined, "summary click must not drill a cluster");

  // Keys: with focus on the summary (event target), graph shortcuts stay out.
  const enterOnSummary = ev(summaryEl, 0, 0, { key: "Enter" });
  fire("keydown", enterOnSummary);
  assert.ok(!enterOnSummary.defaultPrevented, "Enter on a control keeps native meaning (summary toggles)");
  assert.strictEqual(view.expandedByHandler, undefined, "Enter on a control must not expand the selected node");
  const wheelOnPanel = ev(summaryEl);
  fire("wheel", wheelOnPanel);
  assert.ok(!wheelOnPanel.defaultPrevented, "wheel inside the panel is not hijacked");
  const menuOnSummary = ev(summaryEl);
  fire("contextmenu", menuOnSummary);
  assert.ok(!menuOnSummary.defaultPrevented, "context menu stays available on controls");

  // Graph input itself is unaffected: stage-targeted gestures still work.
  const downOnStage = ev(stage);
  fire("pointerdown", downOnStage);
  assert.strictEqual(stage.captured, 7, "stage pointerdown still captures for drags");
  view.pick = () => ({ id: "n1", kind: "code" }); // deterministic hit
  fire("pointerup", ev(stage));
  assert.strictEqual(view.selectedByHandler, "n1", "stage click still selects the picked node");
  const fKey = ev(stage, 0, 0, { key: "f" });
  fire("keydown", fKey);
  assert.ok(fKey.defaultPrevented, "graph shortcut fires when the stage owns the key");
  fire("dblclick", ev(stage));
  assert.strictEqual(view.expandedByHandler, "n1", "double click on the graph still expands");
  const wheelOnStage = ev(stage);
  fire("wheel", wheelOnStage);
  assert.ok(wheelOnStage.defaultPrevented, "wheel zoom on the graph still works");
  // Pointer capture is released on pointerup, so a click elsewhere next does not stick.
  fire("pointerdown", ev(stage));
  assert.strictEqual(stage.captured, 7);
  fire("pointerup", ev(stage));
  assert.strictEqual(stage.released, 7, "pointerup releases capture when held");
  fire("pointercancel", ev(stage, 0, 0, { pointerId: 9 }));
  // Guard never rejects: INTERACTIVE_SELECTOR is the one the handlers use.
  assert.ok(INTERACTIVE_SELECTOR.includes("summary") && INTERACTIVE_SELECTOR.includes("details"),
    "overlay selector covers the metrics panel");
}
/* ---------- the brain layout: regions by stored kind only ---------- */
{
  const B = window.BrainGraph;
  // Contract: the eight regions are exactly the kinds brain-core stores.
  const model = fs.readFileSync(path.join(__dirname, "../brain-core/src/model.rs"), "utf8");
  const stored = [...model.slice(model.indexOf("fn from_stored")).split("_ => None")[0].matchAll(/"([a-z_]+)" => Some/g)].map((m) => m[1]);
  assert.deepStrictEqual(B.BRAIN_REGIONS.map((r) => r.kind).sort(), stored.slice().sort(), "regions = NodeKind::from_stored kinds");
  assert.strictEqual(new Set(B.BRAIN_REGIONS.map((r) => r.name)).size, 8, "eight distinct region names");
  // Only the exact kind decides; text never does, odd kinds are unknown.
  assert.strictEqual(B.regionOf("code").kind, "code");
  for (const odd of ["", undefined, null, "Decision", "decision ", "__proto__", "constructor", "<img src=x onerror=alert(1)>", 7]) {
    assert.strictEqual(B.regionOf(odd), B.UNKNOWN_REGION, "unknown kind " + String(odd));
  }
  const errorWords = { id: "t1", kind: "code", label: "ошибка решение память error decision" };
  const plain = { id: "t1", kind: "code", label: "x" };
  assert.deepStrictEqual(B.brainPosition(errorWords, 1), B.brainPosition(plain, 1), "the label does not move a node");
  assert.deepStrictEqual(B.brainCounts([errorWords, { id: "u", kind: "mystery" }, { id: "e", kind: "error" }]),
    { decision: 0, concept: 0, memory: 0, session: 0, entity: 0, code: 1, document: 0, error: 1, unknown: 1 }, "counts are of the given rows only");
  // Geometry: every node is inside the solid of its region, at every scale.
  const kinds = B.BRAIN_REGIONS.map((r) => r.kind).concat(["mystery"]);
  for (const n of [9, 400, 1000]) {
    const k = B.brainScaleFor(n);
    for (let i = 0; i < n; i++) {
      const node = { id: "id" + i, kind: kinds[i % kinds.length] }, p = B.brainPosition(node, k), r = B.regionOf(node.kind);
      assert.ok(p.every(Number.isFinite));
      if (r.cerebellum) assert.ok(B.cerebellumNorm(p, k) <= 0.861, "error node inside the cerebellum");
      else if (!r.stem) {
        assert.ok(B.hemisphereNorm(p, k) <= 0.901, node.kind + " node inside a hemisphere");
        if (r.side) assert.strictEqual(Math.sign(p[2]), r.side, node.kind + " node on its own side");
        // The side view (2D) keeps it inside the profile too.
        assert.ok(Math.hypot(p[0], p[1]) / k <= B.profileRadius(Math.atan2(p[1], p[0])), node.kind + " inside the side profile");
      } else assert.ok(p[1] < -58 * k, "unknown kinds sit on the stem, below the hemispheres");
    }
  }
  // Frontal regions are in front (+x), the cerebellum behind and below.
  const at = (kind) => B.brainPosition({ id: "probe", kind }, 1);
  assert.ok(at("decision")[0] > 60 && at("concept")[0] > 60 && at("error")[0] < -60 && at("error")[1] < -30);
  // Meshes: bounded and closed with outward normals.
  for (const m of B.BRAIN_MESHES) {
    assert.ok(m.points.length <= 400 && m.faces.length <= 400, "bounded mesh");
    assert.ok(m.faces.every((f) => (f.mid[0] - m.centre[0]) * f.n[0] + (f.mid[1] - m.centre[1]) * f.n[1] + (f.mid[2] - m.centre[2]) * f.n[2] > 0));
  }

  // Picking and the overlay through a view with stubbed surfaces.
  const brainView = (count, labelOf, kindOf) => {
    const v = bareView("brain");
    v.host = { wrap: { getBoundingClientRect: () => ({ left: 0, top: 0, width: 900, height: 600 }) } };
    v.size = { width: 900, height: 600, ratio: 1 };
    v.showNodes({ relations: [], edges: [], nodes: Array.from({ length: count }, (_, i) => ({ id: "b" + i, label: labelOf(i), kind: kindOf(i), links: i % 5 })) });
    delete v.fit; GraphView.prototype.fit.call(v, false);
    v.frame = v.camera.frame(900, 600);
    return v;
  };
  {
    const v = brainView(40, (i) => "n" + i, (i) => kinds[i % kinds.length]);
    const projected = v.nodes.map((n) => [n, project(v.frame, ...n.pos, n.size)]).filter(([, p]) => p);
    let checked = 0;
    for (const [n, p] of projected) {
      // A node with no other node within reach on screen is exactly what is picked.
      const alone = projected.every(([m, q]) => m === n || Math.hypot(q.x - p.x, q.y - p.y) > 2 * Math.max(7, q.r + 3));
      if (!alone) continue;
      assert.strictEqual(v.pick(p.x, p.y).id, n.id, "pick hits " + n.id); checked++;
    }
    assert.ok(checked >= 5, "several isolated nodes were pick-tested: " + checked);
    const all = v.nodes.map((n) => project(v.frame, ...n.pos, 0));
    assert.ok(all.every((p) => p && p.x >= 0 && p.x <= 900 && p.y >= 0 && p.y <= 600), "fit frames every node of the brain");
  }
  const recorder = () => {
    const log = { texts: [], ops: 0 }, ctx = { measureText: (t) => ({ width: 7 * t.length }) };
    for (const m of ["setTransform", "clearRect", "beginPath", "closePath", "fill", "stroke", "fillRect"]) ctx[m] = () => {};
    ctx.moveTo = ctx.lineTo = () => { log.ops++; };
    ctx.fillText = (t) => log.texts.push(t);
    return { log, canvas: { getContext: () => ctx } };
  };
  const style = { bg: [1, 1, 1], ink: [0, 0, 0] };
  const evil = "<img src=x onerror=alert(1)>";
  const opsFor = (count, flat) => {
    const v = brainView(count, () => evil, (i) => (i % 9 === 8 ? evil : kinds[i % 8])), rec = recorder();
    v.host.brainCanvas = rec.canvas; v.camera.flat = !!flat; v.frame = v.camera.frame(900, 600);
    v.drawBrain(v.frame, style);
    return { v, log: rec.log };
  };
  for (const flat of [false, true]) {
    const small = opsFor(12, flat), big = opsFor(1000, flat);
    for (const { log, v } of [small, big]) {
      assert.ok(log.texts.length >= 1 && log.texts.length <= 9, "at most nine region names, got " + log.texts.length);
      for (const t of log.texts) assert.ok(/^(решения|понятия|память|сессии|сущности|код|документы|ошибки|тип неизвестен) · \d+$/.test(t), "fixed words and a count only: " + t);
      assert.ok(!log.texts.some((t) => t.includes("<") || t.includes("onerror")), "stored text never reaches the overlay");
      assert.strictEqual(v.regionLabels.length, log.texts.length);
    }
    assert.ok(big.log.ops <= small.log.ops * 1.05 + 50, "overlay cost does not grow with nodes (" + flat + "): " + small.log.ops + " vs " + big.log.ops);
    assert.ok(big.log.ops < 6000, "overlay path ops bounded: " + big.log.ops);
  }
  // Portrait panel: after fit, and after a resize from desktop to phone, every
  // node and every brain-outline point is inside the panel, in 3D and in 2D.
  {
    const B2 = window.BrainGraph;
    let rect = { left: 0, top: 0, width: 1280, height: 800 };
    const v = bareView("brain");
    v.host = { wrap: { getBoundingClientRect: () => rect }, glCanvas: {}, flatCanvas: {} };
    v.showNodes({ relations: [], edges: [], nodes: Array.from({ length: 400 }, (_, i) => ({ id: "p" + i, label: "p", kind: kinds[i % kinds.length], links: i % 7 })) });
    delete v.fit;
    const outside = () => {
      const f = v.camera.frame(rect.width, rect.height), k = v.brainScale;
      const pts = v.nodes.map((n) => v.drawnPos(n)).concat(B2.BRAIN_MESHES.flatMap((m) => m.points.map((q) => v.drawnPos({ pos: q.map((x) => x * k) }))));
      return pts.filter((p) => { const s = project(f, p[0], p[1], p[2], 0); return !s || s.x < 0 || s.y < 0 || s.x > rect.width || s.y > rect.height; }).length;
    };
    v.resize(); assert.strictEqual(outside(), 0, "desktop fit keeps everything inside");
    rect = { left: 0, top: 0, width: 390, height: 520 }; v.resize();
    assert.strictEqual(outside(), 0, "resize to 390x520 refits: nodes and brain outline inside");
    v.camera.dist = 5; v.fit(false); assert.strictEqual(outside(), 0, "fit from a zoomed-in camera, portrait");
    v.camera.flat = true; v.fit(false); assert.strictEqual(outside(), 0, "flat (2D) fit, portrait");
    v.camera.flat = false; v.zoom(1e-6); assert.strictEqual(v.camera.dist, 20, "zoom limits unchanged");
    // The old vertical-only fit would clip this panel: the regression is real.
    const half = FOV_CHECK => Math.atan(Math.tan(FOV_CHECK / 2) * (390 / 520));
    assert.ok(half(50 * Math.PI / 180) < 25 * Math.PI / 180, "portrait horizontal half-angle is the narrower one");
  }
  // Leaving the stage clears the hover text.
  {
    const listeners = {}, texts = [];
    const v = bareView("brain");
    v.host = { wrap: { addEventListener: (t, fn) => { listeners[t] = fn; }, style: {} }, onHoverText: (n) => texts.push(n) };
    v.bindInput(); v.hover = "n1"; listeners.pointerleave({});
    assert.strictEqual(v.hover, null); assert.deepStrictEqual(texts, [null], "hover text cleared on pointerleave");
  }
  // Other layouts and the cluster overview draw no brain.
  {
    const rec = recorder(), v = bareView("group"); v.host = { brainCanvas: rec.canvas }; v.size = { width: 900, height: 600, ratio: 1 };
    v.showNodes(doc); v.drawBrain(v.camera.frame(900, 600), style);
    assert.strictEqual(rec.log.texts.length + rec.log.ops, 0, "no brain outside the brain layout");
  }
  // The page: nothing injected as HTML, no remote code, brain is a real option.
  const html = fs.readFileSync(path.join(__dirname, "../brain-cli/assets/explorer.html"), "utf8");
  const js = fs.readFileSync(path.join(__dirname, "../brain-cli/assets/graph3d.js"), "utf8");
  for (const text of [html, js]) for (const marker of ["innerHTML", "outerHTML", "insertAdjacentHTML", "document.write", "eval(", "new Function", "<script src=\"http"]) {
    assert.ok(!text.includes(marker), "no " + marker);
  }
  assert.ok(/<option value="brain"/.test(html) && /id="brainCanvas"/.test(html) && /brainCanvas \{ pointer-events: none/.test(html));
  assert.ok(/Это не весь мозг/.test(html), "the page says the counts are of the drawn sample");
  assert.ok(!/requestAnimationFrame\(\s*\(\)\s*=>\s*this\.render|setInterval/.test(js), "no idle animation loop");
}
console.log("graph3d tests: ok");
