// MCP server as a Next.js Route Handler. The same `npm start` (or standalone
// `node server.js`) process that serves the UI also serves this endpoint at
// `/mcp`, so SenClaw only registers the URL — no separate MCP server process.
//
// Uses the MCP SDK's Web Standard transport (Fetch Request/Response). SenClaw
// daemon calls go through the SenclawSpace SDK so the MCP tools share the same
// config/core API contract as the app UI.
import { McpServer } from '@modelcontextprotocol/sdk/server/mcp.js';
import { WebStandardStreamableHTTPServerTransport } from '@modelcontextprotocol/sdk/server/webStandardStreamableHttp.js';
import { SenclawSpace } from '@senclaw/space-sdk';
import { z } from 'zod';

export const dynamic = 'force-dynamic';
export const runtime = 'nodejs';

const APP_ID = process.env.SENCLAW_SPACE_APP_ID || 'google-workspace';
const BASE_URL = (process.env.SENCLAW_BASE_URL || 'http://127.0.0.1:18788').replace(/\/$/, '');
const SETTINGS_KEY = 'google-workspace-settings';
const KNOWN_SERVICES = ['gmail', 'calendar', 'notes'];
const DEFAULTS = { days: 7, services: ['gmail', 'calendar', 'notes'], mcpPort: 4107, mcpName: 'google-workspace-mcp' };
const space = SenclawSpace.forDaemon(APP_ID, BASE_URL);

function normalize(value) {
  if (!value || typeof value !== 'object') return { ...DEFAULTS };
  const services = Array.isArray(value.services) && value.services.length
    ? value.services.map(String).filter((s) => KNOWN_SERVICES.includes(s))
    : [...DEFAULTS.services];
  return {
    days: Number(value.days) || DEFAULTS.days,
    services: services.length ? services : [...DEFAULTS.services],
    mcpPort: Number(value.mcpPort) || DEFAULTS.mcpPort,
    mcpName: typeof value.mcpName === 'string' && value.mcpName.trim() ? value.mcpName : DEFAULTS.mcpName,
  };
}

const getSettings = async () => normalize(await space.getConfig(SETTINGS_KEY));
const ok = (structured, text) => ({ content: [{ type: 'text', text }], structuredContent: structured });
const fail = (e) => ({ isError: true, content: [{ type: 'text', text: `Error: ${e instanceof Error ? e.message : String(e)}` }] });

function buildServer() {
  const server = new McpServer({ name: 'google-workspace-mcp-server', version: '1.0.0' });

  server.registerTool(
    'gworkspace_get_settings',
    {
      title: 'Get Google Workspace Settings',
      description: 'Read the saved Google Workspace settings (sync window, services, MCP port/name). Defaults applied if unset. Read-only.',
      inputSchema: {},
      annotations: { readOnlyHint: true, destructiveHint: false, idempotentHint: true, openWorldHint: true },
    },
    async () => {
      try {
        const s = await getSettings();
        return ok({ ...s }, JSON.stringify(s, null, 2));
      } catch (e) { return fail(e); }
    }
  );

  server.registerTool(
    'gworkspace_set_settings',
    {
      title: 'Update Google Workspace Settings',
      description: 'Merge-update saved settings (only provided fields change). Persisted to the SenClaw config KV shared with the app UI.',
      inputSchema: {
        days: z.number().int().min(1).max(90).optional().describe('Sync look-back window in days'),
        services: z.array(z.enum(KNOWN_SERVICES)).min(1).optional().describe('Enabled services'),
        mcpPort: z.number().int().min(1024).max(65535).optional(),
        mcpName: z.string().min(1).max(120).optional(),
      },
      annotations: { readOnlyHint: false, destructiveHint: false, idempotentHint: true, openWorldHint: true },
    },
    async (args) => {
      try {
        const current = await getSettings();
        const merged = normalize({ ...current, ...args });
        const saved = normalize(await space.setConfig(SETTINGS_KEY, merged));
        return ok({ ...saved }, JSON.stringify(saved, null, 2));
      } catch (e) { return fail(e); }
    }
  );

  server.registerTool(
    'gworkspace_sync',
    {
      title: 'Sync Google Workspace',
      description: 'Sync Gmail / Calendar (Notes reserved) into the Space using a Google OAuth token. Additive; token used only for this run. Unspecified args fall back to saved settings.',
      inputSchema: {
        token: z.string().min(1).describe("Google OAuth 2.0 access token (e.g. 'ya29...')"),
        days: z.number().int().min(1).max(90).optional(),
        services: z.array(z.enum(KNOWN_SERVICES)).min(1).optional(),
      },
      annotations: { readOnlyHint: false, destructiveHint: false, idempotentHint: false, openWorldHint: true },
    },
    async (args) => {
      try {
        const saved = await getSettings();
        const days = args.days ?? saved.days;
        const services = args.services ?? saved.services;
        const result = await space.core('space/sync/google-workspace', {
          method: 'POST',
          headers: { 'Content-Type': 'application/json' },
          body: JSON.stringify({ token: args.token, days, services }),
        });
        const structured = { ...result, settings_used: { ...saved, days, services } };
        return ok(structured, JSON.stringify(structured, null, 2));
      } catch (e) { return fail(e); }
    }
  );

  return server;
}

async function handle(req) {
  // Accept-header shim: the SenClaw Rust MCP client sends no `Accept` header,
  // which the strict transport rejects with 406. Rebuild with a compliant one.
  const headers = new Headers(req.headers);
  const accept = headers.get('accept') || '';
  if (!accept.includes('application/json') || !accept.includes('text/event-stream')) {
    headers.set('accept', 'application/json, text/event-stream');
  }
  const body = req.method === 'POST' ? await req.arrayBuffer() : undefined;
  const patched = new Request(req.url, { method: req.method, headers, body });

  const server = buildServer();
  const transport = new WebStandardStreamableHTTPServerTransport({
    sessionIdGenerator: undefined,
    enableJsonResponse: true,
  });
  await server.connect(transport);
  return transport.handleRequest(patched);
}

export const GET = handle;
export const POST = handle;
