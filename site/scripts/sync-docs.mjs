// Turns the repo's docs/ folder (the one source of truth, plain GitHub-readable Markdown with no frontmatter) into
// the site's content, and writes the Markdown copies agents read:
//
//   src/content/docs/docs/<slug>.md   Starlight pages at /docs/<slug>/: title from the first "# " heading, links
//                                     rewritten to site URLs
//   src/content/docs/docs/index.md    /docs/: every page, by sidebar group
//   public/docs/<slug>.md             the page as Markdown, links pointing at the other .md copies
//   public/llms.txt                   an index of every page (llmstxt.org), at the site's root by convention
//   public/llms-full.txt              every page in one file
//
// Run before `astro dev` and `astro build` (package.json does it). Generated files are gitignored.

import { readFile, writeFile, mkdir, rm, readdir } from "node:fs/promises";
import { dirname, join, relative, resolve, posix } from "node:path";
import { fileURLToPath } from "node:url";
import { site as SITE, pages as PAGES, repo as REPO, docsBase as BASE } from "../site.config.mjs";
import { directoryPages } from "./directory-pages.mjs";

const siteDir = resolve(dirname(fileURLToPath(import.meta.url)), "..");
const repoDir = resolve(siteDir, "..");
const docsDir = join(repoDir, "docs");
const contentRoot = join(siteDir, "src/content/docs");
const contentDir = join(contentRoot, BASE);
const publicDir = join(siteDir, "public", BASE);
const publicRoot = join(siteDir, "public");
// A doc's address on the site, and its Markdown copy's.
const pageUrl = (slug) => `/${BASE}/${slug}/`;
const mdUrl = (slug) => `${SITE.url}/${BASE}/${slug}.md`;

// In Markdown meant for agents, every link to one of our pages goes to its .md copy, never the HTML page with its
// navigation: https://fleetlayer.ai/docs/guides/pages/ becomes .../docs/guides/pages.md, /docs/ becomes /docs.md and
// the homepage becomes /index.md. check-links fails on any that slip through.
const siteRe = SITE.url.replace(/[.]/g, "\\.");
function toMarkdownLinks(text) {
  return text
    .replace(new RegExp(`${siteRe}/${BASE}/([a-z0-9/_-]+?)/(?=[#)\\s"'>\\]]|$)`, "g"), (_, slug) => mdUrl(slug))
    .replace(new RegExp(`${siteRe}/${BASE}/(?=[#)\\s"'>\\]]|$)`, "g"), `${SITE.url}/${BASE}.md`)
    .replace(new RegExp(`${siteRe}/(?=[#)\\s"'>\\]]|$)`, "g"), `${SITE.url}/index.md`);
}

async function walk(dir) {
  const out = [];
  for (const entry of await readdir(dir, { withFileTypes: true })) {
    const path = join(dir, entry.name);
    if (entry.isDirectory()) out.push(...(await walk(path)));
    else if (entry.name.endsWith(".md")) out.push(path);
  }
  return out;
}

// docs/plugins/pages.md -> plugins/pages
const slugOf = (file) => relative(docsDir, file).replace(/\.md$/, "").split("\\").join("/");

const published = new Map(PAGES.map((p) => [p.file, p]));

function splitTitle(source, file) {
  const lines = source.split("\n");
  const i = lines.findIndex((l) => l.trim() !== "");
  if (!lines[i]?.startsWith("# ")) throw new Error(`${file}: the first line must be a "# Title" heading`);
  const title = lines[i].slice(2).trim();
  const body = lines.slice(i + 1).join("\n").replace(/^\n+/, "");
  return { title, body };
}

// The first sentence of the first paragraph, as the page's description.
function describe(body) {
  const para = body
    .split(/\n\s*\n/)
    .map((p) => p.trim())
    .find((p) => p && !/^(#|```|\||- |\d+\. |>|<)/.test(p));
  if (!para) return undefined;
  const flat = para.replace(/\s+/g, " ").replace(/\*\*|`/g, "").replace(/\[([^\]]+)\]\([^)]+\)/g, "$1");
  const m = flat.match(/^.+?[.!?](?=\s|$)/);
  return (m ? m[0] : flat).slice(0, 300);
}

// Rewrites relative Markdown links. A link to another published doc goes to its page (or its .md copy); a link to
// anything else in the repo goes to GitHub. Code fences are left alone.
function rewriteLinks(body, file, toUrl) {
  const fromDir = dirname(relative(repoDir, file));
  let inFence = false;
  return body
    .split("\n")
    .map((line) => {
      if (/^\s*(```|~~~)/.test(line)) inFence = !inFence;
      if (inFence) return line;
      return line.replace(/(\]\()([^)\s]+)(\))/g, (all, open, target, close) => {
        if (/^([a-z]+:|#|\/)/i.test(target)) return all;
        const [path, anchor] = target.split("#");
        const repoPath = posix.normalize(posix.join(fromDir.split("\\").join("/"), path));
        const hash = anchor ? `#${anchor}` : "";
        if (repoPath.startsWith("docs/") && repoPath.endsWith(".md")) {
          const slug = repoPath.slice(5, -3);
          if (published.has(slug)) return `${open}${toUrl(slug)}${hash}${close}`;
        }
        const kind = /\.[a-z0-9]+$/i.test(repoPath) ? "blob" : "tree";
        return `${open}${REPO}/${kind}/main/${repoPath}${hash}${close}`;
      });
    })
    .join("\n");
}

