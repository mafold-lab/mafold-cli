// render.mjs — turn a blocking file into a grey-box animatic for a video model's reference_video.
//
//   node render.mjs <scene.json> [--out <dir>] [--frames] [--embed-only]
//
// Writes next to the scene (or into --out), named after it — for scene.json:
//   scene.animatic.mp4   H.264, yuv420p, exactly duration × fps frames, no audio
//   scene.contact.png    six frames in a row, timestamped — look at this before anything else
//   scene.plan.png       the floor plan: set, paths, camera route, notes
//   scene.first.png / scene.last.png
//   scene.framing.json   per frame, each actor's posed bounding box in the shot camera (NDC, edge = ±1)
//   scene.stage.html     the interactive stage with this scene inside (director / free / plan views)
// and prints one framing line per actor (in frame, or the frames where it leaves). --frames keeps the
// PNG frames in <name>.frames/; --embed-only writes only scene.stage.html.
//
// Time is stepped per frame (t = i / fps) in headless Chrome with software GL, so the same file renders
// the same pixels; nothing is screen-recorded. Needs node 18+, ffmpeg + ffprobe on PATH and a Chrome, Edge
// or Chromium (or CHROME=<path>). playwright-core and three.js are fetched once into a cache
// (MAFOLD_DIRECTOR_CACHE, default ~/.mafold/cache/director) and used offline after that.
import fs from 'node:fs';
import os from 'node:os';
import path from 'node:path';
import { createRequire } from 'node:module';
import { spawnSync } from 'node:child_process';
import { fileURLToPath, pathToFileURL } from 'node:url';

const HERE = path.dirname(fileURLToPath(import.meta.url));
const PW_VERSION = '1.63.0';
const THREE = 'three@0.160.0';

const argv = process.argv.slice(2);
const flag = (f) => argv.includes(f);
const opt = (f) => { const i = argv.indexOf(f); return i >= 0 ? argv[i + 1] : undefined; };
const positional = argv.filter((a, i) => !a.startsWith('--') && argv[i - 1] !== '--out');
if (positional.length !== 1) {
  console.error('usage: node render.mjs <scene.json> [--out <dir>] [--frames] [--embed-only]');
  process.exit(2);
}
const SRC = path.resolve(positional[0]);
const OUT = path.resolve(opt('--out') || path.dirname(SRC));
const NAME = path.basename(SRC).replace(/\.json$/i, '');
const O = (suffix) => path.join(OUT, `${NAME}.${suffix}`);
const CACHE = path.resolve(process.env.MAFOLD_DIRECTOR_CACHE || path.join(os.homedir(), '.mafold', 'cache', 'director'));

function writeChecked(file, buf) {   // read back what was written: a disk that flips bits must not hand over a bad file
  for (let i = 0; i < 3; i++) {
    fs.writeFileSync(file, buf);
    if (fs.readFileSync(file).equals(buf)) return;
    console.warn('read-back mismatch, rewriting', file);
  }
  throw new Error('could not write ' + file);
}
function run(cmd, args) {
  const r = spawnSync(cmd, args, { encoding: 'utf8', maxBuffer: 64 << 20 });
  if (r.error && r.error.code === 'ENOENT') throw new Error(`${cmd} not found — install it or put it on PATH`);
  if (r.status !== 0) throw new Error(`${cmd} failed (${r.status}): ${r.stderr || r.stdout}`);
  return (r.stdout || '').trim();
}
const fromDataUrl = (u) => Buffer.from(u.slice(u.indexOf(',') + 1), 'base64');

// 1) the scene goes into a copy of the stage, so the page and the render can never disagree
const raw = fs.readFileSync(SRC, 'utf8');
const data = JSON.parse(raw);
const re = /(<script type="application\/json" id="blocking">)([\s\S]*?)(<\/script>)/;
const tpl = fs.readFileSync(path.join(HERE, 'stage.html'), 'utf8');
if (!re.test(tpl)) throw new Error('stage.html: no <script id="blocking"> block');
fs.mkdirSync(OUT, { recursive: true });
const STAGE = O('stage.html');
writeChecked(STAGE, Buffer.from(tpl.replace(re, (_, a, _old, c) => `${a}\n${raw.trim().replace(/</g, '\\u003c')}\n${c}`), 'utf8'));
console.log('stage: ' + STAGE);
if (flag('--embed-only')) process.exit(0);

