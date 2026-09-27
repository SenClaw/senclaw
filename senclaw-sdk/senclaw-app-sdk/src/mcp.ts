/**
 * SenclawSpace MCP server harness (Node-only).
 *
 * Turns a Space App into a local MCP server that SenClaw can register and call.
 * It bakes in everything a Space App needs to expose itself over MCP:
 *
 *   - Streamable HTTP transport (stateless JSON) bound to loopback
 *   - An `Accept`-header normalization shim so the CURRENT SenClaw Rust MCP
 *     client (which sends no `Accept` header) interoperates with the strict
 *     MCP TypeScript SDK transport (which would otherwise return HTTP 406)
 *   - Origin validation (DNS-rebinding protection)
 *   - Optional built-in `*_get_settings` / `*_set_settings` tools backed by the
 *     SenClaw config KV (read/write settings)
 *   - Custom tool registration hook
 *   - Optional self-registration back to SenClaw on startup
 *
 * Import only from a Node process: `import { serveSpaceMcp } from '@senclaw/space-sdk/mcp'`.
 */

import { McpServer } from '@modelcontextprotocol/sdk/server/mcp.js';
import { StreamableHTTPServerTransport } from '@modelcontextprotocol/sdk/server/streamableHttp.js';
import type { CallToolResult } from '@modelcontextprotocol/sdk/types.js';
import { timingSafeEqual } from 'node:crypto';
import express, { type Request, type Response } from 'express';
import { z, type ZodRawShape } from 'zod';

import { SenclawSpace } from './index.js';

export { z } from 'zod';
export type { CallToolResult } from '@modelcontextprotocol/sdk/types.js';
export type { McpServer } from '@modelcontextprotocol/sdk/server/mcp.js';

export enum ResponseFormat {
  MARKDOWN = 'markdown',
  JSON = 'json',
}

/** Settings tool configuration. Enables `<prefix>_get_settings` / `<prefix>_set_settings`. */
export interface SpaceMcpSettings<T extends Record<string, unknown>> {
  /** Config KV key the settings live under (shared with the app UI). */
  key: string;
  /** Defaults returned when nothing is stored yet. */
  defaults: T;
  /** Coerce an arbitrary stored/incoming value into a valid settings object. */
  normalize?: (value: unknown) => T;
  /**
   * Optional Zod shape describing the writable fields of `set_settings`. When
   * given, the tool validates input per-field; otherwise it accepts a free-form
   * `patch` object that is shallow-merged over the current settings.
   */
  patchSchema?: ZodRawShape;
}

/** Context handed to the custom `registerTools` hook. */
export interface SpaceMcpContext {
  /** Low-level MCP server — call `.registerTool(...)` to add tools. */
  server: McpServer;
  /** SenclawSpace client pointed at the daemon (config KV, sqlite, core, ...). */
  space: SenclawSpace;
  /** Read the app settings (defaults applied). Only set when `settings` is configured. */
  getSettings?: () => Promise<Record<string, unknown>>;
  /** Persist the app settings. Only set when `settings` is configured. */
  setSettings?: (next: Record<string, unknown>) => Promise<Record<string, unknown>>;
}

export interface ServeSpaceMcpOptions<T extends Record<string, unknown> = Record<string, unknown>> {
  /** Space App id (manifest `id`). */
  appId: string;
  /** Absolute SenClaw daemon UI base URL. Default `http://127.0.0.1:18788`. */
  baseUrl?: string;
  /** Port to listen on. Default `4107`. */
  port?: number;
  /** HTTP path for the transport. Default `/mcp`. */
  mcpPath?: string;
  /** MCP server name. Default `${appId}-mcp-server`. */
  name?: string;
  /** MCP server version. Default `1.0.0`. */
  version?: string;
  /** Tool name prefix. Default derived from `appId` (non-alphanumerics → `_`). */
  toolPrefix?: string;
  /** Enable built-in settings tools (read/write) over the config KV. */
  settings?: SpaceMcpSettings<T>;
  /** Register custom tools. Called once per request-scoped server instance. */
  registerTools?: (ctx: SpaceMcpContext) => void;
  /** Register this server back to SenClaw on startup (transport `http`). */
  autoRegister?: boolean;
  /** Human-readable description used in the self-registration. */
  description?: string;
  /**
   * Refuse any request that does not carry this app's access token — see
   * {@link requireAppToken}. `/health` is exempt automatically; add anything
   * else a client dials directly with `authSkipPaths`.
   *
   * Off by default: turning it on breaks callers that talk to the port without
   * going through the daemon, and that is a decision per app rather than a
   * silent upgrade.
   */
  requireAppToken?: boolean;
  /** Extra paths exempt from `requireAppToken` (exact, or a `/*` prefix). */
  authSkipPaths?: string[];
}

