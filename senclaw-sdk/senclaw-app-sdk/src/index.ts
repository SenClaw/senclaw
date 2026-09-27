export type SenclawSpaceEnv = {
  appId: string;
  apiBase: string;
  coreBase?: string;
  staticBase?: string;
  bridgeEndpoint?: string;
  configEndpoint?: string;
  sqliteEndpoint?: string;
  mcpRegisterEndpoint?: string;
  /**
   * This app's access token, from `SENCLAW_TOKEN_ACCESS_APP`.
   *
   * The daemon mints one per installed app and puts it in the launched
   * process's environment. Presenting it on `/api/space/apps/<id>/…` is what
   * tells the daemon *which* app is calling: a token is bound to one app id,
   * and using it against another is refused. Without it, any local process that
   * knows an app's id — which is public — could read that app's settings, query
   * its database and drive its AI bridge.
   *
   * Empty in the browser by design: the app's own page is trusted same-origin
   * and a secret handed to page JS is a secret in every extension the user has
   * installed. Only the app's server process gets one.
   */
  appToken?: string;
  /** Space-App API contract version, from `SENCLAW_API_VERSION`. */
  apiVersion?: number;
};

/** Env var carrying this app's access token into its process. */
export const ENV_APP_TOKEN = 'SENCLAW_TOKEN_ACCESS_APP';

/** Env var carrying the Space-App API contract version. */
export const ENV_API_VERSION = 'SENCLAW_API_VERSION';

/** Header the access token travels in. */
export const HEADER_APP_TOKEN = 'X-SenClaw-App-Token';

/** Header the contract version travels in, both directions. */
export const HEADER_API_VERSION = 'X-SenClaw-Api-Version';

/**
 * The Space-App API contract this SDK is written against. Sent on every daemon
 * call; a daemon serving an older contract answers 426 rather than
 * half-answering.
 */
export const API_VERSION = 2;

/** The access token the daemon issued this app, or `''` outside SenClaw. */
export function appTokenFromEnv(): string {
  if (typeof process === 'undefined') return '';
  return (process.env[ENV_APP_TOKEN] ?? '').trim();
}

/** The contract version the daemon launched this app under. */
export function apiVersionFromEnv(): number {
  if (typeof process === 'undefined') return API_VERSION;
  const n = Number.parseInt((process.env[ENV_API_VERSION] ?? '').trim(), 10);
  return Number.isFinite(n) && n > 0 ? n : API_VERSION;
}

export type SqliteQueryResult<T = Record<string, unknown>> = {
  rows?: T[];
  rowsAffected?: number;
  lastInsertRowId?: number;
};

/**
 * Provider-reported token usage for one `llm.request`.
 *
 * `inputTokens` is the TOTAL billed input — cache tokens included, not on top
 * of. The two cache fields break it down for providers that report them
 * (Anthropic); adding them to `inputTokens` double-counts.
 */
export type LlmUsage = {
  inputTokens: number;
  outputTokens: number;
  cacheReadTokens: number;
  cacheCreationTokens: number;
};

/** The full reply shape from `llmDetailed()`. */
export type LlmReply = {
  text: string;
  model: string;
  /** `'length'` (hit the token cap), `'stop'`, or `''` when unreported. */
  finish: string;
  /** Null when the provider reported no usage — unknown, not zero. */
  usage: LlmUsage | null;
};

/** One hit from `knowledgeSearch()`. */
export type KnowledgeHit = { name: string; summary: string; score: number };

/** One LLM configured in the daemon. */
export type ModelInfo = { id: string; modelName: string | null; provider: string | null };

export type McpRegistration = {
  name?: string;
  transport: 'stdio' | 'sse' | 'http';
  description?: string;
  url?: string;
  command?: string;
  args?: string[];
  env?: Record<string, string>;
  headers?: Record<string, string>;
  use_tools?: string[];
  enabled?: boolean;
};

type InitMessage = {
  type: 'senclaw:init';
  appId: string;
  env?: Partial<SenclawSpaceEnv>;
};

function isBrowser() {
  return typeof window !== 'undefined';
}

