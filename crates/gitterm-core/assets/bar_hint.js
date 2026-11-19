// Bar hover hint (TRU-192): the app's bottom bar sits below this page, so the
// native side asks the page to draw the hint at its bottom edge.
// `__BG__` / `__FG__` are the page's own foreground and background colours,
// swapped so the bubble reads as inverted and can't be mistaken for a chip.
window.__gittermHint = function (text, x) {
  var el = document.getElementById('__gitterm-hint');
  if (text === null || text === undefined) {
    if (el) el.style.display = 'none';
    return;
  }
  if (!el) {
    el = document.createElement('div');
    el.id = '__gitterm-hint';
    el.style.cssText = 'position:fixed;bottom:4px;z-index:2147483647;pointer-events:none;padding:5px 10px;border-radius:5px;font:15px Menlo,monospace;white-space:nowrap;background:__BG__;color:__FG__;';
    var label = document.createElement('span');
    var arrow = document.createElement('div');
    arrow.style.cssText = 'position:absolute;bottom:-3px;width:6px;height:6px;transform:rotate(45deg);background:__BG__;';
    el.appendChild(label);
    el.appendChild(arrow);
    document.body.appendChild(el);
  }
  el.firstChild.textContent = String(text);
  el.style.display = 'block';
  var w = el.offsetWidth;
  var max = document.documentElement.clientWidth;
  var left = Math.min(Math.max(x - w / 2, 4), Math.max(4, max - w - 4));
  el.style.left = left + 'px';
  el.lastChild.style.left = Math.min(Math.max(x - left - 3, 6), Math.max(6, w - 12)) + 'px';
};
