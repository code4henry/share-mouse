import { useState, useEffect, useCallback } from 'react';
import { invoke } from '@tauri-apps/api/core';
import './index.css';

// Types matching the Rust backend
interface RoleInfo { role: string }
interface CursorInfo { owner: string; peer_id: string | null }
interface PeerInfo { id: string; addr: string }

interface LogEntry {
  time: string;
  msg: string;
  level: 'info' | 'warn' | 'error';
}

// ─── Helpers ────────────────────────────────────────────
function timeNow(): string {
  return new Date().toLocaleTimeString('en-US', { hour12: false });
}

// ─── App ────────────────────────────────────────────────
export default function App() {
  const [role, setRole] = useState<string>('none');
  const [peers, setPeers] = useState<PeerInfo[]>([]);
  const [connectAddr, setConnectAddr] = useState('');
  const [serverPort] = useState(24800);
  const [localScreen, setLocalScreen] = useState<[number, number]>([0, 0]);
  const [logs, setLogs] = useState<LogEntry[]>([]);
  const [cursorInfo, setCursorInfo] = useState<CursorInfo>({ owner: 'local', peer_id: null });
  const [loading, setLoading] = useState(false);

  const addLog = useCallback((msg: string, level: LogEntry['level'] = 'info') => {
    setLogs(prev => [...prev.slice(-100), { time: timeNow(), msg, level }]);
  }, []);

  // Fetch initial state
  useEffect(() => {
    (async () => {
      try {
        const r = await invoke<RoleInfo>('get_role');
        setRole(r.role);
      } catch { /* not ready yet */ }

      try {
        const size = await invoke<[number, number]>('get_local_screen_size');
        setLocalScreen(size);
      } catch { /* ignore */ }
    })();
  }, []);

  // Poll for peers and cursor state
  useEffect(() => {
    const interval = setInterval(async () => {
      try {
        const p = await invoke<PeerInfo[]>('get_peers');
        setPeers(p);
      } catch { /* ignore */ }

      try {
        const c = await invoke<CursorInfo>('get_cursor_state');
        setCursorInfo(c);
      } catch { /* ignore */ }
    }, 2000);
    return () => clearInterval(interval);
  }, []);

  // ── Actions ──
  const becomeHost = async () => {
    setLoading(true);
    try {
      await invoke('set_role_host');
      setRole('host');
      addLog(`Started as Host (port ${serverPort})`);
    } catch (e) {
      addLog(`Failed to start host: ${e}`, 'error');
    }
    setLoading(false);
  };

  const becomeClient = async () => {
    setLoading(true);
    try {
      await invoke('set_role_client');
      setRole('client');
      addLog('Switched to Client mode');
    } catch (e) {
      addLog(`Failed: ${e}`, 'error');
    }
    setLoading(false);
  };

  const connectToHost = async () => {
    if (!connectAddr.trim()) return;
    setLoading(true);
    try {
      const peerId = await invoke<string>('connect_to_host', { addr: connectAddr });
      addLog(`Connected to ${connectAddr} (peer: ${peerId.slice(0, 8)}...)`);
      setConnectAddr('');
    } catch (e) {
      addLog(`Connection failed: ${e}`, 'error');
    }
    setLoading(false);
  };

  const stop = async () => {
    try {
      await invoke('stop_engine');
      setRole('none');
      setPeers([]);
      addLog('Engine stopped');
    } catch (e) {
      addLog(`Stop failed: ${e}`, 'error');
    }
  };

  const isConnected = role !== 'none';
  const isHost = role === 'host';

  return (
    <>
      {/* Header */}
      <header className="header">
        <div className="header-title">
          <svg viewBox="0 0 24 24" fill="none" stroke="currentColor" strokeWidth="2">
            <rect x="2" y="3" width="20" height="14" rx="2" />
            <line x1="8" y1="21" x2="16" y2="21" />
            <line x1="12" y1="17" x2="12" y2="21" />
          </svg>
          <h1>ShareMouse</h1>
        </div>
        <div className="header-status">
          <span className={`status-dot ${isConnected ? 'active' : ''}`} />
          <span>
            {role === 'none' && 'Not connected'}
            {role === 'host' && `Host · ${peers.length} peer${peers.length !== 1 ? 's' : ''}`}
            {role === 'client' && `Client · ${cursorInfo.owner === 'local' ? 'Cursor here' : 'Cursor remote'}`}
          </span>
        </div>
      </header>

      <div className="main">
        {/* Sidebar */}
        <aside className="sidebar">
          {/* Mode selector */}
          <div className="sidebar-section">
            <h3>Mode</h3>
            <div className="mode-selector">
              <button
                className={`mode-btn ${isHost ? 'active' : ''}`}
                onClick={becomeHost}
                disabled={loading}
              >
                <span className="icon">🖥️</span>
                <span className="label">Host</span>
              </button>
              <button
                className={`mode-btn ${role === 'client' ? 'active' : ''}`}
                onClick={becomeClient}
                disabled={loading}
              >
                <span className="icon">💻</span>
                <span className="label">Client</span>
              </button>
            </div>
          </div>

          {/* Connect (client mode) */}
          <div className="sidebar-section">
            <h3>Connect to Host</h3>
            <div className="connect-form">
              <input
                className="input"
                placeholder="192.168.1.100:24800"
                value={connectAddr}
                onChange={e => setConnectAddr(e.target.value)}
                onKeyDown={e => e.key === 'Enter' && connectToHost()}
                disabled={loading}
              />
              <button
                className="btn btn-primary"
                onClick={connectToHost}
                disabled={loading || !connectAddr.trim()}
              >
                Connect
              </button>
            </div>
          </div>

          {/* Peer list */}
          <div className="sidebar-section" style={{ flex: 1 }}>
            <h3>Connected Devices</h3>
            {peers.length === 0 ? (
              <div className="empty-peers">
                No devices connected yet.
                <br />
                {isHost
                  ? 'Waiting for clients to connect...'
                  : 'Connect to a host to get started.'}
              </div>
            ) : (
              <ul className="peer-list">
                {peers.map(p => (
                  <li key={p.id} className="peer-item">
                    <div className="peer-icon">💻</div>
                    <div className="peer-info">
                      <div className="peer-name">
                        {p.id.slice(0, 8)}…
                      </div>
                      <div className="peer-addr">{p.addr}</div>
                    </div>
                    <div className="peer-status" />
                  </li>
                ))}
              </ul>
            )}
          </div>

          {/* Actions */}
          {isConnected && (
            <div className="sidebar-section">
              <button className="btn btn-danger" style={{ width: '100%' }} onClick={stop}>
                Disconnect
              </button>
            </div>
          )}
        </aside>

        {/* Main content */}
        <div className="content">
          {!isConnected ? (
            <WelcomeScreen />
          ) : (
            <ScreenCanvas
              localScreen={localScreen}
              peers={peers}
              isHost={isHost}
            />
          )}

          {/* Log area */}
          {logs.length > 0 && (
            <div className="log-area">
              <h3>Activity Log</h3>
              <div className="log-entries">
                {logs.map((l, i) => (
                  <div key={i} className="log-entry">
                    <span className="log-time">{l.time}</span>
                    <span className={`log-msg ${l.level}`}>{l.msg}</span>
                  </div>
                ))}
              </div>
            </div>
          )}
        </div>
      </div>
    </>
  );
}

