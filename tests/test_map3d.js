// node tests/test_map3d.js — the real rules of the work map's 3D view, without a
// browser: the drawing surface is a stub, but projection, depth ordering, picking,
// caps, clearing and the phase layout are the module's own code.
// A page-level check reads map.html itself: no HTML injection, no external URLs,
// a real 2D/3D switch, and the inline script parses.
"use strict";
const assert = require("assert");
const fs = require("fs");
const path = require("path");

/* ---------- minimal browser stand-ins ---------- */
const rafQueue = [];
globalThis.requestAnimationFrame = (fn) => { rafQueue.push(fn); return rafQueue.length; };
globalThis.performance = globalThis.performance || { now: () => 0 };
globalThis.getComputedStyle = () => ({ getPropertyValue: () => "" });
globalThis.document = {
  documentElement: {},
  hidden: false,
  addEventListener() {},
};
globalThis.window = { matchMedia: () => ({ matches: false }), devicePixelRatio: 1, document: globalThis.document };
function pumpFrames() { const q = rafQueue.splice(0); for (const fn of q) fn(performance.now()); }

new Function(fs.readFileSync(path.join(__dirname, "../brain-cli/assets/map3d.js"), "utf8"))();
const M = window.BrainMap3D;
assert.ok(M && M.Map3DView && M.project && M.CAPS, "the module exposes its pieces");

/* ---------- one projection: target mid-screen, behind the eye dropped ---------- */
{
  const cam = new M.Camera();
  cam.target = [10, 20, 30]; cam.dist = 500; cam.ref = 500;
  const frame = cam.frame(800, 600);
  const p = M.project(frame, 10, 20, 30, 4);
  assert.ok(Math.abs(p.x - 400) < 1e-6 && Math.abs(p.y - 300) < 1e-6, "the target lands mid-screen");
  assert.ok(Math.abs(p.r - 4) < 1e-6, "a point at the reference distance keeps its size");
  const eye = cam.eye(), behind = [2 * eye[0] - 10, 2 * eye[1] - 20, 2 * eye[2] - 30];
  assert.strictEqual(M.project(frame, ...behind, 4), null, "a point behind the eye is not drawn");
  // Genuine depth: a farther point of the same world size draws smaller.
  const near = M.project(frame, 10, 20, 30, 10); // the target itself
  const farPt = [2 * 10 - eye[0], 2 * 20 - eye[1], 2 * 30 - eye[2]]; // beyond the target
  const far = M.project(frame, ...farPt, 10);
  assert.ok(far && far.r < near.r, "distance shrinks the drawn size");
  assert.ok(far.w > near.w, "distance raises the clip w used for ordering");
}

/* ---------- depth ordering: far first, hidden points dropped ---------- */
{
  const cam = new M.Camera(); cam.target = [0, 0, 0]; cam.dist = 500; cam.ref = 500;
  const frame = cam.frame(800, 600), eye = cam.eye(), target = cam.target;
  // f is the fraction along the eye->target ray: f=1 is the target, f>1 goes
  // past it (farther), 0<f<1 comes toward the eye, f<0 is behind the eye.
  const along = (f, id) => ({ id, pos: [eye[0] + (target[0] - eye[0]) * f, eye[1] + (target[1] - eye[1]) * f, eye[2] + (target[2] - eye[2]) * f], size: 6 });
  const items = [along(0.4, "near"), along(1.8, "far"), along(-0.5, "behind")];
  const order = M.depthOrder(items, frame);
  assert.deepStrictEqual(order.map((s) => s.item.id), ["far", "near"], "far drawn first, hidden dropped");
  assert.ok(order[0].w > order[1].w, "the first drawn is indeed the farther one");
  assert.ok(order.every((s) => s.w > 0), "nothing behind the eye survives ordering");
}

/* ---------- picking: nearest to the cursor, then nearest to the eye ---------- */
{
  const cam = new M.Camera(); cam.target = [0, 0, 0]; cam.dist = 500; cam.ref = 500;
  const frame = cam.frame(800, 600);
  const center = { id: "on", pos: [0, 0, 0], size: 8 };
  assert.strictEqual(M.pick([center], frame, 400, 300).id, "on", "a hit on the point selects it");
  assert.strictEqual(M.pick([center], frame, 700, 500), null, "a click far away picks nothing");
  // Two points share the screen centre, one nearer the eye; the nearer one wins.
  const eye = cam.eye(), along = (f, id) => ({ id, pos: [eye[0] * (1 - f), eye[1] * (1 - f), eye[2] * (1 - f)], size: 12 });
  assert.strictEqual(M.pick([along(0.9, "far"), along(0.3, "near")], frame, 400, 300).id, "near",
    "of two stacked points the nearer to the eye is picked");
}

