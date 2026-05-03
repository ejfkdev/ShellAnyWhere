import { StrictMode } from 'react';
import { createRoot } from 'react-dom/client';
import { App } from './App';
import './style.css';

// Render the app immediately — don't block on font loading.
// On mobile, blocking on fonts can cause the page to appear stuck
// (especially after returning from background).  Fonts load async
// and wterm will re-measure once they're ready.
const FONT_LOADS = [
  { family: '14px "FiraCode Nerd Font Mono"', testChars: '\uE0A0\uE0B0\uF410\uF411' },
  { family: '14px "LXGW WenKai Mono"', testChars: '\u4E2D\u6587' },
  { family: '14px "Noto Sans Symbols 2"', testChars: '\u23FA\u2736' },
];

createRoot(document.getElementById('root')!).render(
  <StrictMode>
    <App />
  </StrictMode>,
);

// Load fonts in the background.  The terminal will re-render with
// correct metrics once the fonts become available.
Promise.allSettled(
  FONT_LOADS.map(f => document.fonts.load(f.family, f.testChars)),
).then(() => {
  // Dispatch a resize event so wterm recalculates character widths
  window.dispatchEvent(new Event('resize'));
});
