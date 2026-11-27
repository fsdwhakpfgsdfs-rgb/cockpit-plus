import { useCallback, useEffect, useRef, useState } from 'react';
import { invoke } from '@tauri-apps/api/core';
import { Activity, Copy, KeyRound, Layers, Play, RefreshCw, Server, SlidersHorizontal, Trash2, Users } from 'lucide-react';
import './GatewayPage.css';

interface GatewayStatusResp {
  running: boolean;
  port: number;
  strategy: string;
  sessionAffinity: boolean;
  timeoutSecs?: number;
  maxAttempts?: number;
  quotaCooldownSecs?: number;
  proxy?: string | null;
  keys: { id: string; name: string; enabled: boolean; keyMasked: string }[];
  pool: {
    total: number;
    accounts: {
      id: string;
      name: string;
      email: string;
      platform: string;
      quotaLeft?: number | null;
      exhausted?: boolean;
      cooldownSecs?: number;
    }[];
  };
  stats: { total: number; ok: number; failed: number };
  sessions: number;
  baseUrl: string;
}

interface ManualProvider {
  id: string;
  name: string;
  baseUrl: string;
  models: string[];
  enabled: boolean;
  protocol: string;
  keyMasked?: string;
  hasKey?: boolean;
}

interface GatewayLog {
  id: string;
  at: string;
  method: string;
  endpoint: string;
  model: string;
  account_id: string;
  account_name: string;
  status: number;
  latency_ms: number;
  ok: boolean;
  error: string;
  stream: boolean;
}

const API = (port: number, path: string) => `http://127.0.0.1:${port}${path}`;

const EMPTY_PROVIDER = {
  id: '',
  name: '',
  baseUrl: '',
  apiKey: '',
  protocol: 'openai',
  enabled: true,
};

