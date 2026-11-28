// Offline tests of the shared asset's real functions. WebKit's JavaScriptCore
// runs these without a window, DOM renderer, CDN, filesystem grants or IPC.
function check(value, message) { if (!value) throw new Error(message); }
function loadFunction(name) {
  const match = new RegExp(`  (?:async )?function ${name}\\(`).exec(asset);
  check(match, `missing shared function ${name}`);
  const rest = asset.slice(match.index);
  const end = rest.slice(1).search(/\n  (?:async )?function |\n  const requested/);
  (0, eval)(end < 0 ? rest : rest.slice(0, end + 1));
}
function classes() {
  const values = new Set();
  return { add: v => values.add(v), remove: v => values.delete(v),
    contains: v => values.has(v), toggle(v) { values.has(v) ? values.delete(v) : values.add(v); } };
}
function node(tag) {
  return { tagName: tag, classList: classes(), events: {},
    addEventListener(name, fn) { this.events[name] = fn; } };
}
var nodes = { 'collapse-all': node('BUTTON'), 'expand-all': node('BUTTON') };
var h2s = [node('H2'), node('H2')];
var sections = [node('DIV'), node('DIV')];
h2s.forEach((h, i) => {
  h.id = `section-${i}`;
  h.nextElementSibling = sections[i];
  h.closest = () => null;
  sections[i].classList.add('section');
  sections[i].previousElementSibling = h;
  nodes[h.id] = h;
});
var childHeading = node('H3');
childHeading.id = 'details';
childHeading.closest = () => sections[0];
nodes.details = childHeading;
var links = [node('A'), node('A')];
links[0].dataset = {target: 'section-1'};
links[1].dataset = {target: 'details'};
var events = {};
var document = {
  getElementById: id => nodes[id],
  querySelectorAll: selector => ({'main h2': h2s, 'main .section': sections, '.toc a': links})[selector],
  addEventListener: (name, fn) => { events[name] = fn; }
};
var tocLabel = {style: {}}, toc = {innerHTML: ''};
if (testName === 'toc') {
  // Supply parser heading tokens, as marked does; exercise the actual renderer
  // callback, slugging, duplicate IDs, depth filtering and TOC construction.
  var options;
  var marked = {
    Renderer: function() {},
    setOptions: value => { options = value; },
    parseInline: text => text,
    parse: () => [
      {depth: 1, text: 'Overview'},
      {depth: 2, text: 'Overview'},
      {depth: 3, text: '**Details**'},
      {depth: 2, text: 'Overview'},
      {depth: 4, text: 'Excluded'}
    ].map(token => options.renderer.heading(token)).join('')
  };
  ['slugify', 'escapeHtml', 'renderMarkdown', 'renderTOC'].forEach(loadFunction);
  const rendered = renderMarkdown('placeholder heading tokens');
  check(JSON.stringify(rendered.headings) === JSON.stringify([
    {depth: 2, text: 'Overview', id: 'overview-2'},
    {depth: 3, text: '**Details**', id: 'details'},
    {depth: 2, text: 'Overview', id: 'overview-3'}
  ]), 'TOC must use H2/H3 with stable duplicate IDs');
  check(rendered.html.includes('<h2 id="overview-2">'), 'heading anchor');
  renderTOC(rendered.headings);
  check(toc.innerHTML.includes('href="#details" class="h3"'), 'nested TOC entry');
  check(!toc.innerHTML.includes('Excluded'), 'exclude H4');
  check(tocLabel.style.display === 'block', 'show TOC');
  renderTOC([]);
  check(toc.innerHTML === '' && tocLabel.style.display === 'none', 'clear empty TOC');
} else if (testName === 'collapse') {
  ['setupCollapsing', 'setupTOCLinks'].forEach(loadFunction);
  setupCollapsing();
  h2s[0].events.click();
  check(h2s[0].classList.contains('collapsed') && sections[0].classList.contains('collapsed'), 'collapse one');
  check(!sections[1].classList.contains('collapsed'), 'one leaves neighbour expanded');
  h2s[0].events.click();
  check(!sections[0].classList.contains('collapsed'), 'expand one');
  nodes['collapse-all'].onclick();
  check([...h2s, ...sections].every(n => n.classList.contains('collapsed')), 'collapse all');
  setupTOCLinks();
  links[1].events.click();
  check(!sections[0].classList.contains('collapsed') && !h2s[0].classList.contains('collapsed'), 'H3 TOC expands parent');
  links[0].events.click();
  check(!sections[1].classList.contains('collapsed'), 'H2 TOC expands section');
  nodes['collapse-all'].onclick();
  nodes['expand-all'].onclick();
  check([...h2s, ...sections].every(n => !n.classList.contains('collapsed')), 'expand all');
} else if (testName === 'drop') {
  var dropZone = node('DIV');
  var loads = [], reads = [];
  var loadPlan = (name, text) => loads.push({name, text});
  var FileReader = function() {
    this.readAsText = file => { reads.push(file.name); this.result = file.text; this.onload(); };
  };
  loadFunction('setupDrop');
  setupDrop();
  let prevented = 0;
  events.dragover({preventDefault() { prevented++; }});
  check(dropZone.classList.contains('over'), 'drop affordance');
  events.dragleave();
  check(!dropZone.classList.contains('over'), 'clear drag affordance');
  const drop = files => events.drop({preventDefault() { prevented++; }, dataTransfer: {files}});
  drop([{name: 'placeholder.md', text: '# Placeholder\n## Dropped'}, {name: 'ignored.md', text: 'ignored'}]);
  check(JSON.stringify(loads) === JSON.stringify([{name: 'placeholder.md', text: '# Placeholder\n## Dropped'}]), 'display first Markdown file without copying');
  drop([{name: 'ignored.txt', text: 'not Markdown'}]);
  drop([{name: 'ignored.MD', text: 'case sensitive like Iced'}]);
  drop([]);
  check(reads.length === 1 && loads.length === 1, 'ignore unsupported drops');
  check(prevented === 5 && !dropZone.classList.contains('over'), 'prevent browser file navigation');
} else if (testName === 'escape') {
  var closes = [];
  var window = {webkit: {messageHandlers: {gittermViewer: {postMessage: value => closes.push(value)}}}};
  (0, eval)(escapeScript.replace('<script>', '').replace('</script>', ''));
  events.keydown({key: 'a'});
  check(closes.length === 0, 'ordinary keys do not close');
  events.keydown({key: 'Escape'});
  check(JSON.stringify(closes) === '["close"]', 'Escape only asks to close');
} else {
  throw new Error(`unknown test ${testName}`);
}
