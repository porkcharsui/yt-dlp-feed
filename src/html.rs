use crate::config::{Config, FeedKind};

pub fn render_index(config: &Config) -> String {
    let mut feed_rows = String::new();

    for user in &config.users {
        for service in &user.services {
            for feed in &service.feeds {
                let path = feed_path(&user.name, service.kind.as_path(), &service.account, *feed);
                let title = format!(
                    "{} / {} / {} {}",
                    user.name,
                    service.kind.as_path(),
                    service.account,
                    feed.label()
                );
                feed_rows.push_str(&format!(
                    r#"<a class="feed" href="{path}">
  <span class="icon" aria-hidden="true">☊</span>
  <span>
    <strong>{title}</strong>
    <small>{path}</small>
  </span>
  <span class="rss" aria-label="RSS feed">RSS</span>
</a>"#,
                    path = escape_attr(&path),
                    title = escape_html(&title)
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
  <title>yt-dlp-rss</title>
  <style>
    :root {{ color-scheme: light dark; font-family: ui-sans-serif, system-ui, -apple-system, BlinkMacSystemFont, "Segoe UI", sans-serif; }}
    body {{ margin: 0; background: #f7f8fa; color: #17202a; }}
    main {{ max-width: 820px; margin: 0 auto; padding: 48px 20px; }}
    h1 {{ margin: 0 0 8px; font-size: clamp(2rem, 4vw, 3rem); line-height: 1; }}
    p {{ margin: 0 0 28px; color: #53606d; }}
    .feeds {{ display: grid; gap: 10px; }}
    .feed {{ display: grid; grid-template-columns: 44px 1fr auto; gap: 14px; align-items: center; padding: 14px; color: inherit; text-decoration: none; border: 1px solid #d9dee5; border-radius: 8px; background: #fff; }}
    .feed:hover {{ border-color: #f47621; box-shadow: 0 8px 22px rgba(23, 32, 42, .08); }}
    .icon {{ display: grid; width: 40px; height: 40px; place-items: center; border-radius: 8px; background: #f47621; color: #fff; font-size: 24px; font-weight: 700; }}
    strong {{ display: block; font-size: 1rem; }}
    small {{ display: block; margin-top: 4px; color: #65717d; overflow-wrap: anywhere; }}
    .rss {{ padding: 6px 8px; border-radius: 6px; background: #eef2f6; color: #394552; font-weight: 700; font-size: .75rem; }}
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
    <h1>yt-dlp-rss</h1>
    <p>Podcast feeds generated from configured yt-dlp service accounts.</p>
    <section class="feeds" aria-label="Available feeds">
      {feed_rows}
    </section>
  </main>
</body>
</html>"#
    )
}

pub fn feed_path(user: &str, service: &str, account: &str, feed: FeedKind) -> String {
    format!(
        "/users/{}/{}/{}/{}",
        urlencoding::encode(user),
        urlencoding::encode(service),
        urlencoding::encode(account),
        feed.as_path()
    )
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

    #[test]
    fn index_contains_profile_and_likes_links() {
        let html = render_index(&Config::default());

        assert!(html.contains("/users/derek/soundcloud/dereknet/feed.xml"));
        assert!(html.contains("/users/derek/soundcloud/dereknet/likes.xml"));
        assert!(html.contains("RSS"));
    }
}
