#!/usr/bin/env node
// G11: accessibility rules for the window pages, checked as text so the run
// needs no browser: language, tab order, focus visibility, control labels,
// button text, and text contrast of the theme variables in both themes.
const fs = require("fs");
const path = require("path");

const ASSETS = path.join(__dirname, "..", "brain-cli", "assets");
const PAGES = ["settings.html", "map.html", "explorer.html"];

let failures = 0;
function check(page, name, condition, detail) {
  if (condition) { console.log(`ok - ${page}: ${name}`); }
  else { failures += 1; console.error(`FAIL - ${page}: ${name}${detail ? " — " + detail : ""}`); }
}

// WCAG relative luminance / contrast ratio for #rrggbb colors.
function luminance(hex) {
  const c = hex.replace("#", "");
  const channels = [0, 2, 4].map((i) => parseInt(c.slice(i, i + 2), 16) / 255);
  const linear = (v) => (v <= 0.04045 ? v / 12.92 : Math.pow((v + 0.055) / 1.055, 2.4));
  return 0.2126 * linear(channels[0]) + 0.7152 * linear(channels[1]) + 0.0722 * linear(channels[2]);
}
function ratio(a, b) {
  const la = luminance(a), lb = luminance(b);
  return (Math.max(la, lb) + 0.05) / (Math.min(la, lb) + 0.05);
}

// Extract "var(--name)" colors from a CSS scope (a :root-like block) and
// resolve one level of indirection (e.g. --x: var(--y)).
function paletteOf(css) {
  const palette = {};
  for (const match of css.matchAll(/--([a-z0-9-]+):\s*([^;]+);/g)) {
    palette[match[1]] = match[2].trim();
  }
  const resolved = {};
  for (const [key, value] of Object.entries(palette)) {
    const indirect = value.match(/^var\(--([a-z0-9-]+)\)$/);
    if (indirect && palette[indirect[1]]) resolved[key] = palette[indirect[1]];
    else if (/^#[0-9a-fA-F]{6}$/.test(value)) resolved[key] = value;
  }
  return resolved;
}

for (const page of PAGES) {
  const html = fs.readFileSync(path.join(ASSETS, page), "utf8");

  check(page, "has <html lang>", /<html[^>]*\slang="/.test(html));

  // Tab order: positive tabindex is never used.
  check(page, "no positive tabindex", !/tabindex="[1-9]/.test(html));

  // Every text input has an accessible name: aria-label, aria-labelledby,
  // a <label for=id>, or a wrapping <label>.
  const controls = [...html.matchAll(/<(input|select|textarea)\b([^>]*)>/g)];
  let unnamed = 0;
  for (const match of controls) {
    const attrs = match[2];
    if (/type="(hidden|submit)"/.test(attrs)) continue;
    if (/aria-label|aria-labelledby/.test(attrs)) continue;
    const id = (attrs.match(/id="([^"]+)"/) || [])[1];
    if (id && html.includes(`for="${id}"`)) continue;
    // wrapped in a <label …> whose closing tag comes after this control?
    const before = html.slice(Math.max(0, match.index - 400), match.index);
    const openLabel = before.lastIndexOf("<label");
    const closeLabel = before.lastIndexOf("</label>");
    if (openLabel > closeLabel) continue;
    unnamed += 1;
  }
  check(page, `all ${controls.length} controls are named`, unnamed === 0,
    `${unnamed} unnamed`);

  // Buttons carry their own text.
  const buttons = [...html.matchAll(/<button\b([^>]*)>([\s\S]*?)<\/button>/g)];
  const empty = buttons.filter((b) => !b[2].replace(/<[^>]*>/g, "").trim()
    && !/aria-label=/.test(b[1]));
  check(page, `all ${buttons.length} buttons have text or aria-label`,
    empty.length === 0, `${empty.length} empty`);

  // Focus visibility: every outline:none must be paired with a :focus-visible
  // (or :focus) indicator rule in the same page.
  const stripped = html.replace(/outline:\s*none;?/g, "outline:none;");
  const hasOutlineNone = /outline:none;/.test(stripped);
  const hasFocusRule = /:focus-visible|:focus\b/.test(html);
  check(page, "focus indicators exist", !hasOutlineNone || hasFocusRule);
  check(page, "no legacy :focus outline suppression",
    !/\b(input|button|a|select|textarea)[^{\n]*:focus\s*\{\s*outline:\s*none/.test(html));

  // Text contrast of the theme variables, light and dark, >= 4.5:1.
  const lightBlock = html.slice(html.indexOf(":root"), html.indexOf("prefers-color-scheme"));
  const autoDark = html.slice(html.indexOf("prefers-color-scheme"));
  const darkBlock = (() => {
    const forced = html.indexOf('html[data-theme="dark"]');
    return html.slice(forced, forced + 400);
  })();
  const scopes = [["light", lightBlock], ["dark(auto)", autoDark], ["dark(forced)", darkBlock]];
  for (const [scopeName, block] of scopes) {
    const palette = paletteOf(block);
    const pairs = [
      ["ink", "bg"], ["ink", "panel"], ["dim", "panel"], ["accent", "panel"],
    ];
    for (const [fgKey, bgKey] of pairs) {
      if (!palette[fgKey] || !palette[bgKey]) continue;
      const value = ratio(palette[fgKey], palette[bgKey]);
      check(page, `${scopeName} --${fgKey} on --${bgKey} >= 4.5`, value >= 4.5,
        `${value.toFixed(2)} (${palette[fgKey]} on ${palette[bgKey]})`);
    }
  }

  // The primary button keeps an accessible label color in the dark theme
  // (white on the light-green dark accent was 1.72:1).
  if (/button\.primary\s*{[^}]*color:\s*#fff/.test(html)) {
    check(page, "dark theme re-colors .primary text",
      /html\[data-theme="dark"\]\s*button\.primary\s*{[^}]*color:\s*#(0|1)/.test(html));
  }
}

if (failures) {
  console.error(`${failures} check(s) failed`);
  process.exit(1);
}
console.log("a11y: all checks passed");
