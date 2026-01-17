use anyhow;
use futures_util::StreamExt;
use std::sync::Arc;
use teloxide::prelude::*;
use teloxide::types::{
    CallbackQuery, InlineKeyboardButton, InlineKeyboardMarkup, InlineQuery, InlineQueryResult,
    InlineQueryResultArticle, InputFile, InputMessageContent, InputMessageContentText, Message,
    MessageKind, ParseMode, ReplyMarkup,
};

use crate::config::Config;
use crate::database::{Database, SongInfo};
use crate::error::Result;
use crate::music_api::{format_artists, MusicApi};
use crate::utils::{clean_filename, ensure_dir, parse_music_id};

pub struct BotState {
    pub config: Config,
    pub database: Database,
    pub music_api: MusicApi,
    pub download_semaphore: Arc<tokio::sync::Semaphore>,
    pub bot_username: String,
}

pub async fn run(config: Config) -> Result<()> {
    tracing::info!("Starting Telegram bot...");

    // Ensure cache directory exists
    ensure_dir(&config.cache_dir)?;

    // Initialize database
    let database = Database::new(&config.database).await?;
    tracing::info!("Database initialized");

    // Initialize music API
    let music_api = MusicApi::new(config.music_u.clone(), config.music_api.clone());
    tracing::info!("Music API initialized");

    // Initialize bot with custom API URL support
    let bot = if !config.bot_api.is_empty() && config.bot_api != "https://api.telegram.org" {
        // 使用自定义API URL
        let api_url_str = if config.bot_api.ends_with("/bot") {
            config.bot_api.clone()
        } else {
            format!("{}/bot", config.bot_api)
        };

        match reqwest::Url::parse(&api_url_str) {
            Ok(api_url) => {
                tracing::info!("Using custom Telegram API URL: {}", api_url);

                // Create a custom HTTP client tuned for Cloudflare compatibility (mimic Go http client)
                let client = reqwest::Client::builder()
                    .use_rustls_tls()
                    .user_agent("Go-http-client/2.0")
                    .pool_max_idle_per_host(0)
                    .danger_accept_invalid_certs(false)
                    .timeout(std::time::Duration::from_secs(30))
                    .no_gzip()
                    .build()
                    .unwrap();

                // Create bot with custom client and API URL
                let bot = Bot::with_client(&config.bot_token, client).set_api_url(api_url.clone());

                // Test the connection with timeout and better error handling
                tracing::info!("Testing custom API connection...");
                match tokio::time::timeout(std::time::Duration::from_secs(15), bot.get_me()).await {
                    Ok(Ok(_)) => {
                        tracing::info!("✅ Custom API connection successful: {}", api_url);
                        bot
                    }
                    Ok(Err(e)) => {
                        let error_msg = format!("{}", e);
                        // Check if it's a CloudFlare challenge or other blocking issue
                        if error_msg.contains("Just a moment")
                            || error_msg.contains("cloudflare")
                            || error_msg.contains("challenge")
                        {
                            tracing::warn!("❌ Custom API blocked by CloudFlare protection. Falling back to official API.");
                        } else {
                            tracing::warn!("❌ Custom API connection failed: {}. Falling back to official API.", e);
                        }
                        tracing::info!("Using fallback Telegram API URL: https://api.telegram.org");
                        Bot::new(&config.bot_token)
                    }
                    Err(_) => {
                        tracing::warn!(
                            "❌ Custom API connection timeout (15s). Falling back to official API."
                        );
                        tracing::info!("Using fallback Telegram API URL: https://api.telegram.org");
                        Bot::new(&config.bot_token)
                    }
                }
            }
            Err(e) => {
                tracing::error!(
                    "Invalid custom API URL '{}': {}. Using official API.",
                    config.bot_api,
                    e
                );
                tracing::info!("Using fallback Telegram API URL: https://api.telegram.org");
                Bot::new(&config.bot_token)
            }
        }
    } else {
        // 使用默认API URL
        tracing::info!("Using default Telegram API URL: https://api.telegram.org");
        Bot::new(&config.bot_token)
    };

    // Log the API configuration
    tracing::info!("Music API configured: {}", &config.music_api);

    let me = bot.get_me().await?;
    let bot_username = me
        .username
        .clone()
        .unwrap_or_else(|| "Music163bot".to_string());
    tracing::info!("Bot @{} started successfully!", bot_username);

    // Create bot state (needs bot username)
    let bot_state = Arc::new(BotState {
        config: config.clone(),
        database,
        music_api,
        download_semaphore: Arc::new(tokio::sync::Semaphore::new(10)), // 增加到 10 个并发下载
        bot_username,
    });

    // Create dispatcher
    let handler = dptree::entry()
        .branch(Update::filter_message().endpoint(handle_message))
        .branch(Update::filter_callback_query().endpoint(handle_callback))
        .branch(Update::filter_inline_query().endpoint(handle_inline_query));

    Dispatcher::builder(bot, handler)
        .dependencies(dptree::deps![bot_state])
        .default_handler(|upd| async move {
            tracing::debug!("Unhandled update: {:?}", upd);
        })
        .enable_ctrlc_handler()
        .build()
        .dispatch()
        .await;
    Ok(())
}

async fn handle_message(bot: Bot, msg: Message, state: Arc<BotState>) -> ResponseResult<()> {
    if let MessageKind::Common(common) = &msg.kind {
        if let teloxide::types::MediaKind::Text(text_content) = &common.media_kind {
            let text = text_content.text.clone();
            let bot = bot.clone();
            let msg = msg.clone();
            let state = state.clone();

            tokio::spawn(async move {
                // Handle commands
                if text.starts_with('/') {
                    if let Err(e) = handle_command(&bot, &msg, &state, &text).await {
                        tracing::error!("Error handling command: {}", e);
                    }
                }
                // Handle music URLs
                else if text.contains("music.163.com")
                    || text.contains("163cn.tv")
                    || text.contains("163cn.link")
                {
                    if let Err(e) = handle_music_url(&bot, &msg, &state, &text).await {
                        tracing::error!("Error handling music URL: {}", e);
                    }
                }
            });
        }
    }
    Ok(())
}

