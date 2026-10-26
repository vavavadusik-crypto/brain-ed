/* Ported viewer page checks (docs/e4-viewer-port-plan.md, tasks T1/T2).
 *
 * The viewer itself is WebGL and needs a person at the screen; what a machine
 * can honestly verify is the contract: the page exists with the right title,
 * assets and token wrapper, and the ported script keeps the station's
 * invariants (offline, r124 vendor, no EffectComposer, port patches present).
 * Run: node tests/test_viewer_port.js
 */
const fs = require('fs');
const path = require('path');

const root = path.resolve(__dirname, '..');
const read = (rel) => fs.readFileSync(path.join(root, rel), 'utf8');

let failures = 0;
function check(name, condition, detail) {
  if (condition) {
    console.log('ok - ' + name);
  } else {
    failures += 1;
    console.error('FAIL - ' + name + (detail ? ': ' + detail : ''));
  }
}

const page = read('brain-cli/assets/viewer/brain.html');
const app = read('brain-cli/assets/viewer/app.js');
const three = read('brain-cli/assets/viewer/three.min.js');

// Task T1: the page, its assets and the invariants.
check('page title is New Generation Brain', page.includes('<title>New Generation Brain</title>'));
check('page loads the vendored three.js', page.includes('src="/vendor/three.min.js"'));
check('page loads the ported viewer module', page.includes('src="/brain-viewer/app.js"'));
check('page styles link the viewer css', page.includes('href="/brain-viewer/style.css"'));
check('three.js vendor is r124', three.includes('REVISION="124"'));
check('viewer never constructs EffectComposer', !app.includes('new EffectComposer'));
check('viewer never uses UnrealBloomPass', !app.includes('UnrealBloomPass'));
check('page has no external URL', !/https?:\/\//.test(page));
check('viewer script has no external URL', !/https?:\/\//.test(app));
check('viewer asks for links under the links key', app.includes('DATA.links'));

// Task T1: port patches exist and disable exactly the station-only loaders.
check('port patch block is present', app.includes('BRAIN ED PORT PATCHES'));
for (const name of ['loadLiveSnapshot', 'refreshStationStatus', 'openLiveActivity', 'closeLiveActivity', 'scheduleStationStatusRefresh', 'postBrain']) {
  check('port patch disables ' + name, new RegExp(name + '\\s*=\\s*(async\\s+)?function').test(app));
}
check('station buttons are disabled by the port', app.includes("['addBrain', 'openHome', 'openGraphNote']"));


// Task T9: the stats line is data-driven and the station-only ops surface
// stays hidden until Brain ED has endpoints for it.
check('stats line comes from DATA.stats', app.includes('DATA.stats.nodes'));
check('ops toggle is inert in the port', app.includes("opsToggle.disabled = true"));

// Task T2: the token wrapper stamps the window key and attaches it to fetch.
check('wrapper reads the #k fragment', /#k=\(\[0-9a-f\]\{64\}\)/.test(page) || page.includes('#k='));
check('wrapper stores the window key', page.includes('brain-ui-token'));
check('wrapper attaches the token header', page.includes('X-Brain-Token'));
check('wrapper keeps relative asset paths', page.includes('/brain-viewer/app.js'));

// The page must still be parseable script-wise.
const scripts = [...page.matchAll(/<script>([\s\S]*?)<\/script>/g)].map((m) => m[1]);
check('page has one inline wrapper script', scripts.length === 1, String(scripts.length));
new Function(scripts.join('\n')); // throws on syntax error
check('inline wrapper script parses', true);

if (failures) {
  console.error(String(failures) + ' check(s) failed');
  process.exit(1);
}
console.log('all viewer port checks passed');