export default function GatewayPage() {
  const [status, setStatus] = useState<GatewayStatusResp | null>(null);
  const [logs, setLogs] = useState<GatewayLog[]>([]);
  const [providers, setProviders] = useState<ManualProvider[]>([]);
  const [err, setErr] = useState('');
  const [notice, setNotice] = useState('');
  const [busy, setBusy] = useState(false);
  const [copied, setCopied] = useState('');
  const [port, setPort] = useState(8787);

  const [cfgOpen, setCfgOpen] = useState(false);
  const [cfgForm, setCfgForm] = useState({ port: 8787, strategy: 'round_robin', timeoutSecs: 180, maxAttempts: 3, sessionAffinity: true, quotaCooldownSecs: 300, proxy: '' });

  const [keyOpen, setKeyOpen] = useState(false);
  const [keyName, setKeyName] = useState('');
  const [newKey, setNewKey] = useState('');
  const [keyErr, setKeyErr] = useState('');
  const [provOpen, setProvOpen] = useState(false);
  const [provForm, setProvForm] = useState({ ...EMPTY_PROVIDER });
  const [testing, setTesting] = useState(false);
  const [testResult, setTestResult] = useState('');

  const timer = useRef<number | null>(null);

  const load = useCallback(async (p?: number) => {
    const usePort = p ?? port;
    try {
      const r = await fetch(API(usePort, '/api/status'));
      if (!r.ok) throw new Error('status ' + r.status);
      setStatus(await r.json());
      const lg = await fetch(API(usePort, '/api/logs'));
      if (lg.ok) setLogs(await lg.json());
      setErr('');
    } catch {
      setStatus(null);
      setErr('网关未运行');
    }
  }, [port]);

  const loadProviders = useCallback(async (p?: number) => {
    try {
      const r = await fetch(API(p ?? port, '/api/providers'));
      if (r.ok) setProviders(await r.json());
    } catch {
      /* ignore */
    }
  }, [port]);

  const start = useCallback(async () => {
    setBusy(true);
    try {
      let target = port;
      try {
        const r = await invoke<{ running: boolean; port: number }>('gateway_start');
        target = r.port;
      } catch {
        /* 非 Tauri 或 IPC 不可用时，依赖已有实例 */
      }
      setPort(target);
      for (let i = 0; i < 20; i += 1) {
        await new Promise((res) => setTimeout(res, 300));
        try {
          const s = await fetch(API(target, '/api/status'));
          if (s.ok) break;
        } catch {
          /* retry */
        }
      }
      await load(target);
      await loadProviders(target);
    } finally {
      setBusy(false);
    }
  }, [load, loadProviders, port]);

  useEffect(() => {
    invoke<{ running: boolean; port: number }>('gateway_status')
      .then(async (r) => {
        setPort(r.port);
        await load(r.port);
        await loadProviders(r.port);
        if (!r.running) await start();
      })
      .catch(async () => {
        try {
          const probe = await fetch(API(8787, '/api/status'));
          if (probe.ok) {
            setPort(8787);
            await load(8787);
            await loadProviders(8787);
          }
        } catch {
          setErr('网关未运行，请点击「启动」');
        }
      });
    // eslint-disable-next-line react-hooks/exhaustive-deps
  }, []);

  useEffect(() => {
    if (timer.current) window.clearInterval(timer.current);
    timer.current = window.setInterval(() => void load(), 3000);
    return () => {
      if (timer.current) window.clearInterval(timer.current);
    };
  }, [load]);

  const flash = (msg: string) => {
    setNotice(msg);
    window.setTimeout(() => setNotice(''), 2200);
  };

  const copy = (text: string, tag: string) => {
    void navigator.clipboard.writeText(text);
    setCopied(tag);
    window.setTimeout(() => setCopied(''), 1500);
  };

  const saveConfig = async () => {
    try {
      const r = await fetch(API(port, '/api/config'), {
        method: 'POST',
        headers: { 'Content-Type': 'application/json' },
        body: JSON.stringify({
          port: cfgForm.port,
          enabled: true,
          keys: [],
          strategy: cfgForm.strategy,
          session_affinity: cfgForm.sessionAffinity,
          log_limit: 500,
          timeout_secs: cfgForm.timeoutSecs,
          max_attempts: cfgForm.maxAttempts,
          quota_cooldown_secs: cfgForm.quotaCooldownSecs,
          proxy: cfgForm.proxy.trim() ? cfgForm.proxy.trim() : null,
        }),
      });
      const j = await r.json();
      setCfgOpen(false);
      flash(j.message || '已保存');
      await load();
    } catch (e) {
      setErr(String(e));
    }
  };

  const createKey = async () => {
    setKeyErr('');
    try {
      const r = await fetch(API(port, '/api/keys'), {
        method: 'POST',
        headers: { 'Content-Type': 'application/json' },
        body: JSON.stringify({ name: keyName }),
      });
      const j = await r.json();
      if (!r.ok) {
        setKeyErr(String(j?.error?.message || '创建失败'));
        return;
      }
      setNewKey(j.key);
      setKeyName('');
      await load();
    } catch (e) {
      setKeyErr(String(e));
    }
  };

  const deleteKey = async (id: string) => {
    await fetch(API(port, '/api/keys/' + encodeURIComponent(id)), { method: 'DELETE' }).catch(() => {});
    await load();
    flash('密钥已删除');
  };

  const toggleKey = async (id: string) => {
    await fetch(API(port, '/api/keys/' + encodeURIComponent(id) + '/toggle'), { method: 'POST' }).catch(() => {});
    await load();
  };

  const saveProvider = async () => {
    if (!provForm.baseUrl.trim()) {
      setTestResult('Base URL 不能为空');
      return;
    }
    try {
      const r = await fetch(API(port, '/api/providers'), {
        method: 'POST',
        headers: { 'Content-Type': 'application/json' },
        body: JSON.stringify({
          id: provForm.id,
          name: provForm.name,
          base_url: provForm.baseUrl,
          api_key: provForm.apiKey,
          models: [],
          enabled: provForm.enabled,
          protocol: provForm.protocol,
        }),
      });
      if (!r.ok) {
        setTestResult('保存失败 HTTP ' + r.status);
        return;
      }
      setProvOpen(false);
      setTestResult('');
      await loadProviders();
      await fetch(API(port, '/api/pool/refresh'), { method: 'POST' }).catch(() => {});
      await load();
      flash('供应商已保存');
    } catch (e) {
      setTestResult(String(e));
    }
  };

  const removeProvider = async (id: string) => {
    await fetch(API(port, '/api/providers/' + encodeURIComponent(id)), { method: 'DELETE' }).catch(() => {});
    await loadProviders();
    await fetch(API(port, '/api/pool/refresh'), { method: 'POST' }).catch(() => {});
    await load();
    flash('已删除');
  };

  const testProvider = async () => {
    if (!provForm.baseUrl.trim()) {
      setTestResult('先填 Base URL');
      return;
    }
    setTesting(true);
    setTestResult('测试中…');
    try {
      const r = await fetch(API(port, '/api/providers/test'), {
        method: 'POST',
        headers: { 'Content-Type': 'application/json' },
        body: JSON.stringify({
          id: provForm.id,
          name: provForm.name,
          base_url: provForm.baseUrl,
          api_key: provForm.apiKey,
          models: [],
          enabled: true,
          protocol: provForm.protocol,
        }),
      });
      const j = await r.json();
      setTestResult((j.ok ? '✅ ' : '❌ ') + 'HTTP ' + j.status + ' · ' + j.latencyMs + 'ms · ' + String(j.body || '').slice(0, 140));
    } catch (e) {
      setTestResult('❌ ' + String(e));
    } finally {
      setTesting(false);
    }
  };

  const refreshPool = async () => {
    await fetch(API(port, '/api/pool/refresh'), { method: 'POST' }).catch(() => {});
    await load();
    await loadProviders();
    flash('号池已刷新');
  };

  const clearLogs = async () => {
    await fetch(API(port, '/api/logs'), { method: 'DELETE' }).catch(() => {});
    await load();
  };

  const clearSessions = async () => {
    await fetch(API(port, '/api/sessions'), { method: 'DELETE' }).catch(() => {});
    await load();
  };

  const running = status !== null;
  const okRate = status && status.stats.total > 0
    ? Math.round((status.stats.ok / status.stats.total) * 1000) / 10
    : 0;

  return (
    <div className="gw">
      <div className="gw-head">
        <div>
          <h1 className="gw-title"><Server size={22} /> 统一网关</h1>
          <p className="gw-sub">一个地址调用号池内全部账号，自动选路与故障转移</p>
        </div>
        <div className="gw-head-actions">
          <span className={'gw-pill ' + (running ? 'on' : 'off')}><i />{running ? '运行中' : '未启动'}</span>
          <button className="btn btn-secondary btn-sm icon-only" title="刷新" onClick={() => void load()}><RefreshCw size={14} /></button>
          <button className="btn btn-secondary btn-sm icon-only" title="刷新号池" onClick={() => void refreshPool()}><Users size={14} /></button>
          <button
            className="btn btn-secondary btn-sm"
            onClick={() => {
              if (status) {
                setCfgForm({
                  port: status.port,
                  strategy: status.strategy,
                  timeoutSecs: status.timeoutSecs ?? 180,
                  maxAttempts: status.maxAttempts ?? 3,
                  sessionAffinity: status.sessionAffinity,
                  quotaCooldownSecs: status.quotaCooldownSecs ?? 300,
                  proxy: status.proxy ?? '',
                });
              }
              setCfgOpen(true);
            }}
          >
            <SlidersHorizontal size={13} /> 配置
          </button>
          <button className="btn btn-primary btn-sm" onClick={() => void start()} disabled={busy}>
            <Play size={13} /> {busy ? '启动中…' : running ? '重启' : '启动'}
          </button>
        </div>
      </div>

      {err && <div className="gw-alert err">{err}</div>}
      {notice && <div className="gw-alert ok">{notice}</div>}

      {status && (
        <>
          <div className="gw-card">
            <div className="gw-url-label">统一调用地址</div>
            <div className="gw-url-row">
              <code>{status.baseUrl}</code>
              <button className="btn btn-secondary btn-sm" onClick={() => copy(status.baseUrl, 'url')}>
                <Copy size={12} /> {copied === 'url' ? '已复制' : '复制'}
              </button>
            </div>
            <div className="gw-url-hint">
              兼容 OpenAI <code>/chat/completions</code> 与 Anthropic <code>/messages</code>，鉴权用下方网关密钥
            </div>
          </div>

          <div className="gw-stats">
            <div className="gw-stat">
              <div className="gw-stat-icon"><Users size={16} /></div>
              <div><div className="gw-stat-label">号池账号</div><div className="gw-stat-value">{status.pool.total}</div></div>
            </div>
            <div className="gw-stat">
              <div className="gw-stat-icon"><Activity size={16} /></div>
              <div><div className="gw-stat-label">请求总数</div><div className="gw-stat-value">{status.stats.total}</div></div>
            </div>
            <div className="gw-stat">
              <div className="gw-stat-icon"><Activity size={16} /></div>
              <div><div className="gw-stat-label">成功率</div><div className="gw-stat-value">{okRate}%</div></div>
            </div>
            <div className="gw-stat">
              <div className="gw-stat-icon"><Layers size={16} /></div>
              <div><div className="gw-stat-label">活跃会话</div><div className="gw-stat-value">{status.sessions}</div></div>
            </div>
          </div>

          <div className="gw-cols">
            <div className="gw-card">
              <div className="gw-card-head">
                <KeyRound size={15} /> 网关密钥
                <span className="gw-spacer" />
                <button
                  className="btn btn-primary btn-sm"
                  onClick={() => {
                    setKeyName('');
                    setNewKey('');
                    setKeyErr('');
                    setKeyOpen(true);
                  }}
                >
                  + 新建
                </button>
              </div>
              {status.keys.length === 0 && <div className="gw-empty">未配置密钥</div>}
              {status.keys.map((k) => (
                <div className="gw-key" key={k.id}>
                  <span className="gw-key-name">{k.name}</span>
                  <code>{k.keyMasked}</code>
                  <span className={'gw-dot ' + (k.enabled ? 'on' : 'off')} />
                  <button className="gw-mini" onClick={() => void toggleKey(k.id)}>
                    {k.enabled ? '停用' : '启用'}
                  </button>
                  <button className="gw-mini danger" onClick={() => void deleteKey(k.id)}>删除</button>
                </div>
              ))}
            </div>

            <div className="gw-card">
              <div className="gw-card-head"><Server size={15} /> 路由</div>
              <div className="gw-kv"><span>策略</span><b>{status.strategy === 'round_robin' ? '轮询' : status.strategy === 'least_used' ? '最少使用' : status.strategy}</b></div>
              <div className="gw-kv"><span>会话亲和</span><b>{status.sessionAffinity ? '开启' : '关闭'}</b></div>
              <div className="gw-kv"><span>监听端口</span><b>{status.port}</b></div>
              <div className="gw-kv"><span>最大重试</span><b>{status.maxAttempts ?? 3}</b></div>
              <div className="gw-kv"><span>额度冷却</span><b>{status.quotaCooldownSecs ?? 300}s</b></div>
              <button className="btn btn-outline btn-sm" style={{ marginTop: 10 }} onClick={() => void clearSessions()}>
                清空会话绑定
              </button>
            </div>
          </div>

          <div className="gw-card">
            <div className="gw-card-head">
              <Users size={15} /> 号池账号（{status.pool.total}）
            </div>
            {status.pool.accounts.length === 0 ? (
              <div className="gw-empty">号池为空。在下方添加一个 OpenAI 兼容供应商，或先在平台页添加账号。</div>
            ) : (
              <div className="gw-list">
                {status.pool.accounts.map((a) => {
                  const cooling = (a.cooldownSecs ?? 0) > 0;
                  const state = a.exhausted ? '额度用尽' : cooling ? '冷却中 ' + a.cooldownSecs + 's' : '可用';
                  const tone = a.exhausted || cooling ? 'gw-state-warn' : 'gw-state-ok';
                  return (
                    <div className="gw-item" key={a.id}>
                      <span className="gw-item-name">{a.name}</span>
                      <span className="gw-item-meta">{a.platform}{a.email ? ' · ' + a.email : ''}</span>
                      <span className="gw-item-meta">
                        额度 {a.quotaLeft == null ? '未知' : a.quotaLeft + '%'} ·
                        <span className={tone}> {state}</span>
                      </span>
                    </div>
                  );
                })}
              </div>
            )}
          </div>

          <div className="gw-card">
            <div className="gw-card-head">
              <Server size={15} /> 手动供应商（任意 OpenAI 兼容端点）
              <span className="gw-spacer" />
              <button
                className="btn btn-primary btn-sm"
                onClick={() => {
                  setProvForm({ ...EMPTY_PROVIDER });
                  setTestResult('');
                  setProvOpen(true);
                }}
              >
                + 添加
              </button>
            </div>
            {providers.length === 0 ? (
              <div className="gw-empty">
                还没有供应商。可填任意 OpenAI 兼容地址，例如本地中转 <code>http://127.0.0.1:3065/v1</code>。
              </div>
            ) : (
              <div className="gw-list">
                {providers.map((p) => (
                  <div className="gw-item" key={p.id}>
                    <span className="gw-item-name">{p.name || p.baseUrl}</span>
                    <span className="gw-item-meta">{p.baseUrl}</span>
                    <span className="gw-item-meta">
                      {p.protocol} · {p.hasKey ? p.keyMasked : '无密钥'} · {p.enabled ? '启用' : '停用'}
                    </span>
                    <div className="gw-item-actions">
                      <button
                        className="btn btn-outline btn-sm"
                        onClick={() => {
                          setProvForm({
                            id: p.id,
                            name: p.name,
                            baseUrl: p.baseUrl,
                            apiKey: '',
                            protocol: p.protocol || 'openai',
                            enabled: p.enabled,
                          });
                          setTestResult('');
                          setProvOpen(true);
                        }}
                      >
                        编辑
                      </button>
                      <button className="btn btn-outline btn-sm" onClick={() => void removeProvider(p.id)}>删除</button>
                    </div>
                  </div>
                ))}
              </div>
            )}
          </div>

          <div className="gw-card">
            <div className="gw-card-head">
              <Activity size={15} /> 请求日志（{logs.length}）
              <span className="gw-spacer" />
              <button className="btn btn-outline btn-sm" onClick={() => void clearLogs()}>
                <Trash2 size={12} /> 清空
              </button>
            </div>
            {logs.length === 0 ? (
              <div className="gw-empty">还没有请求。用上面的地址调一次就会出现。</div>
            ) : (
              <div className="gw-logs">
                <table>
                  <thead>
                    <tr>
                      <th>时间</th><th>端点</th><th>模型</th><th>账号</th><th>状态</th><th>耗时</th>
                    </tr>
                  </thead>
                  <tbody>
                    {logs.slice().reverse().slice(0, 80).map((l) => (
                      <tr key={l.id}>
                        <td className="gw-mono">{l.at.slice(11, 19)}</td>
                        <td className="gw-mono">{l.endpoint}</td>
                        <td className="gw-mono">{l.model || '—'}</td>
                        <td>{l.account_name || '—'}</td>
                        <td><span className={'gw-code ' + (l.ok ? 'ok' : 'bad')}>{l.status || 'ERR'}</span></td>
                        <td className="gw-mono">{l.latency_ms}ms</td>
                      </tr>
                    ))}
                  </tbody>
                </table>
              </div>
            )}
          </div>
        </>
      )}

      {keyOpen && (
        <div className="gw-mask" onClick={() => { setKeyOpen(false); setNewKey(''); }}>
          <div className="gw-modal" onClick={(e) => e.stopPropagation()}>
            <h3>{newKey ? '密钥已创建' : '新建网关密钥'}</h3>
            {!newKey && (
              <>
                <label className="gw-field">
                  <span>名称（用于区分用途）</span>
                  <input
                    className="gw-input"
                    value={keyName}
                    onChange={(e) => setKeyName(e.target.value)}
                    placeholder="例如 Cursor / 笔记本"
                    autoFocus
                  />
                </label>
                {keyErr && <div className="gw-test-result">{keyErr}</div>}
                <div className="gw-modal-foot">
                  <button className="btn btn-secondary" onClick={() => setKeyOpen(false)}>取消</button>
                  <button className="btn btn-primary" onClick={() => void createKey()}>生成密钥</button>
                </div>
              </>
            )}
            {newKey && (
              <>
                <div className="gw-alert ok">复制保存好，关闭后不再显示明文。</div>
                <div className="gw-url-row" style={{ marginBottom: 12 }}>
                  <code>{newKey}</code>
                  <button className="btn btn-secondary btn-sm" onClick={() => copy(newKey, 'newkey')}>
                    <Copy size={12} /> {copied === 'newkey' ? '已复制' : '复制'}
                  </button>
                </div>
                <div className="gw-modal-foot">
                  <button
                    className="btn btn-primary"
                    onClick={() => {
                      setKeyOpen(false);
                      setNewKey('');
                    }}
                  >
                    完成
                  </button>
                </div>
              </>
            )}
          </div>
        </div>
      )}

      {cfgOpen && (
        <div className="gw-mask" onClick={() => setCfgOpen(false)}>
          <div className="gw-modal" onClick={(e) => e.stopPropagation()}>
            <h3>网关配置</h3>
            <label className="gw-field">
              <span>端口</span>
              <input className="gw-input" type="number" value={cfgForm.port} onChange={(e) => setCfgForm({ ...cfgForm, port: Number(e.target.value) || 8787 })} />
            </label>
            <label className="gw-field">
              <span>路由策略</span>
              <select className="gw-input" value={cfgForm.strategy} onChange={(e) => setCfgForm({ ...cfgForm, strategy: e.target.value })}>
                <option value="round_robin">轮询</option>
                <option value="least_used">最少使用</option>
              </select>
            </label>
            <label className="gw-field">
              <span>请求超时（秒）</span>
              <input className="gw-input" type="number" value={cfgForm.timeoutSecs} onChange={(e) => setCfgForm({ ...cfgForm, timeoutSecs: Number(e.target.value) || 180 })} />
            </label>
            <label className="gw-field">
              <span>故障转移最大尝试次数</span>
              <input className="gw-input" type="number" value={cfgForm.maxAttempts} onChange={(e) => setCfgForm({ ...cfgForm, maxAttempts: Number(e.target.value) || 3 })} />
            </label>
            <label className="gw-field">
              <span>出站代理（留空直连，例如 http://127.0.0.1:10808）</span>
              <input className="gw-input" value={cfgForm.proxy} onChange={(e) => setCfgForm({ ...cfgForm, proxy: e.target.value })} placeholder="http://127.0.0.1:10808" />
            </label>
            <label className="gw-field">
              <span>额度耗尽冷却（秒）</span>
              <input className="gw-input" type="number" value={cfgForm.quotaCooldownSecs} onChange={(e) => setCfgForm({ ...cfgForm, quotaCooldownSecs: Number(e.target.value) || 300 })} />
            </label>
            <label className="gw-check">
              <input type="checkbox" checked={cfgForm.sessionAffinity} onChange={(e) => setCfgForm({ ...cfgForm, sessionAffinity: e.target.checked })} />
              会话亲和（同一 x-session-id 固定同一账号）
            </label>
            <div className="gw-modal-foot">
              <button className="btn btn-secondary" onClick={() => setCfgOpen(false)}>取消</button>
              <button className="btn btn-primary" onClick={() => void saveConfig()}>保存</button>
            </div>
          </div>
        </div>
      )}

      {provOpen && (
        <div className="gw-mask" onClick={() => setProvOpen(false)}>
          <div className="gw-modal" onClick={(e) => e.stopPropagation()}>
            <h3>{provForm.id ? '编辑供应商' : '添加供应商'}</h3>
            <label className="gw-field">
              <span>名称</span>
              <input className="gw-input" value={provForm.name} onChange={(e) => setProvForm({ ...provForm, name: e.target.value })} placeholder="例如 本地中转" />
            </label>
            <label className="gw-field">
              <span>Base URL（OpenAI 兼容）</span>
              <input className="gw-input" value={provForm.baseUrl} onChange={(e) => setProvForm({ ...provForm, baseUrl: e.target.value })} placeholder="http://127.0.0.1:3065/v1" />
            </label>
            <label className="gw-field">
              <span>API Key{provForm.id ? '（留空保持原值）' : ''}</span>
              <input className="gw-input" type="password" value={provForm.apiKey} onChange={(e) => setProvForm({ ...provForm, apiKey: e.target.value })} placeholder="sk-..." />
            </label>
            <label className="gw-field">
              <span>协议</span>
              <select className="gw-input" value={provForm.protocol} onChange={(e) => setProvForm({ ...provForm, protocol: e.target.value })}>
                <option value="openai">OpenAI（Authorization: Bearer）</option>
                <option value="anthropic">Anthropic（x-api-key）</option>
              </select>
            </label>
            <label className="gw-check">
              <input type="checkbox" checked={provForm.enabled} onChange={(e) => setProvForm({ ...provForm, enabled: e.target.checked })} />
              启用
            </label>
            {testResult && <div className="gw-test-result">{testResult}</div>}
            <div className="gw-modal-foot">
              <button className="btn btn-secondary" onClick={() => void testProvider()} disabled={testing}>测试连通性</button>
              <button className="btn btn-secondary" onClick={() => setProvOpen(false)}>取消</button>
              <button className="btn btn-primary" onClick={() => void saveProvider()}>保存</button>
            </div>
          </div>
        </div>
      )}
    </div>
  );
}
