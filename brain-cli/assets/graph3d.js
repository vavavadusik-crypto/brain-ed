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
      if (this.mode !== "nodes") { this.clear(); this.mode = "nodes"; }
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
      const center = [0, 1, 2].map((k) => pts.reduce((s, p) => s + p[k], 0) / pts.length);
      const radius = Math.max(60, ...pts.map((p) => Math.hypot(p[0] - center[0], p[1] - center[1], p[2] - center[2])));
      return { center, radius };
    }
    fit(animate) {
      const { center, radius } = this.bounds(), dist = radius / Math.sin(FOV / 2) * 1.08;
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
      for (const canvas of [this.host.glCanvas, this.host.flatCanvas]) {
        canvas.width = Math.max(1, Math.round(rect.width * ratio)); canvas.height = Math.max(1, Math.round(rect.height * ratio));
      }
      this.size = { width: rect.width, height: rect.height, ratio }; this.metrics.pixelRatio = ratio;
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

  window.BrainGraph = { GraphView, project, Camera, hash32, ballOffset, anchorsFor, monthOf, INTERACTIVE_SELECTOR };
})();
