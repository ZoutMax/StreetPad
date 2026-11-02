// Live self-test, run inside the Google Maps page by `StreetPad.exe --debug-js tools/selftest.js`.
// Checks StreetPad's page script and the parts of Google's page StreetPad depends on.
new Promise((done) => {
  const checks = {};
  const fn = (name) => typeof window[name] === 'function';
  checks.pageScriptLoaded = window.__streetpad === true;
  for (const f of ['__spCursor', '__spFollow', '__spImmersive', '__spGuide', '__spToast']) checks[f] = fn(f);
  checks.keyboardApi = !!(window.__spKb && typeof __spKb.open === 'function');
  if (checks.keyboardApi) {
    checks.keyboardOpens = __spKb.open(['test']) === true && __spKb.isOpen();
    __spKb.close();
    checks.keyboardCloses = !__spKb.isOpen();
  }
  // Google page hooks (break if Google changes its page).
  checks.googleLayersStreetViewSwitch = !!document.querySelector('button[jsaction*="layerswitcher.intent.streetview"]');
  checks.googlePegmanButton = !!document.querySelector('button[jsaction*="runway.pegman"]');
  checks.googleSearchBox = !!document.querySelector('input[name="q"], #searchboxinput');
  checks.mapUrlHasCoordinates = /\/@-?\d+\.\d+,-?\d+\.\d+,/.test(location.href);
  const failed = Object.entries(checks).filter(([, v]) => !v).map(([k]) => k);
  done({ pass: failed.length === 0, failed, checks });
})
