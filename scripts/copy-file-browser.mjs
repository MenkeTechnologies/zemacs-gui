// Sync the shared multi-pane file browser front end from the zpwr-file-browser submodule into the
// served frontend before each dev/build. Source of truth: crates/zpwr-file-browser/webui
// (file-browser.js + file-browser.css + file-browser.html). No hand-edits to the copies in frontend/ —
// they are regenerated here and gitignored. Mirrors copy-i18n.mjs / copy-embed-terminal.mjs.
import { copyFileSync, existsSync, mkdirSync, readdirSync } from "node:fs";
import { dirname, resolve } from "node:path";
import { fileURLToPath } from "node:url";

const here = dirname(fileURLToPath(import.meta.url));
const webui = resolve(here, "../crates/zpwr-file-browser/webui");
const dstFrontend = resolve(here, "../frontend");

for (const f of ["file-browser.js", "file-browser.css", "file-browser.html"]) {
  const from = resolve(webui, f);
  if (!existsSync(from)) {
    console.error(`copy-file-browser: missing ${from} (run: git submodule update --init crates/zpwr-file-browser)`);
    process.exit(1);
  }
  copyFileSync(from, resolve(dstFrontend, f));
  console.log(`copy-file-browser: ${f}`);
}

// NOTE: the file browser's per-locale catalogs are no longer copied here. Its fb.* keys were folded
// into the shared zpwr-i18n catalog (copy-i18n.mjs), so there is no separate lib/fb-i18n extraBase —
// the base catalog resolves the fb overlay's labels + toasts.
