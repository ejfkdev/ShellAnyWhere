/// Login dialog: collect token and connect.

import { useState } from 'react';
import { StoredConfig } from '../store';

interface Props {
  initialConfig: StoredConfig;
  onLogin: (cfg: StoredConfig) => void;
  error: string;
  connecting: boolean;
}

export function LoginDialog({ initialConfig, onLogin, error, connecting }: Props) {
  const [serverToken, setServerToken] = useState(initialConfig.serverToken);

  const handleConnect = () => {
    onLogin({ serverToken, agentToken: '', fontSize: initialConfig.fontSize });
  };

  const handleKeyDown = (e: React.KeyboardEvent) => {
    if (e.key === 'Enter') handleConnect();
  };

  return (
    <div className="login-overlay">
      <div className="login-card">
        <div className="login-header">
          <h1>ShellAnyWhere</h1>
          <a className="github-link" href="https://github.com/ejfkdev/ShellAnyWhere" target="_blank" rel="noopener noreferrer" aria-label="GitHub">
            <svg viewBox="0 0 16 16" width="22" height="22" fill="currentColor">
              <path d="M8 0C3.58 0 0 3.58 0 8c0 3.54 2.29 6.53 5.47 7.59.4.07.55-.17.55-.38 0-.19-.01-.82-.01-1.49-2.01.37-2.53-.49-2.69-.94-.09-.23-.48-.94-.82-1.13-.28-.15-.68-.52-.01-.53.63-.01 1.08.58 1.23.82.72 1.21 1.87.87 2.33.66.07-.52.28-.87.51-1.07-1.78-.2-3.64-.89-3.64-3.95 0-.87.31-1.59.82-2.15-.08-.2-.36-1.02.08-2.12 0 0 .67-.21 2.2.82.64-.18 1.32-.27 2-.27.68 0 1.36.09 2 .27 1.53-1.04 2.2-.82 2.2-.82.44 1.1.16 1.92.08 2.12.51.56.82 1.27.82 2.15 0 3.07-1.87 3.75-3.65 3.95.29.25.54.73.54 1.48 0 1.07-.01 1.93-.01 2.2 0 .21.15.46.55.38A8.013 8.013 0 0016 8c0-4.42-3.58-8-8-8z"/>
            </svg>
          </a>
        </div>
        <p className="subtitle">Web Terminal</p>

        <label>
          Token
          <input
            type="password"
            value={serverToken}
            onChange={e => setServerToken(e.target.value)}
            onKeyDown={handleKeyDown}
            placeholder="Enter server token"
            autoFocus
          />
        </label>

        {error && <div className="error">{error}</div>}

        <button type="button" className="connect-btn" onClick={handleConnect} disabled={connecting}>
          {connecting ? 'Connecting...' : 'Connect'}
        </button>
      </div>
    </div>
  );
}
