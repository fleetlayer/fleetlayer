// Builds the plugin directory's pages from ../plugins-directory/*.yaml: an index (/directory/) and one page per entry
// (/directory/<name>/). Each comes in two forms, for the site and for the Markdown copies agents read; `link` makes
// the address of another directory page in the right form.
import { loadEntries, licenceKind, ROLES } from "./check-directory.mjs";

const ROLE_NAMES = {
  session_manager: "Session manager",
  harness: "Harness",
  event_source: "Event source",
  action: "Runner action",
  sync_target: "Sync target",
  sync_source: "Sync source",
  observer: "Observer",
};

const KIND_NAMES = { plugin: "Plugin", tool: "Command-line tool", service: "Service" };

function licenceText(id) {
  const kind = licenceKind(id);
  if (kind === "copyleft") return `${id} (copyleft)`;
  if (kind === "proprietary") return `${id} (not an open-source licence Fleetlayer recognises)`;
  return id;
}

function installText(e) {
  if (e.install === "bundled") return "In every Fleetlayer release";
  const [k, v] = Object.entries(e.install)[0];
  if (k === "archive") return `Download ${v}`;
  if (k === "cargo") return `\`cargo install ${v}\``;
  if (k === "npm") return `\`npm install -g ${v}\``;
  return `\`${v}\``;
}

function entryPage(e, { docsLink }) {
  const rows = [
    ["Kind", KIND_NAMES[e.kind] ?? e.kind],
    e.kind === "plugin" ? ["Roles", (e.roles ?? []).length ? e.roles.map((r) => ROLE_NAMES[r] ?? r).join(", ") : "None (a service the node runs)"] : null,
    ["Status", e.status],
    ["Ships with Fleetlayer", e.ships_with_fleetlayer ? "Yes" : "No"],
    ["Licence", licenceText(e.licence)],
    ["Maintainer", e.maintainer],
    ["Source", e.source],
    ["Install", installText(e)],
    ["Command", `\`${e.command}\``],
    ["Platforms", e.platforms.map((p) => ({ linux: "Linux", macos: "macOS", windows: "Windows" })[p] ?? p).join(", ")],
    ["Works with", `Fleetlayer ${e.min_fleetlayer} and later`],
    e.protocol !== undefined ? ["Plugin protocol", `version ${e.protocol}`] : null,
    e.needs?.length ? ["Needs", e.needs.join("; ")] : null,
    e.grants?.length ? ["Grants", e.grants.map((g) => `\`${g}\``).join(", ")] : null,
  ].filter(Boolean);
  const parts = [
    e.summary,
    ["| | |", "|---|---|", ...rows.map(([k, v]) => `| ${k} | ${String(v).replace(/\|/g, "\\|")} |`)].join("\n"),
  ];
  if (e.description) parts.push(`## What it does\n\n${e.description.trim()}`);
  if (e.config) {
    const setup = e.ships_with_fleetlayer
      ? "It comes with Fleetlayer, so there's nothing else to install. To start from:"
      : `Install it (${installText(e)}), then start from:`;
    parts.push(`## Setup\n\n${setup}\n\n\`\`\`yaml\n${e.config.trimEnd()}\n\`\`\``);
  }
  if (e.security) parts.push(`## What it can read and do\n\n${e.security.trim()}`);
  if (e.docs) parts.push(`More: ${docsLink(e.docs)}.`);
  return parts.join("\n\n") + "\n";
}

function table(entries, link) {
  if (!entries.length) return "";
  return [
    "| Name | What it does | Kind | Status |",
    "|---|---|---|---|",
    ...entries.map((e) => `| [${e.name}](${link(e.name)}) | ${e.summary} | ${KIND_NAMES[e.kind] ?? e.kind} | ${e.status} |`),
  ].join("\n");
}

function indexPage(entries, { link, contributing }) {
  const bundled = entries.filter((e) => e.ships_with_fleetlayer);
  const others = entries.filter((e) => !e.ships_with_fleetlayer);
  const parts = [
    "Every plugin, tool and service that works with Fleetlayer, including the ones that ship with it. Each page says " +
      "what it does, what it can read and do, what it needs, and how to set it up, so an agent can choose and install " +
      "one from here alone.",
    `## Ships with Fleetlayer\n\n${table(bundled, link)}`,
    `## From others\n\n${others.length ? table(others, link) : "None yet. Yours could be the first: see below."}`,
    "## By role\n\n" +
      ROLES.map((r) => {
        const with_ = entries.filter((e) => (e.roles ?? []).includes(r));
        return with_.length ? `- **${ROLE_NAMES[r]}:** ${with_.map((e) => `[${e.name}](${link(e.name)})`).join(", ")}` : null;
      })
        .filter(Boolean)
        .join("\n"),
    "## Add yours\n\n" +
      "A plugin needs no fork of Fleetlayer: it's a separate program the node starts, in any language. Listing it is a " +
      `pull request that adds one file to \`plugins-directory/\`. The fields and the check are in ${contributing}.`,
  ];
  return parts.join("\n\n") + "\n";
}

export async function directoryPages({ siteUrl, repo }) {
  const entries = await loadEntries();
  const forms = {
    site: { link: (n) => `/directory/${n}/`, docsLink: (d) => (/^https?:/.test(d) ? d : `[the docs](/${d}/)`) },
    md: {
      link: (n) => `${siteUrl}/directory/${n}.md`,
      docsLink: (d) => (/^https?:/.test(d) ? d : `${siteUrl}/${d}.md`),
    },
  };
  const contributing = `[the directory's README](${repo}/blob/main/plugins-directory/README.md)`;
  const pages = [
    {
      slug: "directory",
      title: "Plugin directory",
      description: "Every plugin, tool and service that works with Fleetlayer, and how to add yours.",
      site: indexPage(entries, { link: forms.site.link, contributing }),
      md: indexPage(entries, { link: forms.md.link, contributing }),
    },
  ];
  for (const e of entries) {
    pages.push({
      slug: `directory/${e.name}`,
      title: e.name,
      description: e.summary,
      site: entryPage(e, forms.site),
      md: entryPage(e, forms.md),
      child: true,
    });
  }
  return pages;
}
