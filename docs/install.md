# Installing graph-sentinel

The short path: the Docker image beside an existing indexer stack, a config file with one line in
it, then one module at a time. The examples assume the StakeSquid
[graphprotocol-mainnet-docker](https://github.com/StakeSquid/graphprotocol-mainnet-docker) stack;
for any other stack, change the host names.

## What you need

- A host running graph-node, and for the REO meter, indexer-service and tap-agent. graph-sentinel
  only reads: it holds no keys and never signs.
- Docker with the compose plugin.
- For notifications, a Telegram bot or a Discord webhook (both below).
- For the network view, a gateway API key from [Subgraph Studio](https://thegraph.com/studio/apikeys/).
- While the image is private: `docker login ghcr.io` with a GitHub token that has `read:packages`.

## Compose route

```sh
git clone https://github.com/nuthatch-org/graph-sentinel
cd graph-sentinel/deploy/compose
cp graph-sentinel.toml.example graph-sentinel.toml
cp .env.example .env
docker network ls | grep monitor-net    # the stack's network; set STACK_NETWORK in .env if it differs
# edit graph-sentinel.toml and .env, then:
docker compose up -d
docker compose logs -f graph-sentinel
```

The container joins the stack's network, so it reaches `index-node-0`, `postgres` and
`postgres-agent` by name. It reads the config read-only from `./graph-sentinel.toml` and keeps its
state (the REO ledger, findings) in the `sentinel-data` volume. The dashboard is published on
`127.0.0.1:9477` only.

Copy the config before the first `up`. If `./graph-sentinel.toml` does not exist, Docker creates
an empty directory in its place, and graph-sentinel stops at once saying the config path is a
directory.

## Config

Start with liveness alone. This is a complete `graph-sentinel.toml`:

```toml
[sources]
index_node_url = "http://index-node-0:8030/graphql"
```

That watches every deployment for frozen, stalled, stopped and failed states. Then switch on the
rest as you need it. Each module turns on when its sources are present; `graph-sentinel doctor`
says which are off and why.

**RPC per chain.** Lets liveness tell a frozen deployment from one whose contracts are
quiet, and lets the REO meter read the oracle contract. Names as in graph-node's `config.toml`.

```toml
[sources.rpc]
"arbitrum-one" = "https://arb1.arbitrum.io/rpc"
"mainnet" = "https://your-mainnet-rpc"
```

**REO meter.** Counts the deployments you serve each day against the Rewards Eligibility Oracle's
minimum. Needs your indexer address and read access to tap-agent's database (`postgres-agent` in
the StakeSquid stack). Put the URL in `.env`, not the file:

```toml
[sources]
indexer_address = "0xYourIndexerAddress"
```

```sh
# .env
GRAPH_SENTINEL_SOURCES__INDEXER_POSTGRES_URL=postgres://sentinel_ro:PASSWORD@postgres-agent:5432/AGENT_DB_NAME
```

tap-agent deletes receipts once a RAV covers them, so the meter's history starts when
graph-sentinel does. Leave it running.

**Payment checks.** On as soon as the REO meter's database URL is set. They catch a TAP
aggregator that stops answering as gRPC (the Cloudflare outage of September), receipts from a
sender your config has no endpoint for (a rotation), a sender on tap-agent's denylist,
unaggregated fees nearing `max_amount_willing_to_lose_grt`, stalled RAV requests and low payer
escrow. Mount indexer-rs's config.toml (the commented line in `docker-compose.yml`) and point at it,
so senders, limits and the collector address can be checked too:

```toml
[tap]
indexer_rs_config = "/etc/graph-sentinel/indexer-rs.toml"
```

Escrow is read on chain, so it also needs the `arbitrum-one` RPC above.

**Network view.** Allocations, POI deadlines, delegation and disputes against you, from the
network subgraph. Needs `indexer_address` and a gateway key:

```toml
[sources]
network_subgraph_token_env = "GRAPH_API_KEY"
```

```sh
# .env
GRAPH_API_KEY=your-gateway-key
```

To watch disputes you filed as a fisherman, add `[network] fisherman_address = "0x..."`.

**RPC spend guard.** Prices graph-node's RPC calls per provider against a daily budget, from its
metrics on port 8040. Warns at 70% and raises a critical finding at 100%.

```toml
[spend]
graph_node_metrics_url = "http://index-node-0:8040/metrics"
daily_budget_usd = 20
billed_providers = ["alchemy"]   # provider labels from graph-node's config.toml
```

`stop_command` and `resume_command` run inside the container, which has no Docker CLI, so leave
them unset under compose.

Any key can be set from the environment as `GRAPH_SENTINEL_SECTION__KEY`, for example
`GRAPH_SENTINEL_SOURCES__INDEX_NODE_URL`. The environment wins over the file.

### Read-only Postgres role

On tap-agent's database (`docker exec -it postgres-agent psql -U <AGENT_DB_USER> <AGENT_DB_NAME>`):

```sql
CREATE ROLE sentinel_ro LOGIN PASSWORD 'choose-one';
GRANT CONNECT ON DATABASE "<AGENT_DB_NAME>" TO sentinel_ro;
GRANT USAGE ON SCHEMA public TO sentinel_ro;
GRANT SELECT ON tap_horizon_receipts, tap_horizon_ravs, tap_horizon_denylist TO sentinel_ro;
-- Only if the legacy TAP tables exist:
GRANT SELECT ON scalar_tap_receipts, scalar_tap_ravs TO sentinel_ro;
```

The meter also listens on tap-agent's notification channel, which needs no grant.

graph-node's database is optional (`postgres_url`; it adds local columns to the candidates list).
On the `postgres` container, after creating the same role there:

```sql
GRANT CONNECT ON DATABASE "<GRAPH_NODE_DB_NAME>" TO sentinel_ro;
GRANT USAGE ON SCHEMA public, subgraphs TO sentinel_ro;
GRANT SELECT ON public.chains, public.deployment_schemas TO sentinel_ro;
GRANT SELECT ON ALL TABLES IN SCHEMA subgraphs TO sentinel_ro;
```

## Telegram

1. In Telegram, message [@BotFather](https://t.me/BotFather), send `/newbot`, and keep the token
   it gives you. Put it in `.env` as `TELEGRAM_BOT_TOKEN`.
2. Send your new bot any message (or add it to a group and post there), then read the chat id:
   ```sh
   curl -s "https://api.telegram.org/bot$TELEGRAM_BOT_TOKEN/getUpdates" | grep -oE '"chat":\{"id":-?[0-9]+'
   ```
   Group ids are negative.
3. Add the channel to `graph-sentinel.toml`:
   ```toml
   [[outputs.notify]]
   kind = "telegram"
   bot_token_env = "TELEGRAM_BOT_TOKEN"
   chat_id = "123456789"
   ```

On start graph-sentinel posts a short "watching" message to every channel, so you know the wiring
works. After that it posts only when a finding appears, escalates or clears.

## Discord

In the Discord channel: Edit Channel, Integrations, Webhooks, New Webhook, Copy Webhook URL. Put
the URL in `.env` as `DISCORD_WEBHOOK_URL` and add:

```toml
[[outputs.notify]]
kind = "discord"
url_env = "DISCORD_WEBHOOK_URL"
min_severity = "critical"   # info, warn or critical; default warn
```

Slack (`kind = "slack"`) and generic JSON (`kind = "generic"`, for Alertmanager and the like) take
`url_env` the same way.

## The dashboard

The page, `/api/snapshot` and `/metrics` are served on port 9477. The page has no login, so the
compose file publishes it on `127.0.0.1` only. From your own machine:

```sh
ssh -N -L 9477:127.0.0.1:9477 you@indexer-host
```

then open <http://localhost:9477>. To reach it another way, put it behind your own reverse proxy
with authentication; the proxy can reach it on the stack network as `graph-sentinel:9477`. Set
`[outputs] public_url` to wherever you open it, and every notification links there.

Prometheus can scrape `graph-sentinel:9477/metrics` from the same network.

## Doctor

```sh
docker compose exec graph-sentinel graph-sentinel doctor
```

It tests every configured source and prints one line each: `ok`, `FAIL` with the reason, or
`skip` with the setting that would turn it on. It exits non-zero if anything failed. Run it after
every config change.

## Upgrading

Pin a version in `.env` (`SENTINEL_VERSION=0.1.0`), then:

```sh
docker compose pull && docker compose up -d
```

State lives in the `sentinel-data` volume and survives upgrades. Do not remove that volume: the
REO ledger cannot be rebuilt from tap-agent's database.

## Other routes

**Kubernetes.** A chart is in `deploy/helm/graph-sentinel`. Put the TOML in `config`, secrets in a
Secret referenced from `envFrom`, and keep `persistence.enabled` on.

```sh
helm install graph-sentinel deploy/helm/graph-sentinel -f my-values.yaml
kubectl port-forward svc/graph-sentinel 9477:9477
```

**Binary.** Each release carries static Linux binaries for x86_64 and aarch64. Unpack, then run
`graph-sentinel` beside its config (or pass `--config path`). It writes `graph-sentinel.sqlite`
in the working directory unless `[outputs] state_db` says otherwise.
