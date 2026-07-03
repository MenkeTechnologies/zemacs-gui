// Sync the shared multi-pane file browser front end from the zpwr-file-browser submodule into the
// served frontend before each dev/build. Source of truth: crates/zpwr-file-browser/webui
// (file-browser.js + file-browser.css) and crates/zpwr-file-browser/i18n (per-locale catalogs merged
// on top of the app catalog via bootI18n({ extraBases })). No hand-edits to the copies in frontend/ —
// they are regenerated here and gitignored. Mirrors copy-i18n.mjs / copy-embed-terminal.mjs.
import { copyFileSync, existsSync, mkdirSync, readdirSync } from "node:fs";
import { dirname, resolve } from "node:path";
import { fileURLToPath } from "node:url";

const here = dirname(fileURLToPath(import.meta.url));
const webui = resolve(here, "../crates/zpwr-file-browser/webui");
const dstFrontend = resolve(here, "../frontend");

for (const f of ["file-browser.js", "file-browser.css"]) {
  const from = resolve(webui, f);
  if (!existsSync(from)) {
    console.error(`copy-file-browser: missing ${from} (run: git submodule update --init crates/zpwr-file-browser)`);
    process.exit(1);
  }
  copyFileSync(from, resolve(dstFrontend, f));
  console.log(`copy-file-browser: ${f}`);
}

// The browser's own i18n catalogs land in lib/fb-i18n/, the extraBase initFileBrowser() registers.
const catSrc = resolve(here, "../crates/zpwr-file-browser/i18n");
if (!existsSync(catSrc)) {
  console.error(`copy-file-browser: missing ${catSrc} (run: git submodule update --init crates/zpwr-file-browser)`);
  process.exit(1);
}
const catDst = resolve(dstFrontend, "lib/fb-i18n");
mkdirSync(catDst, { recursive: true });
for (const f of readdirSync(catSrc).filter((f) => f.endsWith(".json"))) {
  copyFileSync(resolve(catSrc, f), resolve(catDst, f));
  console.log(`copy-file-browser: lib/fb-i18n/${f}`);
}
