use crate::config::{Config, SoundCloudFeedKind};
use crate::metadata::{FeedStatus, IndexJson};

pub fn render_index(config: &Config, index: &IndexJson) -> String {
    let mut feed_rows = String::new();

    for user in &config.users {
        for service in &user.services {
            for feed in &service.feeds {
                let path = feed_path(&user.name, service.kind.as_path(), &service.account, *feed);
                let source_url = feed.source_url(service);
                let title = format!(
                    "{}: {} / {}",
                    service_display_name(service.kind.as_path()),
                    service.account,
                    feed.label()
                );
                let status = index.feeds.iter().find(|candidate| {
                    matches_feed(
                        candidate,
                        &user.name,
                        service.kind.as_path(),
                        &service.account,
                        feed.slug(),
                    )
                });
                let state = status
                    .map(|status| status.metadata_cache.state.as_header_value())
                    .unwrap_or("missing");
                let last_fetched = status
                    .and_then(|status| status.metadata_cache.last_successful_refresh)
                    .map(|timestamp| timestamp.to_rfc3339_opts(chrono::SecondsFormat::Secs, true))
                    .unwrap_or_else(|| "never".to_string());
                let refresh_path = format!("{path}?refresh=1");
                feed_rows.push_str(&format!(
                    r#"<article class="feed">
  <span class="icon" aria-hidden="true">☊</span>
  <span>
    <a class="source" href="{source_url}"><strong>{title}</strong></a>
    <small><a class="source-url" href="{source_url}">{source_url_text}</a></small>
    <small>State: {state} · Last fetched (UTC): {last_fetched}</small>
  </span>
  <span class="actions">
    <a class="rss" href="{path}" aria-label="RSS feed for {title_attr}">RSS</a>
    <a class="rss subtle" href="{refresh_path}" aria-label="Refresh RSS feed for {title_attr}">Refresh</a>
  </span>
</article>"#,
                    path = escape_attr(&path),
                    refresh_path = escape_attr(&refresh_path),
                    source_url = escape_attr(&source_url),
                    source_url_text = escape_html(&source_url),
                    title = escape_html(&title),
                    state = escape_html(state),
                    last_fetched = escape_html(&last_fetched),
                    title_attr = escape_attr(&title)
                ));
            }
        }
    }

    if feed_rows.is_empty() {
        feed_rows.push_str("<p>No feeds are configured.</p>");
    }

    format!(
        r#"<!doctype html>
<html lang="en">
<head>
  <meta charset="utf-8">
  <meta name="viewport" content="width=device-width, initial-scale=1">
  <title>yt-dlp-feed</title>
  <style>
    :root {{ color-scheme: light dark; font-family: ui-sans-serif, system-ui, -apple-system, BlinkMacSystemFont, "Segoe UI", sans-serif; }}
    body {{ margin: 0; background: #f7f8fa; color: #17202a; }}
    main {{ max-width: 820px; margin: 0 auto; padding: 48px 20px; }}
    h1 {{ margin: 0 0 8px; font-size: clamp(2rem, 4vw, 3rem); line-height: 1; }}
    p {{ margin: 0 0 28px; color: #53606d; }}
    .feeds {{ display: grid; gap: 10px; }}
    .feed {{ display: grid; grid-template-columns: 44px 1fr auto; gap: 14px; align-items: center; padding: 14px; color: inherit; border: 1px solid #d9dee5; border-radius: 8px; background: #fff; }}
    .feed:hover {{ border-color: #f47621; box-shadow: 0 8px 22px rgba(23, 32, 42, .08); }}
    .icon {{ display: grid; width: 40px; height: 40px; place-items: center; border-radius: 8px; background: #f47621; color: #fff; font-size: 24px; font-weight: 700; }}
    a {{ color: inherit; }}
    .source {{ text-decoration: none; }}
    .source:hover {{ color: #d95f0e; }}
    .source-url:hover {{ color: #d95f0e; }}
    strong {{ display: block; font-size: 1rem; }}
    small {{ display: block; margin-top: 4px; color: #65717d; overflow-wrap: anywhere; }}
    .actions {{ display: flex; gap: 6px; flex-wrap: wrap; justify-content: end; }}
    .rss {{ padding: 6px 8px; border-radius: 6px; background: #eef2f6; color: #394552; font-weight: 700; font-size: .75rem; text-decoration: none; }}
    .rss.subtle {{ font-weight: 600; }}
    .rss:hover {{ background: #f47621; color: #fff; }}
    @media (prefers-color-scheme: dark) {{
      body {{ background: #111418; color: #edf1f5; }}
      p, small {{ color: #a8b1ba; }}
      .feed {{ background: #181d23; border-color: #303842; }}
      .rss {{ background: #252c34; color: #dbe2e8; }}
    }}
  </style>
</head>
<body>
  <main>
    <h1>yt-dlp-feed</h1>
    <p>Podcast feeds generated from configured yt-dlp service accounts.</p>
    <section class="feeds" aria-label="Available feeds">
      {feed_rows}
    </section>
  </main>
</body>
</html>"#
    )
}

fn matches_feed(status: &FeedStatus, user: &str, service: &str, account: &str, feed: &str) -> bool {
    status.user == user
        && status.service == service
        && status.account == account
        && status.feed == feed
}

pub fn feed_path(user: &str, service: &str, account: &str, feed: SoundCloudFeedKind) -> String {
    format!(
        "/users/{}/{}/{}/{}",
        urlencoding::encode(user),
        urlencoding::encode(service),
        urlencoding::encode(account),
        feed.as_path()
    )
}

fn service_display_name(service: &str) -> &str {
    match service {
        "soundcloud" => "SoundCloud",
        _ => service,
    }
}

fn escape_html(input: &str) -> String {
    input
        .replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
}

fn escape_attr(input: &str) -> String {
    escape_html(input).replace('"', "&quot;")
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::Config;
    use crate::metadata::{IndexJson, IndexSummary};

    #[test]
    fn index_contains_profile_and_likes_links() {
        let html = render_index(
            &Config::default(),
            &IndexJson {
                generated_at: chrono::Utc::now(),
                summary: IndexSummary {
                    feeds_total: 0,
                    feeds_ready: 0,
                    feeds_missing: 0,
                    refreshes_in_progress: 0,
                },
                feeds: vec![],
            },
        );

        assert!(html.contains(&configured_feed_path(
            "dereknet",
            SoundCloudFeedKind::Profile
        )));
        assert!(html.contains(&configured_feed_path("dereknet", SoundCloudFeedKind::Likes)));
        assert!(html.contains(&configured_feed_path("NTS", SoundCloudFeedKind::Profile)));
        assert!(html.contains(&configured_feed_path("NTS", SoundCloudFeedKind::Likes)));
        assert!(html.contains("https://soundcloud.com/dereknet"));
        assert!(html.contains("https://soundcloud.com/dereknet/likes"));
        assert!(html.contains("https://soundcloud.com/user-202286394-991268468"));
        assert!(html.contains("https://soundcloud.com/user-202286394-991268468/likes"));
        assert!(html.contains(
            r#"<a class="source-url" href="https://soundcloud.com/dereknet">https://soundcloud.com/dereknet</a>"#
        ));
        assert!(html.contains("<strong>SoundCloud: NTS / Profile</strong>"));
        assert!(html.contains("<strong>SoundCloud: NTS / Likes</strong>"));
        assert!(!html.contains("derek / soundcloud / NTS Profile"));
        assert!(html.contains("RSS"));
    }

    fn configured_feed_path(account: &str, feed: SoundCloudFeedKind) -> String {
        let config = Config::default();
        let user = &config.users[0];
        let service = config
            .service(&user.name, crate::config::ServiceKind::Soundcloud, account)
            .expect("configured test service");
        feed_path(&user.name, service.kind.as_path(), &service.account, feed)
    }
}
