use crate::json_utils::json_string;

pub(super) fn extract_youtube_video_id(url_or_id: &str) -> Option<String> {
    let raw = url_or_id.trim();
    if raw.len() == 11
        && raw
            .chars()
            .all(|ch| ch.is_ascii_alphanumeric() || ch == '_' || ch == '-')
    {
        return Some(raw.to_string());
    }
    let parsed = reqwest::Url::parse(raw).ok()?;
    let mut host = parsed.host_str()?.to_ascii_lowercase();
    if let Some(stripped) = host.strip_prefix("www.") {
        host = stripped.to_string();
    }
    let candidate = if host == "youtu.be" {
        parsed
            .path_segments()
            .and_then(|mut segments| segments.next())
            .map(str::to_string)
    } else if matches!(
        host.as_str(),
        "youtube.com" | "m.youtube.com" | "music.youtube.com"
    ) {
        if parsed.path() == "/watch" {
            parsed
                .query_pairs()
                .find(|(key, _)| key == "v")
                .map(|(_, value)| value.to_string())
        } else if let Some(value) = parsed.path().strip_prefix("/shorts/") {
            Some(value.split('/').next().unwrap_or_default().to_string())
        } else if let Some(value) = parsed.path().strip_prefix("/embed/") {
            Some(value.split('/').next().unwrap_or_default().to_string())
        } else {
            parsed
                .path()
                .strip_prefix("/live/")
                .map(|value| value.split('/').next().unwrap_or_default().to_string())
        }
    } else {
        None
    }?;
    if candidate.len() == 11
        && candidate
            .chars()
            .all(|ch| ch.is_ascii_alphanumeric() || ch == '_' || ch == '-')
    {
        Some(candidate)
    } else {
        None
    }
}

fn extract_balanced_json_object(source: &str, start: usize) -> Option<&str> {
    let mut depth = 0_i64;
    let mut in_string = false;
    let mut escaped = false;
    for (offset, ch) in source[start..].char_indices() {
        if in_string {
            if escaped {
                escaped = false;
            } else if ch == '\\' {
                escaped = true;
            } else if ch == '"' {
                in_string = false;
            }
            continue;
        }
        match ch {
            '"' => in_string = true,
            '{' => depth += 1,
            '}' => {
                depth -= 1;
                if depth == 0 {
                    return Some(&source[start..start + offset + ch.len_utf8()]);
                }
            }
            _ => {}
        }
    }
    None
}

pub(super) fn extract_youtube_player_response(html: &str) -> Result<serde_json::Value, String> {
    for marker in [
        "ytInitialPlayerResponse =",
        "ytInitialPlayerResponse=",
        "\"ytInitialPlayerResponse\":",
    ] {
        if let Some(marker_index) = html.find(marker) {
            if let Some(relative_start) = html[marker_index..].find('{') {
                let start = marker_index + relative_start;
                if let Some(raw_json) = extract_balanced_json_object(html, start) {
                    return serde_json::from_str(raw_json)
                        .map_err(|error| format!("parse YouTube player response: {error}"));
                }
            }
        }
    }
    Err("Could not locate YouTube player response".to_string())
}

pub(super) fn select_youtube_caption_track<'a>(
    player: &'a serde_json::Value,
    languages: &[String],
) -> Result<&'a serde_json::Value, String> {
    let tracks = player
        .get("captions")
        .and_then(|value| value.get("playerCaptionsTracklistRenderer"))
        .and_then(|value| value.get("captionTracks"))
        .and_then(serde_json::Value::as_array)
        .ok_or_else(|| "Transcript unavailable or empty for this video".to_string())?;
    if tracks.is_empty() {
        return Err("Transcript unavailable or empty for this video".to_string());
    }
    for requested in languages {
        if let Some(track) = tracks.iter().find(|track| {
            json_string(track.get("languageCode"))
                .map(|language| language.eq_ignore_ascii_case(requested))
                .unwrap_or(false)
        }) {
            return Ok(track);
        }
    }
    Ok(&tracks[0])
}

pub(super) fn transcript_json_url(base_url: &str) -> String {
    if base_url.contains("fmt=") {
        base_url.to_string()
    } else if base_url.contains('?') {
        format!("{base_url}&fmt=json3")
    } else {
        format!("{base_url}?fmt=json3")
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn extract_youtube_video_id_accepts_common_sources() {
        assert_eq!(
            extract_youtube_video_id("abc123abc12"),
            Some("abc123abc12".to_string())
        );
        assert_eq!(
            extract_youtube_video_id("https://youtu.be/abc123abc12?si=x"),
            Some("abc123abc12".to_string())
        );
        assert_eq!(
            extract_youtube_video_id("https://www.youtube.com/watch?v=abc123abc12&t=30s"),
            Some("abc123abc12".to_string())
        );
        assert_eq!(
            extract_youtube_video_id("https://m.youtube.com/shorts/abc123abc12"),
            Some("abc123abc12".to_string())
        );
        assert_eq!(
            extract_youtube_video_id("https://example.com/abc123abc12"),
            None
        );
    }

    #[test]
    fn extract_youtube_player_response_reads_balanced_json() {
        let html = r#"
            <script>
              ytInitialPlayerResponse = {"videoDetails":{"title":"Demo {video}"},"ok":true};
            </script>
        "#;

        let player = extract_youtube_player_response(html).expect("player response");

        assert_eq!(player["videoDetails"]["title"], "Demo {video}");
        assert_eq!(player["ok"], true);
    }

    #[test]
    fn select_youtube_caption_track_prefers_requested_language() {
        let player = serde_json::json!({
            "captions": {
                "playerCaptionsTracklistRenderer": {
                    "captionTracks": [
                        { "languageCode": "en", "baseUrl": "https://captions/en" },
                        { "languageCode": "es", "baseUrl": "https://captions/es" }
                    ]
                }
            }
        });

        let track =
            select_youtube_caption_track(&player, &["ES".to_string()]).expect("caption track");
        assert_eq!(track["baseUrl"], "https://captions/es");

        let fallback =
            select_youtube_caption_track(&player, &["fr".to_string()]).expect("fallback track");
        assert_eq!(fallback["baseUrl"], "https://captions/en");
    }

    #[test]
    fn transcript_json_url_preserves_or_adds_format() {
        assert_eq!(
            transcript_json_url("https://captions.example/api"),
            "https://captions.example/api?fmt=json3"
        );
        assert_eq!(
            transcript_json_url("https://captions.example/api?lang=en"),
            "https://captions.example/api?lang=en&fmt=json3"
        );
        assert_eq!(
            transcript_json_url("https://captions.example/api?fmt=json3"),
            "https://captions.example/api?fmt=json3"
        );
    }
}
