// UI smoke: the gateway's real binary serving the real UI, driven in a real
// browser. Any uncaught JS exception on any tab — like a payload field the
// page iterates but the backend doesn't send — fails the run. Leaves
// ui-smoke-report.json behind as the artifact.
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

const port = await freePort();
const config = path.join(os.tmpdir(), `magpie-ui-smoke-${process.pid}.json`);
const gateway = spawn(bin, ['--listen', `127.0.0.1:${port}`, '--config', config, '--no-window']);
let stdout = '';
let stderr = '';
let exit = '';
gateway.stdout.on('data', (d) => (stdout += d));
gateway.stderr.on('data', (d) => (stderr += d));
gateway.on('error', (e) => (exit += `spawn error: ${e.message}`));
gateway.on('exit', (code, signal) => (exit += `exited code=${code} signal=${signal}`));
try {
  let up = false;
  for (let i = 0; i < 150 && !up; i++) {
    if (exit) break; // died on its feet — report why instead of timing out
    up = await connected(port);
    if (!up) await new Promise((r) => setTimeout(r, 100)); // a 15s window, like the rust e2e's
  }
  if (!up) {
    // everything the runner knows about the process, in one glance
    let probed = '';
    try {
      const { execSync } = await import('node:child_process');
      const alive = process.kill(gateway.pid, 0) ? 'alive' : 'dead';
      const listening = execSync(`ss -tln 2>/dev/null | grep ${port} || true`).toString().trim();
      probed = `process ${alive}; ss: ${listening || 'port not listening'}`;
    } catch (e) {
      probed = `probe failed: ${e.message}`;
    }
    throw new Error(`gateway never became ready\n${probed}\n${exit}\nstdout: ${stdout.slice(-2000)}\nstderr: ${stderr.slice(-2000)}`);
  }

  const errors = [];
  const browser = await chromium.launch();
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
  for (const tab of ['Providers', 'Gateway', 'Routing', 'Usage', 'Library', 'Agents']) {
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
  await page.keyboard.press('Escape');
  await browser.close();

  const report = { tabs: 6, customTileFirst: first === 'Custom', errors, passed: errors.length === 0 && brand === 'magpie' };
  fs.writeFileSync('ui-smoke-report.json', JSON.stringify(report, null, 2));
  if (!report.passed) {
    console.error(`UI smoke failed:\n${errors.join('\n')}`);
    process.exit(1);
  }
  console.log('UI smoke passed: six tabs, no uncaught errors');
} finally {
  gateway.kill();
}