/* ---------- deterministic, bounded, phase-branched layout ---------- */
const projects = [
  { name: "beta", past: 4, current: 1, planned: 0, links: 2, conflicts: 1 },
  { name: "alpha", past: 1, current: 2, planned: 3, links: 0, conflicts: 0 },
  { name: "гамма", past: 0, current: 0, planned: 5, links: 1, conflicts: 0 },
];
{
  const one = M.projectCenters(projects, M.CAPS.nodes), two = M.projectCenters(projects, M.CAPS.nodes);
  assert.deepStrictEqual(one.map((p) => p.pos), two.map((p) => p.pos), "the same data gives the same picture");
  assert.ok(one.every((p) => p.pos.every(Number.isFinite)), "every centre is a finite XYZ point");
  // Ranking is by name, so the arrival order does not change the picture.
  const shuffled = M.projectCenters([projects[2], projects[0], projects[1]], M.CAPS.nodes);
  assert.deepStrictEqual(shuffled.map((p) => p.project), one.map((p) => p.project), "centres are ranked by name, order-independent");
  assert.strictEqual(one.find((p) => p.project === "beta").phase, "conflict", "a conflicted project is marked");

  // Bounded: more projects than the cap still yields at most CAPS.nodes centres.
  const many = Array.from({ length: M.CAPS.nodes + 250 }, (_, i) => ({ name: "p" + i, past: 0, current: 0, planned: 0 }));
  assert.strictEqual(M.projectCenters(many, M.CAPS.nodes).length, M.CAPS.nodes, "the project scene is capped");

  // Task branches sit on real phase axes off the centre; deterministic and bounded.
  const center = one.find((p) => p.project === "alpha");
  const tasks = [
    { id: 3, phase: "current", state: "active", title: "now" },
    { id: 1, phase: "past", state: "completed", title: "done" },
    { id: 2, phase: "planned", state: "queued", title: "later" },
    { id: 4, phase: "planned", state: "queued", title: "later2" },
  ];
  const a = M.taskBranches(center, tasks, M.CAPS.nodes), b = M.taskBranches(center, tasks, M.CAPS.nodes);
  assert.deepStrictEqual(a.map((t) => t.pos), b.map((t) => t.pos), "branches are deterministic");
  const span = 30; // BRANCH(92) +/- SPREAD(24)
  const past = a.find((t) => t.phase === "past"), cur = a.find((t) => t.phase === "current"), plan = a.find((t) => t.phase === "planned");
  assert.ok(Math.abs(past.pos[2] - (center.pos[2] - 92)) <= span && Math.abs(past.pos[1] - center.pos[1]) <= span, "past grows behind, on -Z");
  assert.ok(Math.abs(cur.pos[1] - (center.pos[1] + 92)) <= span, "current grows on +Y");
  assert.ok(Math.abs(plan.pos[2] - (center.pos[2] + 92)) <= span, "planned grows ahead, on +Z");
  assert.ok(a.every((t) => t.id.startsWith("task:")), "task ids are namespaced");
  assert.strictEqual(M.taskBranches(center, Array.from({ length: 500 }, (_, i) => ({ id: i, phase: "planned" })), 40).length, 40, "branches honour the cap");
}

