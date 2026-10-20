//! Short artist biographies from Wikipedia's REST summary API (no key needed).

use super::http_client;

pub struct Summary {
    pub text: String,
    pub url: String,
}

fn summary(title: &str) -> Option<serde_json::Value> {
    let url = format!(
        "https://en.wikipedia.org/api/rest_v1/page/summary/{}",
        percent_encoding::utf8_percent_encode(&title.replace(' ', "_"), percent_encoding::NON_ALPHANUMERIC)
    );
    let resp = http_client().get(url).send().ok()?;
    if !resp.status().is_success() {
        return None;
    }
    resp.json().ok()
}

fn looks_like_music(v: &serde_json::Value) -> bool {
    let text = format!("{} {}", v["description"].as_str().unwrap_or(""), v["extract"].as_str().unwrap_or("")).to_lowercase();
    [
        "band", "singer", "musician", "rapper", "songwriter", "dj", "producer", "composer", "group", "duo", "music",
        "vocalist", "guitarist",
    ]
    .iter()
    .any(|w| text.contains(w))
}

/// Find a music-related article for `name`, trying "(band)" / "(musician)" / "(singer)" first.
pub fn artist_summary(name: &str) -> Option<Summary> {
    for title in [format!("{name} (band)"), format!("{name} (musician)"), format!("{name} (singer)"), format!("{name} (rapper)"), name.to_string()] {
        let Some(v) = summary(&title) else { continue };
        if v["type"].as_str() == Some("disambiguation") || !looks_like_music(&v) {
            continue;
        }
        let text = v["extract"].as_str().unwrap_or("").trim().to_string();
        if text.is_empty() {
            continue;
        }
        let url = v["content_urls"]["desktop"]["page"].as_str().unwrap_or("").to_string();
        return Some(Summary { text, url });
    }
    None
}
