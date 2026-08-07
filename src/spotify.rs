//! Spotify links without a Spotify account, API key, or any Spotify audio.
//!
//! Spotify streams are DRM-protected, so nothing here ever plays a Spotify stream. A Spotify link
//! is only a *reference*: we read the public `open.spotify.com/embed/…` page - the same JSON the
//! site's own iframe player loads - to learn each track's name and artist, and hand those to
//! yt-dlp as YouTube searches (see [`crate::youtube`]). A track becomes one search; a playlist or
//! album becomes one search per entry.
//!
//! The embed page is fetched with `curl`, not a linked HTTP stack: this player drives external
//! programs rather than linking libraries (see mpv, yt-dlp). `curl` is therefore required for
//! Spotify links, and only for them.

use serde_json::Value;
use std::process::Command;

/// A real browser UA. The embed endpoint serves the same HTML to anyone, but a plausible UA keeps
/// the occasional bot check happy.
const USER_AGENT: &str = "Mozilla/5.0 (X11; Linux x86_64) AppleWebKit/537.36 (KHTML, like Gecko) Chrome/120.0 Safari/537.36";

/// True for any `open.spotify.com/…` link or `spotify:…` URI.
pub fn is_spotify(url: &str) -> bool {
    url.contains("open.spotify.com/") || url.starts_with("spotify:")
}

/// One playable reference from a Spotify page: the display title for the UI, and the
/// YouTube search that stands in for the stream we can't have.
pub struct Track {
    /// `Artist - Title`, shown in the playlist view before (and after) resolution.
    pub title: String,
    /// `artist title`, handed to yt-dlp as a `ytsearch1:` query.
    pub query: String,
}

/// Resolve a Spotify link to its tracks, one YouTube search each.
///
/// `Ok(None)` when `url` isn't a Spotify link at all, so the caller can fall through to its normal
/// yt-dlp path. `Ok(Some(_))` is a non-empty list; `Err` means it was a Spotify link we couldn't
/// read.
pub fn tracks(url: &str) -> Result<Option<Vec<Track>>, String> {
    if !is_spotify(url) {
        return Ok(None);
    }
    let (kind, id) =
        parse(url).ok_or("That isn't a Spotify track, album, or playlist link.".to_string())?;
    let html = fetch(&format!("https://open.spotify.com/embed/{kind}/{id}"))?;
    let json = extract_next_data(&html).ok_or("Couldn't read that Spotify page.".to_string())?;
    let data: Value =
        serde_json::from_str(json).map_err(|e| format!("Spotify page wasn't valid JSON: {e}"))?;
    let entity = data
        .pointer("/props/pageProps/state/data/entity")
        .ok_or("That Spotify page had no track information.".to_string())?;
    let tracks = tracks_from_entity(entity);
    if tracks.is_empty() {
        return Err("Couldn't read any tracks from that Spotify link.".to_string());
    }
    Ok(Some(tracks))
}

/// The `(kind, id)` of a Spotify link. Tolerates a locale prefix (`/intl-fr/track/…`), a trailing
/// `?si=…`, and the `spotify:track:…` URI form. `None` for anything but a track/album/playlist.
fn parse(url: &str) -> Option<(&str, &str)> {
    // `spotify:track:ID` and `.../track/ID` both split into a `track` segment then its id once we
    // cut on both separators.
    let mut segments = url.split(['/', ':']);
    while let Some(seg) = segments.next() {
        if matches!(seg, "track" | "album" | "playlist") {
            let id = segments.next()?.split(['?', '#']).next()?;
            return (!id.is_empty()).then_some((seg, id));
        }
    }
    None
}

/// Fetch a URL's body with `curl`, mapping a missing binary and a failed request to distinct,
/// human messages.
fn fetch(url: &str) -> Result<String, String> {
    let output = Command::new("curl")
        .args(["-fsSL", "-A", USER_AGENT, url])
        .output()
        .map_err(|_| "Spotify links need `curl` on your PATH. Install curl and try again.")?;
    if !output.status.success() {
        return Err("Couldn't reach Spotify to read that link.".to_string());
    }
    Ok(String::from_utf8_lossy(&output.stdout).into_owned())
}

