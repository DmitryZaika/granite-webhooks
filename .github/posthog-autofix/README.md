# PostHog auto-fix

Every new, unique production error in PostHog becomes one fix attempt. Claude
Opus 5.5 reproduces it as a failing test, fixes it, and the workflow proves the
fix (red, then green) before it opens a pull request and emails the developers.
An error that needs no code change gets a short report instead.

```
time-triggered (every minute) ── POST /v1/jobs/posthog-autofix ──▶ api: intake
  watchdog · discover new issues · settle 30 min · skip noise · fold duplicates
  · dispatch within the caps (or, in dry run, email what it would dispatch)
                                     │ workflow_dispatch(run_id, issue_id)
                                     ▼
GitHub Actions: posthog-autofix.yml, three jobs
  prepare  trusted: warms and saves the Rust build cache, never runs agent code
  fix      both repos · MySQL 8 · isolate the agent (own user, egress firewall)
           · fetch the issue's summary · claude -p (fixer-prompt.md)
           · verify red/green + checks · bundle
  report   fresh runner: re-check the bundle · push · open the PR · report back
                                     │
api: POST /v1/posthog-autofix/runs/{id}/started and /result ◀─┘
  ledger updated · developers emailed
```

## Where things are

| Piece | Path |
|---|---|
| Ledger table | `migrations/20261022120000_create-posthog-autofix-runs.sql` |
| Intake, signatures, rules, emails | `api/crates/admin/src/posthog_autofix/` (start at `mod.rs`) |
| Intake job route | `api/src/routes/jobs/posthog_autofix.rs` |
| Callbacks | `api/crates/admin/src/routes/posthog_autofix.rs` |
| Workflow | `.github/workflows/posthog-autofix.yml` |
| Agent instructions, result schema, PostHog MCP | `fixer-prompt.md`, `result.schema.json`, `mcp.json` |
| Guardrails (Claude Code settings, hook) | `settings.json`, `guard.py` |
| Agent isolation (user, egress firewall) | `isolate.sh`, `autofix/isolation.py`, `autofix/probe.py` |
| Verification, PR, callbacks | `autofix/` (`python3 -m autofix <command>`), tests in `tests/` |

## How an issue is handled

1. **Discover.** Each poll (every 5 minutes) reads issues created since the
   newest one in the ledger, never before `POSTHOG_AUTOFIX_START_AT`. The
   backlog from before the cutover never starts a run. One PostHog issue gives
   one ledger row (`posthog_issue_id` is unique), so a reopened issue never
   starts a second run.
2. **Settle.** The issue waits 30 minutes, so its first burst of events and
   any duplicates arrive first.
3. **Judge.** The issue is skipped when it is:
   - resolved or suppressed;
   - without events;
   - from the removed Remix server;
   - from outside production;
   - matching the exclude list (network noise, browser extensions).

   Otherwise its **signature** is computed: exception type, normalized message,
   source and, once source maps work, the symbolicated top frame. An issue whose
   signature matches a run still in flight, or one finished in the last 14 days,
   becomes a `duplicate` of that run. One "Failed to fetch" was 110 PostHog
   issues: it has one signature.
4. **Dispatch.** Within the caps (2 at once, 3 in any 24 hours by default),
   ordered by users, then occurrences. Rows over a cap stay `queued` for later
   ticks, never dropped.
5. **Fix.** The workflow runs the agent with `fixer-prompt.md`. The agent
   either makes two commits in one repository (`test: reproduce …`, then
   `fix: …`), one commit (`fixed_without_test`, with manual test steps), or
   none (`not_a_code_bug`, `not_reproducible`, `duplicate`, `needs_human`).
6. **Verify.** The workflow, not the agent:
   - checks the commit contract;
   - runs the test on the first commit, where it must fail for the right
     reason, and on the second, where it must pass;
   - then runs the repository's checks.

   A failed check never gives a ready pull request: it gives a draft labelled
   `ai-fix:unverified`, with the evidence.
7. **Report.** The pull request is opened by the `granite-autofix` GitHub App.
   The api records the result and emails `POSTHOG_AUTOFIX_RECIPIENTS`. With
   write-back configured, the next poll assigns the PostHog issue to the
   fix's reviewer.