function getWindowEnv(): Partial<SenclawSpaceEnv> {
  if (!isBrowser()) return {};
  return (window as unknown as { __SENCLAW_SPACE_ENV__?: Partial<SenclawSpaceEnv> }).__SENCLAW_SPACE_ENV__ ?? {};
}

function fromProcessEnv(): Partial<SenclawSpaceEnv> {
  if (typeof process === 'undefined') return {};
  return {
    appId: process.env.SENCLAW_SPACE_APP_ID,
    apiBase: process.env.SENCLAW_SPACE_API_BASE,
    coreBase: process.env.SENCLAW_SPACE_CORE_BASE,
    appToken: appTokenFromEnv() || undefined,
    apiVersion: apiVersionFromEnv(),
  };
}

function joinUrl(base: string, path: string) {
  return `${base.replace(/\/$/, '')}/${path.replace(/^\//, '')}`;
}

function appIdFromLocation(): string | null {
  if (!isBrowser()) return null;
  const match = window.location.pathname.match(/\/api\/space\/apps\/([^/]+)\/static(?:\/|$)/);
  return match ? decodeURIComponent(match[1]) : null;
}

async function parseResponse<T>(response: Response): Promise<T> {
  const text = await response.text();
  const payload = text ? JSON.parse(text) : null;
  if (!response.ok) {
    const message = typeof payload === 'object' && payload && 'error' in payload
      ? String((payload as { error: unknown }).error)
      : text || response.statusText;
    throw new Error(message);
  }
  return payload as T;
}

export class SenclawSpace {
  env: SenclawSpaceEnv;

  constructor(env: Partial<SenclawSpaceEnv> = {}) {
    const merged = {
      ...fromProcessEnv(),
      ...getWindowEnv(),
      ...env,
    };
    const appId = merged.appId;
    if (!appId) {
      throw new Error('SenclawSpace requires appId. Wait for init() in browser or pass env explicitly.');
    }
    const apiBase = merged.apiBase ?? '/api/space/apps';
    this.env = {
      appId,
      apiBase,
      coreBase: merged.coreBase ?? '/api',
      staticBase: merged.staticBase ?? joinUrl(apiBase, `${appId}/static`),
      bridgeEndpoint: merged.bridgeEndpoint ?? joinUrl(apiBase, `${appId}/bridge`),
      configEndpoint: merged.configEndpoint ?? joinUrl(apiBase, `${appId}/config`),
      sqliteEndpoint: merged.sqliteEndpoint ?? joinUrl(apiBase, `${appId}/sqlite/query`),
      mcpRegisterEndpoint: merged.mcpRegisterEndpoint ?? joinUrl(apiBase, `${appId}/mcp/register`),
      appToken: merged.appToken,
      apiVersion: merged.apiVersion ?? API_VERSION,
    };
  }

  /**
   * Every daemon call goes through here, so the app's identity is attached in
   * one place rather than at nine call sites.
   *
   * An absent token is omitted rather than sent blank: the daemon would try to
   * resolve `''`, refusing a call its default mode would have served. That is
   * the normal state in the browser and when running the app by hand.
   */
  private req(url: string, init: RequestInit = {}): Promise<Response> {
    const headers = new Headers(init.headers);
    if (this.env.appToken) headers.set(HEADER_APP_TOKEN, this.env.appToken);
    if (this.env.apiVersion) headers.set(HEADER_API_VERSION, String(this.env.apiVersion));
    return fetch(url, { ...init, headers });
  }

  /**
   * Construct a client for a standalone Node process (e.g. a bundled MCP server)
   * that must reach the daemon over an absolute URL rather than relative paths.
   *
   * `baseUrl` defaults to `SENCLAW_BASE_URL`, which the daemon injects into
   * every app it launches, and only then to the well-known port. Hardcoding the
   * port instead works right up until someone runs the daemon on another one.
   */
  static forDaemon(appId?: string, baseUrl?: string): SenclawSpace {
    const id = appId
      ?? (typeof process !== 'undefined' ? process.env.SENCLAW_SPACE_APP_ID : undefined);
    if (!id) {
      throw new Error('SenclawSpace.forDaemon needs an appId, or SENCLAW_SPACE_APP_ID in the env');
    }
    const resolved = baseUrl
      ?? (typeof process !== 'undefined' ? process.env.SENCLAW_BASE_URL : undefined)
      ?? 'http://127.0.0.1:18788';
    const base = resolved.replace(/\/$/, '');
    return new SenclawSpace({
      appId: id,
      apiBase: `${base}/api/space/apps`,
      coreBase: `${base}/api`,
    });
  }

