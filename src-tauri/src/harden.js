// Injected into EVERY frame of the sandboxed browser before any page script runs.
// Defence in depth: the native layer (navigation policy, content rules, download/popup
// denial, App Sandbox) is the real barrier; this removes the APIs that ad/scam scripts abuse.
(() => {
  'use strict';
  const lock = (obj, name, value) => {
    try {
      Object.defineProperty(obj, name, { value, writable: false, configurable: false });
    } catch (_) {}
  };
  const noop = () => undefined;
  const deny = () => Promise.reject(new DOMException('Blocked by Screen Cast sandbox', 'NotAllowedError'));

  // Popups / popunders / tab-unders.
  lock(window, 'open', () => null);

  // Scam dialogs ("Your Mac is infected!") and print/alert loops.
  lock(window, 'alert', noop);
  lock(window, 'confirm', () => false);
  lock(window, 'prompt', () => null);
  lock(window, 'print', noop);

  // Leave-page traps.
  try {
    Object.defineProperty(window, 'onbeforeunload', { get: () => null, set: noop, configurable: false });
  } catch (_) {}
  const realAdd = EventTarget.prototype.addEventListener;
  lock(EventTarget.prototype, 'addEventListener', function (type, ...rest) {
    if (type === 'beforeunload') return;
    return realAdd.call(this, type, ...rest);
  });

  // Hardware / personal data access.
  const nav = window.navigator;
  if (nav.mediaDevices) {
    lock(nav.mediaDevices, 'getUserMedia', deny);
    lock(nav.mediaDevices, 'getDisplayMedia', deny);
    lock(nav.mediaDevices, 'enumerateDevices', () => Promise.resolve([]));
  }
  if (nav.geolocation) {
    lock(nav.geolocation, 'getCurrentPosition', (_ok, err) => err && err({ code: 1, message: 'Blocked' }));
    lock(nav.geolocation, 'watchPosition', (_ok, err) => { err && err({ code: 1, message: 'Blocked' }); return 0; });
  }
  // Clipboard hijacking ("paste this command in Terminal" scams).
  if (nav.clipboard) {
    lock(nav.clipboard, 'writeText', deny);
    lock(nav.clipboard, 'write', deny);
    lock(nav.clipboard, 'readText', deny);
    lock(nav.clipboard, 'read', deny);
  }
  const realExec = Document.prototype.execCommand;
  lock(Document.prototype, 'execCommand', function (cmd, ...rest) {
    if (/^(copy|cut|paste)$/i.test(String(cmd))) return false;
    return realExec.call(this, cmd, ...rest);
  });
  lock(nav, 'registerProtocolHandler', noop);
  lock(nav, 'share', deny);
  lock(nav, 'vibrate', () => false);
  if (nav.serviceWorker) lock(nav.serviceWorker, 'register', deny);
  if (window.Notification) {
    lock(window.Notification, 'requestPermission', () => Promise.resolve('denied'));
  }
  lock(window, 'showOpenFilePicker', deny);
  lock(window, 'showSaveFilePicker', deny);
  lock(window, 'showDirectoryPicker', deny);

  // ---- In-page fullscreen -------------------------------------------------------------
  // Native fullscreen moves the video into a separate system window, which the cast does not
  // capture (viewers would see a blank screen). Instead, "fullscreen" fills the browser area,
  // which is exactly what is being cast. Works across nested iframes via postMessage.
  const FS_PROPS = {
    position: 'fixed', top: '0', left: '0', right: '0', bottom: '0',
    width: '100vw', height: '100vh', 'max-width': 'none', 'max-height': 'none',
    'min-width': '0', 'min-height': '0', margin: '0', padding: '0', border: '0',
    'z-index': '2147483647', background: '#000', transform: 'none', 'object-fit': 'contain',
  };
  let fsEl = null;
  const fire = (el) => {
    for (const t of ['fullscreenchange', 'webkitfullscreenchange']) {
      try { (el.isConnected ? el : document).dispatchEvent(new Event(t, { bubbles: true })); } catch (_) {}
    }
  };
  const tellParent = (on) => {
    if (window !== window.parent) {
      try { window.parent.postMessage({ __scFs: on }, '*'); } catch (_) {}
    }
  };
  // Styles changed while fullscreen: [element, previous style attribute, previous title].
  let fsSaved = [];
  const save = (node) => {
    if (!fsSaved.some(([n]) => n === node)) fsSaved.push([node, node.getAttribute('style'), node.getAttribute('title')]);
  };
  const setStyles = (node, props) => {
    save(node);
    for (const [k, v] of Object.entries(props)) node.style.setProperty(k, v, 'important');
  };
  // Properties on ancestors that would trap a position:fixed child or clip it.
  const UNTRAP = {
    transform: 'none', filter: 'none', perspective: 'none', contain: 'none', 'will-change': 'auto',
    'backdrop-filter': 'none', 'clip-path': 'none', overflow: 'visible', 'z-index': '2147483647',
  };
  const enterFs = (el) => {
    if (!(el instanceof Element)) return Promise.reject(new TypeError('not an element'));
    if (fsEl === el) return Promise.resolve();
    if (fsEl) exitFs(false);
    fsEl = el;
    fsSaved = [];
    setStyles(el, FS_PROPS);
    if (el.hasAttribute('title')) el.removeAttribute('title'); // no hover tooltip over the movie
    // Walk up: free every ancestor and hide everything that is not on the path to the
    // player (site header, menus, sidebars, ads), so only the movie is visible.
    let child = el;
    for (let node = el.parentElement; node; child = node, node = node.parentElement) {
      setStyles(node, node === document.documentElement || node === document.body
        ? { ...UNTRAP, overflow: 'hidden', background: '#000' }
        : UNTRAP);
      for (const sib of node.children) {
        if (sib !== child && !['SCRIPT', 'STYLE', 'LINK', 'META', 'HEAD'].includes(sib.tagName)) {
          setStyles(sib, { visibility: 'hidden' });
        }
      }
    }
    fire(el);
    tellParent(true);
    return Promise.resolve();
  };
  const exitFs = (notifyParent) => {
    const el = fsEl;
    if (!el) return;
    fsEl = null;
    for (const [node, style, title] of fsSaved.reverse()) {
      if (style == null) node.removeAttribute('style');
      else node.setAttribute('style', style);
      if (title != null) node.setAttribute('title', title);
    }
    fsSaved = [];
    // A fullscreen iframe: tell the page inside it to leave fullscreen too.
    if (el.tagName === 'IFRAME') {
      try { el.contentWindow.postMessage({ __scFsExit: true }, '*'); } catch (_) {}
    }
    fire(el);
    if (notifyParent) tellParent(false);
  };
  window.addEventListener('message', (e) => {
    const d = e.data;
    if (!d || typeof d !== 'object') return;
    if (d.__scFsExit === true && e.source === window.parent) {
      exitFs(false);
      return;
    }
    if (typeof d.__scFs !== 'boolean') return;
    const frame = Array.from(document.querySelectorAll('iframe, frame'))
      .find((f) => { try { return f.contentWindow === e.source; } catch (_) { return false; } });
    if (!frame) return;
    // Only frames the page explicitly allowed to go fullscreen (video players do; ads don't).
    const allowed = frame.hasAttribute('allowfullscreen') || frame.hasAttribute('webkitallowfullscreen')
      || /(^|;)\s*fullscreen/.test(frame.getAttribute('allow') || '');
    if (d.__scFs && allowed) enterFs(frame);
    else if (fsEl === frame) exitFs(true);
  });
  document.addEventListener('keydown', (e) => {
    if (e.key === 'Escape' && fsEl) exitFs(true);
  }, true);

  const request = function () { return enterFs(this); };
  for (const n of ['requestFullscreen', 'webkitRequestFullscreen', 'webkitRequestFullScreen']) {
    lock(Element.prototype, n, request);
  }
  if (window.HTMLVideoElement) {
    for (const n of ['webkitEnterFullscreen', 'webkitEnterFullScreen']) lock(HTMLVideoElement.prototype, n, request);
  }
  const exit = () => { exitFs(true); return Promise.resolve(); };
  for (const n of ['exitFullscreen', 'webkitExitFullscreen', 'webkitCancelFullScreen']) {
    lock(Document.prototype, n, exit);
  }
  const getter = (proto, name, get) => {
    try { Object.defineProperty(proto, name, { get, configurable: false }); } catch (_) {}
  };
  for (const n of ['fullscreenElement', 'webkitFullscreenElement', 'webkitCurrentFullScreenElement']) {
    getter(Document.prototype, n, () => fsEl);
  }
  for (const n of ['fullscreenEnabled', 'webkitFullscreenEnabled', 'webkitFullScreenKeyboardInputAllowed']) {
    getter(Document.prototype, n, () => true);
  }
  for (const n of ['fullscreen', 'webkitIsFullScreen']) getter(Document.prototype, n, () => !!fsEl);
  if (window.HTMLVideoElement) {
    getter(HTMLVideoElement.prototype, 'webkitDisplayingFullscreen', function () { return fsEl === this; });
    getter(HTMLVideoElement.prototype, 'webkitSupportsFullscreen', () => true);
  }

  // Forced downloads via <a download> / blob links (native layer cancels them too).
  document.addEventListener('click', (e) => {
    const a = e.target && e.target.closest && e.target.closest('a[download]');
    if (a) { e.preventDefault(); e.stopImmediatePropagation(); }
  }, true);
})();