/* ---------- the view: build, cap, pick, clear, reduced motion, render-on-demand */
function stubCanvas() {
  const ctx = {};
  for (const m of ["setTransform", "clearRect", "beginPath", "arc", "fill", "stroke", "moveTo", "lineTo", "fillText", "fillRect"]) ctx[m] = () => {};
  return { width: 0, height: 0, getContext: () => ctx, getBoundingClientRect: () => ({ left: 0, top: 0, width: 800, height: 600 }) };
}
{
  const canvas = stubCanvas();
  const wrap = { getBoundingClientRect: () => ({ left: 0, top: 0, width: 800, height: 600 }), getClientRects: () => [1], style: {}, addEventListener() {} };
  let active = true, frames = 0;
  const view = new M.Map3DView({ wrap, canvas, active: () => active, onFrame: () => { frames++; } });
  view.reducedMotion = true;
  view.resize();

  view.setData(projects, "alpha", [{ id: 3, phase: "current", title: "now" }, { id: 1, phase: "past", title: "done" }]);
  pumpFrames();
  assert.strictEqual(view.visibleCount(), 3 + 2, "the scene holds the projects plus the selected project's tasks");
  assert.ok(view.has("project:alpha") && view.has("task:3"), "both namespaces are present");
  assert.ok(frames >= 1, "a frame was drawn on a data change");

  // Picking on the built scene finds a real node; far from the middle finds none.
  const p = view.items.find((i) => i.project === "alpha");
  const s = M.project(view.frame, ...p.pos, p.size);
  assert.strictEqual(view.pick(s.x, s.y).id, "project:alpha", "picking matches the drawn point");
  assert.strictEqual(view.pick(-50, -50), null, "picking nothing returns null");

  // The scene is capped even when the input is huge.
  const huge = Array.from({ length: 900 }, (_, i) => ({ name: "q" + i, past: 0, current: 0, planned: 0 }));
  view.setData(huge, "q0", Array.from({ length: 900 }, (_, i) => ({ id: i, phase: "planned" })));
  pumpFrames();
  assert.ok(view.visibleCount() <= M.CAPS.nodes, "the scene never exceeds its cap");

  // Clearing empties everything, and a space switch relies on exactly this.
  view.clear();
  assert.strictEqual(view.visibleCount(), 0, "clear empties the scene");
  assert.strictEqual(view.has("project:q0"), false, "cleared names are gone");

  // Reduced motion: the camera jumps instead of easing, and no frame loop runs.
  view.moveCamera([5, 6, 7], 300, true);
  assert.deepStrictEqual(view.camera.target, [5, 6, 7], "reduced motion jumps the camera");
  assert.strictEqual(view.animation, null, "reduced motion leaves no animation pending");
  pumpFrames();
  assert.strictEqual(view.pending, false, "the single scheduled frame ran");
  assert.strictEqual(rafQueue.length, 0, "no endless frame loop is left running");

  // Hidden panel: a request draws nothing while the 2D view is active.
  const before = frames;
  active = false;
  view.setData(projects, null, []);
  pumpFrames();
  assert.strictEqual(frames, before, "the 3D renderer stays silent outside 3D mode");
}

/* Newly opened branches fit narrow panels; routine refresh keeps a manual camera. */
{
  let width = 260;
  const canvas = stubCanvas();
  const wrap = { getBoundingClientRect: () => ({ left: 0, top: 0, width, height: 700 }),
    getClientRects: () => [1], style: {}, addEventListener() {} };
  const view = new M.Map3DView({ wrap, canvas, active: () => true });
  view.resize();
  const tasks = [{ id: 1, phase: "past" }, { id: 2, phase: "current" }, { id: 3, phase: "planned" }];
  view.setData(projects, "alpha", tasks);
  // Opening a project frames that project and its branches; "0" frames everything.
  const assertInFrame = (whole) => {
    const frame = view.camera.frame(width, 700);
    for (const item of view.focusItems(whole)) {
      const p = M.project(frame, ...item.pos, item.size);
      assert.ok(p && p.x >= p.r && p.x <= width - p.r && p.y >= p.r && p.y <= 700 - p.r,
        "opening or resizing the scene must keep every node inside the panel: " + item.id);
    }
  };
  assertInFrame();
  view.camera.target = [1, 2, 3]; view.camera.dist = 123;
  view.setData(projects, "alpha", tasks);
  assert.deepStrictEqual(view.camera.target, [1, 2, 3], "an unchanged refresh preserves a manual view");
  assert.strictEqual(view.camera.dist, 123, "an unchanged refresh preserves zoom");
  view.setData(projects, "beta", [{ id: 4, phase: "planned" }]);
  assertInFrame();
  width = 180; view.resize(); assertInFrame();
  view.fit(false, true); assertInFrame(true);
  const overview = new Set(view.focusItems(true));
  assert.ok(view.items.every((i) => overview.has(i)), "the overview covers every drawn node");
  assert.ok(view.regions.every((r) => overview.has(r)), "and every brain region");
  pumpFrames();
}

