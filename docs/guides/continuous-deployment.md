# Continuous deployment that agents can trust

Once agents merge to main many times a day, "deploy when CI passes" starts to go wrong in ways a person merging twice
a week never sees. Two runs finish out of order and the older image goes out last. Someone re-runs an old job and
production goes backwards. A migration drops a column while the old pods, still serving during the rolling update,
read it. None of these are rare at that pace, and nobody is watching each deploy to catch them.

This guide sets up a pipeline where every merge reaches production on its own, and where those races can't happen.
It assumes an app repo (`my-app`) built by GitHub Actions, a container registry (`registry.example.com`), and a
separate infrastructure repo (`infra`) that a GitOps tool such as Argo CD or Flux applies to a Kubernetes cluster. The
examples use Argo CD; any tool that syncs a cluster from git works the same way.

The shape:

- **CI builds the image and commits its digest to the infra repo.** The infra repo stays the one record of what runs
  in production, and a deploy is a commit you can read, review and revert.
- **Production only moves forward.** Before each push, CI checks that the commit it's deploying comes after the one in
  production. An older run finishing late, or a re-run, is skipped.
- **Main's runs queue instead of cancelling.** A deploy is never cut off halfway.
- **Migrations may only add during a rolling deploy.** Removing comes later, in its own deploy, once nothing uses it.
- **The GitOps tool backs up before and checks after**, and a failed check alerts.

Fleetlayer doesn't ship any of this. It's a pattern for the repos your agents work in, and the steps below are what
you ask your agent to set up. The steps marked **you** are admin settings a person does.

## 1. Build the image and pin it by digest

The build job tags the image with the commit and pushes it on main. Pull requests only check that it builds. It reads
back the digest the registry assigned, because a tag can be pushed again and a digest can't:

```yaml
  image:
    runs-on: ubuntu-24.04
    outputs:
      digest: ${{ steps.push.outputs.digest }}
    env:
      IMAGE: registry.example.com/my-app
    steps:
      - uses: actions/checkout@v4
      - run: docker build -t "$IMAGE:${GITHUB_SHA:0:7}" .
      - name: Push
        id: push
        if: github.ref == 'refs/heads/main' && github.event_name == 'push'
        run: |
          short=${GITHUB_SHA:0:7}
          docker push "$IMAGE:$short"
          digest=$(docker inspect --format '{{index .RepoDigests 0}}' "$IMAGE:$short" | cut -d@ -f2)
          [[ "$digest" == sha256:* ]] || { echo "No digest for $IMAGE:$short" >&2; exit 1; }
          echo "digest=$digest" >>"$GITHUB_OUTPUT"
```

(Log in to your registry before the push.) It's worth having the app report the commit it was built from (a build
argument baked into the binary, shown on an info endpoint) and having CI start the image and check that it says the
right commit. That catches a build that picked up a stale layer, and later tells you at a glance what's running.

## 2. Mark the deployed commit in the infra repo

In the infra repo, the app's values file names the image by tag and digest, with a comment that records the full
commit it was built from:

```yaml
image:
  repository: registry.example.com/my-app
  # my-app commit: 3f9c2a1e8b7d6c5f4a3b2c1d0e9f8a7b6c5d4e3f
  tag: '3f9c2a1@sha256:9b1d...'
```

The `tag@digest` form pulls by digest, so the image can't change under the same name, and the short tag stays readable.
The comment line is how CI knows what production runs: it reads it before deploying, and rewrites it together with the
tag line after.

## 3. Give CI a deploy key for the infra repo

CI needs to push to one repo, so give it exactly that: an SSH deploy key with write access on `infra` alone.

