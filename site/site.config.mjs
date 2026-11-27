// What the site publishes from docs/, in sidebar order. A doc not listed here (or in `unpublished`) gets a warning
// at build time, so a new doc isn't forgotten.
export const repo = "https://github.com/fleetlayer/fleetlayer";

export const site = {
  url: "https://fleetlayer.ai",
  title: "Fleetlayer",
  summary:
    "Fleetlayer connects the AI agent sessions you already run, on every machine you use, into one fleet you control: one view of every session, messages between them, webhooks routed to the agent that owns the work, shared Claude accounts, and access rules. Open source (Apache 2.0), self-hosted, no vendor lock-in.",
  // Working files that stay in the repo only.
  unpublished: ["guides-to-write", "problems"],
};

// file: path under docs/ without .md. group: the sidebar group. label: a shorter sidebar name (the page keeps its
// own title). description: overrides the first sentence of the page.
export const pages = [
  { file: "getting-started", group: "Start here" },
  { file: "guides/laptop", group: "Guides", label: "Start on your laptop" },
  { file: "guides/untrusted-input", group: "Guides", label: "An agent that reads strangers' text" },
  { file: "guides/backups", group: "Guides", label: "Backups" },
  { file: "plugins/summaries", group: "Plugins", label: "Summaries" },
  { file: "plugins/pages", group: "Plugins", label: "Pages" },
  { file: "plugins/apps", group: "Plugins", label: "Apps" },
  { file: "plugins/claude-auth-manager", group: "Plugins", label: "Claude accounts" },
  { file: "access-control", group: "Reference", label: "Access control" },
  { file: "protocol", group: "Reference", label: "Plugin protocol" },
  { file: "link", group: "Reference", label: "Node link" },
  { file: "sso", group: "Advanced", label: "Sign-in and SSO" },
];
