// What the site publishes from docs/, in sidebar order. A doc not listed here (or in `unpublished`) gets a warning
// at build time, so a new doc isn't forgotten.
export const repo = "https://github.com/fleetlayer/fleetlayer";

// Every docs page is published under /docs/ (fleetlayer.ai's root is for the product). The repo's docs/ folder and its
// relative links don't change: docs/guides/laptop.md is /docs/guides/laptop/ and /docs/guides/laptop.md.
export const docsBase = "docs";

export const site = {
  url: "https://fleetlayer.ai",
  title: "Fleetlayer",
  summary:
    "An open framework for running AI agents securely and at scale: it connects the Claude Code and Codex sessions you already run in herdr, on your laptop, a home server or a VPS, so any agent can message any other, webhooks reach the right agent, and each agent gets its own access rules. Source-available, self-hosted, no vendor lock-in.",
  // Working files that stay in the repo only.
  unpublished: ["guides-to-write", "problems"],
};

// file: path under docs/ without .md. group: the sidebar group. label: a shorter sidebar name (the page keeps its
// own title). description: overrides the first sentence of the page.
export const pages = [
  { file: "getting-started", group: "Start here" },
  { file: "concepts", group: "Start here", label: "How it fits together" },
  { file: "dashboard", group: "Start here", label: "The dashboard" },
  {
    file: "rex",
    group: "Works with",
    label: "Rex",
    description:
      "How Fleetlayer works with Rex, the terminal and session manager from Superlogical: Rex keeps your sessions, and Fleetlayer lets the Claude Code and Codex agents in them message each other, receive webhooks and work under access rules.",
  },
  { file: "guides/laptop", group: "Guides", label: "Start on your laptop" },
  { file: "guides/node-services", group: "Guides", label: "Add a machine" },
  { file: "guides/moving", group: "Guides", label: "Move a piece to another machine" },
  { file: "guides/agents-vm", group: "Guides", label: "A server VM for your agents" },
  { file: "guides/server-setup", group: "Guides", label: "A whole server, set up by an agent" },
  { file: "guides/day-to-day", group: "Guides", label: "Watch, steer and message" },
  { file: "guides/ci-results", group: "Guides", label: "CI results to the right agent" },
  { file: "guides/agent-instructions", group: "Guides", label: "Instructions on every machine" },
  { file: "guides/agents-know-fleetlayer", group: "Guides", label: "Tell your agents about Fleetlayer" },
  { file: "guides/sync", group: "Guides", label: "Sync notes and skills" },
  { file: "guides/claude-accounts", group: "Guides", label: "Many Claude accounts" },
  { file: "guides/browser", group: "Guides", label: "Share your browser" },
  { file: "guides/shared-folders", group: "Guides", label: "Your screenshots and downloads" },
  { file: "guides/phone", group: "Guides", label: "Every machine on your phone" },
  { file: "guides/untrusted-input", group: "Guides", label: "An agent that reads strangers' text" },
  { file: "guides/admin-window", group: "Guides", label: "An admin role that's off until you open it" },
  { file: "guides/executor", group: "Guides", label: "Executor behind Fleetlayer" },
  { file: "guides/unattended-agents", group: "Guides", label: "Agents that run while you're away" },
  { file: "guides/egress", group: "Guides", label: "Keep agents to the hosts they need" },
  { file: "guides/pages", group: "Guides", label: "Pages instead of artifacts" },
  { file: "guides/sign-in", group: "Guides", label: "One sign-in for your services" },
  { file: "guides/monitoring", group: "Guides", label: "Monitoring" },
  { file: "guides/backups", group: "Guides", label: "Backups" },
  { file: "guides/agents-building-software", group: "Guides", label: "Agents that build software" },
  { file: "guides/continuous-deployment", group: "Guides", label: "Continuous deployment" },
  { file: "guides/make-it-yours", group: "Guides", label: "Make it yours" },
  { file: "directory", group: "Plugins", label: "Plugin directory", generated: true },
  { file: "plugins/rex", group: "Plugins", label: "Rex" },
  { file: "plugins/summaries", group: "Plugins", label: "Summaries" },
  { file: "plugins/pages", group: "Plugins", label: "Pages" },
  { file: "plugins/apps", group: "Plugins", label: "Apps" },
  { file: "plugins/claude-auth-manager", group: "Plugins", label: "Claude accounts" },
  { file: "plugins/signin", group: "Plugins", label: "Sign-in" },
  { file: "plugins/shared-folders", group: "Plugins", label: "Folders" },
  { file: "plugins/scheduler", group: "Plugins", label: "Scheduler" },
  { file: "plugins/matrix", group: "Plugins", label: "Matrix rooms" },
  { file: "plugins/herdr-fleet", group: "Plugins", label: "Every herdr as one" },
  { file: "plugins/cookies", group: "Plugins", label: "Cookies" },
  { file: "plugins/1password", group: "Plugins", label: "1Password" },
  { file: "credentials", group: "Reference", label: "Credentials" },
  { file: "egress", group: "Reference", label: "Network access" },
  { file: "access-control", group: "Reference", label: "Access control" },
  { file: "releasing", group: "Reference", label: "Releases and updates" },
  { file: "protocol", group: "Reference", label: "Plugin protocol" },
  { file: "link", group: "Reference", label: "Node link" },
  { file: "sso", group: "Advanced", label: "Sign-in and SSO" },
];
