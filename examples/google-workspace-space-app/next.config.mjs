import { dirname } from 'node:path';
import { fileURLToPath } from 'node:url';

const appDir = dirname(fileURLToPath(import.meta.url));

/**
 * Full Next.js server (NOT static export): one process serves the web UI, the
 * MCP route (`/mcp`), the `/health` probe, and a same-origin proxy to the
 * SemaClaw daemon (`/api/space/*`). SemaClaw launches it via the manifest
 * `runtime.start` command with an assigned `PORT`.
 *
 * `output: 'standalone'` produces a self-contained `.next/standalone/server.js`
 * (with a minimal node_modules) so the app can be shipped + installed as a ZIP.
 */
/** @type {import('next').NextConfig} */
const nextConfig = {
  output: 'standalone',
  outputFileTracingRoot: appDir,
};

export default nextConfig;
