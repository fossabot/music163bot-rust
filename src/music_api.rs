use crate::error::{BotError, Result};
use image::{DynamicImage, GenericImageView, ImageFormat};
use reqwest::Client;
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::path::Path;

#[derive(Debug, Clone)]
pub struct MusicApi {
    client: Client,
    pub music_u: Option<String>,
    base_url: String,
}

#[derive(Debug, Serialize, Deserialize)]
pub struct SongDetailResponse {
    pub code: i32,
    pub songs: Vec<SongDetail>,
}

#[derive(Debug, Serialize, Deserialize)]
pub struct SongDetail {
    pub id: u64,
    pub name: String,
    #[serde(alias = "duration")]
    pub dt: Option<u64>, // Duration in milliseconds (may be missing)
    #[serde(alias = "artists")]
    pub ar: Option<Vec<Artist>>, // Artists array (may be missing)
    #[serde(alias = "album")]
    pub al: Option<Album>, // Album info (may be missing)
}

#[derive(Debug, Serialize, Deserialize)]
pub struct Artist {
    pub id: u64,
    pub name: String,
}

#[derive(Debug, Serialize, Deserialize)]
pub struct Album {
    pub id: u64,
    pub name: String,
    #[serde(rename = "picUrl")]
    pub pic_url: Option<String>,
}

#[derive(Debug, Serialize, Deserialize)]
pub struct SongUrlResponse {
    pub code: i32,
    pub data: Vec<SongUrl>,
}

#[derive(Debug, Serialize, Deserialize)]
pub struct SongUrl {
    pub id: u64,
    pub url: String,
    pub br: u64,
    pub size: u64,
    pub md5: String,
    #[serde(rename = "type")]
    pub format: String,
}

#[derive(Debug, Serialize, Deserialize)]
pub struct LyricResponse {
    pub code: i32,
    pub lrc: Option<LyricContent>,
    pub tlyric: Option<LyricContent>,
}

#[derive(Debug, Serialize, Deserialize)]
pub struct LyricContent {
    pub lyric: String,
}

#[derive(Debug, Serialize, Deserialize)]
pub struct SearchResponse {
    pub code: i32,
    pub result: SearchResult,
}

#[derive(Debug, Serialize, Deserialize)]
pub struct SearchResult {
    pub songs: Vec<SearchSong>,
    #[serde(rename = "songCount")]
    pub song_count: u64,
}

#[derive(Debug, Serialize, Deserialize)]
pub struct SearchSong {
    pub id: u64,
    pub name: String,
    pub artists: Vec<Artist>,
    pub album: Album,
    pub duration: u64,
}

impl MusicApi {
    pub fn new(music_u: Option<String>, base_url: String) -> Self {
        let mut client_builder = Client::builder();

        // Use rustls TLS for better compatibility
        client_builder = client_builder.use_rustls_tls();

        // Performance optimizations
        client_builder = client_builder
            .tcp_nodelay(true)
            .pool_idle_timeout(std::time::Duration::from_secs(90))
            .pool_max_idle_per_host(32)
            .connect_timeout(std::time::Duration::from_secs(10));

        // Add user agent
        client_builder = client_builder
            .user_agent("Mozilla/5.0 (Windows NT 10.0; Win64; x64) AppleWebKit/537.36 (KHTML, like Gecko) Chrome/91.0.4472.124 Safari/537.36");

        let client = client_builder.build().unwrap();

        Self {
            client,
            music_u,
            base_url,
        }
    }

    /// Get song details
    pub async fn get_song_detail(&self, song_id: u64) -> Result<SongDetail> {
        let url = format!("{}/api/song/detail", self.base_url);
        let mut params = HashMap::new();
        params.insert("id", song_id.to_string());
        params.insert("ids", format!("[{}]", song_id));

        let mut request = self.client.post(url).form(&params);

        // Add MUSIC_U cookie if available
        if let Some(music_u) = &self.music_u {
            request = request.header("Cookie", format!("MUSIC_U={}", music_u));
        }

        let response = request.send().await?;
        let data: SongDetailResponse = response.json().await?;

        if data.code != 200 {
            return Err(BotError::MusicApi(format!(
                "API returned code {}",
                data.code
            )));
        }

        data.songs
            .into_iter()
            .next()
            .ok_or_else(|| BotError::MusicApi("No song found".to_string()))
    }

