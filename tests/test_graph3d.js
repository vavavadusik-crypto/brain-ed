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
for (const layout of ["group", "links", "time"]) {
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
for (const layout of ["group", "links"]) {
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
console.log("graph3d tests: ok");
