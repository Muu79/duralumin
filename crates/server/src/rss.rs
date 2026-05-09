use duralumin_core::{Episode, Feed};

use crate::ServerConfig;

pub fn build_rss(feed: &Feed, episodes: &[Episode], config: &ServerConfig, slug: &str) -> String {
    let title = esc(feed.title.as_deref().unwrap_or(slug));
    // Append ?key=<token> to every server URL so podcast apps can authenticate
    // audio fetches without re-sending the RSS auth header.
    let key_suffix = config
        .auth_token
        .as_deref()
        .map(|t| format!("?key={}", t))
        .unwrap_or_default();
    let feed_url = format!("{}rss/{}{}", config.base_url, slug, key_suffix);

    let mut out = String::with_capacity(4096 + episodes.len() * 512);
    out.push_str("<?xml version=\"1.0\" encoding=\"UTF-8\"?>\n");
    out.push_str("<rss version=\"2.0\"");
    out.push_str(" xmlns:itunes=\"http://www.itunes.com/dtds/podcast-1.0.dtd\"");
    out.push_str(" xmlns:content=\"http://purl.org/rss/modules/content/\">\n");
    out.push_str("<channel>\n");
    out.push_str(&format!("  <title>{title}</title>\n"));
    out.push_str(&format!("  <link>{}</link>\n", esc(&feed_url)));
    out.push_str(&format!("  <description>{title}</description>\n"));

    if let Some(img) = &feed.image_url {
        out.push_str("  <image>\n");
        out.push_str(&format!("    <url>{}</url>\n", esc(img.as_str())));
        out.push_str(&format!("    <title>{title}</title>\n"));
        out.push_str(&format!("    <link>{}</link>\n", esc(&feed_url)));
        out.push_str("  </image>\n");
        out.push_str(&format!("  <itunes:image href=\"{}\"/>\n", esc(img.as_str())));
    }

    for ep in episodes {
        out.push_str("  <item>\n");
        out.push_str(&format!("    <title>{}</title>\n", esc(&ep.title)));
        out.push_str(&format!(
            "    <guid isPermaLink=\"false\">{}</guid>\n",
            esc(&ep.guid)
        ));
        out.push_str(&format!(
            "    <pubDate>{}</pubDate>\n",
            ep.pub_date.to_rfc2822()
        ));

        if let Some(desc) = &ep.description {
            out.push_str(&format!(
                "    <description>{}</description>\n",
                esc(desc)
            ));
        }

        let audio_url = format!("{}rss/{}/{}{}", config.base_url, slug, ep.id, key_suffix);
        let mime = ep.enclosure_mime.as_deref().unwrap_or("audio/mpeg");
        let size = ep.enclosure_size.unwrap_or(0);
        out.push_str(&format!(
            "    <enclosure url=\"{}\" length=\"{}\" type=\"{}\"/>\n",
            esc(&audio_url),
            size,
            esc(mime)
        ));

        if let Some(secs) = ep.duration_secs {
            out.push_str(&format!(
                "    <itunes:duration>{}</itunes:duration>\n",
                format_duration(secs)
            ));
        }

        if let Some(img) = &ep.image_url {
            out.push_str(&format!(
                "    <itunes:image href=\"{}\"/>\n",
                esc(img.as_str())
            ));
        }

        out.push_str("  </item>\n");
    }

    out.push_str("</channel>\n</rss>\n");
    out
}

fn esc(s: &str) -> String {
    s.replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
        .replace('"', "&quot;")
}

fn format_duration(secs: u64) -> String {
    let h = secs / 3600;
    let m = (secs % 3600) / 60;
    let s = secs % 60;
    if h > 0 {
        format!("{h}:{m:02}:{s:02}")
    } else {
        format!("{m}:{s:02}")
    }
}
