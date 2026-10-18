// A stand-in OpenAI-compatible endpoint for DRY RUNS of the P5 measurement tests. It understands nothing: it copies each
// "User:" line of a window, or each sentence of a card summary, into a fact. A dry run proves the tests run end to end;
// its numbers are NOT a measurement of any model and must never be reported as one.
//   node mock-extractor.mjs --port 59931
import http from 'node:http';

const port = Number(process.argv[process.argv.indexOf('--port') + 1] || 59931);
const sentences = (s) => s.split(/(?<=[.!?])\s+/).map((x) => x.trim()).filter((x) => x.length >= 12);

function extract(user) {
  const window = user.split('\n\nKNOWN FACTS:')[0];
  const m = window.match(/Kanban task "[^"]*" completed: ([\s\S]*)$/);
  let texts;
  if (m) texts = m[1].trim().length < 14 ? [] : sentences(m[1]);
  else texts = [...window.matchAll(/^User: (.+)$/gm)].map((x) => x[1].trim());
  return { facts: texts.map((t) => ({ text: t.length > 590 ? t.slice(0, 590) : t, kind: 'world', when: null, entities: [] })) };
}

http
  .createServer((req, res) => {
    let body = '';
    req.on('data', (c) => (body += c));
    req.on('end', () => {
      if (req.method === 'GET' && req.url.endsWith('/models')) {
        res.writeHead(200, { 'content-type': 'application/json' });
        return res.end(JSON.stringify({ data: [{ id: 'dry-run-mock' }] }));
      }
      const msgs = JSON.parse(body || '{}').messages ?? [];
      const user = msgs.filter((x) => x.role === 'user').map((x) => (typeof x.content === 'string' ? x.content : '')).at(-1) ?? '';
      res.writeHead(200, { 'content-type': 'application/json' });
      res.end(JSON.stringify({ id: 'x', object: 'chat.completion', choices: [{ index: 0, finish_reason: 'stop', message: { role: 'assistant', content: JSON.stringify(extract(user)) } }], usage: { prompt_tokens: 1, completion_tokens: 1, total_tokens: 2 } }));
    });
  })
  .listen(port, '127.0.0.1', () => console.log(`dry-run mock on ${port}`));
