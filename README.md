# yt-dlp-feed

`yt-dlp-feed` is a Rust HTTP server that turns configured yt-dlp-supported sources into podcast-style RSS feeds. Feed items point at stable server URLs that perform just-in-time downloads with `yt-dlp`, stream the audio back to the client, and keep the downloaded media only temporarily.

The first target service is SoundCloud. A configured SoundCloud source can expose a profile feed, a likes feed, or both.

## Status

This repository contains the first Rust implementation scaffold: config loading, route wiring, RSS rendering, a simple index page, temporary media cache behavior, optional HTTP Basic auth, and a mockable yt-dlp boundary. Live service behavior depends on yt-dlp, ffmpeg, and the supported extractor behavior for each service.

## Supported Services

The server is designed around yt-dlp-compatible sources. V1 implements SoundCloud profile and likes feeds; other services can be added later by teaching the app how to fetch their feed metadata and download their media.

For the broader list of services that yt-dlp may support, see the canonical yt-dlp documentation:

- [yt-dlp supported sites](https://github.com/yt-dlp/yt-dlp/blob/master/supportedsites.md)

## Configuration

By default, the server looks for `config.yaml`. If no config exists, it uses built-in example SoundCloud defaults. You can also pass a path with `--config` or `YT_DLP_FEED_CONFIG`, and override `cache.data_dir` with `--data-dir` or `YT_DLP_FEED_DATA_DIR`.

```yaml
server:
  bind: "127.0.0.1:8080"

cache:
  data_dir: "./data"
  media_ttl_minutes: 360
  media_max_megabytes: 10240
  disconnect_behavior: "delay_cancel"
  disconnect_grace_seconds: 15

metadata:
  refresh_interval_hours: 24
  refresh_recent_grace_minutes: 60

downloads:
  max_concurrent: 3
  probe_timeout_seconds: 300

auth:
  enabled: false

users:
  - name: "operator"
    services:
      - kind: "soundcloud"
        name: "primary"
        profile_url: "https://soundcloud.com/example-profile"
        feeds:
          - profile
          - likes
```

## Routes

- `GET /` serves a simple HTML index page with direct RSS links.
- `GET /index.json` serves the same configured feed/status data in structured JSON.
- `GET /healthz` returns `ok`.
- `GET /readyz` returns `ready` only after every configured feed has metadata cached at least once.
- `GET /users/{user}/soundcloud/{name}/feed.xml` serves the profile feed.
- `GET /users/{user}/soundcloud/{name}/likes.xml` serves the likes feed.
- `GET /users/{user}/soundcloud/{name}/items/{item_id}/audio.m4a` downloads or serves cached AAC/M4A audio for a feed item.

## Cache Design

The app keeps two separate caches:

- Feed metadata is stored as app-owned JSON under `cache.data_dir/feed-metadata`.
- Downloaded media is stored temporarily under `cache.data_dir/media`.

Metadata cache files are the source of truth for RSS rendering. Each file stores schema version `1`, the feed identity, the source URL, the last successful refresh timestamp, and the normalized item list in the same order returned by SoundCloud/yt-dlp. The app does not sort or deduplicate feed items.

On startup, the server begins warming any configured feed that has no metadata cache yet. Startup does not block on SoundCloud. If a feed is requested before its first successful metadata fetch, it returns `503 Service Unavailable` with a message that the metadata cache is warming.

Scheduled metadata refresh runs every `metadata.refresh_interval_hours`, defaulting to 24 hours. Scheduled work skips feeds refreshed within `metadata.refresh_recent_grace_minutes`, defaulting to 60 minutes. Metadata refreshes run one at a time globally, with manual refreshes taking priority over queued scheduled work. Logs include metadata refresh queue size when work is queued, started, completed, or fails.

Append `?refresh=1` to an RSS feed URL to manually refresh that feed. Manual refresh waits for the fetch to finish when no refresh is already running. If another refresh for that feed is already running, the response serves the current cache when available and includes `X-Yt-Dlp-Feed-Refresh: refreshing`; if no cache exists yet, it returns `503` with `X-Yt-Dlp-Feed-Refresh: warming`. Successful manual refresh responses include `X-Yt-Dlp-Feed-Refresh: ready`; failed refreshes with an existing cache serve stale metadata with `X-Yt-Dlp-Feed-Refresh: stale`.

Metadata refreshes are atomic: the old cache remains in service until a new fetch succeeds and the JSON file is written with a temp-file-then-rename swap. If SoundCloud or yt-dlp fails and cached metadata exists, RSS continues to render from the last successful metadata. Last refresh errors are kept in memory for `index.json` and logs, not persisted to metadata cache files.

RSS responses set `Last-Modified` and `<lastBuildDate>` from the metadata cache's last successful refresh timestamp. Normal feed requests honor `If-Modified-Since` and may return `304 Not Modified`; explicit `?refresh=1` requests always evaluate the refresh path.

`GET /index.json` includes `generated_at`, a summary, and one entry per configured feed with metadata cache state: `missing`, `warming`, `ready`, `refreshing`, `stale`, or `error`. The HTML index keeps things simple: RSS links, quiet per-feed refresh links, state, and `Last fetched (UTC)` timestamps.

`GET /readyz` returns `200 OK` only when every configured feed has at least one successful metadata cache. `GET /healthz` remains a process-alive check and is the endpoint used by the Docker healthcheck.

## Download Behavior

Audio downloads prefer the best available M4A/AAC stream, falling back to yt-dlp's best available audio format when M4A is unavailable. yt-dlp fetches the source audio and ffmpeg transcodes the streamed bytes to AAC in an MP4/M4A container, so the app serves cached media consistently from `.m4a` URLs. If cached media exists and is still inside `media_ttl_minutes`, the server serves it directly. If `media_ttl_minutes` is `null`, existing cached media is considered reusable until another cleanup limit removes it. Otherwise, the first client request starts a new download, and concurrent requests for the same item share the same in-flight job.

No more than `downloads.max_concurrent` media downloads can run at once. The default is `3`. Requests joining an existing in-flight item do not count as new downloads. If the limit is reached for a new item, the server returns `503 Service Unavailable` with `Retry-After: 30`.

Completed cached media supports `HEAD`, `If-Modified-Since`, and byte `Range` requests, including `206 Partial Content` and `416 Range Not Satisfiable`. On a cold cache, plain `GET` requests stream a live fragmented MP4/AAC response as `200 OK` so browsers can begin playback while the download is still running. Cold `HEAD` and byte `Range` probes start or join the download and wait up to `downloads.probe_timeout_seconds` for the completed cache file, then return normal cached-file headers. If the probe wait times out, the response is `503 Service Unavailable` with `Retry-After: 30`.

This behavior targets private Apple Podcasts app use over a private network or Tailscale URL. Apple expects episode enclosures to support `HEAD` and byte-range requests, and it may probe audio before playback. Pocket Casts is explicitly unsupported for tailnet-only feeds because it performs server-side processing of public feeds; its backend cannot reliably fetch private Tailscale URLs.

If every client disconnects while a download is still in flight, `cache.disconnect_behavior` controls whether the server keeps or cancels the orphaned download:

- `continue` keeps downloading and caches the completed file.
- `cancel` stops `yt-dlp` immediately and removes the partial `.download.m4a`.
- `delay_cancel` waits `disconnect_grace_seconds` for a reconnect, then cancels if no client is attached.

Completed media files live under `cache.data_dir` and are cleaned up by the background cache cleaner every five minutes. This is intentional: clients are expected to cache media after the first successful fetch.

Cache cleanup supports either or both of these limits:

- `media_ttl_minutes` removes completed `.m4a` files older than the configured age. The default is `360`, or 6 hours. Set it to `null` to disable age-based cleanup.
- `media_max_megabytes` keeps completed `.m4a` files under the configured total size, measured in MiB, by deleting the oldest files first. For example, `10240` allows about 10 GiB. Set it to `null` or omit it to disable size-based cleanup.

When both limits are configured, TTL cleanup runs first, then the remaining completed media files are trimmed to `media_max_megabytes`. In-progress `.download.m4a` files are not counted against the size limit.

## Security

The V1 security model is intentionally small and explicit. The server is meant
for a single trusted operator on a LAN, private network, VPN, or behind a real
access-control layer such as Tailscale or an auth proxy. It is not designed as a
public multi-tenant service.

The `users` section in the config is still meaningful even for a single
operator. A configured user is the owner of one or more feed sources, and that
user name is part of every feed URL:

```text
/users/{user}/soundcloud/{name}/feed.xml
```

For example, this config says that the authenticated app user `operator` owns a
SoundCloud source labelled `primary`:

```yaml
users:
  - name: "operator"
    services:
      - kind: "soundcloud"
        name: "primary"
        profile_url: "https://soundcloud.com/example-profile"
```

Built-in auth uses one HTTP Basic username/password pair. If `auth.enabled` is
true, the Basic auth username is treated as the configured app user name. A user
authenticated as `operator` can access `/users/operator/...`; requests for another
configured user path return `403 Forbidden`. The HTML index and `index.json`
are also filtered to the authenticated user.

```yaml
auth:
  enabled: true
  username: "operator"
  password: "change-me"
```

If `auth.enabled` is false, the app trusts the surrounding network or proxy and
does not enforce per-user access. In that mode, anyone who can reach the server
can see the index and request any configured feed URL. The app logs a warning
when auth is disabled while binding to `0.0.0.0`.

V1 deliberately does not include browser sessions, OAuth, per-user password
lists, admin screens, or multi-user isolation. Add those only if the deployment
model changes from "single trusted operator" to "shared service."

## Development

This repo includes a Nix dev shell with Rust tooling:

```sh
nix develop
cargo test
cargo run -- --config config.example.yaml
```

To see request traces and yt-dlp wrapper activity in the console, run with debug logging:

```sh
cargo run -- --debug
```

You can also use `YT_DLP_FEED_DEBUG=true` for the same default debug filter, or set
`RUST_LOG` directly for custom filtering.

Normal tests should mock the yt-dlp boundary. Live SoundCloud tests should be opt-in so CI does not depend on network access or service availability.

## Docker

Build the image:

```sh
docker build -t yt-dlp-feed .
```

The image installs `yt-dlp` from PyPI at build time so it gets the current
upstream extractor code instead of the older Debian package. To force Docker to
fetch a fresh `yt-dlp` version, rebuild without cache:

```sh
docker build --no-cache -t yt-dlp-feed .
```

Run the container with the tracked example config, a persistent Docker volume for
media and metadata, and debug logging enabled:

```sh
docker run --rm \
  -p 8080:8080 \
  -v "$PWD/config.example.yaml:/config/config.yaml:ro" \
  -v yt-dlp-feed-data:/data \
  yt-dlp-feed --config /config/config.yaml --data-dir /data --debug
```

The image includes the latest build-time `yt-dlp` and Debian `ffmpeg`.
`--data-dir /data` overrides `cache.data_dir` from the mounted config so cache
files land on the `yt-dlp-feed-data` Docker volume.

For personal feed lists or secrets, copy the example to the ignored root
`config.yaml`, edit it, and mount that file instead:

```sh
cp config.example.yaml config.yaml
docker run --rm \
  -p 8080:8080 \
  -v "$PWD/config.yaml:/config/config.yaml:ro" \
  -v yt-dlp-feed-data:/data \
  yt-dlp-feed --config /config/config.yaml --data-dir /data --debug
```

Plain `docker run -p 8080:8080` requires `server.bind: "0.0.0.0:8080"` in the
mounted config. The Tailscale Compose setup should use `127.0.0.1:8080` so raw
HTTP is only reachable inside the sidecar network namespace.

## Docker Compose With Tailscale

`compose.yaml` runs the app behind a Tailscale sidecar. The app shares the
sidecar network namespace and listens only on `127.0.0.1:8080`; Tailscale Serve
accepts tailnet HTTP on port 80 and HTTPS on port 443, then proxies both to the
app's internal port 8080. Tailscale ACLs provide the tailnet access control
layer, so the app's built-in Basic auth stays disabled in the provided container
config.

Create the ignored runtime config from the tracked template. For Compose, set
`server.bind: "127.0.0.1:8080"` so the app only listens inside the shared
Tailscale network namespace:

```sh
cp config.example.yaml config.yaml
```

Then edit `config.yaml` for your private feed list and Compose bind address.

Create a reusable or ephemeral auth key in Tailscale, then start the stack:

```sh
export TS_AUTHKEY="tskey-auth-..."
docker compose up -d --build
```

Compose mounts the ignored root `config.yaml` into the app container at
`/config/config.yaml` and passes `--data-dir /data` so media and metadata live
on the Docker volume.

Optional environment variables:

- `TAILSCALE_HOSTNAME=yt-dlp-feed` changes the MagicDNS machine name.
- `TS_EXTRA_ARGS=--advertise-tags=tag:container` is useful when authenticating
  with an OAuth client secret or a tagged auth key.
- `RUST_LOG=yt_dlp_feed=debug,yt_dlp=debug,tower_http=debug` enables verbose
  server logs.

The Serve config lives at `docker/tailscale/serve.json` and uses
`${TS_CERT_DOMAIN}` so Tailscale fills in the node's DNS name. It maps both
`${TS_CERT_DOMAIN}:80` and `${TS_CERT_DOMAIN}:443` to
`http://127.0.0.1:8080`. It keeps `AllowFunnel` set to `false`, which exposes
the service only to your tailnet. Set that value to `true` only if you
intentionally want public Funnel ingress and your tailnet policy allows it.
