// Injected into every page of the main (Google Maps) view. Only lets the page ask the app to
// toggle the settings panel or fullscreen; the page can't change any settings.
(() => {
  if (window.top !== window || window.__streetpad) return;
  window.__streetpad = true;

  // Street View's floor marker (gray disc, arrow, cross) is drawn by the 3D engine with
  // flat-colour shader programs: the fragment shader outputs a colour and samples no texture.
  // While immersive mode is on (window.__spNoText), draw calls of such programs are skipped;
  // the photo itself uses textured programs and is untouched.
  for (const C of [window.WebGLRenderingContext, window.WebGL2RenderingContext]) {
    if (!C) continue;
    const P = C.prototype;
    const oCreate = P.createShader;
    P.createShader = function (type) { const sh = oCreate.call(this, type); try { if (sh) sh.__spType = type; } catch (_) {} return sh; };
    const oShader = P.shaderSource;
    P.shaderSource = function (sh, src) { try { sh.__spSrc = src; } catch (_) {} return oShader.call(this, sh, src); };
    const oAttach = P.attachShader;
    P.attachShader = function (pr, sh) {
      try {
        // Only fragment shaders decide: textured (any sampler) = photo, untextured = overlay.
        if (sh.__spType === 0x8B30) {
          const src = sh.__spSrc || '';
          if (/\bsampler/.test(src) || /texture/.test(src)) pr.__spTextured = true;
          else pr.__spFlat = true;
        }
      } catch (_) {}
      return oAttach.call(this, pr, sh);
    };
    const oUse = P.useProgram;
    P.useProgram = function (pr) { this.__spOverlay = !!(pr && pr.__spFlat && !pr.__spTextured); return oUse.call(this, pr); };
    for (const fn of ['drawArrays', 'drawElements']) {
      const o = P[fn];
      P[fn] = function (...a) {
        if (window.__spFreeze) return; // picture frozen (keeps the last image)
        if (window.__spNoText && this.__spOverlay) return;
        return o.apply(this, a);
      };
    }
    // Freeze: no clearing either, so the canvas keeps showing the last image.
    const oClear = P.clear;
    P.clear = function (...a) { if (window.__spFreeze) return; return oClear.apply(this, a); };
  }

  // Text painted into Street View's 3D picture (street names on the road) is first written to
  // a hidden 2D canvas. While window.__spNoText is set (immersive mode), that text is skipped,
  // so the names never appear in the picture.
  for (const proto of [window.CanvasRenderingContext2D && CanvasRenderingContext2D.prototype,
                       window.OffscreenCanvasRenderingContext2D && OffscreenCanvasRenderingContext2D.prototype]) {
    if (!proto) continue;
    for (const fn of ['fillText', 'strokeText']) {
      const orig = proto[fn];
      proto[fn] = function (...args) { if (window.__spNoText) return; return orig.apply(this, args); };
    }
  }
  // Immersive mode survives the reload that applies it (same tab session only).
  let startImmersive = false;
  try { startImmersive = sessionStorage.getItem('sp-immersive') === '1'; } catch (_) {}
  window.__spNoText = startImmersive;
  const send = (cmd) => { try { window.ipc.postMessage(JSON.stringify({ cmd })); } catch (_) {} };
  addEventListener('keydown', (e) => {
    if (e.key === 'F1') { e.preventDefault(); e.stopPropagation(); send('toggleSettings'); }
    else if (e.key === 'F11') { e.preventDefault(); e.stopPropagation(); send('fullscreen'); }
  }, true);

  // Pointer hiding + centre aim crosshair while the controller is in use (toggled by the app).
  const CSS = `
    /* No scrollbars on Google's page (e.g. the side panel's gray bar); wheel scrolling still works. */
    * { scrollbar-width: none !important; }
    ::-webkit-scrollbar { width: 0 !important; height: 0 !important; background: transparent !important; }
    html.sp-nocursor, html.sp-nocursor * { cursor: none !important; }
    #sp-crosshair { position: fixed; left: 50%; top: 50%; width: 26px; height: 26px;
      margin: -13px 0 0 -13px; pointer-events: none; z-index: 2147483647; display: none;
      opacity: .78; filter: drop-shadow(0 0 1.5px rgba(0,0,0,.85)); }
    html.sp-crosshair #sp-crosshair { display: block; }
    #sp-crosshair::before { content: ""; position: absolute; inset: 3px; border-radius: 50%;
      border: 1.5px solid rgba(255,255,255,.9); }
    #sp-crosshair::after { content: ""; position: absolute; left: 50%; top: 50%; width: 4px; height: 4px;
      margin: -2px 0 0 -2px; border-radius: 50%; background: #fff; }
    #sp-guide { position: fixed; left: 50%; top: 50%; transform: translate(-50%, -50%); z-index: 2147483647;
      pointer-events: none; min-width: 460px; max-width: 80vw; padding: 22px 28px 18px; border-radius: 16px;
      background: rgba(18, 20, 25, .9); color: #e8ebf0; box-shadow: 0 10px 40px rgba(0,0,0,.5);
      font: 15px/1.5 "Segoe UI", system-ui, sans-serif; }
    #sp-guide h3 { margin: 0 0 12px; font-size: 18px; font-weight: 600; }
    #sp-guide table { border-collapse: collapse; width: 100%; }
    #sp-guide td { padding: 3px 0; vertical-align: top; }
    #sp-guide td:first-child { color: #8fb8ff; font-weight: 600; padding-right: 22px; white-space: nowrap; }
    #sp-guide .foot { margin-top: 12px; color: #9aa3b2; font-size: 12px; }`;
  const ensure = () => {
    if (!document.documentElement) return;
    if (!document.getElementById('sp-cursor-style')) {
      const s = document.createElement('style');
      s.id = 'sp-cursor-style';
      s.textContent = CSS;
      (document.head || document.documentElement).appendChild(s);
    }
    if (document.body && !document.getElementById('sp-crosshair')) {
      const c = document.createElement('div');
      c.id = 'sp-crosshair';
      document.body.appendChild(c);
    }
  };
  // Map mode: the crosshair follows the app's (hidden) pointer, which only moves when the app
  // moves the crosshair. Moves with a button down are map panning and are ignored.
  let follow = false;
  window.__spFollow = (on) => {
    if (follow === !!on) return;
    follow = !!on;
    const c = document.getElementById('sp-crosshair');
    if (c && !follow) { c.style.left = ''; c.style.top = ''; }
  };
  addEventListener('mousemove', (e) => {
    // Only while the crosshair is showing (controller in use). Real mouse use hides it first,
    // so the crosshair never follows the physical mouse.
    if (!follow || e.buttons !== 0 || !document.documentElement.classList.contains('sp-crosshair')) return;
    const c = document.getElementById('sp-crosshair');
    if (c) { c.style.left = e.clientX + 'px'; c.style.top = e.clientY + 'px'; }
  }, true);

  // Dark crosshair on the (light) map; white in Street View. Own <style> element, because
  // Google rewrites the <html> class list.
  window.__spDarkCrosshair = (on) => {
    let st = document.getElementById('sp-dark-style');
    if (on && !st && document.documentElement) {
      st = document.createElement('style'); st.id = 'sp-dark-style';
      st.textContent = '#sp-crosshair { filter: drop-shadow(0 0 1.5px rgba(255,255,255,.95)) !important; opacity: .85 !important; }' +
        '#sp-crosshair::before { border-color: rgba(20,22,27,.95) !important; }' +
        '#sp-crosshair::after { background: rgb(20,22,27) !important; }';
      (document.head || document.documentElement).appendChild(st);
    } else if (!on && st) st.remove();
  };

  window.__spCursor = (hidden, crosshair, followMode, x, y) => {
    ensure();
    const h = document.documentElement;
    if (followMode && typeof x === 'number') {
      const c = document.getElementById('sp-crosshair');
      if (c) { c.style.left = x + 'px'; c.style.top = y + 'px'; }
    }
    h.classList.toggle('sp-nocursor', !!hidden);
    h.classList.toggle('sp-crosshair', !!hidden && !!crosshair);
  };
  document.addEventListener('DOMContentLoaded', ensure);

  // Immersive mode: only the main picture (largest canvas) stays visible.
  let immersive = false, immTimer = 0;
  const markMain = () => {
    let best = null, area = 0;
    for (const c of document.querySelectorAll('canvas')) {
      const r = c.getBoundingClientRect(), a = r.width * r.height;
      if (a > area) { area = a; best = c; }
    }
    document.querySelectorAll('canvas.sp-main').forEach((c) => { if (c !== best) c.classList.remove('sp-main'); });
    if (best) best.classList.add('sp-main');
    // Google's place panel (opened by clicking a shop): find its content (role="main") and
    // mark the left-edge panel around it so it stays visible in immersive mode.
    for (const m of document.querySelectorAll('[role="main"]')) {
      for (let e = m; e && e !== document.body; e = e.parentElement) {
        const r = e.getBoundingClientRect();
        if (r.x <= 5 && r.width >= 300 && r.width <= 700 && r.height >= innerHeight * 0.6) {
          if (!e.hasAttribute('data-sp-panel')) e.setAttribute('data-sp-panel', '1');
          break;
        }
      }
    }
  };
  // Immersive rules live in their own <style> element that only StreetPad adds/removes
  // (Google rewrites the <html> class list on load, so a class would be lost).
  const IMM_CSS = `
    body * { visibility: hidden !important; }
    canvas.sp-main, #sp-crosshair, #sp-toast, #sp-kb, #sp-kb *, #sp-guide, #sp-guide *,
    div:has(> [jsaction*="minimap.main"]), div:has(> [jsaction*="minimap.main"]) *,
    [data-sp-panel], [data-sp-panel] * { visibility: visible !important; }`;
  const immStyle = (on) => {
    let st = document.getElementById('sp-immersive-style');
    if (on && !st && document.documentElement) {
      st = document.createElement('style'); st.id = 'sp-immersive-style'; st.textContent = IMM_CSS;
      (document.head || document.documentElement).appendChild(st);
    } else if (!on && st) st.remove();
  };
  window.__spImmersive = (on) => {
    ensure();
    on = !!on;
    window.__spNoText = on;
    try { sessionStorage.setItem('sp-immersive', on ? '1' : '0'); } catch (_) {}
    immStyle(on);
    if (on && immersive) { markMain(); return; }
    immersive = on;
    clearInterval(immTimer);
    if (on) { markMain(); immTimer = setInterval(markMain, 1000); }
  };

  // On-screen button guide: rows = [[control, action], ...] or null to hide.
  window.__spGuide = (rows) => {
    let g = document.getElementById('sp-guide');
    if (!rows) { if (g) g.remove(); return; }
    if (!document.body) return;
    if (!g) { g = document.createElement('div'); g.id = 'sp-guide'; document.body.appendChild(g); }
    const esc = (t) => String(t).replace(/[&<>]/g, (c) => ({ '&': '&amp;', '<': '&lt;', '>': '&gt;' }[c]));
    g.innerHTML = '<h3>StreetPad controls</h3><table>' +
      rows.map(([k, v]) => `<tr><td>${esc(k)}</td><td>${esc(v)}</td></tr>`).join('') +
      '</table><div class="foot">Use the controller to close · change buttons in Settings</div>';
  };

  // ---- On-screen search keyboard (controller): __spKb.open(recent), .nav(cmd), .isOpen()
  const KB_CSS = `
    #sp-kb { position: fixed; left: 50%; bottom: 3%; transform: translateX(-50%); z-index: 2147483647;
      width: min(980px, 94vw); padding: 18px 20px 16px; border-radius: 18px; background: rgba(18,20,25,.95);
      box-shadow: 0 14px 50px rgba(0,0,0,.55); color: #e8ebf0; font: 16px/1.3 "Segoe UI", system-ui, sans-serif;
      visibility: visible !important; }
    #sp-kb * { visibility: visible !important; box-sizing: border-box; }
    #sp-kb .txt { display: flex; align-items: center; gap: 10px; height: 52px; padding: 0 16px; margin-bottom: 12px;
      border-radius: 12px; background: #262b35; border: 1px solid #3a4250; font-size: 22px; }
    #sp-kb .txt .q { white-space: pre; overflow: hidden; }
    #sp-kb .txt .caret { width: 2px; height: 26px; background: #66a3ff; animation: spblink 1s steps(1) infinite; }
    #sp-kb .txt .ph { color: #7d8696; font-size: 18px; }
    @keyframes spblink { 50% { opacity: 0; } }
    #sp-kb .row { display: flex; gap: 8px; margin-top: 8px; }
    #sp-kb .key { flex: 1 1 0; height: 50px; display: flex; align-items: center; justify-content: center; border-radius: 10px;
      background: #2b313c; border: 1px solid #39404d; font-size: 19px; text-transform: uppercase; }
    #sp-kb .key.wide { font-size: 16px; text-transform: none; }
    #sp-kb .key.recent { flex: 0 1 auto; padding: 0 14px; font-size: 14px; text-transform: none; height: 38px;
      max-width: 260px; overflow: hidden; text-overflow: ellipsis; white-space: nowrap; display: block; line-height: 36px; }
    #sp-kb .key.sel { background: #3d8bfd; border-color: #66a3ff; color: #fff; transform: scale(1.04); }
    #sp-kb .help { margin-top: 12px; color: #9aa3b2; font-size: 13px; text-align: center; }
    #sp-kb .help b { color: #dfe6f1; }`;
  const KB_ROWS = [
    '1234567890-'.split(''),
    "qwertyuiop'".split(''),
    'asdfghjkl,.'.split(''),
    'zxcvbnm&/#@'.split(''),
    [{ k: 'space', w: 4, l: 'Space' }, { k: 'back', w: 2, l: '⌫ Delete' }, { k: 'clear', w: 2, l: 'Clear' }, { k: 'search', w: 3, l: '🔍 Search' }],
  ];
  const kb = { el: null, text: '', rows: [], r: 1, c: 0 };
  const kbKeys = (recent) => {
    const rows = [];
    if (recent && recent.length) rows.push(recent.map((q) => ({ k: 'recent', q, l: q, w: 0 })));
    for (const row of KB_ROWS) rows.push(row.map((x) => (typeof x === 'string' ? { k: 'char', ch: x, l: x, w: 1 } : x)));
    return rows;
  };
  const kbRender = () => {
    if (!kb.el) return;
    const esc = (t) => String(t).replace(/[&<>]/g, (c) => ({ '&': '&amp;', '<': '&lt;', '>': '&gt;' }[c]));
    let h = '<div class="txt">' + (kb.text ? `<span class="q">${esc(kb.text)}</span>` : '') + '<span class="caret"></span>' +
      (kb.text ? '' : '<span class="ph">Search Google Maps</span>') + '</div>';
    kb.rows.forEach((row, ri) => {
      h += '<div class="row">' + row.map((key, ci) => {
        const cls = ['key', key.k === 'recent' ? 'recent' : '', key.w > 1 ? 'wide' : '', ri === kb.r && ci === kb.c ? 'sel' : ''].join(' ');
        const style = key.w > 1 ? ` style="flex-grow:${key.w}"` : '';
        return `<div class="${cls}"${style}>${key.k === 'recent' ? '🕘 ' : ''}${esc(key.l)}</div>`;
      }).join('') + '</div>';
    });
    const L = kb.labels || { start: 'Start', back: 'Back' };
    h += `<div class="help"><b>A</b> type · <b>B</b> delete · <b>X</b> space · <b>${esc(L.start)}</b> search · <b>${esc(L.back)}</b> close</div>`;
    kb.el.innerHTML = h;
  };
  // Horizontal centre of a key in its row, 0..1, for natural up/down movement.
  const kbCenter = (ri, ci) => {
    const row = kb.rows[ri];
    const ws = row.map((k) => Math.max(1, k.w || 1));
    const total = ws.reduce((a, b) => a + b, 0);
    let x = 0;
    for (let i = 0; i < ci; i++) x += ws[i];
    return (x + ws[ci] / 2) / total;
  };
  const kbSend = (cmd, extra) => { try { window.ipc.postMessage(JSON.stringify(Object.assign({ cmd }, extra || {}))); } catch (_) {} };
  const kbPress = (key) => {
    if (key.k === 'char') kb.text += key.ch;
    else if (key.k === 'space') kb.text += ' ';
    else if (key.k === 'back') kb.text = kb.text.slice(0, -1);
    else if (key.k === 'clear') kb.text = '';
    else if (key.k === 'search') { kbSubmit(); return; }
    else if (key.k === 'recent') { kb.text = key.q; kbSubmit(); return; }
    kb.text = kb.text.slice(0, 200);
    kbRender();
  };
  const kbSubmit = () => {
    const q = kb.text.trim();
    if (!q) return;
    kbSend('kbSearch', { q });
    kbClose(false);
  };
  const kbClose = (notify) => {
    if (kb.el) { kb.el.remove(); kb.el = null; }
    removeEventListener('keydown', kbKeydown, true);
    if (notify) kbSend('kbClose');
  };
  const kbKeydown = (e) => {
    if (!kb.el) return;
    if (e.key === 'Escape') kbClose(true);
    else if (e.key === 'Enter') kbSubmit();
    else if (e.key === 'Backspace') { kb.text = kb.text.slice(0, -1); kbRender(); }
    else if (e.key.length === 1 && !e.ctrlKey && !e.altKey) { kb.text = (kb.text + e.key).slice(0, 200); kbRender(); }
    else return;
    e.preventDefault(); e.stopPropagation();
  };
  window.__spKb = {
    isOpen: () => !!kb.el,
    open(recent, labels) {
      kb.labels = labels || null;
      if (!document.body) return false;
      if (!document.getElementById('sp-kb-style')) {
        const st = document.createElement('style'); st.id = 'sp-kb-style'; st.textContent = KB_CSS; document.head.appendChild(st);
      }
      if (!kb.el) {
        kb.el = document.createElement('div'); kb.el.id = 'sp-kb'; document.body.appendChild(kb.el);
        addEventListener('keydown', kbKeydown, true);
        if (document.activeElement && document.activeElement.blur) document.activeElement.blur();
      }
      kb.text = '';
      kb.rows = kbKeys(recent);
      kb.r = recent && recent.length ? 1 : 0; kb.c = 0;
      kbRender();
      return true;
    },
    close() { kbClose(false); },
    nav(cmd) {
      if (!kb.el) return;
      const rows = kb.rows;
      if (cmd === 'left') kb.c = Math.max(0, kb.c - 1);
      else if (cmd === 'right') kb.c = Math.min(rows[kb.r].length - 1, kb.c + 1);
      else if (cmd === 'up' || cmd === 'down') {
        const nr = kb.r + (cmd === 'down' ? 1 : -1);
        if (nr >= 0 && nr < rows.length) {
          const x = kbCenter(kb.r, kb.c);
          let best = 0, bd = 9;
          rows[nr].forEach((_, ci) => { const d = Math.abs(kbCenter(nr, ci) - x); if (d < bd) { bd = d; best = ci; } });
          kb.r = nr; kb.c = best;
        }
      }
      else if (cmd === 'activate') { kbPress(rows[kb.r][kb.c]); return; }
      else if (cmd === 'backspace') kb.text = kb.text.slice(0, -1);
      else if (cmd === 'space') kb.text = (kb.text + ' ').slice(0, 200);
      else if (cmd === 'submit') { kbSubmit(); return; }
      else if (cmd === 'close') { kbClose(true); return; }
      kbRender();
    },
  };

  if (startImmersive) {
    // Hide Google's interface from the first frame after an immersive reload.
    immStyle(true);
    document.addEventListener('DOMContentLoaded', () => window.__spImmersive(true));
  }

  // Close Google's place panel without reloading: switch the page to the same Street View address
  // without the place, the way Back/Forward does (Google rebuilds the screen from the address).
  window.__spClosePlace = (cleanUrl) => {
    history.pushState(null, '', cleanUrl);
    dispatchEvent(new PopStateEvent('popstate', { state: null }));
    return true;
  };

  // Short on-screen messages from the app (bookmark saved, etc.).
  let toastTimer = 0;
  window.__spToast = (text) => {
    if (!document.body) return;
    let t = document.getElementById('sp-toast');
    if (!t) {
      t = document.createElement('div');
      t.id = 'sp-toast';
      t.style.cssText = 'position:fixed;left:50%;bottom:9%;transform:translateX(-50%);z-index:2147483647;' +
        'pointer-events:none;max-width:70vw;padding:10px 18px;border-radius:22px;background:rgba(20,22,27,.86);' +
        'color:#fff;font:500 15px/1.35 "Segoe UI",system-ui,sans-serif;box-shadow:0 4px 18px rgba(0,0,0,.35);' +
        'white-space:nowrap;overflow:hidden;text-overflow:ellipsis;transition:opacity .25s;opacity:0';
      document.body.appendChild(t);
    }
    t.textContent = text;
    requestAnimationFrame(() => { t.style.opacity = '1'; });
    clearTimeout(toastTimer);
    toastTimer = setTimeout(() => { t.style.opacity = '0'; }, 2200);
  };
})();
