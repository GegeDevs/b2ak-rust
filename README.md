# 🤖 Boot.img to AnyKernel Telegram Bot (Rust)

A Telegram bot that turns an Android `boot.img` into an AnyKernel flashable zip.
Upload a boot image (or send a download link), and the bot unpacks it with
`magiskboot`, swaps the extracted kernel into a fresh AnyKernel checkout, and
sends back a recovery-flashable zip.

> **Attribution:** This project is a Rust rewrite of
> [zylhdrXP/bootimg-to-anykernel](https://github.com/zylhdrXP/bootimg-to-anykernel).
> The command flow, limits, and deployment model follow that Python bot.

This is a Rust rewrite of the original Python bot. Like the Python version it
uses **MTProto** (via [`grammers`](https://github.com/Lonami/grammers)), not the
HTTP Bot API, so the usual **20 MB download limit does not apply** and modern
32–128 MB boot images work fine.

---

## ⚙️ How it works

1. **Input** — the user sends `/start` or `/help`, then uploads a `.img`
   document, a direct `http(s)` link, or a
   [gofile.io](https://gofile.io) link (`https://gofile.io/d/<content_id>`).
2. **Usage limit** — non-owner users get **3 compiles per UTC day**. The
   configured `OWNER_ID` is exempt. Usage is stored atomically in
   `$WORKSPACE_DIR/usage.json`.
3. **magiskboot** — on first use the bot finds the latest Magisk release on
   GitHub, downloads the APK, and extracts the `magiskboot` binary matching the
   host architecture (x86_64, aarch64, x86, arm). It is cached in
   `$WORKSPACE_DIR/magiskboot`.
4. **Unpack** — `magiskboot unpack boot.img` runs inside a per-job temporary
   directory, so concurrent jobs cannot interfere with each other.
5. **AnyKernel** — the extracted `kernel` is renamed to `Image`, a shallow
   clone of the AnyKernel repository is made, and `Image` is copied in.
6. **Zip** — the checkout is archived with all files at the zip root (the same
   layout `shutil.make_archive` produced), and `.git` is excluded.
7. **Delivery** — the zip is uploaded to Telegram, the caption reports the
   remaining runs, and the temporary directory is deleted.

Multiple jobs are processed concurrently (default **4**), bounded by
`MAX_CONCURRENT_JOBS`.

---

## 📂 Repository layout

| Path | Purpose |
| --- | --- |
| `src/main.rs` | Startup, Telegram session, update stream, signal handling |
| `src/bot.rs` | Update loop, document/link handlers, direct downloads |
| `src/repack.rs` | magiskboot unpack, AnyKernel clone, zip creation, upload |
| `src/gofile.rs` | gofile.io API, salt extraction, RC4/base64, recursive file walk |
| `src/magiskboot.rs` | Magisk APK download and arch-aware `magiskboot` extraction |
| `src/config.rs` | Environment configuration |
| `src/usage.rs` | Daily usage limits |
| `Dockerfile` / `docker-compose.yaml` | Container deployment (builds from source) |
| `Dockerfile.release` | Runtime image built from release binaries |
| `.github/workflows/main.yml` | Scheduled GitHub Actions deployment |
| `.github/workflows/release.yml` | Tag-driven multi-arch binary releases |
| `.github/workflows/docker.yml` | Multi-arch Docker Hub publishing |
| `.env.example` | Environment template |

---

## 🚀 Setup & credentials

The bot needs a **Bot Token**, a Telegram **API ID / API Hash** (for MTProto),
and optionally your numeric user ID to bypass the daily limit.

### 1. Bot token

1. Open Telegram and message [@BotFather](https://t.me/BotFather).
2. Send `/newbot` and follow the prompts.
3. Copy the HTTP API bot token.

### 2. API ID and API hash

1. Log in at [https://my.telegram.org](https://my.telegram.org).
2. Open **API development tools**.
3. Create an application (any name/URL is fine).
4. Copy **App api_id** and **App api_hash**.

### 3. Owner user ID (optional)

Message [@userinfobot](https://t.me/userinfobot) or
[@raw_data_bot](https://t.me/raw_data_bot) and copy the numeric **Id**.
Set it as `OWNER_ID` to skip the daily limit.

---

## 🛠️ Build and run locally

### Prerequisites

- Rust **1.89+** (`rustup` recommended)
- `git` on `PATH` (used to clone AnyKernel)
- `ca-certificates`
- A C compiler (`build-essential` / `gcc`) for the rustls `ring` dependency

No OpenSSL development package is required: the HTTP client uses rustls.

```bash
git clone <your-repo-url> b2ak
cd b2ak

cp .env.example .env
$EDITOR .env          # fill in the required values

cargo build --release --locked
./target/release/b2ak
```

`dotenvy` loads `.env` automatically; exported environment variables take
precedence. Logs are controlled by `RUST_LOG` (`info` by default).

### Tests and checks

```bash
cargo test
cargo fmt --check
cargo clippy --all-targets -- -D warnings
```

The test suite covers the gofile RC4/salt helpers, content-ID extraction,
`boot.img` preference, zip root layout, and magiskboot candidate selection.

---

## 🔧 Configuration

All settings are environment variables. Required values have no default.

| Variable | Required | Default | Description |
| --- | --- | --- | --- |
| `TELEGRAM_BOT_TOKEN` | ✅ | — | Bot token from @BotFather |
| `TELEGRAM_API_ID` | ✅ | — | Numeric API ID from my.telegram.org |
| `TELEGRAM_API_HASH` | ✅ | — | API hash from my.telegram.org |
| `OWNER_ID` | — | `0` | Telegram user ID exempt from the daily limit |
| `ANYKERNEL_REPO` | — | `https://github.com/osm0sis/AnyKernel3.git` | Repository cloned for each job |
| `ANYKERNEL_BRANCH` | — | `master` | Branch passed to `git clone -b` |
| `GF_TOKEN` | — | — | gofile.io account token; anonymous account created when empty |
| `GF_TIMEOUT` | — | `15.0` | gofile.io connect/read timeout in seconds |
| `GOFILE_WT_SALT` | — | dynamic | Override the gofile website-token salt if extraction fails |
| `WORKSPACE_DIR` | — | `.` | Sessions, downloads, `magiskboot`, `usage.json` |
| `SESSION_PATH` | — | `$WORKSPACE_DIR/repacker_bot.session` | SQLite Telegram session |
| `MAGISKBOOT_PATH` | — | `$WORKSPACE_DIR/magiskboot` | Cached magiskboot binary |
| `MAX_CONCURRENT_JOBS` | — | `4` | Boot images processed in parallel |
| `RUST_LOG` | — | `info` | `trace`, `debug`, `info`, `warn`, `error` |

Keep the workspace directory on persistent storage: it holds the Telegram
session (so the bot does not re-authenticate every start), the usage database,
and the cached `magiskboot`.

---

## 🐳 Docker

```bash
cp .env.example .env
$EDITOR .env

docker compose up -d --build
docker compose logs -f
```

The multi-stage image builds a static musl binary on `rust:1.89-alpine` and
runs it on `alpine:3.24` with only `git` and `ca-certificates` installed. The
final image is far smaller than the Debian-based build. The `magiskboot`
binary downloaded from the Magisk APK is statically linked, so it runs
unchanged on Alpine/musl.

The named volume `bot_data` is mounted at `/app/workspace`, so the session,
usage data and `magiskboot` survive rebuilds and restarts.

To use a published image instead of building locally, replace the `build`
section in `docker-compose.yaml` with an `image:` reference.

Rebuild after changing the code:

```bash
docker compose up -d --build
```

---

## 🐳 Docker Hub images

The **Docker Hub** workflow publishes a multi-arch image built from the static
binaries attached to a GitHub Release. Since the binary is copied into the
image instead of compiled there, even the emulated architectures build in
seconds. The published image is
[`gegedesembri/b2ak`](https://hub.docker.com/r/gegedesembri/b2ak).

| Docker platform | Release binary |
| --- | --- |
| `linux/amd64` | x86_64 |
| `linux/arm64` | aarch64 |
| `linux/arm/v7` | armv7 |
| `linux/386` | i686 |

### Credentials

Create a Docker Hub **Personal Access Token** with **Read & Write** permission
under **Account settings → Personal access tokens**, then store it and your
username as secrets in the `DOCKERHUB` environment:

```bash
gh secret set DOCKERHUB_USERNAME --env DOCKERHUB --repo GegeDevs/b2ak-rust
gh secret set DOCKERHUB_TOKEN --env DOCKERHUB --repo GegeDevs/b2ak-rust
```

Both commands prompt for the value without echoing it. The workflow declares
`environment: DOCKERHUB`, so environment secrets resolve, and it fails early
with a clear error when either secret is missing.

### Publishing

The `Release` workflow dispatches this workflow automatically after a GitHub
Release is published. To republish an existing release manually:

```bash
gh workflow run docker.yml --repo GegeDevs/b2ak-rust -f version=v0.1.0
gh run watch
```

Stable releases get three tags (`latest`, `X.Y`, `X.Y.Z`); prereleases only get
the exact version tag.

### Running the image

```bash
docker pull gegedesembri/b2ak:latest

docker run -d --name b2ak \
  --env-file .env \
  -v b2ak_data:/app/workspace \
  gegedesembri/b2ak:latest
```

The image is Alpine-based and needs no Rust toolchain. `magiskboot` is still
downloaded from the Magisk APK on first use and cached in the volume.

---

## 📦 Prebuilt releases

Every `v*` tag builds statically linked musl binaries and publishes them on the
repository's **Releases** page. No Rust toolchain is needed on the target
machine.

| Architecture | Rust target | Archive |
| --- | --- | --- |
| x86_64 | `x86_64-unknown-linux-musl` | `b2ak-<version>-linux-x86_64.tar.gz` |
| aarch64 / arm64 | `aarch64-unknown-linux-musl` | `b2ak-<version>-linux-aarch64.tar.gz` |
| armv7 / armhf | `armv7-unknown-linux-musleabihf` | `b2ak-<version>-linux-armv7.tar.gz` |
| i686 / x86 | `i686-unknown-linux-musl` | `b2ak-<version>-linux-i686.tar.gz` |

Each archive contains the `b2ak` binary, `README.md`, and `.env.example`.
A combined `SHA256SUMS` file is published alongside the archives.

```bash
version=0.1.0
base="https://github.com/<owner>/<repo>/releases/download/v${version}"

curl -LO "$base/b2ak-${version}-linux-x86_64.tar.gz"
curl -LO "$base/SHA256SUMS"
sha256sum -c SHA256SUMS

tar -xzf "b2ak-${version}-linux-x86_64.tar.gz"
cp .env.example .env
$EDITOR .env
./b2ak
```

`git` and `ca-certificates` must be installed on the host. `magiskboot` is
downloaded from the latest Magisk APK on first use, so no extra setup is needed.

### Cutting a release

1. Bump `version` in `Cargo.toml`, run a build so `Cargo.lock` picks up the new
   version, and commit both files.
2. Tag the commit and push the tag:

   ```bash
   git tag v0.1.0
   git push origin v0.1.0
   ```

3. Alternatively, open **Actions → Release → Run workflow** and enter `v0.1.0`.
   The manual run creates the tag if it does not exist yet.

The version in the tag (or manual input) must match `Cargo.toml`, otherwise the
workflow fails before building. Versions containing a hyphen (for example
`v0.2.0-beta.1`) are published as pre-releases.

The `Release` workflow builds all four targets with
[`cross`](https://github.com/cross-rs/cross), packages each binary, generates
`SHA256SUMS`, and publishes the GitHub Release.

---

## 🖥️ Deploy on a VPS with systemd

Build on the server (or copy a matching release binary), then install it:

```bash
cargo build --release --locked
sudo install -m 0755 target/release/b2ak /usr/local/bin/b2ak

sudo useradd --system --home /var/lib/b2ak --shell /usr/sbin/nologin b2ak
sudo mkdir -p /var/lib/b2ak
sudo chown b2ak:b2ak /var/lib/b2ak
```

Put your configuration in `/etc/b2ak.env`:

```bash
sudo cp .env.example /etc/b2ak.env
sudo nano /etc/b2ak.env
sudo chmod 600 /etc/b2ak.env
```

Create `/etc/systemd/system/b2ak.service`:

```ini
[Unit]
Description=Boot.img to AnyKernel Telegram Bot
After=network-online.target
Wants=network-online.target

[Service]
Type=simple
User=b2ak
Group=b2ak
WorkingDirectory=/var/lib/b2ak
EnvironmentFile=/etc/b2ak.env
Environment=WORKSPACE_DIR=/var/lib/b2ak
ExecStart=/usr/local/bin/b2ak
Restart=always
RestartSec=5
# Allow running jobs to finish cleanly on stop/restart.
KillSignal=SIGINT
TimeoutStopSec=60

[Install]
WantedBy=multi-user.target
```

Enable and start it:

```bash
sudo systemctl daemon-reload
sudo systemctl enable --now b2ak.service
sudo systemctl status b2ak.service
journalctl -u b2ak.service -f
```

The bot handles `SIGINT`/`Ctrl+C` and `SIGTERM` by shutting down the Telegram
client gracefully, so `docker stop` and `systemctl stop` are clean.

---

## ☁️ Deploy with GitHub Actions

GitHub Actions jobs are limited to 6 hours, so the workflow runs the bot on a
**5-hour cron** and relies on a concurrency group to cancel the previous
instance. That keeps a single bot running indefinitely without duplicate
update polling.

1. Fork or push this repository to GitHub.
2. Go to **Settings → Secrets and variables → Actions** and add the repository
   secrets:
   - `TELEGRAM_BOT_TOKEN`
   - `TELEGRAM_API_ID`
   - `TELEGRAM_API_HASH`
   - `OWNER_ID`
   - Optional: `ANYKERNEL_REPO`, `ANYKERNEL_BRANCH`, `GF_TOKEN`,
     `GOFILE_WT_SALT`
3. Optionally add a repository **variable** `MAX_CONCURRENT_JOBS`.
4. Open the **Actions** tab, choose **Run Telegram Repacker Bot**, and click
   **Run workflow**. The cron schedule takes over afterwards.

Notes:

- `Cargo.lock` is committed and the workflow builds with `--locked`.
- `Swatinem/rust-cache` caches dependencies and build artifacts between runs.
- The workspace is ephemeral on Actions. The Telegram session is recreated on
  each start, and `magiskboot` is re-downloaded when needed. Usage limits reset
  with the runner, which is acceptable for a free serverless setup.
- Empty optional secrets are treated as unset, so defaults still apply.

---

## 📱 Using the bot

1. Send `/start` or `/help` to the bot.
2. Provide a boot image in one of these ways:
   - Upload it as a **document** named `*.img`.
   - Send a direct download link starting with `http://` or `https://`.
   - Send a gofile link: `https://gofile.io/d/abc123`.
   - For password-protected gofile content, put the password after the URL:
     `https://gofile.io/d/abc123 mypassword`.
3. Watch the progress message:
   - ⏳ *Downloading your boot image...*
   - 🔧 *Verifying magiskboot tool...*
   - 📦 *Unpacking boot image...*
   - 🗂️ *Cloning AnyKernel repository...*
   - 🤐 *Creating flashable zip archive...*
   - 📤 *Uploading zip archive...*
4. The bot replies with the flashable zip. Non-owner users see the remaining
   daily runs in the caption.

### gofile.io details

For a gofile link the bot:

1. Creates a temporary anonymous gofile account (or uses `GF_TOKEN`).
2. Recursively walks the content tree, preferring a file named `boot.img` and
   falling back to the first `.img` file.
3. Streams it into the job workspace and runs the normal pipeline.

If gofile changes its website-token obfuscation, set `GOFILE_WT_SALT` to the
current salt; otherwise the bot falls back to the built-in default.

---

## 🧯 Troubleshooting

| Symptom | Fix |
| --- | --- |
| `TELEGRAM_BOT_TOKEN is missing` | Populate `.env` or export the variable; empty values count as missing. |
| `failed to open the session database` | Ensure `WORKSPACE_DIR` exists and is writable by the bot user. |
| `Failed to unpack the boot image` | The upload is not a supported Android boot image, or it is corrupted. |
| `failed to execute git` | Install `git` in the runtime image/host. |
| magiskboot download fails | GitHub may be rate-limiting; retry, or place a working `magiskboot` at `MAGISKBOOT_PATH`. |
| `Limit Exceeded` | The user hit 3 compiles for the current UTC day; set `OWNER_ID` to exempt an account. |

---

## 📄 License

This is a Rust rewrite of
[zylhdrXP/bootimg-to-anykernel](https://github.com/zylhdrXP/bootimg-to-anykernel);
see that project for the original license and credits. The licenses of the
bundled dependencies also apply. AnyKernel3 is cloned at runtime and is not
redistributed with this bot.
