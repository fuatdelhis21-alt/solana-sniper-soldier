<!-- ABACUS_CLOUD_INSTRUCTIONS_BEGIN -->
<!-- This block is auto-managed. Edits inside the markers will be overwritten on the next sync or restart. Add your own instructions ABOVE or BELOW this block to keep them. -->

# Abacus.AI Supercomputer Virtual Machine

## Environment

You are an agent running inside a virtual machine (VM) hosted by Abacus.AI. Users access it as part of their Abacus.AI subscription, via a web interface or SSH.
The OS is Ubuntu with X Window System support; common software and utilities are pre-installed, and you have internet access and a Chrome browser.
You also have external resources — LLM, web search, and crawl APIs, a database server, and a cloud storage bucket (described below) — with credentials provided.

## Managing this VM (SuperComputer Homepage)

This VM's views, services, and settings live on the **SuperComputer Homepage** (https://apps.abacus.ai/chatllm/abacus-cloud/) — a UI you cannot operate yourself. Point the user to the relevant control when they need to watch the VM, or to enable/configure something:

- **Views** (buttons at the top of the right-hand panel) — surfaces where the user watches and inspects this VM:
  - **Computer** — the live VM desktop (mainly Chrome), shown over VNC.
  - **Terminal** — open and manage terminal sessions, including long-lived tmux sessions that persist across visits.
  - **Files** — browse and edit this VM's files.
  - Once OpenClaw/Hermes are configured, their dashboards open from this toolbar too.
- **Service tiles** (home screen icon row) — data & integrations:
  - **GitHub** — connect the user's GitHub account (sets `github_connected`).
  - **SSH** — add an SSH public key for direct `ssh` access to this VM.
  - **Database**, **Storage** — view/attach the hosted database and storage bucket.
  - **OpenClaw** — an open-source AI assistant, running always-on on this VM, that automates real tasks (email, calendar, files, scripts, workflows) through chat apps. Click the icon to configure it. Powered by the provided Kimi K2.6 model; usage is billed to credits.
  - **Hermes** — an open-source, self-improving agent (Nous Research) with persistent memory, reusable skills, scheduled jobs, and messaging/terminal access, running always-on on this VM. Click the icon to configure it. Powered by the provided Kimi K2.6 model; usage is billed to credits.
  - *If the OpenClaw/Hermes icons aren't visible, this VM is on an older image — tell the user to update it from the top-right of the SuperComputer Homepage.*
- **Settings** (gear icon, opens the Cloud Settings panel) — serving & lifecycle:
  - **Public URL** toggle — gates `public_url_enabled` (HTTP ingress).
  - **Manage hostnames** — add/remove the public hostnames routed to this VM.
  - **Shutdown Policy**, **Restart Computer**, **Update Computer**.

## Filesystem

The VM has a persistent filesystem that survives restarts. Since users keep many projects here, start each new project in its own well-named directory (create one if needed).

Don't `rm` system files (`/opt`, `/usr`, …) to free disk space — it breaks the VM. On "No space left on device", report it to the user instead.

## Credentials (IMDSv2)

External-service credentials come from the metadata service at `169.254.169.254`.
Always fetch fresh user-data — cached values may be stale.

```bash
TOKEN=$(curl -s -X PUT "http://169.254.169.254/latest/api/token" \
  -H "X-abacus-vm-metadata-token-ttl-seconds: 21600")
curl -s -H "X-abacus-vm-metadata-token: $TOKEN" \
  http://169.254.169.254/latest/user-data | python3 -m json.tool
```

### user-data fields

| Field | Description |
|-------|-------------|
| `abacus_api_key` | Abacus AI API key for LLM and search service API calls |
| `llm_base_url` | OpenAI-compatible LLM endpoint |
| `brave_search_api_url` | Web search API endpoint |
| `firecrawl_api_url` | Web scraping API endpoint |
| `api_base_url` | Base URL for Abacus API calls from this VM (used for the hostname-management APIs below) |
| `databases` | Array of attached PostgreSQL databases (see below) |
| `storage` | S3-compatible cloud storage info — bucket name and path (see below) |
| `public_url_enabled` | Whether this VM has public URL access enabled |
| `http_ingress_settings` | Single in-VM ingress port and the list of public hostnames routed to it (see below) |
| `personal_agent_computer_id` | This VM's hashed id, used to call public hostname-management APIs (see below) |
| `github_connected` | Whether the user has connected their GitHub account |

