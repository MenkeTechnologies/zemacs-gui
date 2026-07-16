// Stage the `zmax` editor binary as a Tauri externalBin sidecar so the shipped app is
// self-contained — zmax-gui must NOT depend on `zmax` being on the user's PATH. The editor source
// is vendored as the `crates/zemacs` submodule; this builds it (if needed) and copies the binary to
// app/src-tauri/binaries/zmax-<host-triple>, the suffixed name Tauri's `bundle.externalBin` requires.
// Wired into beforeDevCommand/beforeBuildCommand. The runtime resolver (sidecar.rs::resolve_zmax_bin)
// finds the sidecar beside the executable (or the dev staging dir) before falling back to PATH.
//
// Source: ZMAX_SIDECAR_BIN / ZMAX_BIN override → crates/zemacs/target/{release,debug}. If neither
// build exists it runs `cargo build --bin zmax` in the submodule (debug; set ZMAX_NO_BUILD=1 to
// skip and warn instead).
import { execFileSync } from 'node:child_process';
import { existsSync, copyFileSync, chmodSync, mkdirSync } from 'node:fs';
import { dirname, join } from 'node:path';
import { fileURLToPath } from 'node:url';

const root = join(dirname(fileURLToPath(import.meta.url)), '..');
const isWin = process.platform === 'win32';
const exeExt = isWin ? '.exe' : '';
const name = `zmax${exeExt}`;
const submodule = join(root, 'crates', 'zemacs');

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

function builtBinary() {
    for (const kind of ['release', 'debug']) {
        const p = join(submodule, 'target', kind, name);
        if (existsSync(p)) return p;
    }
    return null;
}

function resolveZmax() {
    const override = process.env.ZMAX_SIDECAR_BIN || process.env.ZMAX_BIN;
    if (override && existsSync(override)) return override;
    if (!existsSync(join(submodule, 'Cargo.toml'))) {
        console.error('prepare-zmax-sidecar: crates/zemacs submodule missing — run `git submodule update --init crates/zemacs`');
        return null;
    }
    // Always rebuild so the sidecar tracks the current submodule source. `localinstall` force-syncs
    // crates/zemacs to latest main, but the compiled binary from a prior commit lingers in target/ —
    // reusing it (the old bug) bundles a STALE editor. cargo is incremental: a no-op when the source
    // is unchanged, a real recompile when it advanced. Set ZMAX_NO_BUILD=1 to skip and reuse.
    if (!process.env.ZMAX_NO_BUILD) {
        console.log('prepare-zmax-sidecar: building crates/zemacs (cargo build --bin zmax)…');
        execFileSync('cargo', ['build', '--bin', 'zmax'], { cwd: submodule, stdio: 'inherit' });
        // Prefer the just-built debug binary over any older release binary left in target/.
        const dbg = join(submodule, 'target', 'debug', name);
        if (existsSync(dbg)) return dbg;
    }
    return builtBinary();
}

const triple = hostTriple();
if (!triple) {
    console.error('prepare-zmax-sidecar: could not determine host triple (rustc missing?)');
    process.exit(1);
}

const src = resolveZmax();
if (!src) {
    console.warn(
        'prepare-zmax-sidecar: zmax not built. Sidecar NOT bundled; the app falls back to a ' +
            'system zmax at runtime. Build it (`cargo build --bin zmax` in crates/zemacs) or set ZMAX_SIDECAR_BIN.'
    );
    process.exit(0);
}

const outDir = join(root, 'app', 'src-tauri', 'binaries');
mkdirSync(outDir, { recursive: true });
const out = join(outDir, `zmax-${triple}${exeExt}`);
copyFileSync(src, out);
if (!isWin) chmodSync(out, 0o755);
console.log(`Staged zmax sidecar: ${src} -> ${out}`);
