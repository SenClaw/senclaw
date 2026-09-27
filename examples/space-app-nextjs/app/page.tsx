export default function Page() {
  return (
    <main style={{ fontFamily: 'Inter, system-ui, sans-serif', padding: 24 }}>
      <h1>Next.js Space App</h1>
      <p>This UI is embedded inside SenClaw Space.</p>
      <button
        onClick={() => {
          const requestId = crypto.randomUUID();
          window.parent.postMessage({
            type: 'senclaw:request',
            requestId,
            action: 'capabilities',
            payload: {}
          }, '*');
        }}
      >
        Ask SenClaw bridge
      </button>
    </main>
  );
}
