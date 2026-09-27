// Same-origin proxy: the browser page (iframe) calls `/api/space/...` on THIS
// app's origin; we forward server-to-server to the SenClaw daemon. This keeps
// the SenclawSpace SDK's relative endpoints working without CORS, even though
// the app is served from its own port.
export const dynamic = 'force-dynamic';
export const runtime = 'nodejs';

const BASE_URL = process.env.SENCLAW_BASE_URL || 'http://127.0.0.1:18788';

async function forward(req, ctx) {
  const { path = [] } = await ctx.params;
  const search = new URL(req.url).search;
  const target = `${BASE_URL}/api/space/${path.map(encodeURIComponent).join('/')}${search}`;

  const headers = new Headers();
  const ct = req.headers.get('content-type');
  if (ct) headers.set('content-type', ct);
  const accept = req.headers.get('accept');
  if (accept) headers.set('accept', accept);

  const hasBody = req.method !== 'GET' && req.method !== 'HEAD';
  const init = {
    method: req.method,
    headers,
    body: hasBody ? await req.arrayBuffer() : undefined,
  };

  try {
    const res = await fetch(target, init);
    const buf = await res.arrayBuffer();
    return new Response(buf, {
      status: res.status,
      headers: { 'content-type': res.headers.get('content-type') || 'application/json' },
    });
  } catch (e) {
    return Response.json(
      { error: `Proxy to SenClaw failed: ${e instanceof Error ? e.message : String(e)}` },
      { status: 502 }
    );
  }
}

export const GET = forward;
export const POST = forward;
export const PUT = forward;
export const DELETE = forward;