async function main() {
  await rm(contentRoot, { recursive: true, force: true });
  // Everything in public/ is generated except these (kept in git, as site/.gitignore lists them), so clear the rest: a
  // copy left from an older layout (public/guides/, public/rex.md) would otherwise be served and fail the link check.
  // img/ holds the homepage's screenshots.
  const kept = new Set(["_headers", "favicon.svg", "img"]);
  for (const entry of await readdir(publicRoot)) {
    if (!kept.has(entry)) await rm(join(publicRoot, entry), { recursive: true, force: true });
  }
  await mkdir(contentDir, { recursive: true });

  const files = await walk(docsDir);
  const known = new Set(files.map(slugOf));
  for (const p of PAGES) {
    if (!p.generated && !known.has(p.file)) throw new Error(`site.config.mjs lists docs/${p.file}.md, which doesn't exist`);
    if (!p.generated && !p.description) {
      throw new Error(`site.config.mjs: docs/${p.file}.md needs a description: one sentence on what the page helps you do`);
    }
  }

  const unlisted = [...known].filter((s) => !published.has(s) && !SITE.unpublished.includes(s));
  if (unlisted.length) {
    console.warn(`sync-docs: not on the site yet (add to site.config.mjs pages or unpublished): ${unlisted.join(", ")}`);
  }

  const docs = [];
  for (const file of files) {
    const slug = slugOf(file);
    const page = published.get(slug);
    if (!page) continue;
    const source = await readFile(file, "utf8");
    const { title, body } = splitTitle(source, relative(repoDir, file));
    const description = page.description ?? describe(body);

    const siteBody = rewriteLinks(body, file, pageUrl);
    const front = [
      "---",
      `title: ${JSON.stringify(page.title ?? title)}`,
      description ? `description: ${JSON.stringify(description)}` : null,
      `editUrl: ${JSON.stringify(`${REPO}/edit/main/docs/${slug}.md`)}`,
      page.label ? `sidebar:\n  label: ${JSON.stringify(page.label)}` : null,
      "---",
      "",
    ]
      .filter((l) => l !== null)
      .join("\n");
    const out = join(contentDir, `${slug}.md`);
    await mkdir(dirname(out), { recursive: true });
    await writeFile(out, front + siteBody);

    const mdBody = rewriteLinks(body, file, mdUrl);
    const markdown = toMarkdownLinks(`# ${page.title ?? title}\n\n${mdBody}`);
    const mdOut = join(publicDir, `${slug}.md`);
    await mkdir(dirname(mdOut), { recursive: true });
    await writeFile(mdOut, markdown);

    docs.push({ slug, title: page.title ?? title, description, group: page.group, markdown });
  }

  // The plugin directory, generated from ../plugins-directory/*.yaml.
  const children = [];
  for (const d of await directoryPages({ pageUrl, mdUrl, repo: REPO })) {
    const page = published.get(d.slug);
    const front = [
      "---",
      `title: ${JSON.stringify(d.title)}`,
      `description: ${JSON.stringify(d.description)}`,
      `editUrl: ${JSON.stringify(`${REPO}/edit/main/plugins-directory/${d.child ? `${d.slug.slice(10)}.yaml` : "README.md"}`)}`,
      page?.label ? `sidebar:\n  label: ${JSON.stringify(page.label)}` : null,
      "---",
      "",
    ]
      .filter((l) => l !== null)
      .join("\n");
    const out = join(contentDir, `${d.slug}.md`);
    await mkdir(dirname(out), { recursive: true });
    await writeFile(out, front + d.site);
    const markdown = toMarkdownLinks(`# ${d.title}\n\n${d.md}`);
    const mdOut = join(publicDir, `${d.slug}.md`);
    await mkdir(dirname(mdOut), { recursive: true });
    await writeFile(mdOut, markdown);
    const doc = { slug: d.slug, title: d.title, description: d.description, markdown };
    if (d.child) children.push(doc);
    else docs.push(doc);
  }

  // llms.txt: the order and groups of the sidebar.
  const groups = [];
  for (const p of PAGES) {
    const doc = docs.find((d) => d.slug === p.file);
    let g = groups.find((x) => x.name === p.group);
    if (!g) groups.push((g = { name: p.group, docs: [] }));
    g.docs.push(doc);
  }
  groups.push({ name: "Plugin directory entries", docs: children });
  const homepage = toMarkdownLinks(await readFile(join(siteDir, "src/homepage.md"), "utf8"));
  // An uptime check expects llms.txt to start with "# Fleetlayer": tell the homeserver session before changing it.
  const index = [
    `# ${SITE.title}`,
    "",
    `> ${SITE.summary}`,
    "",
    `Licence: ${SITE.licence}. Source: ${REPO}.`,
    "",
    `Every page is also served as Markdown: add \`.md\` to its address. The core docs in one file: ${SITE.url}/llms-full.txt. Each group below is also one file, linked beside its heading.`,
    "",
    `- [Fleetlayer, in one page](${SITE.url}/index.md): what it is, who it's for and how to start`,
    ...groups.flatMap((g) => [
      "",
      `## ${g.name}`,
      "",
      `All of it in one file: ${SITE.url}/llms/${g.name.toLowerCase().replace(/[^a-z0-9]+/g, "-").replace(/^-|-$/g, "")}.txt`,
      "",
      ...g.docs.map((d) => `- [${d.title}](${mdUrl(d.slug)})${d.description ? `: ${d.description}` : ""}`),
    ]),
    "",
  ].join("\n");
  await writeFile(join(publicRoot, "llms.txt"), index);
  await writeFile(join(publicRoot, "index.md"), homepage);

  // /docs/: every page by group, so the address works on its own.
  const listing = groups
    .filter((g) => g.name !== "Plugin directory entries")
    .map((g) => [`## ${g.name}`, "", ...g.docs.map((d) => `- [${d.title}](LINK:${d.slug})${d.description ? `: ${d.description}` : ""}`)].join("\n"))
    .join("\n\n");
  const intro = `Everything about running Fleetlayer: start with [getting started](LINK:getting-started), then the guides for what you want to do.`;
  const docsIndex = (link) => `${intro}\n\n${listing}\n`.replace(/LINK:([a-z0-9/-]+)/g, (_, s) => link(s));
  await writeFile(
    join(contentDir, "index.md"),
    `---\ntitle: "Documentation"\ndescription: "Every page of Fleetlayer's docs and guides."\nsidebar:\n  hidden: true\n---\n\n${docsIndex(pageUrl)}`,
  );
  await writeFile(join(publicRoot, `${BASE}.md`), `# Documentation\n\n${docsIndex(mdUrl)}`);

  // One file per group, and llms-full.txt with the core groups, so each fits in an agent's context.
  const slugOfGroup = (name) => name.toLowerCase().replace(/[^a-z0-9]+/g, "-").replace(/^-|-$/g, "");
  const section = (d) => `<!-- ${mdUrl(d.slug)} -->\n\n${d.markdown.trim()}`;
  const groupFiles = groups.map((g) => ({ ...g, url: `${SITE.url}/llms/${slugOfGroup(g.name)}.txt` }));
  await mkdir(join(publicRoot, "llms"), { recursive: true });
  for (const g of groupFiles) {
    const text = [`# ${SITE.title}: ${g.name}`, ...g.docs.map(section)].join("\n\n---\n\n");
    await writeFile(join(publicRoot, "llms", `${slugOfGroup(g.name)}.txt`), text + "\n");
  }
  const groupList = groupFiles.map((g) => `- ${g.name}: ${g.url}`).join("\n");
  const core = groupFiles.filter((g) => SITE.llmsFull.includes(g.name));
  const head = `# ${SITE.title}: the core docs\n\nThis file holds ${core.map((g) => g.name).join(", ")}. Every group has a file of its own:\n\n${groupList}`;
  const full = [head, homepage.trim(), ...core.flatMap((g) => g.docs.map(section))].join("\n\n---\n\n");
  await writeFile(join(publicRoot, "llms-full.txt"), full + "\n");

  console.log(`sync-docs: ${docs.length + children.length} pages`);
}

main().catch((err) => {
  console.error(err.message);
  process.exit(1);
});