    /// Get song download URL
    pub async fn get_song_url(&self, song_id: u64, br: u64) -> Result<SongUrl> {
        let url = format!("{}/api/song/enhance/player/url", self.base_url);
        let mut params = HashMap::new();
        params.insert("ids", format!("[{}]", song_id));
        params.insert("br", br.to_string());

        let mut request = self.client.post(url).form(&params);

        if let Some(music_u) = &self.music_u {
            request = request.header("Cookie", format!("MUSIC_U={}", music_u));
        }

        let response = request.send().await?;
        let data: SongUrlResponse = response.json().await?;

        if data.code != 200 {
            return Err(BotError::MusicApi(format!(
                "API returned code {}",
                data.code
            )));
        }

        data.data
            .into_iter()
            .next()
            .ok_or_else(|| BotError::MusicApi("No download URL found".to_string()))
    }

    /// Get song lyrics
    pub async fn get_song_lyric(&self, song_id: u64) -> Result<String> {
        let url = format!("{}/api/song/lyric?id={}&lv=1&tv=1", self.base_url, song_id);

        let mut request = self.client.get(&url);

        if let Some(music_u) = &self.music_u {
            request = request.header("Cookie", format!("MUSIC_U={}", music_u));
        }

        let response = request.send().await?;
        let data: LyricResponse = response.json().await?;

        if data.code != 200 {
            return Err(BotError::MusicApi(format!(
                "API returned code {}",
                data.code
            )));
        }

        let lyric = data
            .lrc
            .map(|l| l.lyric)
            .unwrap_or_else(|| "No lyrics available".to_string());

        Ok(lyric)
    }

    /// Search songs
    pub async fn search_songs(&self, keyword: &str, limit: u32) -> Result<Vec<SearchSong>> {
        let url = format!("{}/api/search/get/web", self.base_url);
        let mut params = HashMap::new();
        params.insert("s", keyword.to_string());
        params.insert("type", "1".to_string()); // Song type
        params.insert("limit", limit.to_string());
        params.insert("offset", "0".to_string());

        let mut request = self.client.post(url).form(&params);

        if let Some(music_u) = &self.music_u {
            request = request.header("Cookie", format!("MUSIC_U={}", music_u));
        }

        let response = request.send().await?;
        let data: SearchResponse = response.json().await?;

        if data.code != 200 {
            return Err(BotError::MusicApi(format!(
                "API returned code {}",
                data.code
            )));
        }

        Ok(data.result.songs)
    }

    /// Download file with proper headers and cookies
    pub async fn download_file(&self, url: &str) -> Result<reqwest::Response> {
        // Apply host replacement similar to the original Go project
        // This helps avoid 403 errors from NetEase servers
        let processed_url = url
            .replace("m8.", "m7.")
            .replace("m801.", "m701.")
            .replace("m804.", "m701.")
            .replace("m704.", "m701.");

        let mut request = self.client.get(&processed_url);

        // Add MUSIC_U cookie if available
        if let Some(music_u) = &self.music_u {
            request = request.header("Cookie", format!("MUSIC_U={}", music_u));
        }

        // Add comprehensive headers to avoid 403 errors
        request = request
            .header("User-Agent", "Mozilla/5.0 (Windows NT 10.0; Win64; x64) AppleWebKit/537.36 (KHTML, like Gecko) Chrome/91.0.4472.124 Safari/537.36")
            .header("Referer", "https://music.163.com/")
            .header("Accept", "audio/mpeg, audio/*, */*")
            .header("Accept-Language", "zh-CN,zh;q=0.9,en;q=0.8")
            .header("Cache-Control", "no-cache")
            .header("DNT", "1")
            .header("Sec-Fetch-Dest", "audio")
            .header("Sec-Fetch-Mode", "cors")
            .header("Sec-Fetch-Site", "cross-site");

        let response = request.send().await?;
        Ok(response)
    }

