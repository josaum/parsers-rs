import { chromium } from 'playwright';
import readline from 'node:readline';

// Line-delimited protocol only. Page console output is never forwarded to stdout.
const input = readline.createInterface({ input: process.stdin, crlfDelay: Infinity });
const pending = new Map();
let browser;
let serial = 0;
let closing = false;
const emit = value => !closing && process.stdout.write(`${JSON.stringify(value)}\n`);
async function shutdown() {
  if (closing) return;
  closing = true;
  for (const resolve of pending.values()) resolve({ error: 'renderer_closed' });
  pending.clear();
  const timer = setTimeout(() => process.exit(1), 3000);
  timer.unref();
  try { await browser?.close(); } catch { /* process already closed */ }
  input.close();
  process.exit(0);
}
process.on('SIGTERM', shutdown);
process.stdout.on('error', shutdown);
process.stdin.on('end', shutdown);
input.on('line', line => {
  let message;
  try { message = JSON.parse(line); } catch { emit({ type: 'error', code: 'invalid_protocol' }); return; }
  if (message.type === 'response') {
    const resolve = pending.get(message.id);
    if (resolve) { pending.delete(message.id); resolve(message); }
  } else if (message.type === 'cancel') {
    void shutdown();
  } else if (message.type === 'init') {
    void render(message).catch(() => emit({ type: 'error', code: 'browser_render_failed' }));
  }
});

async function render(init) {
  const options = init.options;
  const launch = { headless: options.headless !== false, args: ['--disable-background-networking', '--disable-quic', '--force-webrtc-ip-handling-policy=disable_non_proxied_udp'] };
  if (options.executable_path) launch.executablePath = options.executable_path;
  else launch.channel = options.channel;
  browser = await chromium.launch(launch);
  const contextOptions = { serviceWorkers: 'block', acceptDownloads: false };
  if (init.user_agent) contextOptions.userAgent = init.user_agent;
  const context = await browser.newContext(contextOptions);
  if (init.identity_only) {
    const probe = await context.newPage();
    emit({ type: 'identity', user_agent: await probe.evaluate(() => navigator.userAgent) });
    return;
  }
  await context.routeWebSocket('**/*', socket => socket.close());
  await context.addInitScript(() => {
    for (const key of ['RTCPeerConnection', 'webkitRTCPeerConnection', 'WebTransport']) {
      try { Object.defineProperty(globalThis, key, { value: undefined, configurable: false }); } catch {}
    }
  });
  let page;
  context.on('page', other => { if (page && other !== page) void other.close(); });
  let initial = true;
  let requests = 0;
  const diagnostics = [];
  await context.route('**/*', async route => {
    const request = route.request();
    const url = request.url();
    if (!/^https?:/.test(url) || Buffer.byteLength(url) > init.max_url_bytes) return route.abort();
    const navigation = request.isNavigationRequest() && request.frame().parentFrame() === null;
    if (initial && navigation && url === init.url) {
      initial = false;
      return route.fulfill({ status: 200, headers: init.headers, body: Buffer.from(init.body, 'base64') });
    }
    if (options.block_images && ['image', 'media', 'font'].includes(request.resourceType())) return route.abort();
    let redirects = 0;
    for (let prior = request.redirectedFrom(); prior; prior = prior.redirectedFrom()) redirects++;
    if (redirects > init.max_redirects || ++requests > options.max_resources) {
      if (!diagnostics.includes('browser_resource_limit')) diagnostics.push('browser_resource_limit');
      return route.abort();
    }
    const postData = request.postData();
    if (postData && Buffer.byteLength(postData) > 65536) return route.abort();
    const id = ++serial;
    const response = await new Promise(resolve => {
      pending.set(id, resolve);
      emit({ type: 'request', id, url, method: request.method(), post_data: postData, headers: request.headers(), navigation });
    });
    try {
      if (response.error) {
        if (!diagnostics.includes(response.error)) diagnostics.push(response.error);
        await route.abort();
      } else {
        await route.fulfill({ status: response.status, headers: response.headers, body: Buffer.from(response.body, 'base64') });
      }
    } catch { /* navigation can cancel a request before its response arrives */ }
  });
  page = await context.newPage();
  page.on('dialog', dialog => void dialog.dismiss());
  await page.goto(init.url, { waitUntil: 'domcontentloaded', timeout: options.timeout_ms });
  for (let i = 0; i <= options.scroll_steps; i++) {
    try { await page.waitForLoadState('networkidle', { timeout: options.settle_ms }); }
    catch { if (!diagnostics.includes('network_not_idle')) diagnostics.push('network_not_idle'); }
    await page.evaluate(({ quiet, maximum }) => new Promise(resolve => {
      let timer;
      const finish = () => { observer.disconnect(); clearTimeout(timer); clearTimeout(deadline); resolve(); };
      const changed = () => { clearTimeout(timer); timer = setTimeout(finish, quiet); };
      const observer = new MutationObserver(changed);
      observer.observe(document, { subtree: true, childList: true, attributes: true, characterData: true });
      const deadline = setTimeout(finish, maximum);
      changed();
    }), { quiet: options.dom_quiet_ms, maximum: options.settle_ms });
    if (i < options.scroll_steps) await page.evaluate(() => window.scrollBy(0, Math.max(window.innerHeight, 600)));
  }
  const snapshot = await page.evaluate(({ maxHtml, maxState }) => {
    const clone = document.documentElement.cloneNode(true);
    let shadowRoots = 0;
    function shadows(source, target) {
      const originals = [...source.children];
      const copies = [...target.children];
      for (let i = 0; i < originals.length; i++) shadows(originals[i], copies[i]);
      if (source.shadowRoot) {
        shadowRoots++;
        const wrapper = document.createElement('section');
        wrapper.setAttribute('data-web-fast-shadow-root', 'open');
        for (const child of source.shadowRoot.childNodes) wrapper.appendChild(child.cloneNode(true));
        target.appendChild(wrapper);
        const roots = [...source.shadowRoot.children];
        const clones = [...wrapper.children];
        for (let i = 0; i < roots.length; i++) shadows(roots[i], clones[i]);
      }
    }
    shadows(document.documentElement, clone);
    const html = '<!doctype html>' + clone.outerHTML;
    const encoder = new TextEncoder();
    if (encoder.encode(html).length > maxHtml) return { error: 'rendered_html_limit' };
    const states = [];
    let remaining = maxState;
    for (const key of ['__NEXT_DATA__', '__INITIAL_STATE__', '__PRELOADED_STATE__', '__APOLLO_STATE__', '__NUXT__', '__next_f']) {
      try {
        const text = JSON.stringify(globalThis[key]);
        if (!text) continue;
        const bytes = encoder.encode(text).length;
        if (bytes > remaining) continue;
        remaining -= bytes;
        states.push({ key, value: JSON.parse(text) });
      } catch { /* cyclic and executable state is not JSON */ }
    }
    return { html, states, shadowRoots };
  }, { maxHtml: init.max_html_bytes, maxState: init.max_json_bytes });
  if (snapshot.error) { emit({ type: 'error', code: snapshot.error }); return; }
  if (pending.size) diagnostics.push('resource_requests_incomplete');
  emit({ type: 'result', html_base64: Buffer.from(snapshot.html).toString('base64'), final_url: page.url(), states: snapshot.states, shadow_roots: snapshot.shadowRoots, diagnostics });
}