async fn handle_command(
    bot: &Bot,
    msg: &Message,
    state: &Arc<BotState>,
    text: &str,
) -> ResponseResult<()> {
    let parts: Vec<&str> = text.split_whitespace().collect();
    let mut command = parts[0].trim_start_matches('/');

    // Remove bot username if present (e.g., "/start@BotName" -> "start")
    if let Some(at_pos) = command.find('@') {
        command = &command[..at_pos];
    }

    let args = if parts.len() > 1 {
        Some(parts[1..].join(" "))
    } else {
        None
    };

    // Only log music/search commands and admin commands
    match command {
        "music" | "netease" | "search" | "rmcache" => {
            tracing::info!("Command: /{} from chat {}", command, msg.chat.id);
        }
        _ => {} // Don't log about/start/status commands
    }

    match command {
        "start" => handle_start_command(bot, msg, state, args).await,
        "help" => handle_help_command(bot, msg, state).await,
        "music" | "netease" => handle_music_command(bot, msg, state, args).await,
        "search" => handle_search_command(bot, msg, state, args).await,
        "about" => handle_about_command(bot, msg, state).await,
        "lyric" => handle_lyric_command(bot, msg, state, args).await,
        "status" => handle_status_command(bot, msg, state).await,
        "rmcache" => handle_rmcache_command(bot, msg, state, args).await,
        _ => {
            // Unknown commands: don't respond (as requested)
            Ok(())
        }
    }
}

async fn handle_start_command(
    bot: &Bot,
    msg: &Message,
    state: &Arc<BotState>,
    args: Option<String>,
) -> ResponseResult<()> {
    if let Some(arg) = args {
        if let Ok(music_id) = arg.parse::<u64>() {
            // Check if we already have this in database
            if let Ok(Some(song_info)) = state.database.get_song_by_music_id(music_id as i64).await
            {
                if let Some(file_id) = song_info.file_id {
                    let caption = build_caption(
                        &song_info.song_name,
                        &song_info.song_artists,
                        &song_info.song_album,
                        &song_info.file_ext,
                        song_info.music_size,
                        song_info.bit_rate,
                        &state.bot_username,
                    );
                    let keyboard = create_music_keyboard(
                        song_info.music_id as u64,
                        &song_info.song_name,
                        &song_info.song_artists,
                    );

                    let mut send_audio = bot.send_audio(msg.chat.id, InputFile::file_id(file_id));
                    send_audio.caption = Some(caption);
                    send_audio.reply_markup = Some(ReplyMarkup::InlineKeyboard(keyboard));
                    send_audio.reply_to_message_id = Some(msg.id);

                    if let Some(thumb_id) = song_info.thumb_file_id {
                        send_audio.thumb = Some(InputFile::file_id(thumb_id));
                    }

                    send_audio.await?;
                    return Ok(());
                }
            }

            // Not in database or no file_id, trigger download flow
            return handle_music_url(
                bot,
                msg,
                state,
                &format!("https://music.163.com/song?id={}", music_id),
            )
            .await;
        }
    }

    let welcome_text = format!(
        "👋 欢迎使用网易云音乐机器人 <b>@{}</b>\n\n\
        我可以帮你解析网易云音乐链接、搜索音乐、获取歌词。\n\n\
        <b>主要功能：</b>\n\
        • 直接发送网易云音乐链接进行解析\n\
        • 使用 <code>/search &lt;关键词&gt;</code> 搜索音乐\n\
        • 在任何聊天中使用 <code>@{} &lt;关键词&gt;</code> 进行 Inline 搜索\n\
        • 使用 <code>/lyric &lt;关键词或ID&gt;</code> 获取歌词\n\n\
        <b>开源地址：</b> <a href=\"https://github.com/Lemonawa/music163bot-rust\">Lemonawa/music163bot-rust</a>",
        state.bot_username, state.bot_username
    );

    bot.send_message(msg.chat.id, welcome_text)
        .parse_mode(ParseMode::Html)
        .disable_web_page_preview(true)
        .reply_to_message_id(msg.id)
        .await?;

    Ok(())
}

async fn handle_help_command(
    bot: &Bot,
    msg: &Message,
    state: &Arc<BotState>,
) -> ResponseResult<()> {
    let help_text = format!(
        "📖 <b>使用帮助</b>\n\n\
        1️⃣ <b>直接解析</b>\n\
        发送网易云音乐链接给机器人，例如：\n\
        <code>https://music.163.com/song?id=12345</code>\n\n\
        2️⃣ <b>搜索音乐</b>\n\
        使用 <code>/search &lt;关键词&gt;</code> 在私聊中搜索。\n\n\
        3️⃣ <b>Inline 搜索</b>\n\
        在任何对话框输入 <code>@{} &lt;关键词&gt;</code> 即可快速搜索并分享音乐。\n\n\
        4️⃣ <b>获取歌词</b>\n\
        使用 <code>/lyric &lt;关键词或ID&gt;</code> 获取歌词。\n\n\
        5️⃣ <b>更多命令</b>\n\
        • <code>/status</code> - 查看系统状态\n\
        • <code>/about</code> - 关于机器人\n\n\
        💬 <b>项目主页：</b> <a href=\"https://github.com/Lemonawa/music163bot-rust\">GitHub</a>",
        state.bot_username
    );

    bot.send_message(msg.chat.id, help_text)
        .parse_mode(ParseMode::Html)
        .disable_web_page_preview(true)
        .reply_to_message_id(msg.id)
        .await?;

    Ok(())
}

async fn handle_music_command(
    bot: &Bot,
    msg: &Message,
    state: &Arc<BotState>,
    args: Option<String>,
) -> ResponseResult<()> {
    let args = args.unwrap_or_default();

    if args.is_empty() {
        bot.send_message(msg.chat.id, "请输入歌曲ID或歌曲关键词")
            .reply_to_message_id(msg.id)
            .await?;
        return Ok(());
    }

    // Try to parse as music ID first
    if let Some(music_id) = parse_music_id(&args) {
        return process_music(bot, msg, state, music_id).await;
    }

    // If not a number, search for the song
    match state.music_api.search_songs(&args, 1).await {
        Ok(songs) => {
            if let Some(song) = songs.first() {
                process_music(bot, msg, state, song.id).await
            } else {
                bot.send_message(msg.chat.id, "未找到相关歌曲")
                    .reply_to_message_id(msg.id)
                    .await?;
                Ok(())
            }
        }
        Err(e) => {
            bot.send_message(msg.chat.id, format!("搜索失败: {}", e))
                .reply_to_message_id(msg.id)
                .await?;
            Ok(())
        }
    }
}

