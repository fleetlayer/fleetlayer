// Turns the repo's docs/ folder (the one source of truth, plain GitHub-readable Markdown with no frontmatter) into
// the site's content, and writes the Markdown copies agents read:
//
//   src/content/docs/<slug>.md   Starlight pages: title from the first "# " heading, links rewritten to site URLs
//   public/<slug>.md             the page as Markdown, links pointing at the other .md copies
//   public/llms.txt              an index of every page (llmstxt.org)
//   public/llms-full.txt         every page in one file
//
// Run before `astro dev` and `astro build` (package.json does it). Generated files are gitignored.

import { readFile, writeFile, mkdir, rm, readdir } from "node:fs/promises";
import { dirname, join, relative, resolve, posix } from "node:path";
import { fileURLToPath } from "node:url";
import { site as SITE, pages as PAGES, repo as REPO } from "../site.config.mjs";

const siteDir = resolve(dirname(fileURLToPath(import.meta.url)), "..");
const repoDir = resolve(siteDir, "..");
const docsDir = join(repoDir, "docs");
const contentDir = join(siteDir, "src/content/docs");
const publicDir = join(siteDir, "public");

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
  await rm(contentDir, { recursive: true, force: true });
  await mkdir(contentDir, { recursive: true });

  const files = await walk(docsDir);
  const known = new Set(files.map(slugOf));
  for (const p of PAGES) if (!known.has(p.file)) throw new Error(`site.config.mjs lists docs/${p.file}.md, which doesn't exist`);

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

    const siteBody = rewriteLinks(body, file, (s) => `/${s}/`);
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

    const mdBody = rewriteLinks(body, file, (s) => `${SITE.url}/${s}.md`);
    const markdown = `# ${page.title ?? title}\n\n${mdBody}`;
    const mdOut = join(publicDir, `${slug}.md`);
    await mkdir(dirname(mdOut), { recursive: true });
    await writeFile(mdOut, markdown);

    docs.push({ slug, title: page.title ?? title, description, group: page.group, markdown });
  }

  // llms.txt: the order and groups of the sidebar.
  const groups = [];
  for (const p of PAGES) {
    const doc = docs.find((d) => d.slug === p.file);
    let g = groups.find((x) => x.name === p.group);
    if (!g) groups.push((g = { name: p.group, docs: [] }));
    g.docs.push(doc);
  }
  const homepage = await readFile(join(siteDir, "src/homepage.md"), "utf8");
  const index = [
    `# ${SITE.title}`,
    "",
    `> ${SITE.summary}`,
    "",
    `Every page is also served as Markdown: add \`.md\` to its address. All of them in one file: ${SITE.url}/llms-full.txt. Source: ${REPO}.`,
    "",
    `- [Fleetlayer, in one page](${SITE.url}/index.md): what it is, who it's for and how to start`,
    ...groups.flatMap((g) => [
      "",
      `## ${g.name}`,
      "",
      ...g.docs.map((d) => `- [${d.title}](${SITE.url}/${d.slug}.md)${d.description ? `: ${d.description}` : ""}`),
    ]),
    "",
  ].join("\n");
  await writeFile(join(publicDir, "llms.txt"), index);
  await writeFile(join(publicDir, "index.md"), homepage);

  const ordered = PAGES.map((p) => docs.find((d) => d.slug === p.file));
  const full = [homepage.trim(), ...ordered.map((d) => `<!-- ${SITE.url}/${d.slug}/ -->\n\n${d.markdown.trim()}`)].join(
    "\n\n---\n\n",
  );
  await writeFile(join(publicDir, "llms-full.txt"), full + "\n");

  console.log(`sync-docs: ${docs.length} pages`);
}

main().catch((err) => {
  console.error(err.message);
  process.exit(1);
});
