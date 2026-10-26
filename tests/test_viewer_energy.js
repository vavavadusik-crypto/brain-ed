/* The ported viewer's energy model (docs/e4-viewer-port-plan.md, task T6).
 *
 * The render loop itself needs WebGL and a person at the screen; the decision
 * functions that drive it — desiredEnergyMode and targetFps — are plain code
 * over a handful of state flags. This test runs the functions actually shipped
 * in brain-cli/assets/viewer/app.js (extracted by source, not reimplemented)
 * inside a stubbed sandbox, and checks the contract: sleep when hidden or
 * unfocused, sleep after 30 s idle, 10 fps when idle, wake ramp 10 -> 60.
 * Run: node tests/test_viewer_energy.js
 */
const fs = require('fs');
const path = require('path');
const vm = require('vm');

const app = fs.readFileSync(
  path.join(process.env.VIEWER_SOURCE_ROOT || path.join(__dirname, '..'), 'brain-cli', 'assets', 'viewer', 'app.js'),
  'utf8'
);

let failures = 0;
function check(name, condition, detail) {
  if (condition) {
    console.log('ok - ' + name);
  } else {
    failures += 1;
    console.error('FAIL - ' + name + (detail === undefined ? '' : ': ' + detail));
  }
}

// Extract one function's source from the shipped file by brace matching.
function grab(name) {
  const marker = 'function ' + name + '(';
  const start = app.indexOf(marker);
  if (start < 0) throw new Error(name + ' not found in the ported app.js');
  const open = app.indexOf('{', start);
  let depth = 0;
  for (let index = open; index < app.length; index++) {
    if (app[index] === '{') depth += 1;
    if (app[index] === '}') {
      depth -= 1;
      if (depth === 0) return app.slice(start, index + 1);
    }
  }
  throw new Error(name + ' is not closed in the ported app.js');
}

function modeWith(overrides, now) {
  const state = Object.assign(
    {
      document: { hidden: false },
      windowFocused: true,
      replayPlaying: false,
      dragging: false,
      nodeDragging: false,
      hoveredIndex: -1,
      selectedIndex: -1,
      cameraTween: null,
      lastInputAt: 0,
      quietMode: false,
      wakeStartedAt: 0,
    },
    overrides
  );
  const sandbox = Object.assign({}, state, { performance: { now: () => now } });
  vm.createContext(sandbox);
  vm.runInContext(grab('desiredEnergyMode') + '\n' + grab('targetFps'), sandbox);
  return {
    mode: sandbox.desiredEnergyMode(now),
    fpsFor: (wakeAt) => {
      sandbox.wakeStartedAt = wakeAt;
      return sandbox.targetFps('active', now);
    },
    fps: (mode) => sandbox.targetFps(mode, now),
  };
}

// Task T6: the contract of the shipped decision functions.
check('hidden tab sleeps', modeWith({ document: { hidden: true } }, 1000).mode === 'sleep');
check('unfocused window sleeps', modeWith({ windowFocused: false }, 1000).mode === 'sleep');
check(
  'fresh input is active',
  modeWith({ lastInputAt: 500 }, 1000).mode === 'active'
);
check(
  'idle after five seconds',
  modeWith({ lastInputAt: 0 }, 6000).mode === 'idle'
);
check(
  'asleep after thirty seconds',
  modeWith({ lastInputAt: 0 }, 31000).mode === 'idle|sleep' ? false : modeWith({ lastInputAt: 0 }, 31000).mode === 'sleep'
);
check(
  'interaction keeps it active (drag)',
  modeWith({ dragging: true, lastInputAt: 0 }, 60000).mode === 'active'
);
check('idle renders at 10 fps', modeWith({}, 1000).fps('idle') === 10);
check('replay renders at 30 fps', modeWith({ replayPlaying: true }, 1000).fps('active') === 30);
check('quiet mode renders at 30 fps', modeWith({ quietMode: true }, 1000).fps('active') === 30);
const ramp = modeWith({}, 10_000);
check(
  'wake ramp starts near 10 fps',
  ramp.fpsFor(9995) < 15,
  String(ramp.fpsFor(9995))
);
check(
  'wake ramp reaches 60 fps after 300 ms',
  ramp.fpsFor(9600) === 60,
  String(ramp.fpsFor(9600))
);

// Exact boundaries and passive selection must not pin the renderer awake.
check('active before 5 seconds', modeWith({}, 4999).mode === 'active');
check('idle at 5 seconds', modeWith({}, 5000).mode === 'idle');
check('idle before 30 seconds', modeWith({}, 29999).mode === 'idle');
check('sleep at 30 seconds', modeWith({}, 30000).mode === 'sleep');
check('selected node sleeps at 30 seconds', modeWith({selectedIndex: 0}, 30000).mode === 'sleep');
check('hovered node sleeps at 30 seconds', modeWith({hoveredIndex: 0}, 30000).mode === 'sleep');
let entered = 0;
const loop = {animationFrameId: 7, desiredEnergyMode: () => 'sleep', enterSleep: () => { entered++; }};
vm.createContext(loop);
vm.runInContext(grab('animate'), loop);
loop.animate(30000);
check('animate enters sleep and returns without scheduling another frame', entered === 1 && loop.animationFrameId === null);

// The wiring around the functions: sleep enters on hidden/blur, input wakes.
for (const wiring of [
  ["hidden tab stops the loop", /if \(document\.hidden[^)]*\)\s*{\s*closeLiveActivity\(\);\s*enterSleep\(\);/],
  ["focus wakes the animation", /'focus', \(\) => \s*\{\s*windowFocused = true;\s*wakeAnimation\(true\);/],
  ["mouse movement marks activity", /'mousemove', 'keydown', 'wheel', 'touchstart', 'pointerdown'/],
  ["sleep cancels the frame", /sleeping = true;\s*\n\s*if \(animationFrameId !== null\) cancelAnimationFrame/],
  ["sleep state is published", /__GRAPHIFY_BRAIN_ENERGY_MODE = 'sleep'/],
  ["selecting a node wakes a sleeping viewer", /function selectNode\(index\) \{[^}]*?markUserActivity\(\);/],
]) {
  check(wiring[0], wiring[1].test(app));
}

// Task T9 additions: the station-only surfaces are hidden by the port.
check('ops button is disabled by the port', app.includes("getElementById('toggleOps')"));
check('ops panel is hidden by the port', app.includes("opsPanelElement.style.display = 'none'"));

if (failures) {
  console.error(String(failures) + ' check(s) failed');
  process.exit(1);
}
console.log('all viewer energy checks passed');