async fn process_music(
    bot: &Bot,
    msg: &Message,
    state: &Arc<BotState>,
    music_id: u64,
) -> ResponseResult<()> {
    let music_id_i64 = music_id as i64;

    // Check if song is cached
    if let Ok(Some(cached_song)) = state.database.get_song_by_music_id(music_id_i64).await {
        // Validate cached file: must have file_id AND valid size (>1KB)
        if let Some(file_id) = &cached_song.file_id {
            if cached_song.music_size > 1024 {
                // Must be larger than 1KB
                // bitrate fallback if missing
                let bitrate = if cached_song.bit_rate > 0 {
                    cached_song.bit_rate
                } else {
                    let dur = (if cached_song.duration > 0 {
                        cached_song.duration
                    } else {
                        1
                    }) as f64;
                    (8.0 * cached_song.music_size as f64 / dur) as i64
                };
                let caption = build_caption(
                    &cached_song.song_name,
                    &cached_song.song_artists,
                    &cached_song.song_album,
                    &cached_song.file_ext,
                    cached_song.music_size,
                    bitrate,
                    &state.bot_username,
                );

                let keyboard = create_music_keyboard(
                    music_id,
                    &cached_song.song_name,
                    &cached_song.song_artists,
                );

                bot.send_audio(msg.chat.id, InputFile::file_id(file_id))
                    .caption(caption)
                    .reply_markup(keyboard)
                    .reply_to_message_id(msg.id)
                    .await?;

                return Ok(());
            } else {
                // Invalid cached file (too small), remove from database
                tracing::warn!(
                    "Removing invalid cached file for music_id {}: size {} bytes",
                    music_id,
                    cached_song.music_size
                );
                let _ = state.database.delete_song_by_music_id(music_id_i64).await;
            }
        }
    }

    // Send initial message
    let status_msg = bot
        .send_message(msg.chat.id, "🔄 正在获取歌曲信息...")
        .reply_to_message_id(msg.id)
        .await?;

    // Get song details
    let song_detail = match state.music_api.get_song_detail(music_id).await {
        Ok(detail) => detail,
        Err(e) => {
            bot.edit_message_text(
                msg.chat.id,
                status_msg.id,
                format!("❌ 获取歌曲信息失败: {}", e),
            )
            .await?;
            return Ok(());
        }
    };

    // Get download URL - try FLAC first if MUSIC_U is available, then fall back to MP3
    let song_url = if state.music_api.music_u.is_some() {
        // Try FLAC quality first for VIP users
        match state.music_api.get_song_url(music_id, 999000).await {
            Ok(url) if !url.url.is_empty() => {
                tracing::info!("Using FLAC quality for music_id {}", music_id);
                url
            }
            _ => {
                // Fallback to high quality MP3
                tracing::info!(
                    "FLAC not available, falling back to MP3 for music_id {}",
                    music_id
                );
                match state.music_api.get_song_url(music_id, 320000).await {
                    Ok(url) => url,
                    Err(e) => {
                        bot.edit_message_text(
                            msg.chat.id,
                            status_msg.id,
                            format!("❌ 获取下载链接失败: {}", e),
                        )
                        .await?;
                        return Ok(());
                    }
                }
            }
        }
    } else {
        // Get best available MP3 quality
        match state.music_api.get_song_url(music_id, 320000).await {
            Ok(url) => url,
            Err(_) => {
                // Try lower quality as fallback
                match state.music_api.get_song_url(music_id, 128000).await {
                    Ok(url) => url,
                    Err(e) => {
                        bot.edit_message_text(
                            msg.chat.id,
                            status_msg.id,
                            format!("❌ 获取下载链接失败: {}", e),
                        )
                        .await?;
                        return Ok(());
                    }
                }
            }
        }
    };

    if song_url.url.is_empty() {
        bot.edit_message_text(
            msg.chat.id,
            status_msg.id,
            "❌ 无法获取下载链接，可能需要VIP权限",
        )
        .await?;
        return Ok(());
    }

    // Update status
    let artists = format_artists(song_detail.ar.as_deref().unwrap_or(&[]));
    bot.edit_message_text(
        msg.chat.id,
        status_msg.id,
        format!("📥 正在下载: {} - {}", song_detail.name, artists),
    )
    .await?;

    // Download and process the song
    match download_and_send_music(bot, msg, state, &song_detail, &song_url, &status_msg).await {
        Ok(_) => {
            // Delete status message
            bot.delete_message(msg.chat.id, status_msg.id).await.ok();
        }
        Err(e) => {
            bot.edit_message_text(msg.chat.id, status_msg.id, format!("❌ 处理失败: {}", e))
                .await?;
        }
    }

    Ok(())
}

