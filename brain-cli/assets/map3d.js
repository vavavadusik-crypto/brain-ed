/* Brain for AI — the semantic work map in genuine 3D.
 *
 * The projects and their tasks are laid out on real X/Y/Z coordinates and drawn
 * through one perspective projection, `project`: a point behind the eye returns
 * null and is never drawn, sizes shrink with distance, and the painter draws
 * far-to-near (depth ordering). Orbiting, zooming and picking all ask the same
 * function, so the picture, the hit test and the labels agree.
 *
 * Nothing moves on its own: a frame is drawn only when the data or the camera
 * changes, and a reduced-motion preference turns the camera easing into a jump.
 * The scene is bounded (see CAPS.nodes) and deterministic — the same projects
 * and tasks always land in the same places.
 *
 * Layout is a pure function of the data the page already fetched:
 *   - every project sits on a Fibonacci sphere, ranked by name;
 *   - the selected project's tasks grow as three branches off its centre, one
 *     per phase (past behind / current up / planned ahead): each branch is a short helix along
 *     its own axis, tasks ranked by id, at distinct, deterministic XYZ points.
 * The selected project is always kept in the scene, even past the cap, and a
 * part of the cap is reserved for its tasks; summary() names the shown subset.
 * Projects come from the coordinator/map API; none are invented from directories.
 *
 * The overview is shaped as a brain: two lobed hemispheres (left x<0, right
 * x>0, the front is +z) with a fissure between them, a cerebellum and a short
 * stem, drawn through the same projection as a shaded silhouette with a few
 * sulci (see drawBrain). The projects sit inside the hemispheres. Eight labelled
 * regions sit on the surface. They are a product's way of sorting what memory
 * is about, not anatomy. A region only shows a number that the map API
 * already returned (see regionStatus). The other regions say plainly that
 * their records are not loaded here and name the page that has them.
 */
