// node tests/visual/graph3d_fixture.js <out-dir> [label]
// Static, synthetic visual fixture for the memory-graph brain. It is NOT the
// host server: explorer.html and graph3d.js are read from brain-cli/assets (or
// BRAIN_GRAPH_ASSETS) and every /api/* request is answered in the browser from
// the invented rows below. No network, no personal data, no model.
"use strict";
const fs = require("fs");
const path = require("path");

const ASSETS = process.env.BRAIN_GRAPH_ASSETS || path.join(__dirname, "../../brain-cli/assets");
const KINDS = ["code", "document", "memory", "session", "decision", "error", "concept", "entity"];
/* 120 invented nodes: every stored kind, three with a kind the page does not
   know, and one whose label is an HTML injection attempt (must stay text). */
const NODES = Array.from({ length: 120 }, (_, i) => ({
  id: "syn-" + i, kind: i >= 117 ? "legacy_kind" : KINDS[i % 8],
  label: i === 5 ? "<img src=x onerror=alert(1)>" : "synthetic " + KINDS[i % 8] + " " + i,
  group: ["alpha", "beta", "gamma"][i % 3], links: (i * 7) % 11, created_at: 1700000000 + i * 3600,
}));
const EDGES = Array.from({ length: 90 }, (_, i) => [i, (i * 13 + 7) % 120, 0]);
const count = (key) => NODES.reduce((m, n) => (m[n[key]] = (m[n[key]] || 0) + 1, m), {});

/* Field names and shapes follow brain-cli/src/explore.rs (overview, nodes,
   clusters, graph, links); the page reads every one of these. */
const TOTAL_NODES = 5000, TOTAL_EDGES = 9000;
function answer(u) {
  const p = u.pathname;
  if (p === "/api/explore/overview") return { space: "synthetic", total_nodes: TOTAL_NODES, scanned_nodes: TOTAL_NODES, truncated: false,
    truncation: null, total_edges: TOTAL_EDGES, kinds: count("kind"), agents: {},
    groups: Object.entries(count("group")).map(([group, c]) => ({ group, count: c })), group_count: 3, unlisted_group_nodes: 0,
    relations: [{ relation: "relates_to", count: 90 }], timeline: [{ month: "2023-11", count: 120 }],
    timeline_meaning: "месяц, когда узел был записан в Brain, а не когда произошло событие",
    spaces: [{ space: "synthetic", nodes: TOTAL_NODES }], scope: ["Синтетический набор для визуальной проверки."], work: {} };
  if (p === "/api/explore/nodes") return { total_nodes: TOTAL_NODES, matched: TOTAL_NODES, matches_capped: false, offset: 0, limit: 50,
    returned: 50, rows: NODES.slice(0, 50).map((n) => Object.assign({ degree: n.links }, n)) };
  if (p === "/api/explore/clusters") return { space: "synthetic", by: "group", derived: true, meaning: "первая папка пути",
    total_nodes: TOTAL_NODES, total_edges: TOTAL_EDGES, cluster_total: 3, cluster_link_total: 3, dangling_edges: 0,
    cluster_links: [[0, 1, 20], [1, 2, 12], [0, 2, 5]], links_meaning: "сводка",
    clusters: Object.entries(count("group")).map(([key, c]) => ({ key, count: c, kinds: count("kind"), internal_links: 10,
      first_created_at: 1700000000, last_created_at: 1700500000, top_node: null })) };
  if (p === "/api/explore/graph") return { space: "synthetic", selection: { kind: null, group: null, month: null, q: null, order: "links" },
    total_nodes: TOTAL_NODES, total_edges: TOTAL_EDGES, matched: TOTAL_NODES, matches_capped: false, limit: 400, returned: NODES.length,
    nodes: NODES, relations: ["relates_to"], edges: EDGES, edges_among: EDGES.length, edges_truncated: false };
  if (p === "/api/explore/links") return { edges: EDGES, relations: ["relates_to"], missing: [] };
  return { error_kind: "synthetic", message: "not provided by the fixture" };
}
/* The picture is only evidence if the page shows the fixture's real numbers. */
async function assertPage(page, where) {
  const text = await page.evaluate(() => document.body.innerText);
  const fail = (why) => { throw new Error(where + ": " + why); };
  for (const bad of ["не число", "NaN", "память недоступна", "не загружен", "undefined"]) if (text.includes(bad)) fail("page shows «" + bad + "»");
  const head = await page.locator("#headline").textContent();
  if (!/пространство «synthetic»: 5\s000 узлов, 9\s000 связей/.test(head)) fail("headline " + JSON.stringify(head));
  if (!/всего узлов в пространстве 5\s000/.test(await page.locator("#count").textContent())) fail("list total");
  const note = await page.locator("#canvasNote").textContent();
  if (!/На графе 120 узлов из 5\s000/.test(note) || !/в этой выборке 120 из 5\s000/.test(note) || !/тип неизвестен 3/.test(note)) fail("note " + JSON.stringify(note));
  const box = await page.locator("#stage").boundingBox(), vw = page.viewportSize().width;
  if (!box || box.width < 300 || box.height < 300 || box.x < 0 || box.x + box.width > vw + 0.5 || box.y > page.viewportSize().height - 300) fail("stage not visible " + JSON.stringify(box));
  const over = await page.evaluate(() => document.documentElement.scrollWidth - window.innerWidth);
  if (over > 0) fail("horizontal overflow " + over + "px");
  return { head, note };
}