async fn download_and_send_music(
    bot: &Bot,
    msg: &Message,
    state: &Arc<BotState>,
    song_detail: &crate::music_api::SongDetail,
    song_url: &crate::music_api::SongUrl,
    status_msg: &Message,
) -> Result<()> {
    use tokio::io::AsyncWriteExt;

    let _permit = state.download_semaphore.acquire().await.unwrap();

    // Determine file extension
    let file_ext = if song_url.url.contains(".flac") {
        "flac"
    } else {
        "mp3"
    };

    let artists = format_artists(song_detail.ar.as_deref().unwrap_or(&[]));
    let filename = clean_filename(&format!(
        "{} - {}.{}",
        artists.replace('/', ","),
        song_detail.name,
        file_ext
    ));
    let file_path = format!("{}/{}", state.config.cache_dir, filename);

    // Ensure cache directory exists
    ensure_dir(&state.config.cache_dir)?;

    // Start parallel downloads: audio file and album art
    let artwork_future = async {
        if let Some(ref al) = song_detail.al {
            tracing::debug!("Album info found: id={}, name={}", al.id, al.name);
            if let Some(ref pic_url) = al.pic_url {
                if !pic_url.is_empty() {
                    tracing::info!(
                        "Starting album art download for music_id {}, pic_url: {}",
                        song_detail.id,
                        pic_url
                    );
                    let thumb_filename = format!(
                        "thumb_{}_{}.jpg",
                        song_detail.id,
                        chrono::Utc::now().timestamp()
                    );
                    let thumb_path = format!("{}/{}", state.config.cache_dir, thumb_filename);

                    match state
                        .music_api
                        .download_album_art(pic_url, std::path::Path::new(&thumb_path))
                        .await
                    {
                        Ok(_) => {
                            tracing::info!(
                                "✅ Downloaded album art for music_id {}, saved to: {}",
                                song_detail.id,
                                thumb_path
                            );
                            Some(thumb_path)
                        }
                        Err(e) => {
                            tracing::warn!(
                                "❌ Failed to download album art for music_id {}: {}",
                                song_detail.id,
                                e
                            );
                            None
                        }
                    }
                } else {
                    tracing::warn!("Album art URL is empty for music_id {}", song_detail.id);
                    None
                }
            } else {
                tracing::warn!("No pic_url found in album for music_id {}", song_detail.id);
                None
            }
        } else {
            tracing::warn!("No album info found for music_id {}", song_detail.id);
            None
        }
    };

    // Download audio file
    let audio_future = async {
        let response = state.music_api.download_file(&song_url.url).await?;

        // Check response status
        if !response.status().is_success() {
            return Err(anyhow::anyhow!("HTTP {}", response.status()));
        }

        // Check content length
        let content_length = response.content_length().unwrap_or(0);
        if content_length == 0 {
            return Err(anyhow::anyhow!("Empty file or unable to get file size"));
        }

        let mut file = tokio::fs::File::create(&file_path).await?;
        let mut stream = response.bytes_stream();
        let mut downloaded = 0u64;

        while let Some(chunk) = stream.next().await {
            let chunk = chunk?;
            downloaded += chunk.len() as u64;
            file.write_all(&chunk).await?;
        }
        file.flush().await?;

        Ok::<u64, anyhow::Error>(downloaded)
    };

    // Execute both downloads in parallel
    let (downloaded_result, thumbnail_path) = tokio::join!(audio_future, artwork_future);
    let downloaded = downloaded_result?;

    tracing::info!("✅ Audio download completed: {} bytes", downloaded);
    tracing::info!(
        "✅ Cover download result: {}",
        thumbnail_path.as_deref().unwrap_or("None")
    );

    // Simple file existence and size check
    let file_metadata = tokio::fs::metadata(&file_path).await?;
    let actual_size = file_metadata.len();

    if actual_size == 0 {
        let _ = tokio::fs::remove_file(&file_path).await;
        bot.edit_message_text(msg.chat.id, status_msg.id, "❌ 下载失败: 文件为空")
            .await?;
        return Ok(());
    }

    if actual_size < 1024 {
        let _ = tokio::fs::remove_file(&file_path).await;
        bot.edit_message_text(
            msg.chat.id,
            status_msg.id,
            format!("❌ 下载失败: 文件太小({} bytes)", actual_size),
        )
        .await?;
        return Ok(());
    }

    tracing::info!("✅ File validation passed: {} bytes", actual_size);

    // 封面处理：先确保有封面文件，再根据格式处理
    tracing::info!("� Processing cover art for {} format", file_ext);

    let cover_path = if let Some(ref thumb) = thumbnail_path {
        tracing::info!("Using parallel downloaded cover: {}", thumb);
        Some(thumb.clone())
    } else {
        // 并行下载失败，重新尝试下载封面
        tracing::info!("Parallel cover download failed, retrying...");
        if let Some(ref al) = song_detail.al {
            if let Some(ref pic_url) = al.pic_url {
                if !pic_url.is_empty() {
                    let thumb_filename = format!(
                        "thumb_{}_{}.jpg",
                        song_detail.id,
                        chrono::Utc::now().timestamp()
                    );
                    let thumb_path = format!("{}/{}", state.config.cache_dir, thumb_filename);
                    match state
                        .music_api
                        .download_album_art(pic_url, std::path::Path::new(&thumb_path))
                        .await
                    {
                        Ok(_) => {
                            tracing::info!("✅ Successfully downloaded cover: {}", thumb_path);
                            Some(thumb_path)
                        }
                        Err(e) => {
                            tracing::warn!("Cover download failed: {}", e);
                            None
                        }
                    }
                } else {
                    tracing::info!("No cover URL available");
                    None
                }
            } else {
                None
            }
        } else {
            None
        }
    };

    // 根据文件格式嵌入封面
    let final_thumbnail_path = if let Some(ref cover) = cover_path {
        match file_ext {
            "mp3" => {
                tracing::info!("🎵 Adding ID3 tags to MP3: {}", file_path);
                match add_id3_tags_with_artwork(&file_path, song_detail, Some(cover)).await {
                    Ok(_) => tracing::info!("✅ MP3 tags added successfully"),
                    Err(e) => tracing::warn!("Failed to add MP3 tags: {}", e),
                }
                Some(cover.clone())
            }
            "flac" => {
                tracing::info!("🎵 Adding PICTURE block to FLAC: {}", file_path);
                match add_flac_picture_with_artwork(&file_path, cover).await {
                    Ok(_) => tracing::info!("✅ FLAC cover embedded successfully"),
                    Err(e) => tracing::warn!("Failed to embed FLAC cover: {}", e),
                }
                Some(cover.clone())
            }
            _ => {
                tracing::info!("Unknown format {}, skipping cover embedding", file_ext);
                Some(cover.clone())
            }
        }
    } else {
        tracing::info!("No cover available, processing audio only");
        // 即使没有封面，MP3也要写基础标签
        if file_ext == "mp3" {
            tracing::info!("Adding basic ID3 tags to MP3 (no cover)");
            match add_id3_tags_with_artwork(&file_path, song_detail, None).await {
                Ok(_) => tracing::info!("✅ Basic MP3 tags added"),
                Err(e) => tracing::warn!("Failed to add basic MP3 tags: {}", e),
            }
        }
        None
    };

    // Create song info for database
    let mut song_info = SongInfo {
        music_id: song_detail.id as i64,
        song_name: song_detail.name.clone(),
        song_artists: artists.clone(),
        song_album: song_detail
            .al
            .as_ref()
            .map(|al| al.name.clone())
            .unwrap_or_else(|| "Unknown Album".to_string()),
        file_ext: file_ext.to_string(),
        music_size: downloaded as i64,
        pic_size: 0,
        emb_pic_size: 0,
        bit_rate: song_url.br as i64,
        duration: (song_detail.dt.unwrap_or(0) / 1000) as i64,
        file_id: None,
        thumb_file_id: None,
        from_user_id: msg.from().map(|u| u.id.0 as i64).unwrap_or(0),
        from_user_name: msg
            .from()
            .and_then(|u| u.username.clone())
            .unwrap_or_default(),
        from_chat_id: msg.chat.id.0,
        from_chat_name: msg.chat.username().unwrap_or("").to_string(),
        created_at: chrono::Utc::now(),
        updated_at: chrono::Utc::now(),
        ..Default::default()
    };

    // Log final thumbnail status
    tracing::info!(
        "Final thumbnail status: {}",
        if final_thumbnail_path.is_some() {
            "Available"
        } else {
            "None"
        }
    );

    // Send the audio file
    let caption = build_caption(
        &song_info.song_name,
        &song_info.song_artists,
        &song_info.song_album,
        &song_info.file_ext,
        song_info.music_size,
        song_info.bit_rate,
        &state.bot_username,
    );

    let keyboard = create_music_keyboard(
        song_detail.id,
        &song_info.song_name,
        &song_info.song_artists,
    );

    // Use file path directly for size check
    let file_size = match std::fs::metadata(&file_path) {
        Ok(metadata) => {
            if metadata.len() == 0 {
                return Err(anyhow::anyhow!("Audio file is empty: {}", file_path).into());
            }
            metadata.len()
        }
        Err(e) => {
            return Err(anyhow::anyhow!("Cannot access audio file {}: {}", file_path, e).into());
        }
    };

    // Resolve absolute path for upload
    let absolute_path =
        std::fs::canonicalize(&file_path).unwrap_or_else(|_| std::path::PathBuf::from(&file_path));

    tracing::info!(
        "Prepared audio file: {} (abs: {}) ({:.2} MB)",
        file_path,
        absolute_path.display(),
        file_size as f64 / 1024.0 / 1024.0
    );

    // Build a dedicated upload bot. If a custom API is configured, use it but with an upload-optimized HTTP client.
    let (upload_bot, used_custom_api) =
        if !state.config.bot_api.is_empty() && state.config.bot_api != "https://api.telegram.org" {
            // Normalize API URL (ensure it ends with /bot)
            let api_url_str = if state.config.bot_api.ends_with("/bot") {
                state.config.bot_api.clone()
            } else {
                format!("{}/bot", state.config.bot_api)
            };

            let api_url = reqwest::Url::parse(&api_url_str)
                .unwrap_or_else(|_| reqwest::Url::parse("https://api.telegram.org/bot").unwrap());
            tracing::info!("Using custom API for upload: {}", api_url);

            // Create a client optimized for multipart uploads
            let client = reqwest::Client::builder()
                .use_rustls_tls()
                .timeout(std::time::Duration::from_secs(300)) // large files need longer timeouts
                .pool_max_idle_per_host(0)
                .no_gzip() // avoid gzip interference on multipart boundaries via proxies
                .user_agent("Go-http-client/2.0")
                .default_headers(reqwest::header::HeaderMap::new())
                .build()
                .unwrap();

            (
                Bot::with_client(&state.config.bot_token, client).set_api_url(api_url),
                true,
            )
        } else {
            (bot.clone(), false)
        };

    // Send audio file with enhanced error handling and proper MIME type
    tracing::info!(
        "Sending audio file: {} ({:.2} MB)",
        file_path,
        file_size as f64 / 1024.0 / 1024.0
    );

    // Simple approach: try sending as audio first, fallback to document if needed
    let is_flac = file_path.ends_with(".flac");

    tracing::info!("File format: {}", if is_flac { "FLAC" } else { "MP3" });

    // Try sending as audio with basic metadata
    let mut audio_req = upload_bot
        .send_audio(msg.chat.id, InputFile::file(&absolute_path))
        .caption(&caption)
        .title(&song_info.song_name)
        .performer(&song_info.song_artists)
        .duration(song_info.duration as u32)
        .reply_markup(keyboard.clone())
        .reply_to_message_id(msg.id);

    // Attach thumbnail if available
    if let Some(ref thumb) = final_thumbnail_path {
        audio_req = audio_req.thumb(InputFile::file(std::path::Path::new(thumb)));
    }

    // Thumbnail will be embedded into tags for MP3 and FLAC (when possible)
    let audio_result = audio_req.await;

    match audio_result {
        Ok(sent_msg) => {
            tracing::info!(
                "Successfully sent as audio: {}",
                if is_flac { "FLAC" } else { "MP3" }
            );

            // Extract file_id from sent message
            if let MessageKind::Common(common) = &sent_msg.kind {
                if let teloxide::types::MediaKind::Audio(audio) = &common.media_kind {
                    song_info.file_id = Some(audio.audio.file.id.clone());
                }
            }
        }
        Err(e) => {
            tracing::warn!("Audio send failed: {}, trying document fallback", e);

            // Fallback: send as document
            let doc_req = upload_bot
                .send_document(msg.chat.id, InputFile::file(&absolute_path))
                .caption(&caption)
                .reply_markup(keyboard)
                .reply_to_message_id(msg.id);
            // For document, Telegram may not show embedded art; we still embed where possible
            let doc_result = doc_req.await;

            match doc_result {
                Ok(sent_msg) => {
                    tracing::info!("Successfully sent as document");
                    if let MessageKind::Common(common) = &sent_msg.kind {
                        if let teloxide::types::MediaKind::Document(document) = &common.media_kind {
                            song_info.file_id = Some(document.document.file.id.clone());
                        }
                    }
                }
                Err(doc_err) => {
                    tracing::error!("Both audio and document send failed via custom/primary API");
                    // If we were using a custom API, try one last fallback using the official API for upload
                    if used_custom_api {
                        tracing::warn!("Retrying upload via official Telegram API as fallback");
                        let official_bot = Bot::new(&state.config.bot_token);
                        let retry_req = official_bot
                            .send_document(msg.chat.id, InputFile::file(&absolute_path))
                            .caption(&caption)
                            .reply_to_message_id(msg.id);
                        // retry without explicit thumbnail method
                        let retry = retry_req.await;
                        match retry {
                            Ok(sent_msg) => {
                                tracing::info!("Upload succeeded via official API fallback");
                                if let MessageKind::Common(common) = &sent_msg.kind {
                                    if let teloxide::types::MediaKind::Document(document) =
                                        &common.media_kind
                                    {
                                        song_info.file_id = Some(document.document.file.id.clone());
                                    }
                                }
                            }
                            Err(final_err) => {
                                bot.edit_message_text(
                                    msg.chat.id,
                                    status_msg.id,
                                    format!("❌ 发送失败: {}", final_err),
                                )
                                .await
                                .ok();
                                return Err(final_err.into());
                            }
                        }
                    } else {
                        bot.edit_message_text(
                            msg.chat.id,
                            status_msg.id,
                            format!("❌ 发送失败: {}", doc_err),
                        )
                        .await
                        .ok();
                        return Err(doc_err.into());
                    }
                }
            }
        }
    }

    // Save to database
    state.database.save_song_info(&song_info).await?;

    // Clean up downloaded files
    std::fs::remove_file(&file_path).ok();
    if let Some(thumb_path) = thumbnail_path {
        std::fs::remove_file(&thumb_path).ok();
    }

    // Delete status message
    bot.delete_message(msg.chat.id, status_msg.id).await.ok();

    Ok(())
}