"use strict";
(function () {
  const FOV = 50 * Math.PI / 180;
  /* Documented safe cap on the default scene, so the map never tries to draw
     every node of a huge space at once. Everything beyond it is reachable by
     paging the lists, and the page says so. */
  const MAX_NODES = 300;
  const LABEL_BUDGET = 24; // hard cap on texts drawn per frame, every kind included
  const DPR_CAP = 2; // devicePixelRatio ceiling for the drawing surface
  const NARROW_LABELS = 10; // label budget for panels narrower than NARROW_WIDTH
  const NARROW_WIDTH = 420;
  const TASK_RESERVE = 120; // part of MAX_NODES kept for the selected project's tasks
  const BRANCH = 92; // where a phase branch starts, measured from a project centre
  const BRANCH_STEP = 34; // spacing of tasks along a branch while it is short
  const BRANCH_MAX = 260; // a long branch is compressed to this length
  const HELIX = 14; // base radius of the helix a branch winds around its axis
  const TASK_SIZE = 6;
  /* Time runs back to front, like the brain: past behind (-z), planned ahead
     (+z), current up. The default side view sees all three across the screen. */
  const PHASE_AXIS = { past: [0, 0, -1], current: [0, 1, 0], planned: [0, 0, 1] };
  /* Two directions across each axis, for the helix. */
  const PHASE_ACROSS = { past: [[0, 1, 0], [1, 0, 0]], current: [[0, 0, 1], [1, 0, 0]], planned: [[0, 1, 0], [1, 0, 0]] };
  const PHASE_NAMES = { past: "прошлое", current: "сейчас", planned: "в плане" };

  /* ---------- deterministic numbers ---------- */
  function hash32(text, salt) {
    let h = (0x811c9dc5 ^ (salt || 0)) >>> 0;
    for (let i = 0; i < text.length; i++) {
      h ^= text.charCodeAt(i);
      h = Math.imul(h, 0x01000193) >>> 0;
    }
    h ^= h >>> 16;
    h = Math.imul(h, 0x7feb352d) >>> 0;
    h ^= h >>> 15;
    h = Math.imul(h, 0x846ca68b) >>> 0;
    h ^= h >>> 16;
    return h >>> 0;
  }
  const unit = (text, salt) => hash32(text, salt) / 4294967296;
  /* A point inside a ball of radius r, uniform in volume, fixed by the id. */
  function ballOffset(id, r) {
    const theta = 2 * Math.PI * unit(id, 1);
    const cosPhi = 2 * unit(id, 2) - 1;
    const rad = r * Math.cbrt(unit(id, 3));
    const sinPhi = Math.sqrt(1 - cosPhi * cosPhi);
    return [rad * sinPhi * Math.cos(theta), rad * cosPhi, rad * sinPhi * Math.sin(theta)];
  }

  /* ---------- camera and the one projection ---------- */
  function lookAt(eye, target) {
    let zx = eye[0] - target[0], zy = eye[1] - target[1], zz = eye[2] - target[2];
    let len = Math.hypot(zx, zy, zz) || 1;
    zx /= len; zy /= len; zz /= len;
    let xx = zz, xy = 0, xz = -zx; // up = +y, cross(up, z)
    len = Math.hypot(xx, xy, xz) || 1;
    xx /= len; xy /= len; xz /= len;
    const yx = zy * xz - zz * xy, yy = zz * xx - zx * xz, yz = zx * xy - zy * xx;
    return [xx, yx, zx, 0, xy, yy, zy, 0, xz, yz, zz, 0,
      -(xx * eye[0] + xy * eye[1] + xz * eye[2]),
      -(yx * eye[0] + yy * eye[1] + yz * eye[2]),
      -(zx * eye[0] + zy * eye[1] + zz * eye[2]), 1];
  }
  function perspective(aspect, near, far) {
    const f = 1 / Math.tan(FOV / 2), nf = 1 / (near - far);
    return [f / aspect, 0, 0, 0, 0, f, 0, 0, 0, 0, (far + near) * nf, -1, 0, 0, 2 * far * near * nf, 0];
  }
  function multiply(a, b) { // column-major a*b
    const out = new Array(16);
    for (let c = 0; c < 4; c++) for (let r = 0; r < 4; r++) {
      out[c * 4 + r] = a[r] * b[c * 4] + a[4 + r] * b[c * 4 + 1] + a[8 + r] * b[c * 4 + 2] + a[12 + r] * b[c * 4 + 3];
    }
    return out;
  }

  class Camera {
    constructor() {
      // From the left and above: the outer face of the left hemisphere (lobes,
      // sulci, cerebellum, stem; front to the right) reads as a brain, and the
      // right hemisphere rises behind it, so the projects of the two halves
      // do not fall onto each other.
      this.yaw = -1.22; this.pitch = 0.4; this.dist = 600;
      this.target = [0, 0, 0]; this.ref = 600;
    }
    eye() {
      return [this.target[0] + this.dist * Math.cos(this.pitch) * Math.sin(this.yaw),
        this.target[1] + this.dist * Math.sin(this.pitch),
        this.target[2] + this.dist * Math.cos(this.pitch) * Math.cos(this.yaw)];
    }
    frame(width, height) {
      const near = Math.max(1, this.dist / 100), far = this.dist * 6 + 3000;
      const vp = multiply(perspective(width / Math.max(1, height), near, far), lookAt(this.eye(), this.target));
      return { vp, width, height, ref: this.ref || this.dist };
    }
  }

  /* World point -> CSS pixels, clip w, on-screen radius. null behind the eye.
     This is the single source of truth for drawing, picking and labels. */
  function project(frame, x, y, z, size) {
    const m = frame.vp;
    const cx = m[0] * x + m[4] * y + m[8] * z + m[12];
    const cy = m[1] * x + m[5] * y + m[9] * z + m[13];
    const cw = m[3] * x + m[7] * y + m[11] * z + m[15];
    if (cw <= 0.0001) return null;
    return { x: (cx / cw * 0.5 + 0.5) * frame.width, y: (1 - (cy / cw * 0.5 + 0.5)) * frame.height, w: cw,
      r: Math.max(1.5, Math.min(48, size * Math.max(0.5, Math.min(2.5, frame.ref / cw)))) };
  }

  const nodeSize = (weight) => 2.6 + 1.4 * Math.log2(1 + Math.max(0, weight || 0));

  /* ---------- bounded, deterministic layouts ---------- */
  /* Projects on a Fibonacci sphere, ranked by name so the same set always lands
     the same way regardless of arrival order. */
  /* ---------- the brain silhouette ---------- */
  /* Each hemisphere is a lofted solid: a side profile in the z-y plane (front
     is +z, up is +y) swept across x, so that a point is on the surface when
     ((x - cx) / rx)^2 + (r / rho(theta))^2 = 1, with r and theta the polar
     radius and angle of (z, y) and rho the profile's radius at that angle. The
     outer rx is wider than the inner one, which leaves a narrow longitudinal
     fissure between the halves. The profile gives a blunt frontal lobe, a domed
     top, an occipital pole and a temporal lobe under a lateral-fissure notch, so
     the shape reads as a brain; it is a recognisable sketch, not anatomy.
     Sizes are multiplied by brainScale(n) so many projects do not crowd. */
  const TAU = 2 * Math.PI;
  const HEMI = { cx: 74, rx: 80, rxInner: 70, ry: 100, rz: 168 };
  /* The side profile, as (z / rz, y / ry) points; star-shaped around the origin. */
  const PROFILE = [[1, 0.05], [0.9, 0.48], [0.62, 0.84], [0.2, 1], [-0.25, 0.96], [-0.66, 0.74], [-0.93, 0.36],
    [-1, -0.04], [-0.86, -0.34], [-0.56, -0.44], [-0.15, -0.56], [0.24, -0.74], [0.5, -0.63], [0.6, -0.42],
    [0.56, -0.27], [0.8, -0.22], [0.96, -0.12]];
  const PROFILE_POLAR = PROFILE.map(([z, y]) => ({ a: (Math.atan2(y * HEMI.ry, z * HEMI.rz) + TAU) % TAU,
    r: Math.hypot(y * HEMI.ry, z * HEMI.rz) })).sort((p, q) => p.a - q.a);
  const CEREBELLUM = { c: [0, -70, -112], r: [100, 38, 50] };
  const STEM = { from: [0, -58, -40], to: [0, -182, -56], r: [21, 14] };
  const MESH_AROUND = 40, MESH_ACROSS = 12;
  /* Sulci on the outer face of a hemisphere, as (z / rz, y / ry) control points
     lifted onto the surface. major: the lateral fissure and the central sulcus. */
  const SULCI = [
    { major: true, at: [[0.57, -0.27], [0.3, -0.17], [0.04, -0.1], [-0.24, 0.02], [-0.42, 0.18]] },
    { major: true, at: [[0.02, 0.99], [0.09, 0.74], [0, 0.5], [0.08, 0.27], [0.02, 0.02]] },
    { major: false, at: [[0.27, 0.95], [0.32, 0.7], [0.25, 0.45], [0.3, 0.2], [0.24, -0.03]] },
    { major: false, at: [[-0.2, 0.95], [-0.15, 0.7], [-0.22, 0.45], [-0.16, 0.12]] },
    { major: false, at: [[0.84, 0.36], [0.66, 0.5], [0.5, 0.46], [0.36, 0.58]] },
    { major: false, at: [[0.86, 0.06], [0.68, 0.16], [0.52, 0.1], [0.38, 0.22]] },
    { major: false, at: [[0.5, -0.46], [0.28, -0.38], [0.02, -0.33], [-0.28, -0.22], [-0.52, -0.08]] },
    { major: false, at: [[-0.32, 0.62], [-0.5, 0.5], [-0.66, 0.32], [-0.8, 0.14]] },
  ];
  /* Region anchors: a side and a (z / rz, y / ry) point on that hemisphere's
     outer face, the cerebellum, or the bridge between the hemispheres. The
     places follow the usual association of the name, not anatomy: profile and
     decisions/plans on frontal lobes, conversations on a temporal lobe, active
     work by the central sulcus, error correction on the cerebellum. */
  const REGIONS = [
    { id: "profile", name: "Предпочтения и профиль", side: -1, at: [0.84, 0.3] },
    { id: "episodes", name: "Эпизоды и разговоры", side: -1, at: [0.24, -0.5] },
    { id: "projects", name: "Проекты и знания", side: 1, at: [-0.4, 0.55] },
    { id: "decisions", name: "Решения", side: 1, at: [0.58, 0.62] },
    { id: "errors", name: "Ошибки и уроки", cerebellum: true },
    { id: "active", name: "Активная работа", side: -1, at: [0.12, 0.82] },
    { id: "plans", name: "Планы", side: 1, at: [0.84, 0.08] },
    { id: "coordination", name: "Координация", bridge: true },
  ];
  const STALE_MS = 5 * 60 * 1000;

  function brainScale(n) { return Math.max(1, (110 + 40 * Math.sqrt(Math.max(0, n))) / 170); }
  /* The profile's radius at a polar angle: a periodic Catmull-Rom spline. */
  function profileRadius(theta) {
    const P = PROFILE_POLAR, n = P.length;
    let a = ((theta % TAU) + TAU) % TAU, i = n - 1;
    for (let k = 0; k < n; k++) if (P[k].a <= a) i = k;
    if (a < P[0].a) a += TAU;
    const j = (i + 1) % n, a1 = j === 0 ? P[0].a + TAU : P[j].a;
    const t = (a - P[i].a) / (a1 - P[i].a);
    const p0 = P[(i - 1 + n) % n].r, p1 = P[i].r, p2 = P[j].r, p3 = P[(j + 1) % n].r;
    return 0.5 * (2 * p1 + (p2 - p0) * t + (2 * p0 - 5 * p1 + 4 * p2 - p3) * t * t + (3 * p1 - p0 - 3 * p2 + p3) * t * t * t);
  }
  /* 1 on a hemisphere's surface, below 1 inside; pos in scaled world units. */
  function hemisphereNorm(pos, scale) {
    const side = pos[0] < 0 ? -1 : 1, s = scale || 1;
    const dx = pos[0] / s - side * HEMI.cx, y = pos[1] / s, z = pos[2] / s;
    return Math.hypot(dx / (dx * side >= 0 ? HEMI.rx : HEMI.rxInner), Math.hypot(y, z) / profileRadius(Math.atan2(y, z)));
  }
  /* Where a direction from a hemisphere centre meets that hemisphere's surface,
     times k. The x sign of the direction picks the hemisphere (its outer half). */
  function onHemisphere(dir, scale, k) {
    const side = dir[0] < 0 ? -1 : 1, yz = Math.hypot(dir[1], dir[2]);
    const t = 1 / Math.hypot(dir[0] / HEMI.rx, yz / profileRadius(Math.atan2(dir[1], dir[2])));
    return [scale * (side * HEMI.cx + dir[0] * t * k), scale * dir[1] * t * k, scale * dir[2] * t * k];
  }
  /* A (z / rz, y / ry) point lifted onto the outer face of one hemisphere. */
  function liftOuter(side, at) {
    const y = at[1] * HEMI.ry, z = at[0] * HEMI.rz;
    const q = Math.min(0.999, Math.hypot(y, z) / profileRadius(Math.atan2(y, z)));
    return [side * (HEMI.cx + HEMI.rx * Math.sqrt(1 - q * q)), y, z];
  }
  function cerebellumPoint(dir) {
    const { c, r } = CEREBELLUM, t = 1 / Math.hypot(dir[0], dir[1], dir[2]);
    return [c[0] + r[0] * dir[0] * t, c[1] + r[1] * dir[1] * t, c[2] + r[2] * dir[2] * t];
  }
  function regionAnchor(region, scale) {
    if (region.bridge) return [0, scale * HEMI.ry * 0.7, 0];
    const base = region.cerebellum ? cerebellumPoint([-0.6, 0.35, -0.7]) : liftOuter(region.side, region.at);
    const centre = region.cerebellum ? CEREBELLUM.c : [region.side * HEMI.cx, 0, 0];
    return base.map((v, k) => scale * (centre[k] + (v - centre[k]) * 1.05));
  }
  const sub = (a, b) => [a[0] - b[0], a[1] - b[1], a[2] - b[2]];
  const dot = (a, b) => a[0] * b[0] + a[1] * b[1] + a[2] * b[2];
  const cross = (a, b) => [a[1] * b[2] - a[2] * b[1], a[2] * b[0] - a[0] * b[2], a[0] * b[1] - a[1] * b[0]];
  /* Outward surface normal by central differences of a field that grows outward. */
  function fieldNormal(field, p) {
    const e = 0.5, n = [0, 1, 2].map((k) => {
      const a = p.slice(), b = p.slice(); a[k] += e; b[k] -= e;
      return field(a) - field(b);
    });
    const len = Math.hypot(n[0], n[1], n[2]) || 1;
    return n.map((v) => v / len);
  }
  /* Surface curves drawn where they face the eye: sulci on both hemispheres and
     folia rings on the cerebellum. Each is {major, points, normals}, scaled.
     Bounded: 2 * SULCI.length + 4 polylines of at most 25 points. */
  function brainOutline(scale) {
    const lines = [];
    const cerebField = (p) => Math.hypot(...[0, 1, 2].map((k) => (p[k] - CEREBELLUM.c[k]) / CEREBELLUM.r[k]));
    const push = (major, points, field) => lines.push({ major, points: points.map((q) => q.map((v) => v * scale)),
      normals: points.map((q) => fieldNormal(field, q)) });
    for (const side of [-1, 1]) {
      for (const sulcus of SULCI) {
        const c = sulcus.at, points = [];
        for (let i = 0; i < c.length - 1; i++) for (let step = 0; step < 6; step++) {
          const t = step / 6, p0 = c[Math.max(0, i - 1)], p1 = c[i], p2 = c[i + 1], p3 = c[Math.min(c.length - 1, i + 2)];
          const at = [0, 1].map((k) => 0.5 * (2 * p1[k] + (p2[k] - p0[k]) * t + (2 * p0[k] - 5 * p1[k] + 4 * p2[k] - p3[k]) * t * t
            + (3 * p1[k] - p0[k] - 3 * p2[k] + p3[k]) * t * t * t));
          points.push(liftOuter(side, at));
        }
        points.push(liftOuter(side, c[c.length - 1]));
        push(sulcus.major, points, (p) => hemisphereNorm(p, 1));
      }
    }
    for (const h of [-0.45, -0.1, 0.25, 0.6]) {
      const k = Math.sqrt(1 - h * h), { c, r } = CEREBELLUM, points = [];
      for (let i = 0; i <= 24; i++) {
        const a = TAU * i / 24;
        points.push([c[0] + r[0] * k * Math.cos(a), c[1] + r[1] * h, c[2] + r[2] * k * Math.sin(a)]);
      }
      push(false, points, cerebField);
    }
    return lines;
  }
  /* Closed meshes of the solids, for the silhouette and the light shading: each
     hemisphere and the cerebellum as (MESH_ACROSS + 1) x MESH_AROUND points and
     MESH_ACROSS x MESH_AROUND quads with outward normals. */
  function brainShells(scale) {
    const loft = (name, centre, point) => {
      const points = [];
      for (let i = 0; i <= MESH_ACROSS; i++) {
        const s = -Math.PI / 2 + Math.PI * i / MESH_ACROSS;
        for (let j = 0; j < MESH_AROUND; j++) points.push(point(TAU * j / MESH_AROUND, s).map((v) => v * scale));
      }
      const faces = [], c = centre.map((v) => v * scale);
      for (let i = 0; i < MESH_ACROSS; i++) for (let j = 0; j < MESH_AROUND; j++) {
        const jn = (j + 1) % MESH_AROUND;
        const quad = [i * MESH_AROUND + j, i * MESH_AROUND + jn, (i + 1) * MESH_AROUND + jn, (i + 1) * MESH_AROUND + j];
        const [a, b, cc, d] = quad.map((q) => points[q]);
        let n = cross(sub(cc, a), sub(d, b));
        const mid = [0, 1, 2].map((k) => (a[k] + b[k] + cc[k] + d[k]) / 4);
        if (dot(n, sub(mid, c)) < 0) n = n.map((v) => -v);
        const len = Math.hypot(n[0], n[1], n[2]) || 1;
        faces.push({ quad, mid, n: n.map((v) => v / len) });
      }
      return { name, centre: c, points, faces };
    };
    const hemi = (side) => loft(side < 0 ? "left" : "right", [side * HEMI.cx, 0, 0], (theta, s) => {
      const rho = profileRadius(theta) * Math.cos(s), rx = s >= 0 ? HEMI.rx : HEMI.rxInner;
      return [side * (HEMI.cx + rx * Math.sin(s)), rho * Math.sin(theta), rho * Math.cos(theta)];
    });
    const { c, r } = CEREBELLUM;
    return [hemi(-1), hemi(1), loft("cerebellum", c, (theta, s) =>
      [c[0] + r[0] * Math.sin(s), c[1] + r[1] * Math.cos(s) * Math.sin(theta), c[2] + r[2] * Math.cos(s) * Math.cos(theta)])];
  }
  function brainStem(scale) {
    return { from: STEM.from.map((v) => v * scale), to: STEM.to.map((v) => v * scale), r: STEM.r.map((v) => v * scale) };
  }
  /* Convex hull of screen points (monotone chain); the outline of one solid. */
  function hull2d(points) {
    const p = points.slice().sort((a, b) => a.x - b.x || a.y - b.y);
    if (p.length < 3) return p;
    const cross = (o, a, b) => (a.x - o.x) * (b.y - o.y) - (a.y - o.y) * (b.x - o.x);
    const lower = [], upper = [];
    for (const q of p) { while (lower.length >= 2 && cross(lower[lower.length - 2], lower[lower.length - 1], q) <= 0) lower.pop(); lower.push(q); }
    for (let i = p.length - 1; i >= 0; i--) { const q = p[i]; while (upper.length >= 2 && cross(upper[upper.length - 2], upper[upper.length - 1], q) <= 0) upper.pop(); upper.push(q); }
    return lower.slice(0, -1).concat(upper.slice(0, -1));
  }
  function clock(ms) {
    const d = new Date(ms);
    return String(d.getHours()).padStart(2, "0") + ":" + String(d.getMinutes()).padStart(2, "0");
  }
  /* What each region can truthfully say, from the project rows the page loaded
     (GET /api/map/projects) and when it loaded them. Counts exist only for
     regions those rows cover. "elsewhere" names an existing page and never
     claims a number. meta: {now, fetchedAt, hasMore}. */
  function regionStatus(projects, meta) {
    const m = meta || {}, rows = projects || [], loaded = rows.length;
    const sum = (key) => rows.reduce((total, p) => total + (Number(p[key]) || 0), 0);
    const partial = m.hasMore ? " по загруженной части списка" : "";
    const stale = !!(m.fetchedAt && m.now && m.now - m.fetchedAt > STALE_MS);
    const when = m.fetchedAt ? " Получено в " + clock(m.fetchedAt) + (stale ? " — устарело, откройте space заново." : ".")
      : " Время получения неизвестно.";
    const fromBoard = (count, text) => (loaded === 0
      ? { state: "empty", count: 0, stale, text: "Проекты не загружены — данных нет." }
      : { state: count ? "data" : "empty", count, stale, text: text + count + partial + "." + when });
    const elsewhere = (route, text) => ({ state: "elsewhere", count: null, stale: false, route, text });
    return {
      profile: elsewhere("/graph", "Карта работы не загружает предпочтения и профиль. Записи памяти — в «Графе памяти»."),
      episodes: elsewhere("/", "Карта работы не загружает разговоры. Архив переписок — в «Настройках», раздел «Архив переписок»."),
      projects: loaded
        ? { state: "data", count: loaded, stale, text: "Проектов загружено: " + loaded + partial + " (координатор и карта работы)." + when }
        : { state: "empty", count: 0, stale, text: "В этом space проектов нет или они ещё не загружены." },
      decisions: elsewhere("/graph", "Карта работы не загружает решения. Записи памяти — в «Графе памяти»; решения проекта — в его пакете справа."),
      errors: elsewhere("/graph", "Карта работы не загружает ошибки и уроки. Записи памяти — в «Графе памяти»."),
      active: fromBoard(sum("current"), "Задач «сейчас» на доске координатора (заявлено агентами при захвате; не наблюдение чтения или правки файлов): "),
      plans: fromBoard(sum("planned"), "Задач «в плане» на доске координатора: "),
      coordination: (() => {
        const links = sum("links"), conflicts = sum("conflicts");
        if (!loaded) return { state: "empty", count: 0, stale, text: "Проекты не загружены — данных нет." };
        return { state: links + conflicts ? "data" : "empty", count: links + conflicts, stale,
          text: "Связей между задачами: " + links + ", споров: " + conflicts + partial + "." + when };
      })(),
    };
  }

  function projectCenters(projects, cap, keep) {
    const limit = Math.min(projects.length, cap || MAX_NODES);
    let chosen = projects.slice(0, limit);
    // The selected project stays reachable even when it was loaded past the cap.
    if (keep !== undefined && keep !== null && !chosen.some((p) => p.name === keep)) {
      const kept = projects.find((p) => p.name === keep);
      if (kept) chosen = chosen.slice(0, Math.max(0, limit - 1)).concat(kept);
    }
    const ranked = chosen.sort((a, b) => String(a.name).localeCompare(String(b.name), "ru"));
    const n = ranked.length;
    const scale = brainScale(n);
    return ranked.map((p, i) => {
      // A Fibonacci direction; its x sign picks the hemisphere, 0.72 keeps it inside.
      const y = 1 - 2 * (i + 0.5) / n, r = Math.sqrt(Math.max(0, 1 - y * y)), t = i * 2.399963229728653;
      const pos = onHemisphere([r * Math.cos(t) || 1e-9, y, r * Math.sin(t)], scale, 0.72);
      const total = (p.past || 0) + (p.current || 0) + (p.planned || 0);
      return { id: "project:" + p.name, project: p.name, name: p.name, pos, total,
        size: 6 + 18 * Math.sqrt(total / Math.max(1, ...ranked.map((q) => (q.past || 0) + (q.current || 0) + (q.planned || 0)))),
        phase: p.conflicts ? "conflict" : (p.current ? "current" : (p.planned ? "planned" : "past")),
        links: p.links || 0, conflicts: p.conflicts || 0 };
    });
  }

  const phaseOf = (t) => (PHASE_AXIS[t.phase] ? t.phase : "planned");
  /* Distance along a branch of the index-th of n tasks: evenly spaced while the
     branch is short, compressed to BRANCH_MAX once it is long. */
  function branchAlong(index, n) {
    const length = Math.min(BRANCH_MAX, BRANCH_STEP * Math.max(0, n - 1));
    return BRANCH + (n > 1 ? length * index / (n - 1) : 0);
  }
  /* The selected project's tasks as three branches — one per phase. Within a
     branch tasks are ranked by id and wound on a helix, so the same tasks
     always give the same picture at separate XYZ points. Two points can still
     project onto each other from some camera angle; orbiting separates them. */
  function taskBranches(center, tasks, cap) {
    const limit = Math.min(tasks.length, cap === undefined ? MAX_NODES : cap);
    const ranked = tasks.slice(0, limit).sort((a, b) => (Number(a.id) || 0) - (Number(b.id) || 0));
    const counts = { past: 0, current: 0, planned: 0 };
    for (const t of ranked) counts[phaseOf(t)]++;
    const out = [];
    const perPhase = { past: 0, current: 0, planned: 0 };
    for (const t of ranked) {
      const phase = phaseOf(t), n = counts[phase];
      const index = perPhase[phase]++;
      const axis = PHASE_AXIS[phase], [u, v] = PHASE_ACROSS[phase];
      const along = branchAlong(index, n);
      const radius = n > 1 ? HELIX + Math.min(26, n * 0.4) : 0, angle = index * 2.399963229728653;
      const cu = radius * Math.cos(angle), cv = radius * Math.sin(angle);
      const pos = [0, 1, 2].map((k) => center.pos[k] + axis[k] * along + u[k] * cu + v[k] * cv);
      out.push({ id: "task:" + t.id, task: t.id, phase, state: t.state || "", title: t.title || "",
        pos, size: phase === "current" ? TASK_SIZE + 1 : TASK_SIZE, index, project: center.project });
    }
    return out;
  }
  /* Where each non-empty branch is named: just past its last task. */
  function branchTips(center, branchItems) {
    const counts = { past: 0, current: 0, planned: 0 };
    for (const t of branchItems) counts[t.phase]++;
    return Object.keys(counts).filter((phase) => counts[phase]).map((phase) => {
      const along = branchAlong(counts[phase] - 1, counts[phase]) + 40, axis = PHASE_AXIS[phase];
      return { phase, count: counts[phase], pos: [0, 1, 2].map((k) => center.pos[k] + axis[k] * along), size: 1 };
    });
  }

  /* ---------- depth ordering and picking over a flat item list ---------- */
  /* Screen geometry of one item, or null when it is behind the eye. */
  function screenOf(frame, item) {
    const p = project(frame, item.pos[0], item.pos[1], item.pos[2], item.size || 6);
    return p ? { item, x: p.x, y: p.y, r: p.r, w: p.w } : null;
  }
  /* Far points first: larger clip w is farther from the eye. Points behind the
     camera are dropped here, so nothing is drawn to a hidden place. */
  function depthOrder(items, frame) {
    const seen = [];
    for (const item of items) {
      const s = screenOf(frame, item);
      if (s) seen.push(s);
    }
    seen.sort((a, b) => b.w - a.w);
    return seen;
  }
  /* Nearest to the cursor, then nearest to the eye — the same rule the drawn
     picture uses, so what is picked is what is seen. */
  function pick(items, frame, px, py) {
    let best = null, bestScore = Infinity;
    for (const item of items) {
      const s = screenOf(frame, item);
      if (!s) continue;
      // Task dots are small, so they get a slightly wider reach than a bare 8px.
      const d = Math.hypot(s.x - px, s.y - py), reach = Math.max(item.task !== undefined ? 10 : 8, s.r + 3);
      if (d > reach) continue;
      const score = d / reach + s.w * 1e-6;
      if (score < bestScore) { bestScore = score; best = item; }
    }
    return best;
  }

  /* A short canvas label: the id and the start of the agent-written title. */
  function taskLabel(item) {
    const title = String(item.title || "").trim();
    const head = "#" + item.task;
    if (!title) return head;
    const room = Math.max(0, 22 - head.length - 1);
    return head + " " + (title.length > room ? title.slice(0, Math.max(0, room - 1)) + "…" : title);
  }

  /* ---------- the view ---------- */
  const INTERACTIVE_SELECTOR = "button, a, input, select, textarea, summary, details, [contenteditable]";

  class Map3DView {
    /* host: {wrap, canvas, onSelect, onHoverText, onFrame} */
    constructor(host) {
      this.host = host;
      this.camera = new Camera();
      this.rawProjects = [];
      this.projects = [];
      this.tasks = [];
      this.tips = [];
      this.selected = null;
      this.selectedTask = null;
      this.hover = null;
      this.items = [];
      this.regions = [];
      this.outline = [];
      this.regionMeta = {};
      this.layoutSignature = null;
      this.ratioCap = DPR_CAP;
      this.reducedMotion = !!(typeof window !== "undefined" && window.matchMedia
        && window.matchMedia("(prefers-reduced-motion: reduce)").matches);
      this.dirty = true;
      this.pending = false;
      this.animation = null;
      this.ctx = host.canvas.getContext("2d");
      this.fit(false);
      this.bindInput();
      if (typeof document !== "undefined") {
        document.addEventListener("visibilitychange", () => { if (!document.hidden) this.request(); });
      }
      if (typeof ResizeObserver !== "undefined") new ResizeObserver(() => this.resize()).observe(host.wrap);
    }

    /* ----- scene contents ----- */
    setProjects(projects) {
      this.rawProjects = (projects || []).slice();
      this.rebuild();
    }
    selectProject(name) {
      this.selected = name === undefined ? null : name;
      this.selectedTask = null;
      this.rebuild();
    }
    setTasks(tasks) {
      this.tasks = (tasks || []).slice();
      this.rebuild();
    }
    setData(projects, selected, tasks) {
      this.rawProjects = (projects || []).slice();
      const next = selected === undefined ? null : selected;
      if (next !== this.selected) this.selectedTask = null;
      this.selected = next;
      this.tasks = (tasks || []).slice();
      this.rebuild();
    }
    /* {fetchedAt, hasMore} of the loaded project rows, for truthful region text. */
    setRegionMeta(meta) {
      this.regionMeta = Object.assign({}, meta || {});
      this.rebuild();
    }
    regionStatuses() {
      return regionStatus(this.rawProjects, Object.assign({ now: Date.now() }, this.regionMeta));
    }
    clear() {
      this.regionMeta = {};
      this.rawProjects = []; this.projects = []; this.tasks = []; this.tips = [];
      this.selected = null; this.selectedTask = null; this.hover = null;
      this.items = []; this.layoutSignature = null; this.dirty = true; this.request();
    }
    /* The flat, bounded item list the projection, picking and drawing agree on. */
    rebuild() {
      const reserve = this.selected === null ? 0 : Math.min(this.tasks.length, TASK_RESERVE);
      this.projects = projectCenters(this.rawProjects, MAX_NODES - reserve, this.selected);
      const items = this.projects.slice();
      const center = this.selected === null ? null : this.projects.find((p) => p.project === this.selected);
      const branches = center ? taskBranches(center, this.tasks, MAX_NODES - items.length) : [];
      items.push(...branches);
      this.selectedCenter = center || null;
      this.branches = branches;
      this.tips = center ? branchTips(center, branches) : [];
      this.items = items.slice(0, MAX_NODES);
      const scale = brainScale(this.projects.length);
      this.outline = brainOutline(scale);
      this.shells = brainShells(scale);
      this.stem = brainStem(scale);
      const statuses = this.regionStatuses();
      this.regions = REGIONS.map((r) => ({ id: "region:" + r.id, region: r.id, name: r.name,
        pos: regionAnchor(r, scale), size: 9, status: statuses[r.id] }));
      const signature = JSON.stringify([this.selected, this.items.map((n) => [n.id, ...n.pos])]);
      if (signature !== this.layoutSignature) {
        this.layoutSignature = signature;
        this.fit(false);
      }
      this.dirty = true; this.request();
    }
    visibleCount() { return this.items.length; }
    /* What the scene shows out of what the page has loaded — never the whole graph. */
    summary() {
      const tasksShown = this.items.filter((i) => i.task !== undefined).length;
      return { projectsShown: this.items.length - tasksShown, projectsLoaded: this.rawProjects.length,
        tasksShown, tasksLoaded: this.selectedCenter ? this.tasks.length : 0, selected: this.selected };
    }
    has(id) { return this.items.some((i) => i.id === id); }
    pick(clientX, clientY) {
      if (!this.frame) return null;
      const rect = this.host.wrap.getBoundingClientRect();
      return pick(this.items.concat(this.regions), this.frame, clientX - rect.left, clientY - rect.top);
    }

    /* ----- camera ----- */
    /* With a selected project that has tasks, the camera frames that project and
       its branches (readable); otherwise, or on request, the whole scene. */
    focusItems(whole) {
      if (!whole && this.selectedCenter && this.branches && this.branches.length) {
        return [this.selectedCenter, ...this.branches, ...this.tips];
      }
      // The overview frames the whole brain, not only the projects inside it.
      const hull = (this.shells || []).flatMap((m) => m.points).map((pos) => ({ pos, size: 0 }));
      return this.items.concat(this.regions, hull);
    }
    bounds(whole) {
      const focus = this.focusItems(whole);
      if (!focus.length) return { center: [0, 0, 0], radius: 220 };
      const center = [0, 1, 2].map((k) => focus.reduce((s, n) => s + n.pos[k], 0) / focus.length);
      const radius = Math.max(80, ...focus.map((n) =>
        Math.hypot(n.pos[0] - center[0], n.pos[1] - center[1], n.pos[2] - center[2]) + n.size));
      return { center, radius };
    }
    fit(animate, whole) {
      const { center, radius } = this.bounds(whole);
      const width = this.size ? this.size.width : 800, height = this.size ? this.size.height : 600;
      // Fit the narrower field of view and reserve room for screen-sized nodes.
      const usableAspect = Math.max(1, width - 80) / Math.max(1, height - 80);
      const halfAngle = Math.atan(Math.tan(FOV / 2) * Math.min(1, usableAspect));
      let dist = radius / Math.sin(halfAngle) * 1.12, target = center;
      // A bounding sphere wastes room on a long shape: measure the projected
      // extent from the final angle once, then re-centre and rescale to it.
      const probe = new Camera();
      Object.assign(probe, { yaw: this.camera.yaw, pitch: this.camera.pitch, target: center, dist, ref: dist });
      const frame = probe.frame(width, height);
      const seen = this.focusItems(whole).map((n) => screenOf(frame, n)).filter(Boolean);
      if (seen.length) {
        const x0 = Math.min(...seen.map((s) => s.x - s.r)), x1 = Math.max(...seen.map((s) => s.x + s.r));
        const y0 = Math.min(...seen.map((s) => s.y - s.r)), y1 = Math.max(...seen.map((s) => s.y + s.r));
        const perPixel = dist * 2 * Math.tan(FOV / 2) / Math.max(1, height);
        const ox = ((x0 + x1) / 2 - width / 2) * perPixel, oy = ((y0 + y1) / 2 - height / 2) * perPixel;
        const yaw = this.camera.yaw, pitch = this.camera.pitch;
        const right = [Math.cos(yaw), 0, -Math.sin(yaw)];
        const up = [-Math.sin(pitch) * Math.sin(yaw), Math.cos(pitch), -Math.sin(pitch) * Math.cos(yaw)];
        target = center.map((v, k) => v + ox * right[k] - oy * up[k]);
        dist *= Math.max(0.55, Math.min(1.2, Math.max((x1 - x0) / Math.max(1, width - 80), (y1 - y0) / Math.max(1, height - 80))));
      }
      this.camera.ref = dist;
      this.moveCamera(target, dist, animate);
    }
    moveCamera(target, dist, animate) {
      const cam = this.camera;
      if (!animate || this.reducedMotion) {
        cam.target = target.slice(); cam.dist = dist; this.request(); return;
      }
      const from = { target: cam.target.slice(), dist: cam.dist }, started = performance.now(), duration = 300;
      this.animation = (now) => {
        const t = Math.min(1, (now - started) / duration), e = t * t * (3 - 2 * t);
        cam.target = from.target.map((v, k) => v + (target[k] - v) * e);
        cam.dist = from.dist + (dist - from.dist) * e;
        return t < 1;
      };
      this.request();
    }
    orbit(dYaw, dPitch) {
      this.camera.yaw -= dYaw;
      this.camera.pitch = Math.max(-1.45, Math.min(1.45, this.camera.pitch + dPitch));
      this.request();
    }
    pan(dx, dy) {
      const cam = this.camera;
      const k = cam.dist * 2 * Math.tan(FOV / 2) / Math.max(1, this.size ? this.size.height : 600);
      const right = [Math.cos(cam.yaw), 0, -Math.sin(cam.yaw)];
      const up = [-Math.sin(cam.pitch) * Math.sin(cam.yaw), Math.cos(cam.pitch), -Math.sin(cam.pitch) * Math.cos(cam.yaw)];
      for (let i = 0; i < 3; i++) cam.target[i] += (-dx * right[i] + dy * up[i]) * k;
      this.request();
    }
    zoom(factor) {
      this.camera.dist = Math.max(20, Math.min(8000, this.camera.dist * factor));
      this.request();
    }

    /* ----- drawing on demand ----- */
    request() {
      if (this.pending) return;
      this.pending = true;
      if (typeof requestAnimationFrame !== "function") { this.pending = false; this.render(0); return; }
      requestAnimationFrame((now) => { this.pending = false; this.render(now); });
    }
    resize() {
      const rect = this.host.wrap.getBoundingClientRect();
      if (!rect.width || !rect.height) return;
      const changed = !this.size || this.size.width !== rect.width || this.size.height !== rect.height;
      const ratio = Math.min((typeof window !== "undefined" && window.devicePixelRatio) || 1, this.ratioCap);
      this.host.canvas.width = Math.max(1, Math.round(rect.width * ratio));
      this.host.canvas.height = Math.max(1, Math.round(rect.height * ratio));
      this.size = { width: rect.width, height: rect.height, ratio };
      if (changed && this.items.length) this.fit(false);
      this.dirty = true; this.request();
    }
    color(name, fallback) {
      const text = typeof getComputedStyle === "function"
        ? getComputedStyle(document.documentElement).getPropertyValue(name).trim() : "";
      return text || fallback;
    }
    /* No animation loop: one frame per change. A hidden tab draws nothing, and
       when the page is showing the 2D view this renderer stays silent so the
       two views never fight over the one canvas. */
    render(now) {
      if (this.host.active && !this.host.active()) return;
      if (typeof document !== "undefined" && document.hidden) return;
      if (!this.size || (this.host.wrap.getClientRects && this.host.wrap.getClientRects().length === 0)) return;
      if (this.animation && !this.animation(now || 0)) this.animation = null;
      const { width, height, ratio } = this.size;
      const ctx = this.ctx;
      ctx.setTransform(ratio, 0, 0, ratio, 0, 0);
      ctx.clearRect(0, 0, width, height);
      const frame = this.camera.frame(width, height);
      this.frame = frame;
      const order = depthOrder(this.items, frame); // far first
      const byPhase = { past: this.color("--past", "#6b7280"), current: this.color("--current", "#0f7b5f"),
        planned: this.color("--planned", "#2563eb"), conflict: this.color("--conflict", "#b42318") };
      this.drawBrain(frame);
      // Region markers: filled only when the loaded data gives them records.
      const accent = this.color("--accent", "#0f5bd8"), dim = this.color("--dim", "#5c6472");
      this.regionScreens = [];
      for (const region of this.regions) {
        const s = screenOf(frame, region);
        if (!s) continue;
        this.regionScreens.push(s);
        const state = region.status ? region.status.state : "elsewhere";
        ctx.beginPath(); ctx.arc(s.x, s.y, s.r, 0, Math.PI * 2);
        if (ctx.setLineDash) ctx.setLineDash(state === "elsewhere" ? [3, 3] : []);
        ctx.lineWidth = region.id === this.hover ? 3 : 1.5;
        ctx.strokeStyle = state === "data" ? accent : dim;
        if (state === "data") { ctx.globalAlpha = region.status.stale ? 0.4 : 0.85; ctx.fillStyle = accent; ctx.fill(); ctx.globalAlpha = 1; }
        ctx.stroke();
        if (ctx.setLineDash) ctx.setLineDash([]);
      }
      // Each phase branch is one spine in its colour: centre -> tasks in order.
      if (this.selectedCenter && this.branches) {
        ctx.lineWidth = 1.5; ctx.globalAlpha = 0.6;
        for (const phase of Object.keys(PHASE_AXIS)) {
          let prev = screenOf(frame, this.selectedCenter);
          ctx.strokeStyle = byPhase[phase];
          for (const t of this.branches) {
            if (t.phase !== phase) continue;
            const s = screenOf(frame, t);
            if (prev && s) { ctx.beginPath(); ctx.moveTo(prev.x, prev.y); ctx.lineTo(s.x, s.y); ctx.stroke(); }
            prev = s;
          }
        }
        ctx.globalAlpha = 1;
      }
      const ink = this.color("--ink", "#14181f");
      const labels = [];
      for (const s of order) {
        const phase = s.item.phase || (s.item.task ? "planned" : "past");
        ctx.beginPath(); ctx.arc(s.x, s.y, s.r, 0, Math.PI * 2);
        ctx.fillStyle = byPhase[phase] || byPhase.past;
        ctx.globalAlpha = this.selected && !s.item.task && s.item.project !== this.selected ? 0.55 : 1;
        ctx.fill(); ctx.globalAlpha = 1;
        if (s.item.project === this.selected && !s.item.task) {
          ctx.strokeStyle = ink; ctx.lineWidth = 2; ctx.stroke();
        }
        if (s.item.task !== undefined && (s.item.id === this.hover || s.item.task === this.selectedTask)) {
          ctx.strokeStyle = ink; ctx.lineWidth = 2; ctx.stroke();
        }
        labels.push(s);
      }
      this.drawLabels(labels, this.regionScreens);
      this.dirty = false;
      if (this.host.onFrame) this.host.onFrame(this);
    }
    /* The brain behind everything else, far solid first: the stem, then each
       solid's projected front faces filled once (their union is the true,
       non-convex silhouette), two shading tones from a light above the eye,
       the contour edges where a front face meets a back face, and the surface
       curves that face the eye. Fixed cost per frame: about 1.5k projected
       points, a dozen paths, no per-face fill calls. */
    drawBrain(frame) {
      const ctx = this.ctx, ink = this.color("--dim", "#5c6472"), tint = this.color("--brain", ink);
      const eye = this.camera.eye(), view = sub(eye, this.camera.target), vl = Math.hypot(...view) || 1;
      const light = [view[0] / vl, view[1] / vl + 0.8, view[2] / vl], ll = Math.hypot(...light);
      const facing = (p, n) => dot(n, sub(eye, p)) > 0;
      const stem = this.stem;
      if (stem) {
        const a = project(frame, ...stem.from, stem.r[0]), b = project(frame, ...stem.to, stem.r[1]);
        if (a && b) {
          const dx = b.x - a.x, dy = b.y - a.y, len = Math.hypot(dx, dy) || 1, nx = -dy / len, ny = dx / len;
          ctx.beginPath();
          ctx.moveTo(a.x + nx * a.r, a.y + ny * a.r); ctx.lineTo(b.x + nx * b.r, b.y + ny * b.r);
          ctx.lineTo(b.x - nx * b.r, b.y - ny * b.r); ctx.lineTo(a.x - nx * a.r, a.y - ny * a.r);
          if (ctx.closePath) ctx.closePath();
          ctx.globalAlpha = 0.1; ctx.fillStyle = tint; ctx.fill();
          ctx.beginPath(); // the sides only: the top end is hidden inside the brain
          ctx.moveTo(a.x + nx * a.r, a.y + ny * a.r); ctx.lineTo(b.x + nx * b.r, b.y + ny * b.r);
          ctx.lineTo(b.x - nx * b.r, b.y - ny * b.r); ctx.lineTo(a.x - nx * a.r, a.y - ny * a.r);
          ctx.globalAlpha = 0.5; ctx.strokeStyle = ink; ctx.lineWidth = 1.2; ctx.stroke();
        }
      }
      const shells = (this.shells || []).map((m) => ({ m, s: project(frame, ...m.centre, 0) })).filter((x) => x.s)
        .sort((a, b) => b.s.w - a.s.w).map((x) => x.m);
      for (const mesh of shells) {
        const screen = mesh.points.map((q) => project(frame, q[0], q[1], q[2], 0));
        const front = mesh.faces.map((f) => facing(f.mid, f.n) && f.quad.every((q) => screen[q]));
        const fills = [[], [], []]; // whole silhouette, half shade, deep shade
        mesh.faces.forEach((f, k) => {
          if (!front[k]) return;
          fills[0].push(f);
          const lit = dot(f.n, light) / ll;
          if (lit < 0.55) fills[1].push(f);
          if (lit < 0.2) fills[2].push(f);
        });
        const path = (faces) => {
          ctx.beginPath();
          for (const f of faces) {
            const q = f.quad.map((i) => screen[i]);
            const area = (q[1].x - q[0].x) * (q[2].y - q[0].y) - (q[2].x - q[0].x) * (q[1].y - q[0].y)
              + (q[2].x - q[0].x) * (q[3].y - q[0].y) - (q[3].x - q[0].x) * (q[2].y - q[0].y);
            const ordered = area < 0 ? q.slice().reverse() : q; // one winding, so nonzero fills the union
            ctx.moveTo(ordered[0].x, ordered[0].y);
            for (let i = 1; i < 4; i++) ctx.lineTo(ordered[i].x, ordered[i].y);
            if (ctx.closePath) ctx.closePath();
          }
        };
        [[0.13, tint], [0.06, ink], [0.06, ink]].forEach(([alpha, color], i) => {
          if (!fills[i].length) return;
          path(fills[i]); ctx.globalAlpha = alpha; ctx.fillStyle = color; ctx.fill("nonzero");
        });
        // Contour: an edge between a front and a back face.
        ctx.beginPath();
        for (let i = 0; i < MESH_ACROSS; i++) for (let j = 0; j < MESH_AROUND; j++) {
          const k = i * MESH_AROUND + j, right = i * MESH_AROUND + (j + 1) % MESH_AROUND, down = k + MESH_AROUND;
          const edge = (a, b) => { const p = screen[a], q = screen[b]; if (p && q) { ctx.moveTo(p.x, p.y); ctx.lineTo(q.x, q.y); } };
          const quad = mesh.faces[k].quad;
          if (front[k] !== front[right]) edge(quad[1], quad[2]);
          if (i + 1 < MESH_ACROSS && front[k] !== front[down]) edge(quad[3], quad[2]);
        }
        ctx.globalAlpha = 0.75; ctx.strokeStyle = ink; ctx.lineWidth = 1.6; ctx.stroke();
      }
      // Sulci and folia, only where the surface faces the eye.
      for (const major of [false, true]) {
        ctx.beginPath();
        for (const line of this.outline) {
          if (line.major !== major) continue;
          let prev = null;
          line.points.forEach((q, i) => {
            const s = facing(q, line.normals[i]) ? project(frame, q[0], q[1], q[2], 0) : null;
            if (prev && s) { ctx.moveTo(prev.x, prev.y); ctx.lineTo(s.x, s.y); }
            prev = s;
          });
        }
        ctx.globalAlpha = major ? 0.55 : 0.32; ctx.strokeStyle = ink; ctx.lineWidth = major ? 1.5 : 1.1; ctx.stroke();
      }
      ctx.globalAlpha = 1;
    }
    /* Names only for what someone is looking at; the full lists live in the DOM.
       One hard cap counts every text drawn, branch names included: LABEL_BUDGET,
       or NARROW_LABELS on a narrow panel. Order of claim: the selected project,
       the hovered and the opened task, the branch names, the nearest tasks of the
       selected project, then projects.
       Each label tries a few places around its point (below, above, right, left)
       and takes the first that stays in the panel, crosses no other drawn node
       and no placed label. Priority labels fall back to a place clear of nodes,
       then to below; region names try more places (diagonals, farther out)
       before they are skipped; the rest are skipped. Projection can still stack nodes, so
       this keeps text off circles where it can, not always. */
    drawLabels(drawn, regionScreens) {
      const ctx = this.ctx, frame = this.frame;
      const regions = regionScreens || [];
      // Rectangles of page overlays (hover text, legend) no label may cover.
      const avoid = this.host.avoidRects ? this.host.avoidRects() || [] : [];
      const budget = frame.width < NARROW_WIDTH ? NARROW_LABELS : LABEL_BUDGET;
      const LINE = 15;
      ctx.font = "12px system-ui, sans-serif";
      ctx.textAlign = "center";
      const measure = (text) => (ctx.measureText ? ctx.measureText(text).width : 7 * text.length);
      const inside = (s) => s.x >= 0 && s.y >= 0 && s.x <= frame.width && s.y <= frame.height;
      const wanted = [];
      const seen = new Set();
      const node = (s, strong) => {
        if (!s || seen.has(s) || !inside(s)) return;
        seen.add(s);
        const text = s.item.task !== undefined ? taskLabel(s.item)
          : (s.item.project === "-" ? "без проекта" : String(s.item.project).slice(0, 28));
        const w = measure(text), r = s.r;
        // Candidate places as [centre x, top y], first preferred.
        const spots = [[s.x, s.y + r + 1], [s.x, s.y - r - 1 - LINE], [s.x + r + 4 + w / 2, s.y - LINE / 2], [s.x - r - 4 - w / 2, s.y - LINE / 2]];
        wanted.push({ text, strong, w, own: s, spots,
          color: strong ? this.color("--ink", "#14181f") : this.color("--dim", "#5c6472") });
      };
      node(drawn.find((s) => s.item.id === "project:" + this.selected), true);
      node(drawn.find((s) => s.item.id === this.hover), true);
      if (this.selectedTask !== null) node(drawn.find((s) => s.item.task === this.selectedTask), true);
      for (const tip of this.tips) {
        const s = screenOf(frame, tip);
        if (!s || !inside(s)) continue;
        const text = PHASE_NAMES[tip.phase] + " · " + tip.count, w = measure(text);
        wanted.push({ text, strong: true, w, own: null, spots: [[s.x, s.y - LINE / 2], [s.x, s.y - 4 - LINE], [s.x, s.y + 4]],
          color: this.color("--" + tip.phase, "#5c6472") });
      }
      const regionLabel = (s) => {
        if (!inside(s)) return;
        const st = s.item.status || {};
        const text = s.item.name + (st.state === "data" ? " · " + st.count : st.state === "empty" ? " · 0" : " · не здесь");
        const w = measure(text), r = s.r;
        const far = r + 4 + LINE;
        wanted.push({ text, strong: false, w, own: s, color: st.state === "data" ? this.color("--ink", "#14181f") : this.color("--dim", "#5c6472"),
          spots: [[s.x, s.y + r + 1], [s.x, s.y - r - 1 - LINE], [s.x + r + 4 + w / 2, s.y - LINE / 2], [s.x - r - 4 - w / 2, s.y - LINE / 2],
            [s.x + r + w / 2, s.y + r], [s.x - r - w / 2, s.y + r], [s.x + r + w / 2, s.y - r - LINE], [s.x - r - w / 2, s.y - r - LINE],
            [s.x, s.y + far], [s.x, s.y - far - LINE]] });
      };
      // In the overview the regions are what one reads first; with a project
      // open, its branches and tasks come first and regions take what is left.
      if (this.selected === null) for (const s of regions) regionLabel(s);
      for (let k = drawn.length - 1; k >= 0; k--) if (drawn[k].item.task !== undefined) node(drawn[k], false); // nearest first
      if (this.selected !== null) for (const s of regions) regionLabel(s);
      for (let k = drawn.length - 1; k >= 0; k--) if (drawn[k].item.task === undefined) node(drawn[k], false);
      const placed = [];
      const underOverlay = (box) => avoid.some((b) => b[0] < box[0] + box[2] && box[0] < b[0] + b[2] && b[1] < box[1] + box[3] && box[1] < b[1] + b[3]);
      const boxOf = (spot, w) => [spot[0] - w / 2, spot[1], w, LINE];
      const overlapsLabel = (box) => placed.some((b) => b[0] < box[0] + box[2] && box[0] < b[0] + b[2] && b[1] < box[1] + box[3] && box[1] < b[1] + b[3]);
      const crossesNode = (box, own) => drawn.some((d) => {
        if (d === own) return false;
        const nx = Math.max(box[0], Math.min(d.x, box[0] + box[2])), ny = Math.max(box[1], Math.min(d.y, box[1] + box[3]));
        return Math.hypot(d.x - nx, d.y - ny) < d.r + 1;
      });
      const inPanel = (box) => box[0] >= 0 && box[1] >= 0 && box[0] + box[2] <= frame.width && box[1] + box[3] <= frame.height
        && !underOverlay(box);
      for (const label of wanted) {
        if (placed.length >= budget) break;
        const boxes = label.spots.map((spot) => boxOf(spot, label.w));
        let box = boxes.find((b) => inPanel(b) && !crossesNode(b, label.own) && !overlapsLabel(b));
        // A priority label may cross nodes, never the page's own overlay text.
        if (!box && label.strong) box = boxes.find((b) => !crossesNode(b, label.own) && !underOverlay(b)) || boxes.find((b) => !underOverlay(b));
        if (!box) continue;
        placed.push(box);
        ctx.fillStyle = label.color;
        ctx.fillText(label.text, box[0] + box[2] / 2, box[1] + 11);
      }
    }

    /* ----- input ----- */
    interactiveTarget(event) {
      const t = event.target;
      return !!(t && typeof t.closest === "function" && t.closest(INTERACTIVE_SELECTOR));
    }
    bindInput() {
      const wrap = this.host.wrap;
      let drag = null;
      wrap.addEventListener("pointerdown", (e) => {
        if (e.button > 2 || this.interactiveTarget(e)) return;
        if (wrap.setPointerCapture) wrap.setPointerCapture(e.pointerId);
        drag = { x: e.clientX, y: e.clientY, startX: e.clientX, startY: e.clientY, moved: false, pan: e.button === 2 || e.shiftKey };
      });
      wrap.addEventListener("pointermove", (e) => {
        if (drag) {
          const dx = e.clientX - drag.x, dy = e.clientY - drag.y;
          if (!drag.moved && Math.abs(dx) + Math.abs(dy) < 4) return;
          drag.moved = true; drag.x = e.clientX; drag.y = e.clientY;
          if (drag.pan) this.pan(dx, dy); else this.orbit(dx * 0.006, dy * 0.006);
          return;
        }
        if (this.interactiveTarget(e)) {
          if (this.hover !== null) { this.hover = null; this.request(); }
          if (this.host.onHoverText) this.host.onHoverText(null);
          return;
        }
        const n = this.pick(e.clientX, e.clientY), id = n ? n.id : null;
        if (id !== this.hover) {
          this.hover = id; this.request();
          wrap.style.cursor = n ? "pointer" : "grab";
          if (this.host.onHoverText) this.host.onHoverText(n);
        }
      });
      wrap.addEventListener("pointerup", (e) => {
        if (wrap.hasPointerCapture && wrap.hasPointerCapture(e.pointerId)) wrap.releasePointerCapture(e.pointerId);
        if (this.interactiveTarget(e)) { drag = null; return; }
        const press = drag && !drag.moved ? drag : null; drag = null;
        if (!press) return;
        // What was under the pointer when it went down is what gets opened.
        const n = this.pick(press.startX, press.startY);
        if (!n) return;
        if (n.region !== undefined) {
          if (this.host.onSelect) this.host.onSelect({ kind: "region", id: n.region });
        }
        else if (n.task !== undefined) {
          this.selectedTask = n.task; this.request();
          if (this.host.onSelect) this.host.onSelect({ kind: "task", id: n.task });
        }
        else if (this.host.onSelect) this.host.onSelect({ kind: "project", id: n.project });
      });
      wrap.addEventListener("pointercancel", (e) => {
        if (wrap.hasPointerCapture && wrap.hasPointerCapture(e.pointerId)) wrap.releasePointerCapture(e.pointerId);
        drag = null;
      });
      wrap.addEventListener("wheel", (e) => {
        if (this.interactiveTarget(e)) return;
        e.preventDefault(); this.zoom(e.deltaY < 0 ? 1 / 1.15 : 1.15);
      }, { passive: false });
      wrap.addEventListener("keydown", (e) => {
        if (e.target !== wrap && e.target !== this.host.canvas) return;
        const step = e.shiftKey ? 0.25 : 0.08, handled = {
          ArrowLeft: () => this.orbit(-step, 0),
          ArrowRight: () => this.orbit(step, 0),
          ArrowUp: () => this.orbit(0, -step),
          ArrowDown: () => this.orbit(0, step),
          "+": () => this.zoom(1 / 1.2), "=": () => this.zoom(1 / 1.2), "-": () => this.zoom(1.2),
          f: () => this.fit(true), "а": () => this.fit(true),
          "0": () => this.fit(true, true), Home: () => this.fit(true, true),
          Enter: () => { if (this.selected && this.host.onSelect) this.host.onSelect({ kind: "project", id: this.selected }); },
          Escape: () => { this.selectProject(null); if (this.host.onSelect) this.host.onSelect({ kind: "clear" }); },
        }[e.key];
        if (handled) { e.preventDefault(); handled(); }
      });
    }
  }

  window.BrainMap3D = {
    Map3DView, Camera, project, screenOf, depthOrder, pick,
    REGIONS, regionStatus, brainOutline, brainShells, brainStem, hull2d, brainScale, regionAnchor, onHemisphere,
    hemisphereNorm, profileRadius, BRAIN: { hemisphere: HEMI, cerebellum: CEREBELLUM, stem: STEM, staleMs: STALE_MS,
      mesh: { around: MESH_AROUND, across: MESH_ACROSS } },
    projectCenters, taskBranches, branchTips, taskLabel, hash32, unit, ballOffset, nodeSize,
    INTERACTIVE_SELECTOR, PHASE_AXIS, PHASE_NAMES,
    CAPS: { nodes: MAX_NODES, labels: LABEL_BUDGET, narrowLabels: NARROW_LABELS, taskReserve: TASK_RESERVE, dpr: DPR_CAP },
    LAYOUT: { branch: BRANCH, branchMax: BRANCH_MAX, helix: HELIX },
  };
})();
