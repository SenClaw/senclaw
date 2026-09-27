/**
 * Node-only. How a Space App is started, stopped and configured by the daemon.
 *
 * Three things a Node Space App gets wrong on its own, and what this fixes:
 *
 * 1. **Which interface to bind.** A Space App authenticates nothing of its own.
 *    The daemon reaches it over `127.0.0.1` and its UI is same-origin, so
 *    `0.0.0.0` hands the whole REST + MCP surface to anyone on the network.
 *    Use {@link bindHost}. (Next.js in particular binds `0.0.0.0` unless you
 *    pass `-H`.)
 * 2. **SIGTERM.** A session app is stopped when it goes idle: SIGTERM to the
 *    process group, SIGKILL two seconds later. An app that ignores it loses
 *    whatever it had not flushed. Use {@link onShutdown}.
 * 3. **The manifest.** Every field the daemon does not understand is ignored
 *    silently — a misspelled `mode` turns an always-on app into an on-demand
 *    one with no warning anywhere. Use {@link defineManifest} /
 *    {@link validateManifest}.
 *
 * Import from a Node process only: `import { bindHost } from '@senclaw/space-sdk/lifecycle'`.
 */

export type RunMode = 'background' | 'session';
export type Runner = 'binary' | 'node' | 'python' | 'shell';
export type ReadMode = 'open' | 'strict' | 'allowlist';
export type NetworkMode = 'off' | 'all' | 'hosts';

/**
 * The interface this app may listen on: loopback unless the operator set
 * `SENCLAW_BIND_HOST` explicitly.
 */
export function bindHost(): string {
  return process.env.SENCLAW_BIND_HOST || '127.0.0.1';
}

/** The port the daemon assigned, from `PORT`. */
export function appPort(fallback?: number): number {
  const raw = (process.env.PORT || '').trim();
  if (/^\d+$/.test(raw)) return Number(raw);
  if (fallback) return fallback;
  throw new Error('PORT is not set and no fallback was given');
}

/** The app id the daemon launched this process under. */
export function appId(fallback?: string): string {
  const id = process.env.SENCLAW_SPACE_APP_ID || fallback;
  if (!id) {
    throw new Error(
      'SENCLAW_SPACE_APP_ID is not set. Run the app through SenClaw, or pass a fallback.',
    );
  }
  return id;
}

/**
 * Run `fn` when the daemon stops this app, then exit.
 *
 * The budget is about two seconds — after that the process group is SIGKILLed,
 * so anything still in flight is gone. Close the server and flush; do not start
 * new work.
 */
export function onShutdown(fn: () => void | Promise<void>, timeoutMs = 1500): void {
  let running = false;
  const handle = async (signal: NodeJS.Signals) => {
    if (running) return;
    running = true;
    console.log(`[senclaw] ${signal} — shutting down`);
    const timer = setTimeout(() => process.exit(0), timeoutMs);
    try {
      await fn();
    } catch (e) {
      console.error('[senclaw] shutdown handler failed:', e);
    } finally {
      clearTimeout(timer);
      process.exit(0);
    }
  };
  process.on('SIGTERM', handle);
  process.on('SIGINT', handle);
}

export interface RuntimeBlock {
  kind: 'server';
  /**
   * `background` for an app that does work nobody asked for at that moment —
   * polls a channel, runs a schedule, holds a WebSocket a browser extension
   * dials into. Everything else is `session` (the default): started when the
   * app is opened or one of its tools is called, stopped once idle.
   */
  mode?: RunMode;
  start: string;
  port?: number;
  healthPath?: string;
  runner?: Runner;
  /** Session apps only. Minimum 15s; 60s by default. */
  idleTimeoutSecs?: number;
  /** Run once after install/update, before the first launch (`npm ci`). */
  install?: string;
  venv?: boolean;
  [key: string]: unknown;
}

export interface RequiresBlock {
  /** A range: `>=18`, `^18`, `18.x`, `>=18 <21`. */
  node?: string;
  python?: string;
  /** Executables that must be on PATH — `ffmpeg`, `git`. */
  bin?: string[];
  optionalBin?: string[];
  env?: string[];
  optionalEnv?: string[];
  os?: Array<'macos' | 'linux' | 'windows'>;
}

export interface SandboxBlock {
  /** The user may not turn the sandbox off in Plugins → Space Apps. */
  force?: boolean;
  enabled?: boolean;
  readMode?: ReadMode;
  network?: NetworkMode;
  hosts?: string[];
  daemonApi?: boolean;
  loopback?: number[];
  folders?: Array<string | { path: string; readOnly?: boolean }>;
}

export interface SpaceManifest {
  id: string;
  name: string;
  description?: string;
  icon?: string;
  runtime?: RuntimeBlock;
  requires?: RequiresBlock;
  sandbox?: SandboxBlock;
  integration?: Record<string, unknown>;
  bridge?: Record<string, unknown>;
  mcp?: Record<string, unknown>;
  [key: string]: unknown;
}

const MODES: RunMode[] = ['background', 'session'];
const RUNNERS: Runner[] = ['binary', 'node', 'python', 'shell'];
const READ_MODES: ReadMode[] = ['open', 'strict', 'allowlist'];

/** Type-checks a manifest at authoring time and returns it unchanged. */
export function defineManifest(m: SpaceManifest): SpaceManifest {
  const problems = validateManifest(m);
  if (problems.length) {
    throw new Error(`invalid senclaw-manifest:\n  - ${problems.join('\n  - ')}`);
  }
  return m;
}

/** Problems in a manifest. Empty means the daemon will read what you meant. */
export function validateManifest(m: SpaceManifest): string[] {
  const problems: string[] = [];
  if (!m.id) problems.push('missing `id`');
  const rt = m.runtime;
  if (rt) {
    if (rt.kind === 'server' && !rt.start) {
      problems.push('runtime.kind is `server` but there is no `start` command');
    }
    if (rt.mode !== undefined && !MODES.includes(rt.mode)) {
      problems.push(
        `runtime.mode = ${JSON.stringify(rt.mode)} is not understood — it is treated as ` +
        `\`session\`, so an always-on app would silently stop when idle. Use ${MODES.join(' | ')}.`,
      );
    }
    if (rt.runner !== undefined && !RUNNERS.includes(rt.runner)) {
      problems.push(`runtime.runner must be one of ${RUNNERS.join(' | ')}`);
    }
    if (rt.idleTimeoutSecs !== undefined && rt.idleTimeoutSecs < 15) {
      problems.push('runtime.idleTimeoutSecs below 15 is clamped to 15 — a shorter window thrashes');
    }
  }
  const sb = m.sandbox;
  if (sb) {
    if (sb.network === 'hosts' && !(sb.hosts?.length)) {
      problems.push('sandbox.network is "hosts" but `hosts` is empty — the app gets no network');
    }
    if (sb.readMode !== undefined && !READ_MODES.includes(sb.readMode)) {
      problems.push(`sandbox.readMode must be one of ${READ_MODES.join(' | ')}`);
    }
  }
  const mcp = m.mcp as { autoRegister?: boolean; path?: string; url?: string } | undefined;
  if (mcp?.autoRegister && !mcp.path && !mcp.url) {
    problems.push('mcp.autoRegister is set but there is neither `path` nor `url`');
  }
  return problems;
}
