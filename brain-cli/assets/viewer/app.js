const SHADERS = Object.freeze({
  nodeVertex: `attribute float size;
attribute float activity;
attribute float importance;
attribute float phase;
attribute float aActivity;
attribute vec3 agentColor;
varying vec3 vColor;
varying float vActivity;
varying float vImportance;
varying float vLiveActivity;
uniform float uTime;
uniform float uScale;
uniform float uMotion;
void main() {
  float liveAge = max(0.0, (uTime - aActivity) * 0.001);
  float liveActivity = step(0.0, aActivity) * (1.0 - smoothstep(0.0, 0.6, liveAge));
  vColor = mix(color, agentColor, liveActivity);
  vActivity = activity;
  vImportance = importance;
  vLiveActivity = liveActivity;
  float breath = 1.0 + (0.012 + importance * 0.014 + activity * 0.012) * sin(uTime * 0.0014 + phase);
  vec3 animatedPosition = position * breath;
  animatedPosition.x += uMotion * sin(uTime * 0.0008 + phase * 1.7) * (0.65 + activity * 0.9);
  animatedPosition.y += cos(uTime * 0.0007 + phase) * 0.7;
  vec4 mvPosition = modelViewMatrix * vec4(animatedPosition, 1.0);
  float pulse = 1.0 + activity * 0.22 * (0.5 + 0.5 * sin(uTime * 0.003 + position.x * 0.05 + position.y * 0.03)) + liveActivity * 0.85;
  gl_PointSize = size * pulse * (uScale / max(80.0, -mvPosition.z));
  gl_Position = projectionMatrix * mvPosition;
}`,
  nodeFragment: `varying vec3 vColor;
varying float vActivity;
varying float vImportance;
varying float vLiveActivity;
void main() {
  vec2 uv = gl_PointCoord - vec2(0.5);
  float d = length(uv);
  if (d > 0.5) discard;
  float core = smoothstep(0.48, 0.05, d);
  float rim = smoothstep(0.5, 0.22, d) * (0.18 + vImportance * 0.32);
  vec3 coldGlow = vec3(0.72, 0.92, 1.0) * vActivity * 0.32;
  vec3 color = vColor * (0.52 + vImportance * 0.52 + vLiveActivity * 1.2) + coldGlow;
  float alpha = core * (0.36 + vImportance * 0.26 + vLiveActivity * 0.48) + rim * 0.55;
  gl_FragColor = vec4(color, alpha);
}`,
  haloVertex: `attribute float size;
attribute float activity;
attribute float importance;
attribute float phase;
attribute float aActivity;
attribute vec3 agentColor;
varying vec3 vColor;
varying float vActivity;
varying float vImportance;
varying float vLiveActivity;
uniform float uTime;
uniform float uScale;
uniform float uMotion;
void main() {
  float liveAge = max(0.0, (uTime - aActivity) * 0.001);
  float liveActivity = step(0.0, aActivity) * (1.0 - smoothstep(0.0, 0.6, liveAge));
  vColor = mix(color, agentColor, liveActivity);
  vActivity = activity;
  vImportance = importance;
  vLiveActivity = liveActivity;
  float breath = 1.0 + (0.012 + importance * 0.014 + activity * 0.012) * sin(uTime * 0.0014 + phase);
  vec3 animatedPosition = position * breath;
  animatedPosition.x += uMotion * sin(uTime * 0.0008 + phase * 1.7) * (0.65 + activity * 0.9);
  animatedPosition.y += cos(uTime * 0.0007 + phase) * 0.7;
  vec4 mvPosition = modelViewMatrix * vec4(animatedPosition, 1.0);
  float pulse = 1.0 + activity * 0.26 * (0.5 + 0.5 * sin(uTime * 0.0022 + position.z * 0.04)) + liveActivity * 1.3;
  gl_PointSize = size * (1.9 + importance * 1.4) * pulse * (uScale / max(80.0, -mvPosition.z));
  gl_Position = projectionMatrix * mvPosition;
}`,
  haloFragment: `varying vec3 vColor;
varying float vActivity;
varying float vImportance;
varying float vLiveActivity;
void main() {
  vec2 uv = gl_PointCoord - vec2(0.5);
  float d = length(uv);
  if (d > 0.5) discard;
  float halo = pow(1.0 - d * 2.0, 2.4);
  float alpha = halo * (0.024 + vImportance * 0.055 + vActivity * 0.04 + vLiveActivity * 0.28);
  gl_FragColor = vec4(vColor + vec3(0.25, 0.45, 0.55) * vActivity + vColor * vLiveActivity, alpha);
}`,
  pulseVertex: `attribute vec3 targetPosition;
attribute float pulseOffset;
attribute float pulseSpeed;
attribute float pulseStart;
attribute float pulseKind;
attribute vec3 pulseColor;
uniform float uTime;
varying vec3 vPulseColor;
varying float vPulseAlpha;
void main() {
  float liveAge = max(0.0, (uTime - pulseStart) * 0.001);
  float liveAlpha = step(0.0, pulseStart) * (1.0 - smoothstep(0.8, 1.2, liveAge));
  float ambientProgress = mod(pulseOffset + uTime * pulseSpeed, 1.0);
  float progress = mix(ambientProgress, clamp(liveAge / 0.9, 0.0, 1.0), pulseKind);
  vPulseColor = pulseColor;
  vPulseAlpha = mix(0.9, liveAlpha, pulseKind);
  vec3 animatedPosition = mix(position, targetPosition, progress);
  vec4 mvPosition = modelViewMatrix * vec4(animatedPosition, 1.0);
  gl_PointSize = (3.4 + pulseKind * 3.0) * (330.0 / max(80.0, -mvPosition.z));
  gl_Position = projectionMatrix * mvPosition;
}`,
  pulseFragment: `varying vec3 vPulseColor;
varying float vPulseAlpha;
void main() {
  vec2 uv = gl_PointCoord - vec2(0.5);
  float alpha = smoothstep(0.5, 0.05, length(uv));
  if (alpha <= 0.0 || vPulseAlpha <= 0.0) discard;
  gl_FragColor = vec4(vPulseColor, alpha * vPulseAlpha);
}`,
  starsVertex: `attribute float seed;
varying float vTwinkle;
uniform float uTime;
uniform float uSize;
void main() {
  vTwinkle = 0.45 + 0.55 * sin(uTime * 0.0009 + seed * 43.0);
  vec4 mvPosition = modelViewMatrix * vec4(position, 1.0);
  gl_PointSize = uSize * (0.6 + vTwinkle) * (320.0 / max(60.0, -mvPosition.z));
  gl_Position = projectionMatrix * mvPosition;
}`,
  starsFragment: `varying float vTwinkle;
uniform vec3 uHue;
void main() {
  vec2 uv = gl_PointCoord - vec2(0.5);
  float d = length(uv);
  if (d > 0.5) discard;
  float alpha = smoothstep(0.5, 0.0, d) * (0.18 + vTwinkle * 0.5);
  gl_FragColor = vec4(uHue, alpha);
}`,
  nebulaVertex: `varying vec3 vDir;
void main() {
  vDir = normalize(position);
  gl_Position = projectionMatrix * modelViewMatrix * vec4(position, 1.0);
}`,
  nebulaFragment: `precision highp float;
varying vec3 vDir;
uniform float uTime;
float hash(vec3 p) { return fract(sin(dot(p, vec3(17.1, 113.5, 71.7))) * 43758.5453); }
float noise(vec3 p) {
  vec3 i = floor(p);
  vec3 f = fract(p);
  f = f * f * (3.0 - 2.0 * f);
  float n000 = hash(i), n100 = hash(i + vec3(1.0, 0.0, 0.0));
  float n010 = hash(i + vec3(0.0, 1.0, 0.0)), n110 = hash(i + vec3(1.0, 1.0, 0.0));
  float n001 = hash(i + vec3(0.0, 0.0, 1.0)), n101 = hash(i + vec3(1.0, 0.0, 1.0));
  float n011 = hash(i + vec3(0.0, 1.0, 1.0)), n111 = hash(i + vec3(1.0, 1.0, 1.0));
  return mix(mix(mix(n000, n100, f.x), mix(n010, n110, f.x), f.y),
             mix(mix(n001, n101, f.x), mix(n011, n111, f.x), f.y), f.z);
}
float fbm(vec3 p) {
  float value = 0.0;
  float amplitude = 0.5;
  for (int i = 0; i < 5; i++) { value += amplitude * noise(p); p *= 2.03; amplitude *= 0.5; }
  return value;
}
void main() {
  float t = uTime * 0.00002;
  vec3 p = vDir * 2.4 + vec3(t, t * 0.6, -t);
  float n = fbm(p);
  float m = fbm(p * 1.9 + n);
  float density = smoothstep(0.45, 1.0, n * 0.7 + m * 0.45);
  vec3 deep = vec3(0.03, 0.07, 0.16);
  vec3 glow = vec3(0.18, 0.42, 0.72);
  vec3 warm = vec3(0.35, 0.22, 0.5);
  vec3 color = mix(deep, glow, density) + warm * pow(density, 3.0) * 0.5;
  float vignette = smoothstep(-0.9, 0.5, vDir.y);
  float alpha = density * 0.22 * (0.5 + 0.5 * vignette);
  gl_FragColor = vec4(color, alpha);
}`,
  fullscreenVertex: `varying vec2 vUv; void main(){ vUv = uv; gl_Position = vec4(position.xy, 0.0, 1.0); }`,
  blurFragment: `varying vec2 vUv; uniform sampler2D tDiffuse; uniform vec2 uDir; uniform vec2 uTexel;void main(){ vec2 o = uDir * uTexel; vec4 c = texture2D(tDiffuse, vUv) * 0.227027; c += texture2D(tDiffuse, vUv + o * 1.3846) * 0.316216; c += texture2D(tDiffuse, vUv - o * 1.3846) * 0.316216; c += texture2D(tDiffuse, vUv + o * 3.2308) * 0.070270; c += texture2D(tDiffuse, vUv - o * 3.2308) * 0.070270; gl_FragColor = c; }`,
  brightFragment: `varying vec2 vUv; uniform sampler2D tDiffuse; uniform float uThreshold;void main(){ vec4 c = texture2D(tDiffuse, vUv); float l = dot(c.rgb, vec3(0.299, 0.587, 0.114)); float k = smoothstep(uThreshold, uThreshold + 0.35, l); gl_FragColor = vec4(c.rgb * k, c.a); }`,
  presentFragment: `varying vec2 vUv; uniform sampler2D tScene; uniform vec2 uTexel;float luma(vec3 c){ return dot(c, vec3(0.299, 0.587, 0.114)); }void main(){ vec4 scene = texture2D(tScene, vUv); vec3 n = texture2D(tScene, vUv + vec2(0.0, uTexel.y)).rgb; vec3 s = texture2D(tScene, vUv - vec2(0.0, uTexel.y)).rgb; vec3 e = texture2D(tScene, vUv + vec2(uTexel.x, 0.0)).rgb; vec3 w = texture2D(tScene, vUv - vec2(uTexel.x, 0.0)).rgb; float edge = max(max(abs(luma(scene.rgb)-luma(n)), abs(luma(scene.rgb)-luma(s))), max(abs(luma(scene.rgb)-luma(e)), abs(luma(scene.rgb)-luma(w)))); vec3 fxaa = (n + s + e + w + scene.rgb * 2.0) / 6.0; scene.rgb = mix(scene.rgb, fxaa, smoothstep(0.08, 0.3, edge)); gl_FragColor = vec4(scene.rgb * 1.08, scene.a); }`,
  compositeFragment: `varying vec2 vUv; uniform sampler2D tBloom; uniform float uBloom; uniform float uDof; uniform float uAspect;void main(){ vec3 bloom = texture2D(tBloom, vUv).rgb; vec2 d = (vUv - 0.5) * vec2(uAspect, 1.0); float r = length(d); float periphery = smoothstep(0.2, 0.72, r); vec3 glow = bloom * (uBloom + periphery * uDof); gl_FragColor = vec4(glow, 1.0); }`,
});

window.__GRAPHIFY_BRAIN_BUILD = "6fba010a4f040ab670968c7e1da5e1557a8c120f|1786739046|2026-08-14T20:24:07+00:00";
window.__GRAPHIFY_BRAIN_READY = false;
window.addEventListener('error', event => {
  const target = document.getElementById('loading');
  if (target && !window.__GRAPHIFY_BRAIN_READY) {
    target.textContent = `New Generation Brain JS error: ${event.message || 'unknown error'}`;
  }
});
window.addEventListener('unhandledrejection', event => {
  const target = document.getElementById('loading');
  if (target && !window.__GRAPHIFY_BRAIN_READY) {
    const reason = event.reason && (event.reason.message || String(event.reason));
    target.textContent = `New Generation Brain promise error: ${reason || 'unknown rejection'}`;
  }
});
setTimeout(() => {
  const target = document.getElementById('loading');
  if (!window.__GRAPHIFY_BRAIN_READY && target && target.textContent.startsWith('Загружаю')) {
    target.textContent = 'New Generation Brain ещё загружается. Если это не исчезает, проверь WebGL, локальный vendor Three.js или сеть.';
  }
}, 7000);