/** Running server handle. */
export interface SpaceMcpHandle {
  port: number;
  mcpPath: string;
  url: string;
  /** Stop listening. */
  close: () => Promise<void>;
}

const DEFAULT_BASE_URL = 'http://127.0.0.1:18788';
const DEFAULT_PORT = 4107;
const DEFAULT_MCP_PATH = '/mcp';

const responseFormatField = z
  .nativeEnum(ResponseFormat)
  .default(ResponseFormat.MARKDOWN)
  .describe("Output format: 'markdown' for human-readable or 'json' for machine-readable");

function sanitizePrefix(appId: string): string {
  return appId.replace(/[^a-z0-9]+/gi, '_').replace(/^_+|_+$/g, '') || 'space';
}

function ok(structured: Record<string, unknown>, text: string): CallToolResult {
  return { content: [{ type: 'text', text }], structuredContent: structured };
}

function fail(error: unknown): CallToolResult {
  const message = error instanceof Error ? error.message : String(error);
  return { isError: true, content: [{ type: 'text', text: `Error: ${message}` }] };
}

function settingsMarkdown(settings: Record<string, unknown>, heading: string): string {
  const lines = [`# ${heading}`, ''];
  for (const [key, value] of Object.entries(settings)) {
    lines.push(`- **${key}**: ${Array.isArray(value) ? value.join(', ') : String(value)}`);
  }
  return lines.join('\n');
}

/**
 * Express middleware that refuses any request not carrying this app's access
 * token, closing the app's own API to everything except the SenClaw daemon.
 *
 * An app's REST and MCP endpoints have no authentication of their own: the port
 * is open to every process on the machine, and the app id in a URL is public.
 * With this mounted, the only caller that gets through is the daemon — its
 * proxy stamps the token on every request it forwards (the UI iframe, the app's
 * own fetches, MCP tool calls) — and a direct hit on the port answers 401.
 *
 * Two things are deliberately not refused:
 *
 * - **No token in the environment.** That is what a bare `npm start` outside
 *   SenClaw looks like, and 401ing every request including the daemon's health
 *   check would turn "no token issued" into "app permanently down".
 * - **`skip` paths** (exact, or a trailing `/*` prefix). Pass the health path
 *   and anything a browser extension dials directly — the daemon's health check
 *   decides whether the app started, and it runs before anything is proxied.
 *
 * ```ts
 * app.use(requireAppToken({ skip: ['/health', '/public/*'] }));
 * ```
 */
export function requireAppToken(opts: { token?: string; skip?: string[] } = {}) {
  const token = (opts.token ?? process.env.SENCLAW_TOKEN_ACCESS_APP ?? '').trim();
  const skip = opts.skip ?? [];
  return (req: Request, res: Response, next: () => void): void => {
    if (!token) return next();
    const path = req.path || req.url.split('?')[0];
    for (const pattern of skip) {
      if (!pattern) continue;
      if (pattern.endsWith('/*') ? path.startsWith(pattern.slice(0, -1)) : path === pattern) {
        return next();
      }
    }
    const presented =
      String(req.headers['x-senclaw-app-token'] ?? '').trim() ||
      String((req.query as Record<string, unknown>)?.app_token ?? '').trim();
    // Constant time: comparing secrets with === leaks the matched prefix
    // length through timing.
    const a = Buffer.from(presented);
    const b = Buffer.from(token);
    if (a.length !== b.length || !timingSafeEqual(a, b)) {
      res.status(401).json({
        error: 'this app only answers requests from the SenClaw daemon',
        code: 'app_token_required',
      });
      return;
    }
    next();
  };
}

/**
 * Only accept requests with no Origin (typical for non-browser MCP clients such
 * as the SenClaw Rust client) or a loopback Origin.
 */
function originAllowed(origin: string | undefined): boolean {
  if (!origin) return true;
  try {
    const host = new URL(origin).hostname;
    return host === '127.0.0.1' || host === 'localhost' || host === '::1';
  } catch {
    return false;
  }
}