  static async init(timeoutMs = 1500): Promise<SenclawSpace> {
    if (!isBrowser()) return new SenclawSpace();
    const existing = getWindowEnv();
    if (existing.appId) return new SenclawSpace(existing);

    const message = await new Promise<InitMessage | null>(resolve => {
      const timer = window.setTimeout(() => {
        window.removeEventListener('message', onMessage);
        resolve(null);
      }, timeoutMs);
      const onMessage = (event: MessageEvent) => {
        if (event.data?.type !== 'senclaw:init') return;
        window.clearTimeout(timer);
        window.removeEventListener('message', onMessage);
        resolve(event.data as InitMessage);
      };
      window.addEventListener('message', onMessage);
      window.parent?.postMessage({ type: 'senclaw:ready' }, '*');
    });

    if (!message) {
      const appId = appIdFromLocation();
      if (!appId) {
        throw new Error('Timed out waiting for senclaw:init.');
      }
      const fallback = await parseResponse<SenclawSpaceEnv>(
        await fetch(`/api/space/apps/${encodeURIComponent(appId)}/env`)
      );
      (window as unknown as { __SENCLAW_SPACE_ENV__?: Partial<SenclawSpaceEnv> }).__SENCLAW_SPACE_ENV__ = fallback;
      return new SenclawSpace(fallback);
    }
    const env = { appId: message.appId, ...message.env };
    (window as unknown as { __SENCLAW_SPACE_ENV__?: Partial<SenclawSpaceEnv> }).__SENCLAW_SPACE_ENV__ = env;
    return new SenclawSpace(env);
  }

  async getConfig<T = unknown>(key: string): Promise<T | null> {
    const response = await this.req(`${this.env.configEndpoint}/${encodeURIComponent(key)}`);
    if (response.status === 404) return null;
    const payload = await parseResponse<{ value: T }>(response);
    return payload.value;
  }

  async setConfig<T = unknown>(key: string, value: T): Promise<T> {
    const response = await this.req(`${this.env.configEndpoint}/${encodeURIComponent(key)}`, {
      method: 'PUT',
      headers: { 'Content-Type': 'application/json' },
      body: JSON.stringify({ value }),
    });
    const payload = await parseResponse<{ value: T }>(response);
    return payload.value;
  }

  async deleteConfig(key: string): Promise<void> {
    await parseResponse(await this.req(`${this.env.configEndpoint}/${encodeURIComponent(key)}`, { method: 'DELETE' }));
  }

  async listConfig(): Promise<Array<{ key: string; value: unknown; updated_at: number }>> {
    const payload = await parseResponse<{ items: Array<{ key: string; value: unknown; updated_at: number }> }>(
      await this.req(this.env.configEndpoint ?? '')
    );
    return payload.items;
  }

  async sqlite<T = Record<string, unknown>>(sql: string, params: unknown[] = []): Promise<SqliteQueryResult<T>> {
    return parseResponse<SqliteQueryResult<T>>(await this.req(this.env.sqliteEndpoint ?? '', {
      method: 'POST',
      headers: { 'Content-Type': 'application/json' },
      body: JSON.stringify({ sql, params }),
    }));
  }

  async registerMcp(registration: McpRegistration): Promise<unknown> {
    return parseResponse(await this.req(this.env.mcpRegisterEndpoint ?? '', {
      method: 'POST',
      headers: { 'Content-Type': 'application/json' },
      body: JSON.stringify(registration),
    }));
  }

  async core<T = unknown>(path: string, init?: RequestInit): Promise<T> {
    return parseResponse<T>(await this.req(joinUrl(this.env.coreBase ?? '/api', path), init));
  }

