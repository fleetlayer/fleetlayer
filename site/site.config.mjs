// What the site publishes from docs/, in sidebar order. A doc not listed here (or in `unpublished`) gets a warning
// at build time, so a new doc isn't forgotten.
export const repo = "https://github.com/fleetlayer/fleetlayer";

// Every docs page is published under /docs/ (fleetlayer.ai's root is for the product). The repo's docs/ folder and its
// relative links don't change: docs/guides/laptop.md is /docs/guides/laptop/ and /docs/guides/laptop.md.
export const docsBase = "docs";

export const site = {
  url: "https://fleetlayer.ai",
  title: "Fleetlayer",
  licence:
    "FSL-1.1-ALv2 (Functional Source License, source-available: use it for anything except a competing product or service; each release becomes Apache 2.0 two years after it ships)",
  summary:
    "An open framework for running AI agents securely and at scale: it connects the Claude Code and Codex sessions you already run in herdr, on your laptop, a home server or a VPS, so any agent can message any other, webhooks reach the right agent, and each agent gets its own access rules. Source-available, self-hosted, no vendor lock-in.",
  // The groups whose pages go into llms-full.txt, the core an agent reads first. Every group also has a file of its own,
  // /llms/<group>.txt, listed at the top of llms-full.txt and in llms.txt.
  llmsFull: ["Start here", "Works with", "Set up", "Day to day", "Comparisons"],
  // Working files that stay in the repo only.
  unpublished: ["guides-to-write", "problems"],
};