Before the agent starts, `python3 -m autofix issue-context` queries PostHog's
HogQL API (same read-only key as the MCP) and puts the issue's summary and
latest stack in the agent's first message, fenced as untrusted. It is best
effort: on any failure the agent fetches the issue through the MCP tools.

Browser errors resolve to real files and functions once the SPA uploads its
source maps at deploy (`general_datebase`, `docs/static-hosting.md`). Until
then their frames are minified, and only the error text identifies the bug.

Ledger states: `discovered → queued | skipped | duplicate`, `queued →
dispatched | previewed (dry run) | failed`, `dispatched → running`, `running →
succeeded | failed | timed_out | queued (usage limit, retried in an hour)`.
The migration header has the full diagram.

## Configuration

### api Lambda (`granite-api`) settings

The Lambda's environment is at AWS's 4 KB limit, so it holds only
`POSTHOG_AUTOFIX_MODE` and `POSTHOG_AUTOFIX_START_AT`. Everything else goes,
as `NAME=value` lines, in one **SecureString** parameter in Systems Manager
Parameter Store, `/granite-manager/posthog-autofix` (us-east-2;
`POSTHOG_AUTOFIX_PARAMETER` overrides the name). The api reads it on the
first auto-fix request of each Lambda instance, never at start-up and never
with the mode `off` (3 s limit; a failed read is retried on the next request).
A variable set in the environment wins over the parameter. The Lambda's role needs:

```json
{
  "Version": "2012-10-17",
  "Statement": [{
    "Effect": "Allow",
    "Action": "ssm:GetParameter",
    "Resource": "arn:aws:ssm:us-east-2:741448943665:parameter/granite-manager/posthog-autofix"
  }]
}
```