// 2) tools
function findChrome() {
  if (process.env.CHROME) return process.env.CHROME;
  const env = (k) => process.env[k] || '';
  const list = process.platform === 'win32'
    ? [env('PROGRAMFILES'), env('PROGRAMFILES(X86)'), env('LOCALAPPDATA')].filter(Boolean).flatMap((b) => [
      path.join(b, 'Google', 'Chrome', 'Application', 'chrome.exe'), path.join(b, 'Microsoft', 'Edge', 'Application', 'msedge.exe'),
      path.join(b, 'Chromium', 'Application', 'chrome.exe')])
    : process.platform === 'darwin'
      ? ['/Applications/Google Chrome.app/Contents/MacOS/Google Chrome', '/Applications/Chromium.app/Contents/MacOS/Chromium',
        '/Applications/Microsoft Edge.app/Contents/MacOS/Microsoft Edge']
      : ['/usr/bin/google-chrome', '/usr/bin/google-chrome-stable', '/usr/bin/chromium', '/usr/bin/chromium-browser',
        '/snap/bin/chromium', '/usr/bin/microsoft-edge'];
  const hit = list.find((f) => fs.existsSync(f));
  if (!hit) throw new Error('no Chrome, Edge or Chromium found — set CHROME=<path to the browser>');
  return hit;
}
function loadPlaywright() {
  const require = createRequire(import.meta.url);
  const pkg = path.join(CACHE, 'node_modules', 'playwright-core');
  for (const p of [process.env.PLAYWRIGHT_CORE, pkg, 'playwright-core'].filter(Boolean)) {
    try { return require(p); } catch { /* next */ }
  }
  console.log(`installing playwright-core@${PW_VERSION} into ${CACHE} (once) …`);
  fs.mkdirSync(CACHE, { recursive: true });
  const r = spawnSync(`npm install --no-save --no-audit --no-fund --prefix ${JSON.stringify(CACHE)} playwright-core@${PW_VERSION}`,
    { shell: true, encoding: 'utf8', env: { ...process.env, PLAYWRIGHT_SKIP_BROWSER_DOWNLOAD: '1' } });
  if (r.status !== 0) throw new Error('npm install playwright-core failed: ' + (r.stderr || r.stdout || r.error));
  return require(pkg);
}
// three.js: a vendored copy next to this script (development), else the cache, else fetched once and cached
const threeDirs = [path.join(HERE, 'vendor', THREE), path.join(CACHE, THREE)];
const threeFile = (rel) => threeDirs.map((d) => path.join(d, ...rel.split('/'))).find((f) => fs.existsSync(f));