/* ---------- readable selected-project branches ---------- */
function recordingCanvas(width, height) {
  const texts = [];
  const ctx = { measureText: (t) => ({ width: 6.5 * String(t).length }) };
  for (const m of ["setTransform", "clearRect", "beginPath", "arc", "fill", "stroke", "moveTo", "lineTo", "fillRect"]) ctx[m] = () => {};
  texts.places = [];
  ctx.fillText = (t, x, y) => { texts.push(String(t)); texts.places.push({ text: String(t), x, y }); };
  const box = () => ({ left: 0, top: 0, width: width(), height: height() });
  return { texts, canvas: { width: 0, height: 0, getContext: () => ctx, getBoundingClientRect: box } };
}
function viewOf(width, height) {
  const rec = recordingCanvas(() => width.value, () => height);
  const handlers = {};
  const wrap = { getBoundingClientRect: () => ({ left: 0, top: 0, width: width.value, height }), getClientRects: () => [1],
    style: {}, addEventListener(type, fn) { handlers[type] = fn; } };
  const view = new M.Map3DView({ wrap, canvas: rec.canvas, active: () => true });
  view.reducedMotion = true; view.resize();
  return { view, texts: rec.texts, handlers };
}
const manyProjects = Array.from({ length: 350 }, (_, i) => ({ name: "p" + String(i).padStart(3, "0"), past: 1, current: 1, planned: 1 }));
const lastName = manyProjects[manyProjects.length - 1].name;
const manyTasks = Array.from({ length: 140 }, (_, i) => ({ id: i + 1, phase: ["past", "current", "planned"][i % 3], title: "задача номер " + (i + 1) }));
{
  // The selected project sits past the first 300 and still gets its branches.
  const { view, texts } = viewOf({ value: 1280 }, 720);
  view.setData(manyProjects, lastName, manyTasks);
  pumpFrames();
  assert.ok(view.has("project:" + lastName), "a selected project beyond the cap is still in the scene");
  assert.ok(view.items.some((i) => i.task !== undefined), "the selected project keeps its task branches");
  for (const phase of ["past", "current", "planned"]) assert.ok(view.items.some((i) => i.phase === phase && i.task !== undefined), "branch " + phase + " drawn");
  assert.ok(view.visibleCount() <= M.CAPS.nodes, "the global cap still holds");
  const sum = view.summary();
  assert.strictEqual(sum.projectsLoaded, 350, "the summary counts loaded projects");
  assert.ok(sum.projectsShown < 350 && sum.projectsShown + sum.tasksShown === view.visibleCount(), "the summary names the shown subset");
  assert.strictEqual(sum.tasksLoaded, 140, "the summary counts loaded tasks");

  // Labels are bounded, and the branches carry short task labels plus phase names.
  const taskLabels = texts.filter((t) => t.startsWith("#"));
  const phaseLabels = texts.filter((t) => /^(прошлое|сейчас|в плане)/.test(t));
  assert.ok(taskLabels.length >= 2, "a dense branch still labels its nearest tasks");
  assert.ok(taskLabels.every((t) => t.length <= 24), "task labels stay short");
  assert.strictEqual(phaseLabels.length, 3, "each phase branch is named once");
  assert.ok(texts.length <= M.CAPS.labels, "every text, branch names included, stays within the hard cap: " + texts.length);

  // Unchanged refresh with the large list keeps a manual orbit.
  view.camera.yaw = 2.2; view.camera.dist = 333; view.camera.target = [4, 5, 6];
  view.setData(manyProjects, lastName, manyTasks);
  assert.deepStrictEqual([view.camera.yaw, view.camera.dist, view.camera.target], [2.2, 333, [4, 5, 6]], "unchanged refresh keeps the manual camera");

  // A space change clears every node; the next space starts from its own data.
  view.clear();
  assert.strictEqual(view.visibleCount(), 0, "clear empties the scene");
  view.setData([{ name: "other", past: 0, current: 1, planned: 0 }], null, []);
  assert.ok(!view.has("project:" + lastName) && view.has("project:other"), "nothing from the old space lingers");
  assert.deepStrictEqual(view.summary().tasksLoaded, 0, "no task counts survive a clear");
}
{
  // One hard cap for every text in a frame — branch names, hover and the opened
  // task included — while those keep their claim ahead of ordinary labels.
  for (const [w, cap] of [[1280, M.CAPS.labels], [260, M.CAPS.narrowLabels]]) {
    const { view, texts } = viewOf({ value: w }, 720);
    view.setData(manyProjects, lastName, manyTasks);
    pumpFrames();
    const hovered = view.items.filter((i) => i.task !== undefined).find((i) => i.phase === "current");
    const opened = view.items.filter((i) => i.task !== undefined).find((i) => i.phase === "past");
    view.hover = hovered.id; view.selectedTask = opened.task;
    texts.length = 0; view.request(); pumpFrames();
    assert.ok(texts.length <= cap, w + "px draws " + texts.length + " texts, cap " + cap);
    assert.ok(texts.length >= Math.min(cap, 6), w + "px still labels what matters");
    for (const want of [lastName, M.taskLabel(hovered), M.taskLabel(opened)]) {
      assert.ok(texts.includes(want), w + "px keeps the priority label " + want);
    }
    assert.strictEqual(texts.filter((t) => /^(прошлое|сейчас|в плане) · /.test(t)).length, 3, w + "px names each branch once");
  }
}
{
  // At the default view two planned tasks do not coincide on screen, are legible, and each click picks itself.
  for (const w of [1280, 260]) {
    const { view } = viewOf({ value: w }, 720);
    view.setData(projects, "alpha", [{ id: 1, phase: "past", title: "a" }, { id: 2, phase: "planned", title: "b" },
      { id: 3, phase: "current", title: "c" }, { id: 4, phase: "planned", title: "d" }]);
    pumpFrames();
    const two = ["task:2", "task:4"].map((id) => M.screenOf(view.frame, view.items.find((i) => i.id === id)));
    const gap = Math.hypot(two[0].x - two[1].x, two[0].y - two[1].y);
    assert.ok(gap >= two[0].r + two[1].r + 6, "planned tasks are distinct at the default view at " + w + "px: gap " + gap.toFixed(1));
    assert.ok(two.every((s) => s.r >= 4), "task dots are legible at " + w + "px");
    for (const s of two) {
      assert.ok(s.x >= 0 && s.x <= w && s.y >= 0 && s.y <= 720, "the focused branch is inside the panel at " + w + "px");
      assert.strictEqual(view.pick(s.x, s.y).id, s.item.id, "clicking a task picks that task");
    }
  }
}
{
  // A click selects what was under the pointer when it went down, not where it was released.
  const { view, handlers } = viewOf({ value: 1280 }, 720);
  const picked = [];
  view.host.onSelect = (hit) => picked.push(hit);
  view.setData(projects, "alpha", [{ id: 2, phase: "planned" }, { id: 4, phase: "planned" }]);
  pumpFrames();
  const s = M.screenOf(view.frame, view.items.find((i) => i.id === "task:2"));
  handlers.pointerdown({ button: 0, clientX: s.x, clientY: s.y, pointerId: 1, target: {} });
  handlers.pointerup({ clientX: s.x + 2, clientY: s.y + 1, pointerId: 1, target: {} });
  assert.deepStrictEqual(picked, [{ kind: "task", id: 2 }], "the pressed task is the selected one");
}

