# fleetlayer.ai

The site: a homepage and the docs, built with [Astro](https://astro.build) and
[Starlight](https://starlight.astro.build) from the repo's `docs/` folder, the one source of truth.

```sh
cd site
npm ci
npm run dev      # http://localhost:4321
npm run build    # dist/
npm run check    # fails on broken internal links
```

- **A new doc** goes in `docs/` as plain Markdown starting with a `# Title` line (no frontmatter), and gets a line in
  `site.config.mjs` (its sidebar group). The build warns about any doc that isn't listed there or in `unpublished`.
- **Links between docs** stay relative (`../access-control.md`), so they work on GitHub too; the build rewrites them.
- **Every page is also Markdown** at its address plus `.md`, and `/llms.txt` and `/llms-full.txt` list and hold them
  all (`scripts/sync-docs.mjs`).
- **The homepage** is `src/pages/index.astro`, and its Markdown twin for agents is `src/homepage.md`: change both.
- **Deploys**: `.github/workflows/site.yml` builds and checks on every pull request and push, and deploys pushes to
  main to Cloudflare Workers (static assets, `wrangler.jsonc`).
- No analytics, trackers or third-party scripts; fonts are self-hosted.