fn create_music_keyboard(music_id: u64, song_name: &str, artists: &str) -> InlineKeyboardMarkup {
    InlineKeyboardMarkup::new(vec![
        vec![InlineKeyboardButton::url(
            format!("{} - {}", song_name, artists),
            reqwest::Url::parse(&format!("https://music.163.com/song?id={}", music_id)).unwrap(),
        )],
        vec![InlineKeyboardButton::switch_inline_query(
            "分享给朋友",
            format!("https://music.163.com/song?id={}", music_id),
        )],
    ])
}

async fn handle_music_url(
    bot: &Bot,
    msg: &Message,
    state: &Arc<BotState>,
    text: &str,
) -> ResponseResult<()> {
    if let Some(music_id) = parse_music_id(text) {
        process_music(bot, msg, state, music_id).await
    } else {
        bot.send_message(msg.chat.id, "无法从链接中提取音乐ID")
            .reply_to_message_id(msg.id)
            .await?;
        Ok(())
    }
}

async fn handle_search_command(
    bot: &Bot,
    msg: &Message,
    state: &Arc<BotState>,
    args: Option<String>,
) -> ResponseResult<()> {
    let keyword = match args {
        Some(kw) if !kw.is_empty() => kw,
        _ => {
            bot.send_message(msg.chat.id, "请输入搜索关键词")
                .reply_to_message_id(msg.id)
                .await?;
            return Ok(());
        }
    };

    let search_msg = bot
        .send_message(msg.chat.id, "🔍 搜索中...")
        .reply_to_message_id(msg.id)
        .await?;

    match state.music_api.search_songs(&keyword, 10).await {
        Ok(songs) => {
            if songs.is_empty() {
                bot.edit_message_text(msg.chat.id, search_msg.id, "未找到相关歌曲")
                    .await?;
                return Ok(());
            }

            let mut results = String::from("🔍 搜索结果：\n\n");
            for (i, song) in songs.iter().take(5).enumerate() {
                let artists = format_artists(&song.artists);
                results.push_str(&format!(
                    "{}. {} - {}\n   💿 {}\n   🆔 {}\n\n",
                    i + 1,
                    song.name,
                    artists,
                    song.album.name,
                    song.id
                ));
            }
            results.push_str("💡 使用 `/music <ID>` 获取歌曲");

            bot.edit_message_text(msg.chat.id, search_msg.id, results)
                .await?;
        }
        Err(e) => {
            bot.edit_message_text(msg.chat.id, search_msg.id, format!("搜索失败: {}", e))
                .await?;
        }
    }

    Ok(())
}

