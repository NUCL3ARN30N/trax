//! Spotify Web API (client-credentials flow, no user login): artist profiles and artwork.
//! Create an app at https://developer.spotify.com/dashboard to get a client id/secret.
//! Playlists and albums to import are read from the public embed player instead, which needs
//! no login (the Web API only lists a playlist's songs to its owner).

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

/// A song of a Spotify playlist or album.
pub struct ListTrack {
    pub title: String,
    pub artists: String,
    pub duration_ms: i64,
}

pub struct List {
    pub name: String,
    pub owner: String,
    pub cover_url: Option<String>,
    pub tracks: Vec<ListTrack>,
}

/// A public playlist or album (`kind` "playlist" or "album", `id` already checked) from the
/// embed player, which holds the first 100 songs.
pub fn list(kind: &str, id: &str) -> Result<List> {
    let resp = http_client().get(format!("https://open.spotify.com/embed/{kind}/{id}")).send().context("cannot reach Spotify")?;
    match resp.status().as_u16() {
        200 => parse_embed(&resp.text()?),
        404 => bail!("Spotify doesn't know this {kind}; private playlists can't be imported"),
        s => bail!("Spotify returned status {s}"),
    }
}

fn parse_embed(html: &str) -> Result<List> {
    const START: &str = r#"<script id="__NEXT_DATA__" type="application/json">"#;
    let json = html
        .split_once(START)
        .and_then(|(_, rest)| rest.split_once("</script>"))
        .map(|(json, _)| json)
        .context("unexpected Spotify page")?;
    let v: serde_json::Value = serde_json::from_str(json).context("unexpected Spotify page")?;
    let e = &v["props"]["pageProps"]["state"]["data"]["entity"];
    let tracks = e["trackList"]
        .as_array()
        .into_iter()
        .flatten()
        // Playlists can hold podcast episodes too.
        .filter(|t| t["entityType"].as_str().is_none_or(|k| k == "track"))
        .filter_map(|t| {
            Some(ListTrack {
                title: t["title"].as_str()?.trim().to_string(),
                artists: t["subtitle"].as_str().unwrap_or("").trim().to_string(),
                duration_ms: t["duration"].as_i64().unwrap_or(0),
            })
        })
        .collect();
    Ok(List {
        name: e["name"].as_str().or(e["title"].as_str()).unwrap_or("").trim().to_string(),
        owner: e["subtitle"].as_str().unwrap_or("").trim().to_string(),
        // Playlists have `coverArt`, albums only the images of `visualIdentity`.
        cover_url: e["coverArt"]["sources"]
            .as_array()
            .and_then(|s| s.first())
            .or_else(|| e["visualIdentity"]["image"].as_array()?.iter().max_by_key(|i| i["maxWidth"].as_u64().unwrap_or(0)))
            .and_then(|i| i["url"].as_str())
            .map(String::from),
        tracks,
    })
}

#[cfg(test)]
mod tests {
    #[test]
    fn embed_page() {
        let html = r#"<html><script id="__NEXT_DATA__" type="application/json">{"props":{"pageProps":{"state":{"data":{"entity":{
            "name":"Road trip","subtitle":"Alex","coverArt":{"sources":[{"url":"https://i.scdn.co/image/x"}]},
            "trackList":[
                {"title":"Housewife","subtitle":"Tate McRae","duration":208810,"entityType":"track"},
                {"title":"Duet","subtitle":"A, B","duration":1000,"entityType":"track"},
                {"title":"Episode 1","subtitle":"Podcast","duration":5,"entityType":"episode"}
            ]}}}}}}</script></html>"#;
        let list = super::parse_embed(html).unwrap();
        assert_eq!(list.name, "Road trip");
        assert_eq!(list.owner, "Alex");
        assert_eq!(list.cover_url.as_deref(), Some("https://i.scdn.co/image/x"));
        assert_eq!(list.tracks.len(), 2);
        assert_eq!(list.tracks[0].title, "Housewife");
        assert_eq!(list.tracks[0].duration_ms, 208_810);
        assert_eq!(list.tracks[1].artists, "A, B");
        let album = r#"<script id="__NEXT_DATA__" type="application/json">{"props":{"pageProps":{"state":{"data":{"entity":{
            "name":"Album","visualIdentity":{"image":[{"url":"small","maxWidth":64},{"url":"large","maxWidth":640}]},"trackList":[]}}}}}}</script>"#;
        assert_eq!(super::parse_embed(album).unwrap().cover_url.as_deref(), Some("large"));
        assert!(super::parse_embed("<html></html>").is_err());
    }
}
