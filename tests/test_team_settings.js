// Exercises the same manifest view model as the settings window (no browser/network).
const fs = require('fs');
const vm = require('vm');
const assert = require('assert');
const html = fs.readFileSync(require('path').join(__dirname, '../brain-cli/assets/settings.html'), 'utf8');
const block = html.match(/<script id="team-panel-model">([\s\S]*?)<\/script>/);
assert(block);
const sandbox = { window: {} };
vm.createContext(sandbox);
vm.runInContext(block[1], sandbox);
const rows = sandbox.window.__BrainTeamPanel__.rows;
assert(rows(null).error);
assert.equal(rows({ manifests: [] }).items.length, 0);
const manifest = { space: 'team.p', machine_id: '7', project_id: 'p', enabled: false, revision: 2,
  classes: ['error'], disabled_fields: ['error.why_summary'], recipients: [{ machine: 'boss', agent: 'lead' }] };
const result = rows({ classes: ['error', 'plan'], fields: { error: ['what_summary', 'why_summary'] }, manifests: [{ manifest, ready: false,
  last_slice: { generated_at: 100 } }] });
assert(result.items[0].text.includes('boss:lead'));
assert(result.items[0].text.includes('выдача закрыта'));
assert(result.items[0].text.includes('1970-01-01T00:01:40.000Z'));
assert.equal(result.items[0].enabled, false);
assert.equal(result.items[0].classes[0].enabled, true);
assert.equal(result.items[0].classes[1].enabled, false);
assert.equal(result.items[0].classes[0].fields[0].enabled, true);
assert.equal(result.items[0].classes[0].fields[1].enabled, false);
assert(html.includes('description.textContent = item.text'));
assert(html.includes('"/api/team/toggle"'));
assert(html.includes('class: className, enabled: input.checked'));
assert(!block[1].includes('innerHTML'));
console.log('team manifest UI: defaults, recipients, time, class controls, safe text rendering OK');