/* Labels are placed off other nodes' circles where a free place exists. */
function labelCrossings(view, texts, strongTexts) {
  const circles = M.depthOrder(view.items, view.frame);
  const bad = [];
  for (const { text, x, y } of texts.places) {
    if (strongTexts.includes(text) || /^(прошлое|сейчас|в плане) · /.test(text)) continue;
    const w = 6.5 * text.length, box = [x - w / 2, y - 11, w, 15];
    for (const c of circles) {
      const own = c.item.task !== undefined ? M.taskLabel(c.item) === text : String(c.item.project) === text;
      if (own) continue;
      const nx = Math.max(box[0], Math.min(c.x, box[0] + box[2])), ny = Math.max(box[1], Math.min(c.y, box[1] + box[3]));
      if (Math.hypot(c.x - nx, c.y - ny) < c.r + 1) { bad.push(text + " over " + c.item.id); break; }
    }
  }
  return bad;
}
{
  const fewTasks = [{ id: 1, phase: "past", title: "первая задача" }, { id: 2, phase: "planned", title: "план" },
    { id: 3, phase: "current", title: "сейчас идёт" }, { id: 4, phase: "planned", title: "план два" }];
  for (const [w, list, tasks, sel] of [[1280, projects, fewTasks, "alpha"], [1280, manyProjects, manyTasks, lastName],
    [260, projects, fewTasks, "alpha"], [260, manyProjects, manyTasks, lastName]]) {
    const { view, texts } = viewOf({ value: w }, 720);
    view.setData(list, sel, tasks);
    texts.length = 0; texts.places.length = 0; view.request(); pumpFrames();
    const bad = labelCrossings(view, texts, [sel]);
    assert.deepStrictEqual(bad, [], w + "px, " + list.length + " projects: ordinary labels avoid other nodes");
    if (tasks === fewTasks && w === 1280) {
      for (const t of view.items.filter((i) => i.task !== undefined)) assert.ok(texts.includes(M.taskLabel(t)), "a small branch labels every task: " + t.id);
    }
  }
}

