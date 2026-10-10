/* NOVA sync: one source of truth for the app AND the website.
   - version  ← package.json
   - "what's new" lines ← the <!-- summary --> block of RELEASE_NOTES_v<version>.txt
   Stamps: tauri.conf.json, Cargo.toml, the in-app Help (version chip + "new in" section),
   the What's-new popup VERSION, and docs/index.html (hero badge, download chip/file, offline What's-new card).
   Runs automatically before every `tauri dev` / `tauri build` (scripts/build.mjs). Manual: `npm run sync`. */
import { readFile, writeFile } from 'node:fs/promises';
import { existsSync } from 'node:fs';

const pkg = JSON.parse(await readFile('package.json', 'utf8'));
const V = pkg.version;
const esc = s => String(s).replace(/&/g, '&amp;').replace(/</g, '&lt;').replace(/>/g, '&gt;').replace(/"/g, '&quot;');
const changed = [];
const edit = async (file, fn) => {
  if (!existsSync(file)) return;
  const before = await readFile(file, 'utf8');
  const after = fn(before);
  if (after !== before) { await writeFile(file, after, 'utf8'); changed.push(file); }
};

/* summary lines: "title: fa || en" and "- fa || en" */
let title = null, points = [];
const notesFile = `RELEASE_NOTES_v${V}.txt`;
if (existsSync(notesFile)) {
  const m = /<!--\s*(?:nova-)?summary\b([\s\S]*?)-->/i.exec(await readFile(notesFile, 'utf8'));
  if (m) for (const raw of m[1].split(/\r?\n/)) {
    const line = raw.trim(); if (!line) continue;
    const split = s => { const [fa, en] = s.split('||').map(x => x.trim()); return { fa, en: en || fa }; };
    if (/^title\s*:/i.test(line)) title = split(line.replace(/^title\s*:/i, ''));
    else if (line.startsWith('-')) points.push(split(line.slice(1)));
  }
}

await edit('src-tauri/tauri.conf.json', s => s.replace(/("version"\s*:\s*")[^"]+(")/, `$1${V}$2`));
await edit('src-tauri/Cargo.toml', s => s.replace(/^(version\s*=\s*")[^"]+(")/m, `$1${V}$2`));

await edit('index.html', s => {
  s = s.replace(/(<span class="help-ver-chip">)v[^<]*(<\/span>)/, `$1v${V}$2`);
  s = s.replace(/(const VERSION=')[^']+(')/, `$1${V}$2`);
  s = s.replace(/(<div class="about-ver">VERSION )[0-9.]+/, `$1${V}`);
  s = s.replace(/(appVersion:')[^']+(')/, `$1${V}$2`);
  s = s.replace(/(const FALLBACK_VER=')[^']+(')/, `$1${V}$2`);
  s = s.replace(/(id="nsetVer">)v[^<]+/, `$1v${V}`);
  s = s.replace(/(BUILD=')[^']+(')/, `$1${V}+support-fix$2`);
  if (points.length) s = s.replace(/(<!-- nova-sync:help -->)[\s\S]*?(<!-- \/nova-sync:help -->)/, (_, a, b) =>
    `${a}\n      <div class="help-section">\n        <div class="help-sec-title">✨ تازه‌های ${V}</div>\n` +
    points.map(p => `        <div class="help-item"><span class="help-key">تازه</span><span class="help-desc">${esc(p.fa)}</span></div>`).join('\n') +
    `\n      </div>\n      ${b}`);
  if (points.length) {
    const dict = { 'تازه': 'New', [`✨ تازه‌های ${V}`]: `✨ New in ${V}` };
    points.forEach(p => { dict[p.fa] = p.en; });
    const js = `<script id="nova-sync-i18n">(()=>{const d=${JSON.stringify(dict).replace(/</g, '\\u003c')};const go=()=>window.NovaI18nAdd?.(d);window.NovaI18nAdd?go():document.addEventListener('DOMContentLoaded',go);})();</script>`;
    s = s.replace(/(<!-- nova-sync:i18n -->)[\s\S]*?(<!-- \/nova-sync:i18n -->)/, (_, a, b) => `${a}${js}${b}`);
  }
  return s;
});

await edit('docs/index.html', s => {
  s = s.replace(/(<span id="nvHeroBadge" data-en="Version )[^ "]+( is live">نسخه )[^ <]+( منتشر شد<\/span>)/, `$1${V}$2${V}$3`);
  s = s.replace(/(aria-label="دانلود Nova Player نسخه )[^ "]+( برای ویندوز" data-en-aria="Download Nova Player )[^ "]+( for Windows")/, `$1${V}$2${V}$3`);
  s = s.replace(/(id="nvHeroVer">)v[^<]+/, `$1v${V}`).replace(/(id="nvChipVer">)v[^<]+/, `$1v${V}`);
  s = s.replace(/(id="nvFile">NOVA\.Player_)[^_]+(_x64-setup\.exe)/, `$1${V}$2`);
  if (points.length) s = s.replace(/(<!-- nova-sync:site -->)[\s\S]*?(<!-- \/nova-sync:site -->)/, (_, a, b) =>
    `${a}\n    <li class="wn-item reveal d1">\n      <div class="wn-head"><span class="wn-ver">v${V}</span><span class="wn-tag" data-en="New">جدید</span></div>\n      <ul class="wn-points">\n` +
    points.map(p => `        <li data-en="${esc(p.en)}">${esc(p.fa)}</li>`).join('\n') +
    `\n      </ul>\n    </li>\n    ${b}`);
  return s;
});

console.log(`NOVA sync v${V}: ${changed.length ? changed.join(', ') : 'everything already in sync'}`);
