/**
 * Autonomous work dispatch — the app side of the contract.
 *
 * The daemon's `MCPDispatcher` engine can drive any app that exposes four
 * endpoints. Implement {@link DispatchProvider} over your own store, mount
 * {@link dispatchRouter} (Express) or {@link handleDispatch} (anything else),
 * and the engine will claim work from you, keep leases alive, recover items
 * whose worker died, and report terminal outcomes back.
 *
 * ```ts
 * import express from 'express';
 * import { dispatchRouter } from '@senclaw/space-sdk/dispatch';
 *
 * app.use('/api/dispatch', dispatchRouter(provider));
 * ```
 *
 * The wire shape is the Rust SDK's, field for field, because the same engine
 * parses both: snake_case JSON, `Outcome` tagged by `status`, `Workspace`
 * tagged by `kind`, `McpServerSpec` tagged by `transport`. Note `depends_on`
 * and `timeout_secs` — camelCase there is silently dropped by serde, which
 * surfaces as a dependency that never held rather than as an error.
 */

/** How many workers the dispatcher can spawn right now. */
export type Capacity = {
  /** Max items to claim across this source this tick. */
  total: number;
  /** Max concurrent items per assignee (worker lane). 0 = unlimited. */
  per_assignee: number;
};

/** Where a worker runs. */
export type Workspace =
  /** Fresh temp dir, discarded when the worker finishes. */
  | { kind: 'scratch' }
  /** A persistent absolute path. */
  | { kind: 'dir'; path: string }
  /** A git worktree, for coding tasks. */
  | { kind: 'worktree'; repo: string; branch?: string };

/**
 * An MCP server a worker needs.
 *
 * Prefer `stdio` — an `http` spec has to be bridged to stdio by the engine at
 * launch, which is one more process and one more failure mode.
 */
export type McpServerSpec =
  | { transport: 'stdio'; name: string; command: string; args?: string[]; env?: Record<string, string> }
  | { transport: 'http'; name: string; url: string };

/** A single dispatchable unit of work. */
export type WorkItem = {
  /** Source-scoped id, opaque to the engine. */
  id: string;
  /** The task to run — becomes the agent's user prompt. */
  prompt: string;
  /** Worker/persona to route to. Omitted = the source's default persona. */
  assignee?: string | null;
  /** Extra system-prompt block appended to the persona's own. */
  guidance?: string | null;
  /** MCP servers the worker gets, usually including this app's own tools. */
  mcp?: McpServerSpec[];
  /** Where the worker runs. Defaults to scratch. */
  workspace?: Workspace;
  /** Ids this item depends on. Already satisfied by the time you return it. */
  depends_on?: string[];
  /** Higher runs first. */
  priority?: number;
  /** Per-item run timeout. */
  timeout_secs?: number | null;
};

/** The terminal result of a worker run. */
export type Outcome =
  | { status: 'completed'; summary?: string; metadata?: unknown }
  | { status: 'blocked'; reason: string }
  | { status: 'failed'; error: string }
  | { status: 'timed_out' };

/** Implement over your own store, then mount it. */
export interface DispatchProvider {
  /**
   * Atomically claim up to `capacity` ready items.
   *
   * **Atomically** matters: the engine may poll again before these finish, and
   * an item handed out twice is run twice.
   */
  claimReady(capacity: Capacity): Promise<WorkItem[]> | WorkItem[];
  /** Extend the lease on an in-flight item. Optional — omit if you have no leases. */
  heartbeat?(itemId: string): Promise<void> | void;
  /** Return dead-worker/expired-lease items to ready; return their ids. */
  reclaim?(): Promise<string[]> | string[];
  /** Record a terminal outcome. Map it to your own states. */
  finalize(itemId: string, outcome: Outcome): Promise<void> | void;
}

/** The four actions, as the engine names them in the URL path. */
export type DispatchAction = 'poll' | 'heartbeat' | 'reclaim' | 'finalize';

/** What {@link handleDispatch} answers with: a status and a JSON body. */
export type DispatchResult = { status: number; body: unknown };

/**
 * Framework-agnostic core. Route `POST <prefix>/<action>` to this and send
 * back `status` + `body` however your server does that.
 *
 * Errors become `500 {error}` rather than propagating: the engine reads that
 * field and backs off, whereas an exception escaping into the HTTP layer
 * reaches it as a connection reset it cannot distinguish from a crash.
 */
export async function handleDispatch(
  provider: DispatchProvider,
  action: DispatchAction | string,
  body: unknown,
): Promise<DispatchResult> {
  const b = (body ?? {}) as Record<string, unknown>;
  const itemId = typeof b.item_id === 'string' ? b.item_id : '';
  try {
    switch (action) {
      case 'poll': {
        const c = (b.capacity ?? {}) as Partial<Capacity>;
        const items = await provider.claimReady({
          total: Number(c.total ?? 0),
          per_assignee: Number(c.per_assignee ?? 0),
        });
        return { status: 200, body: items };
      }
      case 'heartbeat':
        await provider.heartbeat?.(itemId);
        return { status: 200, body: { ok: true } };
      case 'reclaim':
        return { status: 200, body: (await provider.reclaim?.()) ?? [] };
      case 'finalize': {
        const outcome = (b.outcome ?? { status: 'failed', error: 'no outcome sent' }) as Outcome;
        await provider.finalize(itemId, outcome);
        return { status: 200, body: { ok: true } };
      }
      default:
        return { status: 404, body: { error: `unknown dispatch action: ${action}` } };
    }
  } catch (err) {
    return { status: 500, body: { error: err instanceof Error ? err.message : String(err) } };
  }
}

/**
 * An Express router with `/poll`, `/heartbeat`, `/reclaim`, `/finalize`.
 * Mount it at `/api/dispatch`.
 *
 * Express is already a dependency of the `/mcp` subpath, so this costs nothing
 * extra — but it is imported lazily so that importing this module from a
 * browser bundle (for the types) does not drag a server framework in with it.
 */
export async function dispatchRouter(provider: DispatchProvider) {
  const { Router, json } = await import('express');
  const router = Router();
  router.use(json({ limit: '4mb' }));
  for (const action of ['poll', 'heartbeat', 'reclaim', 'finalize'] as const) {
    router.post(`/${action}`, async (req, res) => {
      const { status, body } = await handleDispatch(provider, action, req.body);
      res.status(status).json(body);
    });
  }
  return router;
}

// -- constructors, so callers never hand-write a tagged union ---------------

export const workspace = {
  scratch: (): Workspace => ({ kind: 'scratch' }),
  dir: (path: string): Workspace => ({ kind: 'dir', path }),
  worktree: (repo: string, branch?: string): Workspace => ({ kind: 'worktree', repo, ...(branch ? { branch } : {}) }),
};

export const mcpServer = {
  stdio: (name: string, command: string, args: string[] = [], env: Record<string, string> = {}): McpServerSpec =>
    ({ transport: 'stdio', name, command, args, env }),
  http: (name: string, url: string): McpServerSpec => ({ transport: 'http', name, url }),
};

export const outcome = {
  completed: (summary = '', metadata: unknown = null): Outcome => ({ status: 'completed', summary, metadata }),
  blocked: (reason: string): Outcome => ({ status: 'blocked', reason }),
  failed: (error: string): Outcome => ({ status: 'failed', error }),
  timedOut: (): Outcome => ({ status: 'timed_out' }),
};
