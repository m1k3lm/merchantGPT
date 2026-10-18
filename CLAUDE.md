# CLAUDE.md

This file provides guidance to Claude Code (claude.ai/code) when working with code in this repository.

## What this is

merchantGPT is a merchant-facing chatbot that replaces the admin section of common e-commerce platforms (WooCommerce, Magento, PrestaShop, Shopify, ...) for day-to-day operations: catalog, orders, customers, reporting and sales analysis. It is a standalone native Rust desktop application for Linux, macOS and Windows. A browser/WASM build was considered and dropped: calling platform admin APIs from a browser origin hits CORS and some platforms refuse it outright. There is no login: shops are identified by their URL, several shops can be managed with a switcher, and platform API credentials are entered through the `+` connector flow.

## Toolchains: Docker for the Linux gate, the macOS host for running the app

Two toolchains, same Rust version (1.99.1):

- **Docker (`dev` service)** is the Linux build and the quality gate. `docker-compose.yml` builds it from the repo `Dockerfile` (official `rust:1-bookworm` image plus eframe's Linux build deps, libdbus for the keyring, and cmake/perl for vendored SQLCipher), mounts the repo at `/app`, and keeps `target/` and the cargo caches in named volumes. Add dependencies with `docker compose run --rm dev cargo add <crate> -p <member>`.
- **The macOS host** builds and runs the native GUI, since the container cannot display a window. The machine's global rustup default is an old x86_64 toolchain used by other projects, so this repo is pinned with a per-directory override (machine-local state, not in git): `rustup override set 1.99.1-aarch64-apple-darwin`. Never change the global default. Host builds write to `./target`, separate from the container's volume.

Ad-hoc-signed rebuilds change the binary's identity, so macOS may ask to allow keychain access to the `merchantgpt` / `db-key` item after a rebuild; choose "Always Allow". Denying it leaves the app on its "could not start" screen.

## Commands

When piping cargo output through `grep`/`head`, the pipeline's exit code is the filter's, not cargo's. Use `scripts/check.sh` (it sets `pipefail`) as the source of truth. It caps parallel jobs at 4 (`CARGO_BUILD_JOBS` overrides), because linking every test binary at once ran the 8 GB Docker VM out of memory (`ld terminated with signal 9 [Killed]`); if that still happens, lower it or stop the `woo`/`presta` stores.

```bash
# macOS host: native build and run (window opens on your desktop)
cargo build --release && ./target/release/merchantgpt
cargo test                                             # also passes natively

# Docker: Linux build and the full gate
docker compose build                                   # (re)build the dev image
docker compose run --rm dev bash scripts/check.sh      # full gate: build, test, clippy -D warnings, fmt --check
bash scripts/build-release.sh                          # macOS release; warns if a Google credential is missing from release.env
docker compose run --rm dev cargo build                # native debug build
docker compose run --rm dev cargo build --release
docker compose run --rm dev cargo test                 # all tests
docker compose run --rm dev cargo test <test_name>     # single test (substring match)
docker compose run --rm dev cargo test -p <crate>      # tests of one workspace crate
docker compose run --rm dev cargo clippy --all-targets -- -D warnings
docker compose run --rm dev cargo fmt --check
```

### Local WooCommerce store (`woo` compose profile)

A real WordPress 7 + WooCommerce store with sample data, for manual testing of the app and the ignored live test. Its home URL is `http://localhost:8080`, so over plain HTTP the connector signs requests with OAuth 1.0a.

```bash
docker compose --profile woo up -d woo
docker compose --profile woo run --rm woo-cli bash /scripts/woo-setup.sh   # prints WOO_URL / WOO_CK / WOO_CS (new key each run)
docker compose run --rm -e WOO_URL=http://localhost:8080 -e WOO_CK=... -e WOO_CS=... \
  -e WOO_RESOLVE=host.docker.internal:8080 dev cargo test -p connectors --test woocommerce_live -- --ignored
# same env, full agent path (scripted model, real store, audit): cargo test -p chat-core --test live_woo_agent -- --ignored
docker compose --profile woo stop
docker compose --profile woo rm -sf woo woo-db woo-cli && docker volume rm merchantgpt_woo-db merchantgpt_woo-html   # reset the store only
```

### Local PrestaShop store (`presta` compose profile)

PrestaShop 9.2 with demo data at `http://localhost:8081`. The image's start script exits with an error right after a successful first install, so the service has `restart: on-failure`. `PRESTA_IMAGE` overrides the image (e.g. an already-pulled image built on the official one).

```bash
docker compose --profile presta up -d presta
docker compose --profile presta exec presta bash /scripts/presta-setup.sh   # enables the webservice; fixed local key, prints PRESTA_URL / PRESTA_KEY
PRESTA_URL=http://localhost:8081 PRESTA_KEY=... cargo test -p connectors --test prestashop_live -- --ignored   # from the host
docker compose --profile presta rm -sf presta presta-db && docker volume rm merchantgpt_presta-db merchantgpt_presta-html   # reset
```

### Local mail catcher (`mail` compose profile)

mailpit on SMTP port 1025 (any login) with its API and UI on 8025: `docker compose --profile mail up -d mailpit`, then `cargo test -p connectors --test mail_smtp_live -- --ignored` from the host.

Never use `docker compose down -v` to reset the store: it also deletes the dev service's cargo cache volumes. `WOO_RESOLVE` keeps the request URL equal to the store's home URL (part of the OAuth signature) while connecting through the Docker host.

## Architecture

merchantGPT is an agent in the Hermes Agent / OpenClaw style: an LLM drives merchant operations through native tool calling. Connectors contribute tools, skills contribute instructions and scripts, and every mutating tool call is gated behind a confirmation card in the UI. The full phased plan lives in `docs/PLAN.md`.

The workspace (`crates/*`) has seven crates. Dependencies only point downward:

```
tools  <-  connectors, llm, skills, store  <-  chat-core  <-  app
```

- **`tools`** is the leaf: `ToolDefinition`, `ToolCall`, `ToolResult`, the async `Tool` trait, `Mutability` (Read vs Mutate) and `ToolRegistry`. It exists so connectors (which produce tools) and `llm` (which consumes tool definitions) never depend on each other.
- **`connectors`** holds a generic `Connector` trait (`kind`, `credential_schema`, `verify`, `tools`). Every platform has a real client; `StubShop` remains for adding new ones. Kinds are `ConnectorKind::Shop(Platform)`, `ConnectorKind::GoogleAds` and the email/marketing services `ConnectorKind::MAIL` (`Smtp`, `Brevo`, `Mailchimp`, `Klaviyo`, module `connectors/src/mail`, linked to the active shop with `Command::ConnectService`); carriers are future kinds. A shop holds a list of connectors (its platform plus linked services such as Google Ads), all saved as `connector_credentials` rows of that shop and all contributing tools. `ShopConnector` is the async shop-operations trait; the `ShopTools` adapter turns any `ShopConnector` into tools once, so a new platform only implements `ShopConnector`. Writes are grouped per entity (`shop_manage_product|category|customer|address|coupon|order` with an `action` argument) to keep the per-turn tool definitions short; `shop_api_get` / `shop_api_write` reach the platform's own API, refuse paths with a host or `..`, and cap their output. Methods a platform lacks keep the trait's default `ConnectorError::Unsupported` (not `NotImplemented`, which means a stub connector), and `hidden()` removes tools or `tool:action` pairs from what the model sees. HTTP goes through the `HttpTransport` trait so connectors are tested with `FakeTransport` or wiremock. The UI renders credential forms from `CredentialSchema`, so adding a platform never touches UI code.
- **`llm`** normalises OpenAI, Anthropic, Google, DeepSeek and AWS Bedrock into one `ChatMessage` / `StreamEvent` model behind `LlmProvider::stream`. `ChatMessage::ToolResults` groups all results of a turn because Anthropic requires them in one user message. Gemini has no tool-call ids, so ids are synthesised and results matched by order. DeepSeek reuses the OpenAI codec with another base URL. Bedrock is Claude in Amazon Bedrock on the Messages-API endpoint (`https://bedrock-mantle.{region}.api.aws/anthropic`, `anthropic.`-prefixed model ids), so it reuses the Anthropic codec; it authenticates with a Bedrock API key in `x-api-key`, or with AWS access keys signed by `sigv4.rs` for service `bedrock-mantle` (plus `x-amz-security-token` for temporary credentials). Bedrock has no server-side `fallbacks` and gets no `eager_input_streaming`. Provider-specific settings (region, AWS key id, session token) live in `ProviderConfig::options`, persisted in the `llm_settings.options` JSON column; `llm::is_secret_option` marks the secret ones, which are never sent to the UI and are redacted like API keys. A blank secret in Settings keeps the saved one, but only for the same AWS key id.
- **`skills`** discovers skill folders containing `SKILL.md` (`---` frontmatter with `name` and `description`, then instructions) plus optional scripts. Only names and descriptions go into the system prompt; the agent pulls a body with the `load_skill` tool and runs scripts with `run_skill_script` (a Mutate tool, so it is confirmed). Scripts run only from the skill's `scripts/` folder, by listed file name, with a scrubbed environment, a timeout and an output cap. Typing `/skill-name ...` in the chat expands the skill's instructions into the user message. Skill roots are `{config_dir}/skills` (seeded on first run with the bundled `skills/` examples from this repo) plus `[skills] dirs` in config.toml.
- **`store`** is rusqlite with SQLCipher. The 32-byte DB key is generated once and kept in the OS keyring behind the `KeyStore` trait; there is never a fallback to an unencrypted DB. Migrations are embedded SQL tracked by `PRAGMA user_version`. Stores shops, connector credentials, LLM settings, conversations, memory notes, scheduled tasks and the audit log.
- **`chat-core`** (imported as `chat_core`, because a crate named `core` would shadow Rust's built-in `core`) owns the agent loop `run_turn`, the `ConfirmationGate` trait (`UiGate`, `AutoDeny`, `AlwaysApprove`, `TaskGate`), the system prompt builder, TOML config, memory tools and the scheduler. An unattended run refuses every change unless its gate says `decides_unattended()`, which only `TaskGate` does: it approves exactly the changing tools or `tool:action` pairs the merchant ticked when creating the task (`scheduled_tasks.allowed_tools`, validated against the shop's current changing tools; the UI asks for a second confirmation that lists them). Everything else is denied, and approvals are audited as "allowed by scheduled task …". A task with no permissions is read-only, as before. The app's runtime sends `Command::RunDueTasks` every 60 s (first tick at startup, which catches up a task missed while the app was closed, once). Each run starts from a fresh context, records `last_run_at` before running so a failing task is not retried every minute, appends its transcript to a `Task: <name>` conversation, and never streams into the open chat. Schedules are `daily HH:MM`, `weekly mon HH:MM` or `every 6h` / `every 30m` (minimum 15m), in local time; `Backend::run_due_tasks_at(now)` takes the clock as a parameter for tests.
- **`app`** (binary `merchantgpt`) is the eframe/egui shell. Assistant replies render as Markdown (`app/src/markdown.rs`): tables are drawn with a plain `egui::Grid` of labels because `egui_commonmark` 0.26 sizes table columns from the header only and long cells overlap; everything else goes through `egui_commonmark` (image loading disabled). Each message gets its own id derived from `ui.scope_id()`. A tokio runtime is built in `main` before `eframe::run_native`; the UI sends commands over a channel and drains `AgentEvent`s with `try_recv` at the start of each frame, while producers call `ctx.request_repaint()`. Logic lives in `AppState::apply`, which is unit-tested headlessly; widgets only render state.

### Conventions that are easy to get wrong

- eframe 0.37 changed the trait: implement `App::ui(&mut self, ui: &mut egui::Ui, frame: &mut Frame)`, not the older `update(ctx, frame)`. Panels take `&mut Ui`; `egui::Window::show` still takes `&Context`.
- Assistant messages carry `provider_content`: the provider's raw blocks (Anthropic thinking blocks with signatures, Gemini parts with thought signatures). Encoders replay them unchanged to the same provider and rebuild from text and tool calls for any other. Never edit or strip them, and keep history append-only.
- The forced final turn keeps `tools` declared and sends `tool_choice: none`, because Anthropic rejects a history containing tool blocks without tool definitions.
- Tool calls in a reply that stopped on `max_tokens` are answered with error results and never run. A `refusal` stop ends the turn without running anything or saving the partial text.
- Anthropic requests add `fallbacks: "default"` (beta `server-side-fallback-2026-07-01`) and `eager_input_streaming` only on the official endpoint; a custom base URL may be a proxy that rejects them. The suggested default model is `claude-opus-5-5`.
- Tool names must match `^[a-zA-Z0-9_-]{1,64}$` to be valid on all four providers.
- Tool output (product descriptions, order notes, customer text) is data, never instructions. The system prompt says so, and the confirmation card shows the exact arguments the model chose.
- `memory_save` and `memory_delete` are Mutate tools: notes are injected into every future system prompt, so a prompt injection in shop data must not be able to plant one without the merchant seeing it.
- Money is `rust_decimal::Decimal`, never `f64`.
- Google Ads (`connectors/src/google_ads`, REST API v26): sign-in is Google OAuth for installed apps (consent page in the system browser, one-shot listener on `127.0.0.1`, PKCE, `access_type=offline`), run as a spawned task that posts `Command::GoogleSignInFinished` back into the backend queue, so chat is not blocked. The refresh token never reaches the UI (`Secret` has a redacted Debug). The app's OAuth client and developer token are compiled in at build time (`GoogleApp::built_in`, `option_env!` of `MERCHANTGPT_GOOGLE_OAUTH_CLIENT_ID`, `MERCHANTGPT_GOOGLE_OAUTH_CLIENT_SECRET`, `MERCHANTGPT_GOOGLE_ADS_DEVELOPER_TOKEN`, set by `crates/core/build.rs` from the gitignored `release.env` at the workspace root on every build, environment variables winning; the file is watched only while it exists, so a newly created one is first picked up by `scripts/build-release.sh`); any value missing from the build is asked once and stored in the encrypted `app_settings` table, and built-in values always win. Per-shop credentials hold only the refresh token and account ids; `build_connectors` adds the app credentials at build time, so a new release token applies without re-linking. Test fixtures reset `google_built_in` so results do not depend on how the tests were compiled. Drafted Search campaigns are one atomic `googleAds:mutate` with temporary negative ids and are always created `PAUSED`; the EU political-advertising declaration is a required tool argument, never defaulted. Verified only against fakes, mock servers and the RFC 7636 PKCE example, not against Google.
- Magento 2 uses an integration access token as a bearer token (Magento 2.4.4+ must allow integration tokens as standalone bearer tokens; the credential form says how). Products are addressed by SKU, so a Magento `Product.id` is its SKU; product writes go to `/rest/all/V1` (all store views); order ids are numeric `entity_id`s, not increment ids. Order status changes map to cancel/hold/unhold or a status comment; "complete" is refused because Magento completes orders by invoicing and shipping. The sales report is summed client-side from non-cancelled orders (max 5,000 per range, UTC dates). Magento is only verified against fakes and wiremock, not a live store.
- Shopify (`connectors/src/shopify`) uses the GraphQL Admin API, version pinned in `API_VERSION` (2026-10). Auth is a legacy custom app's `X-Shopify-Access-Token`, or a Dev Dashboard app's client ID and secret exchanged through the client credentials grant (24-hour token, cached, renewed on expiry or 401; it only works for an app of the same organization as the store). The API lives on the `*.myshopify.com` domain, so an optional credential overrides a custom shop URL. Each **variant** is a domain `Product` (id is the variant gid; price, SKU and stock live there); name and status update the parent product, so they change every variant. A sale is `price` below `compareAtPrice`; sale price 0 ends it. Stock is set with `inventorySetQuantities`, which since 2026-04 needs `@idempotent(key:)` and an explicit `changeFromQuantity` (null here); variants stocked at several locations are refused. Order status is synthesised (`paid, unfulfilled`, `cancelled`, `..., closed`), and status filters accept those words plus other platforms' (`processing`, `completed`). Status changes map to cancel (restock, no refund, async job), close, open and mark-as-paid; fulfilment is refused. Pages are cursor-based: page N walks N-1 cursors (max page 20), and totals come from the count queries only where they accept the filter. Throttled requests are retried, even mutations, because Shopify never ran them. Only verified against fakes and wiremock.
- PrestaShop (`connectors/src/prestashop`) uses the webservice at `{shop}/api` with the key as Basic auth user; a plain-text 401 means the server stripped the header, so the key moves to the `ws_key` query parameter. Reads use `output_format=JSON`; writes must be XML (`xml.rs`, a writer only) and use PATCH, which PrestaShop 1.7 lacks (405 is reported as "needs 8 or later"). A multilang field in a PATCH must carry every language, or PrestaShop blanks the missing ones; names change in the first language with text and keep the others. Prices are base prices excluding tax; `final` is a computed field (`price[final][use_reduction]=1`) giving the sale price. A sale price replaces the product's plain specific prices (everyone, quantity 1, whole product) with one amount reduction; 0 ends the sale; group, customer or quantity prices stay. Stock is the product-level `stock_availables` row, refused for products with combinations. Order statuses are the shop's own `order_states`, matched by id, name, common words or a unique part of a name; a change is a POST to `order_histories` (PrestaShop's side effects, no email). Pagination is offset-based with one extra row to detect the last page (no totals). The sales report sums `valid` orders. Verified against the `presta` store by `tests/prestashop_live.rs`.
- Phase 2 operations per platform (create/update/delete, coupons, abandoned carts, raw API) are summarised in `docs/PLAN.md`. The traps: WooCommerce addresses are two fixed slots (`billing`, `shipping`) and core has no abandoned carts (hidden) or marketing-consent field; a sale price of 0 is sent as an empty string, since `"0"` makes the product free. Magento creates orders through the admin quote flow with an offline payment method and refuses price overrides; a coupon is a sales rule plus its code; customer and address writes re-save the whole customer. An abandoned Magento cart is an active quote (never turned into an order) with items and an email, untouched for `ABANDONED_AFTER_HOURS` (2) in UTC; a comment with `is_customer_notified` does email the customer (merchant-confirmed). Shopify categories are collections (no nesting); since 2026-07 hand-picked products are `inclusion` selections of the collection's `CollectionConditionsSource`, changed through `collectionUpdate` (`sourcesToUpdate` / `sourcesToCreate`), not the deprecated `collectionAddProducts` / `collectionRemoveProducts`. Deleting a product variant keeps its siblings, and order notes cannot notify the customer. PrestaShop orders are created from a cart, and the webservice only records them as paid when the amount equals the cart total exactly (otherwise "Payment error" emails the customer), so the total is computed from PrestaShop's own tax-inclusive prices with an active free carrier; Spanish addresses need `Address.id_number` (sent as `dni`). Shopify and WooCommerce refuse `id_number`; Magento stores it as `vat_id`.
- Email and marketing tools take a `purpose` or status argument plus `consent_confirmed`; marketing without confirmed consent is refused, never defaulted. Messages go one per recipient so addresses are never exposed to each other. A service other than the shop that cannot be reached is `ConnectorError::Unreachable`, naming the service. SMTP uses `lettre` behind a `Mailer` trait; Klaviyo pins its `revision` header.
- WooCommerce's OAuth base string double-encodes each `key=value` pair (a space becomes `%2520`). Only the live test against the `woo` store proves signing; wiremock does not check signatures.
- Secrets (LLM API keys, consumer secrets) must never reach logs, audit rows or error strings returned to the model.
- LLM API keys are never written to `config.toml`; it only holds defaults (provider, model, base URL, agent limits, skill dirs, DB path). Keys live in the encrypted DB.
- The OS keyring cannot be exercised inside the container. Tests use `InMemoryKeyStore`; the real keyring is a manual host check.

## Open decisions

- **Windows builds and distribution.** macOS builds natively on the host; Windows needs either a CI runner or a cross toolchain. Shareable builds go only to the `tech-ai-tools` S3 bucket.
