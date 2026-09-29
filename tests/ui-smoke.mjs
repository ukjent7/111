// UI smoke: the gateway's real binary serving the real UI, driven in a real
// browser. Any uncaught JS exception on any tab — like a payload field the
// page iterates but the backend doesn't send — fails the run. Three gateways
// on fresh ports: one with the network (the tab walkthrough, plus the catalog
// refresh landing in the disk cache), one booted with a pre-seeded cache and
// no network (a launch is served from the cache, never cold), one with no
// cache and no network (the add sheet shows skeleton tiles while the catalog
// loads). Leaves ui-smoke-report.json behind as the artifact.
import { chromium } from 'playwright';
import { spawn } from 'node:child_process';
import net from 'node:net';
import fs from 'node:fs';
import os from 'node:os';
import path from 'node:path';

const bin = process.platform === 'win32' ? 'target/debug/magpie-gateway.exe' : 'target/debug/magpie-gateway';
if (!fs.existsSync(bin)) {
  console.error(`gateway binary missing at ${bin} — run cargo test first`);
  process.exit(1);
}

const freePort = () =>
  new Promise((resolve) => {
    const s = net.createServer();
    s.listen(0, '127.0.0.1', () => {
      const port = s.address().port;
      s.close(() => resolve(port));
    });
  });
const connected = (port) =>
  new Promise((resolve) => {
    const s = net.connect(port, '127.0.0.1');
    s.on('connect', () => { s.end(); resolve(true); });
    s.on('error', () => resolve(false));
  });

// one gateway on a fresh port, resolved once it answers; a process that dies
// on its feet reports why instead of timing out
const startGateway = async (config, env = {}) => {
  const port = await freePort();
  const proc = spawn(bin, ['--listen', `127.0.0.1:${port}`, '--config', config, '--no-window'], { env: { ...process.env, ...env } });
  let out = '';
  let err = '';
  let exit = '';
  proc.stdout.on('data', (d) => (out += d));
  proc.stderr.on('data', (d) => (err += d));
  proc.on('error', (e) => (exit += `spawn error: ${e.message}`));
  proc.on('exit', (code, signal) => (exit += `exited code=${code} signal=${signal}`));
  let up = false;
  for (let i = 0; i < 150 && !up; i++) {
    if (exit) break;
    up = await connected(port);
    if (!up) await new Promise((r) => setTimeout(r, 100)); // a 15s window, like the rust e2e's
  }
  if (!up) {
    const alive = process.kill(proc.pid, 0) ? 'alive' : 'dead';
    throw new Error(`gateway never became ready (process ${alive})\n${exit}\nstdout: ${out.slice(-2000)}\nstderr: ${err.slice(-2000)}`);
  }
  return { proc, port };
};
const api = async (port, p) => fetch(`http://127.0.0.1:${port}/api/${p}`).then((r) => r.json());

// a closed port stands in for "no internet": models.dev's fetch fails at
// once, so whatever the catalog cache holds is all there is
const offline = { HTTPS_PROXY: 'http://127.0.0.1:9', https_proxy: 'http://127.0.0.1:9' };