async fn handle_about_command(
    bot: &Bot,
    msg: &Message,
    _state: &Arc<BotState>,
) -> ResponseResult<()> {
    let about_text = format!(
        r#"🎵 Music163bot-Rust v{}

一个用来下载/分享/搜索网易云歌曲的 Telegram Bot

特性：
• 🔗 分享链接嗅探
• 🎵 歌曲搜索与下载
• 💾 智能缓存系统
• 🎤 歌词获取
• 📊 使用统计

技术栈：
• 🦀 Rust + Teloxide
• 🔧 高并发处理
• 📦 轻量级部署

源码：GitHub | 原版：Music163bot-Go"#,
        env!("CARGO_PKG_VERSION")
    );

    bot.send_message(msg.chat.id, about_text)
        .reply_to_message_id(msg.id)
        .disable_web_page_preview(true)
        .await?;

    Ok(())
}

async fn handle_lyric_command(
    bot: &Bot,
    msg: &Message,
    state: &Arc<BotState>,
    args: Option<String>,
) -> ResponseResult<()> {
    let args = args.unwrap_or_default();

    if args.is_empty() {
        bot.send_message(msg.chat.id, "请输入歌曲ID或关键词")
            .reply_to_message_id(msg.id)
            .await?;
        return Ok(());
    }

    let music_id = if let Some(id) = parse_music_id(&args) {
        id
    } else {
        // Search for song first
        match state.music_api.search_songs(&args, 1).await {
            Ok(songs) => {
                if let Some(song) = songs.first() {
                    song.id
                } else {
                    bot.send_message(msg.chat.id, "未找到相关歌曲")
                        .reply_to_message_id(msg.id)
                        .await?;
                    return Ok(());
                }
            }
            Err(e) => {
                bot.send_message(msg.chat.id, format!("搜索失败: {}", e))
                    .reply_to_message_id(msg.id)
                    .await?;
                return Ok(());
            }
        }
    };

    let status_msg = bot
        .send_message(msg.chat.id, "🎵 正在获取歌词...")
        .reply_to_message_id(msg.id)
        .await?;

    match state.music_api.get_song_lyric(music_id).await {
        Ok(lyric) => {
            let formatted_lyric = if lyric.trim().is_empty() {
                "该歌曲暂无歌词".to_string()
            } else {
                // Clean up lyric format
                lyric
                    .lines()
                    .filter(|line| !line.trim().is_empty())
                    .map(|line| {
                        // Remove timestamp like [00:12.34]
                        let re = regex::Regex::new(r"\[\d+:\d+\.\d+\]").unwrap();
                        re.replace(line, "").trim().to_string()
                    })
                    .filter(|line| !line.is_empty())
                    .collect::<Vec<_>>()
                    .join("\n")
            };

            // Telegram has a message length limit
            let max_length = 4000;
            let final_lyric = if formatted_lyric.len() > max_length {
                format!("{}...\n\n歌词过长，已截断", &formatted_lyric[..max_length])
            } else {
                formatted_lyric
            };

            bot.edit_message_text(
                msg.chat.id,
                status_msg.id,
                format!("🎵 歌词：\n\n{}", final_lyric),
            )
            .await?;
        }
        Err(e) => {
            bot.edit_message_text(msg.chat.id, status_msg.id, format!("获取歌词失败: {}", e))
                .await?;
        }
    }

    Ok(())
}