## LLM and other APIs

An OpenAI-compatible LLM endpoint is available. Use `llm_base_url` and `abacus_api_key` from user-data.

Most frontier models are available, plus media and audio APIs. Popular models: gpt-5.5, claude-sonnet-4-6, claude-opus-4-7, gemini-3.5-flash, kimi-k2.6.
Full list at runtime: `curl -s "$llm_base_url/models" -H "Authorization: Bearer $ABACUS_API_KEY"`. Billing is through the Abacus.AI subscription.

```bash
curl -s "$llm_base_url/chat/completions" \
  -H "Authorization: Bearer $ABACUS_API_KEY" \
  -H "Content-Type: application/json" \
  -d '{"model":"gpt-5.5-mini","messages":[{"role":"user","content":"hello"}]}'
```

Brave web search and Firecrawl APIs are also available at their user-data base URLs, authenticated with `abacus_api_key`.

## Databases

Abacus.AI provides hosted PostgreSQL. A default database named `default` is attached to this VM — use it for any task needing a database, creating new tables per use case. Existing tables may belong to other projects, so check the schema first and don't modify tables you didn't create.
Other databases are accessible if the user attached them from the **Database** tile on the SuperComputer Homepage.

The `databases` field of user-data lists a descriptor for each accessible database:

```json
{
  "id": 12345,
  "name": "default",
  "database_url": "postgresql://role:pass@host:5432/dbname?connect_timeout=15",
  "host": "db-xxx.db005.hosteddb.reai.io",
  "port": 5432,
  "database_name": "dbname",
  "role_name": "role_xxx",
  "role_password": "password"
}
```

Connect directly: `psql "$database_url"` or use individual fields.

**Connection limits:** Max 25 concurrent connections. Queries over 5 seconds are killed; transactions idle over 30 seconds are killed. Reuse a single client or pool — don't open excessive connections.

**Schema updates:** Ensure all schema changes are backward compatible. Dropping or renaming columns/tables without defaults can cause data loss. Use `ALTER TABLE ... ADD COLUMN` with defaults for safe additions. Always verify existing data won't be affected before modifying the schema.

## Cloud Storage

The `storage` field in user-data has a bucket name reachable through an S3-compatible API, and a `path` to use by default for this VM's projects. Other parts of the bucket may belong to the user's other projects.

```json
{
  "bucket_name": "abacusai-apps-foo-us-west-2",
  "path": "personal_agent/123214/"
}
```

The pre-installed AWS CLI auto-discovers credentials from the metadata service:

```bash
aws s3 ls "s3://<bucket_name>/<path>"
aws s3 cp file.txt "s3://<bucket_name>/<path>file.txt"
```

## GitHub

Connecting GitHub from the **GitHub** tile on the SuperComputer Homepage sets `github_connected` to `true` in user-data. When `true`, use the snippet below with `ABACUS_API_KEY` to fetch a fresh access token (expires after 8 hours — re-run for a new one).
If `false`, the snippet won't work — have the user connect GitHub from the UI, or use `gh auth login` (or a user-provided token).

```bash
ACCESS_TOKEN=$(curl -s "https://api.abacus.ai/api/getUserConnectorAuth?service=GITHUBUSER" \
  -H "apiKey: $ABACUS_API_KEY" | python3 -c "import sys,json; print(json.load(sys.stdin)['result']['auth']['accessToken'])")

git config --global credential.helper store
echo "https://oauth2:${ACCESS_TOKEN}@github.com" > ~/.git-credentials
```

## SSH Access

This VM accepts a real `ssh` session from the user's own machine (separate from the web terminal), key-based as the `ubuntu` user. Keys are managed from the **SSH** tile on the SuperComputer Homepage — point the user there to add their public key. Do NOT hand-edit `~/.ssh/authorized_keys`: those keys are product-managed (deduped and re-applied on every restart/reset), so a manually appended key is lost on the next rebuild.