if (!window.THREE) {
  throw new Error('THREE global was not loaded');
}
function rgbFromHex(value) {
  const hex = String(value || '#ffffff').replace('#', '');
  return [0, 2, 4].map(offset => parseInt(hex.slice(offset, offset + 2), 16) / 255);
}
function inflatePayload(raw) {
  if (raw.format !== 'brain-data-v2') return raw;
  const dictionaries = raw.dictionaries;
  const nodes = raw.nodes.map(row => {
    const type = dictionaries.types[row[2]];
    const color = dictionaries.colors[row[6]];
    const lobe = raw.lobes[row[7]];
    return {
      id: row[0],
      label: row[1],
      type,
      community: row[3],
      source_file: dictionaries.sources[row[4]],
      glyph: row[5],
      color,
      rgb: rgbFromHex(color),
      type_rgb: rgbFromHex(dictionaries.type_colors[row[2]]),
      brain_lobe: lobe.id,
      lobe_name: lobe.name,
      lobe_role: lobe.role,
      lobe_rgb: lobe.rgb,
      importance: row[8],
      visual_size: row[9],
      activity: { score: row[10], ts: row[11] },
      x: row[12],
      y: row[13],
      z: row[14],
      phase: row[15],
      degree: row[16],
      first_seen_ts: row[17],
    };
  });
  const links = raw.links.map(row => ({
    source_index: row[0],
    target_index: row[1],
    relation: dictionaries.relations[row[2]],
    confidence: dictionaries.confidences[row[3]],
  }));
  return { ...raw, nodes, links };
}
const dataResponse = await fetch("brain-data.json?limit=1", { cache: "no-cache" });
if (!dataResponse.ok) {
  throw new Error(`brain-data.json request failed: ${dataResponse.status}`);
}
// A level change loads a fresh bounded scene. The URL retains the overview
// generation, so a changed partition cannot silently open a different cluster.
function clusterScene(template, overview, page = 0) {
  const clusters = overview.clusters.slice(page * 4000, (page + 1) * 4000);
  const positions = new Map();
  const nodes = clusters.map((cluster, index) => {
    positions.set(cluster.id, index);
    const angle = index * 2.399963229728653;
    const z = 1 - 2 * (index + 0.5) / Math.max(1, clusters.length);
    const r = Math.sqrt(1 - z * z) * 145;
    return [String(cluster.id), `${cluster.label} (${cluster.size})`, cluster.lobe, cluster.id, 0,
      'C', cluster.lobe, cluster.lobe, 1, 8 * Math.cbrt(cluster.size), 0.15, 0,
      r * Math.cos(angle), z * 105, r * Math.sin(angle), 0, cluster.size, 0];
  });
  const links = overview.links.filter(link => positions.has(link.source) && positions.has(link.target))
    .map(link => [positions.get(link.source), positions.get(link.target), 2, 0]);
  // The renderer progressively reveals a prefix of links along with nodes.
  links.sort((a, b) => Math.max(a[0], a[1]) - Math.max(b[0], b[1]));
  return {...template, nodes, links, clusterOverview: true,
    stats: {...template.stats, nodes: nodes.length, links: links.length,
      total_nodes: overview.total_nodes, total_edges: overview.total_edges,
      note: `Кластеры ${page * 4000 + 1}–${page * 4000 + nodes.length} из ${overview.clusters.length}; клик или приближение раскрывает кластер`}};
}
function clusterLocation(id, offset = 0) {
  const params = new URLSearchParams();
  params.set('view', 'clusters');
  if (id !== null) {
    params.set('cluster', String(id));
    params.set('generation', clusterOverview.generation);
    if (offset) params.set('offset', String(offset));
  }
  return location.pathname + '?' + params.toString();
}
function expandCluster(id) {
  location.assign(clusterLocation(id));
}
function collapseCluster() {
  location.assign(clusterLocation(null));
}
function zoomCluster(delta, distance, index) {
  if (delta < 0 && distance <= 230 && DATA.clusterOverview && index >= 0) {
    expandCluster(Number(DATA.nodes[index].id));
  } else if (delta > 0 && distance >= 720 && clusterDetail) {
    collapseCluster();
  }
}
let rawData = await dataResponse.json();
const clusterParams = new URLSearchParams(location.search);
let clusterOverview = null;
let clusterDetail = null;
if (rawData.stats.total_nodes > 4000 || clusterParams.get('view') === 'clusters') {
  const response = await fetch('brain-clusters.json', {cache: 'no-cache'});
  if (!response.ok) throw new Error(`Кластеры: HTTP ${response.status}`);
  clusterOverview = await response.json();
  if (clusterOverview.error) throw new Error(clusterOverview.error);
  if (clusterParams.has('cluster')) {
    const params = new URLSearchParams({id: clusterParams.get('cluster'),
      generation: clusterParams.get('generation') || clusterOverview.generation,
      offset: clusterParams.get('offset') || '0'});
    const detailResponse = await fetch('/api/cluster?' + params.toString(), {cache: 'no-cache'});
    if (!detailResponse.ok) throw new Error(`Кластер недоступен (HTTP ${detailResponse.status}). Вернитесь к общему плану.`);
    clusterDetail = await detailResponse.json();
    rawData = clusterDetail.graph;
    rawData.stats.note = `${clusterDetail.label}: узлы ${clusterDetail.offset + 1}–${clusterDetail.offset + clusterDetail.returned} из ${clusterDetail.size}; связи между узлами страницы`;
  } else {
    const page = Math.min(Math.max(0, Number(clusterParams.get('page')) || 0),
      Math.max(0, Math.ceil(clusterOverview.clusters.length / 4000) - 1));
    rawData = clusterScene(rawData, clusterOverview, Math.floor(page));
  }
}
if (!clusterOverview && rawData.stats.total_nodes > rawData.nodes.length) {
  const response = await fetch('brain-data.json', {cache: 'no-cache'});
  if (!response.ok) throw new Error(`Граф: HTTP ${response.status}`);
  rawData = await response.json();
}
window.DATA = inflatePayload(rawData);
const DATA = window.DATA;
window.__GRAPHIFY_BRAIN_DATA = DATA;
window.__BRAIN_CLUSTER_STATE = {overview: clusterOverview, detail: clusterDetail};
const DETAILS_URL = "brain-details.json";
const BACKGROUND_URL = "";
const canvas = document.getElementById('brain');
const loading = document.getElementById('loading');
const labelLayer = document.getElementById('labelLayer');
const hoverCard = document.getElementById('hoverCard');
const communityFilter = document.getElementById('communityFilter');
const lobeFilter = document.getElementById('lobeFilter');
const sortMode = document.getElementById('sortMode');
function renderNoWebGLFallback(error) {
  const topNodes = DATA.nodes
    .slice()
    .sort((a, b) => Number(b.importance || 0) - Number(a.importance || 0))
    .slice(0, 42);
  loading.style.display = 'none';
  document.body.insertAdjacentHTML('beforeend', `
    <main style="position:fixed;inset:0;overflow:auto;padding:118px 22px 32px;color:#e7f6ff;background:radial-gradient(circle at 50% 24%,rgba(103,232,249,.18),transparent 34%),linear-gradient(180deg,#020510,#071426 56%,#030714);">
      <section style="max-width:1180px;margin:0 auto;">
        <div style="border:1px solid rgba(125,211,252,.22);background:rgba(3,8,20,.66);border-radius:12px;padding:18px 20px;margin-bottom:16px;box-shadow:0 24px 80px rgba(0,0,0,.35);">
          <h2 style="margin:0 0 8px;font-size:22px;">New Generation Brain — semantic fallback</h2>
          <p style="margin:0;color:#9db7d4;line-height:1.45;">WebGL недоступен в этом браузере/режиме, поэтому включён статичный fallback. В обычном Chrome/VS Code браузере откроется полноценная 3D-сцена. Причина: ${esc(error && error.message ? error.message : error || 'unknown WebGL error')}</p>
        </div>
        <div style="display:grid;grid-template-columns:repeat(auto-fit,minmax(250px,1fr));gap:12px;margin-bottom:16px;">
          ${DATA.lobes.filter(l => l.count > 0).map(lobe => `
            <article style="border:1px solid rgba(125,211,252,.18);background:rgba(3,10,24,.58);border-radius:10px;padding:12px;">
              <div style="font-weight:800;color:${esc(lobe.color)}">${esc(lobe.name)}</div>
              <div style="color:#9db7d4;font-size:12px;margin-top:4px;">${esc(lobe.role)}</div>
              <div style="margin-top:8px;color:#cfefff;">${esc(lobe.count)} узлов · avg importance ${Math.round(Number(lobe.avg_importance || 0) * 100)}%</div>
            </article>
          `).join('')}
        </div>
        <div style="display:grid;grid-template-columns:repeat(auto-fit,minmax(220px,1fr));gap:10px;">
          ${topNodes.map(node => `
            <article style="border:1px solid rgba(125,211,252,.16);background:rgba(3,10,24,.52);border-radius:10px;padding:11px;min-height:96px;">
              <div style="display:flex;align-items:center;gap:8px;">
                <span style="display:grid;place-items:center;width:32px;height:32px;border-radius:8px;background:linear-gradient(135deg,${esc(node.color)},#67e8f9);color:#03101c;font-weight:900;font-size:10px;">${esc(node.glyph || 'N')}</span>
                <strong style="font-size:13px;line-height:1.2;">${esc(node.label)}</strong>
              </div>
              <div style="color:#9db7d4;font-size:12px;margin-top:8px;">${esc(node.lobe_name)} · ${esc(node.type)} · community ${esc(node.community)}</div>
              <div style="color:#cfefff;font-size:12px;margin-top:4px;">importance ${Math.round(Number(node.importance || 0) * 100)}% · links ${esc(node.degree || 0)} · activity ${Math.round(Number(node.activity?.score || 0) * 100)}%</div>
            </article>
          `).join('')}
        </div>
      </section>
    </main>
  `);
  window.__GRAPHIFY_BRAIN_READY = true;
}
const nodeById = new Map(DATA.nodes.map((node, index) => [node.id, { node, index }]));
const nodeBySource = new Map();
DATA.nodes.forEach((node, index) => {
  if (node.source_file && !nodeBySource.has(node.source_file)) {
    nodeBySource.set(node.source_file, { node, index });
  }
});
const AGENT_COLORS = Object.freeze({
  'claude-code': '#ff9f43',
  codex: '#67e8f9',
  hermes: '#c084fc',
  kimi: '#fb7185',
  gemini: '#60a5fa',
  'loop-engine': '#a3e635',
  git: '#facc15',
  station: '#e2e8f0'
});
function agentColor(agent) {
  return AGENT_COLORS[agent] || '#e2e8f0';
}
const adjacency = DATA.nodes.map(() => []);
DATA.links.forEach((link) => {
  adjacency[link.source_index].push({ index: link.target_index, relation: link.relation, direction: 'out' });
  adjacency[link.target_index].push({ index: link.source_index, relation: link.relation, direction: 'in' });
});
DATA.nodes.forEach((node, index) => {
  const neighborText = adjacency[index]
    .slice(0, 18)
    .map(item => `${DATA.nodes[item.index].label} ${item.relation}`)
    .join(' ');
  node.extended_search = [
    node.id,
    node.label,
    node.type,
    node.community,
    node.source_file,
    node.lobe_name,
    node.lobe_role,
    neighborText,
  ].filter(value => value !== null && value !== undefined).join(' ').toLowerCase();
  node.neighbor_count = adjacency[index].length;
});
Array.from(new Set(DATA.nodes.map(node => node.community).filter(value => value !== null && value !== undefined)))
  .sort((a, b) => String(a).localeCompare(String(b), undefined, { numeric: true }))
  .forEach((community) => {
    const option = document.createElement('option');
    option.value = String(community);
    option.textContent = `Сообщество ${community}`;
    communityFilter.appendChild(option);
  });
DATA.lobes
  .filter(lobe => lobe.count > 0)
  .sort((a, b) => b.count - a.count)
  .forEach((lobe) => {
    const option = document.createElement('option');
    option.value = lobe.id;
    option.textContent = `${lobe.name} (${lobe.count})`;
    lobeFilter.appendChild(option);
  });

// Keep level controls available in the semantic WebGL fallback too.
const clusterControls = document.getElementById('clusterControls');
if (clusterOverview) {
  clusterControls.hidden = false;
  const back = document.createElement('button');
  back.textContent = 'Весь мозг · кластеры';
  back.onclick = collapseCluster;
  clusterControls.appendChild(back);
  const chooser = document.createElement('select');
  chooser.id = 'clusterChooser';
  const placeholder = document.createElement('option');
  placeholder.textContent = 'Раскрыть кластер…';
  placeholder.value = '';
  chooser.appendChild(placeholder);
  const choices = DATA.clusterOverview
    ? DATA.nodes.map(node => clusterOverview.clusters[Number(node.id)])
    : [clusterOverview.clusters[clusterDetail.id]];
  choices.forEach(cluster => {
    const option = document.createElement('option');
    option.value = String(cluster.id);
    option.textContent = `${cluster.label} (${cluster.size})`;
    chooser.appendChild(option);
  });
  chooser.onchange = () => { if (chooser.value !== '') expandCluster(Number(chooser.value)); };
  clusterControls.appendChild(chooser);
  const pageSize = clusterDetail ? 500 : 4000;
  const total = clusterDetail ? clusterDetail.size : clusterOverview.clusters.length;
  const offset = clusterDetail ? clusterDetail.offset : (Number(clusterParams.get('page')) || 0) * 4000;
  for (const [label, next] of [['←', offset - pageSize], ['→', offset + pageSize]]) {
    const button = document.createElement('button');
    button.textContent = label;
    button.disabled = next < 0 || next >= total;
    button.onclick = () => location.assign(clusterDetail
      ? clusterLocation(clusterDetail.id, next)
      : location.pathname + '?view=clusters&page=' + Math.floor(next / pageSize));
    clusterControls.appendChild(button);
  }
}

let renderer;
try {
  renderer = new THREE.WebGLRenderer({
    canvas,
    antialias: false,
    alpha: true,
    premultipliedAlpha: false,
    powerPreference: 'low-power'
  });
} catch (error) {
  renderNoWebGLFallback(error);
  throw new Error(`WebGL unavailable; semantic fallback rendered: ${error && error.message ? error.message : error}`);
}
renderer.setPixelRatio(Math.min(window.devicePixelRatio || 1, 1.5));
renderer.setSize(window.innerWidth, window.innerHeight);
renderer.setClearColor(0x020510, 0.32);
if ('outputColorSpace' in renderer && THREE.SRGBColorSpace) {
  renderer.outputColorSpace = THREE.SRGBColorSpace;
} else if ('outputEncoding' in renderer && THREE.sRGBEncoding) {
  renderer.outputEncoding = THREE.sRGBEncoding;
}

const scene = new THREE.Scene();
scene.fog = new THREE.FogExp2(0x06111f, 0.0023);
const camera = new THREE.PerspectiveCamera(55, window.innerWidth / window.innerHeight, 0.1, 2000);
camera.position.set(0, 0, 360);

const cinematicGroup = new THREE.Group();
scene.add(cinematicGroup);

function makeMistTexture() {
  const c = document.createElement('canvas');
  c.width = 512;
  c.height = 512;
  const ctx = c.getContext('2d');
  ctx.clearRect(0, 0, c.width, c.height);
  for (let i = 0; i < 42; i++) {
    const x = 80 + Math.random() * 352;
    const y = 80 + Math.random() * 352;
    const r = 42 + Math.random() * 145;
    const g = ctx.createRadialGradient(x, y, 0, x, y, r);
    g.addColorStop(0, `rgba(210, 235, 255, ${0.09 + Math.random() * 0.11})`);
    g.addColorStop(0.42, `rgba(105, 150, 190, ${0.045 + Math.random() * 0.05})`);
    g.addColorStop(1, 'rgba(2, 8, 18, 0)');
    ctx.fillStyle = g;
    ctx.beginPath();
    ctx.arc(x, y, r, 0, Math.PI * 2);
    ctx.fill();
  }
  const texture = new THREE.CanvasTexture(c);
  texture.minFilter = THREE.LinearFilter;
  texture.magFilter = THREE.LinearFilter;
  return texture;
}

const mistGroup = new THREE.Group();
cinematicGroup.add(mistGroup);
const mistTexture = makeMistTexture();
const mistPlanes = [];
for (let i = 0; i < 34; i++) {
  const size = 90 + Math.random() * 230;
  const material = new THREE.MeshBasicMaterial({
    map: mistTexture,
    color: i % 3 === 0 ? 0xb9ddff : 0x8db4d3,
    transparent: true,
    opacity: 0.035 + Math.random() * 0.055,
    depthWrite: false,
    blending: THREE.AdditiveBlending,
    side: THREE.DoubleSide
  });
  const mesh = new THREE.Mesh(new THREE.PlaneGeometry(size, size * (0.46 + Math.random() * 0.5)), material);
  mesh.position.set(
    -420 + Math.random() * 840,
    -165 + Math.random() * 330,
    -500 + Math.random() * 520
  );
  mesh.rotation.set(0, 0, Math.random() * Math.PI);
  mesh.userData = {
    speed: 0.00006 + Math.random() * 0.00016,
    sway: 6 + Math.random() * 18,
    phase: Math.random() * Math.PI * 2
  };
  mistPlanes.push(mesh);
  mistGroup.add(mesh);
}

const portal = new THREE.Mesh(
  new THREE.CircleGeometry(165, 96),
  new THREE.MeshBasicMaterial({
    color: 0x9ad8ff,
    transparent: true,
    opacity: 0.07,
    blending: THREE.AdditiveBlending,
    depthWrite: false
  })
);
portal.position.set(0, 24, -330);
portal.scale.set(1.6, 0.72, 1);
cinematicGroup.add(portal);

if (BACKGROUND_URL) {
  const loader = new THREE.TextureLoader();
  loader.load(BACKGROUND_URL, texture => {
    texture.minFilter = THREE.LinearFilter;
    texture.magFilter = THREE.LinearFilter;
    if ('colorSpace' in texture && THREE.SRGBColorSpace) {
      texture.colorSpace = THREE.SRGBColorSpace;
    } else if ('encoding' in texture && THREE.sRGBEncoding) {
      texture.encoding = THREE.sRGBEncoding;
    }
    const geometry = new THREE.PlaneGeometry(980, 560, 28, 16);
    const pos = geometry.attributes.position;
    for (let i = 0; i < pos.count; i++) {
      const x = pos.getX(i);
      const y = pos.getY(i);
      const ripple = Math.sin(x * 0.018) * Math.cos(y * 0.015) * 10;
      pos.setZ(i, ripple);
    }
    pos.needsUpdate = true;
    geometry.computeVertexNormals();
    const material = new THREE.MeshBasicMaterial({
      map: texture,
      transparent: true,
      opacity: 0.78,
      depthWrite: false
    });
    const backdrop = new THREE.Mesh(geometry, material);
    backdrop.position.set(0, 12, -560);
    backdrop.userData.isBackdrop = true;
    cinematicGroup.add(backdrop);
  });
}

const root = new THREE.Group();
root.rotation.x = -0.08;
scene.add(root);
scene.add(new THREE.AmbientLight(0x88aaff, 0.9));
const glow = new THREE.PointLight(0x67e8f9, 2.6, 700);
glow.position.set(0, 40, 220);
scene.add(glow);

const lobeGroup = new THREE.Group();
root.add(lobeGroup);
const lobeShellGeometry = new THREE.SphereGeometry(1, 46, 26);
const lobeWireGeometry = new THREE.SphereGeometry(1, 30, 16);
// Each lobe is drawn as two mirrored hemispheres around a median fissure so
// the regions read as one coherent brain silhouette rather than scattered blobs.
DATA.lobes.forEach((lobe) => {
  if (!lobe.count) return;
  const color = new THREE.Color(lobe.color);
  const opacity = 0.05 + Math.min(0.04, lobe.avg_importance * 0.07);
  const spread = lobe.radii[0] * 0.4;
  [-1, 1].forEach((side) => {
    const shell = new THREE.Mesh(
      lobeShellGeometry,
      new THREE.MeshBasicMaterial({
        color,
        transparent: true,
        opacity,
        blending: THREE.AdditiveBlending,
        depthWrite: false,
        side: THREE.BackSide
      })
    );
    shell.position.set(lobe.center[0] + side * spread, lobe.center[1], lobe.center[2]);
    shell.scale.set(lobe.radii[0] * 0.62, lobe.radii[1], lobe.radii[2]);
    shell.userData.lobeId = lobe.id;
    shell.userData.baseOpacity = opacity;
    lobeGroup.add(shell);
  });
  const wire = new THREE.Mesh(
    lobeWireGeometry,
    new THREE.MeshBasicMaterial({
      color,
      wireframe: true,
      transparent: true,
      opacity: 0.07,
      blending: THREE.AdditiveBlending,
      depthWrite: false
    })
  );
  wire.position.set(lobe.center[0], lobe.center[1], lobe.center[2]);
  wire.scale.set(lobe.radii[0], lobe.radii[1], lobe.radii[2]);
  wire.userData.lobeId = lobe.id;
  wire.userData.baseOpacity = 0.07;
  lobeGroup.add(wire);
});
// Unifying translucent brain hull: two hemispheres enclosing every lobe with
// a soft median gap, giving the whole graph a recognizable brain shape.
(function buildBrainHull() {
  let x0 = Infinity, x1 = -Infinity, y0 = Infinity, y1 = -Infinity, z0 = Infinity, z1 = -Infinity, any = false;
  DATA.lobes.forEach((lobe) => {
    if (!lobe.count) return;
    any = true;
    x0 = Math.min(x0, lobe.center[0] - lobe.radii[0]); x1 = Math.max(x1, lobe.center[0] + lobe.radii[0]);
    y0 = Math.min(y0, lobe.center[1] - lobe.radii[1]); y1 = Math.max(y1, lobe.center[1] + lobe.radii[1]);
    z0 = Math.min(z0, lobe.center[2] - lobe.radii[2]); z1 = Math.max(z1, lobe.center[2] + lobe.radii[2]);
  });
  if (!any) return;
  const cx = (x0 + x1) / 2, cy = (y0 + y1) / 2, cz = (z0 + z1) / 2;
  const rx = (x1 - x0) / 2 * 1.05, ry = (y1 - y0) / 2 * 1.06, rz = (z1 - z0) / 2 * 1.04;
  const hullGeometry = new THREE.SphereGeometry(1, 56, 32);
  [-1, 1].forEach((side) => {
    const hull = new THREE.Mesh(
      hullGeometry,
      new THREE.MeshBasicMaterial({
        color: 0x8fc7ff,
        transparent: true,
        opacity: 0.03,
        blending: THREE.AdditiveBlending,
        depthWrite: false,
        side: THREE.BackSide
      })
    );
    hull.position.set(cx + side * rx * 0.46, cy, cz);
    hull.scale.set(rx * 0.6, ry, rz);
    hull.userData.lobeId = 'hull';
    hull.userData.baseOpacity = 0.03;
    lobeGroup.add(hull);
  });
})();

