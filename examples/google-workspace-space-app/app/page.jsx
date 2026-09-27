'use client';

import { useEffect, useMemo, useState } from 'react';
import {
  Alert,
  Button,
  Card,
  Checkbox,
  Col,
  ConfigProvider,
  Form,
  Input,
  InputNumber,
  List,
  Modal,
  Row,
  Space,
  Statistic,
  Tag,
  Typography,
  message,
} from 'antd';
import {
  CalendarOutlined,
  CheckCircleOutlined,
  DatabaseOutlined,
  GoogleOutlined,
  MailOutlined,
  SaveOutlined,
  SettingOutlined,
  SyncOutlined,
} from '@ant-design/icons';
import { SenclawSpace } from '@senclaw/space-sdk';

const { Title, Text, Paragraph } = Typography;

const SERVICE_OPTIONS = [
  { id: 'gmail', label: 'Gmail', detail: 'Inbox sync and searchable message cache', icon: <MailOutlined /> },
  { id: 'calendar', label: 'Calendar', detail: 'Events imported into Space Calendar', icon: <CalendarOutlined /> },
  { id: 'notes', label: 'Notes', detail: 'Reserved Keep/Drive notes pipeline', icon: <DatabaseOutlined /> },
];

const defaultSettings = {
  days: 7,
  services: ['gmail', 'calendar', 'notes'],
  mcpPort: 4107,
  mcpName: 'google-workspace-mcp',
};

function formatTime(value) {
  if (!value) return '-';
  return new Intl.DateTimeFormat(undefined, {
    month: 'short',
    day: '2-digit',
    hour: '2-digit',
    minute: '2-digit',
  }).format(new Date(value));
}

function normalizeSettings(value) {
  if (!value || typeof value !== 'object') return defaultSettings;
  return {
    ...defaultSettings,
    ...value,
    days: Number(value.days || defaultSettings.days),
    mcpPort: Number(value.mcpPort || defaultSettings.mcpPort),
    services: Array.isArray(value.services) && value.services.length ? value.services : defaultSettings.services,
  };
}