## HTTP Ingress

Check `public_url_enabled` in user-data to see if this VM can serve public traffic. If `false`, ask the user to enable the **Public URL** toggle in **Cloud Settings** before deploying any web-facing app.

When enabled, `http_ingress_settings` gives the in-VM ingress port and the public hostnames routed to this VM:

```json
{
  "public_url_enabled": true,
  "http_ingress_settings": {
    "port": 80,
    "hostnames": [
      "abc.abacusai.cloud",
      "myapp.abacusai.cloud"
    ]
  }
}
```

All hostnames resolve to this VM on the same `port`. The user manages the list from **Manage hostnames** in **Cloud Settings** — re-fetch `user-data` after changes.

### Deploying a web app — one app per hostname

Each hostname in `http_ingress_settings.hostnames` is an independent site. For every app you deploy:

- **One app → one hostname.** Never put two apps on the same hostname unless user requests.
- **Never write to `/var/www/html/`** — it's the shared default page (`READY`) served on every hostname without its own vhost, so deploying there exposes your app on all domains and collides with other apps.
- **Deploy via a per-hostname nginx vhost** at `/etc/nginx/conf.d/<subdomain>.conf` with `server_name <subdomain>.vm.internal`.

*Updating an app you already deployed? Reuse its hostname and existing `<subdomain>.conf` — rebuild or restart what's behind it; don't allocate a new hostname.*

**Step 0 — Pick a free port for your app process.** This VM is persistent — other sessions may already hold ports. Pick an unused port, **never kill a process you didn't start just to free its port**, and don't rely on a framework's default port without checking first.

**Step 1 — Pick a free hostname.** Read `http_ingress_settings.hostnames`, then check which subdomains already have an app (so you don't clobber another session's work):

```bash
ls /etc/nginx/conf.d/                                 # one <subdomain>.conf per claimed hostname
grep -rh server_name /etc/nginx/conf.d/ 2>/dev/null   # the subdomains already in use
```

A subdomain with no config in `/etc/nginx/conf.d/` is free (it still serves the default `READY` page) — pick one. Public hostname `myapp.abacusai.cloud` maps to nginx `server_name myapp.vm.internal`; name your config `/etc/nginx/conf.d/myapp.conf` so the next session can discover it.

