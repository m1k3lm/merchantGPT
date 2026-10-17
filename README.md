# merchantGPT

merchantGPT is a desktop chat assistant that runs an online shop for its merchant. Instead of clicking through the WooCommerce or Magento admin, the merchant asks in plain language: "which orders are still processing?", "put the blue hoodie on sale at 29.90", "how did we sell last week compared to the week before?". An AI model reads the shop through its API, answers with real data, and proposes changes that the merchant approves before anything is written.

It is a native Rust application for macOS, Linux and Windows. There is no account or login: you add a shop by its URL and API credentials, and everything is stored locally in an encrypted database.

## What it can do

- **Chat with your shop.** List and search products, orders and customers, read single orders with their line items, and get sales reports for any date range.
- **Change the shop, with your approval.** Update product name, price, sale price, stock and status, or change an order's status. Every change appears as a confirmation card showing the exact values; nothing is written until you click Approve.
- **Activity log.** Every change the assistant made or tried to make, approved or denied, is kept per shop under **Activity**.
- **Several shops.** Switch between shops in the side panel; each shop has its own chats, notes and tasks.
- **Saved chats.** Conversations are kept and can be reopened, renamed or deleted.
- **Memory.** The assistant can save short notes about you or your shop (for example "we ship on Mondays") and uses them in later chats. Saving a note also asks for approval.
- **Skills.** Reusable instructions in a skills folder, run by typing `/skill-name` in the chat. A daily sales summary skill is installed on first run.
- **Google Ads drafts.** Link a Google Ads account to a shop with **+ > Google Ads > Sign in with Google**. The assistant can then list campaigns with recent results and draft complete Search campaigns (budget, ad group, responsive search ad, keywords, countries). Drafts are always created paused: nothing runs or spends until you enable them in Google Ads.
- **Scheduled tasks.** Run a prompt or a skill daily, weekly or every few hours. Scheduled runs can read the shop but never change it; their reports appear in a "Task: ..." chat.

### Supported platforms and AI providers

| Shops | Status |
|---|---|
| WooCommerce | Supported, tested against a real store |
| Magento 2 | Supported, tested against simulated responses only |
| Shopify, PrestaShop | Can be added, but their operations report "not implemented yet" |

| AI providers | Notes |
|---|---|
| Anthropic | Suggested model `claude-opus-5-5` |
| AWS Bedrock | Claude in Amazon Bedrock, model ids like `anthropic.claude-opus-5-5`; Bedrock API key or AWS access keys |
| OpenAI | |
| Google Gemini | |
| DeepSeek | |

## Building

The repository is a Cargo workspace. There are two ways to build it.

### Native build (macOS, run the app)