async fn handle_status_command(
    bot: &Bot,
    msg: &Message,
    state: &Arc<BotState>,
) -> ResponseResult<()> {
    let user_id = msg.from().map(|u| u.id.0 as i64).unwrap_or(0);
    let chat_id = msg.chat.id.0;

    let total_count = state.database.count_total_songs().await.unwrap_or(0);
    let user_count = state
        .database
        .count_songs_from_user(user_id)
        .await
        .unwrap_or(0);
    let chat_count = state
        .database
        .count_songs_from_chat(chat_id)
        .await
        .unwrap_or(0);

    let status_text = format!(
        r#"📊 *统计信息*

🎵 数据库中总缓存歌曲数量: {}
👤 当前用户缓存歌曲数量: {}
💬 当前对话缓存歌曲数量: {}

🤖 Bot 运行状态: 正常
🦀 语言: Rust
⚡ 框架: Teloxide
"#,
        total_count, user_count, chat_count
    );

    bot.send_message(msg.chat.id, status_text)
        .parse_mode(ParseMode::MarkdownV2)
        .reply_to_message_id(msg.id)
        .await?;

    Ok(())
}

async fn handle_rmcache_command(
    bot: &Bot,
    msg: &Message,
    state: &Arc<BotState>,
    args: Option<String>,
) -> ResponseResult<()> {
    // Check if user is admin
    let user_id = msg.from().map(|u| u.id.0 as i64).unwrap_or(0);

    tracing::info!(
        "rmcache command from user_id: {}, configured admins: {:?}",
        user_id,
        state.config.bot_admin
    );

    if !state.config.bot_admin.contains(&user_id) {
        bot.send_message(msg.chat.id, "❌ 该命令仅限管理员使用")
            .reply_to_message_id(msg.id)
            .await?;
        return Ok(());
    }

    let args = args.unwrap_or_default();

    if args.is_empty() {
        bot.send_message(
            msg.chat.id,
            "请输入要删除缓存的歌曲ID\n\n用法: `/rmcache <音乐ID>`",
        )
        .reply_to_message_id(msg.id)
        .await?;
        return Ok(());
    }

    if let Some(music_id) = parse_music_id(&args) {
        let music_id_i64 = music_id as i64;

        // Get song info before deletion
        if let Ok(Some(song_info)) = state.database.get_song_by_music_id(music_id_i64).await {
            match state.database.delete_song_by_music_id(music_id_i64).await {
                Ok(deleted) => {
                    if deleted {
                        bot.send_message(
                            msg.chat.id,
                            format!("✅ 已删除歌曲缓存: {}", song_info.song_name),
                        )
                        .reply_to_message_id(msg.id)
                        .await?;
                    } else {
                        bot.send_message(msg.chat.id, "歌曲未缓存")
                            .reply_to_message_id(msg.id)
                            .await?;
                    }
                }
                Err(e) => {
                    bot.send_message(msg.chat.id, format!("删除缓存失败: {}", e))
                        .reply_to_message_id(msg.id)
                        .await?;
                }
            }
        } else {
            bot.send_message(msg.chat.id, "歌曲未缓存")
                .reply_to_message_id(msg.id)
                .await?;
        }
    } else {
        bot.send_message(msg.chat.id, "无效的歌曲ID")
            .reply_to_message_id(msg.id)
            .await?;
    }

    Ok(())
}

async fn handle_callback(
    _bot: Bot,
    _query: CallbackQuery,
    _state: Arc<BotState>,
) -> ResponseResult<()> {
    // TODO: Implement callback handling
    Ok(())
}

/// Add ID3 tags with album artwork to MP3 file
async fn add_id3_tags_with_artwork(
    file_path: &str,
    song_detail: &crate::music_api::SongDetail,
    artwork_path: Option<&str>,
) -> Result<()> {
    use id3::{frame, Tag, TagLike};
    use std::path::Path;

    // Only process MP3 files
    if !file_path.ends_with(".mp3") {
        tracing::debug!("Skipping ID3 tags for non-MP3 file: {}", file_path);
        return Ok(());
    }

    let path = Path::new(file_path);
    if !path.exists() {
        tracing::warn!("MP3 file not found for ID3 tagging: {}", file_path);
        return Ok(());
    }

    // Create and write ID3 tags
    let mut tag = Tag::new();

    // Basic metadata
    tag.set_title(&song_detail.name);
    let album_name = song_detail
        .al
        .as_ref()
        .map(|al| al.name.as_str())
        .unwrap_or("Unknown Album");
    tag.set_album(album_name);
    tag.set_artist(format_artists(song_detail.ar.as_deref().unwrap_or(&[])));

    // Duration in seconds
    tag.set_duration((song_detail.dt.unwrap_or(0) / 1000) as u32);

    // Add album artwork if provided
    if let Some(artwork_path) = artwork_path {
        tracing::info!("Attempting to add album artwork to ID3: {}", artwork_path);
        if Path::new(artwork_path).exists() {
            match std::fs::read(artwork_path) {
                Ok(artwork_data) => {
                    tracing::info!("Read artwork file: {} bytes", artwork_data.len());
                    let picture = frame::Picture {
                        mime_type: "image/jpeg".to_string(),
                        picture_type: frame::PictureType::CoverFront,
                        description: "Album Cover".to_string(),
                        data: artwork_data,
                    };
                    tag.add_frame(picture);
                    tracing::info!("✅ Added album artwork to ID3 tags for {}", file_path);
                }
                Err(e) => {
                    tracing::warn!("Failed to read artwork file {}: {}", artwork_path, e);
                }
            }
        } else {
            tracing::warn!("Artwork file not found: {}", artwork_path);
        }
    } else {
        tracing::info!("No artwork provided for MP3: {}", file_path);
    }

    // Save the tag
    match tag.write_to_path(file_path, id3::Version::Id3v24) {
        Ok(_) => tracing::info!("✅ ID3 tags written successfully to {}", file_path),
        Err(e) => tracing::warn!("Failed to write ID3 tags to {}: {}", file_path, e),
    }

    Ok(())
}