/** Build a settings read/write helper pair bound to a SenclawSpace + config. */
function makeSettingsAccessors<T extends Record<string, unknown>>(
  space: SenclawSpace,
  cfg: SpaceMcpSettings<T>
) {
  const normalize = cfg.normalize ?? ((v: unknown) => ({ ...cfg.defaults, ...(v as object) }) as T);
  const get = async (): Promise<T> => normalize(await space.getConfig(cfg.key));
  const set = async (next: T): Promise<T> => normalize(await space.setConfig(cfg.key, next));
  return { normalize, get, set };
}

/** Register the built-in settings tools onto a server instance. */
function registerSettingsTools<T extends Record<string, unknown>>(
  server: McpServer,
  prefix: string,
  space: SenclawSpace,
  cfg: SpaceMcpSettings<T>
): SpaceMcpContext {
  const { get, set } = makeSettingsAccessors(space, cfg);

  server.registerTool(
    `${prefix}_get_settings`,
    {
      title: 'Get Settings',
      description: `Read the saved settings for the "${space.env.appId}" Space App from SenClaw.

Returns the persisted configuration (defaults applied when nothing is stored yet). Read-only.

Args:
  - response_format ('markdown' | 'json'): Output format (default: 'markdown').

Returns (JSON): the settings object.`,
      inputSchema: { response_format: responseFormatField },
      annotations: {
        readOnlyHint: true,
        destructiveHint: false,
        idempotentHint: true,
        openWorldHint: true,
      },
    },
    async ({ response_format }: { response_format: ResponseFormat }): Promise<CallToolResult> => {
      try {
        const settings = await get();
        const text =
          response_format === ResponseFormat.JSON
            ? JSON.stringify(settings, null, 2)
            : settingsMarkdown(settings, 'Settings');
        return ok({ ...settings }, text);
      } catch (error) {
        return fail(error);
      }
    }
  );

  const patchShape: ZodRawShape = cfg.patchSchema ?? {
    patch: z
      .record(z.unknown())
      .describe('Partial settings object; provided fields are merged over the current settings'),
  };

  server.registerTool(
    `${prefix}_set_settings`,
    {
      title: 'Update Settings',
      description: `Update the saved settings for the "${space.env.appId}" Space App.

Performs a read-modify-write merge: only the fields you pass change, the rest are
preserved. Persisted to the SenClaw config KV so the app UI sees the same values.

Returns (JSON): the full settings object after the merge.`,
      inputSchema: { ...patchShape, response_format: responseFormatField },
      annotations: {
        readOnlyHint: false,
        destructiveHint: false,
        idempotentHint: true,
        openWorldHint: true,
      },
    },
    async (args: Record<string, unknown>): Promise<CallToolResult> => {
      try {
        const { response_format = ResponseFormat.MARKDOWN, patch, ...fields } = args as {
          response_format?: ResponseFormat;
          patch?: Record<string, unknown>;
        } & Record<string, unknown>;
        const current = await get();
        const incoming = cfg.patchSchema ? fields : (patch ?? {});
        const merged = { ...current, ...incoming } as T;
        const saved = await set(merged);
        const text =
          response_format === ResponseFormat.JSON
            ? JSON.stringify(saved, null, 2)
            : settingsMarkdown(saved, 'Settings Saved');
        return ok({ ...saved }, text);
      } catch (error) {
        return fail(error);
      }
    }
  );

  return { server, space, getSettings: get as () => Promise<Record<string, unknown>>, setSettings: set as (n: Record<string, unknown>) => Promise<Record<string, unknown>> };
}

/**
 * Start a Space App MCP server over Streamable HTTP. Resolves once it is
 * listening (and, if `autoRegister`, registered with SenClaw).
 */
