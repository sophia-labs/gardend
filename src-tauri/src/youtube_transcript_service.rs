use crate::{
    json_utils::json_string,
    runtime_config::LOCAL_WEB_CLIP_MAX_BYTES,
    web_fetch::{fetch_limited_url_bytes, local_web_fetch_client},
    youtube_transcript_format::{build_youtube_transcript_markdown, parse_youtube_transcript_rows},
    youtube_transcript_sources::{
        extract_youtube_player_response, extract_youtube_video_id, select_youtube_caption_track,
        transcript_json_url,
    },
};
use serde::Deserialize;

pub(super) use crate::youtube_transcript_format::YOUTUBE_NOTICE;

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
pub(super) struct YoutubeTranscriptRequest {
    pub(super) url: String,
    #[serde(default)]
    pub(super) title: Option<String>,
    #[serde(default, alias = "folder_id")]
    pub(super) folder_id: Option<String>,
    #[serde(default)]
    pub(super) languages: Option<Vec<String>>,
    #[serde(default)]
    pub(super) mode: Option<String>,
    #[serde(default, alias = "chunk_seconds")]
    pub(super) chunk_seconds: Option<i64>,
    #[serde(default, alias = "show_ranges")]
    pub(super) show_ranges: Option<bool>,
}

fn trim_optional(value: Option<String>) -> Option<String> {
    value.and_then(|value| {
        let trimmed = value.trim();
        if trimmed.is_empty() {
            None
        } else {
            Some(trimmed.to_string())
        }
    })
}

pub(super) async fn fetch_youtube_transcript_markdown(
    body: &YoutubeTranscriptRequest,
) -> Result<(String, String, String), String> {
    let video_id = extract_youtube_video_id(&body.url)
        .ok_or_else(|| "Invalid YouTube URL or video ID".to_string())?;
    let watch_url = format!("https://www.youtube.com/watch?v={video_id}");
    let client = local_web_fetch_client()?;
    let watch_parsed = reqwest::Url::parse(&watch_url).map_err(|error| error.to_string())?;
    let (watch_bytes, _) =
        fetch_limited_url_bytes(&client, watch_parsed, LOCAL_WEB_CLIP_MAX_BYTES).await?;
    let watch_html = String::from_utf8_lossy(&watch_bytes).to_string();
    let player = extract_youtube_player_response(&watch_html)?;
    let requested_languages = body
        .languages
        .clone()
        .unwrap_or_else(|| vec!["en".to_string(), "en-US".to_string()])
        .into_iter()
        .map(|language| language.trim().to_string())
        .filter(|language| !language.is_empty())
        .collect::<Vec<_>>();
    let caption_track = select_youtube_caption_track(&player, &requested_languages)?;
    let base_url = json_string(caption_track.get("baseUrl"))
        .ok_or_else(|| "Selected caption track did not include a transcript URL".to_string())?;
    let transcript_url =
        reqwest::Url::parse(&transcript_json_url(&base_url)).map_err(|error| error.to_string())?;
    let (transcript_bytes, _) =
        fetch_limited_url_bytes(&client, transcript_url, LOCAL_WEB_CLIP_MAX_BYTES).await?;
    let transcript_json: serde_json::Value = serde_json::from_slice(&transcript_bytes)
        .map_err(|error| format!("parse YouTube transcript JSON: {error}"))?;
    let rows = parse_youtube_transcript_rows(&transcript_json);
    if rows.is_empty() {
        return Err("Transcript unavailable or empty for this video".to_string());
    }
    let resolved_title = trim_optional(body.title.clone())
        .or_else(|| {
            json_string(
                player
                    .get("videoDetails")
                    .and_then(|details| details.get("title")),
            )
        })
        .unwrap_or_else(|| format!("YouTube Transcript ({video_id})"));
    let mode = body
        .mode
        .as_deref()
        .map(str::trim)
        .filter(|value| *value == "readable" || *value == "timestamped")
        .unwrap_or("readable");
    let chunk_seconds = body.chunk_seconds.unwrap_or(30).clamp(5, 600);
    let markdown = build_youtube_transcript_markdown(
        &resolved_title,
        &watch_url,
        &video_id,
        &rows,
        mode,
        chunk_seconds,
        body.show_ranges.unwrap_or(false),
    );
    Ok((markdown, resolved_title, video_id))
}
