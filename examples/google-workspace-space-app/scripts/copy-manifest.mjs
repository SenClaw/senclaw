// Publish the manifest so the running Next.js server serves it at
// `/senclaw-manifest.json` (Next serves the `public/` folder at the root).
import { copyFile, mkdir } from 'node:fs/promises';
import { dirname, join } from 'node:path';
import { fileURLToPath } from 'node:url';

const root = join(dirname(fileURLToPath(import.meta.url)), '..');
await mkdir(join(root, 'public'), { recursive: true });
await copyFile(join(root, 'senclaw-manifest.json'), join(root, 'public', 'senclaw-manifest.json'));
console.log('Copied senclaw-manifest.json to public/');
