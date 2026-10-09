// node tests/visual/map3d_fixture.js <out-dir> [label]
// Static, synthetic visual fixture for the 3D brain map. It is NOT the host
// server: map.html and map3d.js are read from brain-cli/assets and every
// /api/* request is answered from the invented rows below, inside the browser
// (Playwright route interception). Nothing leaves the machine; no personal data.
// Needs a local Chromium with Playwright; writes PNG screenshots to <out-dir>.
"use strict";
const fs = require("fs");
const path = require("path");

// BRAIN_MAP_ASSETS points at another copy of the two files (e.g. the base
// commit's) to take "before" screenshots from the same fixture.
const ASSETS = process.env.BRAIN_MAP_ASSETS || path.join(__dirname, "../../brain-cli/assets");
const PHASE_STATE = { past: "completed", current: "active", planned: "queued" };
/* Twelve invented projects; counts are arbitrary but cover every phase, a
   conflict, links and a project with no open work. */
const PROJECTS = [
  ["alpha-api", 6, 2, 3, 2, 0], ["beta-ui", 3, 1, 4, 1, 1], ["gamma-docs", 8, 0, 1, 0, 0],
  ["delta-cli", 2, 3, 2, 3, 0], ["epsilon-infra", 5, 1, 0, 0, 0], ["zeta-search", 1, 0, 5, 1, 0],
  ["eta-sync", 4, 2, 2, 2, 0], ["theta-auth", 7, 0, 0, 0, 0], ["iota-export", 0, 1, 3, 0, 0],
  ["kappa-tests", 9, 1, 1, 1, 0], ["lambda-cache", 2, 0, 2, 0, 0], ["mu-metrics", 3, 2, 1, 1, 1],
].map(([name, past, current, planned, links, conflicts]) => ({ name, past, current, planned, links, conflicts }));

function tasksOf(project) {
  const rows = [];
  let id = 100 * (PROJECTS.indexOf(project) + 1);
  for (const phase of ["past", "current", "planned"]) {
    for (let i = 0; i < project[phase]; i++) {
      rows.push({ id: ++id, phase, state: PHASE_STATE[phase], title: "synthetic " + PHASE_STATE[phase] + " task " + (i + 1) });
    }
  }
  return rows;
}

function answer(url) {
  const u = new URL(url), p = u.pathname;
  if (p === "/api/map/status") return { status: "ok", events: 42, links: 12, ingest_cursors: 1 };
  if (p === "/api/map/projects") return { status: "ok", projects: PROJECTS, has_more: false };
  if (p === "/api/map/project") {
    const project = PROJECTS.find((x) => x.name === u.searchParams.get("project")) || PROJECTS[0];
    return { status: "ok", tasks: tasksOf(project), has_more: false, links: { rows: [], next_cursor: null } };
  }
  if (p === "/api/map/packet") return { status: "ok", packet_bytes: 2, packet_json: "{}" };
  if (p === "/api/map/timeline") return { status: "ok", entries: [], next_after: null };
  return { status: "error", message: "synthetic fixture: not provided" };
}

async function main() {
  const out = path.resolve(process.argv[2] || "map3d-shots");
  const label = process.argv[3] || "shot";
  fs.mkdirSync(out, { recursive: true });
  const { chromium } = require("playwright");
  const browser = await chromium.launch({ executablePath: process.env.CHROMIUM || undefined });
  const page = await browser.newPage({ viewport: { width: 1280, height: 800 }, colorScheme: "light" });
  const errors = [];
  page.on("pageerror", (e) => errors.push(String(e)));
  await page.route("**/*", (route) => {
    const u = new URL(route.request().url());
    if (u.hostname !== "fixture.invalid") return route.abort();
    if (u.pathname === "/map") return route.fulfill({ contentType: "text/html", body: fs.readFileSync(path.join(ASSETS, "map.html")) });
    if (u.pathname === "/map/map3d.js") return route.fulfill({ contentType: "text/javascript", body: fs.readFileSync(path.join(ASSETS, "map3d.js")) });
    if (u.pathname.startsWith("/api/")) return route.fulfill({ contentType: "application/json", body: JSON.stringify(answer(u.href)) });
    return route.fulfill({ status: 404, body: "" });
  });
  await page.goto("http://fixture.invalid/map?space=fixture#k=" + "0".repeat(64));
  await page.waitForSelector("#projects .item");
  await page.click("#mode-3d");
  await page.waitForTimeout(300);
  await page.screenshot({ path: path.join(out, label + "-overview.png") });
  await page.click("#projects .item:nth-child(4)");
  await page.waitForTimeout(400);
  await page.screenshot({ path: path.join(out, label + "-selected.png") });
  await page.setViewportSize({ width: 390, height: 844 });
  await page.click("#mode-3d");
  await page.focus("#scene");
  await page.keyboard.press("Escape");
  await page.waitForTimeout(300);
  await page.locator("section.center").screenshot({ path: path.join(out, label + "-narrow.png") });
  await browser.close();
  if (errors.length) { console.error("page errors:\n" + errors.join("\n")); process.exit(1); }
  console.log("fixture screenshots written to " + out);
}

if (require.main === module) main().catch((e) => { console.error(e); process.exit(1); });
module.exports = { PROJECTS, tasksOf };
