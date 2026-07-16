// Stage the `stryke` runtime as a Tauri externalBin sidecar so the shipped app is self-contained.
// stryke is NOT vendored as source (it is a large separate language repo) — instead this pulls the
// prebuilt binary for the host platform from the latest strykelang GitHub release and copies it to
// app/src-tauri/binaries/stryke-<host-triple>, the name Tauri's `bundle.externalBin` requires. Wired
// into beforeDevCommand/beforeBuildCommand. The runtime resolver (sidecar.rs::resolve_stryke_bin) finds
// the sidecar beside the executable before falling back to PATH.
//
// Source: STRYKE_SIDECAR_BIN / STRYKE_BIN override → latest GitHub release asset for the host triple
// (cached by release tag, so repeat builds don't re-download) → PATH / ~/.cargo/bin / Homebrew if the
// download fails (offline). A machine with none of these skips the sidecar with a warning.
import { execFileSync } from 'node:child_process';
import { existsSync, copyFileSync, chmodSync, mkdirSync, readFileSync, writeFileSync, readdirSync, statSync, rmSync } from 'node:fs';
import { dirname, join } from 'node:path';
import { tmpdir } from 'node:os';
import { fileURLToPath } from 'node:url';

const REPO = 'MenkeTechnologies/strykelang';
const root = join(dirname(fileURLToPath(import.meta.url)), '..');
const isWin = process.platform === 'win32';
const exeExt = isWin ? '.exe' : '';
const name = `stryke${exeExt}`;
const outDir = join(root, 'app', 'src-tauri', 'binaries');

function hostTriple() {
    try {
        const t = execFileSync('rustc', ['--print', 'host-tuple'], { encoding: 'utf8' }).trim();
        if (t) return t;
    } catch {}
    try {
        const v = execFileSync('rustc', ['-vV'], { encoding: 'utf8' });
        const m = v.match(/host:\s*(\S+)/);
        if (m) return m[1];
    } catch {}
    return null;
}

function stageFrom(src, out) {
    copyFileSync(src, out);
    if (!isWin) chmodSync(out, 0o755);
    console.log(`Staged stryke sidecar: ${src} -> ${out}`);
}

function fromPath() {
    const sep = isWin ? ';' : ':';
    for (const d of (process.env.PATH || '').split(sep)) if (d && existsSync(join(d, name))) return join(d, name);
    const home = process.env.HOME || process.env.USERPROFILE || '';
    for (const c of [home && join(home, '.cargo', 'bin', name), '/opt/homebrew/bin/stryke', '/usr/local/bin/stryke'].filter(Boolean)) {
        if (existsSync(c)) return c;
    }
    return null;
}

// Find the `stryke` executable inside an extracted release directory (it may sit in a subfolder).
function findStryke(dir) {
    for (const ent of readdirSync(dir, { withFileTypes: true })) {
        const p = join(dir, ent.name);
        if (ent.isDirectory()) { const hit = findStryke(p); if (hit) return hit; }
        else if (ent.name === name) return p;
    }
    return null;
}

const triple = hostTriple();
if (!triple) {
    console.error('prepare-stryke-sidecar: could not determine host triple (rustc missing?)');
    process.exit(1);
}
mkdirSync(outDir, { recursive: true });
const out = join(outDir, `stryke-${triple}${exeExt}`);

// 1. explicit override
const override = process.env.STRYKE_SIDECAR_BIN || process.env.STRYKE_BIN;
if (override && existsSync(override)) {
    stageFrom(override, out);
    process.exit(0);
}

// 2. latest GitHub release asset for this triple (cached by tag)
const marker = join(outDir, '.stryke-release');
try {
    // Authenticate the API call when a token is available (CI) to avoid the 60/hr unauthenticated
    // rate limit; the asset download itself is from the public release CDN and needs no auth.
    const ghHeaders = { 'User-Agent': 'zmax-gui-build', Accept: 'application/vnd.github+json' };
    const token = process.env.GITHUB_TOKEN || process.env.GH_TOKEN;
    if (token) ghHeaders.Authorization = `Bearer ${token}`;
    const rel = await (await fetch(`https://api.github.com/repos/${REPO}/releases/latest`, {
        headers: ghHeaders,
    })).json();
    const tag = rel.tag_name;
    const want = `${tag}:${triple}`;
    if (existsSync(out) && existsSync(marker) && readFileSync(marker, 'utf8') === want) {
        console.log(`prepare-stryke-sidecar: stryke ${tag} (${triple}) already staged`);
        process.exit(0);
    }
    const asset = (rel.assets || []).find((a) => a.name.includes(triple) && /\.(tar\.gz|tgz)$/.test(a.name));
    if (!asset) throw new Error(`no release asset for ${triple} in ${tag}`);

    const work = join(tmpdir(), `stryke-dl-${process.pid}`);
    mkdirSync(work, { recursive: true });
    const tarball = join(work, asset.name);
    execFileSync('curl', ['-fsSL', asset.browser_download_url, '-o', tarball]);
    execFileSync('tar', ['-xzf', tarball, '-C', work]);
    const bin = findStryke(work);
    if (!bin) throw new Error(`stryke binary not found inside ${asset.name}`);
    stageFrom(bin, out);
    writeFileSync(marker, want);
    rmSync(work, { recursive: true, force: true });
    console.log(`prepare-stryke-sidecar: pulled stryke ${tag} (${triple}) from GitHub release`);
    process.exit(0);
} catch (e) {
    console.warn(`prepare-stryke-sidecar: GitHub release fetch failed (${e.message}); falling back to a local stryke`);
}

// 3. offline fallback: a stryke already on the machine
const local = fromPath();
if (local) {
    stageFrom(local, out);
    process.exit(0);
}
console.warn(
    'prepare-stryke-sidecar: stryke not found (release download failed and none on PATH/~/.cargo/bin/Homebrew). ' +
        'Sidecar NOT bundled; the app falls back to a system stryke at runtime. Set STRYKE_SIDECAR_BIN to bundle one.'
);
process.exit(0);
