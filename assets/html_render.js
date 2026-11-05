// Shared by chat and the isolated CDP preview. No agent HTML enters the host DOM.
(() => {
  'use strict';
  const CSP = "default-src 'none'; script-src 'unsafe-inline'; style-src 'unsafe-inline'; img-src data:; connect-src 'none'; frame-src 'none'; child-src 'none'; worker-src 'none'; object-src 'none'; media-src 'none'; font-src 'none'; base-uri 'none'; form-action 'none'; webrtc 'block'";
  const palettes = {
    dark: { bg: '#1e1e2e', 'bg-alt': '#181825', 'bg-block': '#313244', fg: '#bac2de', 'fg-muted': '#7f849c', accent: '#89b4fa', border: '#45475a', user: '#a6e3a1', result: '#94e2d5', tool: '#fab387', warn: '#f9e2af', err: '#f38ba8' },
    light: { bg: '#eff1f5', 'bg-alt': '#e6e9ef', 'bg-block': '#ccd0da', fg: '#4c4f69', 'fg-muted': '#6c6f85', accent: '#1e66f5', border: '#bcc0cc', user: '#40a02b', result: '#179299', tool: '#fe640b', warn: '#df8e1d', err: '#d20f39' }
  };
  function themeCss(theme) {
    const colors = palettes[theme === 'light' ? 'light' : 'dark'];
    return ':root{color-scheme:' + (theme === 'light' ? 'light' : 'dark') + ';' + Object.entries(colors).map(([k, v]) => '--' + k + ':' + v).join(';') + ';--mono:ui-monospace,SFMono-Regular,Menlo,monospace}';
  }
  // This executes BEFORE all agent HTML. The private port is never exposed
  // to agent code; a forged window message cannot claim a trusted link click.
  function bootstrap(html) {
    'use strict';
    const channel = new MessageChannel();
    const send = channel.port1.postMessage.bind(channel.port1);
    const add = window.addEventListener.bind(window);
    const stringify = JSON.stringify.bind(JSON);
    const StringValue = String;
    const rootStyle = document.createElement('style');
    document.head.appendChild(rootStyle);
    channel.port1.onmessage = (e) => {
      if (typeof e.data === 'string') rootStyle.textContent = e.data;
    };
    parent.postMessage({ type: 'gitterm-html-ready' }, '*', [channel.port2]);
    if (window.__gittermHtmlGuard !== true || typeof RTCPeerConnection !== 'undefined' || typeof webkitRTCPeerConnection !== 'undefined') {
      send({ type: 'guard-error' });
      return;
    }
    add('click', (e) => {
      const a = e.target && e.target.closest && e.target.closest('a');
      if (!a) return;
      e.preventDefault();
      if (e.isTrusted) send({ type: 'link', url: a.getAttribute('href') });
    }, true);
    add('submit', (e) => e.preventDefault(), true);
    add('keydown', (e) => { if (e.isTrusted && e.key === 'Escape') send({ type: 'escape' }); }, true);
    for (const level of ['log', 'info', 'warn', 'error']) {
      const original = console[level].bind(console);
      console[level] = (...args) => {
        original(...args);
        const text = args.map((v) => {
          try { return typeof v === 'string' ? v : stringify(v); }
          catch (_) { return StringValue(v); }
        }).join(' ').slice(0, 4000);
        send({ type: 'console', level, text });
      };
    }
    add('error', (e) => send({ type: 'console', level: 'error', text: StringValue(e.message).slice(0, 4000) }));
    add('unhandledrejection', (e) => send({ type: 'console', level: 'error', text: StringValue(e.reason).slice(0, 4000) }));
    // Normal-flow content is measured independently of the iframe viewport.
    const measure = () => {
      const b = document.body;
      const height = b ? Math.ceil(Math.max(b.scrollHeight, b.getBoundingClientRect().bottom)) : 100;
      send({ type: 'height', height });
    };
    add('load', () => {
      measure();
      new ResizeObserver(measure).observe(document.body);
    }, { once: true });
    document.write(html);
  }
  function valid(html, height) {
    return typeof html === 'string' && html.trim() && new TextEncoder().encode(html).length <= 524288 && Number.isInteger(height) && height >= 100 && height <= 4000;
  }
  function httpUrl(value) {
    if (typeof value !== 'string' || value.length > 8192) return null;
    try {
      const url = new URL(value);
      return ['http:', 'https:'].includes(url.protocol) && !url.username && !url.password ? url.href : null;
    } catch (_) { return null; }
  }
  function create(html, height, theme, onLink, onEscape) {
    if (!valid(html, height)) throw new Error('HTML must be nonempty, at most 512 KiB; height must be 100–4000px');
    const frame = document.createElement('iframe');
    frame.title = 'Visual reply';
    frame.setAttribute('sandbox', 'allow-scripts');
    frame.setAttribute('referrerpolicy', 'no-referrer');
    frame.setAttribute('allow', "camera 'none'; microphone 'none'; geolocation 'none'; payment 'none'; usb 'none'; fullscreen 'none'");
    frame.style.cssText = 'display:block;border:0;width:100%;height:' + height + 'px';
    let port = null, connectedWindow = null, disposed = false;
    const state = { contentHeight: 100, consoleMessages: [], ready: false };
    const currentCss = () => themeCss(theme);
    const hello = (e) => {
      if (disposed || e.source !== frame.contentWindow || e.data?.type !== 'gitterm-html-ready') return;
      // Timeline virtualization detaches and later reinserts the iframe,
      // creating a fresh Window. Invalidate the old document's channel.
      if (port && connectedWindow !== e.source) { port.close(); port = null; }
      if (port || e.ports.length !== 1) { for (const p of e.ports) p.close(); return; }
      port = e.ports[0];
      connectedWindow = e.source;
      port.onmessage = (event) => {
        if (disposed) return;
        const d = event.data;
        if (!d || typeof d !== 'object') return;
        if (d.type === 'guard-error') {
          state.guardError = true;
          frame.srcdoc = '<meta http-equiv="Content-Security-Policy" content="' + CSP + '"><p>Visual reply unavailable: browser isolation guard did not initialize.</p>';
        } else if (d.type === 'link') {
          const url = httpUrl(d.url);
          if (url && onLink) onLink(url);
        } else if (d.type === 'escape' && onEscape) onEscape();
        else if (d.type === 'height' && Number.isFinite(d.height)) {
          state.contentHeight = Math.max(100, Math.min(20000, Math.ceil(d.height)));
          state.ready = true;
        } else if (d.type === 'console' && typeof d.text === 'string' && ['log', 'info', 'warn', 'error'].includes(d.level) && state.consoleMessages.length < 100) {
          state.consoleMessages.push({ level: d.level, text: d.text.slice(0, 4000) });
        }
      };
      port.postMessage(currentCss());
    };
    window.addEventListener('message', hello);
    // Fixed prefix keeps the policy and bridge ahead of even a hostile <head>.
    frame.srcdoc = '<!doctype html><html><head><meta charset="utf-8"><meta http-equiv="Content-Security-Policy" content="' + CSP + '"><style>' + currentCss() + 'html,body{margin:0;padding:0;background:var(--bg);color:var(--fg);font:14px system-ui}*{box-sizing:border-box}</style><script>(' + bootstrap.toString() + ')(' + JSON.stringify(html).replace(/</g, '\\u003c') + ')<\/script></head></html>';
    return {
      frame, state,
      setTheme(value) { theme = value; if (port) port.postMessage(currentCss()); },
      dispose() { disposed = true; window.removeEventListener('message', hello); if (port) port.close(); frame.remove(); }
    };
  }
  window.GitTermHtml = Object.freeze({ create, valid, httpUrl, themeCss });
})();