const base = new Float32Array(DATA.nodes.length * 3);
const current = new Float32Array(DATA.nodes.length * 3);
const colors = new Float32Array(DATA.nodes.length * 3);
const baseColors = new Float32Array(DATA.nodes.length * 3);
const sizes = new Float32Array(DATA.nodes.length);
const baseSizes = new Float32Array(DATA.nodes.length);
const activities = new Float32Array(DATA.nodes.length);
const importances = new Float32Array(DATA.nodes.length);
const phases = new Float32Array(DATA.nodes.length);
const liveActivities = new Float32Array(DATA.nodes.length);
const liveAgentColors = new Float32Array(DATA.nodes.length * 3);
const historyHeat = new Float32Array(DATA.nodes.length);
let historyHeatEnabled = false;
const visibleSet = new Set(DATA.nodes.map((_, index) => index));
const focusSet = new Set();
const highlightedEdgeIndices = new Set();
let selectedFocusDepth = 0;
DATA.nodes.forEach((node, index) => {
  base[index * 3] = node.x;
  base[index * 3 + 1] = node.y;
  base[index * 3 + 2] = node.z;
  current[index * 3] = node.x;
  current[index * 3 + 1] = node.y;
  current[index * 3 + 2] = node.z;
  const rgb = node.type_rgb || node.rgb;
  colors[index * 3] = rgb[0];
  colors[index * 3 + 1] = rgb[1];
  colors[index * 3 + 2] = rgb[2];
  baseColors[index * 3] = rgb[0];
  baseColors[index * 3 + 1] = rgb[1];
  baseColors[index * 3 + 2] = rgb[2];
  sizes[index] = node.visual_size || 6;
  baseSizes[index] = sizes[index];
  activities[index] = node.activity?.score || 0;
  importances[index] = node.importance || 0.1;
  phases[index] = node.phase || 0;
  liveActivities[index] = -1000000000;
  liveAgentColors[index * 3] = rgb[0];
  liveAgentColors[index * 3 + 1] = rgb[1];
  liveAgentColors[index * 3 + 2] = rgb[2];
});

const nodeGeometry = new THREE.BufferGeometry();
nodeGeometry.setAttribute('position', new THREE.BufferAttribute(current, 3));
nodeGeometry.setAttribute('color', new THREE.BufferAttribute(colors, 3));
nodeGeometry.setAttribute('size', new THREE.BufferAttribute(sizes, 1));
nodeGeometry.setAttribute('activity', new THREE.BufferAttribute(activities, 1));
nodeGeometry.setAttribute('importance', new THREE.BufferAttribute(importances, 1));
nodeGeometry.setAttribute('phase', new THREE.BufferAttribute(phases, 1));
nodeGeometry.setAttribute('aActivity', new THREE.BufferAttribute(liveActivities, 1));
nodeGeometry.setAttribute('agentColor', new THREE.BufferAttribute(liveAgentColors, 3));
const initialNodeCount = Math.min(800, DATA.nodes.length);
let renderedNodeCount = initialNodeCount;
nodeGeometry.setDrawRange(0, renderedNodeCount);
const nodeMaterial = new THREE.ShaderMaterial({
  uniforms: {
    uTime: { value: 0 },
    uScale: { value: 330 },
    uMotion: { value: 1 }
  },
  vertexColors: true,
  transparent: true,
  blending: THREE.AdditiveBlending,
  depthWrite: false,
  vertexShader: SHADERS.nodeVertex,
  fragmentShader: SHADERS.nodeFragment
});
const haloMaterial = new THREE.ShaderMaterial({
  uniforms: {
    uTime: { value: 0 },
    uScale: { value: 330 },
    uMotion: { value: 1 }
  },
  vertexColors: true,
  transparent: true,
  blending: THREE.AdditiveBlending,
  depthWrite: false,
  vertexShader: SHADERS.haloVertex,
  fragmentShader: SHADERS.haloFragment
});
const halos = new THREE.Points(nodeGeometry, haloMaterial);
root.add(halos);
const points = new THREE.Points(nodeGeometry, nodeMaterial);
root.add(points);

const edgePositions = new Float32Array(DATA.links.length * 6);
const edgeColors = new Float32Array(DATA.links.length * 6);
const linkSourceIndices = new Int32Array(DATA.links.length);
const linkTargetIndices = new Int32Array(DATA.links.length);
const nodeEdgeIndices = DATA.nodes.map(() => []);
for (let edgeIndex = 0; edgeIndex < DATA.links.length; edgeIndex++) {
  const link = DATA.links[edgeIndex];
  linkSourceIndices[edgeIndex] = link.source_index;
  linkTargetIndices[edgeIndex] = link.target_index;
  nodeEdgeIndices[link.source_index].push(edgeIndex);
  nodeEdgeIndices[link.target_index].push(edgeIndex);
  const a = DATA.nodes[link.source_index].rgb || [0.5, 0.7, 1];
  const b = DATA.nodes[link.target_index].rgb || [0.5, 0.7, 1];
  const sourceOffset = link.source_index * 3;
  const targetOffset = link.target_index * 3;
  const edgeOffset = edgeIndex * 6;
  for (let i = 0; i < 3; i++) {
    edgePositions[edgeOffset + i] = current[sourceOffset + i];
    edgePositions[edgeOffset + 3 + i] = current[targetOffset + i];
    edgeColors[edgeOffset + i] = a[i] * 0.72;
    edgeColors[edgeOffset + 3 + i] = b[i] * 0.72;
  }
}
const baseEdgeColors = new Float32Array(edgeColors);
const edgeGeometry = new THREE.BufferGeometry();
edgeGeometry.setAttribute('position', new THREE.BufferAttribute(edgePositions, 3));
edgeGeometry.setAttribute('color', new THREE.BufferAttribute(edgeColors, 3));
function linkCountForNodeCount(nodeCount) {
  let count = 0;
  while (
    count < DATA.links.length &&
    Math.max(DATA.links[count].source_index, DATA.links[count].target_index) < nodeCount
  ) {
    count += 1;
  }
  return count;
}
edgeGeometry.setDrawRange(0, linkCountForNodeCount(renderedNodeCount) * 2);
const edgeMaterial = new THREE.LineBasicMaterial({
  vertexColors: true,
  transparent: true,
  opacity: 0.28,
  blending: THREE.AdditiveBlending,
  depthWrite: false
});
const edges = new THREE.LineSegments(edgeGeometry, edgeMaterial);
root.add(edges);