/// Pull the JSON out of `<script id="__NEXT_DATA__" type="application/json">…</script>`, the blob
/// the embed player hydrates from.
fn extract_next_data(html: &str) -> Option<&str> {
    let after_tag = &html[html.find(r#"id="__NEXT_DATA__""#)?..];
    let body = &after_tag[after_tag.find('>')? + 1..];
    Some(body[..body.find("</script>")?].trim())
}

/// One [`Track`] per entry. Handles both the single-track entity (`name` + `artists[]`) and a
/// playlist/album's `trackList` entries (each `title` + `subtitle`).
fn tracks_from_entity(entity: &Value) -> Vec<Track> {
    match entity.get("trackList").and_then(Value::as_array) {
        Some(list) if !list.is_empty() => list.iter().filter_map(track_info).collect(),
        _ => track_info(entity).into_iter().collect(),
    }
}

/// The [`Track`] for one track object, or `None` if it carries no usable title.
fn track_info(track: &Value) -> Option<Track> {
    let str_field = |k| {
        track
            .get(k)
            .and_then(Value::as_str)
            .filter(|s: &&str| !s.is_empty())
    };
    let name = str_field("name").or_else(|| str_field("title"))?;
    // Playlist/album entries name the artist in `subtitle`; a lone track lists `artists[]`.
    let artist = str_field("subtitle").map(str::to_string).or_else(|| {
        let names: Vec<&str> = track
            .get("artists")?
            .as_array()?
            .iter()
            .filter_map(|a| a.get("name").and_then(Value::as_str))
            .collect();
        (!names.is_empty()).then(|| names.join(" "))
    });
    Some(match artist {
        Some(artist) => Track {
            title: format!("{artist} - {name}"),
            query: format!("{artist} {name}"),
        },
        None => Track {
            title: name.to_string(),
            query: name.to_string(),
        },
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn recognises_spotify_links() {
        assert!(is_spotify("https://open.spotify.com/track/abc"));
        assert!(is_spotify("spotify:playlist:xyz"));
        assert!(!is_spotify("https://www.youtube.com/watch?v=dQw4w9WgXcQ"));
        assert!(!is_spotify("https://soundcloud.com/artist/track"));
    }

    #[test]
    fn parses_every_link_shape() {
        assert_eq!(
            parse("https://open.spotify.com/track/4cOdK2wGLETKBW3PvgPWqT?si=abc"),
            Some(("track", "4cOdK2wGLETKBW3PvgPWqT"))
        );
        // Locale prefix and a query string both get stripped.
        assert_eq!(
            parse("https://open.spotify.com/intl-fr/playlist/37i9dQ?foo=bar"),
            Some(("playlist", "37i9dQ"))
        );
        assert_eq!(parse("spotify:album:4m2880"), Some(("album", "4m2880")));
        // Artists and users aren't playable references.
        assert_eq!(parse("https://open.spotify.com/artist/1234"), None);
    }

    #[test]
    fn single_track_carries_title_and_query() {
        // Shape of a track entity: `name` + `artists[]`, no `subtitle`.
        let entity = json!({
            "name": "Never Gonna Give You Up",
            "title": "Never Gonna Give You Up",
            "artists": [{"name": "Rick Astley"}],
        });
        let tracks = tracks_from_entity(&entity);
        assert_eq!(tracks.len(), 1);
        assert_eq!(tracks[0].title, "Rick Astley - Never Gonna Give You Up");
        assert_eq!(tracks[0].query, "Rick Astley Never Gonna Give You Up");
    }

    #[test]
    fn playlist_tracks_use_tracklist_subtitles() {
        // Shape of a playlist/album entity: `trackList` of `title` + `subtitle` (the artist).
        let entity = json!({
            "name": "Today's Top Hits",
            "subtitle": "Spotify",
            "trackList": [
                {"title": "stupid song", "subtitle": "Olivia Rodrigo"},
                {"title": "Earrings", "subtitle": "Malcolm Todd"},
                {"title": "", "subtitle": "No Name"},
            ],
        });
        // The playlist's own subtitle is ignored; a track with no title is dropped.
        let tracks = tracks_from_entity(&entity);
        let queries: Vec<&str> = tracks.iter().map(|t| t.query.as_str()).collect();
        let titles: Vec<&str> = tracks.iter().map(|t| t.title.as_str()).collect();
        assert_eq!(
            queries,
            vec!["Olivia Rodrigo stupid song", "Malcolm Todd Earrings"]
        );
        assert_eq!(
            titles,
            vec!["Olivia Rodrigo - stupid song", "Malcolm Todd - Earrings"]
        );
    }

    #[test]
    fn extracts_next_data_blob() {
        let html = r#"<html><body><script id="__NEXT_DATA__" type="application/json">{"a":1}</script></body></html>"#;
        assert_eq!(extract_next_data(html), Some(r#"{"a":1}"#));
        assert_eq!(extract_next_data("<html>no blob here</html>"), None);
    }
}