const { chromium } = loadPlaywright();
const TMP = fs.mkdtempSync(path.join(os.tmpdir(), 'mafold-director-'));   // Chrome's profile, temp and crash files
const FRAMES = flag('--frames') ? O('frames') : path.join(TMP, 'frames');
fs.mkdirSync(FRAMES, { recursive: true });
const ctx = await chromium.launchPersistentContext(path.join(TMP, 'profile'), {
  executablePath: findChrome(), headless: true,
  viewport: { width: 854, height: 480 }, deviceScaleFactor: 1,
  args: ['--use-angle=swiftshader', '--enable-unsafe-swiftshader', '--no-first-run', '--no-default-browser-check',
    '--disable-extensions', '--disable-component-update', '--disable-background-networking', '--disable-sync', '--disk-cache-size=1', '--mute-audio'],
  env: { ...process.env, TEMP: TMP, TMP: TMP, TMPDIR: TMP },
});
const t0 = Date.now();
try {
  await ctx.route(/^https?:\/\//, (r) => r.abort());                       // offline …
  await ctx.route(new RegExp('/' + THREE.replace(/\./g, '\\.') + '/'), (r) => {   // … except three.js
    const rel = new URL(r.request().url()).pathname.split(`/${THREE}/`)[1] || '';
    const f = rel && threeFile(rel);
    if (!f) return r.continue();                                             // first run: from the CDN, cached below
    return r.fulfill({ status: 200, body: fs.readFileSync(f),
      headers: { 'content-type': 'text/javascript; charset=utf-8', 'access-control-allow-origin': '*', 'cache-control': 'no-store' } });
  });
  const page = ctx.pages()[0] || (await ctx.newPage());
  const logs = [];
  page.on('console', (m) => logs.push(`[${m.type()}] ${m.text()}`));
  page.on('pageerror', (e) => logs.push('[pageerror] ' + e.message));
  page.on('response', async (res) => {
    const rel = new URL(res.url()).pathname.split(`/${THREE}/`)[1];
    if (!rel || res.request().method() !== 'GET' || !res.ok() || threeFile(rel)) return;
    try { const f = path.join(CACHE, THREE, ...rel.split('/')); fs.mkdirSync(path.dirname(f), { recursive: true }); writeChecked(f, await res.body()); } catch { /* next run fetches again */ }
  });
  await page.addInitScript((d) => { window.__RENDER__ = true; window.__BLOCKING__ = d; }, data);
  await page.goto(pathToFileURL(STAGE).href + '?render=1');
  try { await page.waitForFunction(() => window.stage && window.stage.ready, null, { timeout: 90000 }); }
  catch (e) { console.error(logs.join('\n')); throw e; }

  const info = await page.evaluate(() => window.stage.info());
  const N = Math.round(info.duration * info.fps);
  console.log(`rendering ${N} frames (${info.duration}s @ ${info.fps}fps) at 854x480 …`);
  const bufs = [], framing = [];
  for (let i = 0; i < N; i++) {
    const r = await page.evaluate((t) => window.stage.frame(t), i / info.fps);
    const buf = fromDataUrl(r.png);
    writeChecked(path.join(FRAMES, `f${String(i).padStart(4, '0')}.png`), buf);
    bufs.push(buf); framing.push({ i, t: i / info.fps, ...r.framing });
  }
  if (logs.some((l) => l.startsWith('[pageerror]') || l.startsWith('[error]'))) console.warn(logs.join('\n'));

  // 3) stills, contact strip, floor plan
  writeChecked(O('first.png'), bufs[0]);
  writeChecked(O('last.png'), bufs[N - 1]);
  const pick = [0, 1, 2, 3, 4, 5].map((j) => Math.round((j * (N - 1)) / 5));
  const contact = await page.evaluate(([u, l]) => window.stage.contact(u, l, 427, 240),
    [pick.map((i) => 'data:image/png;base64,' + bufs[i].toString('base64')), pick.map((i) => `${(i / info.fps).toFixed(2)} s · #${i}`)]);
  writeChecked(O('contact.png'), fromDataUrl(contact));
  writeChecked(O('plan.png'), fromDataUrl(await page.evaluate(() => window.stage.plan(1800, 1040))));
  writeChecked(O('framing.json'), Buffer.from(JSON.stringify(framing) + '\n'));

  // 4) framing check: every actor's posed bounding box, projected into the shot camera
  const M = 0.97;
  console.log('framing (NDC, bounding box; frame edge = ±1):');
  for (const id of Object.keys(framing[0]).filter((k) => !['i', 't'].includes(k))) {
    const bx = framing.map((f) => f[id]);
    const bad = framing.filter((f) => { const b = f[id]; return b.behind || b.x0 < -M || b.x1 > M || b.y0 < -M || b.y1 > M; });
    const ext = (k, fn) => fn(...bx.map((b) => b[k]));
    console.log(`  ${id}: x ${ext('x0', Math.min).toFixed(2)}…${ext('x1', Math.max).toFixed(2)}  y ${ext('y0', Math.min).toFixed(2)}…${ext('y1', Math.max).toFixed(2)}`
      + `  height ${(ext('h', Math.min) * 100).toFixed(0)}–${(ext('h', Math.max) * 100).toFixed(0)}% of frame  ` + (bad.length ? `OUT/EDGE in ${bad.length} frames: ${bad.map((f) => f.i).join(',')}` : 'in frame all frames'));
  }

  // 5) encode: exactly N frames at fps -> N/fps seconds, H.264 yuv420p, no audio
  const mp4 = O('animatic.mp4');
  run('ffmpeg', ['-y', '-hide_banner', '-loglevel', 'error', '-framerate', String(info.fps), '-start_number', '0', '-i', path.join(FRAMES, 'f%04d.png'),
    '-frames:v', String(N), '-c:v', 'libx264', '-preset', 'slow', '-crf', '16', '-pix_fmt', 'yuv420p', '-r', String(info.fps), '-an', '-movflags', '+faststart', mp4]);
  run('ffmpeg', ['-v', 'error', '-i', mp4, '-f', 'null', '-']);   // full decode = integrity check
  console.log('animatic: ' + mp4 + '\nffprobe: ' + run('ffprobe', ['-v', 'error', '-count_frames', '-select_streams', 'v:0', '-show_entries',
    'stream=codec_name,pix_fmt,width,height,r_frame_rate,nb_read_frames:format=duration,size', '-of', 'compact=p=0', mp4]).replace(/\r?\n/g, ' | '));
  console.log(`done in ${((Date.now() - t0) / 1000).toFixed(1)} s`);
} finally {
  await ctx.close();
  try { fs.rmSync(TMP, { recursive: true, force: true }); } catch { /* Windows may still hold a file for a moment */ }
}
