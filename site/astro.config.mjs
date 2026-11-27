import { defineConfig } from "astro/config";
import starlight from "@astrojs/starlight";
import { site, pages, repo } from "./site.config.mjs";

// Sidebar groups in the order site.config.mjs lists them. Advanced starts collapsed.
const groups = [];
for (const p of pages) {
  let g = groups.find((x) => x.label === p.group);
  if (!g) groups.push((g = { label: p.group, items: [], collapsed: p.group === "Advanced" }));
  g.items.push(p.file);
}

export default defineConfig({
  site: site.url,
  trailingSlash: "always",
  integrations: [
    starlight({
      title: site.title,
      description: site.summary,
      logo: { src: "./src/assets/mark.svg", replacesTitle: false },
      favicon: "/favicon.svg",
      // Privacy-friendly analytics by Plausible (no cookies). The homepage adds the same in src/pages/index.astro.
      head: [
        { tag: "script", attrs: { async: true, src: "https://pa.docspring.com/js/pa-uqEecMZ-VpxrX9c2zdVKv.js" } },
        {
          tag: "script",
          content:
            "window.plausible=window.plausible||function(){(plausible.q=plausible.q||[]).push(arguments)},plausible.init=plausible.init||function(i){plausible.o=i||{}};plausible.init()",
        },
      ],
      social: [{ icon: "github", label: "GitHub", href: repo }],
      sidebar: groups,
      customCss: ["./src/styles/docs.css"],
      lastUpdated: false,
      pagination: true,
      // Plain code frames: no drawn terminal window around shell blocks.
      expressiveCode: { defaultProps: { frame: "code" } },
      components: {
        PageTitle: "./src/components/PageTitle.astro",
      },
    }),
  ],
});