A setting change takes effect on the next cold start (any save of the
Lambda's configuration forces one).

A tick stops starting PostHog or GitHub calls after 20 s, so it always answers
within the api's 30 s limit; the rest waits for the next tick. Its emails are
sent after it releases the database lock.

| Setting | Default | Meaning |
|---|---|---|
| `POSTHOG_AUTOFIX_MODE` | `off` | `off`, `dry_run` or `live`. The kill switch. |
| `POSTHOG_AUTOFIX_START_AT` | none | RFC 3339 cutover, e.g. `2026-10-22T00:00:00Z`. Required unless off. |
| `POSTHOG_AUTOFIX_POSTHOG_KEY` | none | PostHog personal API key, `query:read`, project 141685. Required unless off. |
| `POSTHOG_AUTOFIX_GITHUB_TOKEN` | none | Fine-grained token, `granite-webhooks` only, **Actions: read and write** only. Required for live. |
| `POSTHOG_AUTOFIX_CALLBACK_SECRET` | none | Shared with the workflow (`AUTOFIX_CALLBACK_SECRET`). Callbacks are refused without it. |
| `POSTHOG_AUTOFIX_RECIPIENTS` | none | Comma-separated developer emails. |
| `POSTHOG_AUTOFIX_MAX_CONCURRENT` | `2` | Runs in flight at once. |
| `POSTHOG_AUTOFIX_MAX_PER_DAY` | `3` | Runs dispatched in any rolling 24 hours. |
| `POSTHOG_AUTOFIX_SETTLE_MINUTES` | `30` | Wait before an issue is judged. |
| `POSTHOG_AUTOFIX_POLL_MINUTES` | `5` | PostHog is read at most this often. |
| `POSTHOG_AUTOFIX_SIGNATURE_WINDOW_DAYS` | `14` | How long a finished run absorbs duplicates. |
| `POSTHOG_AUTOFIX_RUN_TIMEOUT_MINUTES` | `120` | The watchdog gives up on a silent run after this. |
| `POSTHOG_AUTOFIX_MAX_ATTEMPTS` | `3` | Dispatch failures and usage-limit retries before a run fails. |
| `POSTHOG_AUTOFIX_STORM_THRESHOLD` | `20` | More new issues than this in one storm window send one digest. |
| `POSTHOG_AUTOFIX_STORM_WINDOW_MINUTES` | `30` | The storm window. |
| `POSTHOG_AUTOFIX_EXCLUDE` | see `config.rs` `DEFAULT_EXCLUDE` | Newline- or `;`-separated, case-insensitive substrings of `<type>: <message>`. Replaces the defaults. |
| `POSTHOG_AUTOFIX_WRITE_KEY` | none | Optional write-back: PostHog personal API key with `error_tracking:write`, project 141685. |
| `POSTHOG_AUTOFIX_ASSIGNEE` | none | Optional write-back: comma-separated `user:<PostHog user id>` or `role:<role uuid>`, e.g. `user:378526,user:123`. When a fix's pull request opens, its issue is assigned to one of them (PostHog allows one assignee per issue; they take turns by run), retried on the next poll if PostHog fails. Both write-back variables must be set. |
| `POSTHOG_AUTOFIX_GITHUB_REPO` / `_WORKFLOW` / `_REF` | `DmitryZaika/granite-webhooks` / `posthog-autofix.yml` / `main` | Where the workflow lives. |

An unparsable value keeps its default and is logged; secrets never appear in logs.

### GitHub (`granite-webhooks`, Environment `posthog-autofix`)

| Name | Kind | Meaning |
|---|---|---|
| `AUTOFIX_APP_ID` | variable | The `granite-autofix` GitHub App. |
| `AUTOFIX_APP_PRIVATE_KEY` | secret | Its private key (.pem contents). |
| `CLAUDE_CODE_OAUTH_TOKEN` | secret | From `claude setup-token` on the Claude Max account. `ANTHROPIC_API_KEY` works instead. |
| `POSTHOG_AUTOFIX_MCP_KEY` | secret | PostHog personal API key, read-only (`error_tracking:read`, `query:read`), project 141685. |
| `AUTOFIX_CALLBACK_SECRET` | secret | Same value as the api's `POSTHOG_AUTOFIX_CALLBACK_SECRET`. |
| `AUTOFIX_API_URL` | variable | The production api base URL. |
| `AUTOFIX_EFFORT` / `AUTOFIX_MAX_TURNS` / `AUTOFIX_BUDGET_USD` | variables, optional | Defaults `medium` / `150` / `10`. Raise the effort to `high` only when measured runs show it helps. |

The App needs Contents, Pull requests and Issues (read and write) and Metadata
(read) on both repositories, and **no** Workflows permission.

## Rollout

1. Apply the migration (`sqlx migrate run`) to the production database.
2. Deploy `api` and `time-triggered`.
3. Set the api env with `POSTHOG_AUTOFIX_MODE=dry_run` and `START_AT` = now.
4. **Dry run for 2–3 days.** Nothing is dispatched. Every poll that finds
   eligible issues sends one "Dry run: N issue(s) would be fixed" email. Check
   that duplicates fold, noise is skipped, and the volume matches the caps.
5. Do one manual workflow run (below) to prove the GitHub side.
6. Switch to `POSTHOG_AUTOFIX_MODE=live` with the default caps (2 at once, 3
   a day). Raise them only after a week of reviewed results.

Switching modes keeps the watermark, so no backlog floods in. Issues
previewed during the dry run stay `previewed` and are never dispatched; set
one back to `queued` to fix it (below).

## Kill switch

- **Stop new runs at once:** set `POSTHOG_AUTOFIX_MODE=off` on the api Lambda.
  The next tick does nothing.
- **Stop runs in progress:** cancel them in GitHub Actions. The failure step
  reports them as failed. Or disable the workflow (Actions, then PostHog
  auto-fix, then Disable workflow).

## Run one issue by hand

Re-run an issue that already has a ledger row; the next tick dispatches it,
within the caps:

```sql
UPDATE posthog_autofix_runs
   SET status = 'queued', retry_at = NULL, attempts = 0
 WHERE posthog_issue_id = '<issue uuid>';
```

First end-to-end check of the workflow, without waiting for the intake. Insert
a row the callbacks will accept, then start the workflow with its id:

```sql
INSERT INTO posthog_autofix_runs
  (posthog_issue_id, issue_created_at, issue_name, issue_description, status, dispatched_at, attempts)
VALUES ('<issue uuid>', UTC_TIMESTAMP(3), '<type>', '<message>', 'dispatched', UTC_TIMESTAMP(3), 1);
SELECT LAST_INSERT_ID();
```

Then go to Actions, then PostHog auto-fix, then Run workflow, with `run_id` =
that id and `issue_id` = the uuid. The `started` callback refuses any run the
ledger did not dispatch, so a typo stops the run before the agent starts.

## Reading the ledger

```sql
-- What happened recently
SELECT id, status, outcome, verification, skip_reason, issue_name, pr_url, cost_usd, discovered_at
  FROM posthog_autofix_runs ORDER BY id DESC LIMIT 50;

-- Volume by result over the last week
SELECT status, outcome, skip_reason, COUNT(*) FROM posthog_autofix_runs
 WHERE discovered_at >= UTC_TIMESTAMP() - INTERVAL 7 DAY
 GROUP BY status, outcome, skip_reason;

-- Duplicates folded into one run
SELECT duplicate_of_run_id, COUNT(*) FROM posthog_autofix_runs
 WHERE status = 'duplicate' GROUP BY duplicate_of_run_id ORDER BY 2 DESC;

-- Spend
SELECT DATE(finished_at), COUNT(*), SUM(cost_usd) FROM posthog_autofix_runs
 WHERE finished_at IS NOT NULL GROUP BY 1 ORDER BY 1 DESC;
```

`result` holds the full callback body (the agent's analysis, the verification
notes) for any run.

## Safety model

The agent reads production error text, which anyone can shape, so the design
assumes it may be talked into running hostile code and limits what that code
could reach.

- **No production access on the runner.** There are no AWS credentials and no
  production database URL: the runner's MySQL is a throwaway container.
- **A dispatch-only token on the api.** The internet-facing api holds a token
  that can only start this workflow. The App's key stays in GitHub.
- **The agent is isolated.** It runs as its own Linux user, with no sudo and
  no Docker, and so do its tests, the verification checks and every git
  command on its repositories. That user's network reaches only the Anthropic
  API and the pinned PostHog MCP and Claude sign-in hosts; DNS included,
  everything else is rejected. The job checks this before the agent starts
  and fails if it does not hold.
- **The fix job holds no write access.** The checkouts keep no credentials.
  The only write token is minted in a separate job, on a fresh runner that
  never executes agent code and re-checks the commits it receives.
- **Guardrails inside Claude Code.**
  - `settings.json` and `guard.py` refuse pushes, `gh`, network tools,
    deploys, migrations, other databases, secrets, and edits to `.github/`,
    `migrations/`, lockfiles and manifests.
  - The hook fails closed (`onFailure: block`), and `--restricted` ignores any
    settings found in the checkouts.
  - `CLAUDE_CODE_SUBPROCESS_ENV_SCRUB=1` keeps the model token out of the
    environment of the commands the agent runs. On Linux the CLI runs each
    command in bubblewrap for it (user and PID namespaces, the filesystem and
    network as they are); `isolate.sh` installs it and lifts Ubuntu's AppArmor
    block on user namespaces, and `check-isolation` starts it as the agent
    user, so a missing sandbox fails the job before the agent runs. Recheck
    the probe's flags when the pinned CLI version changes.
  - The account's claude.ai connectors are switched off
    (`ENABLE_CLAUDEAI_MCP_SERVERS=false`).
- **The build cache stays clean.** Only the trusted `prepare` job saves it;
  the fix job restores it read-only.
- **Residual risk.** Code the agent writes runs as the same user as Claude
  Code, so in principle it could read the model token. The firewall means it
  could only send it to the Anthropic API or PostHog. Use a dedicated,
  revocable token, and rotate it if a run ever looks wrong.
- **Error text is data.**
  - The dispatch carries only two ids.
  - The prompt tells the agent that error text is untrusted.
  - Everything the agent writes is escaped in the PR body and the emails.
- **Every claim is checked.** The workflow re-runs the agent's test command
  itself, matched against a strict allowlist, and never executes the agent's
  free text.
- **Every fix gets a human review.** `main` requires a review, and the App is
  not on any bypass list.