/* Every drawn node and every brain-outline point projects inside the stage. */
async function assertInside(page, where) {
  const r = await page.evaluate(() => {
    const v = window.__brainGraph.view, B = window.BrainGraph, box = document.getElementById("stage").getBoundingClientRect();
    const f = v.camera.frame(box.width, box.height), k = v.brainScale || 1;
    const pts = v.nodes.map((n) => v.drawnPos(n)).concat(B.BRAIN_MESHES.flatMap((m) => m.points.map((q) => v.drawnPos({ pos: q.map((x) => x * k) }))));
    const bad = pts.filter((p) => { const s = B.project(f, p[0], p[1], p[2], 0); return !s || s.x < 0 || s.y < 0 || s.x > box.width || s.y > box.height; }).length;
    return { bad, total: pts.length, w: box.width, h: box.height };
  });
  if (r.bad) throw new Error(where + ": " + r.bad + " of " + r.total + " points outside the " + r.w + "x" + r.h + " stage");
  return r;
}

async function main() {
  const out = path.resolve(process.argv[2] || "graph3d-shots"), label = process.argv[3] || "shot";
  fs.mkdirSync(out, { recursive: true });
  const { chromium } = require("playwright");
  const browser = await chromium.launch();
  const page = await browser.newPage({ viewport: { width: 1280, height: 800 }, colorScheme: "light" });
  const errors = [], dialogs = [];
  page.on("pageerror", (e) => errors.push(String(e)));
  page.on("dialog", (d) => { dialogs.push(d.message()); d.dismiss(); });
  await page.route("**/*", (route) => {
    const u = new URL(route.request().url());
    if (u.hostname !== "fixture.invalid") return route.abort();
    if (u.pathname === "/graph") return route.fulfill({ contentType: "text/html", body: fs.readFileSync(path.join(ASSETS, "explorer.html")) });
    if (u.pathname === "/graph/graph3d.js") return route.fulfill({ contentType: "text/javascript", body: fs.readFileSync(path.join(ASSETS, "graph3d.js")) });
    if (u.pathname.startsWith("/api/")) return route.fulfill({ contentType: "application/json", body: JSON.stringify(answer(u)) });
    return route.fulfill({ status: 404, body: "" });
  });
  await page.goto("http://fixture.invalid/graph?renderer=" + (process.env.RENDERER || "webgl") + "#k=" + "0".repeat(64));
  await page.waitForSelector("#results li");
  await page.click("#drawSelection");
  await page.waitForTimeout(700);
  const wide = await assertPage(page, "wide");
  await assertInside(page, "wide");
  await page.screenshot({ path: path.join(out, label + "-wide.png") });
  await page.click("#dim2"); await page.waitForTimeout(300);
  await page.locator("#stage").screenshot({ path: path.join(out, label + "-wide-2d.png") });
  await page.click("#dim3");
  await page.setViewportSize({ width: 390, height: 844 }); await page.waitForTimeout(600);
  await assertPage(page, "narrow");
  await assertInside(page, "narrow after resize");
  // Usable on a phone: a tap on a drawn node opens it (hover text), and a drag orbits.
  const hit = await page.evaluate(() => {
    const v = window.__brainGraph.view, r = document.getElementById("stage").getBoundingClientRect();
    for (const n of v.nodes) { const p = window.BrainGraph.project(v.frame, ...v.drawnPos(n), n.size || 6);
      if (p && p.x > 20 && p.y > 20 && p.x < r.width - 20 && p.y < r.height - 60) return { x: r.left + p.x, y: r.top + p.y }; }
    return null; });
  if (!hit) throw new Error("narrow: no node on screen");
  await page.mouse.move(hit.x, hit.y); await page.waitForTimeout(150);
  if (!(await page.locator("#hoverText").isVisible())) throw new Error("narrow: hovering a node shows nothing");
  const yaw0 = await page.evaluate(() => window.__brainGraph.view.camera.yaw);
  await page.mouse.down(); await page.mouse.move(hit.x + 60, hit.y + 5, { steps: 4 }); await page.mouse.up();
  if (await page.evaluate(() => window.__brainGraph.view.camera.yaw) === yaw0) throw new Error("narrow: drag did not orbit");
  await page.mouse.move(195, 5); await page.waitForTimeout(150); // leave the stage
  if (await page.locator("#hoverText").isVisible()) throw new Error("narrow: hover text left over after leaving the stage");
  await page.click("#fit"); await page.waitForTimeout(500);
  await page.mouse.move(195, 5); await page.waitForTimeout(100);
  await assertInside(page, "narrow after Вписать");
  if (await page.locator("#hoverText").isVisible()) throw new Error("narrow: hover text over the legend");
  await page.screenshot({ path: path.join(out, label + "-narrow.png") });
  const noteText = wide.note, headline = wide.head;
  const metrics = await page.evaluate(() => window.__brainGraph.metrics());
  await browser.close();
  fs.writeFileSync(path.join(out, label + "-page.json"), JSON.stringify({ headline, note: noteText, renderer: metrics.renderer, draws: metrics.draws,
    nodes: metrics.nodes, edges: metrics.edges, errors, dialogs }, null, 1));
  if (errors.length || dialogs.length) { console.error("page errors/dialogs:", errors, dialogs); process.exit(1); }
  console.log("fixture screenshots written to " + out + " (renderer " + metrics.renderer + ")");
}
if (require.main === module) main().catch((e) => { console.error(e); process.exit(1); });
module.exports = { NODES, EDGES };
