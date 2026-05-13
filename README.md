# yt-dlp-feed

`yt-dlp-feed` is a Rust HTTP server that turns configured yt-dlp-supported accounts into podcast-style RSS feeds. Feed items point at stable server URLs that perform just-in-time downloads with `yt-dlp`, stream the audio back to the client, and keep the downloaded media only temporarily.

The first target service is SoundCloud. The default config exposes the main profile and likes feeds for Derek's SoundCloud account, [`dereknet`](https://soundcloud.com/dereknet).

## Status

This repository contains the first Rust implementation scaffold: config loading, route wiring, RSS rendering, a simple index page, temporary media cache behavior, optional HTTP Basic auth, and a mockable yt-dlp boundary. Live service behavior depends on yt-dlp, ffmpeg, and the supported extractor behavior for each service.

## Supported Services

The server is designed around yt-dlp-compatible sources. v1 implements SoundCloud account feeds, and future services should use the same internal service boundary.

For the broader list of services that yt-dlp may support, see the canonical yt-dlp documentation:

- [yt-dlp supported sites](https://github.com/yt-dlp/yt-dlp/blob/master/supportedsites.md)

## Configuration

By default, the server looks for `config.yaml`. If no config exists, it uses the built-in Derek/SoundCloud defaults. You can also pass a path with `--config` or `YT_DLP_FEED_CONFIG`.

```yaml
server:
  bind: "127.0.0.1:8080"

cache:
  data_dir: "./data"
  media_ttl_seconds: 86400
  disconnect_behavior: "delay_cancel"
  disconnect_grace_seconds: 15

auth:
  enabled: false

users:
  - name: "derek"
    services:
      - kind: "soundcloud"
        account: "dereknet"
        profile_url: "https://soundcloud.com/dereknet"
        feeds:
          - profile
          - likes
```

## Routes

- `GET /` serves a simple HTML index page with direct RSS links.
- `GET /healthz` returns `ok`.
- `GET /users/{user}/soundcloud/{account}/feed.xml` serves the account profile feed.
- `GET /users/{user}/soundcloud/{account}/likes.xml` serves the account likes feed.
- `GET /users/{user}/soundcloud/{account}/items/{item_id}/audio.m4a` downloads or serves cached AAC/M4A audio for a feed item.

## Download And Cache Behavior

The server fetches feed metadata through the Rust [`yt-dlp`](https://github.com/boul2gom/yt-dlp) library. Audio downloads prefer the best available AAC stream and serve `audio/mp4` from `.m4a` URLs. If cached media exists and is still inside `media_ttl_seconds`, the server serves it directly. Otherwise, the first client request starts a new download, and concurrent requests for the same item share the same in-flight job.

If every client disconnects while a download is still in flight, `cache.disconnect_behavior` controls whether the server keeps or cancels the orphaned download:

- `continue` keeps downloading and caches the completed file.
- `cancel` stops `yt-dlp` immediately and removes the partial `.download.m4a`.
- `delay_cancel` waits `disconnect_grace_seconds` for a reconnect, then cancels if no client is attached.

Completed media files live under `cache.data_dir` and are cleaned up after the configured TTL. This is intentional: clients are expected to cache media after the first successful fetch.

## Security

The default deployment model is a trusted network, such as a LAN, or deployment behind an auth proxy. Built-in auth is intentionally simple. If `auth.enabled` is true, configure HTTP Basic auth credentials in `config.yaml`:

```yaml
auth:
  enabled: true
  username: "derek"
  password: "change-me"
```

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