// file: path under docs/ without .md. group: the sidebar group. label: a shorter sidebar name (the page keeps its
// own title). description (required): one sentence on what the page helps you do, shown in llms.txt, search results
// and the page's meta description. Write it for someone deciding whether to open the page, never a status line.
export const pages = [
  { file: "getting-started", group: "Start here", description: "Install Fleetlayer on your laptop in a few minutes, link a second machine, and send your first message between agents." },
  { file: "concepts", group: "Start here", label: "How it fits together", description: "How the pieces fit: the control plane, a node on each machine, plugins for session managers and tools, and the access policy over all of it." },
  { file: "dashboard", group: "Start here", label: "The dashboard", description: "The web page for people: every machine and session, what needs you, events, approvals and the rules in force, live." },
  { file: "rex", group: "Works with", label: "Rex", description: "Use Fleetlayer with Rex, Superlogical's terminal and session manager: Rex keeps your sessions, and Fleetlayer connects the agents in them." },
  { file: "guides/laptop", group: "Set up", label: "Start on your laptop", description: "Run everything on one Mac or Linux laptop with fleetlayer up, then grow from there." },
  { file: "guides/node-services", group: "Set up", label: "Add a machine", description: "Add a server, a second Mac or another Unix user to your fleet, as a node that runs as a service, updates itself and reports its health." },
  { file: "guides/agents-vm", group: "Set up", label: "A server VM for your agents", description: "Set up an always-on Linux VM for your agents: a user without sudo, SSH from your laptop, herdr, Claude Code and a node." },
  { file: "guides/mac-vm", group: "Set up", label: "Agents on a Mac, inside a Linux VM", description: "Run your Mac's agents inside a Linux VM (Lima or OrbStack), so a stray rm -rf hits only the VM, while dev servers still open in your browser." },
  { file: "guides/server-setup", group: "Set up", label: "A whole server, set up by an agent", description: "Have an agent set up a whole server from a worked example: a Unix user per kind of agent, backups, monitoring and secrets." },
  { file: "guides/moving", group: "Set up", label: "Move a piece to another machine", description: "Move the control plane, a node, sign-in or any plugin to another machine with one command, keeping its identity, keys and data." },
  { file: "guides/day-to-day", group: "Day to day", label: "Watch, steer and message", description: "See what every agent on every machine is doing, steer one, send messages between them, and start or resume sessions anywhere." },
  { file: "guides/phone", group: "Day to day", label: "Every machine on your phone", description: "See and answer every agent on every machine from your phone, through one herdr phone relay." },
  { file: "guides/ci-results", group: "Day to day", label: "CI results to the right agent", description: "Get each CI result to the agent that pushed the commit, through a webhook instead of polling GitHub." },
  { file: "guides/agent-instructions", group: "Day to day", label: "Instructions on every machine", description: "Keep one set of agent instructions on every machine, with a per-machine file for what's true only there." },
  { file: "guides/agents-know-fleetlayer", group: "Day to day", label: "Tell your agents about Fleetlayer", description: "Tell every agent what Fleetlayer gives it: the block for their instructions and the skills to install." },
  { file: "guides/sync", group: "Day to day", label: "Sync notes and skills", description: "Keep notes, skills and agent instructions the same on every machine, and know when a sync stops." },
  { file: "guides/claude-accounts", group: "Shared tools and accounts", label: "Many Claude accounts", description: "Use your own Claude accounts on your own machines: year-long setup tokens on every machine, usage in one place, and a rotator that moves sessions off a full account." },
  { file: "guides/browser", group: "Shared tools and accounts", label: "Share your browser", description: "Let agents on any machine use your signed-in Chrome on your laptop, for any harness and any Claude account." },
  { file: "guides/shared-folders", group: "Shared tools and accounts", label: "Your screenshots and downloads", description: "Let agents anywhere read your Mac's screenshots and downloads, and upload or delete where you allow it." },
  { file: "guides/pages", group: "Shared tools and accounts", label: "Pages instead of artifacts", description: "Have agents publish screenshots, reports and mock-ups as pages behind one sign-in, instead of vendor artifacts only one account can open." },
  { file: "guides/executor", group: "Shared tools and accounts", label: "Executor behind Fleetlayer", description: "Put Executor's MCP server for Gmail, Slack and the rest behind Fleetlayer, so no agent holds its token and the policy decides each call." },
  { file: "guides/shared-commands", group: "Shared tools and accounts", label: "Run approved commands on another machine", description: "Let agents elsewhere run a few declared commands on your laptop (a deploy that needs your security key), checked and approved, without a shell." },
  { file: "guides/untrusted-input", group: "Security", label: "An agent that reads strangers' text", description: "Run an agent that reads email, issues or web pages in a Unix user of its own, limited in whom it may message and what it may reach." },
  { file: "guides/egress", group: "Security", label: "Keep agents to the hosts they need", description: "Keep each role's agents to the hosts they need, with a proxy that never decrypts and approvals for new hosts." },
  { file: "guides/admin-window", group: "Security", label: "An admin role that's off until you open it", description: "Keep the agent that can change everything switched off until you approve a window, then close it and clean up when the time is up." },
  { file: "guides/sign-in", group: "Security", label: "One sign-in for your services", description: "Give people one passkey sign-in for every self-hosted web service, with who may use what in the access policy." },
  { file: "sso", group: "Security", label: "Single sign-on, teams and tenants", description: "How sign-in, single sign-on, teams and separate organisations work, for a household, a team or a company." },
  { file: "guides/unattended-agents", group: "Running it", label: "Agents that run while you're away", description: "Keep sessions working overnight: the prompts, sign-ins and folder checks that stop an agent nobody is watching, and how to avoid each." },
  { file: "guides/shared-build-machines", group: "Running it", label: "Many agents, a few build machines", description: "Share one or two build machines among many agents without running them out of memory: slots, limits and a cache." },
  { file: "guides/monitoring", group: "Running it", label: "Monitoring", description: "Watch everything that runs in the background with heartbeats and health checks, and have an agent fix what breaks." },
  { file: "guides/backups", group: "Running it", label: "Backups", description: "Back up what your agents create (notes, pages, sign-ins, databases, history) and check that a restore works." },
  { file: "guides/agents-building-software", group: "Building software", label: "Agents that build software", description: "Avoid the traps agents fall into building and testing code on small always-on machines." },
  { file: "guides/continuous-deployment", group: "Building software", label: "Continuous deployment", description: "Deploy many merges a day safely: one deploy at a time, images by digest, and a pipeline agents can trust." },
  { file: "guides/make-it-yours", group: "Building software", label: "Make it yours", description: "Change Fleetlayer, run your own build on your machines, and contribute it back if you like." },
  { file: "directory", group: "Plugins", label: "Plugin directory", generated: true },
  { file: "plugins/rex", group: "Plugins", label: "Rex", description: "Set up and configure the Rex session manager plugin." },
  { file: "plugins/tmux", group: "Plugins", label: "tmux", description: "Put the Claude Code and Codex sessions you run in tmux panes in your fleet: their status, messages and webhooks, prompts answered from anywhere, sessions started and closed remotely, and fleetlayer attach, without changing your layout." },
  { file: "plugins/summaries", group: "Plugins", label: "Summaries", description: "Get a one-liner, a paragraph and a page summarising each hour of every agent session, without reading transcripts." },
  { file: "plugins/pages", group: "Plugins", label: "Pages", description: "Set up and run the pages service: Markdown and images from plain folders, live updates and sign-in." },
  { file: "plugins/apps", group: "Plugins", label: "Apps", description: "Host small interactive apps your agents build, each on a subdomain of its own, behind the same sign-in as pages." },
  { file: "plugins/claude-auth-manager", group: "Plugins", label: "Claude accounts", description: "How the Claude accounts plugin works: the hub that holds your sign-ins, setup tokens delivered sealed, usage readings and the rotator." },
  { file: "plugins/signin", group: "Plugins", label: "Sign-in", description: "Set up and configure the sign-in service: clients, Google Workspace and other providers, groups as teams, forward-auth, signing out and keys." },
  { file: "plugins/shared-folders", group: "Plugins", label: "Folders", description: "Configure the shared folders tool: which folders, read or write, and what macOS asks for." },
  { file: "plugins/scheduler", group: "Plugins", label: "Scheduler", description: "Send a message to an agent session on a schedule: every few hours, on a cron expression, or once at a set time." },
  { file: "plugins/matrix", group: "Plugins", label: "Matrix rooms", description: "Turn Matrix rooms (an alerts room, a team room) into channels agents hear, and let them answer as the room's bot." },
  { file: "plugins/leases", group: "Plugins", label: "Leases", description: "Take turns on shared machines and claim things for a while (a build slot, an approved mail filter), with leases that end when their holder dies." },
  { file: "plugins/herdr-fleet", group: "Plugins", label: "Every herdr as one", description: "Point any herdr client (the phone relay, a dashboard) at every herdr in your fleet as if it were one." },
  { file: "plugins/cookies", group: "Plugins", label: "Cookies", description: "Give command-line tools on other machines your browser's cookies for chosen sites, sealed in transit, and know what that exposes." },
  { file: "plugins/1password", group: "Plugins", label: "1Password", description: "Deliver 1Password items to the roles you grant them to, from a service-account token no agent holds." },
  { file: "openclaw", group: "Comparisons",
    label: "OpenClaw", description: "Compare Fleetlayer with OpenClaw feature by feature, and build your own OpenClaw from Fleetlayer's pieces with Claude Code or Codex." },
  { file: "credentials", group: "Reference", label: "Credentials", description: "How Fleetlayer keeps secrets: sealed delivery, per-role grants, using a secret without seeing it, scrubbing, and what none of it covers." },
  { file: "egress", group: "Reference", label: "Network access", description: "Reference for network access per role: the proxy, DNS, modes, presets, approvals and the commands." },
  { file: "access-control", group: "Reference", label: "Access control", description: "Write the access policy: who may message whom, use which tools and receive which secrets, what needs your approval, and how to test and explain each decision." },
  { file: "events", group: "Reference", label: "Events", description: "Reference for events and channels: CI results, alerts, session status and the rest, how sessions subscribe and catch up." },
  { file: "changelog", group: "Reference", label: "Changelog", description: "What changed in each release, newest first, and the steps to take when you upgrade from an older one." },
  { file: "releasing", group: "Reference", label: "Releases and updates", description: "How releases and self-updates work, how to check a release, and how to sign your own." },
  { file: "protocol", group: "Reference", label: "Plugin protocol", description: "Write a plugin: the JSON-RPC protocol between a node and its plugins, roles, sealed bundles, readings and status." },
  { file: "link", group: "Reference", label: "Node link", description: "Reference for the node's link to the control plane: the WebSocket, its messages and how a moved control plane is followed." },
];
