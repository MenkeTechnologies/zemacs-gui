// Sync the embeddable GUI surface (menu bar + shortcuts + dialogs) from the shared zmax-gui-core
// submodule into the served frontend before each dev/build. Source of truth:
// crates/zemacs-gui-core/webui (no hand-edits to the copies in frontend/ — regenerated here and
// gitignored). index.html + main.js (the appShell host) and lib/zgui-core stay app-local; the
// terminal + editor come from zpwr-embed-terminal + the zmax sidecar. Mirrors copy-embed-terminal.
import { copyFileSync, existsSync } from "node:fs";
import { dirname, resolve } from "node:path";
import { fileURLToPath } from "node:url";

const here = dirname(fileURLToPath(import.meta.url));
const src = resolve(here, "../crates/zemacs-gui-core/webui");
const dst = resolve(here, "../frontend");

for (const f of ["menu.js", "zmax.css"]) {
  const from = resolve(src, f);
  if (!existsSync(from)) {
    console.error(`copy-webui: missing ${from} (run: git submodule update --init crates/zemacs-gui-core)`);
    process.exit(1);
  }
  copyFileSync(from, resolve(dst, f));
  console.log(`copy-webui: ${f}`);
}
