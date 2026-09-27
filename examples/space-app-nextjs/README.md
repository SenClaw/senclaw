# Space App Next.js Example

This example shows the package contract for a Space App that can be installed from a ZIP.

## Manifest

The ZIP root must contain `senclaw-manifest.json`:

```json
{
  "id": "nextjs-dashboard",
  "name": "Next.js Dashboard",
  "description": "Example Space App embedded in the Space sidebar.",
  "icon": "N",
  "integration": {
    "type": "iframe",
    "url": "/api/space/apps/nextjs-dashboard/static/index.html"
  },
  "bridge": {
    "postMessage": true,
    "capabilities": ["llm.request", "mcp.call", "space.rest"]
  }
}
```

## Build and install

For a static Next.js app:

```bash
npm install
npm run build
cp senclaw-manifest.json out/senclaw-manifest.json
cd out
zip -r nextjs-dashboard.zip .
```

Install `nextjs-dashboard.zip` from `Settings -> Space Apps` or `Space -> Apps -> Cài từ ZIP`.

## Bridge call

Inside the iframe app:

```js
const requestId = crypto.randomUUID();
window.parent.postMessage({
  type: 'senclaw:request',
  requestId,
  action: 'capabilities',
  payload: {}
}, '*');

window.addEventListener('message', event => {
  if (event.data?.type === 'senclaw:response' && event.data.requestId === requestId) {
    console.log(event.data.payload);
  }
});
```

`llm.request` and `mcp.call` are declared in the bridge contract but intentionally gated in the backend until approved execution handlers are wired.
