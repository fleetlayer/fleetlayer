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
  { file: "guides/laptop", group: "Set up", label: "Start on your laptop" },
  { file: "guides/node-services", group: "Set up", label: "Add a machine" },
  { file: "guides/agents-vm", group: "Set up", label: "A server VM for your agents" },
  { file: "guides/mac-vm", group: "Set up", label: "Agents on a Mac, inside a Linux VM" },
  { file: "guides/server-setup", group: "Set up", label: "A whole server, set up by an agent" },
  { file: "guides/moving", group: "Set up", label: "Move a piece to another machine" },
  { file: "guides/day-to-day", group: "Day to day", label: "Watch, steer and message" },
  { file: "guides/phone", group: "Day to day", label: "Every machine on your phone" },
  { file: "guides/ci-results", group: "Day to day", label: "CI results to the right agent" },
  { file: "guides/agent-instructions", group: "Day to day", label: "Instructions on every machine" },
  { file: "guides/agents-know-fleetlayer", group: "Day to day", label: "Tell your agents about Fleetlayer" },
  { file: "guides/sync", group: "Day to day", label: "Sync notes and skills" },
  { file: "guides/claude-accounts", group: "Shared tools and accounts", label: "Many Claude accounts" },
  { file: "guides/browser", group: "Shared tools and accounts", label: "Share your browser" },
  { file: "guides/shared-folders", group: "Shared tools and accounts", label: "Your screenshots and downloads" },
  { file: "guides/pages", group: "Shared tools and accounts", label: "Pages instead of artifacts" },
  { file: "guides/executor", group: "Shared tools and accounts", label: "Executor behind Fleetlayer" },
  { file: "guides/untrusted-input", group: "Security", label: "An agent that reads strangers' text" },
  { file: "guides/egress", group: "Security", label: "Keep agents to the hosts they need" },
  { file: "guides/admin-window", group: "Security", label: "An admin role that's off until you open it" },
  { file: "guides/sign-in", group: "Security", label: "One sign-in for your services" },
  { file: "sso", group: "Security", label: "Single sign-on, teams and tenants" },
  { file: "guides/unattended-agents", group: "Running it", label: "Agents that run while you're away" },
  { file: "guides/monitoring", group: "Running it", label: "Monitoring" },
  { file: "guides/backups", group: "Running it", label: "Backups" },
  { file: "guides/agents-building-software", group: "Building software", label: "Agents that build software" },
  { file: "guides/continuous-deployment", group: "Building software", label: "Continuous deployment" },
  { file: "guides/make-it-yours", group: "Building software", label: "Make it yours" },
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
  { file: "plugins/leases", group: "Plugins", label: "Leases" },
  { file: "plugins/herdr-fleet", group: "Plugins", label: "Every herdr as one" },
  { file: "plugins/cookies", group: "Plugins", label: "Cookies" },
  { file: "plugins/1password", group: "Plugins", label: "1Password" },
  { file: "credentials", group: "Reference", label: "Credentials" },
  { file: "egress", group: "Reference", label: "Network access" },
  { file: "access-control", group: "Reference", label: "Access control" },
  { file: "releasing", group: "Reference", label: "Releases and updates" },
  { file: "protocol", group: "Reference", label: "Plugin protocol" },
  { file: "link", group: "Reference", label: "Node link" },
];
