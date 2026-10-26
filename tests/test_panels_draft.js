#!/usr/bin/env node
// E6.4: the draft/confirm logic of the agents and privacy panels must be
// testable without a browser: the page keeps it in a DOM-free script block.
const fs = require("fs");
const path = require("path");
const vm = require("vm");

const html = fs.readFileSync(path.join(__dirname, "..", "brain-cli", "assets", "settings.html"), "utf8");
const match = html.match(/<script id="panels-draft">([\s\S]*?)<\/script>/);
if (!match) {
  console.error("panels-draft script block not found in settings.html");
  process.exit(1);
}
const sandbox = { window: {} };
vm.createContext(sandbox);
vm.runInContext(match[1], sandbox);
const draft = sandbox.window.__BrainPanelsDraft__;
if (!draft) {
  console.error("window.__BrainPanelsDraft__ not exported");
  process.exit(1);
}

let failures = 0;
function check(name, condition) {
  if (condition) { console.log("ok - " + name); }
  else { failures += 1; console.error("FAIL - " + name); }
}

// --- agent draft: nothing without the explicit confirm ----------------------
check("unknown agent is refused", draft.selectAgent("unknown-agent", "connect") !== null);
check("invalid action is refused", draft.selectAgent("claude", "delete") !== null);
check("valid selection returns no error", draft.selectAgent("claude", "connect") === null);
check("no confirm means no payload", draft.agentPayload() === null);
check("confirm=false means no payload", draft.agentPayload(false) === null);
check("confirmation without a successful preview cannot apply", draft.agentPayload(true) === null);
const oldSelection = draft.agent.draft;
draft.selectAgent("codex", "connect");
check("late preview of another agent is rejected", !draft.acceptAgentPreview(oldSelection, {agent: "claude", revision: "test-revision", plans: [{changed: true}]}));
draft.selectAgent("claude", "connect");
check("conflicts cannot be confirmed", !draft.acceptAgentPreview(draft.agent.draft, {agent: "claude", revision: "test-revision", plans: [{changed: true, conflict: "drift"}]}));
check("successful preview can be confirmed", draft.acceptAgentPreview(draft.agent.draft, {agent: "claude", revision: "test-revision", plans: [{changed: true}]}));
check("confirm=true builds the payload", JSON.stringify(draft.agentPayload(true))
  === JSON.stringify({ agent: "claude", action: "connect", confirm: true, preview_revision: "test-revision" }));
draft.clearAgent();
check("cleared draft has no payload", draft.agentPayload(true) === null);

// --- privacy draft: same rules as the memory draft ---------------------------
draft.resetPrivacy();
const server = { enabled: true, since_days: 14, roles: ["user", "assistant"], clients: ["claude", "codex"] };
draft.adoptPrivacy(server);
check("clean state carries no changes", !draft.hasPrivacyChanges());

check("a subset of roles is accepted", draft.editPrivacy("roles", ["user"]) === null);
check("only the edited field is in the payload",
  JSON.stringify(draft.privacyPayload()) === JSON.stringify({ roles: ["user"] }));

check("empty roles are refused", draft.editPrivacy("roles", []) !== null);
check("duplicated roles are refused", draft.editPrivacy("roles", ["user", "user"]) !== null);
check("unknown roles are refused", draft.editPrivacy("roles", ["user", "root"]) !== null);
check("an invalid field never reaches the payload",
  JSON.stringify(draft.privacyPayload()) === JSON.stringify({}));

check("enabled must be boolean", draft.editPrivacy("enabled", "yes") !== null);
check("boolean enabled is accepted", draft.editPrivacy("enabled", false) === null);

check("since_days 0 is refused", draft.editPrivacy("since_days", 0) !== null);
check("since_days 1.5 is refused", draft.editPrivacy("since_days", 1.5) !== null);
check("since_days 3650 is accepted", draft.editPrivacy("since_days", 3650) === null);
check("since_days 3651 is refused", draft.editPrivacy("since_days", 3651) !== null);

check("unknown client is refused", draft.editPrivacy("clients", ["claude", "team"]) !== null);
check("a valid client list is accepted", draft.editPrivacy("clients", ["hermes"]) === null);

// an invalid field keeps its error until the next edit, like the memory draft
check("the invalid since_days keeps its error", draft.editPrivacy("since_days", 20) === null);
draft.acceptedPrivacy(server);
check("accepted clears the dirty fields", !draft.hasPrivacyChanges());
check("clean fields follow the server again", draft.privacy.values.since_days === 14);

// dirty fields survive adoption (the refresh must not overwrite editing)
draft.editPrivacy("since_days", 30);
draft.adoptPrivacy({ enabled: true, since_days: 14, roles: ["user"], clients: ["claude"] });
check("a dirty field keeps the user's value during refresh", draft.privacy.values.since_days === 30);

// --- the page wording the review asked for -----------------------------------
check("the page says turning a role off keeps stored rows",
  html.includes("уже сохранённое не удаляется"));
check("the page says nothing is written without confirmation",
  html.includes("ничего не пишется"));

if (failures) {
  console.error(failures + " check(s) failed");
  process.exit(1);
}
console.log("panels draft: all checks passed");
