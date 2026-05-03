import { defineConfig } from 'vite';
import react from '@vitejs/plugin-react';
import wasm from 'vite-plugin-wasm';
import { readFileSync } from 'fs';
import path from 'path';
import os from 'os';

function getHttpsConfig() {
  const configDir = path.join(os.homedir(), '.config', 'ShellAnyWhere');
  const certPath = path.join(configDir, 'server.crt');
  const keyPath = path.join(configDir, 'server.key');
  try {
    return {
      cert: readFileSync(certPath),
      key: readFileSync(keyPath),
    };
  } catch {
    return true;
  }
}

export default defineConfig({
  plugins: [wasm(), react()],
  base: '/',
  build: {
    outDir: 'dist',
    emptyOutDir: true,
    target: 'esnext',
    sourcemap: true,
    rollupOptions: {
      output: {
        manualChunks(id) {
          if (id.includes('node_modules')) {
            if (id.includes('eruda')) return 'eruda';
            if (id.includes('/react/') || id.includes('/react-dom/')) return 'vendor-react';
            if (id.includes('lz4-wasm')) return 'vendor-lz4';
            return undefined;
          }
          // Split app code into feature chunks
          if (id.includes('/wterm/core/')) return 'wterm-core';
          if (id.includes('/wterm/dom/')) return 'wterm-dom';
          if (id.includes('/wterm/react/')) return 'wterm-react';
          if (id.includes('/protocol/')) return 'protocol';
        },
      },
    },
  },
  server: {
    https: getHttpsConfig(),
  },
});