// ─── Welcome Screen ─────────────────────────────────────
function WelcomeScreen() {
  return (
    <div className="welcome">
      <h2>Share Mouse &amp; Keyboard</h2>
      <p>
        Seamlessly share a single mouse and keyboard between your Mac and Windows machines
        over your local network.
      </p>
      <div className="steps">
        <div className="step">
          <div className="step-num">1</div>
          <div className="step-text">
            On the machine with the <strong>physical mouse &amp; keyboard</strong>, click{' '}
            <strong>Host</strong> to start the server.
          </div>
        </div>
        <div className="step">
          <div className="step-num">2</div>
          <div className="step-text">
            On the other machine, enter the host's <strong>IP address</strong> and click{' '}
            <strong>Connect</strong>.
          </div>
        </div>
        <div className="step">
          <div className="step-num">3</div>
          <div className="step-text">
            Move your cursor to the <strong>screen edge</strong> — it will seamlessly
            appear on the other machine!
          </div>
        </div>
      </div>
    </div>
  );
}

// ─── Screen Layout Canvas ───────────────────────────────
function ScreenCanvas({
  localScreen,
  peers,
  isHost,
}: {
  localScreen: [number, number];
  peers: PeerInfo[];
  isHost: boolean;
}) {
  const totalDevices = 1 + peers.length;

  return (
    <div className="screen-layout">
      {/* Local screen block — always present */}
      <div
        className="screen-block local"
        style={{
          left: `${(100 / totalDevices) * 0}%`,
          top: '15%',
          width: `${100 / totalDevices - 2}%`,
          height: '70%',
        }}
      >
        <span className="screen-label">
          {isHost ? '🖥️ This Mac (Host)' : '💻 This Mac (Client)'}
        </span>
        <span className="screen-res">
          {localScreen[0]}×{localScreen[1]}
        </span>
      </div>

      {/* Remote peer screens */}
      {peers.map((p, i) => (
        <div
          key={p.id}
          className="screen-block"
          style={{
            left: `${(100 / totalDevices) * (i + 1)}%`,
            top: '15%',
            width: `${100 / totalDevices - 2}%`,
            height: '70%',
          }}
        >
          <span className="screen-label">💻 Remote</span>
          <span className="screen-res">{p.addr}</span>
        </div>
      ))}
    </div>
  );
}