async fn handle_inline_query(
    bot: Bot,
    query: InlineQuery,
    state: Arc<BotState>,
) -> ResponseResult<()> {
    let text = query.query.trim();
    if text.is_empty() {
        // Return help information via inline
        let help_article = InlineQueryResultArticle::new(
            "usage_help",
            "如何使用此机器人？",
            InputMessageContent::Text(InputMessageContentText::new(format!(
                "使用方法：\n1. 直接输入关键词搜索音乐\n2. 粘贴网易云音乐链接\n3. 输入歌曲 ID"
            ))),
        )
        .description("在输入框中输入关键词开始搜索音乐");

        bot.answer_inline_query(&query.id, vec![InlineQueryResult::Article(help_article)])
            .await?;
        return Ok(());
    }

    // Perform search
    match state.music_api.search_songs(text, 20).await {
        Ok(songs) => {
            let mut results = Vec::new();

            for song in songs {
                let _artists = format_artists(&song.artists);

                // Check if cached
                let is_cached = if let Ok(Some(info)) =
                    state.database.get_song_by_music_id(song.id as i64).await
                {
                    info.file_id.is_some()
                } else {
                    false
                };

                let description = if is_cached {
                    format!("✅ 已缓存 | 专辑: {}", song.album.name)
                } else {
                    format!("专辑: {}", song.album.name)
                };

                let mut article = InlineQueryResultArticle::new(
                    song.id.to_string(),
                    &song.name,
                    InputMessageContent::Text(InputMessageContentText::new(format!(
                        "/netease {}",
                        song.id
                    ))),
                )
                .description(description);

                if let Some(ref pic_url) = song.album.pic_url {
                    article.thumb_url = Some(reqwest::Url::parse(pic_url).unwrap());
                }

                results.push(InlineQueryResult::Article(article));
            }

            bot.answer_inline_query(&query.id, results)
                .cache_time(300)
                .await?;
        }
        Err(e) => {
            tracing::error!("Inline search error: {}", e);
        }
    }

    Ok(())
}

/// Add FLAC PICTURE (front cover) using JPEG artwork
async fn add_flac_picture_with_artwork(flac_path: &str, artwork_path: &str) -> Result<()> {
    use metaflac::block::{Picture, PictureType};
    use metaflac::Tag;
    use std::path::Path;

    if !flac_path.ends_with(".flac") {
        tracing::debug!("Skipping FLAC cover for non-FLAC file: {}", flac_path);
        return Ok(());
    }

    let fpath = Path::new(flac_path);
    let apath = Path::new(artwork_path);
    if !fpath.exists() {
        tracing::warn!("FLAC file not found: {}", flac_path);
        return Ok(());
    }
    if !apath.exists() {
        tracing::warn!("Artwork file not found for FLAC: {}", artwork_path);
        return Ok(());
    }

    tracing::info!("Reading FLAC metadata from: {}", flac_path);
    // Read or create a tag
    let mut tag = match Tag::read_from_path(fpath) {
        Ok(t) => {
            tracing::info!("Successfully read existing FLAC metadata");
            t
        }
        Err(e) => {
            tracing::info!("Creating new FLAC metadata (read failed: {})", e);
            Tag::new()
        }
    };

    // Remove existing front covers to avoid duplicates
    tracing::info!("Removing existing front cover pictures");
    tag.remove_picture_type(PictureType::CoverFront);

    // Read image bytes
    tracing::info!("Reading artwork file: {}", artwork_path);
    let data = std::fs::read(apath)?;
    tracing::info!("Read artwork: {} bytes", data.len());

    // Try to infer dimensions via image crate (optional but helps some players)
    let (width, height) = match image::load_from_memory(&data) {
        Ok(img) => {
            let (w, h) = (img.width(), img.height());
            tracing::info!("Artwork dimensions: {}x{}", w, h);
            (w, h)
        }
        Err(e) => {
            tracing::warn!("Failed to decode artwork for dimensions (using 0x0): {}", e);
            (0, 0)
        }
    };

    let mut pic = Picture::new();
    pic.picture_type = PictureType::CoverFront;
    pic.mime_type = "image/jpeg".to_string();
    pic.description = "Album Cover".to_string();
    pic.width = width;
    pic.height = height;
    pic.depth = 24; // JPEG typically 24-bit
    pic.num_colors = 0;
    pic.data = data;

    tracing::info!("Adding PICTURE block to FLAC metadata");
    // Add to tag and write back
    tag.push_block(metaflac::Block::Picture(pic));

    // If we read from a file, prefer saving back to same path via save();
    // otherwise, write_to_path.
    // Use write_to_path to be explicit and robust.
    tracing::info!("Writing FLAC metadata back to file");
    tag.write_to_path(fpath)
        .map_err(|e| anyhow::anyhow!("metaflac write failed: {}", e))?;
    tracing::info!("✅ Embedded FLAC cover into {}", flac_path);
    Ok(())
}

/// Build caption with exact format:
/// 「Title」- Artists
/// 专辑: Album
/// #网易云音乐 #ext {sizeMB}MB {kbps}kbps
/// via @BotName
fn build_caption(
    title: &str,
    artists: &str,
    album: &str,
    file_ext: &str,
    size_bytes: i64,
    bitrate_bps: i64,
    bot_username: &str,
) -> String {
    let size_mb = (size_bytes as f64) / 1024.0 / 1024.0;
    // bitrate_bps may already be bps, convert to kbps with 2 decimals
    let kbps = (bitrate_bps as f64) / 1000.0;
    let ext = file_ext.to_lowercase();
    format!(
        "「{}」- {}\n专辑: {}\n#网易云音乐 #{} {:.2}MB {:.2}kbps\nvia @{}",
        title, artists, album, ext, size_mb, kbps, bot_username,
    )
}
