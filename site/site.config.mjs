// What the site publishes from docs/, in sidebar order. A doc not listed here (or in `unpublished`) gets a warning
// at build time, so a new doc isn't forgotten.
export const repo = "https://github.com/fleetlayer/fleetlayer";

export const site = {
  url: "https://fleetlayer.ai",
  title: "Fleetlayer",
  summary:
    "Fleetlayer connects and controls the AI agent sessions you already run, on every machine you use, into one fleet you control: one view of every session, messages between them, webhooks routed to the agent that owns the work, shared Claude accounts, and access rules. Open source (Apache 2.0), self-hosted, no vendor lock-in.",
  // Working files that stay in the repo only.
  unpublished: ["guides-to-write", "problems"],
};

// file: path under docs/ without .md. group: the sidebar group. label: a shorter sidebar name (the page keeps its
// own title). description: overrides the first sentence of the page.
export const pages = [
  { file: "getting-started", group: "Start here" },
  { file: "concepts", group: "Start here", label: "How it fits together" },
  { file: "guides/laptop", group: "Guides", label: "Start on your laptop" },
  { file: "guides/node-services", group: "Guides", label: "Add a machine" },
  { file: "guides/agents-vm", group: "Guides", label: "A server VM for your agents" },
  { file: "guides/day-to-day", group: "Guides", label: "Watch, steer and message" },
  { file: "guides/agent-instructions", group: "Guides", label: "Instructions on every machine" },
  { file: "guides/sync", group: "Guides", label: "Sync notes and skills" },
  { file: "guides/claude-accounts", group: "Guides", label: "Many Claude accounts" },
  { file: "guides/browser", group: "Guides", label: "Share your browser" },
  { file: "guides/untrusted-input", group: "Guides", label: "An agent that reads strangers' text" },
  { file: "guides/unattended-agents", group: "Guides", label: "Agents that run while you're away" },
  { file: "guides/pages", group: "Guides", label: "Pages instead of artifacts" },
  { file: "guides/monitoring", group: "Guides", label: "Monitoring" },
  { file: "guides/backups", group: "Guides", label: "Backups" },
  { file: "guides/agents-building-software", group: "Guides", label: "Agents that build software" },
  { file: "guides/continuous-deployment", group: "Guides", label: "Continuous deployment" },
  { file: "guides/make-it-yours", group: "Guides", label: "Make it yours" },
  { file: "directory", group: "Plugins", label: "Plugin directory", generated: true },
  { file: "plugins/summaries", group: "Plugins", label: "Summaries" },
  { file: "plugins/pages", group: "Plugins", label: "Pages" },
  { file: "plugins/apps", group: "Plugins", label: "Apps" },
  { file: "plugins/claude-auth-manager", group: "Plugins", label: "Claude accounts" },
  { file: "plugins/signin", group: "Plugins", label: "Sign-in" },
  { file: "access-control", group: "Reference", label: "Access control" },
  { file: "protocol", group: "Reference", label: "Plugin protocol" },
  { file: "link", group: "Reference", label: "Node link" },
  { file: "sso", group: "Advanced", label: "Sign-in and SSO" },
];