export default function Page() {
  const [space, setSpace] = useState(null);
  const [settings, setSettings] = useState(defaultSettings);
  const [draftSettings, setDraftSettings] = useState(defaultSettings);
  const [settingsOpen, setSettingsOpen] = useState(false);
  const [token, setToken] = useState('');
  const [syncing, setSyncing] = useState(false);
  const [status, setStatus] = useState('Loading Space runtime...');
  const [result, setResult] = useState('');
  const [runs, setRuns] = useState([]);
  const [mcpStatus, setMcpStatus] = useState(null);

  const selectedLabels = useMemo(() => {
    return SERVICE_OPTIONS
      .filter(service => settings.services.includes(service.id))
      .map(service => service.label)
      .join(', ');
  }, [settings.services]);

  useEffect(() => {
    let cancelled = false;
    SenclawSpace.init()
      .then(async client => {
        if (cancelled) return;
        setSpace(client);
        await ensureSchema(client);
        const saved = normalizeSettings(await client.getConfig('google-workspace-settings'));
        if (cancelled) return;
        setSettings(saved);
        setDraftSettings(saved);
        await loadRuns(client);
        await loadMcpStatus();
        setStatus('Ready');
      })
      .catch(error => {
        if (!cancelled) setStatus(error instanceof Error ? error.message : String(error));
      });
    return () => {
      cancelled = true;
    };
  }, []);

  const getClient = () => space ?? new SenclawSpace({ appId: 'google-workspace' });

  const ensureSchema = async client => {
    await client.sqlite(
      'CREATE TABLE IF NOT EXISTS sync_runs (id INTEGER PRIMARY KEY AUTOINCREMENT, service TEXT NOT NULL, status TEXT NOT NULL, created_at INTEGER NOT NULL)'
    );
  };

  const loadRuns = async client => {
    await ensureSchema(client);
    const data = await client.sqlite(
      'SELECT id, service, status, created_at FROM sync_runs ORDER BY id DESC LIMIT 8'
    );
    setRuns(data.rows ?? []);
  };

  const saveSettings = async () => {
    const next = normalizeSettings(draftSettings);
    if (!next.services.length) {
      message.warning('Select at least one Google Workspace service.');
      return;
    }
    const client = getClient();
    await client.setConfig('google-workspace-settings', next);
    setSettings(next);
    setSettingsOpen(false);
    setStatus('Settings saved');
    message.success('Settings saved');
  };

  const sync = async () => {
    const accessToken = token.trim();
    if (!accessToken) {
      message.warning('Google access token is required for this sync run.');
      setStatus('Google access token is required for this sync run.');
      return;
    }
    const client = getClient();
    setSyncing(true);
    setStatus('Syncing Google Workspace...');
    setResult('');
    try {
      await ensureSchema(client);
      await client.setConfig('google-workspace-settings', settings);
      await client.sqlite(
        'INSERT INTO sync_runs (service, status, created_at) VALUES (?1, ?2, ?3)',
        ['google-workspace', 'started', Date.now()]
      );
      const payload = await client.core('space/sync/google-workspace', {
        method: 'POST',
        headers: { 'Content-Type': 'application/json' },
        body: JSON.stringify({
          token: accessToken,
          days: Number(settings.days || 7),
          services: settings.services,
        })
      });
      await client.sqlite(
        'INSERT INTO sync_runs (service, status, created_at) VALUES (?1, ?2, ?3)',
        ['google-workspace', payload.status ?? 'completed', Date.now()]
      );
      await loadRuns(client);
      setResult(JSON.stringify(payload, null, 2));
      setStatus('Sync completed');
      message.success('Sync completed');
    } catch (error) {
      await client.sqlite(
        'INSERT INTO sync_runs (service, status, created_at) VALUES (?1, ?2, ?3)',
        ['google-workspace', 'error', Date.now()]
      ).catch(() => {});
      await loadRuns(client).catch(() => {});
      const text = error instanceof Error ? error.message : String(error);
      setStatus(text);
      message.error(text);
    } finally {
      setSyncing(false);
    }
  };

  const loadMcpStatus = async () => {
    try {
      const res = await fetch('/api/space/apps/google-workspace/mcp');
      if (!res.ok) return;
      const data = await res.json();
      setMcpStatus({
        status: data?.server?.status ?? 'unknown',
        tools: data?.server?.tools?.length ?? 0,
        autoRegister: !!data?.declared?.autoRegister,
        error: data?.server?.error ?? null,
      });
    } catch {
      // ignore — MCP status is informational
    }
  };

  return (
    <ConfigProvider
      theme={{
        token: {
          borderRadius: 8,
          colorPrimary: '#2563eb',
          fontFamily: 'Inter, ui-sans-serif, system-ui, -apple-system, BlinkMacSystemFont, "Segoe UI", sans-serif',
        },
      }}
    >
      <main style={{ minHeight: '100vh', background: '#f7f9fc', padding: 24 }}>
        <Space direction="vertical" size={16} style={{ width: '100%', maxWidth: 1120 }}>
          <Row align="middle" justify="space-between" gutter={[16, 16]}>
            <Col>
              <Space align="center" size={12}>
                <div
                  style={{
                    width: 40,
                    height: 40,
                    borderRadius: 8,
                    border: '1px solid #d9e1ec',
                    background: '#fff',
                    display: 'grid',
                    placeItems: 'center',
                    color: '#2563eb',
                    fontWeight: 800,
                  }}
                >
                  <GoogleOutlined />
                </div>
                <div>
                  <Title level={3} style={{ margin: 0 }}>Google Workspace</Title>
                  <Text type="secondary">Workspace sync app powered by SenclawSpace SDK.</Text>
                </div>
              </Space>
            </Col>
            <Col>
              <Space wrap>
                <Button
                  icon={<SettingOutlined />}
                  onClick={() => {
                    setDraftSettings(settings);
                    setSettingsOpen(true);
                  }}
                >
                  Settings
                </Button>
                <Button type="primary" icon={<SyncOutlined />} loading={syncing} onClick={sync}>
                  Sync now
                </Button>
              </Space>
            </Col>
          </Row>

          <Card>
            <Row gutter={[12, 12]} align="middle">
              <Col xs={24} md={12}>
                <Text strong>Access token for this run</Text>
                <Input.Password
                  value={token}
                  onChange={event => setToken(event.target.value)}
                  placeholder="ya29..."
                  autoComplete="off"
                  style={{ marginTop: 6 }}
                />
              </Col>
              <Col xs={12} md={6}>
                <Statistic title="Sync window" value={settings.days} suffix="days" />
              </Col>
              <Col xs={12} md={6}>
                <Statistic title="Enabled services" value={settings.services.length} />
              </Col>
            </Row>
            <Alert
              style={{ marginTop: 12 }}
              type={status === 'Ready' || status.includes('saved') || status.includes('completed') ? 'success' : 'info'}
              showIcon
              message={status}
            />
          </Card>

          <Row gutter={[16, 16]}>
            <Col xs={24} lg={15}>
              <Card title="Services">
                <Row gutter={[10, 10]}>
                  {SERVICE_OPTIONS.map(service => {
                    const enabled = settings.services.includes(service.id);
                    return (
                      <Col xs={24} md={8} key={service.id}>
                        <Card
                          size="small"
                          type="inner"
                          style={{
                            height: '100%',
                            borderColor: enabled ? '#93c5fd' : undefined,
                            background: enabled ? '#eff6ff' : '#fff',
                          }}
                        >
                          <Space direction="vertical" size={4}>
                            <Space>
                              {service.icon}
                              <Text strong>{service.label}</Text>
                            </Space>
                            <Paragraph type="secondary" style={{ margin: 0, fontSize: 12 }}>
                              {service.detail}
                            </Paragraph>
                            <Tag color={enabled ? 'blue' : 'default'}>
                              {enabled ? 'Enabled' : 'Disabled'}
                            </Tag>
                          </Space>
                        </Card>
                      </Col>
                    );
                  })}
                </Row>
                <Text type="secondary" style={{ display: 'block', marginTop: 12 }}>
                  Selected: {selectedLabels || 'none'}
                </Text>
                {result && (
                  <pre
                    style={{
                      margin: '12px 0 0',
                      padding: 12,
                      borderRadius: 8,
                      overflow: 'auto',
                      background: '#0f172a',
                      color: '#dbeafe',
                      fontSize: 12,
                      lineHeight: 1.45,
                      maxHeight: 280,
                    }}
                  >
                    {result}
                  </pre>
                )}
              </Card>
            </Col>

            <Col xs={24} lg={9}>
              <Card title="Run log">
                <Card size="small" type="inner" style={{ marginBottom: 10 }}>
                  <Space direction="vertical" size={4} style={{ width: '100%' }}>
                    <Space wrap>
                      <Text strong>{settings.mcpName}</Text>
                      {mcpStatus?.autoRegister && <Tag color="geekblue">auto-register</Tag>}
                      <Tag
                        color={
                          mcpStatus?.status === 'connected'
                            ? 'green'
                            : mcpStatus?.status === 'error'
                              ? 'red'
                              : 'default'
                        }
                      >
                        {mcpStatus?.status ?? 'unknown'}
                      </Tag>
                    </Space>
                    <Text type="secondary" style={{ fontSize: 12 }}>
                      MCP server tự động đăng ký khi cài/khởi động ·{' '}
                      {mcpStatus?.tools ?? 0} tool · cổng {settings.mcpPort}
                    </Text>
                    {mcpStatus?.error && (
                      <Text type="danger" style={{ fontSize: 12 }}>{mcpStatus.error}</Text>
                    )}
                  </Space>
                </Card>
                <List
                  locale={{ emptyText: 'No sync runs yet.' }}
                  dataSource={runs}
                  renderItem={run => (
                    <List.Item>
                      <Space>
                        <CheckCircleOutlined style={{ color: run.status === 'error' ? '#dc2626' : '#2563eb' }} />
                        <Text>{run.status}</Text>
                      </Space>
                      <Text type="secondary">{formatTime(run.created_at)}</Text>
                    </List.Item>
                  )}
                />
              </Card>
            </Col>
          </Row>
        </Space>

        <Modal
          title="Google Workspace Settings"
          open={settingsOpen}
          onCancel={() => setSettingsOpen(false)}
          onOk={saveSettings}
          okText="Save settings"
          okButtonProps={{ icon: <SaveOutlined /> }}
          width={720}
        >
          <Paragraph type="secondary">
            These values are stored with SenclawSpace config KV for this app.
          </Paragraph>
          <Form layout="vertical">
            <Row gutter={12}>
              <Col xs={24} md={8}>
                <Form.Item label="Sync window">
                  <InputNumber
                    min={1}
                    max={90}
                    value={draftSettings.days}
                    onChange={value => setDraftSettings(current => ({ ...current, days: value ?? 7 }))}
                    addonAfter="days"
                    style={{ width: '100%' }}
                  />
                </Form.Item>
              </Col>
              <Col xs={24} md={8}>
                <Form.Item label="MCP server name">
                  <Input
                    value={draftSettings.mcpName}
                    onChange={event => setDraftSettings(current => ({ ...current, mcpName: event.target.value }))}
                  />
                </Form.Item>
              </Col>
              <Col xs={24} md={8}>
                <Form.Item label="Local MCP port">
                  <InputNumber
                    min={1024}
                    max={65535}
                    value={draftSettings.mcpPort}
                    onChange={value => setDraftSettings(current => ({ ...current, mcpPort: value ?? 4107 }))}
                    style={{ width: '100%' }}
                  />
                </Form.Item>
              </Col>
            </Row>
            <Form.Item label="Services">
              <Checkbox.Group
                value={draftSettings.services}
                onChange={values => setDraftSettings(current => ({ ...current, services: values.map(String) }))}
              >
                <Row gutter={[10, 10]}>
                  {SERVICE_OPTIONS.map(service => (
                    <Col xs={24} md={8} key={service.id}>
                      <Card size="small" type="inner">
                        <Checkbox value={service.id}>
                          <Space direction="vertical" size={2}>
                            <Text strong>{service.label}</Text>
                            <Text type="secondary" style={{ fontSize: 12 }}>{service.detail}</Text>
                          </Space>
                        </Checkbox>
                      </Card>
                    </Col>
                  ))}
                </Row>
              </Checkbox.Group>
            </Form.Item>
          </Form>
        </Modal>
      </main>
    </ConfigProvider>
  );
}
