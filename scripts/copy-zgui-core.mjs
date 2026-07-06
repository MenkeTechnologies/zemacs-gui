// Vendor the shared ZGui toolkit (zgui-core) into the served frontend at build time.
//
// Tauri serves static files from frontend/; the webview loads lib/zgui-core/webui/*.js as ordered
// classic <script> tags by relative path, and node_modules is not part of the served tree — so the
// toolkit must be copied in. The SOURCE is the pinned zgui-core in node_modules, installed as a pnpm
// git dependency ("zgui-core": "github:MenkeTechnologies/zgui-core#vX"). `pnpm install` refreshes it
// to the pinned tag; there is no committed copy and no per-app submodule pin to drift out of sync.
//
// zemacs-gui's npm project is the REPO ROOT (package.json + node_modules + scripts live there, not in
// app/), so paths resolve relative to the repo root — this script's dir (scripts/) is a child of it.
import { existsSync, rmSync, cpSync, mkdirSync } from "node:fs";
import { dirname, resolve } from "node:path";
import { fileURLToPath } from "node:url";

const root = resolve(dirname(fileURLToPath(import.meta.url)), "..");
const srcWebui = resolve(root, "node_modules", "zgui-core", "webui");
if (!existsSync(srcWebui)) {
  console.error(`copy-zgui-core: ${srcWebui} not found — run \`pnpm install\` (zgui-core is a git dependency)`);
  process.exit(1);
}
const dst = resolve(root, "frontend", "lib", "zgui-core", "webui");
rmSync(dst, { recursive: true, force: true });
mkdirSync(dst, { recursive: true });
cpSync(srcWebui, dst, { recursive: true });
console.log(`copy-zgui-core: ${srcWebui} -> ${dst}`);
