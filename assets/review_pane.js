// Review pane (TRU-178). The page arrives fully rendered and escaped by
// Rust (src/review_pane.rs); this script only adds behaviour and posts the
// pane's messages back through window.ipc, each stamped with the page token.
(function () {
  'use strict';
  const body = document.body;
  const token = body.dataset.token || '';

  function post(message) {
    message.token = token;
    const ipc = window.ipc;
    if (!ipc || typeof ipc.postMessage !== 'function') {
      console.warn('[review] no IPC bridge; dropped', message.type);
      return;
    }
    ipc.postMessage(JSON.stringify(message));
  }

  // Toolbar.
  const hide = document.getElementById('hide-noise');
  function applyNoise() {
    body.classList.toggle('hide-noise', !!(hide && hide.checked));
  }
  if (hide) hide.addEventListener('change', applyNoise);
  document.querySelectorAll('button[data-source]').forEach((button) => {
    button.addEventListener('click', () => {
      if (button.disabled || button.classList.contains('on')) return;
      post({ type: 'review.source', source: button.dataset.source, hide_noise: !!(hide && hide.checked) });
    });
  });
  const close = document.getElementById('close');
  if (close) close.addEventListener('click', () => post({ type: 'review.close' }));
  document.addEventListener('keydown', (event) => {
    if (event.key === 'Escape') {
      event.preventDefault();
      post({ type: 'review.close' });
    }
  });

  // Tree.
  const files = document.getElementById('files');
  function treeItem(index) {
    return document.querySelector('#tree .tf[data-idx="' + index + '"]');
  }
  let current = null;
  function markCurrent(item) {
    if (current) current.classList.remove('current');
    current = item;
    if (current) current.classList.add('current');
  }
  document.querySelectorAll('#tree .tf').forEach((item) => {
    item.addEventListener('click', () => {
      const section = document.getElementById('f' + item.dataset.idx);
      if (!section) return;
      section.scrollIntoView({ block: 'start' });
      markCurrent(item);
    });
  });

  // File sections.
  document.querySelectorAll('section.file').forEach((section) => {
    const chev = section.querySelector('.chev');
    if (chev) chev.addEventListener('click', () => section.classList.toggle('collapsed'));
    const viewed = section.querySelector('input.viewed');
    if (viewed) {
      viewed.addEventListener('change', () => {
        const on = viewed.checked;
        section.classList.toggle('viewed', on);
        section.classList.toggle('collapsed', on);
        section.classList.remove('stale');
        const item = treeItem(section.dataset.idx);
        if (item) {
          item.classList.toggle('viewed', on);
          item.classList.remove('stale');
        }
        // Keep the next file in view when this one folds away above it.
        if (on && files && section.getBoundingClientRect().top < files.getBoundingClientRect().top) {
          section.scrollIntoView({ block: 'start' });
        }
        post({ type: 'review.viewed', path: section.dataset.path, viewed: on });
      });
    }
    const open = section.querySelector('button.open');
    if (open) open.addEventListener('click', () => post({ type: 'review.open_file', path: section.dataset.path }));
  });

  // Syntax highlighting, by extension, as sections come into view. Each
  // line is highlighted on its own (hljs escapes what it returns).
  const LANGUAGES = {
    rs: 'rust', ts: 'typescript', tsx: 'typescript', mts: 'typescript', cts: 'typescript',
    js: 'javascript', jsx: 'javascript', mjs: 'javascript', cjs: 'javascript',
    py: 'python', go: 'go', rb: 'ruby', java: 'java', kt: 'kotlin', kts: 'kotlin',
    swift: 'swift', c: 'c', h: 'c', cc: 'cpp', cpp: 'cpp', cxx: 'cpp', hpp: 'cpp', hh: 'cpp',
    m: 'objectivec', mm: 'objectivec', cs: 'csharp', php: 'php', sh: 'bash', bash: 'bash',
    zsh: 'bash', json: 'json', jsonc: 'json', yml: 'yaml', yaml: 'yaml', toml: 'ini', ini: 'ini',
    cfg: 'ini', md: 'markdown', markdown: 'markdown', html: 'xml', htm: 'xml', xml: 'xml',
    svg: 'xml', plist: 'xml', css: 'css', scss: 'scss', less: 'less', sql: 'sql', lua: 'lua',
    graphql: 'graphql', gql: 'graphql', pl: 'perl', r: 'r', vb: 'vbnet', diff: 'diff', patch: 'diff'
  };
  const NAMES = { makefile: 'makefile', 'cargo.lock': 'ini', dockerfile: 'bash' };
  function languageFor(path) {
    const name = (path || '').split('/').pop().toLowerCase();
    if (NAMES[name]) return NAMES[name];
    const dot = name.lastIndexOf('.');
    return dot > 0 ? LANGUAGES[name.slice(dot + 1)] : undefined;
  }
  function highlight(section) {
    if (section.dataset.highlighted) return;
    section.dataset.highlighted = '1';
    const hljs = window.hljs;
    if (!hljs) return;
    const language = languageFor(section.dataset.path);
    if (!language || !hljs.getLanguage(language)) return;
    section.querySelectorAll('td.c').forEach((cell) => {
      const text = cell.textContent;
      if (!text) return;
      try {
        cell.innerHTML = hljs.highlight(text, { language: language, ignoreIllegals: true }).value;
      } catch (error) {
        console.warn('[review] highlight failed for', section.dataset.path, error);
      }
    });
  }
  const sections = document.querySelectorAll('section.file');
  if ('IntersectionObserver' in window && files) {
    const observer = new IntersectionObserver((entries) => {
      entries.forEach((entry) => {
        if (entry.isIntersecting) {
          highlight(entry.target);
          observer.unobserve(entry.target);
        }
      });
    }, { root: files, rootMargin: '800px 0px' });
    sections.forEach((section) => observer.observe(section));
  } else {
    sections.forEach(highlight);
  }

  applyNoise();
})();
