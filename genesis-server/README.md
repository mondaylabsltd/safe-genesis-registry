# genesis-server — the docker shell

The same service as [`genesis-cf`](../genesis-cf), for self-hosting: the same
routes, byte-for-byte the same response bodies (both come from
`genesis_rules::view`), the same cache policy and alert rules. One container,
one volume, nothing else to run.

| genesis-cf (Cloudflare) | genesis-server (docker) |
| --- | --- |
| stateless Worker | Axum |
| Durable Object SQLite: tasks, alerts, bookkeeping | a SQLite file on the `/data` volume, same schema |
| Durable Object alarm (submission pass) | a tokio loop every 15 s, woken at once by an import |
| cron trigger (maintenance and alerts) | a tokio loop every 60 s |
| platform Cache API | an in-memory cache, same keys and TTLs |

## Run

```shell
cp .env.example .env            # at the repo root: REGISTRY_ADDRESS, optionally PRIVATE_KEY, TELEGRAM_*
docker compose up -d            # builds the image from the repo's Dockerfile
curl localhost:8080/api/health
```

Behind an HTTP proxy, set `HTTPS_PROXY` (and `NO_PROXY` for local RPC
endpoints) in `.env`: the service's outbound calls honor them. Pulling the
base images is the docker daemon's business, configured in Docker Desktop
(Settings → Resources → Proxies), not here.

Without docker: `cargo run --release -p genesis-server` with the same
variables in the environment (`DATABASE_PATH` defaults to
`/data/genesis.sqlite`, `LISTEN_ADDR` to `0.0.0.0:8080`).

The API, the configuration variables and what was tested end to end are
described in [genesis-cf's README](../genesis-cf/README.md); both shells were
run side by side against the same forks and answered every lookup
identically.

The image was built and run end to end on Docker Desktop (arm64, Debian 12
— the base image's release): a Lido multisig imported from Base's transaction
service was recorded on a Gnosis fork in one broadcast, read back verified,
and the task survived a container restart on its volume; the service runs as
`nobody`.
