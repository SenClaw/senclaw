import { copyFile } from 'node:fs/promises';
import { dirname, join } from 'node:path';
import { fileURLToPath } from 'node:url';

const root = join(dirname(fileURLToPath(import.meta.url)), '..');

await copyFile(join(root, 'senclaw-manifest.json'), join(root, 'out', 'senclaw-manifest.json'));
console.log('Copied senclaw-manifest.json to out/');
