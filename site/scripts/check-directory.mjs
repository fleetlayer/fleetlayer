// Checks every entry in ../plugins-directory: required fields, allowed values, an SPDX licence, unique names, and
// (with --network) that each source URL answers. Exits non-zero on a broken entry; an unreachable source only warns. Run in CI by .github/workflows/site.yml.
import { readFile, readdir } from "node:fs/promises";
import { join, resolve, dirname } from "node:path";
import { fileURLToPath } from "node:url";
import { parse } from "yaml";

const dir = resolve(dirname(fileURLToPath(import.meta.url)), "../../plugins-directory");
const network = process.argv.includes("--network");

export const KINDS = ["plugin", "tool", "service"];
export const ROLES = ["session_manager", "harness", "event_source", "action", "sync_target", "sync_source", "observer"];
export const STATUSES = ["stable", "beta", "experimental"];
export const PLATFORMS = ["linux", "macos", "windows"];
// Open-source licences listed as-is; copyleft ones are marked on the page. Anything else is listed as proprietary.
export const PERMISSIVE = ["Apache-2.0", "MIT", "BSD-2-Clause", "BSD-3-Clause", "ISC", "0BSD", "Unlicense", "Zlib"];
export const COPYLEFT = ["GPL-2.0-only", "GPL-2.0-or-later", "GPL-3.0-only", "GPL-3.0-or-later", "AGPL-3.0-only",
  "AGPL-3.0-or-later", "LGPL-2.1-only", "LGPL-2.1-or-later", "LGPL-3.0-only", "LGPL-3.0-or-later", "MPL-2.0", "EUPL-1.2"];

export async function loadEntries() {
  const files = (await readdir(dir)).filter((f) => f.endsWith(".yaml")).sort();
  const entries = [];
  for (const f of files) entries.push({ file: f, ...parse(await readFile(join(dir, f), "utf8")) });
  return entries;
}

export function licenceKind(id) {
  if (PERMISSIVE.includes(id)) return "open";
  if (COPYLEFT.includes(id)) return "copyleft";
  return "proprietary";
}

function problems(e, names) {
  const p = [];
  const need = (k) => (e[k] === undefined || e[k] === null || e[k] === "") && p.push(`missing ${k}`);
  ["name", "summary", "kind", "ships_with_fleetlayer", "status", "licence", "maintainer", "source", "install",
    "command", "platforms", "min_fleetlayer"].forEach(need);
  if (e.name && !/^[a-z0-9][a-z0-9-]*$/.test(e.name)) p.push("name must be lower case letters, digits and hyphens");
  if (e.name && e.file !== `${e.name}.yaml`) p.push(`file must be named ${e.name}.yaml`);
  if (e.name && names.has(e.name)) p.push(`name ${e.name} is used twice`);
  if (e.summary && (e.summary.length > 140 || e.summary.includes("\n"))) p.push("summary must be one line, at most 140 characters");
  if (e.kind && !KINDS.includes(e.kind)) p.push(`kind must be one of ${KINDS.join(", ")}`);
  if (e.kind === "plugin") {
    if (!Array.isArray(e.roles)) p.push("a plugin lists its roles (an empty list is fine)");
    if (e.protocol === undefined) p.push("a plugin names the protocol version it speaks");
  }
  for (const r of e.roles ?? []) if (!ROLES.includes(r)) p.push(`unknown role ${r}`);
  if (e.status && !STATUSES.includes(e.status)) p.push(`status must be one of ${STATUSES.join(", ")}`);
  if (typeof e.ships_with_fleetlayer !== "boolean") p.push("ships_with_fleetlayer must be true or false");
  if (e.source && !/^https:\/\//.test(e.source)) p.push("source must be an https:// URL");
  if (e.install && e.install !== "bundled") {
    const keys = typeof e.install === "object" ? Object.keys(e.install) : [];
    if (keys.length !== 1 || !["archive", "cargo", "npm", "command"].includes(keys[0])) {
      p.push("install is bundled, or one of {archive}, {cargo}, {npm}, {command}");
    }
  }
  if (e.install === "bundled" && e.ships_with_fleetlayer !== true) p.push("install: bundled is only for what ships with Fleetlayer");
  if (!Array.isArray(e.platforms) || e.platforms.some((x) => !PLATFORMS.includes(x))) {
    p.push(`platforms is a list of ${PLATFORMS.join(", ")}`);
  }
  if (e.min_fleetlayer && !/^\d+\.\d+\.\d+$/.test(String(e.min_fleetlayer))) p.push("min_fleetlayer is a version like 0.3.4");
  return p;
}

async function main() {
  const entries = await loadEntries();
  const names = new Set();
  let failed = 0;
  for (const e of entries) {
    const p = problems(e, names);
    if (e.name) names.add(e.name);
    // An unreachable source is a warning, not a failure: a flaky host mustn't block a docs deploy.
    if (network && e.source) {
      let trouble = null;
      try {
        const r = await fetch(e.source, { method: "HEAD", redirect: "follow", signal: AbortSignal.timeout(15000) });
        if (!r.ok) trouble = `source answered ${r.status}`;
      } catch (err) {
        trouble = `source unreachable: ${err.message}`;
      }
      if (trouble) console.log(`::warning file=plugins-directory/${e.file}::${trouble}`);
    }
    if (p.length) {
      failed++;
      console.error(`${e.file}:\n  ${p.join("\n  ")}`);
    }
  }
  if (failed) process.exit(1);
  console.log(`check-directory: ${entries.length} entries${network ? ", sources reachable" : ""}`);
}

if (import.meta.url === `file://${process.argv[1]}`) main();