const rankedPulseLinks = [...DATA.links].sort((a, b) => {
  const an = DATA.nodes[a.source_index];
  const at = DATA.nodes[a.target_index];
  const bn = DATA.nodes[b.source_index];
  const bt = DATA.nodes[b.target_index];
  const av = (an.importance || 0) + (at.importance || 0) + (an.activity?.score || 0) + (at.activity?.score || 0);
  const bv = (bn.importance || 0) + (bt.importance || 0) + (bn.activity?.score || 0) + (bt.activity?.score || 0);
  return bv - av;
});
const pulseCount = DATA.links.length ? Math.min(220, DATA.links.length) : 0;
const pulseSources = new Float32Array(pulseCount * 3);
const pulseTargets = new Float32Array(pulseCount * 3);
const pulseOffsets = new Float32Array(pulseCount);
const pulseSpeeds = new Float32Array(pulseCount);
const pulseStarts = new Float32Array(pulseCount);
const pulseKinds = new Float32Array(pulseCount);
const pulseColors = new Float32Array(pulseCount * 3);
const livePulseSlotCount = Math.min(32, pulseCount);
const ambientPulseCount = pulseCount - livePulseSlotCount;
const nodePulseIndices = DATA.nodes.map(() => []);
const pulseMeta = Array.from({ length: pulseCount }, (_, i) => ({
  link: rankedPulseLinks[Math.floor((i * 37) % rankedPulseLinks.length)],
  offset: (i / pulseCount) % 1,
  speed: 0.0018 + (i % 7) * 0.00025
}));
for (let index = 0; index < pulseMeta.length; index++) {
  const pulse = pulseMeta[index];
  const sourceOffset = pulse.link.source_index * 3;
  const targetOffset = pulse.link.target_index * 3;
  nodePulseIndices[pulse.link.source_index].push(index);
  nodePulseIndices[pulse.link.target_index].push(index);
  pulseSources.set(current.subarray(sourceOffset, sourceOffset + 3), index * 3);
  pulseTargets.set(current.subarray(targetOffset, targetOffset + 3), index * 3);
  pulseOffsets[index] = pulse.offset;
  pulseSpeeds[index] = pulse.speed;
  pulseStarts[index] = -1000000000;
  pulseKinds[index] = index >= ambientPulseCount ? 1 : 0;
  pulseColors[index * 3] = 0.4;
  pulseColors[index * 3 + 1] = 0.91;
  pulseColors[index * 3 + 2] = 0.98;
}
const pulseGeometry = new THREE.BufferGeometry();
pulseGeometry.setAttribute('position', new THREE.BufferAttribute(pulseSources, 3));
pulseGeometry.setAttribute('targetPosition', new THREE.BufferAttribute(pulseTargets, 3));
pulseGeometry.setAttribute('pulseOffset', new THREE.BufferAttribute(pulseOffsets, 1));
pulseGeometry.setAttribute('pulseSpeed', new THREE.BufferAttribute(pulseSpeeds, 1));
pulseGeometry.setAttribute('pulseStart', new THREE.BufferAttribute(pulseStarts, 1));
pulseGeometry.setAttribute('pulseKind', new THREE.BufferAttribute(pulseKinds, 1));
pulseGeometry.setAttribute('pulseColor', new THREE.BufferAttribute(pulseColors, 3));
pulseGeometry.setDrawRange(0, 0);
const pulseMaterial = new THREE.ShaderMaterial({
  uniforms: { uTime: { value: 0 } },
  transparent: true,
  depthWrite: false,
  blending: THREE.AdditiveBlending,
  vertexShader: SHADERS.pulseVertex,
  fragmentShader: SHADERS.pulseFragment
});
const pulses = new THREE.Points(pulseGeometry, pulseMaterial);
root.add(pulses);
function syncDraggedNodeConnections(nodeIndex) {
  for (const edgeIndex of nodeEdgeIndices[nodeIndex]) {
    const sourceOffset = linkSourceIndices[edgeIndex] * 3;
    const targetOffset = linkTargetIndices[edgeIndex] * 3;
    const edgeOffset = edgeIndex * 6;
    edgePositions.set(current.subarray(sourceOffset, sourceOffset + 3), edgeOffset);
    edgePositions.set(current.subarray(targetOffset, targetOffset + 3), edgeOffset + 3);
  }
  edgeGeometry.attributes.position.needsUpdate = true;
  for (const pulseIndex of nodePulseIndices[nodeIndex]) {
    const pulse = pulseMeta[pulseIndex];
    const sourceOffset = pulse.link.source_index * 3;
    const targetOffset = pulse.link.target_index * 3;
    pulseSources.set(current.subarray(sourceOffset, sourceOffset + 3), pulseIndex * 3);
    pulseTargets.set(current.subarray(targetOffset, targetOffset + 3), pulseIndex * 3);
  }
  pulseGeometry.attributes.position.needsUpdate = true;
  pulseGeometry.attributes.targetPosition.needsUpdate = true;
}
const lastNodeByAgent = new Map();
let livePulseCursor = 0;
function resolveLiveNode(event) {
  const candidate = String(event.node || event.target || '').replace(/^\.\//, '');
  return nodeById.get(candidate) || nodeBySource.get(candidate) || null;
}
function triggerLiveConnection(sourceIndex, targetIndex, color) {
  if (!livePulseSlotCount || sourceIndex === targetIndex) return;
  const pulseIndex = ambientPulseCount + (livePulseCursor++ % livePulseSlotCount);
  const sourceOffset = sourceIndex * 3;
  const targetOffset = targetIndex * 3;
  pulseSources.set(current.subarray(sourceOffset, sourceOffset + 3), pulseIndex * 3);
  pulseTargets.set(current.subarray(targetOffset, targetOffset + 3), pulseIndex * 3);
  pulseStarts[pulseIndex] = performance.now();
  pulseColors.set(color, pulseIndex * 3);
  pulseGeometry.attributes.position.needsUpdate = true;
  pulseGeometry.attributes.targetPosition.needsUpdate = true;
  pulseGeometry.attributes.pulseStart.needsUpdate = true;
  pulseGeometry.attributes.pulseColor.needsUpdate = true;
  window.__GRAPHIFY_BRAIN_LIVE_PULSE_COUNT =
    (window.__GRAPHIFY_BRAIN_LIVE_PULSE_COUNT || 0) + 1;
}
function heatLiveLobe(lobeId) {
  const token = performance.now();
  lobeGroup.children.forEach(child => {
    if (child.userData?.lobeId !== lobeId || !child.material) return;
    child.userData.liveHeatToken = token;
    child.material.opacity = Math.min(0.2, Number(child.userData.baseOpacity || 0.03) + 0.075);
    window.setTimeout(() => {
      if (child.userData.liveHeatToken !== token) return;
      child.material.opacity = Number(child.userData.baseOpacity || 0.03);
    }, 4200);
  });
}
function applyLiveVisualEvent(event) {
  const resolved = resolveLiveNode(event);
  if (!resolved) return;
  const rgb = rgbFromHex(agentColor(event.agent));
  const previous = lastNodeByAgent.get(event.agent);
  lastNodeByAgent.set(event.agent, { index: resolved.index, action: event.action });
  const fresh = Math.abs(Date.now() - Number(event.ts) * 1000) <= 10000;
  if (!fresh) return;
  nodeGeometry.attributes.aActivity.setX(resolved.index, performance.now());
  nodeGeometry.attributes.agentColor.setXYZ(resolved.index, rgb[0], rgb[1], rgb[2]);
  nodeGeometry.attributes.aActivity.needsUpdate = true;
  nodeGeometry.attributes.agentColor.needsUpdate = true;
  window.__GRAPHIFY_BRAIN_LAST_LIVE_NODE = {
    index: resolved.index,
    id: resolved.node.id,
    agent: event.agent,
    color: agentColor(event.agent)
  };
  heatLiveLobe(resolved.node.brain_lobe);
  if (
    previous &&
    previous.action === 'read' &&
    (event.action === 'edit' || event.action === 'write')
  ) {
    triggerLiveConnection(previous.index, resolved.index, rgb);
  }
  markUserActivity();
}
window.__GRAPHIFY_BRAIN_RENDERED_NODE_COUNT = renderedNodeCount;
function revealProgressiveBatch() {
  if (renderedNodeCount >= DATA.nodes.length) return;
  const remaining = DATA.nodes.length - initialNodeCount;
  const batchSize = Math.max(1, Math.ceil(remaining / 3));
  renderedNodeCount = Math.min(DATA.nodes.length, renderedNodeCount + batchSize);
  nodeGeometry.setDrawRange(0, renderedNodeCount);
  edgeGeometry.setDrawRange(0, linkCountForNodeCount(renderedNodeCount) * 2);
  if (renderedNodeCount === DATA.nodes.length) {
    pulseGeometry.setDrawRange(0, pulseCount);
  }
  window.__GRAPHIFY_BRAIN_RENDERED_NODE_COUNT = renderedNodeCount;
  if (renderedNodeCount < DATA.nodes.length) scheduleProgressiveBatch();
}
function scheduleProgressiveBatch() {
  if ('requestIdleCallback' in window) {
    window.requestIdleCallback(revealProgressiveBatch, { timeout: 600 });
  } else {
    window.setTimeout(revealProgressiveBatch, 80);
  }
}

const highlight = new THREE.Mesh(
  new THREE.SphereGeometry(7.5, 24, 16),
  new THREE.MeshBasicMaterial({ color: 0xffffff, transparent: true, opacity: 0.84, blending: THREE.AdditiveBlending })
);
highlight.visible = false;
root.add(highlight);

// Procedural deep-space environment: two twinkling starfield layers and an
// fbm nebula shell. Custom ShaderMaterials skip fog chunks on purpose so the
// backdrop is not swallowed by scene.fog like a plain PointsMaterial would be.
const envUniforms = { uTime: { value: 0 } };
function makeStarLayer(count, rMin, rMax, baseSize, hue) {
  const geometry = new THREE.BufferGeometry();
  const positions = new Float32Array(count * 3);
  const seeds = new Float32Array(count);
  for (let i = 0; i < count; i++) {
    const r = rMin + Math.random() * (rMax - rMin);
    const theta = Math.random() * Math.PI * 2;
    const phi = Math.acos(2 * Math.random() - 1);
    positions[i * 3] = r * Math.sin(phi) * Math.cos(theta);
    positions[i * 3 + 1] = r * Math.cos(phi);
    positions[i * 3 + 2] = r * Math.sin(phi) * Math.sin(theta);
    seeds[i] = Math.random();
  }
  geometry.setAttribute('position', new THREE.BufferAttribute(positions, 3));
  geometry.setAttribute('seed', new THREE.BufferAttribute(seeds, 1));
  const material = new THREE.ShaderMaterial({
    uniforms: { uTime: envUniforms.uTime, uSize: { value: baseSize }, uHue: { value: new THREE.Color(hue) } },
    transparent: true,
    depthWrite: false,
    blending: THREE.AdditiveBlending,
    vertexShader: SHADERS.starsVertex,
    fragmentShader: SHADERS.starsFragment
  });
  return new THREE.Points(geometry, material);
}
scene.add(makeStarLayer(720, 520, 900, 1.7, 0x9fc4ff));
scene.add(makeStarLayer(420, 360, 620, 1.1, 0xbcd8ff));

const nebula = new THREE.Mesh(
  new THREE.SphereGeometry(1280, 48, 32),
  new THREE.ShaderMaterial({
    uniforms: envUniforms,
    side: THREE.BackSide,
    transparent: true,
    depthWrite: false,
    blending: THREE.AdditiveBlending,
    vertexShader: SHADERS.nebulaVertex,
    fragmentShader: SHADERS.nebulaFragment
  })
);
nebula.userData.isNebula = true;
scene.add(nebula);

const statsElement = document.getElementById('stats');
const statsPrefix =
  `${DATA.stats.total_nodes ?? DATA.stats.nodes} узлов · ${DATA.stats.links} связей · ${DATA.stats.active_nodes} активных · ${DATA.lobes.filter(l => l.count > 0).length} долей · ${DATA.stats.note || ''}`;
statsElement.textContent = statsPrefix;

const inspector = document.getElementById('inspector');
const structure = document.getElementById('structure');
const opsPanel = document.getElementById('opsPanel');
let selectedIndex = -1;
let hoveredIndex = -1;
let showLabels = true;
let showCards = false;
let showLobes = true;
let showOps = false;
let dragMode = false;
let brainWriteToken = '';
let stationStatus = null;
const liveActivityEvents = [];
const liveEventIds = new Set();
let liveEventSource = null;
let opsExpiryTimer = 0;
let stationRefreshTimer = 0;
const LIVE_ACTIONS = new Set(['read', 'edit', 'write', 'run', 'decide', 'remember', 'error']);
function esc(value) {
  return String(value ?? '').replace(/[&<>"']/g, ch => ({'&':'&amp;','<':'&lt;','>':'&gt;','"':'&quot;',"'":'&#39;'}[ch]));
}
function requireBrainWriteToken() {
  if (!brainWriteToken) {
    brainWriteToken = window.prompt('Brain write token (хранится только в памяти этой вкладки):') || '';
  }
  if (!brainWriteToken) throw new Error('Операция отменена: нужен write token');
  return brainWriteToken;
}
async function postBrain(endpoint, payload) {
  const response = await fetch(endpoint, {
    method: 'POST',
    headers: {
      'Authorization': `Bearer ${requireBrainWriteToken()}`,
      'Content-Type': 'application/json',
    },
    body: JSON.stringify(payload),
  });
  const result = await response.json().catch(() => ({ error: `HTTP ${response.status}` }));
  if (response.status === 401) brainWriteToken = '';
  if (!response.ok) throw new Error(result.error || `HTTP ${response.status}`);
  return result;
}
function reportBrainAction(message) {
  window.alert(message);
}
async function rememberFromViewer(text, links = [], memoryType = 'fact', tags = []) {
  return postBrain('/api/remember', {
    text,
    type: memoryType,
    links,
    tags,
    agent: 'viewer',
  });
}
async function addToBrain() {
  const kind = (window.prompt('Тип: repo, document или idea') || '').trim().toLowerCase();
  if (!kind) return;
  if (kind === 'repo') {
    const repo = (window.prompt('GitHub owner/repo:') || '').trim();
    if (!repo) return;
    const branch = (window.prompt('Ветка мозга: github, ai-tools, prompts-workflows, monetization, design-art-ref…', 'github') || '').trim();
    const note = (window.prompt('Зачем этот репозиторий нужен станции:', '') || '').trim();
    const tags = (window.prompt('Теги через запятую:', '') || '').trim();
    const task = await postBrain('/api/queue', {
      action: 'brain-add-repo',
      parameters: { repo, branch, note, tags },
      requested_by: 'viewer',
    });
    reportBrainAction(`Репозиторий поставлен в очередь: ${task.id}. Подтверди: brain queue run ${task.id}`);
    return;
  }
  if (kind === 'document') {
    const path = (window.prompt('Путь к документу внутри station/vault:') || '').trim();
    if (!path) return;
    const note = (window.prompt('Кратко: зачем документ нужен:', '') || '').trim();
    const result = await rememberFromViewer(
      `Document: ${path}${note ? ` — ${note}` : ''}`,
      [path],
      'fact',
      ['document'],
    );
    reportBrainAction(`Документ добавлен в общую память: ${result.id}`);
    return;
  }
  if (kind === 'idea') {
    const idea = (window.prompt('Идея или заметка:') || '').trim();
    if (!idea) return;
    const result = await rememberFromViewer(idea, [], 'fact', ['idea']);
    reportBrainAction(`Идея сохранена: ${result.id}`);
    return;
  }
  throw new Error('Допустимые типы: repo, document, idea');
}
function liveEventIdentity(event) {
  return [event.ts, event.agent, event.action, event.target, event.weight].join('|');
}
function liveOpsStatus() {
  return stationStatus?.ops?.status || 'unknown';
}
function refreshLiveOpsStatus() {
  const status = liveOpsStatus();
  statsElement.textContent = `${statsPrefix} · ops ${status}`;
  window.clearTimeout(opsExpiryTimer);
  const expiresAt = Number(stationStatus?.ops?.expires_at || 0) * 1000;
  if (status === 'active' && expiresAt) {
    const delay = Math.max(100, expiresAt - Date.now() + 50);
    opsExpiryTimer = window.setTimeout(refreshStationStatus, delay);
  }
}
function relativeEventTime(timestamp) {
  const seconds = Math.max(0, Math.round((Date.now() - Number(timestamp) * 1000) / 1000));
  if (seconds < 60) return `${seconds} с назад`;
  if (seconds < 3600) return `${Math.floor(seconds / 60)} мин назад`;
  return `${Math.floor(seconds / 3600)} ч назад`;
}
function handleLiveActivity(event) {
  if (!event || !Number.isFinite(Number(event.ts)) || !LIVE_ACTIONS.has(event.action)) return;
  const identity = liveEventIdentity(event);
  if (liveEventIds.has(identity)) return;
  liveEventIds.add(identity);
  liveActivityEvents.push(event);
  if (liveActivityEvents.length > 20) liveActivityEvents.shift();
  if (liveEventIds.size > 200) {
    liveEventIds.clear();
    liveActivityEvents.forEach(item => liveEventIds.add(liveEventIdentity(item)));
  }
  window.__GRAPHIFY_BRAIN_LIVE_EVENT_COUNT = liveActivityEvents.length;
  applyLiveVisualEvent(event);
  scheduleStationStatusRefresh();
  if (showOps) renderOpsPanel();
}
async function loadLiveSnapshot() {
  try {
    const response = await fetch('/live/snapshot.json', { cache: 'no-cache' });
    if (!response.ok) return;
    const snapshot = await response.json();
    (snapshot.events || []).forEach(handleLiveActivity);
  } catch (_error) {
    // The static viewer remains usable when the activity daemon is stopped.
  }
}
function closeLiveActivity() {
  if (!liveEventSource) return;
  liveEventSource.close();
  liveEventSource = null;
  window.__GRAPHIFY_BRAIN_LIVE_CONNECTED = false;
}
function openLiveActivity() {
  if (document.hidden || liveEventSource || !window.EventSource) return;
  liveEventSource = new EventSource('/live');
  liveEventSource.addEventListener('open', () => {
    window.__GRAPHIFY_BRAIN_LIVE_CONNECTED = true;
  });
  liveEventSource.addEventListener('activity', message => {
    try {
      handleLiveActivity(JSON.parse(message.data));
    } catch (_error) {
      // Ignore malformed external events; never inject them into the DOM.
    }
  });
  liveEventSource.addEventListener('error', () => {
    window.__GRAPHIFY_BRAIN_LIVE_CONNECTED = false;
  });
}
async function refreshStationStatus() {
  try {
    const response = await fetch('/api/station', { cache: 'no-cache' });
    if (!response.ok) throw new Error(`station API ${response.status}`);
    stationStatus = await response.json();
    window.__GRAPHIFY_BRAIN_STATION = stationStatus;
  } catch (_error) {
    stationStatus = null;
  }
  refreshLiveOpsStatus();
  if (showOps) renderOpsPanel();
}
function scheduleStationStatusRefresh(delay = 100) {
  window.clearTimeout(stationRefreshTimer);
  stationRefreshTimer = window.setTimeout(refreshStationStatus, delay);
}
function renderOpsPanel() {
  const ops = DATA.ops || {};
  const states = ops.states || {};
  const reports = ops.reports || {};
  const station = stationStatus || {};
  const loops = station.loop_engine || {};
  const providers = station.providers || {};
  const resources = station.resources || {};
  const queue = station.queue || {};
  const events = liveActivityEvents.slice().reverse();
  const onlineAgents = [...new Set(
    liveActivityEvents
      .filter(event => Date.now() - Number(event.ts) * 1000 < 10 * 60 * 1000)
      .map(event => event.agent)
  )];
  const generatedReportItems = Object.entries(reports)
    .filter(([, report]) => report && report.path)
    .map(([key, report]) => `<li><a href="${esc(report.url)}">${esc(key)}</a><div class="ops-line">${esc(report.path)} · ${esc(report.mtime || '')}</div></li>`)
    .join('');
  const liveReportItems = (loops.latest_reports || [])
    .map(report => `<li><b>${esc(report.name)}</b><div class="ops-line">${esc(relativeEventTime(report.mtime))}</div></li>`)
    .join('');
  const reportItems = liveReportItems || generatedReportItems
    || '<li><div class="ops-line">Отчётов пока нет.</div></li>';
  const eventItems = events.map(event => `
    <li>
      <div class="ops-event"><b style="color:${esc(agentColor(event.agent))}">${esc(event.agent || 'agent')}</b> · ${esc(event.action || 'event')}</div>
      <div class="ops-line">${esc(event.target || 'runtime')} · ${esc(relativeEventTime(event.ts))} · weight ${esc(event.weight || 1)}</div>
    </li>
  `).join('') || '<li><div class="ops-line">Событий пока нет.</div></li>';
  const agentLine = onlineAgents.length ? onlineAgents.join(', ') : 'none';
  const palette = ['claude-code', 'codex', 'hermes', 'kimi', 'gemini', 'loop-engine']
    .map(agent => `<span class="agent-key"><span class="agent-dot" style="color:${agentColor(agent)};background:${agentColor(agent)}"></span>${esc(agent)}</span>`)
    .join('');
  const status = liveOpsStatus();
  const temperature = resources.temperature?.max_celsius;
  const boxItems = (loops.boxes || [])
    .map(box => `<span class="chip">${box.enabled ? '●' : '○'} ${esc(box.id)}</span>`)
    .join('') || '<span class="ops-line">Loop Engine status unavailable</span>';
  const queueItems = (queue.recent || []).slice().reverse().map(task => `
    <li>
      <div class="ops-event"><b>${esc(task.action || 'action')}</b> · ${esc(task.status || 'unknown')}</div>
      <div class="ops-line">${esc(task.id)}</div>
    </li>
  `).join('') || '<li><div class="ops-line">Очередь пуста.</div></li>';
  const actionOptions = (queue.actions || [])
    .filter(action => action.id !== 'brain-add-repo')
    .map(action => `<option value="${esc(action.id)}">${esc(action.label)}</option>`)
    .join('');
  opsPanel.innerHTML = `
    <div class="ops-head">
      <div>
        <div class="ops-title">Live Ops Pulse</div>
        <div class="ops-line">SSE activity stream · paths and action types only</div>
      </div>
      <span class="ops-status ${status}">${status}</span>
    </div>
    <div class="ops-grid">
      <div class="ops-card"><b>Online ≤10m</b><span>${esc(onlineAgents.length)}</span></div>
      <div class="ops-card"><b>Live events</b><span>${esc(liveActivityEvents.length)} / 20</span></div>
      <div class="ops-card"><b>Loop boxes</b><span>${esc(loops.enabled_boxes ?? '?')} / ${esc(loops.total_boxes ?? '?')}</span></div>
      <div class="ops-card"><b>Hermes</b><span>${esc(providers.hermes?.status || 'unknown')} · ${esc(providers.hermes?.processes ?? 0)} proc</span></div>
      <div class="ops-card"><b>Claude bridge</b><span>${esc(providers.claude_bridge?.status || states.claudeBridgeHealth?.health || 'unknown')}</span></div>
      <div class="ops-card"><b>Resource guard</b><span>${esc(resources.guard?.status || states.resourceGuard?.status || 'unknown')} · CPU ${Math.round(resources.guard?.total_cpu || states.resourceGuard?.total_cpu || 0)}%</span></div>
      <div class="ops-card"><b>Disk free</b><span>${esc(resources.disk_free_percent ?? '?')}%</span></div>
      <div class="ops-card"><b>CPU temp</b><span>${temperature == null ? 'unavailable' : `${esc(temperature)} °C`}</span></div>
    </div>
    <div class="ops-section">Loop Engine</div>
    <div class="chips">${boxItems}</div>
    <div class="ops-section">Who is online</div>
    <div class="ops-line">${esc(agentLine)}</div>
    <div class="agent-legend">${palette}</div>
    <div class="ops-section">Latest reports</div>
    <ul class="ops-list">${reportItems}</ul>
    <div class="ops-section">Confirmed action queue · ${esc(queue.pending || 0)} pending</div>
    <div class="ops-queue-controls">
      <select id="queueAction" aria-label="Разрешённое действие">${actionOptions}</select>
      <button id="queueSubmit" ${actionOptions ? '' : 'disabled'}>В очередь</button>
    </div>
    <div class="ops-line">Viewer только ставит задачу. Запуск: brain queue run &lt;id&gt;.</div>
    <ul class="ops-list">${queueItems}</ul>
    <div class="ops-section">Latest 20 events</div>
    <ul class="ops-list">${eventItems}</ul>
  `;
  const queueSubmit = document.getElementById('queueSubmit');
  if (queueSubmit && actionOptions) {
    queueSubmit.onclick = async () => {
      try {
        const action = document.getElementById('queueAction').value;
        const task = await postBrain('/api/queue', {
          action,
          parameters: {},
          requested_by: 'viewer',
        });
        reportBrainAction(`Задача поставлена в очередь: ${task.id}`);
        await refreshStationStatus();
      } catch (error) {
        reportBrainAction(error.message || String(error));
      }
    };
  }
}
function relationChips(index, limit = 12) {
  return adjacency[index]
    .slice(0, limit)
    .map(item => {
      const other = DATA.nodes[item.index];
      const arrow = item.direction === 'out' ? '->' : '<-';
      return `<span class="chip">${esc(arrow)} ${esc(item.relation)} ${esc(other.label)}</span>`;
    })
    .join('');
}
function refreshVisualState() {
  let growthVisibleCount = 0;
  for (let i = 0; i < DATA.nodes.length; i++) {
    const visible = visibleSet.has(i);
    const growthVisible = timelineNodeVisible(i);
    if (growthVisible) growthVisibleCount += 1;
    const focused = focusSet.size === 0 || focusSet.has(i);
    const boost = i === selectedIndex ? 1.55 : i === hoveredIndex ? 1.3 : focused ? 1 : 0.22;
    const heat = historyHeatEnabled ? historyHeat[i] : 0;
    const dim = growthVisible ? (visible ? boost : 0.08) : 0;
    colors[i * 3] = growthVisible ? Math.min(1, baseColors[i * 3] * dim + (i === selectedIndex ? 0.18 : 0) + heat * 0.5) : 0;
    colors[i * 3 + 1] = growthVisible ? Math.min(1, baseColors[i * 3 + 1] * dim + (i === selectedIndex ? 0.18 : 0) + heat * 0.16) : 0;
    colors[i * 3 + 2] = growthVisible ? Math.min(1, baseColors[i * 3 + 2] * dim + (i === selectedIndex ? 0.18 : 0)) : 0;
    sizes[i] = growthVisible
      ? baseSizes[i] * (visible ? (i === selectedIndex ? 1.34 : i === hoveredIndex ? 1.18 : focused ? 1 : 0.72) : 0.52) * (1 + heat * 0.65)
      : 0;
  }
  window.__GRAPHIFY_BRAIN_GROWTH_VISIBLE_COUNT = growthVisibleCount;
  nodeGeometry.attributes.color.needsUpdate = true;
  nodeGeometry.attributes.size.needsUpdate = true;
  edgeMaterial.opacity = focusSet.size ? 0.42 : 0.28;
  labelLayer.style.display = showLabels || showCards ? 'block' : 'none';
  lobeGroup.visible = showLobes;
  for (let edgeIndex = 0; edgeIndex < DATA.links.length; edgeIndex++) {
    const offset = edgeIndex * 6;
    const highlighted = highlightedEdgeIndices.has(edgeIndex);
    const growthVisible = (
      timelineNodeVisible(linkSourceIndices[edgeIndex])
      && timelineNodeVisible(linkTargetIndices[edgeIndex])
    );
    for (let component = 0; component < 6; component++) {
      edgeColors[offset + component] = growthVisible
        ? highlighted
          ? (component % 3 === 1 ? 1 : 0.35)
          : baseEdgeColors[offset + component] * (highlightedEdgeIndices.size ? 0.28 : 1)
        : 0;
    }
  }
  pulses.visible = timelineMode !== 'growth' || growthVisibleCount === DATA.nodes.length;
  edgeGeometry.attributes.color.needsUpdate = true;
}
function setFocus(index, depth = 1, preserveEdges = false) {
  focusSet.clear();
  if (!preserveEdges) highlightedEdgeIndices.clear();
  if (index >= 0) {
    const queue = [{ index, depth: 0 }];
    focusSet.add(index);
    while (queue.length) {
      const currentItem = queue.shift();
      if (currentItem.depth >= depth) continue;
      adjacency[currentItem.index].forEach(item => {
        if (focusSet.has(item.index)) return;
        focusSet.add(item.index);
        queue.push({ index: item.index, depth: currentItem.depth + 1 });
      });
    }
  }
  refreshVisualState();
}
function structBlock(otherIndex, relation, direction) {
  const other = DATA.nodes[otherIndex];
  const arrow = direction === 'out' ? '→' : '←';
  return `<div class="struct-block" data-index="${otherIndex}">
    <span class="struct-rel">${esc(arrow)} ${esc(relation)}</span>
    <span class="struct-glyph" style="--c:${esc(other.color)}">${esc(other.glyph)} ${esc(other.type)}</span>
    <span class="struct-label">${esc(other.label)}</span>
    <span class="struct-meta">${esc(other.lobe_name)} · w${Math.round((other.importance || 0) * 100)} · a${Math.round((other.activity?.score || 0) * 100)}</span>
  </div>`;
}
function renderStructure(index) {
  const node = DATA.nodes[index];
  const outgoing = adjacency[index].filter(item => item.direction === 'out');
  const incoming = adjacency[index].filter(item => item.direction === 'in');
  const outHtml = outgoing.slice(0, 12).map(item => structBlock(item.index, item.relation, 'out')).join('')
    || '<div class="struct-empty">нет исходящих связей</div>';
  const inHtml = incoming.slice(0, 12).map(item => structBlock(item.index, item.relation, 'in')).join('')
    || '<div class="struct-empty">нет входящих связей</div>';
  structure.innerHTML = `
    <div class="struct-head">Как устроен узел</div>
    <div class="struct-sub">Структура и связи в живых блоках. Клик по блоку — перейти к узлу.</div>
    <div class="struct-grid">
      <div class="struct-block struct-center">
        <span class="struct-rel">этот узел</span>
        <span class="struct-glyph" style="--c:${esc(node.color)}">${esc(node.glyph)} ${esc(node.type)}</span>
        <span class="struct-label">${esc(node.label)}</span>
        <span class="struct-meta">${esc(node.lobe_name)} · ${esc(node.lobe_role)}</span>
        <span class="struct-meta">важность ${Math.round((node.importance || 0) * 100)}% · активность ${Math.round((node.activity?.score || 0) * 100)}% · связей ${esc(node.neighbor_count)} · сообщество ${esc(node.community)}</span>
      </div>
    </div>
    <div class="struct-section">Влияет на → (${outgoing.length})</div>
    <div class="struct-grid">${outHtml}</div>
    <div class="struct-section">Опирается на ← (${incoming.length})</div>
    <div class="struct-grid">${inHtml}</div>
  `;
  structure.style.display = 'block';
  structure.querySelectorAll('.struct-block[data-index]').forEach(element => {
    element.onclick = () => selectNode(Number(element.dataset.index));
  });
}
let detailsPromise = null;
function loadDetails() {
  if (!detailsPromise) {
    detailsPromise = fetch(DETAILS_URL, { cache: 'no-cache' }).then(response => {
      if (!response.ok) throw new Error(`brain-details.json request failed: ${response.status}`);
      return response.json();
    });
  }
  return detailsPromise;
}
async function renderInspector(index) {
  const node = DATA.nodes[index];
  const detailsData = await loadDetails();
  if (selectedIndex !== index) return;
  const row = detailsData.nodes.find(detail => detail[9] === node.id) || [];
  const details = {
    source_location: row[0],
    source_url: row[1],
    obsidian_file: row[2],
    obsidian_url: row[3],
    last_touch: row[4],
    age_days: row[5],
    dirty: row[6],
    hyperedge_count: row[7],
    source_entity_count: row[8],
  };
  inspector.innerHTML = `
    <h2>${esc(node.label)}</h2>
    <div class="field"><b>type:</b> ${esc(node.type)} · <b>community:</b> ${esc(node.community)}</div>
    <div class="field"><b>brain lobe:</b> ${esc(node.lobe_name)} · ${esc(node.lobe_role)}</div>
    <div class="field"><b>importance:</b> ${Math.round((node.importance || 0) * 100)}% · <b>activity:</b> ${Math.round((node.activity?.score || 0) * 100)}%</div>
    <div class="field"><b>last touch:</b> ${esc(details.last_touch || 'unknown')}${details.dirty ? ' · dirty now' : ''}</div>
    <div class="field"><b>degree:</b> ${esc(node.degree)} · <b>neighbors:</b> ${esc(node.neighbor_count)}</div>
    <div class="field"><b>hyperedges:</b> ${esc(details.hyperedge_count)} · <b>source entities:</b> ${esc(details.source_entity_count)}</div>
    <div class="field"><b>source:</b> ${esc(node.source_file || '-')} ${esc(details.source_location || '')}</div>
    <div class="field"><b>Obsidian:</b> ${esc(details.obsidian_file)}</div>
    <div class="chips">
      <span class="chip">${esc(node.glyph)}</span>
      <span class="chip">${esc(node.lobe_name)}</span>
      <span class="chip">importance ${Math.round((node.importance || 0) * 100)}%</span>
      <span class="chip">activity ${Math.round((node.activity?.score || 0) * 100)}%</span>
      <span class="chip">community ${esc(node.community)}</span>
      <span class="chip">${esc(node.type)}</span>
    </div>
    <div class="field"><b>Связи:</b></div>
    <div class="chips">${relationChips(index)}</div>
    <div class="button-row">
      ${details.obsidian_url ? '<button id="nodeObsidian">Open in Obsidian</button>' : ''}
      ${node.source_file ? '<button id="nodeOpenFile">Open file</button>' : ''}
      <button id="copyPath">Copy path</button>
      <button id="copyNodeId">Copy node ID</button>
      <button id="rememberNode">Запомнить</button>
      <button id="nodeConnections">Показать связи: 1 hop</button>
    </div>
  `;
  const obsidianButton = document.getElementById('nodeObsidian');
  if (obsidianButton) obsidianButton.onclick = () => { window.location.href = details.obsidian_url; };
  const openFileButton = document.getElementById('nodeOpenFile');
  if (openFileButton) {
    openFileButton.onclick = async () => {
      try {
        await postBrain('/api/open', { path: node.source_file });
      } catch (error) {
        reportBrainAction(error.message || String(error));
      }
    };
  }
  document.getElementById('copyPath').onclick = () => navigator.clipboard?.writeText(node.source_file || node.id);
  document.getElementById('copyNodeId').onclick = () => navigator.clipboard?.writeText(node.id);
  document.getElementById('rememberNode').onclick = async () => {
    const text = (window.prompt(`Что запомнить про ${node.label}?`) || '').trim();
    if (!text) return;
    try {
      const result = await rememberFromViewer(text, [node.id], 'fact', ['viewer']);
      reportBrainAction(`Сохранено: ${result.id}`);
    } catch (error) {
      reportBrainAction(error.message || String(error));
    }
  };
  document.getElementById('nodeConnections').onclick = event => {
    selectedFocusDepth = selectedFocusDepth === 0 ? 1 : selectedFocusDepth === 1 ? 2 : 0;
    setFocus(index, selectedFocusDepth);
    event.currentTarget.textContent = selectedFocusDepth === 0
      ? 'Показать связи: 1 hop'
      : selectedFocusDepth === 1
        ? 'Показать связи: 2 hops'
        : 'Скрыть связи';
    markUserActivity();
  };
}
function selectNode(index) {
  // A selection (click, search result or #node= link) is activity: a sleeping viewer must draw it.
  markUserActivity();
  if (DATA.clusterOverview) { expandCluster(Number(DATA.nodes[index].id)); return; }
  selectedIndex = index;
  selectedFocusDepth = 0;
  const node = DATA.nodes[index];
  setFocus(index, 0);
  highlight.visible = true;
  highlight.position.set(current[index * 3], current[index * 3 + 1], current[index * 3 + 2]);
  inspector.innerHTML = `<h2>${esc(node.label)}</h2><div class="field">Загружаю детали…</div>`;
  renderStructure(index);
  renderLabels(true);
  renderInspector(index).catch(error => {
    if (selectedIndex === index) {
      inspector.insertAdjacentHTML('beforeend', `<div class="field bad">${esc(error.message || error)}</div>`);
    }
  });
}
function localShortestPath(startIndex, endIndex) {
  const queue = [startIndex];
  const previous = new Map();
  const visited = new Set([startIndex]);
  while (queue.length && !visited.has(endIndex)) {
    const index = queue.shift();
    adjacency[index].forEach(item => {
      if (visited.has(item.index)) return;
      visited.add(item.index);
      previous.set(item.index, index);
      queue.push(item.index);
    });
  }
  if (!visited.has(endIndex)) return [];
  const path = [endIndex];
  while (path[0] !== startIndex) path.unshift(previous.get(path[0]));
  return path;
}
function applyHighlightedPath(pathIndices) {
  focusSet.clear();
  highlightedEdgeIndices.clear();
  pathIndices.forEach(index => focusSet.add(index));
  for (let offset = 0; offset + 1 < pathIndices.length; offset++) {
    const source = pathIndices[offset];
    const target = pathIndices[offset + 1];
    const edgeIndex = nodeEdgeIndices[source].find(candidate => {
      const a = linkSourceIndices[candidate];
      const b = linkTargetIndices[candidate];
      return (a === source && b === target) || (a === target && b === source);
    });
    if (edgeIndex !== undefined) highlightedEdgeIndices.add(edgeIndex);
  }
  refreshVisualState();
  renderLabels(true);
}
async function highlightPath(startIndex, endIndex) {
  let pathIndices = [];
  try {
    const start = DATA.nodes[startIndex].id;
    const end = DATA.nodes[endIndex].id;
    const response = await fetch(
      `/api/path?from=${encodeURIComponent(start)}&to=${encodeURIComponent(end)}`,
      { cache: 'no-cache' }
    );
    if (!response.ok) throw new Error(`path API ${response.status}`);
    const result = await response.json();
    pathIndices = (result.nodes || [])
      .map(node => nodeById.get(node.id)?.index)
      .filter(index => Number.isInteger(index));
  } catch (_error) {
    pathIndices = localShortestPath(startIndex, endIndex);
  }
  if (selectedIndex === endIndex && pathIndices.length) {
    applyHighlightedPath(pathIndices);
    markUserActivity();
  }
}

document.getElementById('openHome').onclick = () => { window.location.href = DATA.vault.home; };
document.getElementById('openGraphNote').onclick = () => { window.location.href = DATA.vault.graphify; };
document.getElementById('addBrain').onclick = () => {
  addToBrain().catch(error => reportBrainAction(error.message || String(error)));
};
document.getElementById('toggleLabels').onclick = event => {
  showLabels = !showLabels;
  event.currentTarget.classList.toggle('active', showLabels);
  renderLabels(true);
};
document.getElementById('toggleCards').onclick = event => {
  showCards = !showCards;
  event.currentTarget.classList.toggle('active', showCards);
  renderLabels(true);
};
document.getElementById('toggleLobes').onclick = event => {
  showLobes = !showLobes;
  event.currentTarget.classList.toggle('active', showLobes);
  refreshVisualState();
  renderLabels(true);
};
document.getElementById('toggleDrag').onclick = event => {
  dragMode = !dragMode;
  event.currentTarget.classList.toggle('active', dragMode);
  canvas.style.cursor = dragMode ? 'grab' : 'default';
};
document.getElementById('toggleOps').onclick = event => {
  showOps = !showOps;
  event.currentTarget.classList.toggle('active', showOps);
  opsPanel.style.display = showOps ? 'block' : 'none';
  if (showOps) {
    renderOpsPanel();
    refreshStationStatus();
  }
};
document.getElementById('resetView').onclick = () => {
  root.rotation.set(-0.08, 0, 0);
  camera.position.set(0, 0, 360);
  selectedIndex = -1;
  hoveredIndex = -1;
  focusSet.clear();
  highlightedEdgeIndices.clear();
  highlight.visible = false;
  structure.style.display = 'none';
  refreshVisualState();
  renderLabels(true);
};
let motion = true;
let cinematic = true;
let quietMode = false;
let growthTimeCutoff = null;
let cameraTween = null;
try {
  quietMode = localStorage.getItem('graphify-brain-quiet') === '1';
} catch (_error) {
  quietMode = false;
}
const quietButton = document.getElementById('toggleQuiet');
quietButton.classList.toggle('active', quietMode);
document.getElementById('toggleMotion').onclick = event => {
  motion = !motion;
  event.currentTarget.textContent = motion ? 'Пауза' : 'Движение';
};
document.getElementById('toggleCinematic').onclick = event => {
  cinematic = !cinematic;
  event.currentTarget.classList.toggle('active', cinematic);
};
quietButton.onclick = () => {
  quietMode = !quietMode;
  quietButton.classList.toggle('active', quietMode);
  try {
    localStorage.setItem('graphify-brain-quiet', quietMode ? '1' : '0');
  } catch (_error) {
    // Private browsing may deny storage; the in-memory toggle still works.
  }
  markUserActivity();
};

const search = document.getElementById('search');
const results = document.getElementById('results');
const typeFilter = document.getElementById('typeFilter');
let searchRequest = 0;
let activeSearchResult = -1;
function scoreNode(node, index, q) {
  if (!q) return Math.min(100, Number(node.degree || 0) + Number(node.importance || 0) * 80);
  let score = 0;
  const label = node.label.toLowerCase();
  const source = (node.source_file || '').toLowerCase();
  if (label === q) score += 120;
  if (label.startsWith(q)) score += 70;
  if (label.includes(q)) score += 42;
  if (source.includes(q)) score += 30;
  if ((node.id || '').includes(q)) score += 28;
  if ((node.extended_search || '').includes(q)) score += 16;
  score += Math.min(28, Number(node.degree || 0));
  score += Number(node.importance || 0) * 24;
  score += Number(node.activity?.score || 0) * 18;
  if (focusSet.has(index)) score += 24;
  return score;
}
function passesFilters(node, _index, _q = '') {
  const type = typeFilter.value;
  const community = communityFilter.value;
  const lobe = lobeFilter.value;
  return (type === 'all' || node.type === type) &&
    (community === 'all' || String(node.community) === community) &&
    (lobe === 'all' || node.brain_lobe === lobe);
}
function sortMatches(items, q) {
  const mode = sortMode.value;
  return items.sort((a, b) => {
    if (mode === 'degree') return Number(b.node.degree || 0) - Number(a.node.degree || 0);
    if (mode === 'importance') return Number(b.node.importance || 0) - Number(a.node.importance || 0);
    if (mode === 'activity') return Number(b.node.activity?.score || 0) - Number(a.node.activity?.score || 0);
    if (mode === 'lobe') return String(a.node.brain_lobe || '').localeCompare(String(b.node.brain_lobe || ''));
    if (mode === 'label') return a.node.label.localeCompare(b.node.label);
    return scoreNode(b.node, b.index, q) - scoreNode(a.node, a.index, q);
  });
}
function flyToNode(index) {
  const local = new THREE.Vector3(
    current[index * 3],
    current[index * 3 + 1],
    current[index * 3 + 2]
  );
  const world = root.localToWorld(local);
  cameraTween = {
    t: 0,
    dur: 42,
    from: currentView(),
    to: {
      px: world.x,
      py: world.y,
      pz: Math.max(150, Math.min(310, camera.position.z)),
      rx: root.rotation.x,
      ry: root.rotation.y,
    }
  };
  markUserActivity();
}
async function showExternalSearchNode(id) {
  try {
    const response = await fetch(`/api/node?id=${encodeURIComponent(id)}`, { cache: 'no-cache' });
    if (!response.ok) throw new Error(`node API ${response.status}`);
    const node = await response.json();
    selectedIndex = -1;
    setFocus(-1, 0);
    structure.style.display = 'none';
    inspector.innerHTML = `
      <h2>${esc(node.label || node.id)}</h2>
      <div class="field"><b>type:</b> ${esc(node.memory_type || node.type)}</div>
      <div class="field">${esc(node.text || '')}</div>
      <div class="field"><b>agent:</b> ${esc(node.agent || 'unknown')} · <b>updated:</b> ${esc(node.updated_at || '')}</div>
      <div class="field"><b>tags:</b> ${esc((node.tags || []).join(', ') || '-')}</div>
      <div class="field"><b>source:</b> ${esc(node.source_file || '-')}</div>
      <div class="button-row">
        <button id="copyExternalId">Copy node ID</button>
        ${node.source_file ? '<button id="openExternalFile">Open file</button>' : ''}
        <button id="rememberExternal">Запомнить</button>
      </div>
    `;
    document.getElementById('copyExternalId').onclick = () => navigator.clipboard?.writeText(node.id);
    const openExternalFile = document.getElementById('openExternalFile');
    if (openExternalFile) {
      openExternalFile.onclick = async () => {
        try {
          await postBrain('/api/open', { path: node.source_file });
        } catch (error) {
          reportBrainAction(error.message || String(error));
        }
      };
    }
    document.getElementById('rememberExternal').onclick = async () => {
      const text = (window.prompt(`Что запомнить про ${node.label || node.id}?`) || '').trim();
      if (!text) return;
      try {
        const result = await rememberFromViewer(text, [node.id], 'fact', ['viewer']);
        reportBrainAction(`Сохранено: ${result.id}`);
      } catch (error) {
        reportBrainAction(error.message || String(error));
      }
    };
  } catch (error) {
    inspector.innerHTML = `<h2>Узел недоступен</h2><div class="field bad">${esc(error.message || error)}</div>`;
  }
}
function chooseSearchResult(index, id = '') {
  if (index >= 0) {
    selectNode(index);
    flyToNode(index);
  } else if (id) {
    showExternalSearchNode(id);
  }
  results.style.display = 'none';
}
function renderSearchResults(matches) {
  activeSearchResult = matches.length ? 0 : -1;
  results.innerHTML = matches.map(({ node, index, score }, resultIndex) =>
    `<div class="result${resultIndex === activeSearchResult ? ' active' : ''}" data-index="${index}" data-id="${esc(node.id)}" data-result-index="${resultIndex}"><span class="score">${Number(score).toPrecision(3)}</span>${esc(node.label)}<small>${esc(node.lobe_name)} · w${Math.round((node.importance || 0) * 100)} · a${Math.round((node.activity?.score || 0) * 100)} · ${esc(node.source_file || node.id)}</small></div>`
  ).join('');
  results.style.display = matches.length ? 'block' : 'none';
  results.querySelectorAll('.result').forEach(item => {
    item.onclick = () => chooseSearchResult(Number(item.dataset.index), item.dataset.id);
  });
}
async function updateSearch() {
  const q = search.value.trim().toLowerCase();
  const request = ++searchRequest;
  visibleSet.clear();
  const filtered = DATA.nodes
    .map((node, index) => ({ node, index }))
    .filter(({ node, index }) => passesFilters(node, index, q));
  filtered.forEach(({ index }) => visibleSet.add(index));
  refreshVisualState();
  renderLabels(true);
  if (!q) {
    results.style.display = 'none';
    return;
  }
  let matches = [];
  try {
    const response = await fetch(`/api/search?q=${encodeURIComponent(q)}&limit=10`, { cache: 'no-cache' });
    if (!response.ok) throw new Error(`search API ${response.status}`);
    const apiMatches = await response.json();
    matches = apiMatches
      .map(match => {
        const found = nodeById.get(match.id);
        return found
          ? { ...found, score: match.score }
          : {
              index: -1,
              score: match.score,
              node: {
                id: match.id,
                label: match.label,
                type: match.type,
                source_file: match.path,
                lobe_name: match.lobe || 'Shared memory',
                brain_lobe: match.lobe,
                community: match.community,
                importance: match.importance || 0,
                activity: { score: match.activity || 0 },
              }
            };
      })
      .filter(item => passesFilters(item.node, item.index, q));
  } catch (_error) {
    matches = DATA.nodes
      .map((node, index) => ({ node, index, score: scoreNode(node, index, q) }))
      .filter(({ node, index }) => passesFilters(node, index, q) && (node.extended_search || '').includes(q));
    sortMatches(matches, q);
    matches = matches.slice(0, 10);
  }
  if (request !== searchRequest) return;
  // No matches should not erase the entire brain. Keep the locally filtered
  // graph visible, while the empty result list still truthfully reports no hits.
  if (matches.length) {
    visibleSet.clear();
    matches.forEach(({ index }) => { if (index >= 0) visibleSet.add(index); });
  }
  refreshVisualState();
  renderLabels(true);
  renderSearchResults(matches);
}
search.addEventListener('input', updateSearch);
search.addEventListener('keydown', event => {
  const items = [...results.querySelectorAll('.result')];
  if (!items.length || !['ArrowDown', 'ArrowUp', 'Enter', 'Escape'].includes(event.key)) return;
  event.preventDefault();
  if (event.key === 'Escape') {
    results.style.display = 'none';
    return;
  }
  if (event.key === 'Enter') {
    const item = items[Math.max(0, activeSearchResult)];
    if (item) chooseSearchResult(Number(item.dataset.index), item.dataset.id);
    return;
  }
  activeSearchResult = event.key === 'ArrowDown'
    ? (activeSearchResult + 1) % items.length
    : (activeSearchResult - 1 + items.length) % items.length;
  items.forEach((item, index) => item.classList.toggle('active', index === activeSearchResult));
  items[activeSearchResult].scrollIntoView({ block: 'nearest' });
});
typeFilter.addEventListener('change', updateSearch);
communityFilter.addEventListener('change', updateSearch);
lobeFilter.addEventListener('change', updateSearch);
sortMode.addEventListener('change', updateSearch);

const projectionVector = new THREE.Vector3();
const labelPool = Array.from({ length: 60 }, () => {
  const element = document.createElement('div');
  element.style.display = 'none';
  labelLayer.appendChild(element);
  return element;
});
let labelRankingDirty = true;
let labelOrder = [];
let lastLabelUpdate = 0;
function projectNode(index) {
  projectionVector.set(current[index * 3], current[index * 3 + 1], current[index * 3 + 2]);
  root.localToWorld(projectionVector);
  projectionVector.project(camera);
  if (projectionVector.z < -1 || projectionVector.z > 1) return null;
  return {
    x: (projectionVector.x * 0.5 + 0.5) * window.innerWidth,
    y: (-projectionVector.y * 0.5 + 0.5) * window.innerHeight,
    depth: projectionVector.z
  };
}
function labelPriority(index) {
  const node = DATA.nodes[index];
  let score = Number(node.degree || 0) + Number(node.importance || 0) * 80 + Number(node.activity?.score || 0) * 30;
  if (index === selectedIndex) score += 1000;
  if (index === hoveredIndex) score += 800;
  if (focusSet.has(index)) score += 120;
  if (!visibleSet.has(index)) score -= 200;
  return score;
}
function renderLabels(force = false, energyMode = 'active', time = performance.now()) {
  if (energyMode === 'idle' && !force && time - lastLabelUpdate < 500) return;
  lastLabelUpdate = time;
  if (force) labelRankingDirty = true;
  if (!showLabels && !showCards) {
    labelPool.forEach(element => { element.style.display = 'none'; });
    return;
  }
  if (labelRankingDirty) {
    const ranked = DATA.nodes
      .map((_, index) => ({ index, score: labelPriority(index) }))
      .filter(item => item.score > 0 && visibleSet.has(item.index) && timelineNodeVisible(item.index))
      .sort((a, b) => b.score - a.score);
    const forced = [];
    if (selectedIndex >= 0 && visibleSet.has(selectedIndex) && timelineNodeVisible(selectedIndex)) forced.push(selectedIndex);
    if (
      hoveredIndex >= 0
      && hoveredIndex !== selectedIndex
      && visibleSet.has(hoveredIndex)
      && timelineNodeVisible(hoveredIndex)
    ) forced.push(hoveredIndex);
    labelOrder = forced.concat(ranked.map(item => item.index).filter(index => !forced.includes(index)));
    labelRankingDirty = false;
    window.__GRAPHIFY_BRAIN_LABEL_RANK_COUNT = (window.__GRAPHIFY_BRAIN_LABEL_RANK_COUNT || 0) + 1;
  }
  const maxLabels = showCards ? 22 : 60;

  const placed = [];
  let shown = 0;
  for (const index of labelOrder) {
    if (shown >= maxLabels) break;
    const screen = projectNode(index);
    if (!screen) continue;
    const node = DATA.nodes[index];
    const useCard = showCards || index === selectedIndex || index === hoveredIndex;
    const forcedLabel = index === selectedIndex || index === hoveredIndex;
    // Decluttering: estimate the label's screen box and skip it if it would
    // overlap an already-placed label, unless it is the forced node.
    const boxWidth = Math.min(232, node.label.length * 6.6 + (useCard ? 64 : 20)) + 8;
    const boxHeight = (useCard ? 38 : 19) + 6;
    const box = { x: screen.x - 4, y: screen.y - (useCard ? 30 : 17), w: boxWidth, h: boxHeight };
    if (!forcedLabel) {
      let overlap = false;
      for (let p = 0; p < placed.length; p++) {
        const q = placed[p];
        if (box.x < q.x + q.w && box.x + box.w > q.x && box.y < q.y + q.h && box.y + box.h > q.y) { overlap = true; break; }
      }
      if (overlap) continue;
    }
    placed.push(box);
    const card = labelPool[shown];
    card.className = useCard ? 'node-card' : 'node-label';
    card.dataset.glyph = node.glyph || 'N';
    card.style.display = 'block';
    card.style.left = '0';
    card.style.top = '0';
    card.style.transform = `translate3d(${screen.x}px, ${screen.y - (useCard ? 28 : 16)}px, 0) translate(-50%, -50%)`;
    card.style.borderColor = index === selectedIndex ? 'rgba(103, 232, 249, 0.85)' : 'rgba(125, 211, 252, 0.24)';
    card.style.opacity = String(Math.max(0.42, 1 - Math.max(0, screen.depth) * 0.36));
    const renderKey = `${index}:${useCard ? 'card' : 'label'}`;
    if (card.dataset.renderKey !== renderKey) {
      card.dataset.renderKey = renderKey;
      if (useCard) {
        card.innerHTML = `${esc(node.label)}<small>${esc(node.lobe_name)} · w${Math.round((node.importance || 0) * 100)} · a${Math.round((node.activity?.score || 0) * 100)}</small>`;
      } else {
        card.textContent = node.label;
      }
    }
    shown += 1;
  }
  for (let index = shown; index < labelPool.length; index++) {
    labelPool[index].style.display = 'none';
  }
}

const raycaster = new THREE.Raycaster();
raycaster.params.Points.threshold = 8;
const pointer = new THREE.Vector2();
let dragging = false;
let nodeDragging = false;
let dragIndex = -1;
let dragPlane = null;
let moved = false;
let lastX = 0;
let lastY = 0;
function setPointer(event) {
  pointer.x = (event.clientX / window.innerWidth) * 2 - 1;
  pointer.y = -(event.clientY / window.innerHeight) * 2 + 1;
  raycaster.setFromCamera(pointer, camera);
}
function pickNode(event) {
  setPointer(event);
  return raycaster.intersectObject(points).find(
    hit => visibleSet.has(hit.index) && timelineNodeVisible(hit.index)
  );
}
function showHover(event, index) {
  hoveredIndex = index;
  if (index < 0) {
    hoverCard.style.display = 'none';
    refreshVisualState();
    renderLabels(true);
    return;
  }
  const node = DATA.nodes[index];
  hoverCard.dataset.glyph = node.glyph || 'N';
  hoverCard.style.display = 'block';
  hoverCard.style.left = `${event.clientX}px`;
  hoverCard.style.top = `${event.clientY}px`;
  hoverCard.innerHTML = `${esc(node.label)}<small>${esc(node.lobe_name)} · w${Math.round((node.importance || 0) * 100)} · a${Math.round((node.activity?.score || 0) * 100)} · ${esc(node.source_file || node.id)}</small>`;
  refreshVisualState();
  renderLabels(true);
}
canvas.addEventListener('pointerdown', event => {
  const hit = pickNode(event);
  moved = false; lastX = event.clientX; lastY = event.clientY;
  if (dragMode && hit && hit.index !== undefined) {
    nodeDragging = true;
    dragIndex = hit.index;
    selectNode(dragIndex);
    const worldPoint = new THREE.Vector3(current[dragIndex * 3], current[dragIndex * 3 + 1], current[dragIndex * 3 + 2]);
    root.localToWorld(worldPoint);
    const normal = new THREE.Vector3();
    camera.getWorldDirection(normal);
    dragPlane = new THREE.Plane().setFromNormalAndCoplanarPoint(normal, worldPoint);
    canvas.style.cursor = 'grabbing';
  } else {
    dragging = true;
  }
  canvas.setPointerCapture(event.pointerId);
});
canvas.addEventListener('pointermove', event => {
  const dx = event.clientX - lastX;
  const dy = event.clientY - lastY;
  if (Math.abs(dx) + Math.abs(dy) > 2) moved = true;
  if (nodeDragging && dragIndex >= 0 && dragPlane) {
    setPointer(event);
    const hitPoint = new THREE.Vector3();
    if (raycaster.ray.intersectPlane(dragPlane, hitPoint)) {
      const localPoint = root.worldToLocal(hitPoint.clone());
      base[dragIndex * 3] = localPoint.x;
      base[dragIndex * 3 + 1] = localPoint.y;
      base[dragIndex * 3 + 2] = localPoint.z;
      current[dragIndex * 3] = localPoint.x;
      current[dragIndex * 3 + 1] = localPoint.y;
      current[dragIndex * 3 + 2] = localPoint.z;
      DATA.nodes[dragIndex].x = localPoint.x;
      DATA.nodes[dragIndex].y = localPoint.y;
      DATA.nodes[dragIndex].z = localPoint.z;
      nodeGeometry.attributes.position.needsUpdate = true;
      syncDraggedNodeConnections(dragIndex);
      renderLabels();
    }
  } else if (dragging) {
    root.rotation.y += dx * 0.004;
    root.rotation.x += dy * 0.003;
    root.rotation.x = Math.max(-0.9, Math.min(0.9, root.rotation.x));
  } else {
    const hit = pickNode(event);
    const nextHover = hit && hit.index !== undefined ? hit.index : -1;
    if (nextHover !== hoveredIndex) showHover(event, nextHover);
    if (nextHover >= 0) {
      hoverCard.style.left = `${event.clientX}px`;
      hoverCard.style.top = `${event.clientY}px`;
    }
  }
  lastX = event.clientX; lastY = event.clientY;
});
canvas.addEventListener('pointerup', event => {
  if (nodeDragging) {
    nodeDragging = false;
    dragIndex = -1;
    dragPlane = null;
    canvas.style.cursor = dragMode ? 'grab' : 'default';
    return;
  }
  dragging = false;
  if (moved) return;
  const hit = pickNode(event);
  if (hit && hit.index !== undefined) {
    if (event.shiftKey && selectedIndex >= 0 && selectedIndex !== hit.index) {
      const startIndex = selectedIndex;
      selectNode(hit.index);
      highlightPath(startIndex, hit.index);
    } else {
      selectNode(hit.index);
    }
  }
});
canvas.addEventListener('pointerleave', () => {
  if (!nodeDragging) showHover({ clientX: 0, clientY: 0 }, -1);
});
canvas.addEventListener('wheel', event => {
  event.preventDefault();
  // Zoom toward the cursor: keep the world point under the mouse fixed on the
  // screen while changing the camera distance, instead of always zooming to
  // the centre. The graph's nominal focus plane is z = 0 in world space.
  const oldZ = camera.position.z;
  const newZ = Math.max(145, Math.min(780, oldZ + event.deltaY * 0.22));
  const dz = oldZ - newZ;
  if (dz !== 0) {
    const ndcX = (event.clientX / window.innerWidth) * 2 - 1;
    const ndcY = -(event.clientY / window.innerHeight) * 2 + 1;
    const tanHalf = Math.tan((camera.fov * Math.PI / 180) / 2);
    camera.position.x = Math.max(-460, Math.min(460, camera.position.x + ndcX * tanHalf * camera.aspect * dz));
    camera.position.y = Math.max(-360, Math.min(360, camera.position.y + ndcY * tanHalf * dz));
    camera.position.z = newZ;
  }
  const hit = pickNode(event);
  zoomCluster(event.deltaY, newZ, hit && hit.index !== undefined ? hit.index : hoveredIndex);
}, { passive: false });

// ---------- Cinematic post-processing: hand-written bloom + lens depth-of-field ----------
// r124 vendors no EffectComposer/passes, so this is a manual render-target
// pipeline. The scene is additive and mostly depthWrite:false, so depth-based
// DOF is meaningless; DOF here is a screen-space lens focus (sharp centre, soft
// periphery). Everything degrades to a direct render if a target/probe fails
// (e.g. a SwiftShader limitation).
let postfx = null;
const fsCamera = new THREE.OrthographicCamera(-1, 1, 1, -1, 0, 1);
const fsScene = new THREE.Scene();
const fsQuad = new THREE.Mesh(new THREE.PlaneGeometry(2, 2), new THREE.MeshBasicMaterial());
fsScene.add(fsQuad);
function runPass(material, target) {
  fsQuad.material = material;
  renderer.setRenderTarget(target || null);
  renderer.render(fsScene, fsCamera);
}
function makePassMaterial(uniforms, fragment) {
  return new THREE.ShaderMaterial({
    uniforms,
    depthTest: false,
    depthWrite: false,
    vertexShader: SHADERS.fullscreenVertex,
    fragmentShader: fragment
  });
}
function makeTarget(w, h, depth) {
  return new THREE.WebGLRenderTarget(Math.max(2, w), Math.max(2, h), {
    minFilter: THREE.LinearFilter,
    magFilter: THREE.LinearFilter,
    format: THREE.RGBAFormat,
    type: THREE.UnsignedByteType,
    depthBuffer: !!depth,
    stencilBuffer: false
  });
}
function initPostFX() {
  try {
    const dpr = renderer.getPixelRatio();
    const W = Math.max(2, Math.floor(window.innerWidth * dpr));
    const H = Math.max(2, Math.floor(window.innerHeight * dpr));
    const targets = {
      scene: makeTarget(W, H, true),
      bright: makeTarget(W >> 2, H >> 2),
      bloomA: makeTarget(W >> 2, H >> 2),
      bloomB: makeTarget(W >> 2, H >> 2),
      probe: makeTarget(16, 16)
    };
    const blurMat = makePassMaterial(
      { tDiffuse: { value: null }, uDir: { value: new THREE.Vector2(1, 0) }, uTexel: { value: new THREE.Vector2(1, 1) } },
      SHADERS.blurFragment
    );
    const brightMat = makePassMaterial(
      { tDiffuse: { value: null }, uThreshold: { value: 0.6 } },
      SHADERS.brightFragment
    );
    const presentMat = new THREE.ShaderMaterial({
      uniforms: {
        tScene: { value: null },
        uTexel: { value: new THREE.Vector2(1 / W, 1 / H) }
      },
      transparent: true,
      depthTest: false,
      depthWrite: false,
      blending: THREE.NoBlending,
      vertexShader: SHADERS.fullscreenVertex,
      fragmentShader: SHADERS.presentFragment
    });
    const compositeMat = new THREE.ShaderMaterial({
      uniforms: {
        tBloom: { value: null },
        uBloom: { value: 0.95 },
        uDof: { value: 0.3 },
        uAspect: { value: W / H }
      },
      transparent: true,
      depthTest: false,
      depthWrite: false,
      blending: THREE.CustomBlending,
      blendEquation: THREE.AddEquation,
      blendSrc: THREE.SrcAlphaFactor,
      blendDst: THREE.OneFactor,
      blendEquationAlpha: THREE.AddEquation,
      blendSrcAlpha: THREE.ZeroFactor,
      blendDstAlpha: THREE.OneFactor,
      vertexShader: SHADERS.fullscreenVertex,
      fragmentShader: SHADERS.compositeFragment
    });
    postfx = { targets, blurMat, brightMat, presentMat, compositeMat, W, H, frame: 0, bloomReady: false };
    // Capability probe: render the scene once to a target and read a pixel.
    // If SwiftShader/WebGL rejects the target, this throws and we fall back.
    renderer.setRenderTarget(targets.scene);
    renderer.setClearColor(0x000000, 0);
    renderer.clear();
    renderer.render(scene, camera);
    const probe = new Uint8Array(4);
    renderer.readRenderTargetPixels(targets.scene, Math.floor(W / 2), Math.floor(H / 2), 1, 1, probe);
    renderer.setRenderTarget(null);
    renderer.setClearColor(0x020510, 0.32);
    return true;
  } catch (err) {
    if (postfx) {
      Object.values(postfx.targets).forEach(t => t.dispose && t.dispose());
    }
    postfx = null;
    return false;
  }
}
function resizePostFX() {
  if (!postfx) return;
  Object.values(postfx.targets).forEach(t => t.dispose && t.dispose());
  postfx = null;
  initPostFX();
  window.__GRAPHIFY_BRAIN_POSTFX_RESIZE_COUNT = (window.__GRAPHIFY_BRAIN_POSTFX_RESIZE_COUNT || 0) + 1;
}
function renderFrame() {
  window.__GRAPHIFY_BRAIN_FRAME_COUNT = (window.__GRAPHIFY_BRAIN_FRAME_COUNT || 0) + 1;
  if (!cinematic || !postfx) {
    renderer.setRenderTarget(null);
    renderer.setClearColor(0x020510, 0.32);
    renderer.render(scene, camera);
    window.__GRAPHIFY_BRAIN_SCENE_RENDER_COUNT = (window.__GRAPHIFY_BRAIN_SCENE_RENDER_COUNT || 0) + 1;
    return;
  }
  const t = postfx.targets;
  // Render scene geometry exactly once; every remaining operation is a cheap
  // fullscreen pass. The composite pass also supplies edge-aware FXAA.
  renderer.setRenderTarget(t.scene);
  renderer.setClearColor(0x000000, 0);
  renderer.clear();
  renderer.render(scene, camera);
  window.__GRAPHIFY_BRAIN_SCENE_RENDER_COUNT = (window.__GRAPHIFY_BRAIN_SCENE_RENDER_COUNT || 0) + 1;
  postfx.frame += 1;
  // Always present the freshly rendered scene before optional bloom work.
  // This is deliberately outside updateBloom: skipped bloom frames must still
  // replace the canvas with a valid picture.
  postfx.presentMat.uniforms.tScene.value = t.scene.texture;
  postfx.presentMat.uniforms.uTexel.value.set(1 / postfx.W, 1 / postfx.H);
  runPass(postfx.presentMat, null);
  const updateBloom = !quietMode && (!postfx.bloomReady || postfx.frame % 3 === 0);
  if (updateBloom) {
    postfx.brightMat.uniforms.tDiffuse.value = t.scene.texture;
    runPass(postfx.brightMat, t.bright);
    let read = t.bright;
    for (let i = 0; i < 2; i++) {
      postfx.blurMat.uniforms.uTexel.value.set(1 / (postfx.W >> 2), 1 / (postfx.H >> 2));
      postfx.blurMat.uniforms.tDiffuse.value = read.texture;
      postfx.blurMat.uniforms.uDir.value.set(1, 0);
      runPass(postfx.blurMat, t.bloomA);
      postfx.blurMat.uniforms.tDiffuse.value = t.bloomA.texture;
      postfx.blurMat.uniforms.uDir.value.set(0, 1);
      runPass(postfx.blurMat, t.bloomB);
      read = t.bloomB;
    }
    postfx.bloomReady = true;
    window.__GRAPHIFY_BRAIN_POSTFX_UPDATE_COUNT = (window.__GRAPHIFY_BRAIN_POSTFX_UPDATE_COUNT || 0) + 1;
  }
  postfx.compositeMat.uniforms.tBloom.value = t.bloomB.texture;
  postfx.compositeMat.uniforms.uBloom.value = quietMode ? 0 : 0.95;
  postfx.compositeMat.uniforms.uDof.value = quietMode ? 0 : 0.3;
  postfx.compositeMat.uniforms.uAspect.value = postfx.W / postfx.H;
  // Cached bloom is an RGB-only overlay. Do not let this pass clear the scene
  // blit or force the transparent canvas alpha to 1.
  const previousAutoClear = renderer.autoClear;
  renderer.autoClear = false;
  runPass(postfx.compositeMat, null);
  renderer.autoClear = previousAutoClear;
}
initPostFX();

function brightestProbePixel(pixels) {
  const brightest = [0, 0, 0, 0];
  let brightestEnergy = -1;
  for (let offset = 0; offset < pixels.length; offset += 4) {
    const energy = pixels[offset] + pixels[offset + 1] + pixels[offset + 2] + pixels[offset + 3] * 0.01;
    if (energy > brightestEnergy) {
      brightestEnergy = energy;
      brightest[0] = pixels[offset];
      brightest[1] = pixels[offset + 1];
      brightest[2] = pixels[offset + 2];
      brightest[3] = pixels[offset + 3];
    }
  }
  return brightest;
}
function readRenderedProbe() {
  let target = postfx?.targets.probe || null;
  let temporaryTarget = false;
  const pixels = new Uint8Array(16 * 16 * 4);
  const previousTarget = renderer.getRenderTarget();
  const previousAutoClear = renderer.autoClear;
  const previousClearColor = renderer.getClearColor(new THREE.Color()).getHex();
  const previousClearAlpha = renderer.getClearAlpha();
  try {
    if (!target) {
      target = makeTarget(16, 16, true);
      temporaryTarget = true;
      renderer.setRenderTarget(target);
      renderer.setClearColor(0x000000, 0);
      renderer.clear();
      renderer.render(scene, camera);
    } else {
      const targets = postfx.targets;
      if (!cinematic) {
        renderer.setRenderTarget(targets.scene);
        renderer.setClearColor(0x000000, 0);
        renderer.clear();
        renderer.render(scene, camera);
      }
      postfx.presentMat.uniforms.tScene.value = targets.scene.texture;
      postfx.presentMat.uniforms.uTexel.value.set(1 / postfx.W, 1 / postfx.H);
      renderer.autoClear = true;
      runPass(postfx.presentMat, target);
      if (cinematic && postfx.bloomReady) {
        renderer.autoClear = false;
        runPass(postfx.compositeMat, target);
      }
    }
    renderer.readRenderTargetPixels(target, 0, 0, 16, 16, pixels);
    return brightestProbePixel(pixels);
  } catch (_error) {
    return [0, 0, 0, 0];
  } finally {
    renderer.autoClear = previousAutoClear;
    renderer.setRenderTarget(previousTarget);
    renderer.setClearColor(previousClearColor, previousClearAlpha);
    if (temporaryTarget && target) target.dispose();
  }
}
window.__GRAPHIFY_BRAIN_PROBE = function graphifyBrainProbe() {
  renderFrame();
  const pixel = readRenderedProbe();
  return {
    rgba: pixel,
    energyMode: window.__GRAPHIFY_BRAIN_ENERGY_MODE || 'starting',
    frames: window.__GRAPHIFY_BRAIN_FRAME_COUNT || 0,
    cinematic
  };
};

function updateAtmosphere(time) {
  mistPlanes.forEach((mesh, i) => {
    const phase = mesh.userData.phase || 0;
    mesh.position.x += Math.sin(time * mesh.userData.speed + phase) * 0.02;
    mesh.position.y += Math.cos(time * mesh.userData.speed * 1.4 + phase) * 0.012;
    mesh.rotation.z += (i % 2 ? 1 : -1) * mesh.userData.speed * 0.22;
    mesh.material.opacity = 0.032 + 0.05 * (0.5 + 0.5 * Math.sin(time * 0.00035 + phase));
  });
  cinematicGroup.children.forEach(child => {
    if (child.userData?.isBackdrop) {
      child.position.x = Math.sin(time * 0.00012) * 8 + root.rotation.y * -18;
      child.position.y = 12 + Math.cos(time * 0.0001) * 5 + root.rotation.x * 10;
    }
  });
  portal.material.opacity = 0.052 + 0.035 * (0.5 + 0.5 * Math.sin(time * 0.00055));
  if (selectedIndex >= 0) {
    highlight.position.set(current[selectedIndex * 3], current[selectedIndex * 3 + 1], current[selectedIndex * 3 + 2]);
    highlight.scale.setScalar(1 + 0.22 * Math.sin(time * 0.006));
  }
}

const energyState = document.getElementById('energyState');
let animationFrameId = null;
let lastRenderedAt = 0;
let lastInputAt = performance.now();
let wakeStartedAt = lastInputAt;
let windowFocused = true;
let sleeping = false;
let batteryMode = false;
let replayPlaying = false;
let appliedPixelRatio = renderer.getPixelRatio();
let displayedEnergyState = '';
function setEnergyState(mode, fps = 0) {
  const text = mode === 'sleep' ? 'спит · шевельни мышью' : `${mode} · ${Math.round(fps)} fps`;
  if (text === displayedEnergyState) return;
  displayedEnergyState = text;
  energyState.textContent = text;
  energyState.classList.toggle('bad', mode === 'sleep');
}
function applyEnergyPixelRatio(mode) {
  const activeRatio = Math.min(window.devicePixelRatio || 1, 1.5);
  const target = batteryMode || mode !== 'active' ? 1 : activeRatio;
  if (Math.abs(target - appliedPixelRatio) < 0.01) return;
  appliedPixelRatio = target;
  renderer.setPixelRatio(target);
  renderer.setSize(window.innerWidth, window.innerHeight);
  resizePostFX();
}
function desiredEnergyMode(now) {
  if (document.hidden || !windowFocused) return 'sleep';
  if (replayPlaying || dragging || nodeDragging || cameraTween) return 'active';
  const idleFor = now - lastInputAt;
  if (idleFor >= 30000) return 'sleep';
  if (idleFor >= 5000) return 'idle';
  return 'active';
}
function queueAnimationFrame() {
  if (!sleeping && animationFrameId === null) {
    animationFrameId = requestAnimationFrame(animate);
  }
}
function enterSleep() {
  if (sleeping) return;
  sleeping = true;
  if (animationFrameId !== null) cancelAnimationFrame(animationFrameId);
  animationFrameId = null;
  window.__GRAPHIFY_BRAIN_ENERGY_MODE = 'sleep';
  setEnergyState('sleep', 0);
  applyEnergyPixelRatio('sleep');
  // A DPR change resizes and clears the canvas. Present one final frame after
  // that resize, then leave it untouched for the whole sleep interval.
  renderFrame();
}
function wakeAnimation(resetInput = true) {
  if (resetInput) lastInputAt = performance.now();
  wakeStartedAt = performance.now();
  sleeping = false;
  lastRenderedAt = 0;
  queueAnimationFrame();
}
function markUserActivity() {
  wakeAnimation(true);
}
function targetFps(mode, now) {
  if (replayPlaying) return 30;
  if (mode === 'idle') return 10;
  if (quietMode) return 30;
  const ramp = Math.min(1, Math.max(0, (now - wakeStartedAt) / 300));
  return 10 + 50 * ramp;
}
function animate(time) {
  animationFrameId = null;
  const mode = desiredEnergyMode(time);
  if (mode === 'sleep') {
    enterSleep();
    return;
  }
  sleeping = false;
  const fps = targetFps(mode, time);
  const minimumInterval = fps >= 59 ? 0 : 1000 / fps;
  if (minimumInterval && time - lastRenderedAt < minimumInterval) {
    queueAnimationFrame();
    return;
  }
  lastRenderedAt = time;
  window.__GRAPHIFY_BRAIN_ENERGY_MODE = mode;
  applyEnergyPixelRatio(mode);
  setEnergyState(mode, fps);
  nodeMaterial.uniforms.uTime.value = time;
  haloMaterial.uniforms.uTime.value = time;
  nodeMaterial.uniforms.uMotion.value = motion ? 1 : 0;
  haloMaterial.uniforms.uMotion.value = motion ? 1 : 0;
  pulseMaterial.uniforms.uTime.value = time;
  envUniforms.uTime.value = time;
  nebula.rotation.y = time * 0.000004;
  if (cameraTween) {
    cameraTween.t += 1;
    const k = Math.min(1, cameraTween.t / cameraTween.dur);
    const ease = k < 0.5 ? 2 * k * k : 1 - Math.pow(-2 * k + 2, 2) / 2;
    const a = cameraTween.from, b = cameraTween.to;
    camera.position.x = a.px + (b.px - a.px) * ease;
    camera.position.y = a.py + (b.py - a.py) * ease;
    camera.position.z = a.pz + (b.pz - a.pz) * ease;
    root.rotation.x = a.rx + (b.rx - a.rx) * ease;
    root.rotation.y = a.ry + (b.ry - a.ry) * ease;
    if (k >= 1) cameraTween = null;
  }
  if (motion && !dragging && !nodeDragging && !cameraTween) root.rotation.y += 0.0012;
  if (mode === 'active') updateAtmosphere(time);
  renderFrame();
  renderLabels(false, mode, time);
  queueAnimationFrame();
}

for (const eventName of ['mousemove', 'keydown', 'wheel', 'touchstart', 'pointerdown']) {
  window.addEventListener(eventName, markUserActivity, { passive: true });
}
window.addEventListener('blur', () => {
  windowFocused = false;
  enterSleep();
});
window.addEventListener('focus', () => {
  windowFocused = true;
  wakeAnimation(true);
});
document.addEventListener('visibilitychange', () => {
  if (document.hidden) {
    closeLiveActivity();
    enterSleep();
  } else {
    windowFocused = true;
    openLiveActivity();
    wakeAnimation(true);
  }
});
window.addEventListener('beforeunload', closeLiveActivity);
if (navigator.getBattery) {
  navigator.getBattery().then(battery => {
    function applyBatteryState() {
      batteryMode = !battery.charging;
      if (batteryMode) {
        lastInputAt = performance.now() - 5001;
        wakeAnimation(false);
      } else {
        markUserActivity();
      }
    }
    battery.addEventListener('chargingchange', applyBatteryState);
    applyBatteryState();
  }).catch(() => {
    batteryMode = false;
  });
}

let postfxResizeTimer = 0;
window.addEventListener('resize', () => {
  camera.aspect = window.innerWidth / window.innerHeight;
  camera.updateProjectionMatrix();
  renderer.setSize(window.innerWidth, window.innerHeight);
  window.clearTimeout(postfxResizeTimer);
  postfxResizeTimer = window.setTimeout(resizePostFX, 150);
});

function applyHashSelection() {
  const hash = (location.hash || '').replace(/^#/, '');
  if (!hash) return;
  const params = new URLSearchParams(hash);
  const byId = params.get('node');
  const byIndex = params.get('n');
  let index = -1;
  if (byId && nodeById.has(byId)) index = nodeById.get(byId).index;
  else if (byIndex !== null && byIndex !== '' && Number.isFinite(Number(byIndex))) index = Number(byIndex);
  if (index >= 0 && index < DATA.nodes.length) selectNode(index);
}

// ---------- Historical growth + activity replay ----------
const timelineEl = document.getElementById('timeline');
const timeRange = document.getElementById('timeRange');
const timeLabel = document.getElementById('timeLabel');
const timePlay = document.getElementById('timePlay');
const timeMode = document.getElementById('timeMode');
const timeWindow = document.getElementById('timeWindow');
const timeSpeed = document.getElementById('timeSpeed');
const timeWho = document.getElementById('timeWho');
const timeHeat = document.getElementById('timeHeat');
const TIME_WINDOW_SECONDS = Object.freeze({
  '24h': 24 * 60 * 60,
  '7d': 7 * 24 * 60 * 60,
  '30d': 30 * 24 * 60 * 60,
  '180d': 180 * 24 * 60 * 60,
});
function growthTimelineBounds(nodes, windowValue, nowTimestamp) {
  const timestamps = nodes
    .map(node => Number(node.first_seen_ts))
    .filter(timestamp => Number.isFinite(timestamp) && timestamp > 0);
  if (!timestamps.length) return { start: 0, end: 0 };
  const knownStart = Math.min(...timestamps);
  const knownEnd = Math.max(...timestamps);
  const end = Math.max(knownEnd, Number(nowTimestamp) || knownEnd);
  const duration = TIME_WINDOW_SECONDS[windowValue];
  return {
    start: duration ? Math.max(knownStart, end - duration) : knownStart,
    end,
  };
}
function growthCutoffAt(value, bounds) {
  if (!(bounds.end > bounds.start)) return bounds.end;
  const position = Math.max(0, Math.min(1000, Number(value))) / 1000;
  return bounds.start + position * (bounds.end - bounds.start);
}
function growthNodeVisible(node, cutoff) {
  const timestamp = Number(node.first_seen_ts);
  return !Number.isFinite(timestamp) || timestamp <= 0 || cutoff === null || timestamp <= cutoff;
}
function growthVisibleCount(nodes, cutoff) {
  return nodes.reduce(
    (count, node) => count + (growthNodeVisible(node, cutoff) ? 1 : 0),
    0,
  );
}
let timelineMode = 'growth';
let growthBounds = growthTimelineBounds(DATA.nodes, timeWindow.value, DATA.stats.now_ts);
let replayEvents = [];
let replayAnimationId = 0;
let replayCursor = 0;
let replayVirtualTimestamp = 0;
let replayLastWallTime = 0;
function timelineNodeVisible(index) {
  return timelineMode !== 'growth' || growthNodeVisible(DATA.nodes[index], growthTimeCutoff);
}
function replayNodeIndex(event) {
  return resolveLiveNode(event)?.index ?? -1;
}
function replayTimestampLabel(timestamp) {
  if (!timestamp) return 'нет событий';
  return new Date(timestamp * 1000).toLocaleString('ru-RU', {
    month: '2-digit',
    day: '2-digit',
    hour: '2-digit',
    minute: '2-digit',
    second: '2-digit',
  });
}
function growthTimestampLabel(timestamp) {
  if (!timestamp) return 'древний слой';
  return new Date(timestamp * 1000).toLocaleString('ru-RU', {
    year: 'numeric',
    month: '2-digit',
    day: '2-digit',
    hour: '2-digit',
    minute: '2-digit',
  });
}
function updateHistoryHeat(events) {
  historyHeat.fill(0);
  const counts = new Map();
  const lobeCounts = new Map();
  events.forEach(event => {
    const index = replayNodeIndex(event);
    if (index < 0) return;
    const weight = Number(event.weight || 1);
    counts.set(index, (counts.get(index) || 0) + weight);
    const lobeId = DATA.nodes[index].brain_lobe;
    lobeCounts.set(lobeId, (lobeCounts.get(lobeId) || 0) + weight);
  });
  const maximum = Math.max(1, ...counts.values());
  counts.forEach((count, index) => {
    historyHeat[index] = Math.min(1, count / maximum);
  });
  const maximumLobe = Math.max(1, ...lobeCounts.values());
  lobeGroup.children.forEach(child => {
    if (!child.material || child.userData?.lobeId === 'hull') return;
    const baseOpacity = Number(child.userData.baseOpacity || 0.03);
    const intensity = (lobeCounts.get(child.userData.lobeId) || 0) / maximumLobe;
    child.material.opacity = historyHeatEnabled
      ? Math.min(0.24, baseOpacity + intensity * 0.14)
      : baseOpacity;
  });
}
function applyGrowthPosition(value) {
  growthBounds = growthTimelineBounds(DATA.nodes, timeWindow.value, DATA.stats.now_ts);
  const normalized = Math.max(0, Math.min(1000, Number(value)));
  const cutoff = growthCutoffAt(normalized, growthBounds);
  growthTimeCutoff = normalized >= 1000 ? null : cutoff;
  historyHeatEnabled = false;
  historyHeat.fill(0);
  timeHeat.classList.remove('active');
  focusSet.clear();
  highlightedEdgeIndices.clear();
  const visibleCount = growthVisibleCount(DATA.nodes, growthTimeCutoff);
  timeLabel.textContent = normalized >= 1000
    ? `полный · ${growthTimestampLabel(growthBounds.end)}`
    : `до ${growthTimestampLabel(cutoff)}`;
  timeWho.textContent = `нейроны: ${visibleCount}/${DATA.nodes.length}`;
  labelRankingDirty = true;
  refreshVisualState();
  renderLabels(true);
}
function applyActivityPosition(value, pulseLatest = false) {
  if (!replayEvents.length) {
    timeLabel.textContent = 'нет реальных событий';
    timeWho.textContent = 'агенты: —';
    historyHeat.fill(0);
    refreshVisualState();
    return;
  }
  const first = replayEvents[0].ts;
  const last = replayEvents[replayEvents.length - 1].ts;
  const cutoff = first + (Math.max(0, Math.min(1000, value)) / 1000) * Math.max(0, last - first);
  const visibleHistory = replayEvents.filter(event => event.ts <= cutoff);
  const recent = visibleHistory.filter(event => event.ts >= cutoff - 60).slice(-30);
  const moment = recent.length ? recent : visibleHistory.slice(-1);
  focusSet.clear();
  highlightedEdgeIndices.clear();
  const agents = new Set();
  moment.forEach(event => {
    const index = replayNodeIndex(event);
    if (index >= 0) focusSet.add(index);
    agents.add(event.agent);
  });
  if (pulseLatest && moment.length) {
    applyLiveVisualEvent({ ...moment[moment.length - 1], ts: Date.now() / 1000 });
  }
  timeLabel.textContent = replayTimestampLabel(cutoff);
  timeWho.textContent = `агенты: ${[...agents].join(', ') || '—'}`;
  updateHistoryHeat(historyHeatEnabled ? visibleHistory : []);
  refreshVisualState();
  renderLabels(true);
}
function stopReplay() {
  replayPlaying = false;
  if (replayAnimationId) cancelAnimationFrame(replayAnimationId);
  replayAnimationId = 0;
  timePlay.textContent = '▶';
}
function playGrowth() {
  if (!(growthBounds.end > growthBounds.start)) return;
  stopReplay();
  replayPlaying = true;
  timePlay.textContent = '⏸';
  let value = Number(timeRange.value);
  if (value >= 1000) value = 0;
  replayLastWallTime = performance.now();
  wakeAnimation(true);
  const normalizedRate = { 10: 25, 60: 90, 600: 400 }[
    Number(timeSpeed.value || 60)
  ] || 90;
  const step = now => {
    if (!replayPlaying) return;
    const elapsed = Math.min(250, Math.max(0, now - replayLastWallTime));
    replayLastWallTime = now;
    value = Math.min(1000, value + elapsed * 0.001 * normalizedRate);
    timeRange.value = String(Math.round(value));
    applyGrowthPosition(value);
    if (value >= 1000) {
      timeRange.value = '1000';
      applyGrowthPosition(1000);
      stopReplay();
      return;
    }
    replayAnimationId = requestAnimationFrame(step);
  };
  replayAnimationId = requestAnimationFrame(step);
}
function playActivityReplay() {
  if (replayEvents.length < 2) return;
  stopReplay();
  replayPlaying = true;
  timePlay.textContent = '⏸';
  const first = replayEvents[0].ts;
  const last = replayEvents[replayEvents.length - 1].ts;
  let value = Number(timeRange.value);
  if (value >= 1000) value = 0;
  replayVirtualTimestamp = first + (value / 1000) * (last - first);
  replayCursor = replayEvents.findIndex(event => event.ts >= replayVirtualTimestamp);
  if (replayCursor < 0) replayCursor = 0;
  replayLastWallTime = performance.now();
  wakeAnimation(true);
  const step = now => {
    if (!replayPlaying) return;
    const elapsed = Math.min(250, Math.max(0, now - replayLastWallTime));
    replayLastWallTime = now;
    replayVirtualTimestamp += elapsed * 0.001 * Number(timeSpeed.value || 60);
    let pulsed = false;
    while (
      replayCursor < replayEvents.length
      && replayEvents[replayCursor].ts <= replayVirtualTimestamp
    ) {
      pulsed = true;
      replayCursor += 1;
    }
    value = Math.min(1000, ((replayVirtualTimestamp - first) / Math.max(1, last - first)) * 1000);
    timeRange.value = String(Math.round(value));
    applyActivityPosition(value, pulsed);
    if (value >= 1000 || replayCursor >= replayEvents.length) {
      timeRange.value = '1000';
      applyActivityPosition(1000, true);
      stopReplay();
      return;
    }
    replayAnimationId = requestAnimationFrame(step);
  };
  replayAnimationId = requestAnimationFrame(step);
}
async function loadReplayHistory() {
  stopReplay();
  timeLabel.textContent = 'загрузка истории…';
  try {
    const response = await fetch(
      `/api/history?since=${encodeURIComponent(timeWindow.value)}&limit=5000`,
      { cache: 'no-cache' }
    );
    if (!response.ok) throw new Error(`history API ${response.status}`);
    const history = await response.json();
    replayEvents = history.events || [];
    window.__GRAPHIFY_BRAIN_HISTORY_COUNT = replayEvents.length;
    window.__GRAPHIFY_BRAIN_HISTORY = history;
    timeRange.value = '1000';
    applyActivityPosition(1000);
  } catch (_error) {
    replayEvents = [];
    timeLabel.textContent = 'история недоступна';
    timeWho.textContent = 'агенты: —';
  }
}
function applyTimelineMode() {
  stopReplay();
  timelineMode = timeMode.value;
  timelineEl.dataset.mode = timelineMode;
  timeRange.value = '1000';
  growthTimeCutoff = null;
  historyHeatEnabled = false;
  historyHeat.fill(0);
  timeHeat.classList.remove('active');
  timeHeat.disabled = timelineMode === 'growth';
  timeRange.setAttribute(
    'aria-label',
    timelineMode === 'growth' ? 'История роста мозга' : 'Реплей активности',
  );
  if (timelineMode === 'growth') {
    applyGrowthPosition(1000);
  } else {
    labelRankingDirty = true;
    refreshVisualState();
    loadReplayHistory();
  }
}
function applyTimelinePosition(value, pulseLatest = false) {
  if (timelineMode === 'growth') applyGrowthPosition(value);
  else applyActivityPosition(value, pulseLatest);
}
timeRange.addEventListener('input', () => {
  stopReplay();
  applyTimelinePosition(Number(timeRange.value));
});
document.getElementById('timeAll').onclick = () => {
  stopReplay();
  timeRange.value = '1000';
  applyTimelinePosition(1000);
};
timePlay.onclick = () => {
  if (replayPlaying) stopReplay();
  else if (timelineMode === 'growth') playGrowth();
  else playActivityReplay();
};
timeMode.addEventListener('change', applyTimelineMode);
timeWindow.addEventListener('change', () => {
  timeRange.value = '1000';
  if (timelineMode === 'growth') applyGrowthPosition(1000);
  else loadReplayHistory();
});
timeHeat.onclick = () => {
  if (timelineMode !== 'activity') return;
  historyHeatEnabled = !historyHeatEnabled;
  timeHeat.classList.toggle('active', historyHeatEnabled);
  applyActivityPosition(Number(timeRange.value));
};

// ---------- Saved camera views ----------
const VIEWS_KEY = 'ngb-views-v1';
const viewsEl = document.getElementById('views');
function loadViews() { try { return JSON.parse(localStorage.getItem(VIEWS_KEY) || '[]'); } catch (e) { return []; } }
function persistViews(list) { try { localStorage.setItem(VIEWS_KEY, JSON.stringify(list)); } catch (e) {} }
function currentView() {
  return { px: camera.position.x, py: camera.position.y, pz: camera.position.z, rx: root.rotation.x, ry: root.rotation.y };
}
function applyView(view) {
  cameraTween = { t: 0, dur: 48, from: currentView(), to: view };
}
function renderViews() {
  const list = loadViews();
  viewsEl.innerHTML = '';
  const save = document.createElement('div');
  save.className = 'view-chip view-save';
  save.textContent = '+ вид';
  save.title = 'Сохранить текущий ракурс';
  save.onclick = () => {
    const current = loadViews();
    current.push(Object.assign({ name: 'V' + (current.length + 1) }, currentView()));
    persistViews(current);
    renderViews();
  };
  viewsEl.appendChild(save);
  list.forEach((view, index) => {
    const chip = document.createElement('div');
    chip.className = 'view-chip';
    const name = document.createElement('span');
    name.textContent = view.name || ('V' + (index + 1));
    const del = document.createElement('span');
    del.className = 'view-del';
    del.textContent = '×';
    chip.appendChild(name);
    chip.appendChild(del);
    chip.onclick = (event) => { if (event.target !== del) applyView(view); };
    del.onclick = (event) => {
      event.stopPropagation();
      const current = loadViews();
      current.splice(index, 1);
      persistViews(current);
      renderViews();
    };
    viewsEl.appendChild(chip);
  });
}
renderViews();

/* ------------------------------------------------------------------ *
 * BRAIN ED PORT PATCHES (docs/e4-viewer-port-plan.md, tasks T1/T2).
 * Adapted from the Graphify brain viewer; see THIRD_PARTY_NOTICES.md.
 * Further E4C fixes are documented in the port plan and regression tests. *
 * ------------------------------------------------------------------ */
// The live activity daemon, the station API and write actions belong to the
// station, not to Brain ED (plan §3): their loaders become no-ops so the
// viewer takes its designed quiet-degradation path instead of a 404 on load.
loadLiveSnapshot = async function () {};
refreshStationStatus = async function () {};
scheduleStationStatusRefresh = function () {};
openLiveActivity = function () {};
closeLiveActivity = function () {};
postBrain = async function () {
  throw new Error('действия из вьюера недоступны в Brain ED');
};
['addBrain', 'openHome', 'openGraphNote'].forEach((id) => {
  const button = document.getElementById(id);
  if (button) {
    button.disabled = true;
    button.title = 'станционная функция, в Brain ED недоступна';
    button.onclick = () => {};
  }
});
// No station ops feed either (plan §3, task T9): the panel and its button
// stay in the DOM for the script's structure but are hidden and inert.
const opsToggle = document.getElementById('toggleOps');
if (opsToggle) {
  opsToggle.disabled = true;
  opsToggle.title = 'опер-панель станции, в Brain ED недоступна';
  opsToggle.onclick = () => {};
}
const opsPanelElement = document.getElementById('opsPanel');
if (opsPanelElement) opsPanelElement.style.display = 'none';

loading.style.display = 'none';
refreshVisualState();
renderLabels(true);
applyHashSelection();
window.addEventListener('hashchange', applyHashSelection);
loadLiveSnapshot().then(refreshStationStatus).finally(openLiveActivity);
applyTimelineMode();
window.__GRAPHIFY_BRAIN_READY = true;
queueAnimationFrame();
requestAnimationFrame(scheduleProgressiveBatch);