const errors = [];
const browser = await chromium.launch({ channel: 'chrome' });
try {
  // ---- the walkthrough: real network, every tab, no uncaught errors ----
  const configA = path.join(os.tmpdir(), `magpie-ui-smoke-${process.pid}.json`);
  const { proc, port } = await startGateway(configA);
  try {
    const page = await browser.newPage();
    page.on('pageerror', (e) => errors.push(`uncaught: ${e.message}`));
    // resource-load noise (a 404 for /api/icons, say) is the page working as
    // designed; only real script errors count
    page.on('console', (m) => {
      if (m.type() === 'error' && !/Failed to load resource/.test(m.text())) errors.push(`console: ${m.text()}`);
    });

    await page.goto(`http://127.0.0.1:${port}/?shell=1`, { waitUntil: 'load' });
    await page.waitForTimeout(1500);
    const brand = await page.textContent('.brand span:last-child');
    for (const tab of ['Providers', 'Gateway', 'Usage']) {
      await page.click(`#nav button:has-text("${tab}")`, { timeout: 5000 }).catch((e) => errors.push(`click ${tab}: ${e.message}`));
      await page.waitForTimeout(800);
    }
    // settings opens from the gear, not from the nav
    await page.click('#prefs', { timeout: 5000 }).catch((e) => errors.push(`click settings: ${e.message}`));
    await page.waitForTimeout(800);
    // the add sheet: the user's own Custom tile first, then the vendors.
    // with no provider configured the sheet is already open (and its button
    // hidden), so only click when the button is there to click
    await page.click('#nav button:has-text("Providers")');
    const addBtn = await page.$('#addProvider');
    if (addBtn && (await addBtn.isVisible())) {
      await addBtn.click();
      await page.waitForTimeout(600);
    }
    const first = await page.textContent('#addSheet .tiles .tile .n').catch(() => null);
    if (first !== 'Custom') errors.push(`first add-sheet tile is "${first}", expected "Custom"`);
    await page.close();

    // the refreshed catalog lands in the disk cache next to the config
    let refreshed = false;
    for (let i = 0; i < 60 && !refreshed; i++) {
      await new Promise((r) => setTimeout(r, 500));
      try {
        refreshed = (await api(port, 'providers')).catalogReady === true;
      } catch {}
    }
    if (!refreshed) errors.push('catalog never became ready within 30s (models.dev unreachable?)');
    else if (!fs.existsSync(path.join(os.tmpdir(), 'catalog.json'))) errors.push('catalog ready but no disk cache written beside the config');
    var customTileFirst = first === 'Custom';
    var brandOk = brand === 'magpie';
  } finally {
    proc.kill();
  }

  // ---- a seeded cache serves a launch with no network at all ----
  {
    const dir = fs.mkdtempSync(path.join(os.tmpdir(), 'magpie-smoke-cache-'));
    fs.writeFileSync(
      path.join(dir, 'catalog.json'),
      JSON.stringify({ acme: { id: 'acme', name: 'Acme', api: 'https://api.acme.test/v1' } }),
    );
    const gw = await startGateway(path.join(dir, 'config.json'), offline);
    try {
      const r = await api(gw.port, 'providers');
      const cached = r.catalogReady === true && r.presets.some((p) => p.id === 'acme');
      if (!cached) errors.push(`seeded cache not served at boot: catalogReady=${r.catalogReady}, presets=${JSON.stringify(r.presets).slice(0, 200)}`);
      const page = await browser.newPage();
      page.on('pageerror', (e) => errors.push(`uncaught (cache): ${e.message}`));
      await page.goto(`http://127.0.0.1:${gw.port}/?shell=1`, { waitUntil: 'load' });
      await page.click('#nav button:has-text("Providers")');
      await page.waitForTimeout(800);
      const tile = await page.textContent('#addSheet .tiles .grid .tile .n').catch(() => null);
      if (tile !== 'Acme') errors.push(`cached vendor tile is "${tile}", expected "Acme"`);
      await page.close();
      var catalogFromCache = cached && tile === 'Acme';
    } finally {
      gw.proc.kill();
    }
  }

  // ---- no cache, no network: the sheet says the vendors are on their way ----
  {
    const dir = fs.mkdtempSync(path.join(os.tmpdir(), 'magpie-smoke-cold-'));
    const gw = await startGateway(path.join(dir, 'config.json'), offline);
    try {
      const r = await api(gw.port, 'providers');
      if (r.catalogReady !== false || r.presets.length) errors.push(`an unfetched catalog must be catalogReady=false with no presets, got ${r.catalogReady}, ${r.presets.length} presets`);
      const page = await browser.newPage();
      page.on('pageerror', (e) => errors.push(`uncaught (cold): ${e.message}`));
      await page.goto(`http://127.0.0.1:${gw.port}/?shell=1`, { waitUntil: 'load' });
      await page.click('#nav button:has-text("Providers")');
      await page.waitForTimeout(800);
      const bits = await page.$$eval('#addSheet .tiles .tile[disabled] .skeleton', (els) => els.length).catch(() => 0);
      if (bits < 8) errors.push(`expected skeleton tiles while the catalog loads, found ${bits} skeleton bits`);
      await page.close();
      var catalogLoadingTiles = bits >= 8;
    } finally {
      gw.proc.kill();
    }
  }
  var passed = errors.length === 0 && brandOk && customTileFirst;
} finally {
  await browser.close();
}

const report = { tabs: 3, customTileFirst, catalogFromCache, catalogLoadingTiles, errors, passed };
fs.writeFileSync('ui-smoke-report.json', JSON.stringify(report, null, 2));
if (!report.passed) {
  console.error(`UI smoke failed:\n${errors.join('\n')}`);
  process.exit(1);
}
console.log('UI smoke passed: three tabs, catalog cached and loading states as designed');