**Step 2 — If every hostname is already claimed, or the user asks for a specific hostname not yet in the list, add a new one** with the `addPersonalAgentComputerHostname` API — see [Managing hostnames from inside the VM](#managing-hostnames-from-inside-the-vm) below — rather than overwriting an existing app. Wait for DNS to propagate, confirm the new hostname appears in `http_ingress_settings.hostnames`, then deploy to it.

**Step 3 — Deploy to that hostname.** Envoy forwards traffic as `Host: <subdomain>.vm.internal:80`, so nginx matches the vhost on that name; your app gets the original public hostname via the `X-Original-Host` header. nginx workers run as `ubuntu`, so they can serve files straight from your home dir — no need for a system web root. Two patterns:

**Pattern A — reverse-proxy to an app process** (any framework, dynamic or static):

Run your app under systemd so it auto-starts on VM boot and survives stop/start (bare `nohup &` does NOT).

```bash
sudo tee /etc/systemd/system/myapp.service > /dev/null <<'UNIT'
[Unit]
After=network-online.target
Wants=network-online.target

[Service]
User=ubuntu
WorkingDirectory=/home/ubuntu/myapp
ExecStart=/usr/bin/python3 -m http.server 3000
Restart=on-failure
RestartSec=3

[Install]
WantedBy=multi-user.target
UNIT
sudo systemctl daemon-reload && sudo systemctl enable --now myapp
# Logs: journalctl -u myapp -n 50 --no-pager
# Tear down: sudo systemctl disable --now myapp && sudo rm /etc/systemd/system/myapp.service && sudo systemctl daemon-reload && sudo rm /etc/nginx/conf.d/myapp.conf && sudo nginx -t && sudo systemctl reload nginx
```

```nginx
# /etc/nginx/conf.d/myapp.conf
# Public URL: https://myapp.abacusai.cloud  →  server_name myapp.vm.internal
server {
    listen 80;
    server_name myapp.vm.internal;
    location / {
        proxy_pass http://127.0.0.1:3000;
        proxy_set_header Host $http_x_original_host;
        proxy_http_version 1.1;
        proxy_set_header Upgrade $http_upgrade;
        proxy_set_header Connection "upgrade";
    }
}
```

**Pattern B — serve static files directly from a directory** (no app process needed):

```nginx
# /etc/nginx/conf.d/abc.conf — files live in your home dir
# Public URL: https://abc.abacusai.cloud  →  server_name abc.vm.internal
server {
    listen 80;
    server_name abc.vm.internal;
    root /home/ubuntu/abc;
    index index.html;
    location / { try_files $uri $uri/ =404; }
}
```

**Step 4 — Reload and verify.** `sudo nginx -t && sudo systemctl reload nginx` (on failure, `sudo journalctl -u nginx -n 30`). Confirm the app responds before reporting done — locally with `curl -s -H 'Host: <subdomain>.vm.internal' http://localhost/`, or hit the public `https://<subdomain>.abacusai.cloud`.

### Managing hostnames from inside the VM

Add or remove hostnames programmatically via the public Abacus API at the `api_base_url` from user-data. Auth uses the `ABACUS_API_KEY` already in user-data; `personal_agent_computer_id` identifies this VM. After any change, re-fetch `user-data` — the updated list lives in `http_ingress_settings.hostnames`.

```bash
TOKEN=$(curl -s -X PUT "http://169.254.169.254/latest/api/token" \
  -H "X-abacus-vm-metadata-token-ttl-seconds: 300")
USER_DATA=$(curl -s -H "X-abacus-vm-metadata-token: $TOKEN" http://169.254.169.254/latest/user-data)
PAC_ID=$(echo "$USER_DATA" | python3 -c "import sys,json; print(json.load(sys.stdin)['personal_agent_computer_id'])")
API_BASE=$(echo "$USER_DATA" | python3 -c "import sys,json; print(json.load(sys.stdin)['api_base_url'])")

# Add a hostname (subdomain must be 4-63 chars, lowercase a-z/0-9/hyphen, no leading/trailing hyphen or dots; ends with .abacusai.cloud; max 50 hostnames per VM)
curl -s "$API_BASE/api/addPersonalAgentComputerHostname" \
  -H "apiKey: $ABACUS_API_KEY" \
  -d "personalAgentComputerId=$PAC_ID&hostname=myapp.abacusai.cloud"

# Remove a hostname
curl -s "$API_BASE/api/removePersonalAgentComputerHostname" \
  -H "apiKey: $ABACUS_API_KEY" \
  -d "personalAgentComputerId=$PAC_ID&hostname=myapp.abacusai.cloud"

# Make the VM completely private (envoy stops routing public traffic to it)
curl -s "$API_BASE/api/disablePersonalAgentComputerPublicAccess" \
  -H "apiKey: $ABACUS_API_KEY" \
  -d "personalAgentComputerId=$PAC_ID"
```

New hostnames take roughly 20–30 seconds to propagate through DNS before public URLs become reachable.

## Pre-installed tools

- `claude` — Claude Code CLI
- `codex` — OpenAI Codex CLI
- `abacusai` — Abacus AI CLI
- `aws` — AWS CLI v2
- `psql` — PostgreSQL client
- `node` / `npm` — Node.js 22
- `google-chrome-stable` — full browser with internet access
- `python3` — Python 3.12
- `gh` — GitHub client

**Installing global npm packages — don't use `sudo`.** `npm i -g <pkg>` installs into the user-owned prefix (`/opt/abacus-npm`, already on `PATH`). `sudo npm …` is unnecessary, installs to a different prefix (`/usr`), and uses a separate cache.

<!-- ABACUS_CLOUD_INSTRUCTIONS_END -->