Use this to run the desktop app. It needs Rust 1.99 or newer (via [rustup](https://rustup.rs)) and, on macOS, the Xcode command line tools.

```bash
cargo build --release
./target/release/merchantgpt
```

If your global Rust toolchain is older, pin this folder only:

```bash
rustup override set 1.99.1-aarch64-apple-darwin
```

The first build takes a few minutes because it compiles SQLCipher and OpenSSL from source; the binary does not depend on system OpenSSL or SQLite.

### Docker build (Linux, quality checks)

The `dev` service in `docker-compose.yml` has the full Linux toolchain, so you can build and test without installing Rust. It cannot show the app's window.

```bash
docker compose build
docker compose run --rm dev cargo build --release
```

### Release builds with Google Ads credentials

The publisher's Google credentials are compiled into the release binary, so they never live in the repository:

- An **OAuth client** (client ID and secret) of type "Desktop app" in a Google Cloud project with the Google Ads API enabled.
- A **developer token** from the API Center of the publisher's Google Ads manager account. Until Google approves it for Basic access, it only works with test accounts.

```bash
cp release.env.example release.env      # gitignored; fill in the three values
bash scripts/build-release.sh           # first time; afterwards plain cargo build --release is enough
```

Every build of the workspace (`cargo build`, `cargo run`, `cargo test`) compiles in the values from `release.env`, and a build with all three values never asks the merchant for them. Cargo starts tracking the file at the first build that finds it, so after creating it run the script once (or `touch crates/core/build.rs`); after that, edits to the file trigger a rebuild. Variables set in the environment override the file, which suits CI secrets. The script also warns about any missing value; that build asks users for it.

Anything compiled into a desktop app can be extracted from the binary. The developer token alone gives no access to anyone's ads account (each call also needs the merchant's own Google sign-in), but it could be misused under the publisher's API quota. A server-side proxy that adds the token would avoid shipping it.

## Running and first steps

1. Start the app. On first launch it creates its configuration and an encrypted database, and stores the database key in the OS keychain. On macOS, choose "Always Allow" if asked for keychain access (this can happen again after each rebuild).
2. Open **Settings**, pick an AI provider, enter the model and API key, and save.
3. Click **+**, then **Shop**, choose the platform, and enter the shop URL and credentials.
4. Ask something, or type `/` to see the available skills.

### Shop credentials

- **WooCommerce:** in the WordPress admin go to WooCommerce > Settings > Advanced > REST API, add a key with Read/Write permission, and copy the consumer key and secret. HTTPS stores use Basic auth; plain-HTTP stores are signed with OAuth 1.0a automatically.
- **Magento 2:** go to System > Integrations, add an integration with API access to Catalog, Sales and Customers, activate it and copy the **Access Token** (the other three values are not used). On Magento 2.4.4 or later also set Stores > Configuration > Services > OAuth > Consumer Settings > "Allow OAuth Access Tokens to be used as standalone Bearer tokens" to Yes. Integration tokens do not expire; rotate one with **Reauthorize**.

### Google Ads

Click **+**, then **Google Ads** (a shop must be selected; the ads account is linked to it), then **Sign in with Google**. Your browser opens Google's consent page; after you allow access, pick the Google Ads account to link. Client accounts under a manager account are listed too.

Merchants never enter Google credentials other than their own sign-in: the app's credentials ship inside the release build (see [Release builds with Google Ads credentials](#release-builds-with-google-ads-credentials)). A build made without them asks for the missing ones once and stores them encrypted.

To unlink, right-click the shop and choose **Unlink Google Ads**.

### Where things are stored

On macOS everything lives in `~/Library/Application Support/com.seQura.merchantGPT/` (on Linux and Windows, the platform's config and data folders):

| File | Contents |
|---|---|
| `config.toml` | Defaults: provider, model, agent limits, extra skill folders, database path. API keys are refused here. |
| `merchantgpt.db` | SQLCipher-encrypted database: shops, credentials, API keys, chats, notes, tasks, activity log. |
| `skills/` | Your skills. Each is a folder with a `SKILL.md` and optional `scripts/`. |

The database key is kept in the macOS Keychain, Windows Credential Manager or the Linux Secret Service. Without it the database cannot be opened, and the app refuses to create a new key over existing data.

### Writing a skill

A skill is a folder containing a `SKILL.md`:

```markdown
---
name: restock-report
description: List products with low stock and suggest reorder quantities.
---

1. Call shop_list_products and find products with stock_quantity of 5 or less.
2. ...
```

Run it with `/restock-report`. Scripts placed in the skill's `scripts/` folder can be run by the assistant, always after you approve each run. See `skills/daily-sales-summary/SKILL.md` for a full example.

## Testing

### Full quality gate

Builds everything, runs all tests, runs clippy with warnings as errors, and checks formatting. This is the check to run before committing:

```bash
docker compose run --rm dev bash scripts/check.sh
```

### Running tests directly

```bash
cargo test                       # all crates (also works inside the dev container)
cargo test -p connectors         # one crate
cargo test sales_report          # tests whose name contains "sales_report"
```

The tests need no network or API keys. AI providers are tested with a scripted fake model and recorded streams, shops with fake HTTP responses and local mock servers, and the database with in-memory stores.

### Live tests against a local WooCommerce store

The `woo` compose profile runs a real WordPress and WooCommerce store with sample data at `http://localhost:8080`. You can also connect the app to it.

```bash
docker compose --profile woo up -d woo
docker compose --profile woo run --rm woo-cli bash /scripts/woo-setup.sh   # prints WOO_URL, WOO_CK, WOO_CS
```

Then run the two ignored live tests with those values:

```bash
docker compose run --rm \
  -e WOO_URL=http://localhost:8080 -e WOO_CK=... -e WOO_CS=... \
  -e WOO_RESOLVE=host.docker.internal:8080 \
  dev cargo test -p connectors --test woocommerce_live -- --ignored

# same environment variables
docker compose run --rm ... dev cargo test -p chat-core --test live_woo_agent -- --ignored
```

The first exercises the WooCommerce connector; the second runs the whole agent (scripted model, real store, approval and activity log).

To reset the store, remove only its containers and volumes. Do not use `docker compose down -v`, which also deletes the build cache volumes:

```bash
docker compose --profile woo rm -sf woo woo-db woo-cli
docker volume rm merchantgpt_woo-db merchantgpt_woo-html
```

## Project layout

| Path | Purpose |
|---|---|
| `crates/tools` | Tool definitions shared by connectors and the AI layer |
| `crates/connectors` | Shop connectors (WooCommerce, Magento, stubs) and HTTP transport |
| `crates/llm` | AI provider clients with streaming and tool calling |
| `crates/skills` | Skill loading, slash commands and script running |
| `crates/store` | Encrypted SQLite database and keychain access |
| `crates/core` | Agent loop, approvals, prompts, configuration, scheduler (package `chat-core`) |
| `crates/app` | The desktop interface (binary `merchantgpt`) |
| `docs/PLAN.md` | The implementation plan |
| `CLAUDE.md` | Conventions and architecture notes for contributors |

## Known limitations

- Shopify and PrestaShop are placeholders.
- The Magento and Google Ads connectors have not yet been verified against a live store or account.
- Windows binaries are not produced yet; build on Windows directly or add a CI runner.
- Shared or published builds must be hosted on seQura's `tech-ai-tools` S3 bucket (request access in Slack #ai-tools).