  /**
   * Call one of the daemon's bridge actions.
   *
   * The generic form; prefer the named wrappers below, which document the trap
   * in each. `bridge('capabilities', {})` lists what this daemon supports.
   *
   * The wire field is `action`. The daemon's request struct requires it and has
   * no alias, so any other spelling is a 422 before a single line of handler
   * code runs — which looks like "the bridge is down" rather than "you sent the
   * wrong key".
   */
  async bridge<T = unknown>(action: string, payload: Record<string, unknown>): Promise<T> {
    const result = await parseResponse<T>(await this.req(this.env.bridgeEndpoint ?? '', {
      method: 'POST',
      headers: { 'Content-Type': 'application/json' },
      body: JSON.stringify({ action, payload }),
    }));
    // A failed bridge action comes back as **HTTP 200** carrying
    // `{status: "error", message}` — the transport worked, the action did not.
    // Checking only the HTTP code turns a dead provider into an empty string,
    // which reads downstream as "the model had nothing to say".
    const env = result as { status?: string; message?: string } | null;
    if (env && typeof env === 'object' && 'status' in env && env.status !== 'ok') {
      if (env.status === 'pending') {
        throw new Error(`bridge action '${action}' is not enabled in this daemon`);
      }
      throw new Error(env.message || `bridge action '${action}' failed`);
    }
    return result;
  }

  /** What this daemon's bridge actually supports, straight from the daemon. */
  async capabilities(): Promise<string[]> {
    const r = await this.bridge<{ capabilities?: string[] }>('capabilities', {});
    return r?.capabilities ?? [];
  }

  /**
   * One model call, through the provider the *user* configured. A Space App
   * never holds a provider API key.
   *
   * Only `system`, `prompt`, `maxTokens` and `profile` are read — there is no
   * temperature knob, and passing one is ignored rather than honoured.
   *
   * Watch `maxTokens`: a reply that hits the ceiling comes back truncated with
   * `finish === 'length'`, which reads as a short answer rather than as an
   * error. This throws on that instead of returning the fragment — split long
   * work into chunks rather than raising the ceiling and hoping.
   */
  async llm(opts: {
    prompt: string;
    system?: string;
    maxTokens?: number;
    profile?: string;
  }): Promise<string> {
    const result = await this.bridge<{ text?: string; content?: string; finish?: string }>(
      'llm.request',
      {
        prompt: opts.prompt,
        maxTokens: opts.maxTokens ?? 4000,
        ...(opts.system ? { system: opts.system } : {}),
        ...(opts.profile ? { profile: opts.profile } : {}),
      },
    );
    if (result?.finish === 'length') {
      throw new Error(
        'the model hit maxTokens and the reply is truncated — split the work into smaller ' +
        'chunks rather than raising the ceiling',
      );
    }
    return result?.text ?? result?.content ?? '';
  }

  /**
   * The same call as `llm()`, returning everything the provider reported
   * instead of just the text.
   *
   * Use this when you need to *handle* a truncated reply rather than have it
   * thrown at you — `finish === 'length'` means the model hit the cap — or when
   * you want real token counts for bookkeeping. `usage` is null when the
   * provider reported none, which some local models do; treat that as unknown,
   * not as zero.
   */
  async llmDetailed(opts: {
    prompt: string;
    system?: string;
    maxTokens?: number;
    profile?: string;
  }): Promise<LlmReply> {
    const r = await this.bridge<{
      text?: string; content?: string; model?: string; finish?: string;
      usage?: { inputTokens?: number; outputTokens?: number; cacheReadTokens?: number; cacheCreationTokens?: number };
    }>('llm.request', {
      prompt: opts.prompt,
      maxTokens: opts.maxTokens ?? 4000,
      ...(opts.system ? { system: opts.system } : {}),
      ...(opts.profile ? { profile: opts.profile } : {}),
    });
    return {
      text: r?.text ?? r?.content ?? '',
      model: r?.model ?? '',
      finish: r?.finish ?? '',
      usage: r?.usage
        ? {
            inputTokens: r.usage.inputTokens ?? 0,
            outputTokens: r.usage.outputTokens ?? 0,
            cacheReadTokens: r.usage.cacheReadTokens ?? 0,
            cacheCreationTokens: r.usage.cacheCreationTokens ?? 0,
          }
        : null,
    };
  }

