// Fails the build when a page links to something the site doesn't have: every internal href and src in the built
// HTML, and every https://fleetlayer.ai/... link in the Markdown copies and llms.txt files.
import { readFile, readdir, stat } from "node:fs/promises";
import { join, resolve, dirname } from "node:path";
import { fileURLToPath } from "node:url";
import { site } from "../site.config.mjs";

const dist = resolve(dirname(fileURLToPath(import.meta.url)), "../dist");

async function walk(dir) {
  const out = [];
  for (const e of await readdir(dir, { withFileTypes: true })) {
    const p = join(dir, e.name);
    if (e.isDirectory()) out.push(...(await walk(p)));
    else out.push(p);
  }
  return out;
}

async function exists(urlPath) {
  const clean = decodeURIComponent(urlPath.split(/[?#]/)[0]);
  for (const candidate of [clean, join(clean, "index.html")]) {
    try {
      if ((await stat(join(dist, candidate))).isFile()) return true;
    } catch {}
  }
  return false;
}

const files = await walk(dist);
const broken = [];
for (const file of files) {
  const rel = "/" + file.slice(dist.length + 1);
  if (rel === "/404.html") continue; // its canonical link names /404/, which hosts serve from 404.html
  let links = [];
  if (file.endsWith(".html")) {
    const html = await readFile(file, "utf8");
    links = [...html.matchAll(/\s(?:href|src)="([^"]+)"/g)].map((m) => m[1]);
    links = links
      .filter((l) => !/^([a-z]+:|#|\/\/)/i.test(l) || l.startsWith(site.url))
      .map((l) => (l.startsWith(site.url) ? l.slice(site.url.length) || "/" : l))
      .map((l) => (l.startsWith("/") ? l : new URL(l, "https://x" + rel).pathname));
  } else if (/\.(md|txt)$/.test(file)) {
    const text = await readFile(file, "utf8");
    links = [...text.matchAll(new RegExp(site.url.replace(/\./g, "\\.") + "(/[^\\s)\\]>\"'`]*)", "g"))].map((m) => m[1].replace(/[.,;:]+$/, ""));
  }
  for (const l of new Set(links)) {
    if (l === "" || l.startsWith("/_astro/") || l.startsWith("/pagefind/")) continue;
    if (!(await exists(l))) broken.push(`${rel} -> ${l}`);
  }
}

if (broken.length) {
  console.error(`check-links: ${broken.length} broken link(s):\n  ${broken.join("\n  ")}`);
  process.exit(1);
}
console.log(`check-links: ${files.length} files, no broken internal links`);
