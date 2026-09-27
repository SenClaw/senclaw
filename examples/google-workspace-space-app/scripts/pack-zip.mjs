// Pack the app into a self-contained installable ZIP.
//
// Uses the Next.js standalone output (`.next/standalone/server.js` + a minimal
// node_modules). SemaClaw's install-zip extracts it and launches it via the
// manifest `runtime.start` ("node server.js"). Layout of the ZIP root:
//   server.js, package.json, node_modules/, .next/ (server build),
//   .next/static/ (assets), public/, skills/, senclaw-manifest.json
import { cp, mkdir, rm, writeFile, readFile } from 'node:fs/promises';
import { existsSync } from 'node:fs';
import { dirname, join } from 'node:path';
import { fileURLToPath } from 'node:url';
import { execSync } from 'node:child_process';

const root = join(dirname(fileURLToPath(import.meta.url)), '..');
const stage = join(root, '.pack');
const standalone = join(root, '.next', 'standalone');

if (!existsSync(standalone)) {
  console.error('[pack] Missing .next/standalone — run `npm run build` first.');
  process.exit(1);
}

await rm(stage, { recursive: true, force: true });
await mkdir(stage, { recursive: true });

// Standalone server (server.js + package.json + minimal node_modules + .next).
await cp(standalone, stage, { recursive: true });
// Static assets + public are NOT in standalone — copy them next to server.js.
await cp(join(root, '.next', 'static'), join(stage, '.next', 'static'), { recursive: true });
if (existsSync(join(root, 'public'))) {
  await cp(join(root, 'public'), join(stage, 'public'), { recursive: true });
}
// Bundled skills installed alongside the app.
if (existsSync(join(root, 'skills'))) {
  await cp(join(root, 'skills'), join(stage, 'skills'), { recursive: true });
}

// Manifest: standalone has no npm scripts, so start with `node server.js`.
const manifest = JSON.parse(await readFile(join(root, 'senclaw-manifest.json'), 'utf8'));
manifest.runtime = { ...manifest.runtime, start: 'node server.js' };
await writeFile(join(stage, 'senclaw-manifest.json'), `${JSON.stringify(manifest, null, 2)}\n`);

const zip = join(root, 'google-workspace-space-app.zip');
await rm(zip, { force: true });
execSync(`cd "${stage}" && zip -r -q "${zip}" .`, { stdio: 'inherit' });
await rm(stage, { recursive: true, force: true });
console.log(`[pack] Wrote ${zip}`);