  /** Run a full agent turn — tools, multiple steps. Slower and far more capable than `llm`. */
  async agent<T = unknown>(prompt: string, tools?: string[]): Promise<T> {
    return this.bridge<T>('agent.run', { prompt, ...(tools ? { tools } : {}) });
  }

  // -- knowledge -----------------------------------------------------------
  //
  // Each *space* is an independent memory partition. Omitting `space` uses the
  // app's own private one, named after the app id — so an app that never passes
  // a space can neither read nor pollute anybody else's memory.

  /** Save one memory into a knowledge space. */
  async knowledgeSave(text: string, opts: { space?: string; source?: string; tags?: string[] } = {}): Promise<void> {
    await this.bridge('knowledge.save', {
      text,
      ...(opts.space ? { space: opts.space } : {}),
      ...(opts.source ? { source: opts.source } : {}),
      ...(opts.tags ? { tags: opts.tags } : {}),
    });
  }

  /** Scoped search over one knowledge space — raw hits, no synthesis. */
  async knowledgeSearch(query: string, opts: { space?: string; limit?: number } = {}): Promise<KnowledgeHit[]> {
    const r = await this.bridge<{ hits?: Array<{ name?: string; summary?: string; score?: number }> }>(
      'knowledge.search',
      { query, limit: opts.limit ?? 10, ...(opts.space ? { space: opts.space } : {}) },
    );
    return (r?.hits ?? []).map(h => ({ name: h.name ?? '', summary: h.summary ?? '', score: h.score ?? 0 }));
  }

  /**
   * Scoped recall *with* LLM synthesis — one answer instead of a hit list.
   * Empty string when the space holds nothing relevant, which is a real answer
   * and not an error.
   */
  async knowledgeRecall(query: string, opts: { space?: string; limit?: number; hops?: number } = {}): Promise<string> {
    const r = await this.bridge<{ answer?: string }>('knowledge.recall', {
      query,
      ...(opts.space ? { space: opts.space } : {}),
      ...(opts.limit ? { limit: opts.limit } : {}),
      ...(opts.hops ? { hops: opts.hops } : {}),
    });
    return r?.answer ?? '';
  }

  /**
   * Report tokens for a provider call the app made **directly** — its own API
   * key, not through `llm()` — so the daemon's accounting stays whole.
   *
   * Fire-and-forget: a failure here must never take down the work it is
   * describing, so this swallows errors. Pass `estimated: true` when the
   * numbers are chars/4 guesses rather than provider counts.
   */
  async usageReport(u: {
    model: string;
    provider: string;
    inputTokens: number;
    outputTokens: number;
    latencyMs?: number;
    estimated?: boolean;
  }): Promise<void> {
    try {
      await this.bridge('usage.report', {
        model: u.model,
        provider: u.provider,
        inputTokens: u.inputTokens,
        outputTokens: u.outputTokens,
        latencyMs: u.latencyMs ?? 0,
        estimated: u.estimated ?? false,
      });
    } catch {
      /* accounting is not worth failing the caller over */
    }
  }

  /** The daemon's configured LLMs, plus which one is active. */
  async listModels(): Promise<{ activeId: string | null; models: ModelInfo[] }> {
    const v = await this.core<{
      activeId?: string;
      configs?: Array<{ id?: string; modelName?: string; provider?: string; adapt?: string }>;
    }>('/llm-config');
    return {
      activeId: v?.activeId ?? null,
      models: (v?.configs ?? [])
        .filter(c => typeof c.id === 'string')
        .map(c => ({ id: c.id as string, modelName: c.modelName ?? null, provider: c.provider ?? c.adapt ?? null })),
    };
  }

  /**
   * Switch the daemon's active main model.
   *
   * This is **global** — the agent and every other app share it. An app that
   * wants its own model should pass `profile` to `llm()` instead of moving
   * everyone else's cheese.
   */
  async setActiveModel(id: string): Promise<void> {
    await this.core('/llm-config/active', {
      method: 'POST',
      headers: { 'Content-Type': 'application/json' },
      body: JSON.stringify({ id }),
    });
  }
}
