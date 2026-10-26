/* The memory-settings draft logic (docs: E6.1 review items 1-3).
 *
 * The page keeps this logic in a DOM-free script block so this test can run
 * the very code the browser runs, inside a vm: only user-changed fields are
 * saved, clean fields always follow the server even while the block is
 * focused (the page guards the focused input's text separately), invalid
 * input is refused with a message at the field, and a failed save's message
 * survives until the next change.
 * Run: node tests/test_memory_settings.js
 */
const fs = require('fs');
const path = require('path');
const vm = require('vm');

const html = fs.readFileSync(
  path.join(__dirname, '..', 'brain-cli', 'assets', 'settings.html'),
  'utf8'
);
const match = html.match(/<script id="memory-draft">([\s\S]*?)<\/script>/);
if (!match) {
  console.error('FAIL - the memory-draft script block is missing from settings.html');
  process.exit(1);
}
const sandbox = { window: {} };
vm.createContext(sandbox);
vm.runInContext(match[1], sandbox);
const draft = sandbox.window.__BrainMemoryDraft__;

let failures = 0;
function check(name, condition, detail) {
  if (condition) {
    console.log('ok - ' + name);
  } else {
    failures += 1;
    console.error('FAIL - ' + name + (detail === undefined ? '' : ': ' + detail));
  }
}

// Review item 1: the payload carries only what the user changed.
draft.reset();
draft.adopt({ include_archive: true, archive_min_age_minutes: 120, session_start_notice: true });
draft.edit("include_archive", false);
check(
  'only the edited key is sent',
  JSON.stringify(draft.payload()) === '{"include_archive":false}',
  JSON.stringify(draft.payload())
);
check('a profile-only save sends no memory keys', (() => {
  draft.reset();
  draft.adopt({ include_archive: true, archive_min_age_minutes: 120, session_start_notice: true });
  return JSON.stringify(draft.payload()) === '{}';
})());

// Review item 2: clean fields adopt a foreign (CLI) change, dirty ones do not.
draft.reset();
draft.adopt({ include_archive: true, archive_min_age_minutes: 120, session_start_notice: true });
draft.edit("archive_min_age_minutes", 45);
const after = draft.adopt({ include_archive: false, archive_min_age_minutes: 999, session_start_notice: true });
check('clean field follows the server', after.include_archive === false);
check('dirty field keeps the user value', after.archive_min_age_minutes === 45);
check('dirty state survives adoption', draft.state.dirty.archive_min_age_minutes === true);

// After the server accepts the save, the saved fields follow the server again.
draft.accepted({ include_archive: false, archive_min_age_minutes: 60, session_start_notice: true });
check(
  'accepted save clears the dirty flag',
  draft.state.dirty.archive_min_age_minutes === undefined
);
check('accepted value is the server one', draft.state.values.archive_min_age_minutes === 60);

// Review item 3: invalid minutes are refused with a message and block saving.
draft.reset();
draft.adopt({ include_archive: true, archive_min_age_minutes: 120, session_start_notice: true });
check('minus five is refused', draft.edit("archive_min_age_minutes", -5) !== null);
check('one and a half is refused', draft.edit("archive_min_age_minutes", 1.5) !== null);
check('over the u32 ceiling is refused', draft.edit("archive_min_age_minutes", 4294967296) !== null);
check('an invalid field never reaches the payload', Object.keys(draft.payload()).length === 0);
check('an invalid field blocks saving', draft.hasErrors() === true);
check('the ceiling itself is accepted', draft.edit("archive_min_age_minutes", 4294967295) === null);
check('zero is accepted', draft.edit("archive_min_age_minutes", 0) === null);
draft.edit("archive_min_age_minutes", 30);
check('a later valid edit clears the error', draft.hasErrors() === false);
check(
  'the valid edit is in the payload',
  draft.payload().archive_min_age_minutes === 30
);
check('booleans reject non-boolean values', draft.edit("include_archive", "yes") !== null);

// Review item 4 (wording) is pinned as a grep: the label must say that
// records YOUNGER than N minutes are hidden, and 0 shows everything.
const page = html;
check(
  'the minutes label says younger records are hidden',
  page.includes('Прятать записи архива моложе')
);
check(
  'the hint says 0 shows records of any age',
  page.includes('показывать записи любого возраста')
);

if (failures) {
  console.error(String(failures) + ' check(s) failed');
  process.exit(1);
}
console.log('all memory settings draft checks passed');
