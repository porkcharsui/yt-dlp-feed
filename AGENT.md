# Agent Guide

This repository implements `yt-dlp-rss`, a Rust HTTP server that creates podcast-style RSS feeds backed by just-in-time yt-dlp audio downloads.

## Project Intent

- Read a small YAML config with users and service accounts.
- Expose RSS feeds for configured accounts.
- Serve stable media URLs that download audio on first request.
- Keep downloaded media temporarily because clients are expected to cache it.
- Provide a simple `/` index page with RSS links and lightweight styling.
- Keep v1 SoundCloud-focused while leaving room for additional yt-dlp-supported services.

## Architecture

- Use Rust with `tokio` and `axum`.
- Keep config in `src/config.rs`.
- Keep route handlers in `src/routes.rs`.
- Keep server-rendered HTML in `src/html.rs`.
- Keep RSS generation in `src/rss_feed.rs`.
- Keep yt-dlp and media cache behavior behind the `MediaBackend` trait in `src/media.rs`.
- Normal tests should use mock `MediaBackend` implementations instead of calling live services.

## yt-dlp Integration

Use the Rust [`boul2gom/yt-dlp`](https://github.com/boul2gom/yt-dlp) library for yt-dlp access. Prefer library APIs over shelling out. The implementation should:

- Fetch SoundCloud profile metadata from `https://soundcloud.com/{account}`.
- Fetch SoundCloud likes metadata from `https://soundcloud.com/{account}/likes`.
- Prefer best available AAC audio.
- Output `.m4a` files served as `audio/mp4`.
- Use disk-backed metadata/cache support where the library version supports it cleanly.

## Config Defaults

If `config.yaml` is absent, the server should still start with:

- bind address `127.0.0.1:8080`
- data directory `./data`
- media TTL `86400` seconds
- auth disabled
- user `derek`
- SoundCloud account `dereknet`
- profile and likes feeds enabled

## HTTP Interface

- `GET /`
- `GET /healthz`
- `GET /users/{user}/soundcloud/{account}/feed.xml`
- `GET /users/{user}/soundcloud/{account}/likes.xml`
- `GET /users/{user}/soundcloud/{account}/items/{item_id}/audio.m4a`

Route URLs should stay stable because RSS clients cache enclosure URLs.

## Testing Expectations

Cover these behaviors with unit or integration tests:

- YAML config parsing and defaults.
- Index page includes configured feed links.
- RSS output includes stable enclosure URLs.
- Optional Basic auth accepts valid credentials and rejects invalid requests.
- Cache keys and media paths are stable.
- Concurrent requests for the same item share one in-flight download.
- Expired media files are removed by cleanup.

Live yt-dlp/SoundCloud tests should be ignored by default or feature-gated.

## Implementation Notes

- Do not build a frontend app for the index page; server-rendered HTML is enough.
- Keep built-in auth deliberately simple. Trusted LAN or auth proxy deployment is the primary model.
- Avoid service-specific assumptions outside service modules or the media boundary.
- Do not make docs claim broad service support beyond what is implemented. Link users to the official [yt-dlp supported sites](https://github.com/yt-dlp/yt-dlp/blob/master/supportedsites.md) list for extractor coverage.
