//! Spotify Web API (client-credentials flow, no user login): artist profiles and artwork.
//! Create an app at https://developer.spotify.com/dashboard to get a client id/secret.

use super::http_client;
use crate::paths;
use anyhow::{Context, Result, bail};
use std::sync::Mutex;
use std::time::{Duration, Instant};

#[derive(Clone)]
pub struct Credentials {
    pub id: String,
    pub secret: String,
}

impl Credentials {
    pub fn is_set(&self) -> bool {
        !self.id.trim().is_empty() && !self.secret.trim().is_empty()
    }
}

#[derive(Clone, Debug)]
pub struct ArtistInfo {
    pub id: String,
    pub name: String,
    pub image_url: Option<String>,
    pub thumb_url: Option<String>,
    pub genres: Vec<String>,
    pub followers: i64,
    pub url: String,
}

#[derive(Clone, Debug)]
pub struct TrackMatch {
    pub title: String,
    pub artists: String,
    pub album: String,
    pub image_url: Option<String>,
    pub thumb_url: Option<String>,
}

static TOKEN: Mutex<Option<(String, String, Instant)>> = Mutex::new(None);

fn token(c: &Credentials) -> Result<String> {
    if !c.is_set() {
        bail!("Spotify client id/secret not set (Settings → Artist info & artwork)");
    }
    if let Some((id, tok, until)) = TOKEN.lock().unwrap().as_ref() {
        if *id == c.id && Instant::now() < *until {
            return Ok(tok.clone());
        }
    }
    let resp: serde_json::Value = http_client()
        .post("https://accounts.spotify.com/api/token")
        .basic_auth(c.id.trim(), Some(c.secret.trim()))
        .form(&[("grant_type", "client_credentials")])
        .send()
        .context("cannot reach Spotify")?
        .json()?;
    let tok = resp["access_token"].as_str().map(|s| s.to_string()).with_context(|| {
        format!("Spotify auth failed: {}", resp["error_description"].as_str().unwrap_or("check client id/secret"))
    })?;
    let ttl = resp["expires_in"].as_u64().unwrap_or(3600).saturating_sub(60);
    *TOKEN.lock().unwrap() = Some((c.id.clone(), tok.clone(), Instant::now() + Duration::from_secs(ttl)));
    Ok(tok)
}

/// GET a Web API endpoint, waiting and retrying once when rate limited.
fn api(c: &Credentials, url: &str, query: &[(&str, &str)]) -> Result<serde_json::Value> {
    let client = http_client();
    for attempt in 0..3 {
        let resp = client.get(url).bearer_auth(token(c)?).query(query).send().context("cannot reach Spotify")?;
        match resp.status().as_u16() {
            200 => return Ok(resp.json()?),
            429 if attempt < 2 => {
                let wait = resp
                    .headers()
                    .get("retry-after")
                    .and_then(|v| v.to_str().ok())
                    .and_then(|v| v.parse::<u64>().ok())
                    .unwrap_or(2);
                std::thread::sleep(Duration::from_secs(wait.min(30)));
            }
            401 if attempt < 2 => {
                *TOKEN.lock().unwrap() = None;
            }
            s => bail!("Spotify returned status {s}"),
        }
    }
    bail!("Spotify is rate limiting requests, try again later")
}

/// Largest and smallest (>= 64 px) image of a Spotify `images` array.
fn images(v: &serde_json::Value) -> (Option<String>, Option<String>) {
    let Some(arr) = v.as_array() else { return (None, None) };
    let width = |i: &serde_json::Value| i["width"].as_u64().unwrap_or(0);
    let large = arr.iter().max_by_key(|i| width(i)).and_then(|i| i["url"].as_str()).map(String::from);
    let small = arr
        .iter()
        .filter(|i| width(i) >= 64)
        .min_by_key(|i| width(i))
        .and_then(|i| i["url"].as_str())
        .map(String::from)
        .or_else(|| large.clone());
    (large, small)
}