/* The overlay stacks in normal flow: a wrapped hover pushes the legend down. */
{
  const html = fs.readFileSync(path.join(__dirname, "../brain-cli/assets/map.html"), "utf8");
  const overlay = html.match(/<div class="overlay">([\s\S]*?)<\/div>\s*<\/div>/);
  assert.ok(overlay, "hover and legend share one overlay container");
  const inner = overlay[1];
  assert.ok(inner.indexOf('id="hover"') >= 0 && inner.indexOf('id="hover"') < inner.indexOf('id="legend3d"'), "the hover line comes first, the legend after it");
  const rule = (sel) => (html.match(new RegExp("\\n" + sel.replace(/[.:]/g, "\\$&") + " \\{([^}]*)\\}")) || [])[1] || "";
  assert.ok(/display:\s*flex/.test(rule(".overlay")) && /flex-direction:\s*column/.test(rule(".overlay")), "the overlay is a column in flow");
  assert.ok(/pointer-events:\s*none/.test(rule(".overlay")), "the overlay lets the canvas get pointer events");
  assert.ok(!/position|top:/.test(rule(".hover")) && !/position|top:/.test(rule(".legend")), "no fixed offsets that assume one-line height");
  assert.ok(/display:\s*none/.test(rule(".hover:empty")), "an empty hover takes no room");
}

/* ---------- the page itself: self-contained, switchable, parseable ---------- */
{
  const html = fs.readFileSync(path.join(__dirname, "../brain-cli/assets/map.html"), "utf8");
  assert.ok(html.includes("/map/map3d.js"), "the page loads its 3D script");
  assert.ok(html.includes('id="mode-2d"') && html.includes('id="mode-3d"'), "a real 2D/3D switch exists");
  assert.ok(html.includes('id="legend3d"'), "the 3D view carries a phase legend");
  for (const marker of ["innerHTML", "insertAdjacentHTML", "eval(", "http://", "https://"]) {
    assert.ok(!html.includes(marker), "the page avoids " + marker);
  }
  assert.ok(!html.toLowerCase().includes("подтвердить"), "no confirmation control is fabricated");
  const bodies = [...html.matchAll(/<script(?![^>]*\bsrc=)[^>]*>([\s\S]*?)<\/script>/g)].map((m) => m[1]);
  assert.ok(bodies.length >= 1, "the page carries an inline script");
  for (const body of bodies) new Function(body); // throws on a syntax error
}

