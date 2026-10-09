/* Brain for AI — the bounded graph view.
 *
 * One scene of at most a thousand points and their edges, drawn in two WebGL
 * calls (points, then lines) only when something changed: input, data, size.
 * Nothing moves on its own, so an idle page costs nothing.
 *
 * Every screen position comes from one function, `project`. The WebGL shader,
 * the Canvas 2D fallback, picking and the label layer all agree because they
 * all ask it (the shader repeats the same arithmetic on the GPU).
 *
 * Layout is a pure function of what is stored — group, kind, time — plus, in
 * the "links" mode only, a fixed number of relaxation steps for nodes that
 * have just arrived, with every earlier node pinned. Reloading the same view
 * gives the same picture; expanding a node never moves the rest.
 *
 * The "brain" layout puts every drawn node into one of eight regions of a
 * brain-shaped volume, chosen only by the node's stored `kind` (the eight
 * NodeKind values); a kind outside that list goes to an "unknown" zone on the
 * stem. Labels, titles and text are never read to classify anything. The
 * silhouette and the region names are drawn on a separate transparent 2D
 * overlay, so the WebGL and Canvas renderers stay as they are. Region numbers
 * count the nodes of the drawn selection only, never the whole space.
 */
"use strict";
(function () {
  const KIND_COLOR = {
    code: [0.23, 0.51, 0.96], document: [0.47, 0.53, 0.62], memory: [0.06, 0.62, 0.45],
    session: [0.66, 0.33, 0.97], decision: [0.92, 0.35, 0.05], error: [0.86, 0.15, 0.15],
    concept: [0.03, 0.57, 0.70], entity: [0.79, 0.54, 0.02],
  };
  const KINDS = Object.keys(KIND_COLOR);
  const DERIVED_COLOR = [0.55, 0.60, 0.70];
  const FOV = 50 * Math.PI / 180;
  const LABEL_BUDGET = 10;
  const TIME_HALF = 420;

  /* ---------- deterministic numbers ---------- */
  function hash32(text, salt) {
    let h = (0x811c9dc5 ^ (salt || 0)) >>> 0;
    for (let i = 0; i < text.length; i++) { h ^= text.charCodeAt(i); h = Math.imul(h, 0x01000193) >>> 0; }
    h ^= h >>> 16; h = Math.imul(h, 0x7feb352d) >>> 0; h ^= h >>> 15; h = Math.imul(h, 0x846ca68b) >>> 0; h ^= h >>> 16;
    return h >>> 0;
  }
  const unit = (text, salt) => hash32(text, salt) / 4294967296;
  /* A point inside a ball of radius r, uniform in volume, fixed by the id. */
  function ballOffset(id, r) {
    const theta = 2 * Math.PI * unit(id, 1), cosPhi = 2 * unit(id, 2) - 1, rad = r * Math.cbrt(unit(id, 3));
    const sinPhi = Math.sqrt(1 - cosPhi * cosPhi);
    return [rad * sinPhi * Math.cos(theta), rad * cosPhi, rad * sinPhi * Math.sin(theta)];
  }
  const ballRadius = (count) => 12 + 7 * Math.cbrt(Math.max(1, count));

  /* Cluster anchors on a Fibonacci sphere by rank, or along a line for time. */
  function anchorsFor(clusters, by) {
    const anchors = new Map(), n = clusters.length;
    const shell = 90 + 34 * Math.sqrt(n);
    clusters.forEach((c, i) => {
      let pos;
      if (by === "month") {
        const spacing = Math.min(70, (2 * TIME_HALF) / Math.max(1, n - 1));
        pos = [(i - (n - 1) / 2) * spacing, 0, 0];
      } else if (n === 1) {
        pos = [0, 0, 0];
      } else {
        const y = 1 - 2 * (i + 0.5) / n, r = Math.sqrt(Math.max(0, 1 - y * y)), t = i * 2.399963229728653;
        pos = [shell * r * Math.cos(t), shell * y, shell * r * Math.sin(t)];
      }
      anchors.set(c.key === null ? "\u0000rest" : c.key, { pos, r: ballRadius(c.count), index: i });
    });
    return anchors;
  }

  /* ---------- the brain layout ---------- */
  /* Axes: +x is the front, +y is up, z is across (left hemisphere z<0), so the
     flat 2D view (camera on +z) is a side view of the same brain. Each
     hemisphere is a side profile swept across z: a point is on its surface
     when ((z - side*CZ) / w)^2 + (r / rho(theta))^2 = 1, with r, theta the
     polar radius and angle of (x, y) and w wider on the outer side, which
     leaves a narrow fissure between the halves. A sketch, not anatomy. */
  const TAU = 2 * Math.PI;
  const BRAIN_SHAPE = { cz: 74, w: 80, wInner: 70, h: 100, l: 168 };
  const PROFILE = [[1, 0.05], [0.9, 0.48], [0.62, 0.84], [0.2, 1], [-0.25, 0.96], [-0.66, 0.74], [-0.93, 0.36],
    [-1, -0.04], [-0.86, -0.34], [-0.56, -0.44], [-0.15, -0.56], [0.24, -0.74], [0.5, -0.63], [0.6, -0.42],
    [0.56, -0.27], [0.8, -0.22], [0.96, -0.12]].map(([x, y]) => ({ a: (Math.atan2(y * BRAIN_SHAPE.h, x * BRAIN_SHAPE.l) + TAU) % TAU,
    r: Math.hypot(y * BRAIN_SHAPE.h, x * BRAIN_SHAPE.l) })).sort((p, q) => p.a - q.a);
  const CEREBELLUM = { c: [-112, -70, 0], r: [50, 38, 100] };
  const STEM = [[-40, -58, 0], [-56, -182, 0]];
  /* Eight regions, one per stored kind; the place follows the usual
     association of the word, not neuroanatomy. at: (x / l, y / h) on the
     side profile; side: -1 left, 1 right, 0 split between both halves. */
  const BRAIN_REGIONS = [
    { kind: "decision", name: "решения", side: -1, at: [0.62, 0.35] },
    { kind: "concept", name: "понятия", side: 1, at: [0.62, 0.35] },
    { kind: "memory", name: "память", side: -1, at: [0.24, -0.42] },
    { kind: "session", name: "сессии", side: 1, at: [0.24, -0.42] },
    { kind: "entity", name: "сущности", side: -1, at: [-0.32, 0.55] },
    { kind: "code", name: "код", side: 1, at: [-0.32, 0.55] },
    { kind: "document", name: "документы", side: 0, at: [-0.76, 0.12] },
    { kind: "error", name: "ошибки", cerebellum: true },
  ];
  const UNKNOWN_REGION = { kind: null, name: "тип неизвестен", stem: true };
  const REGION_OF = new Map(BRAIN_REGIONS.map((r) => [r.kind, r]));
  /* Only an exact stored kind picks a region; anything else is unknown. */
  const regionOf = (kind) => (typeof kind === "string" && REGION_OF.get(kind)) || UNKNOWN_REGION;
  const REGION_SPREAD = 34, MESH_AROUND = 36, MESH_ACROSS = 10;
  const brainScaleFor = (n) => Math.max(1, Math.cbrt(Math.max(0, n) / 240));

  function profileRadius(theta) {
    const P = PROFILE, n = P.length;
    let a = ((theta % TAU) + TAU) % TAU, i = n - 1;
    for (let k = 0; k < n; k++) if (P[k].a <= a) i = k;
    if (a < P[0].a) a += TAU;
    const j = (i + 1) % n, a1 = j === 0 ? P[0].a + TAU : P[j].a, t = (a - P[i].a) / (a1 - P[i].a);
    const p0 = P[(i - 1 + n) % n].r, p1 = P[i].r, p2 = P[j].r, p3 = P[(j + 1) % n].r;
    return 0.5 * (2 * p1 + (p2 - p0) * t + (2 * p0 - 5 * p1 + 4 * p2 - p3) * t * t + (3 * p1 - p0 - 3 * p2 + p3) * t * t * t);
  }
  /* 1 on a hemisphere's surface, below 1 inside (scaled world units). */
  function hemisphereNorm(p, scale) {
    const k = scale || 1, x = p[0] / k, y = p[1] / k, side = p[2] < 0 ? -1 : 1, dz = p[2] / k - side * BRAIN_SHAPE.cz;
    return Math.hypot(dz / (dz * side >= 0 ? BRAIN_SHAPE.w : BRAIN_SHAPE.wInner), Math.hypot(x, y) / profileRadius(Math.atan2(y, x)));
  }
  const cerebellumNorm = (p, scale) => Math.hypot(...[0, 1, 2].map((k) => (p[k] / (scale || 1) - CEREBELLUM.c[k]) / CEREBELLUM.r[k]));
  /* A side-profile point lifted onto the outer face of one hemisphere, times k
     from the hemisphere's centre line (k < 1 is inside, k > 1 outside). */
  function liftOuter(side, at, k) {
    const x = at[0] * BRAIN_SHAPE.l, y = at[1] * BRAIN_SHAPE.h;
    const q = Math.min(0.999, Math.hypot(x, y) / profileRadius(Math.atan2(y, x)));
    return [x, y, side * (BRAIN_SHAPE.cz + BRAIN_SHAPE.w * Math.sqrt(1 - q * q) * (k === undefined ? 1 : k))];
  }
  function regionSide(region, id) { return region.side ? region.side : (unit(String(id), 7) < 0.5 ? -1 : 1); }
  /* Unscaled centre of a region (for side 0, of the half the id picks). */
  function regionAnchor(region, side) {
    if (region.cerebellum) return [CEREBELLUM.c[0] - 6, CEREBELLUM.c[1] + 4, 0];
    if (region.stem) return [STEM[0][0] - 8, STEM[0][1] - 60, 0];
    const x = region.at[0] * BRAIN_SHAPE.l, y = region.at[1] * BRAIN_SHAPE.h;
    return [x, y, side * (BRAIN_SHAPE.cz + 0.3 * BRAIN_SHAPE.w)];
  }
  /* Where a node sits: its region's ball, fixed by the id, pulled inward until
     it is inside the solid that region belongs to. Pure function of id, kind
     and scale, so expanding never moves anything already drawn. */
  function brainPosition(node, scale) {
    const region = regionOf(node.kind), side = regionSide(region, node.id), a = regionAnchor(region, side);
    let o = ballOffset(String(node.id), region.stem ? 16 : REGION_SPREAD);
    if (region.stem) o = [o[0] * 0.7, o[1] * 2.6, o[2] * 0.7];
    if (region.cerebellum) o = [o[0] * 0.8, o[1] * 0.6, o[2] * 1.9];
    const inside = (p) => region.stem ? true : region.cerebellum ? cerebellumNorm(p) <= 0.86
      : hemisphereNorm(p) <= 0.9 && Math.sign(p[2]) === side;
    let p = [a[0] + o[0], a[1] + o[1], a[2] + o[2]];
    for (let step = 0; step < 6 && !inside(p); step++) { o = o.map((v) => v * 0.6); p = [a[0] + o[0], a[1] + o[1], a[2] + o[2]]; }
    if (!inside(p)) p = a.slice();
    return p.map((v) => v * scale);
  }
  /* Loaded-sample counts per region: {kind|"unknown": n}. Never a total. */
  function brainCounts(nodes) {
    const counts = Object.fromEntries(BRAIN_REGIONS.map((r) => [r.kind, 0]).concat([["unknown", 0]]));
    for (const n of nodes || []) { const r = regionOf(n.kind); counts[r.kind || "unknown"]++; }
    return counts;
  }
  const sub3 = (a, b) => [a[0] - b[0], a[1] - b[1], a[2] - b[2]];
  const dot3 = (a, b) => a[0] * b[0] + a[1] * b[1] + a[2] * b[2];
  /* Closed meshes (unscaled) of both hemispheres and the cerebellum: points
     and quads with outward normals, for the silhouette. Bounded: 3 meshes of
     (MESH_ACROSS + 1) * MESH_AROUND points. */
  function brainMeshes() {
    const loft = (centre, point) => {
      const points = [];
      for (let i = 0; i <= MESH_ACROSS; i++) {
        const s = -Math.PI / 2 + Math.PI * i / MESH_ACROSS;
        for (let j = 0; j < MESH_AROUND; j++) points.push(point(TAU * j / MESH_AROUND, s));
      }
      const faces = [];
      for (let i = 0; i < MESH_ACROSS; i++) for (let j = 0; j < MESH_AROUND; j++) {
        const jn = (j + 1) % MESH_AROUND;
        const quad = [i * MESH_AROUND + j, i * MESH_AROUND + jn, (i + 1) * MESH_AROUND + jn, (i + 1) * MESH_AROUND + j];
        const [a, b, c, d] = quad.map((q) => points[q]), u = sub3(c, a), v = sub3(d, b);
        let n = [u[1] * v[2] - u[2] * v[1], u[2] * v[0] - u[0] * v[2], u[0] * v[1] - u[1] * v[0]];
        const mid = [0, 1, 2].map((k) => (a[k] + b[k] + c[k] + d[k]) / 4);
        if (dot3(n, sub3(mid, centre)) < 0) n = n.map((x) => -x);
        faces.push({ quad, mid, n });
      }
      return { centre, points, faces };
    };
    const hemi = (side) => loft([0, 0, side * BRAIN_SHAPE.cz], (theta, s) => {
      const rho = profileRadius(theta) * Math.cos(s), w = s >= 0 ? BRAIN_SHAPE.w : BRAIN_SHAPE.wInner;
      return [rho * Math.cos(theta), rho * Math.sin(theta), side * (BRAIN_SHAPE.cz + w * Math.sin(s))];
    });
    const { c, r } = CEREBELLUM;
    return [hemi(-1), hemi(1), loft(c, (theta, s) =>
      [c[0] + r[0] * Math.cos(s) * Math.cos(theta), c[1] + r[1] * Math.cos(s) * Math.sin(theta), c[2] + r[2] * Math.sin(s)])];
  }
  const MESHES = brainMeshes();
  /* Two fissures per hemisphere (lateral, central), lifted onto its outer face. */
  const SULCI = [[[0.57, -0.27], [0.3, -0.17], [0.04, -0.1], [-0.24, 0.02], [-0.42, 0.18]],
    [[0.02, 0.99], [0.09, 0.74], [0, 0.5], [0.08, 0.27], [0.02, 0.02]]];
  const SULCUS_LINES = [-1, 1].flatMap((side) => SULCI.map((at) => {
    const points = [];
    for (let i = 0; i + 1 < at.length; i++) for (let t = 0; t < 4; t++) {
      const f = t / 4; points.push(liftOuter(side, [at[i][0] + (at[i + 1][0] - at[i][0]) * f, at[i][1] + (at[i + 1][1] - at[i][1]) * f]));
    }
    points.push(liftOuter(side, at[at.length - 1]));
    return { side, points };
  }));

  /* ---------- camera and the one projection ---------- */
  function lookAt(eye, target) {
    let zx = eye[0] - target[0], zy = eye[1] - target[1], zz = eye[2] - target[2];
    let len = Math.hypot(zx, zy, zz) || 1; zx /= len; zy /= len; zz /= len;
    let xx = zz, xy = 0, xz = -zx; // up = +y, cross(up, z)
    len = Math.hypot(xx, xy, xz) || 1; xx /= len; xy /= len; xz /= len;
    const yx = zy * xz - zz * xy, yy = zz * xx - zx * xz, yz = zx * xy - zy * xx;
    return [xx, yx, zx, 0, xy, yy, zy, 0, xz, yz, zz, 0,
      -(xx * eye[0] + xy * eye[1] + xz * eye[2]), -(yx * eye[0] + yy * eye[1] + yz * eye[2]),
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
    constructor() { this.yaw = 0.6; this.pitch = 0.35; this.dist = 600; this.target = [0, 0, 0]; this.flat = false; }
    eye() {
      const pitch = this.flat ? 0 : this.pitch, yaw = this.flat ? 0 : this.yaw;
      return [this.target[0] + this.dist * Math.cos(pitch) * Math.sin(yaw),
        this.target[1] + this.dist * Math.sin(pitch),
        this.target[2] + this.dist * Math.cos(pitch) * Math.cos(yaw)];
    }
    /* Everything a frame needs: the matrix and the pixel scale for sizes. */
    frame(width, height) {
      const near = Math.max(1, this.dist / 100), far = this.dist * 6 + 3000;
      const vp = multiply(perspective(width / Math.max(1, height), near, far), lookAt(this.eye(), this.target));
      return { vp, width, height, ref: this.ref || this.dist };
    }
  }

  /* World point -> CSS pixels, clip w, and on-screen radius. Sizes are CSS
     pixels at the distance the view was last fitted from, scaled at most
     2.5x / 0.5x as the camera moves, so a node stays a dot instead of filling
     the screen. The shader computes the same values; keep them in step. */
  function project(frame, x, y, z, size) {
    const m = frame.vp;
    const cx = m[0] * x + m[4] * y + m[8] * z + m[12], cy = m[1] * x + m[5] * y + m[9] * z + m[13];
    const cw = m[3] * x + m[7] * y + m[11] * z + m[15];
    if (cw <= 0.0001) return null;
    return { x: (cx / cw * 0.5 + 0.5) * frame.width, y: (1 - (cy / cw * 0.5 + 0.5)) * frame.height, w: cw,
      r: Math.max(1.5, Math.min(48, size * Math.max(0.5, Math.min(2.5, frame.ref / cw)))) };
  }

  /* ---------- WebGL renderer ---------- */
  const POINT_VS = `attribute vec3 a_pos; attribute vec3 a_color; attribute float a_size; attribute float a_flag;
uniform mat4 u_vp; uniform float u_ref; uniform float u_ratio; uniform vec2 u_range;
varying vec3 v_color; varying float v_flag; varying float v_px;
void main() { vec4 clip = u_vp * vec4(a_pos, 1.0); gl_Position = clip;
  float r = clamp(a_size * clamp(u_ref / max(clip.w, 0.0001), 0.5, 2.5), 1.5, 48.0);
  v_px = clamp(2.0 * r * u_ratio, u_range.x, u_range.y); gl_PointSize = v_px; v_color = a_color; v_flag = a_flag; }`;
  const POINT_FS = `precision mediump float; varying vec3 v_color; varying float v_flag; varying float v_px; uniform vec3 u_ink;
void main() { vec2 c = gl_PointCoord * 2.0 - 1.0; float d = length(c); float aa = 2.0 / max(v_px, 1.0);
  if (d > 1.0) discard;
  vec3 color = v_color;
  if (v_flag > 3.5) { if (d < 0.62) discard; }                   // derived cluster: hollow ring
  else if (v_flag > 0.5 && d > 0.70) { color = v_flag > 1.5 ? u_ink : mix(v_color, u_ink, 0.6); }
  float edge = 1.0 - smoothstep(1.0 - aa, 1.0, d);
  if (edge < 0.05) discard;
  gl_FragColor = vec4(color, 1.0); }`;
  const LINE_VS = `attribute vec3 a_pos; attribute vec4 a_color; uniform mat4 u_vp; varying vec4 v_color;
void main() { gl_Position = u_vp * vec4(a_pos, 1.0); v_color = a_color; }`;
  const LINE_FS = `precision mediump float; varying vec4 v_color; void main() { gl_FragColor = v_color; }`;

  class WebGLRenderer {
    constructor(canvas, onLost) {
      this.kind = "webgl"; this.canvas = canvas; this.onLost = onLost; this.lost = false;
      this.gl = canvas.getContext("webgl", { antialias: true, alpha: false, preserveDrawingBuffer: false,
        powerPreference: "low-power", failIfMajorPerformanceCaveat: false });
      if (!this.gl) throw new Error("WebGL недоступен в этом браузере");
      // Without preventDefault the context is never restored (WebGL 1.0 §5.15.2).
      canvas.addEventListener("webglcontextlost", (e) => { e.preventDefault(); this.lost = true; this.onLost("lost"); });
      canvas.addEventListener("webglcontextrestored", () => { this.lost = false; this.init(); this.onLost("restored"); });
      this.init();
    }
    init() {
      const gl = this.gl;
      const compile = (type, src) => { const s = gl.createShader(type); gl.shaderSource(s, src); gl.compileShader(s);
        if (!gl.getShaderParameter(s, gl.COMPILE_STATUS) && !gl.isContextLost()) throw new Error(gl.getShaderInfoLog(s)); return s; };
      const program = (vs, fs) => { const p = gl.createProgram(); gl.attachShader(p, compile(gl.VERTEX_SHADER, vs));
        gl.attachShader(p, compile(gl.FRAGMENT_SHADER, fs)); gl.linkProgram(p);
        if (!gl.getProgramParameter(p, gl.LINK_STATUS) && !gl.isContextLost()) throw new Error(gl.getProgramInfoLog(p)); return p; };
      this.points = program(POINT_VS, POINT_FS); this.lines = program(LINE_VS, LINE_FS);
      this.buffers = { pos: gl.createBuffer(), color: gl.createBuffer(), size: gl.createBuffer(), flag: gl.createBuffer(),
        lpos: gl.createBuffer(), lcolor: gl.createBuffer() };
      const range = gl.getParameter(gl.ALIASED_POINT_SIZE_RANGE) || [1, 64];
      this.pointRange = [Math.max(1, range[0]), Math.max(1, range[1])];
      this.counts = { points: 0, lines: 0 }; this.uploaded = 0;
    }
    upload(name, data) {
      const gl = this.gl; gl.bindBuffer(gl.ARRAY_BUFFER, this.buffers[name]); gl.bufferData(gl.ARRAY_BUFFER, data, gl.DYNAMIC_DRAW);
      this.uploaded += data.byteLength; return data.byteLength;
    }
    setGeometry(geo) {
      if (this.lost) return 0;
      let bytes = 0;
      if (geo.positions) bytes += this.upload("pos", geo.positions);
      if (geo.colors) bytes += this.upload("color", geo.colors);
      if (geo.sizes) bytes += this.upload("size", geo.sizes);
      if (geo.flags) bytes += this.upload("flag", geo.flags);
      if (geo.linePositions) bytes += this.upload("lpos", geo.linePositions);
      if (geo.lineColors) bytes += this.upload("lcolor", geo.lineColors);
      this.counts = { points: geo.pointCount, lines: geo.lineVertexCount };
      return bytes;
    }
    attribute(program, name, buffer, size) {
      const gl = this.gl, loc = gl.getAttribLocation(program, name); if (loc < 0) return;
      gl.bindBuffer(gl.ARRAY_BUFFER, this.buffers[buffer]); gl.enableVertexAttribArray(loc); this.enabled.push(loc);
      gl.vertexAttribPointer(loc, size, gl.FLOAT, false, 0, 0);
    }
    draw(frame, style, ratio) {
      const gl = this.gl; if (this.lost || gl.isContextLost()) return 0;
      gl.viewport(0, 0, this.canvas.width, this.canvas.height);
      gl.clearColor(style.bg[0], style.bg[1], style.bg[2], 1); gl.clear(gl.COLOR_BUFFER_BIT | gl.DEPTH_BUFFER_BIT);
      let calls = 0;
      this.enabled = [];
      if (this.counts.points) {
        gl.enable(gl.DEPTH_TEST); gl.depthFunc(gl.LEQUAL); gl.depthMask(true); gl.disable(gl.BLEND);
        gl.useProgram(this.points);
        this.attribute(this.points, "a_pos", "pos", 3); this.attribute(this.points, "a_color", "color", 3);
        this.attribute(this.points, "a_size", "size", 1); this.attribute(this.points, "a_flag", "flag", 1);
        gl.uniformMatrix4fv(gl.getUniformLocation(this.points, "u_vp"), false, new Float32Array(frame.vp));
        gl.uniform1f(gl.getUniformLocation(this.points, "u_ref"), frame.ref);
        gl.uniform1f(gl.getUniformLocation(this.points, "u_ratio"), ratio);
        gl.uniform2f(gl.getUniformLocation(this.points, "u_range"), this.pointRange[0], this.pointRange[1]);
        gl.uniform3f(gl.getUniformLocation(this.points, "u_ink"), style.ink[0], style.ink[1], style.ink[2]);
        gl.drawArrays(gl.POINTS, 0, this.counts.points); calls++;
        this.disableAttributes();
      }
      if (this.counts.lines) {
        // Lines pass behind nodes (depth test) but never hide each other.
        gl.depthMask(false); gl.enable(gl.BLEND); gl.blendFunc(gl.SRC_ALPHA, gl.ONE_MINUS_SRC_ALPHA);
        gl.useProgram(this.lines);
        this.attribute(this.lines, "a_pos", "lpos", 3); this.attribute(this.lines, "a_color", "lcolor", 4);
        gl.uniformMatrix4fv(gl.getUniformLocation(this.lines, "u_vp"), false, new Float32Array(frame.vp));
        gl.drawArrays(gl.LINES, 0, this.counts.lines); calls++;
        gl.depthMask(true);
        this.disableAttributes();
      }
      return calls;
    }
    /* Arrays enabled for one program must not stay on for the next. */
    disableAttributes() { for (const loc of this.enabled) this.gl.disableVertexAttribArray(loc); this.enabled = []; }
  }

  /* ---------- Canvas 2D renderer: same projection, no GPU ---------- */
  class CanvasRenderer {
    constructor(canvas) { this.kind = "canvas"; this.canvas = canvas; this.ctx = canvas.getContext("2d"); this.geo = null; this.uploaded = 0; }
    setGeometry(geo) {
      this.geo = Object.assign(this.geo || {}, Object.fromEntries(Object.entries(geo).filter(([, v]) => v !== undefined)));
      return 0;
    }
    draw(frame, style, ratio) {
      const ctx = this.ctx, g = this.geo; if (!ctx) return 0;
      ctx.setTransform(ratio, 0, 0, ratio, 0, 0);
      ctx.fillStyle = rgb(style.bg); ctx.fillRect(0, 0, frame.width, frame.height);
      if (!g) return 1;
      const lp = g.linePositions, lc = g.lineColors;
      ctx.lineWidth = 1;
      for (let i = 0; i + 1 < g.lineVertexCount; i += 2) {
        const a = project(frame, lp[i * 3], lp[i * 3 + 1], lp[i * 3 + 2], 1), b = project(frame, lp[i * 3 + 3], lp[i * 3 + 4], lp[i * 3 + 5], 1);
        if (!a || !b) continue;
        ctx.strokeStyle = `rgba(${lc[i * 4] * 255 | 0},${lc[i * 4 + 1] * 255 | 0},${lc[i * 4 + 2] * 255 | 0},${lc[i * 4 + 3]})`;
        ctx.beginPath(); ctx.moveTo(a.x, a.y); ctx.lineTo(b.x, b.y); ctx.stroke();
      }
      const order = [];
      for (let i = 0; i < g.pointCount; i++) {
        const p = project(frame, g.positions[i * 3], g.positions[i * 3 + 1], g.positions[i * 3 + 2], g.sizes[i]);
        if (p) order.push([p, i]);
      }
      order.sort((a, b) => b[0].w - a[0].w); // far first
      for (const [p, i] of order) {
        const flag = g.flags[i], color = [g.colors[i * 3], g.colors[i * 3 + 1], g.colors[i * 3 + 2]];
        ctx.beginPath(); ctx.arc(p.x, p.y, p.r, 0, 2 * Math.PI);
        if (flag > 3.5) { ctx.lineWidth = Math.max(1.5, p.r * 0.38); ctx.strokeStyle = rgb(color); ctx.stroke(); continue; }
        ctx.fillStyle = rgb(color); ctx.fill();
        if (flag > 0.5) { ctx.lineWidth = 2; ctx.strokeStyle = flag > 1.5 ? rgb(style.ink) : rgb(mix(color, style.ink, 0.6)); ctx.stroke(); }
      }
      return 1;
    }
  }
  const rgb = (c) => `rgb(${Math.round(c[0] * 255)},${Math.round(c[1] * 255)},${Math.round(c[2] * 255)})`;
  const mix = (a, b, t) => [a[0] + (b[0] - a[0]) * t, a[1] + (b[1] - a[1]) * t, a[2] + (b[2] - a[2]) * t];
  function cssColor(text, fallback) {
    const m = /rgba?\(([\d.]+)[ ,]+([\d.]+)[ ,]+([\d.]+)/.exec(text || "");
    return m ? [m[1] / 255, m[2] / 255, m[3] / 255] : fallback;
  }

  /* ---------- the view ---------- */
  const INTERACTIVE_SELECTOR = "button, a, input, select, textarea, summary, details, [contenteditable], .metrics";

  class GraphView {
    /* host: {wrap, glCanvas, flatCanvas, labels, onSelect, onExpand, onCluster, onRenderer, onHoverText} */
    constructor(host) {
      this.host = host; this.camera = new Camera();
      this.nodes = []; this.index = new Map(); this.edges = []; this.relations = [];
      this.mode = "empty"; this.layout = "group"; this.by = "group";
      this.anchors = new Map(); this.timeRange = null; this.clusterDoc = null;
      this.selected = null; this.hover = null; this.expanded = new Set();
      this.ratioCap = 1.5; this.dirty = { geometry: true, colors: true };
      this.metrics = { renderer: null, draws: 0, drawCalls: 0, lastDrawMs: 0, samples: [], uploadedBytes: 0,
        lastUploadBytes: 0, layoutMs: 0, contextLosses: 0, pointSizeMax: null, pixelRatio: 1, frameScheduled: 0 };
      this.reducedMotion = window.matchMedia && window.matchMedia("(prefers-reduced-motion: reduce)").matches;
      this.pending = false; this.animation = null;
      this.chooseRenderer(new URLSearchParams(location.search).get("renderer") === "canvas");
      this.bindInput();
      document.addEventListener("visibilitychange", () => { if (!document.hidden) this.request(); });
      new ResizeObserver(() => this.resize()).observe(host.wrap);
    }

    chooseRenderer(forceCanvas, why) {
      const { glCanvas, flatCanvas } = this.host;
      let reason = why || null;
      if (!forceCanvas) {
        try {
          if (!this.gl) this.gl = new WebGLRenderer(glCanvas, (event) => this.contextEvent(event));
          this.renderer = this.gl;
        } catch (error) { reason = String(error.message || error); this.renderer = null; }
      } else reason = reason || "выбран режим без WebGL (?renderer=canvas)";
      if (!this.renderer || this.renderer.lost) {
        if (!this.flat) this.flat = new CanvasRenderer(flatCanvas);
        this.renderer = this.flat;
      }
      glCanvas.hidden = this.renderer !== this.gl; flatCanvas.hidden = this.renderer === this.gl;
      this.metrics.renderer = this.renderer.kind;
      this.metrics.pointSizeMax = this.gl && this.gl.pointRange ? this.gl.pointRange[1] : null;
      this.dirty.geometry = true; this.dirty.colors = true;
      if (this.host.onRenderer) this.host.onRenderer(this.renderer.kind, reason);
      this.resize();
    }
    contextEvent(event) {
      if (event === "lost") { this.metrics.contextLosses++; this.chooseRenderer(true, "контекст WebGL потерян — например, после сброса видеодрайвера"); }
      else if (event === "restored") { this.chooseRenderer(false); }
      // Draw the replacement now rather than waiting for a frame callback: a
      // page whose GPU just went away must not be left showing nothing.
      this.pending = false; this.render(performance.now());
    }

    /* ----- scene contents ----- */
    clear() {
      this.nodes = []; this.index = new Map(); this.edges = []; this.relations = [];
      this.selected = null; this.hover = null; this.expanded = new Set(); this.mode = "empty"; this.clusterDoc = null;
      this.dirty.geometry = true; this.request();
    }
    setAnchors(clusterDoc) { // the space's clusters fix where every group sits
      this.anchorDoc = clusterDoc; this.by = clusterDoc.by; this.anchors = anchorsFor(clusterDoc.clusters, clusterDoc.by);
      const times = clusterDoc.clusters.flatMap((c) => [c.first_created_at, c.last_created_at]).filter((t) => Number.isFinite(t));
      this.timeRange = times.length ? [Math.min(...times), Math.max(...times)] : null;
    }
    showClusters(doc) {
      this.clear(); this.mode = "clusters"; this.clusterDoc = doc; this.by = doc.by;
      this.anchors = anchorsFor(doc.clusters, doc.by);
      const maxCount = Math.max(1, ...doc.clusters.map((c) => c.count));
      doc.clusters.forEach((c, i) => {
        const a = this.anchors.get(c.key === null ? "\u0000rest" : c.key);
        const dominant = Object.entries(c.kinds || {}).sort((x, y) => y[1] - x[1])[0];
        this.addNode({ id: "\u0000cluster:" + i, cluster: c, label: c.key === null ? `прочие ${c.rest_clusters} групп` : c.key,
          kind: doc.by === "kind" ? c.key : (dominant ? dominant[0] : ""), links: c.internal_links, count: c.count,
          pos: a.pos.slice(), size: 5 + 19 * Math.sqrt(c.count / maxCount) });
      });
      // Every pair drawn is a hairball; each group keeps its three strongest
      // partners, which is what the overview is for. The note says how many.
      const keep = new Set(), perCluster = new Map();
      doc.cluster_links.forEach(([a, b], k) => {
        for (const c of [a, b]) { const n = perCluster.get(c) || 0; if (n < 3) { perCluster.set(c, n + 1); keep.add(k); } }
      });
      this.edges = doc.cluster_links.filter((_, k) => keep.has(k)).map(([a, b, count]) => ({ a, b, relation: -1, weight: count }));
      this.fit(false); this.dirty.geometry = true; this.request();
    }
    addNode(node) {
      if (this.index.has(node.id)) return this.nodes[this.index.get(node.id)];
      this.index.set(node.id, this.nodes.length); this.nodes.push(node); return node;
    }
    /* Positions for nodes that have none, by the current layout. */
    place(nodes, parentOf) {
      for (const n of nodes) {
        const parent = parentOf && parentOf.get(n.id);
        if (this.layout === "time") n.pos = this.timePosition(n);
        else if (this.layout === "brain") n.pos = brainPosition(n, this.brainScale || 1);
        else if (this.layout === "links" && parent && parent.pos) {
          const o = ballOffset(n.id, 34); n.pos = [parent.pos[0] + o[0], parent.pos[1] + o[1], parent.pos[2] + o[2]];
        } else n.pos = this.groupPosition(n);
        n.home = n.pos.slice();
      }
    }
    /* Settling for freshly placed nodes: time positions are exact and never
       move; the other layouts relax a fixed number of steps, old nodes pinned. */
    settle(fresh, steps) {
      if (this.layout === "links") this.relax(fresh, steps, 0);
      else if (this.layout === "group") this.relax(fresh, steps, 0.04);
    }
    groupPosition(n) {
      const key = this.by === "kind" ? n.kind : this.by === "month" ? monthOf(n.created_at) : n.group;
      const a = this.anchors.get(key) || this.anchors.get("\u0000rest");
      if (!a) { const o = ballOffset(n.id, 260); return o; }
      const o = ballOffset(n.id, a.r);
      return [a.pos[0] + o[0], a.pos[1] + o[1], a.pos[2] + o[2]];
    }
    timePosition(n) {
      const [t0, t1] = this.timeRange || [n.created_at, n.created_at + 1];
      const x = t1 > t0 ? ((n.created_at - t0) / (t1 - t0)) * 2 * TIME_HALF - TIME_HALF : 0;
      const k = Math.max(0, KINDS.indexOf(n.kind)), angle = (k / KINDS.length) * 2 * Math.PI;
      const o = ballOffset(n.id, 14);
      return [x + o[0] * 0.3, 95 * Math.cos(angle) + o[1], 95 * Math.sin(angle) + o[2]];
    }
    /* A fixed number of steps over new nodes only; old ones are pinned.
       Springs along edges, repulsion through a uniform grid: O(steps*(N+E)). */
    relax(fresh, steps, homePull) {
      if (!fresh.length) return;
      const moving = new Set(fresh), cell = 40, rest = 32;
      const adjacency = this.edges.filter((e) => moving.has(e.a) || moving.has(e.b));
      for (let s = 0; s < steps; s++) {
        const cool = 1 - s / steps, grid = new Map(), force = new Map(fresh.map((i) => [i, [0, 0, 0]]));
        this.nodes.forEach((n, i) => { const k = cellKey(n.pos, cell); if (!grid.has(k)) grid.set(k, []); grid.get(k).push(i); });
        for (const i of fresh) {
          const p = this.nodes[i].pos, f = force.get(i), [gx, gy, gz] = p.map((v) => Math.floor(v / cell));
          for (let dx = -1; dx <= 1; dx++) for (let dy = -1; dy <= 1; dy++) for (let dz = -1; dz <= 1; dz++) {
            for (const j of grid.get(`${gx + dx},${gy + dy},${gz + dz}`) || []) {
              if (j === i) continue;
              const q = this.nodes[j].pos, vx = p[0] - q[0], vy = p[1] - q[1], vz = p[2] - q[2], d2 = vx * vx + vy * vy + vz * vz + 1;
              if (d2 > cell * cell) continue;
              const push = 260 / d2; f[0] += vx * push; f[1] += vy * push; f[2] += vz * push;
            }
          }
        }
        for (const e of adjacency) {
          const p = this.nodes[e.a].pos, q = this.nodes[e.b].pos, vx = q[0] - p[0], vy = q[1] - p[1], vz = q[2] - p[2];
          const d = Math.hypot(vx, vy, vz) + 0.01, pull = (d - rest) * 0.05 / d;
          if (moving.has(e.a)) { const f = force.get(e.a); f[0] += vx * pull; f[1] += vy * pull; f[2] += vz * pull; }
          if (moving.has(e.b)) { const f = force.get(e.b); f[0] -= vx * pull; f[1] -= vy * pull; f[2] -= vz * pull; }
        }
        for (const i of fresh) {
          const n = this.nodes[i], p = n.pos, f = force.get(i);
          // In the group layout each node is held near its group's place, so
          // groups stay apart and only their insides follow the links.
          if (homePull && n.home) for (let k = 0; k < 3; k++) f[k] += (n.home[k] - p[k]) * homePull;
          for (let k = 0; k < 3; k++) p[k] += Math.max(-12, Math.min(12, f[k])) * cool;
        }
      }
    }
    /* Replace the scene with a drawn selection from /api/explore/graph. */
    showNodes(doc) {
      const started = performance.now();
      this.clear(); this.mode = "nodes";
      for (const row of doc.nodes) this.addNode(Object.assign({}, row, { size: nodeSize(row.links) }));
      this.brainScale = brainScaleFor(this.nodes.length);
      this.relations = doc.relations.slice();
      this.edges = doc.edges.map(([a, b, r]) => ({ a, b, relation: r }));
      this.place(this.nodes, null);
      this.settle(this.nodes.map((_, i) => i), 60);
      this.metrics.layoutMs = performance.now() - started;
      this.fit(false); this.dirty.geometry = true; this.request();
    }
    /* Add a page of neighbours around one node, up to the budget. */
    addNeighbours(center, rows, budget) {
      const started = performance.now();
      if (this.mode !== "nodes") { this.clear(); this.mode = "nodes"; this.brainScale = brainScaleFor(budget); }
      let c = this.index.has(center.id) ? this.nodes[this.index.get(center.id)] : null;
      const fresh = [];
      if (!c) { c = this.addNode(Object.assign({}, center, { size: nodeSize(center.links) })); this.place([c], null); fresh.push(this.index.get(c.id)); }
      const parents = new Map(), added = [];
      let skipped = 0;
      for (const row of rows) {
        if (!this.index.has(row.id)) {
          if (this.nodes.length >= budget) { skipped++; continue; }
          const n = this.addNode({ id: row.id, label: row.label, kind: row.kind, group: row.group, links: row.degree,
            created_at: row.created_at, size: nodeSize(row.degree) });
          parents.set(n.id, c); added.push(n); fresh.push(this.index.get(n.id));
        }
        const relation = this.internRelation(row.relation);
        const a = this.index.get(center.id), b = this.index.get(row.id);
        if (!this.edges.some((e) => (e.a === a && e.b === b) || (e.a === b && e.b === a))) {
          this.edges.push(row.direction === "in" ? { a: b, b: a, relation } : { a, b: b, relation });
        }
      }
      this.place(added, parents);
      this.settle(fresh, 40);
      this.expanded.add(center.id);
      this.metrics.layoutMs = performance.now() - started;
      this.dirty.geometry = true; this.request();
      return { added: added.length, skipped };
    }
    internRelation(name) {
      let i = this.relations.indexOf(name); if (i < 0) { i = this.relations.length; this.relations.push(name); } return i;
    }
    /* Exact edges among the drawn nodes, answered for the ids we sent. */
    applyLinks(sentIds, doc) {
      const map = sentIds.map((id) => this.index.get(id));
      const names = doc.relations;
      const next = [], seen = new Set();
      for (const [a, b, r] of doc.edges) {
        const ia = map[a], ib = map[b]; if (ia === undefined || ib === undefined) continue;
        const key = ia + ">" + ib + ":" + names[r]; if (seen.has(key)) continue; seen.add(key);
        next.push({ a: ia, b: ib, relation: this.internRelation(names[r]) });
      }
      this.edges = next; this.dirty.geometry = true; this.request();
      return next.length;
    }
    /* Ids another space owns cannot stay: drop them and reindex. */
    dropMissing(sentIds, missing) {
      if (!missing.length) return;
      const gone = new Set(missing.map((i) => sentIds[i]));
      const keep = this.nodes.filter((n) => !gone.has(n.id)), remap = new Map();
      keep.forEach((n, i) => remap.set(this.index.get(n.id), i));
      this.edges = this.edges.filter((e) => remap.has(e.a) && remap.has(e.b)).map((e) => ({ a: remap.get(e.a), b: remap.get(e.b), relation: e.relation }));
      this.nodes = keep; this.index = new Map(keep.map((n, i) => [n.id, i]));
      if (gone.has(this.selected)) this.selected = null;
      this.dirty.geometry = true; this.request();
    }
    setLayout(layout) {
      this.layout = layout;
      if (this.mode !== "nodes") return;
      const started = performance.now();
      for (const n of this.nodes) n.pos = null;
      this.brainScale = brainScaleFor(this.nodes.length);
      this.place(this.nodes, null);
      this.settle(this.nodes.map((_, i) => i), 60);
      this.metrics.layoutMs = performance.now() - started;
      this.fit(false); this.dirty.geometry = true; this.request();
    }
    setFlat(flat) { this.camera.flat = flat; this.dirty.geometry = true; this.fit(false); this.request(); }
    setQuality(cap) { this.ratioCap = cap; this.resize(); }
    visibleIds() { return this.mode === "nodes" ? this.nodes.map((n) => n.id) : []; }
    has(id) { return this.mode === "nodes" && this.index.has(id); }
    select(id) { this.selected = id; this.dirty.colors = true; this.request(); }

    /* ----- camera ----- */
    bounds() {
      if (!this.nodes.length) return { center: [0, 0, 0], radius: 200 };
      const pts = this.nodes.map((n) => this.drawnPos(n));
      // The brain layout frames the whole brain, not only the drawn nodes.
      if (this.brainShown()) for (const m of MESHES) for (const q of m.points) pts.push(this.drawnPos({ pos: q.map((v) => v * this.brainScale) }));
      const center = [0, 1, 2].map((k) => pts.reduce((s, p) => s + p[k], 0) / pts.length);
      const radius = Math.max(60, ...pts.map((p) => Math.hypot(p[0] - center[0], p[1] - center[1], p[2] - center[2])));
      return { center, radius };
    }
    fit(animate) {
      // Fit the narrower field of view: FOV is vertical, so a portrait panel
      // (aspect < 1) has a smaller horizontal half-angle and needs more room.
      const { center, radius } = this.bounds(), aspect = this.size ? this.size.width / Math.max(1, this.size.height) : 1;
      const half = Math.atan(Math.tan(FOV / 2) * Math.min(1, aspect)), dist = radius / Math.sin(half) * 1.08;
      this.camera.ref = dist;
      this.moveCamera(center, dist, animate);
    }
    focus(id) {
      const i = this.index.get(id); if (i === undefined) return;
      this.moveCamera(this.drawnPos(this.nodes[i]), Math.min(this.camera.dist, 260), true);
    }
    moveCamera(target, dist, animate) {
      const cam = this.camera;
      if (!animate || this.reducedMotion) { cam.target = target.slice(); cam.dist = dist; this.request(); return; }
      const from = { target: cam.target.slice(), dist: cam.dist }, started = performance.now(), duration = 320;
      this.animation = (now) => {
        const t = Math.min(1, (now - started) / duration), e = t * t * (3 - 2 * t);
        cam.target = from.target.map((v, k) => v + (target[k] - v) * e); cam.dist = from.dist + (dist - from.dist) * e;
        return t < 1;
      };
      this.request();
    }
    drawnPos(n) { return this.camera.flat ? [n.pos[0], n.pos[1], 0] : n.pos; }
    brainShown() { return this.layout === "brain" && this.mode === "nodes" && !!this.brainScale; }

    /* ----- drawing on demand ----- */
    request() {
      if (this.pending) return;
      this.pending = true; this.metrics.frameScheduled++;
      requestAnimationFrame((now) => { this.pending = false; this.render(now); });
    }
    resize() {
      const rect = this.host.wrap.getBoundingClientRect();
      if (!rect.width || !rect.height) return;
      const ratio = Math.min(window.devicePixelRatio || 1, this.ratioCap);
      const changed = !this.size || this.size.width !== rect.width || this.size.height !== rect.height;
      for (const canvas of [this.host.glCanvas, this.host.flatCanvas, this.host.brainCanvas].filter(Boolean)) {
        canvas.width = Math.max(1, Math.round(rect.width * ratio)); canvas.height = Math.max(1, Math.round(rect.height * ratio));
      }
      this.size = { width: rect.width, height: rect.height, ratio }; this.metrics.pixelRatio = ratio;
      // A new panel shape (phone rotation, window resize) refits, so nothing is left off-screen.
      if (changed && this.nodes.length) this.fit(false);
      this.request();
    }
    style() {
      const cs = getComputedStyle(this.host.wrap);
      return { bg: cssColor(cs.backgroundColor, [0.97, 0.97, 0.98]), ink: cssColor(cs.color, [0.1, 0.1, 0.12]) };
    }
    buildGeometry(style) {
      const n = this.nodes.length;
      const positions = new Float32Array(n * 3), colors = new Float32Array(n * 3), sizes = new Float32Array(n), flags = new Float32Array(n);
      const neighbours = this.neighbourSet();
      this.nodes.forEach((node, i) => {
        const p = this.drawnPos(node); positions.set(p, i * 3);
        const base = this.mode === "clusters" && this.by === "group" ? DERIVED_COLOR : (KIND_COLOR[node.kind] || DERIVED_COLOR);
        const dim = neighbours && !neighbours.has(i);
        colors.set(dim ? mix(base, style.bg, 0.72) : base, i * 3);
        sizes[i] = node.size || 6;
        flags[i] = this.mode === "clusters" && this.by === "group" ? 4 : node.id === this.selected ? 2 : (node.id === this.hover || this.expanded.has(node.id)) ? 1 : 0;
      });
      // Only edges whose both ends exist get vertices; a reserved but unwritten
      // pair would be drawn as a line to the origin.
      const edges = this.edges.filter((e) => this.nodes[e.a] && this.nodes[e.b]);
      const linePositions = new Float32Array(edges.length * 6), lineColors = new Float32Array(edges.length * 8);
      const maxWeight = edges.reduce((m, e) => Math.max(m, e.weight || 1), 1);
      const sel = this.selected !== null ? this.index.get(this.selected) : undefined;
      edges.forEach((e, k) => {
        const a = this.nodes[e.a], b = this.nodes[e.b];
        linePositions.set(this.drawnPos(a), k * 6); linePositions.set(this.drawnPos(b), k * 6 + 3);
        const touching = sel !== undefined && (e.a === sel || e.b === sel);
        let alpha = this.mode === "clusters" ? 0.12 + 0.55 * Math.log1p(e.weight) / Math.log1p(maxWeight) : 0.30;
        if (sel !== undefined && !touching) alpha *= 0.35;
        const color = touching ? style.ink : mix(style.ink, style.bg, 0.45);
        for (const off of [0, 4]) lineColors.set([color[0], color[1], color[2], touching ? 0.9 : alpha], k * 8 + off);
      });
      return { positions, colors, sizes, flags, linePositions, lineColors, pointCount: n, lineVertexCount: edges.length * 2 };
    }
    neighbourSet() {
      if (this.selected === null || !this.index.has(this.selected)) return null;
      const s = this.index.get(this.selected), set = new Set([s]);
      for (const e of this.edges) { if (e.a === s) set.add(e.b); if (e.b === s) set.add(e.a); }
      return set;
    }
    render(now) {
      // Hidden tab or hidden panel: nothing to see, so nothing is drawn; the
      // visibility and resize observers ask again when it shows.
      if (document.hidden || !this.size || this.host.wrap.getClientRects().length === 0) return;
      const started = performance.now();
      if (this.animation && !this.animation(now)) this.animation = null;
      const style = this.style(), frame = this.camera.frame(this.size.width, this.size.height);
      if (this.dirty.geometry || this.dirty.colors) {
        const bytes = this.renderer.setGeometry(this.buildGeometry(style));
        this.metrics.lastUploadBytes = bytes; this.metrics.uploadedBytes += bytes;
        this.dirty.geometry = false; this.dirty.colors = false;
      }
      this.frame = frame;
      const calls = this.renderer.draw(frame, style, this.size.ratio);
      this.drawBrain(frame, style);
      this.drawLabels(frame);
      const ms = performance.now() - started;
      const m = this.metrics; m.draws++; m.drawCalls = calls; m.lastDrawMs = ms;
      m.samples.push(ms); if (m.samples.length > 120) m.samples.shift();
      if (this.animation) this.request();
      if (this.host.onFrame) this.host.onFrame(this);
    }
    summary() {
      const s = this.metrics.samples.slice().sort((a, b) => a - b), q = (p) => s.length ? s[Math.min(s.length - 1, Math.floor(p * s.length))] : 0;
      return Object.assign({}, this.metrics, { samples: undefined, p50: q(0.5), p95: q(0.95), nodes: this.nodes.length, edges: this.edges.length });
    }

    /* The brain overlay: a transparent 2D canvas above the renderer. In 3D the
       front faces of each solid are filled once as one path (their union is
       the true silhouette) and the contour is where a front face meets a back
       face; in 2D the side profile is drawn. Region names carry only fixed
       words and loaded-sample counts — no stored text ever reaches it. A
       fixed cost per frame: ~1.2k projected points, a few paths, <= 9 texts. */
    drawBrain(frame, style) {
      const canvas = this.host.brainCanvas, ctx = canvas && canvas.getContext && canvas.getContext("2d");
      if (!ctx) return;
      ctx.setTransform(this.size.ratio, 0, 0, this.size.ratio, 0, 0);
      ctx.clearRect(0, 0, frame.width, frame.height);
      this.regionLabels = [];
      if (!this.brainShown()) return;
      const k = this.brainScale, ink = rgb(style.ink), flat = this.camera.flat;
      const at = (q) => project(frame, ...this.drawnPos({ pos: [q[0] * k, q[1] * k, q[2] * k] }), 0);
      ctx.strokeStyle = ink; ctx.fillStyle = ink;
      if (flat) {
        ctx.beginPath();
        for (let i = 0; i <= 72; i++) {
          const t = TAU * i / 72, r = profileRadius(t), s = at([r * Math.cos(t), r * Math.sin(t), 0]);
          if (s) { if (i) ctx.lineTo(s.x, s.y); else ctx.moveTo(s.x, s.y); }
        }
        ctx.globalAlpha = 0.05; ctx.fill(); ctx.globalAlpha = 0.5; ctx.lineWidth = 1.4; ctx.stroke();
        ctx.beginPath();
        for (let i = 0; i <= 36; i++) {
          const t = TAU * i / 36, s = at([CEREBELLUM.c[0] + CEREBELLUM.r[0] * Math.cos(t), CEREBELLUM.c[1] + CEREBELLUM.r[1] * Math.sin(t), 0]);
          if (s) { if (i) ctx.lineTo(s.x, s.y); else ctx.moveTo(s.x, s.y); }
        }
        ctx.globalAlpha = 0.4; ctx.stroke();
      } else {
        const eye = this.camera.eye();
        for (const mesh of MESHES) {
          const screen = mesh.points.map(at);
          const front = mesh.faces.map((f) => dot3(f.n, sub3(eye, f.mid.map((v) => v * k))) > 0 && f.quad.every((q) => screen[q]));
          ctx.beginPath();
          mesh.faces.forEach((f, i) => {
            if (!front[i]) return;
            let q = f.quad.map((j) => screen[j]);
            const area = (q[2].x - q[0].x) * (q[3].y - q[1].y) - (q[3].x - q[1].x) * (q[2].y - q[0].y);
            if (area < 0) q = q.slice().reverse(); // one winding, so nonzero fills the union
            ctx.moveTo(q[0].x, q[0].y); for (let j = 1; j < 4; j++) ctx.lineTo(q[j].x, q[j].y); ctx.closePath();
          });
          ctx.globalAlpha = 0.05; ctx.fill("nonzero");
          ctx.beginPath();
          for (let i = 0; i < MESH_ACROSS; i++) for (let j = 0; j < MESH_AROUND; j++) {
            const f = i * MESH_AROUND + j, right = i * MESH_AROUND + (j + 1) % MESH_AROUND, down = f + MESH_AROUND, quad = mesh.faces[f].quad;
            const edge = (a, b) => { if (screen[a] && screen[b]) { ctx.moveTo(screen[a].x, screen[a].y); ctx.lineTo(screen[b].x, screen[b].y); } };
            if (front[f] !== front[right]) edge(quad[1], quad[2]);
            if (i + 1 < MESH_ACROSS && front[f] !== front[down]) edge(quad[3], quad[2]);
          }
          ctx.globalAlpha = 0.5; ctx.lineWidth = 1.4; ctx.stroke();
        }
        ctx.beginPath();
        for (const line of SULCUS_LINES) {
          let prev = null;
          for (const q of line.points) {
            // A sulcus point is drawn when its hemisphere's outer face looks at the eye.
            const s = (eye[2] - q[2] * k) * line.side > 0 ? at(q) : null;
            if (prev && s) { ctx.moveTo(prev.x, prev.y); ctx.lineTo(s.x, s.y); }
            prev = s;
          }
        }
        ctx.globalAlpha = 0.35; ctx.lineWidth = 1.2; ctx.stroke();
        const stem = STEM.map(at);
        if (stem[0] && stem[1]) { ctx.beginPath(); ctx.moveTo(stem[0].x, stem[0].y); ctx.lineTo(stem[1].x, stem[1].y); ctx.globalAlpha = 0.25; ctx.lineWidth = 10; ctx.stroke(); }
      }
      // Region names with loaded-sample counts, placed on the surface outside the
      // nodes; a name that would cover another is skipped (the note lists all).
      const counts = brainCounts(this.nodes), placed = [];
      ctx.font = "12px system-ui, sans-serif"; ctx.textBaseline = "middle"; ctx.textAlign = "center";
      const regions = BRAIN_REGIONS.concat(counts.unknown ? [UNKNOWN_REGION] : []);
      for (const region of regions) {
        const sides = region.side === 0 ? [flat ? 1 : (this.camera.eye()[2] < 0 ? -1 : 1)] : [region.side || 0];
        const anchor = region.cerebellum ? [CEREBELLUM.c[0] - 30, CEREBELLUM.c[1] - CEREBELLUM.r[1] - 22, 0]
          : region.stem ? [STEM[1][0], STEM[1][1] - 18, 0]
          : (() => { const p = liftOuter(sides[0], region.at, 1.35); return [p[0], p[1] + (region.at[1] > 0 ? 22 : -22), p[2]]; })();
        const s = at(anchor); if (!s) continue;
        const text = region.name + " · " + counts[region.kind || "unknown"];
        const w = ctx.measureText ? ctx.measureText(text).width + 10 : 7 * text.length + 10;
        // A few places around the anchor; the first that fits the panel and
        // covers no other region name wins.
        const box = [[0, 0], [0, -20], [0, 20], [w * 0.6, 0], [-w * 0.6, 0]].map(([dx, dy]) => [s.x + dx - w / 2, s.y + dy - 9, w, 18])
          .find((b) => b[0] >= 0 && b[1] >= 0 && b[0] + w <= frame.width && b[1] + 18 <= frame.height
            && !placed.some((q) => q[0] < b[0] + b[2] && b[0] < q[0] + q[2] && q[1] < b[1] + b[3] && b[1] < q[1] + q[3]));
        if (!box) continue;
        placed.push(box);
        ctx.globalAlpha = 0.85; ctx.fillStyle = rgb(style.bg); ctx.fillRect(box[0], box[1], w, 18);
        ctx.globalAlpha = counts[region.kind || "unknown"] ? 1 : 0.55; ctx.fillStyle = ink; ctx.fillText(text, box[0] + w / 2, box[1] + 9);
        this.regionLabels.push({ region: region.kind || "unknown", text, box });
      }
      ctx.globalAlpha = 1;
    }
    /* Names only for what someone is looking at; the rest are in the lists. */
    drawLabels(frame) {
      const layer = this.host.labels, wanted = [];
      const push = (i, strong) => { if (i !== undefined && !wanted.some((w) => w.i === i)) wanted.push({ i, strong }); };
      push(this.index.get(this.selected), true); push(this.index.get(this.hover), true);
      for (const id of this.expanded) push(this.index.get(id), false);
      if (this.mode === "clusters") this.nodes.map((n, i) => [n.count, i]).sort((a, b) => b[0] - a[0]).forEach(([, i]) => push(i, false));
      const placed = [], spans = [];
      for (const { i, strong } of wanted) {
        if (spans.length >= (this.mode === "clusters" ? 40 : LABEL_BUDGET)) break;
        const n = this.nodes[i], p = project(frame, ...this.drawnPos(n), n.size || 6); if (!p) continue;
        if (p.x < 0 || p.y < 0 || p.x > frame.width || p.y > frame.height) continue;
        const text = this.mode === "clusters" ? `${n.label} · ${n.count.toLocaleString("ru-RU")}` : n.label || n.id;
        const width = Math.min(260, 7 * Math.min(text.length, 40)), box = [p.x + p.r + 3, p.y - 9, width, 18];
        if (!strong && placed.some((b) => b[0] < box[0] + box[2] && box[0] < b[0] + b[2] && b[1] < box[1] + box[3] && box[1] < b[1] + b[3])) continue;
        placed.push(box); spans.push({ text, x: box[0], y: box[1], strong });
      }
      while (layer.children.length > spans.length) layer.lastChild.remove();
      spans.forEach((s, k) => {
        const span = layer.children[k] || layer.appendChild(document.createElement("span"));
        span.textContent = s.text.length > 60 ? s.text.slice(0, 59) + "…" : s.text;
        span.className = s.strong ? "strong" : "";
        span.style.transform = `translate(${Math.round(s.x)}px, ${Math.round(s.y)}px)`;
      });
    }

    /* ----- input ----- */
    /* Interactive overlays live inside the stage (metrics <details>, notes,
       buttons). Pointer capture or a key handler that fires for their
       descendants retargets the gesture away from them, so a real click on
       "Замеры" never reaches the native <summary> toggle. Every listener
       below ignores events whose target is a control, and keyboard shortcuts
       apply only while the stage itself — nothing nested — owns focus. */
    interactiveTarget(event) {
      const t = event.target;
      return !!(t && typeof t.closest === "function" && t.closest(INTERACTIVE_SELECTOR));
    }
    pick(clientX, clientY) {
      if (!this.frame) return null;
      const rect = this.host.wrap.getBoundingClientRect(), x = clientX - rect.left, y = clientY - rect.top;
      let best = null, bestScore = Infinity;
      this.nodes.forEach((n, i) => {
        const p = project(this.frame, ...this.drawnPos(n), n.size || 6); if (!p) return;
        const d = Math.hypot(p.x - x, p.y - y), reach = Math.max(7, p.r + 3);
        if (d > reach) return;
        const score = d / reach + p.w * 1e-6; // nearer to the cursor, then nearer to the eye
        if (score < bestScore) { bestScore = score; best = i; }
      });
      return best === null ? null : this.nodes[best];
    }
    bindInput() {
      const wrap = this.host.wrap;
      let drag = null;
      wrap.addEventListener("pointerdown", (e) => {
        if (e.button > 2 || this.interactiveTarget(e)) return;
        wrap.setPointerCapture(e.pointerId);
        drag = { x: e.clientX, y: e.clientY, moved: false, pan: e.button === 2 || e.shiftKey || this.camera.flat };
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
          if (this.hover !== null) { this.hover = null; this.dirty.colors = true; this.request(); }
          if (this.host.onHoverText) this.host.onHoverText(null);
          return;
        }
        const n = this.pick(e.clientX, e.clientY), id = n ? n.id : null;
        if (id !== this.hover) {
          this.hover = id; this.dirty.colors = true; this.request();
          wrap.style.cursor = n ? "pointer" : "grab";
          if (this.host.onHoverText) this.host.onHoverText(n);
        }
      });
      wrap.addEventListener("pointerup", (e) => {
        if (wrap.hasPointerCapture && wrap.hasPointerCapture(e.pointerId)) wrap.releasePointerCapture(e.pointerId);
        if (this.interactiveTarget(e)) { drag = null; return; }
        const clicked = drag && !drag.moved; drag = null;
        if (!clicked) return;
        const n = this.pick(e.clientX, e.clientY);
        if (!n) return;
        if (n.cluster) this.host.onCluster(n.cluster); else this.host.onSelect(n.id);
      });
      // Leaving the stage ends the hover, so no tooltip stays over the legend.
      wrap.addEventListener("pointerleave", () => {
        if (drag) return;
        if (this.hover !== null) { this.hover = null; this.dirty.colors = true; this.request(); }
        if (this.host.onHoverText) this.host.onHoverText(null);
      });
      wrap.addEventListener("pointercancel", (e) => {
        if (wrap.hasPointerCapture && wrap.hasPointerCapture(e.pointerId)) wrap.releasePointerCapture(e.pointerId);
        drag = null;
      });
      wrap.addEventListener("dblclick", (e) => {
        if (this.interactiveTarget(e)) return;
        const n = this.pick(e.clientX, e.clientY); if (n && !n.cluster) this.host.onExpand(n.id);
      });
      wrap.addEventListener("contextmenu", (e) => { if (!this.interactiveTarget(e)) e.preventDefault(); });
      wrap.addEventListener("wheel", (e) => {
        if (this.interactiveTarget(e)) return;
        e.preventDefault(); this.zoom(e.deltaY < 0 ? 1 / 1.15 : 1.15);
      }, { passive: false });
      wrap.addEventListener("keydown", (e) => {
        // Shortcuts only when the stage itself owns focus; inside a control
        // (the open metrics panel, a button) keys keep their native meaning —
        // Enter on a focused <summary> toggles it, typing reaches inputs.
        if (e.target !== wrap) return;
        const step = e.shiftKey ? 0.25 : 0.08, handled = {
          ArrowLeft: () => this.camera.flat ? this.pan(40, 0) : this.orbit(-step, 0),
          ArrowRight: () => this.camera.flat ? this.pan(-40, 0) : this.orbit(step, 0),
          ArrowUp: () => this.camera.flat ? this.pan(0, 40) : this.orbit(0, -step),
          ArrowDown: () => this.camera.flat ? this.pan(0, -40) : this.orbit(0, step),
          "+": () => this.zoom(1 / 1.2), "=": () => this.zoom(1 / 1.2), "-": () => this.zoom(1.2),
          f: () => this.fit(true), "а": () => this.fit(true),
          Enter: () => { if (this.selected) this.host.onExpand(this.selected); },
          Escape: () => { this.select(null); this.host.onSelect(null); },
        }[e.key];
        if (handled) { e.preventDefault(); handled(); }
      });
    }
    orbit(dYaw, dPitch) {
      if (this.camera.flat) return;
      this.camera.yaw -= dYaw; this.camera.pitch = Math.max(-1.45, Math.min(1.45, this.camera.pitch + dPitch)); this.request();
    }
    pan(dx, dy) {
      const cam = this.camera, k = cam.dist * 2 * Math.tan(FOV / 2) / Math.max(1, this.size ? this.size.height : 600);
      const yaw = cam.flat ? 0 : cam.yaw, pitch = cam.flat ? 0 : cam.pitch;
      const right = [Math.cos(yaw), 0, -Math.sin(yaw)];
      const up = [-Math.sin(pitch) * Math.sin(yaw), Math.cos(pitch), -Math.sin(pitch) * Math.cos(yaw)];
      for (let i = 0; i < 3; i++) cam.target[i] += (-dx * right[i] + dy * up[i]) * k;
      this.request();
    }
    zoom(factor) { this.camera.dist = Math.max(20, Math.min(8000, this.camera.dist * factor)); this.request(); }
  }

  const nodeSize = (links) => 2.2 + 1.1 * Math.log2(1 + Math.max(0, links || 0));
  const cellKey = (p, cell) => `${Math.floor(p[0] / cell)},${Math.floor(p[1] / cell)},${Math.floor(p[2] / cell)}`;
  function monthOf(ts) { const d = new Date(ts * 1000); return `${d.getUTCFullYear()}-${String(d.getUTCMonth() + 1).padStart(2, "0")}`; }

  window.BrainGraph = { GraphView, project, Camera, hash32, ballOffset, anchorsFor, monthOf, INTERACTIVE_SELECTOR,
    BRAIN_REGIONS, UNKNOWN_REGION, regionOf, brainPosition, brainCounts, brainScaleFor, hemisphereNorm, cerebellumNorm,
    profileRadius, BRAIN_MESHES: MESHES };
})();
