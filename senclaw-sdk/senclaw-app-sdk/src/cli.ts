#!/usr/bin/env node
/**
 * `npx senclaw-manifest <senclaw-manifest.json>` — the Node twin of
 * `python -m senclaw_space.manifest`.
 *
 * Worth having as a binary rather than only a function: the manifest mistakes
 * that matter are the silent ones (a misspelled `runtime.mode` falls back to
 * `session`, so an app that must poll a channel quietly stops after a minute),
 * and a check you can run in CI without writing a script is a check that gets
 * run.
 */
import { readFileSync } from 'node:fs';
import { validateManifest, type RuntimeBlock, type SpaceManifest } from './lifecycle.js';

function main(argv: string[]): number {
  const files = argv.filter(a => !a.startsWith('-'));
  if (files.length === 0) {
    process.stderr.write('usage: senclaw-manifest <senclaw-manifest.json> [...]\n');
    return 2;
  }
  let failed = 0;
  for (const file of files) {
    let manifest: SpaceManifest;
    try {
      manifest = JSON.parse(readFileSync(file, 'utf8')) as SpaceManifest;
    } catch (err) {
      process.stdout.write(`✗ ${file}: ${err instanceof Error ? err.message : String(err)}\n`);
      failed++;
      continue;
    }
    const problems = validateManifest(manifest);
    if (problems.length) {
      failed++;
      for (const p of problems) process.stdout.write(`✗ ${file}: ${p}\n`);
    } else {
      const rt: Partial<RuntimeBlock> = manifest.runtime ?? {};
      process.stdout.write(
        `✓ ${manifest.id}: mode=${rt.mode ?? 'session'} runner=${rt.runner ?? 'auto'}\n`,
      );
    }
  }
  return failed ? 1 : 0;
}

process.exitCode = main(process.argv.slice(2));
