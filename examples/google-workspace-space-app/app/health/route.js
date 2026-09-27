// Liveness probe. SenClaw polls this after launching `npm start` to know when
// the app server (UI + MCP) is ready before registering it.
export const dynamic = 'force-dynamic';

export function GET() {
  return Response.json({
    status: 'ok',
    app: process.env.SENCLAW_SPACE_APP_ID || 'google-workspace',
    senclaw: process.env.SENCLAW_BASE_URL || 'http://127.0.0.1:18788',
  });
}