/* ---------- the brain overview: shape, regions, truthful status, overlay ---------- */
{
  assert.strictEqual(M.REGIONS.length, 8, "eight regions");
  assert.strictEqual(new Set(M.REGIONS.map((r) => r.id)).size, 8, "region ids are unique");
  // Shape: lobed hemispheres, not two plain ellipsoids.
  const H = M.BRAIN.hemisphere, P = (z, y) => M.profileRadius(Math.atan2(y * H.ry, z * H.rz));
  const frontal = [H.rz * 0.96, -0.12 * H.ry], temporal = [H.rz * 0.5, -0.63 * H.ry], notch = Math.atan2(-0.27 * H.ry, 0.56 * H.rz);
  const chord = (() => { // distance from the origin to the frontal-temporal chord along the notch angle
    const [x1, y1] = frontal, [x2, y2] = temporal, dx = Math.cos(notch), dy = Math.sin(notch);
    const t = (x1 * (y2 - y1) - y1 * (x2 - x1)) / (dx * (y2 - y1) - dy * (x2 - x1));
    return t;
  })();
  assert.ok(M.profileRadius(notch) < chord - 5, "the side profile has a lateral-fissure notch (non-convex)");
  const tall = Math.max(...Array.from({ length: 360 }, (_, i) => M.profileRadius(i * Math.PI / 180) * Math.sin(i * Math.PI / 180)));
  assert.ok(tall / P(1, 0.05) < 0.7, "longer than tall, like a brain from the side");
  const shells = M.brainShells(1), mesh = M.BRAIN.mesh;
  assert.deepStrictEqual(shells.map((m) => m.name), ["left", "right", "cerebellum"], "two hemispheres and a cerebellum");
  for (const m of shells) {
    assert.strictEqual(m.points.length, (mesh.across + 1) * mesh.around, m.name + " mesh is bounded");
    assert.strictEqual(m.faces.length, mesh.across * mesh.around, m.name + " faces are bounded");
    assert.ok(m.faces.every((f) => (f.mid[0] - m.centre[0]) * f.n[0] + (f.mid[1] - m.centre[1]) * f.n[1] + (f.mid[2] - m.centre[2]) * f.n[2] > 0),
      m.name + " face normals point outward");
  }
  const gap = Math.min(...shells[0].points.map((q) => -q[0])) + Math.min(...shells[1].points.map((q) => q[0]));
  assert.ok(gap > 2 && gap < 20, "a narrow longitudinal fissure separates the hemispheres: " + gap);
  const lines = M.brainOutline(1);
  assert.ok(lines.length <= 24 && lines.every((l) => l.points.length <= 25 && l.points.length === l.normals.length), "bounded surface curves");
  assert.ok(lines.every((l) => l.points.every((q) => q.every(Number.isFinite))), "every curve point is finite");
  assert.ok(lines.some((l) => l.major) && lines.some((l) => !l.major), "major fissures and minor sulci");
  const hemiCurves = lines.filter((l) => l.points.every((q) => Math.abs(q[0]) > 30));
  assert.ok(hemiCurves.every((l) => l.points.every((q) => Math.abs(M.hemisphereNorm(q, 1) - 1) < 0.02)), "sulci lie on the hemisphere surface");
  // Projects sit inside the hemisphere their direction picks, region markers just outside.
  const many = Array.from({ length: 60 }, (_, i) => ({ name: "p" + String(i).padStart(2, "0"), past: 1 }));
  const k = M.brainScale(60);
  for (const c of M.projectCenters(many, 300, null)) {
    assert.ok(M.hemisphereNorm(c.pos, k) <= 0.73, "project " + c.name + " is inside a hemisphere");
  }
  const anchor = Object.fromEntries(M.REGIONS.map((r) => [r.id, M.regionAnchor(r, 1)]));
  for (const r of M.REGIONS.filter((r) => !r.bridge && !r.cerebellum)) {
    assert.ok(M.hemisphereNorm(anchor[r.id], 1) >= 1, r.id + " marker sits on the surface, outside the projects");
  }
  assert.ok(["profile", "decisions", "plans"].every((id) => anchor[id][2] > 0), "profile, decisions and plans are frontal");
  assert.ok(anchor.episodes[1] < 0, "conversations sit on a temporal lobe");
  assert.ok(anchor.errors[2] < -60 && anchor.errors[1] < -30, "errors sit on the cerebellum");
  assert.strictEqual(anchor.coordination[0], 0, "coordination bridges the hemispheres");
  // The default view frames the whole brain inside the panel, wide or narrow.
  for (const [w, h] of [[620, 750], [390, 420]]) {
    const wrap = { getBoundingClientRect: () => ({ left: 0, top: 0, width: w, height: h }), getClientRects: () => [1], style: {}, addEventListener() {} };
    const v = new M.Map3DView({ wrap, canvas: stubCanvas(), active: () => true });
    v.reducedMotion = true; v.resize(); v.setData(many.slice(0, 12), null, []); pumpFrames();
    const pts = v.shells.flatMap((m) => m.points).map((q) => M.project(v.frame, q[0], q[1], q[2], 0));
    assert.ok(pts.every((p) => p && p.x >= 0 && p.x <= w && p.y >= 0 && p.y <= h), "the brain fits a " + w + "x" + h + " panel");
    const xs = pts.map((p) => p.x), ys = pts.map((p) => p.y);
    assert.ok(Math.max(...xs) - Math.min(...xs) > 0.6 * (w - 80) || Math.max(...ys) - Math.min(...ys) > 0.6 * (h - 80),
      "the brain fills most of a " + w + "x" + h + " panel");
  }
  // Status: nothing loaded -> no invented numbers anywhere.
  const none = M.regionStatus([], {});
  for (const id of ["profile", "episodes", "decisions", "errors"]) {
    assert.strictEqual(none[id].state, "elsewhere");
    assert.strictEqual(none[id].count, null, id + " never claims a count");
    assert.ok(["/graph", "/"].includes(none[id].route), id + " routes to an existing page");
  }
  for (const id of ["projects", "active", "plans", "coordination"]) assert.strictEqual(none[id].state, "empty", id);
  const now = Date.UTC(2026, 9, 9, 12, 0);
  const rows = [{ name: "a", past: 1, current: 2, planned: 1, links: 3, conflicts: 1 }, { name: "b", past: 4, current: 0, planned: 0 }];
  const st = M.regionStatus(rows, { now, fetchedAt: now - 60000, hasMore: true });
  assert.deepStrictEqual(["projects", "active", "plans", "coordination"].map((id) => [st[id].state, st[id].count]),
    [["data", 2], ["data", 2], ["data", 1], ["data", 4]], "counts come only from the loaded rows");
  assert.ok(/не наблюдение/.test(st.active.text), "active work is declared board state, not observed edits");
  assert.ok(/загруженной части/.test(st.active.text), "a partial list says so");
  assert.ok(!st.active.stale && /Получено в/.test(st.active.text), "fresh data names its time");
  const old = M.regionStatus(rows, { now, fetchedAt: now - M.BRAIN.staleMs - 1 });
  assert.ok(old.active.stale && /устарело/.test(old.active.text), "old data is marked stale");
  assert.ok(/неизвестно/.test(M.regionStatus(rows, {}).active.text), "unknown fetch time is said");
  const quiet = M.regionStatus([{ name: "a", past: 3 }], { now, fetchedAt: now });
  assert.deepStrictEqual([quiet.active.state, quiet.active.count], ["empty", 0], "no current tasks: empty, not invented activity");

  // Rendering: regions are drawn and pickable; labels avoid the page overlay.
  const texts = [];
  const ctx = { measureText: (t) => ({ width: 7 * t.length }) };
  for (const m of ["setTransform", "clearRect", "beginPath", "arc", "fill", "stroke", "moveTo", "lineTo", "fillRect", "setLineDash"]) ctx[m] = () => {};
  ctx.fillText = (text, x, y) => texts.push({ text, x, y });
  const canvas = { width: 0, height: 0, getContext: () => ctx };
  for (const [w, h] of [[1200, 800], [390, 420]]) {
    texts.length = 0;
    const wrap = { getBoundingClientRect: () => ({ left: 0, top: 0, width: w, height: h }), getClientRects: () => [1], style: {}, addEventListener() {} };
    const overlay = [12, 12, w - 24, 90]; // hover + legend block at the top
    const view = new M.Map3DView({ wrap, canvas, active: () => true, avoidRects: () => [overlay] });
    view.reducedMotion = true; view.resize();
    view.setData(rows, null, []);
    view.setRegionMeta({ fetchedAt: Date.now() });
    pumpFrames();
    assert.strictEqual(view.regions.length, 8, "eight region markers");
    assert.strictEqual(view.summary().projectsShown, 2, "regions are not counted as projects");
    assert.ok(texts.length > 0, "labels were drawn at " + w);
    for (const t of texts) {
      const top = t.y - 11, half = 3.5 * t.text.length;
      const hits = t.x + half > overlay[0] && t.x - half < overlay[0] + overlay[2] && top < overlay[1] + overlay[3] && top + 15 > overlay[1];
      assert.ok(!hits, "label '" + t.text + "' stays out of the overlay at " + w);
    }
    assert.ok(texts.some((t) => /Активная работа · 2/.test(t.text)), "a region label shows a loaded count at " + w);
    const region = view.regions.find((r) => r.region === "decisions");
    const s = M.screenOf(view.frame, region);
    if (s) assert.strictEqual(view.pick(s.x, s.y).region, "decisions", "a region marker is pickable");
  }
}
console.log("map3d tests: ok");
