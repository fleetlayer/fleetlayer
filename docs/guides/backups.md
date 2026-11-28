# Backups for a fleet of agents

Your agents create things you'd hate to lose: notes, pages, sign-ins, databases, the history of what they did. This
guide is how to back them up so a lost disk, a deleted folder or a rebuilt machine costs you an afternoon, not your
work. The example throughout is a real setup: one home server running Proxmox, backing up to Backblaze B2 with restic.

## The shape

- **One host does the uploading.** Other machines and containers put what needs keeping where the backup host can
  read it (a staging folder it shares over NFS, or files copied there on a schedule); the backup host runs restic
  against those folders and a few chosen datasets. Credentials for the offsite store live on one machine.
- **restic, to an object store.** restic encrypts before upload, deduplicates, and keeps snapshots you can browse and
  restore by date. Backblaze B2 is cheap and S3-compatible; any S3 store works the same way.
- **One restic repository per source** (`<bucket>/<host>/notes`, `<bucket>/<host>/git`, ...). A problem in one (a
  stale lock, a corrupt pack) never blocks the others, and you can restore one source without touching the rest.
- **Separate accounts for separate owners.** If a company's data and a family's data are on the same server, give each
  its own B2 account, bucket, keys and restic password. One host may run both jobs, but what each can read, decrypt
  and delete stays separate, and either side can be handed over or closed on its own.

## The example setup

| Source | How it reaches the backup host | Schedule |
|---|---|---|
| A small device's config (a Raspberry Pi) | rsync to an NFS-mounted staging folder | every 2 hours |
| A blog's database | dumped and rsynced to staging | every 2 hours, offset 30 minutes |
| Services and chosen datasets (notes, git server, agent state, archives) | restic on the host, one repository each, personal B2 account | hourly, with up to 30 minutes of jitter |
| The company's archive | restic on the host, company B2 account and bucket | hourly, offset, with jitter |
| Service databases in the cluster | their own backup jobs in Kubernetes | per service |

- **Retention:** 7 daily, 4 weekly and 12 monthly snapshots, applied in each repository.
- **Credentials:** in the secrets manager (1Password here): the B2 key, the bucket and the restic password per account.
  The company's bucket has a runtime key limited to that bucket, separate from the key that provisions it.
- **The buckets themselves are code** (Terraform: buckets, lifecycle rules, application keys), with the company bucket
  protected against accidental destroy.
- **A service with its own encryption** (an agent gateway that stores sign-ins encrypted, for example) is backed up
  whole, stopped for a moment so its database is consistent, with its encryption key kept in the secrets manager and
  never in the backup.

## Fleetlayer's own state

Fleetlayer keeps each piece's state in one folder and its settings in one file, so the list is short. Back up these,
with the default paths on the machine that runs each piece:

- **The control plane:** `~/.config/fleetlayer/control.yaml` and `~/.local/state/fleetlayer/control/` (the database
  `control.db`, the `admin-token` and the `audit-key`), and the `audit_anchor_dir` if control.yaml names one. The
  database is SQLite in WAL mode: stop the control plane for a moment, or copy it with
  `sqlite3 control.db ".backup <file>"`, so the copy is consistent.
- **Each node:** `~/.config/fleetlayer/node.yaml`, `node-key` and `node-token`. The key is the node's identity: secrets
  sealed to it can't be opened without it, and a new key means enrolling the node again.
- **Sign-in:** `~/.local/state/fleetlayer/signin/` (passkeys, the token-signing key, sessions) and its token,
  `~/.config/fleetlayer/signin-token`. Without the folder everyone registers their passkeys again.
- **Pages:** the space folders (`~/Fleetlayer/personal/` on a laptop) and the login key,
  `~/.local/state/fleetlayer/pages/login`.
- **The gateway:** `~/.config/fleetlayer/gateway.yaml`, and the heartbeat token files it names.
- **Your policy files**, if they live outside a git repository.

The node key, tokens and signing keys are secrets: they belong only in an encrypted backup (restic encrypts), never in
a plain copy on a share. The Claude sign-in hub's folder is a choice (see below). [Moving a piece](moving.md#the-pieces)
lists the same state per piece.

## What not to back up

Leave out what's rebuilt from code or downloaded again, and say so in writing so nobody wonders later:

- the Kubernetes cluster itself (Argo CD rebuilds it from git) and anything configured by Ansible;
- media you can download again;
- search indexes and embeddings that can be rebuilt from their sources;
- build output and dependencies: `node_modules`, `.next`, `.turbo`, `.cache`, `target`, `.venv`, `__pycache__`. Exclude
  them from every synced share and every backup, not only one;
- some state is safer left out on purpose: the Claude sign-in hub's sign-ins cost ten minutes to redo and a copy in a
  backup is one more place they could leak. Write that decision down next to the service.

## Watch it

- **A heartbeat per job.** Each restic job pushes a heartbeat to the uptime checker when it finishes (Gatus external
  endpoints here), with an alert window a little over a day. A job that stops running, or runs and fails, alerts.
- **Alerts go where an agent listens**, and that agent investigates (a full disk, an expired key, a stale lock) before
  asking you.
- **Watch the inputs too:** a staging job that silently stops leaves the backup succeeding with yesterday's data. Give
  staging jobs their own heartbeats.

## Prove it

A backup you haven't restored isn't a backup.

- **A scheduled restore check:** `restic check` with a sample of the data packs read back (`--read-data-subset=1/20`),
  then restore one known file into a private temporary folder, compare its SHA-256 with the live copy, and delete the
  restore. It never restores over live data. Fail loudly on a missing snapshot or a mismatch.
- **Restore by hand once**, end to end, and time it, so you know what a real recovery costs.

## When the server is gone

1. Install Proxmox on new hardware and recreate the storage pools.
2. Install restic, read the credentials from the secrets manager, list snapshots, and restore one repository at a time
   into a scratch folder.
3. Check out the infrastructure repository and run the full setup; services come back from code and pick up their
   restored data.

The secrets manager is what makes this possible: without the restic password a repository can't be read. Keep that
password's history; never replace a missing one for an existing repository, recover it.

## Agents and backups

- **Every new service gets its backup decision in the same change that adds it:** backed up (where, how often, with
  which heartbeat) or left out on purpose (and why). The same rule as monitoring.
- **Agents never hold the backup credentials.** The backup host does; agents that need to restore something ask the
  agent that owns the server, or the person.
- **Keep runbooks with the code:** where each source lives, how to list snapshots and restore it, and the restore check,
  in the infrastructure repository, so an agent can follow them during an incident.