1. Your agent generates the key pair (`ssh-keygen -t ed25519 -N '' -f infra-deploy`).
2. **You** add the public key to `infra` as a deploy key with write access (the repo's Settings › Deploy keys).
3. **You** add the private key to `my-app` as an Actions secret, `INFRA_DEPLOY_KEY`.

The same key can sign the deploy commits, so they show as verified if `infra` requires signed commits. Here the job
has written the `INFRA_DEPLOY_KEY` secret to `~/.ssh/infra-deploy`:

```sh
git -C infra config gpg.format ssh
git -C infra config user.signingkey "$HOME/.ssh/infra-deploy"
git -C infra config commit.gpgsign true
```

Pin GitHub's host key in `~/.ssh/known_hosts` from GitHub's published fingerprints, rather than trusting whatever
answers on first connect.

Have the deploy job check that the secret exists and only warn when it doesn't, so a fork or a fresh clone of the repo
runs CI without failing:

```yaml
    env:
      SET_UP: ${{ secrets.INFRA_DEPLOY_KEY != '' }}
    steps:
      - name: Continuous deployment is set up
        if: env.SET_UP != 'true'
        run: 'echo "::warning::INFRA_DEPLOY_KEY is not set, so nothing was deployed"'
```

## 4. Queue main's runs

By default, GitHub Actions runs every push at once. Put the whole workflow in one concurrency group per branch, and
cancel older runs only on pull requests:

```yaml
concurrency:
  group: ci-${{ github.ref }}
  cancel-in-progress: ${{ github.ref != 'refs/heads/main' }}
```

On a pull request, a new push cancels the run it replaces, which is what you want. On main, one run goes at a time
and the newest push waits. GitHub keeps only one waiting run per group: when a third push arrives, the waiting one is
cancelled and the newest takes its place. That's fine, because the newest commit contains everything before it, and
it's why the next step checks ancestry rather than counting on every run to deploy.

Queueing alone doesn't make deploys safe. A re-run of an old job, a second workflow that deploys, or a person running
the job by hand all slip past it. Step 5 covers those.

## 5. Only move production forward

The deploy job runs after the tests and the image job, on main only. Check out the app repo with its full history
(`fetch-depth: 0`), because the check needs to see both commits. Then compare the commit being deployed with the one in
the infra repo's comment line:

```sh
deployed=$(sed -n 's/^ *# my-app commit: \([0-9a-f]\{40\}\)$/\1/p' "$VALUES")
[ -n "$deployed" ] || { echo "$VALUES has no '# my-app commit:' line" >&2; exit 1; }
git cat-file -e "$deployed^{commit}" || { echo "Production's commit $deployed isn't in this repository" >&2; exit 1; }
if [ "$deployed" = "$GITHUB_SHA" ] || ! git merge-base --is-ancestor "$deployed" "$GITHUB_SHA"; then
  echo "::notice::Not deployed: production runs $deployed, which this commit doesn't come after"
  exit 0
fi
```

`git merge-base --is-ancestor A B` exits 0 when `A` is an ancestor of `B`. So the job deploys only a commit that comes
after production's. An older run finishing late sees a newer commit in production and stops with a notice, without
failing.

Then rewrite the two lines and push, and check again on every attempt. Another run may have pushed a newer deploy
between your clone and your push:

```sh
for _ in 1 2 3 4 5; do
  deployed=$(sed -n 's/^ *# my-app commit: \([0-9a-f]\{40\}\)$/\1/p' "$VALUES")
  if [ "$deployed" = "$GITHUB_SHA" ] || ! git merge-base --is-ancestor "$deployed" "$GITHUB_SHA"; then
    echo "::notice::Not deployed: production moved to $deployed meanwhile"
    exit 0
  fi
  # rewrite the commit line and the tag line, then check both say what you expect
  git -C infra commit -q -am "my-app $short to production: $subject"
  if git -C infra push -q origin HEAD:main; then exit 0; fi
  git -C infra fetch -q origin main
  git -C infra reset -q --hard origin/main
done
echo "Couldn't push to infra after 5 attempts" >&2
exit 1
```

A rejected push means someone else pushed to `infra` first, maybe a deploy, maybe an unrelated change. The job takes
their version, checks ancestry again and rewrites its two lines on top. Put the app commit, the digest and a link to the
run in the commit message body, so the infra repo's log says where each deploy came from.

After rewriting, have the job check that both lines now hold the new values (`grep` for each). If someone reformats the
values file, the `sed` matches nothing, and you want a failed deploy rather than a commit that changes nothing.

## 6. Migrations may only add

During a rolling update, Kubernetes starts the new pods before it stops the old ones. The new pods run the migration,
and for a while the old pods keep serving on the migrated database. If the migration dropped or renamed a column they
read, they fail until they're gone.

So split every removal into two deploys (often called expand and contract):

1. **Expand.** Add the new column or table, and ship code that writes to both and reads the new one. The old pods don't
   know about the addition and keep working.
2. **Contract.** In a later deploy, once no running code uses the old column, drop it.

Have the deploy job enforce the first half. Look at the migration files that changed since production's commit, and
refuse any that drop or rename, unless a commit message in that range says the removal is safe:

```sh
destructive=$(git diff --name-only --diff-filter=AM -z "$deployed" "$GITHUB_SHA" -- db/migrate |
  xargs -0 -r grep -l -i -E '\b(DROP|RENAME)\b' || true)
if [ -n "$destructive" ] &&
  ! git log --format=%B "$deployed..$GITHUB_SHA" | grep -q '\[allow-destructive-migration\]'; then
  echo "$destructive"
  echo "::error::A migration drops or renames something the running app may still use: split it"
  exit 1
fi
```

Change `db/migrate` to wherever your migrations live. A word match is crude: it can't tell a dropped index from a
dropped column. That's the point of the override. The contract deploy carries `[allow-destructive-migration]` in its
commit message, and an agent writing it has to say, in that message, that nothing running uses what it removes. Say in
the app repo's agent instructions (`AGENTS.md`) that migrations only add, and when the override is allowed.

## 7. Back up before, check after

When the infra commit lands, the GitOps tool syncs it. Use its hooks so every sync is bracketed by a backup and a check.
In Argo CD these are Jobs with hook annotations:

```yaml
apiVersion: batch/v1
kind: Job
metadata:
  name: my-app-presync-backup
  annotations:
    argocd.argoproj.io/hook: PreSync
    argocd.argoproj.io/hook-delete-policy: BeforeHookCreation
spec:
  backoffLimit: 0
  activeDeadlineSeconds: 600
  ttlSecondsAfterFinished: 86400
  template:
    spec:
      restartPolicy: Never
      containers:
        - name: backup
          image: registry.example.com/my-app-backup
          command: ["/bin/sh", "-c", "sh /scripts/backup.sh"]
```

- **PreSync** runs before anything is applied. Back up the database there. If the backup fails, the sync fails and
  nothing is applied, so the old pods keep serving.
- **PostSync** runs after the rollout is healthy. Check the app from inside the cluster, through its Service, the way
  real traffic reaches it: poll its health endpoint for a minute or two and fail the Job if it never answers.

`BeforeHookCreation` replaces the previous run's Job, and `ttlSecondsAfterFinished` keeps a failed one around for a day
so you can read its logs. `backoffLimit: 0` makes a failure final: Argo CD doesn't retry a failed hook, so the sync
stays failed until someone fixes it and syncs again.

Make both Jobs report their result to your monitoring (a heartbeat endpoint, or an alert from Argo CD's
notifications), and route that alert to an agent that can act on it. A deploy pipeline that runs without anyone
watching needs to say when it stops working. [Monitoring](monitoring.md) covers how.

## Roll back

Revert the deploy commit in the infra repo. The GitOps tool syncs the previous image, with a backup first.

The ancestry check still works after a revert. The comment line now names an older commit, and the next merge to main
comes after it, so the next deploy goes ahead and moves production forward past the bad commit. If the bad commit is
still on main, revert or fix it in the app repo first, or the next deploy brings it back.

A revert puts back the old code, not the old database. That's another reason migrations only add: the old code keeps
working on a database that has the new column.

## Why not an image updater

Argo CD Image Updater and Flux's image automation watch the registry and write the newest image into git themselves.
They save you the deploy job, and for images tagged with release versions cut in order, they work well.

For deploying every merge they get the order wrong. "Newest" to the registry means pushed last or built last, and build
order isn't commit order: a run that started later can finish first, and a re-run of an old commit builds a new image.
The updater can't ask git whether one commit comes after another. The deploy job can, because it runs in the app repo
with its history, and it's also the place to check migrations before anything ships.

## Other ways to do it

- **CI applies to the cluster directly** (`kubectl` or `helm upgrade` from the workflow). Fewer moving parts, but the
  cluster's credentials live in CI, and what runs is recorded only in CI's logs. The ancestry check still applies:
  read the deployed commit from a label or annotation on the Deployment instead of a values file.
- **A deploy service with its own queue** (a platform's deploy pipeline, or a tool such as Kargo that promotes images
  between stages). These handle ordering and promotion for you and are worth it with several environments. Check how
  they decide which image is newest before trusting them with every merge.
- **Release trains.** Deploy on a schedule or by tag instead of on every merge. Slower, and simpler to reason about when
  agents merge faster than you can review deploys.

## When it doesn't work

- **"Production's commit isn't in this repository".** The checkout is shallow. Set `fetch-depth: 0` on the deploy
  job's checkout. The infra repo can stay a shallow clone: the ancestry check runs in the app repo.
- **A deploy was skipped with "doesn't come after".** That's the check doing its job: a newer commit is already in
  production, or this run is a re-run. If you meant to deploy an older commit, revert in the infra repo instead.
- **Some merges never get a deploy run.** Their waiting runs were replaced by newer ones (step 4). The newest commit
  still deploys and contains the others. Look at the run that went through, not the cancelled ones.
- **The job says the values file doesn't have the lines it rewrites.** Someone reformatted the values file, or renamed
  the comment. Restore the `# my-app commit: <sha>` line exactly as the job's `sed` expects: on a line of its own,
  with the full 40-character commit.
- **A merged pull request's migration was refused.** Split it: add in this deploy, remove in a later one with
  `[allow-destructive-migration]` in its commit message. Until then, nothing deploys, because every later run sees the
  same migration in its range. Revert the migration on main if the fix will take a while.
- **The sync failed at PreSync.** Nothing was applied, and the old version is still serving. Read the backup Job's
  logs, fix it, and sync again.
- **The sync failed at PostSync.** The new version is running and not answering. Roll back (revert the deploy commit)
  first, then investigate.
- **Two workflows deploy the same app.** Each queues only against itself, so they can push at the same time. The
  ancestry check and the retry loop still keep production moving forward. Simpler still: deploy from one workflow.