    /// Download and resize album art image
    pub async fn download_album_art(&self, pic_url: &str, output_path: &Path) -> Result<()> {
        if pic_url.is_empty() {
            return Err(BotError::MusicApi("Empty album art URL".to_string()));
        }

        // Download the image
        let mut request = self.client.get(pic_url);

        // Add headers for image download
        request = request
            .header(
                "User-Agent",
                "Mozilla/5.0 (Windows NT 10.0; Win64; x64) AppleWebKit/537.36",
            )
            .header("Referer", "https://music.163.com/")
            .header(
                "Accept",
                "image/webp,image/apng,image/svg+xml,image/*,*/*;q=0.8",
            );

        let response = request.send().await?;
        if !response.status().is_success() {
            return Err(BotError::MusicApi(format!(
                "Failed to download album art: {}",
                response.status()
            )));
        }

        let bytes = response.bytes().await?;
        let output_path_buf = output_path.to_path_buf();

        // Offload image processing to a blocking thread to avoid blocking the async runtime
        tokio::task::spawn_blocking(move || {
            // Load and resize image
            let img = image::load_from_memory(&bytes)
                .map_err(|e| BotError::MusicApi(format!("Failed to decode image: {}", e)))?;

            // Resize to 320x320 with black padding (like the original Go project)
            let resized = resize_image_with_padding(img, 320, 320);

            // Save as JPEG
            resized
                .save_with_format(&output_path_buf, ImageFormat::Jpeg)
                .map_err(|e| BotError::MusicApi(format!("Failed to save image: {}", e)))?;

            Ok(())
        })
        .await
        .map_err(|e| BotError::MusicApi(format!("Image processing task panicked: {}", e)))?
    }
}

/// Parse artists into a formatted string
pub fn format_artists(artists: &[Artist]) -> String {
    artists
        .iter()
        .map(|a| a.name.as_str())
        .collect::<Vec<_>>()
        .join("/")
}

/// Resize image with black padding to maintain aspect ratio (like the original Go project)
fn resize_image_with_padding(
    img: DynamicImage,
    target_width: u32,
    target_height: u32,
) -> DynamicImage {
    use image::{Rgb, RgbImage};

    let (orig_width, orig_height) = img.dimensions();
    let aspect_ratio = orig_width as f32 / orig_height as f32;
    let target_aspect_ratio = target_width as f32 / target_height as f32;

    // Calculate new dimensions while maintaining aspect ratio
    let (new_width, new_height) = if aspect_ratio > target_aspect_ratio {
        // Image is wider than target ratio, fit by width
        let new_width = target_width;
        let new_height = (target_width as f32 / aspect_ratio) as u32;
        (new_width, new_height)
    } else {
        // Image is taller than target ratio, fit by height
        let new_height = target_height;
        let new_width = (target_height as f32 * aspect_ratio) as u32;
        (new_width, new_height)
    };

    // Resize the image
    let resized = img.resize(new_width, new_height, image::imageops::FilterType::Lanczos3);

    // Create black background canvas
    let mut canvas = RgbImage::new(target_width, target_height);
    for pixel in canvas.pixels_mut() {
        *pixel = Rgb([0, 0, 0]); // Black background
    }

    // Calculate position to center the resized image
    let offset_x = (target_width - new_width) / 2;
    let offset_y = (target_height - new_height) / 2;

    // Convert resized image to RGB and overlay on canvas
    let resized_rgb = resized.to_rgb8();
    for (x, y, pixel) in resized_rgb.enumerate_pixels() {
        if x + offset_x < target_width && y + offset_y < target_height {
            canvas.put_pixel(x + offset_x, y + offset_y, *pixel);
        }
    }

    DynamicImage::ImageRgb8(canvas)
}