fn parse_artist(a: &serde_json::Value) -> Option<ArtistInfo> {
    let (image_url, thumb_url) = images(&a["images"]);
    Some(ArtistInfo {
        id: a["id"].as_str()?.to_string(),
        name: a["name"].as_str()?.to_string(),
        image_url,
        thumb_url,
        genres: a["genres"].as_array().map(|g| g.iter().filter_map(|x| x.as_str().map(String::from)).collect()).unwrap_or_default(),
        followers: a["followers"]["total"].as_i64().unwrap_or(0),
        url: a["external_urls"]["spotify"].as_str().unwrap_or("").to_string(),
    })
}

pub fn search_artists(c: &Credentials, query: &str, limit: u32) -> Result<Vec<ArtistInfo>> {
    let limit = limit.to_string();
    let v = api(c, "https://api.spotify.com/v1/search", &[("q", query), ("type", "artist"), ("limit", &limit)])?;
    Ok(v["artists"]["items"].as_array().into_iter().flatten().filter_map(parse_artist).collect())
}

pub fn artist_by_id(c: &Credentials, id: &str) -> Result<ArtistInfo> {
    let v = api(c, &format!("https://api.spotify.com/v1/artists/{id}"), &[])?;
    parse_artist(&v).context("unexpected Spotify response")
}

fn norm(s: &str) -> String {
    s.chars().filter(|c| c.is_alphanumeric()).collect::<String>().to_lowercase()
}

/// Best automatic match for an artist name: exact (normalised) name first, else the top hit.
pub fn find_artist(c: &Credentials, name: &str) -> Result<Option<ArtistInfo>> {
    let hits = search_artists(c, name, 5)?;
    let wanted = norm(name);
    Ok(hits.iter().find(|a| norm(&a.name) == wanted).cloned().or_else(|| hits.into_iter().next()))
}

pub fn search_tracks(c: &Credentials, query: &str, limit: u32) -> Result<Vec<TrackMatch>> {
    let limit = limit.to_string();
    let v = api(c, "https://api.spotify.com/v1/search", &[("q", query), ("type", "track"), ("limit", &limit)])?;
    Ok(v["tracks"]["items"]
        .as_array()
        .into_iter()
        .flatten()
        .filter_map(|t| {
            let (image_url, thumb_url) = images(&t["album"]["images"]);
            Some(TrackMatch {
                title: t["name"].as_str()?.to_string(),
                artists: t["artists"]
                    .as_array()
                    .map(|a| a.iter().filter_map(|x| x["name"].as_str()).collect::<Vec<_>>().join(", "))
                    .unwrap_or_default(),
                album: t["album"]["name"].as_str().unwrap_or("").to_string(),
                image_url,
                thumb_url,
            })
        })
        .collect())
}

/// Best automatic artwork match for a track (exact artist preferred).
pub fn find_track(c: &Credentials, artist: &str, title: &str) -> Result<Option<TrackMatch>> {
    let q = if artist.is_empty() { format!("track:{title}") } else { format!("track:{title} artist:{artist}") };
    let mut hits = search_tracks(c, &q, 5)?;
    if hits.is_empty() && !artist.is_empty() {
        hits = search_tracks(c, &format!("{artist} {title}"), 5)?;
    }
    let wanted = norm(artist);
    Ok(hits
        .iter()
        .find(|t| t.artists.split(", ").any(|a| norm(a) == wanted))
        .cloned()
        .or_else(|| hits.into_iter().next()))
}

pub fn download(url: &str) -> Result<Vec<u8>> {
    Ok(http_client().get(url).send()?.error_for_status()?.bytes()?.to_vec())
}

/// Store an artist image in the artwork folder. Returns the file path.
pub fn save_artist_image(artist: &str, bytes: &[u8]) -> Result<String> {
    let path = paths::artwork_dir().join(format!("{}.jpg", paths::sanitize_file_name(artist)));
    let img = image::load_from_memory(bytes).context("not an image")?;
    let img = if img.width() > 1600 { img.thumbnail(1600, 1600) } else { img };
    img.to_rgb8().save_with_format(&path, image::ImageFormat::Jpeg)?;
    Ok(path.to_string_lossy().into_owned())
}
