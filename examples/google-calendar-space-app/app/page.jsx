'use client';

import { useEffect, useState } from 'react';
import { SenclawSpace } from '@senclaw/space-sdk';

const styles = {
  page: {
    minHeight: '100vh',
    margin: 0,
    color: '#1f2937',
    background: '#f7f9fc',
    fontFamily: 'Inter, ui-sans-serif, system-ui, -apple-system, BlinkMacSystemFont, "Segoe UI", sans-serif'
  },
  main: {
    maxWidth: 820,
    padding: 24
  },
  header: {
    display: 'flex',
    alignItems: 'center',
    gap: 12,
    marginBottom: 18
  },
  mark: {
    width: 36,
    height: 36,
    borderRadius: 8,
    display: 'grid',
    placeItems: 'center',
    background: '#ffffff',
    border: '1px solid #d9e1ec',
    color: '#2563eb',
    fontWeight: 700
  },
  title: {
    fontSize: 22,
    lineHeight: 1.2,
    margin: 0
  },
  subtitle: {
    margin: '4px 0 0',
    color: '#64748b',
    fontSize: 13
  },
  panel: {
    background: '#ffffff',
    border: '1px solid #d9e1ec',
    borderRadius: 8,
    padding: 16,
    boxShadow: '0 1px 2px rgba(15, 23, 42, 0.04)'
  },
  grid: {
    display: 'grid',
    gridTemplateColumns: 'repeat(auto-fit, minmax(220px, 1fr))',
    gap: 12
  },
  label: {
    display: 'grid',
    gap: 6,
    fontSize: 12,
    color: '#475569',
    fontWeight: 600
  },
  input: {
    width: '100%',
    border: '1px solid #cbd5e1',
    borderRadius: 6,
    padding: '9px 10px',
    font: 'inherit',
    color: '#0f172a',
    background: '#ffffff'
  },
  button: {
    marginTop: 16,
    border: 0,
    borderRadius: 6,
    padding: '9px 14px',
    background: '#2563eb',
    color: '#ffffff',
    fontWeight: 700,
    cursor: 'pointer'
  },
  pre: {
    margin: '16px 0 0',
    padding: 12,
    borderRadius: 6,
    overflow: 'auto',
    background: '#0f172a',
    color: '#dbeafe',
    fontSize: 12,
    lineHeight: 1.45
  }
};

export default function Page() {
  const [token, setToken] = useState('');
  const [days, setDays] = useState(30);
  const [syncing, setSyncing] = useState(false);
  const [result, setResult] = useState('');
  const [space, setSpace] = useState(null);

  useEffect(() => {
    SenclawSpace.init()
      .then(async client => {
        setSpace(client);
        const saved = await client.getConfig('google-calendar-settings');
        if (saved?.days) setDays(saved.days);
      })
      .catch(error => setResult(error instanceof Error ? error.message : String(error)));
  }, []);

  const sync = async () => {
    const accessToken = token.trim();
    if (!accessToken) {
      setResult('Google access token is required.');
      return;
    }
    setSyncing(true);
    setResult('Syncing...');
    try {
      const client = space ?? new SenclawSpace({ appId: 'google-calendar' });
      await client.setConfig('google-calendar-settings', { days: Number(days || 30) });
      await client.sqlite(
        'CREATE TABLE IF NOT EXISTS sync_runs (id INTEGER PRIMARY KEY AUTOINCREMENT, service TEXT NOT NULL, created_at INTEGER NOT NULL)'
      );
      await client.sqlite(
        'INSERT INTO sync_runs (service, created_at) VALUES (?1, ?2)',
        ['google-calendar', Date.now()]
      );
      const payload = await client.core('space/sync/google-calendar', {
        method: 'POST',
        headers: { 'Content-Type': 'application/json' },
        body: JSON.stringify({ token: accessToken, days: Number(days || 30) })
      });
      setResult(JSON.stringify(payload, null, 2));
    } catch (error) {
      setResult(error instanceof Error ? error.message : String(error));
    } finally {
      setSyncing(false);
    }
  };

  return (
    <main style={styles.page}>
      <div style={styles.main}>
        <header style={styles.header}>
          <div style={styles.mark}>G</div>
          <div>
            <h1 style={styles.title}>Google Calendar</h1>
            <p style={styles.subtitle}>Sync Google Calendar events into Space calendar.</p>
          </div>
        </header>
        <section style={styles.panel}>
          <div style={styles.grid}>
            <label style={styles.label}>
              Access token
              <input
                style={styles.input}
                type="password"
                value={token}
                onChange={event => setToken(event.target.value)}
                placeholder="ya29..."
                autoComplete="off"
              />
            </label>
            <label style={styles.label}>
              Sync window
              <input
                style={styles.input}
                type="number"
                min="1"
                max="365"
                value={days}
                onChange={event => setDays(event.target.value)}
              />
            </label>
          </div>
          <button style={{ ...styles.button, opacity: syncing ? 0.6 : 1 }} disabled={syncing} onClick={sync}>
            Sync Google Calendar
          </button>
          {result && <pre style={styles.pre}>{result}</pre>}
        </section>
      </div>
    </main>
  );
}