export async function serveSpaceMcp<T extends Record<string, unknown> = Record<string, unknown>>(
  options: ServeSpaceMcpOptions<T>
): Promise<SpaceMcpHandle> {
  const {
    appId,
    baseUrl = process.env.SENCLAW_BASE_URL ?? DEFAULT_BASE_URL,
    port = Number(process.env.PORT) || DEFAULT_PORT,
    mcpPath = process.env.MCP_PATH ?? DEFAULT_MCP_PATH,
    name = `${appId}-mcp-server`,
    version = '1.0.0',
    toolPrefix = sanitizePrefix(appId),
    settings,
    registerTools,
    autoRegister = false,
    description,
    requireAppToken: guard = false,
    authSkipPaths = [],
  } = options;

  const space = SenclawSpace.forDaemon(appId, baseUrl);

  // A fresh server + transport per request keeps things stateless and avoids
  // request-id collisions.
  const createServer = (): McpServer => {
    const server = new McpServer({ name, version });
    const ctx: SpaceMcpContext = settings
      ? registerSettingsTools(server, toolPrefix, space, settings)
      : { server, space };
    registerTools?.(ctx);
    return server;
  };

  const app = express();
  app.use(express.json({ limit: '1mb' }));

  // Before anything else: only the daemon gets to talk to this port. Mounted
  // ahead of the transport shim so an unauthorised caller never reaches the
  // MCP machinery at all.
  if (guard) {
    app.use(requireAppToken({ skip: ['/health', ...authSkipPaths] }));
  }

  // Compatibility shim: the SenClaw Rust MCP client sends no `Accept` header,
  // which the strict Streamable HTTP transport rejects with HTTP 406. The
  // transport (via Hono's node adapter) rebuilds request headers from
  // `req.rawHeaders` — the original [key, value, ...] array — so we must patch
  // THAT, not the parsed `req.headers` object, for the change to take effect.
  app.use(mcpPath, (req, _res, next) => {
    const desired = 'application/json, text/event-stream';
    const raw = req.rawHeaders;
    let idx = -1;
    for (let i = 0; i < raw.length; i += 2) {
      if (raw[i]?.toLowerCase() === 'accept') {
        idx = i + 1;
        break;
      }
    }
    const current = idx >= 0 ? String(raw[idx]) : '';
    if (!current.includes('application/json') || !current.includes('text/event-stream')) {
      if (idx >= 0) raw[idx] = desired;
      else raw.push('Accept', desired);
      req.headers.accept = desired; // keep the parsed view consistent too
    }
    next();
  });

  app.get('/health', (_req: Request, res: Response) => {
    res.json({ status: 'ok', server: name, appId, senclaw: baseUrl });
  });

  app.post(mcpPath, async (req: Request, res: Response) => {
    if (!originAllowed(req.headers.origin)) {
      res.status(403).json({ jsonrpc: '2.0', error: { code: -32600, message: 'Origin not allowed' }, id: null });
      return;
    }
    const server = createServer();
    const transport = new StreamableHTTPServerTransport({
      sessionIdGenerator: undefined,
      enableJsonResponse: true,
    });
    res.on('close', () => {
      transport.close();
      server.close();
    });
    try {
      await server.connect(transport);
      await transport.handleRequest(req, res, req.body);
    } catch (error) {
      if (!res.headersSent) {
        res.status(500).json({ jsonrpc: '2.0', error: { code: -32603, message: 'Internal server error' }, id: null });
      }
      // eslint-disable-next-line no-console
      console.error('Error handling MCP request:', error);
    }
  });

  const methodNotAllowed = (_req: Request, res: Response): void => {
    res.status(405).json({ jsonrpc: '2.0', error: { code: -32000, message: 'Method not allowed. Use POST for MCP.' }, id: null });
  };
  app.get(mcpPath, methodNotAllowed);
  app.delete(mcpPath, methodNotAllowed);

  const url = `http://127.0.0.1:${port}${mcpPath}`;

  const httpServer = await new Promise<ReturnType<typeof app.listen>>((resolve) => {
    const s = app.listen(port, '127.0.0.1', () => resolve(s));
  });
  // eslint-disable-next-line no-console
  console.error(`${name} listening on ${url} (app=${appId}, senclaw=${baseUrl})`);

  if (autoRegister) {
    try {
      await space.registerMcp({
        name,
        transport: 'http',
        url,
        description: description ?? `MCP server for the ${appId} Space App`,
      });
      // eslint-disable-next-line no-console
      console.error(`Registered ${name} with SenClaw at ${baseUrl}`);
    } catch (error) {
      // eslint-disable-next-line no-console
      console.error(`Self-registration failed (continuing): ${error instanceof Error ? error.message : String(error)}`);
    }
  }

  return {
    port,
    mcpPath,
    url,
    close: () =>
      new Promise<void>((resolve, reject) =>
        httpServer.close((err?: Error) => (err ? reject(err) : resolve()))
      ),
  };
}
