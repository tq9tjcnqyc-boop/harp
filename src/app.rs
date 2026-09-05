use std::path::PathBuf;
use std::sync::mpsc::{self, Receiver};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use anyhow::{Context, Result};
use rand::Rng;
use ratatui_image::picker::Picker;
use ratatui_image::protocol::StatefulProtocol;

use crate::library;
use crate::lyrics::Lyrics;
use crate::model::{PlayMode, SearchResult, SearchSource, Track};
use crate::playback::Playback;
use crate::state::{self, SavedState};

type SharedDownloadProgress = Arc<Mutex<(u64, u64)>>;

type SharedDownloadResult = Arc<Mutex<Option<Result<PathBuf, String>>>>;

pub struct App {
    pub source_id: String,
    pub source_dir: PathBuf,
    pub tracks: Vec<Track>,
    pub current: usize,
    pub queue_selected: usize,
    pub queue_scroll: usize,
    queue_marquee_started: Instant,
    queue_marquee_selected: usize,
    queue_marquee_steps: usize,
    pub playback: Playback,
    pub mode: PlayMode,
    pub lyrics: Lyrics,
    pub lyric_scroll: usize,
    pub lyrics_focused: bool,
    pub cover: Option<StatefulProtocol>,
    pub picker: Picker,
    pub cover_index: Option<usize>,
    cover_render_size: Option<(u16, u16)>,
    cover_source_index: Option<usize>,
    cover_source: Option<image::DynamicImage>,
    pub status: Option<String>,
    pub status_started: Option<Instant>,
    pub help_visible: bool,
    pub needs_full_redraw: bool,
    pub should_quit: bool,
    pub finished: bool,
    pub is_dark: bool,
    pub state_path: PathBuf,
    pub last_save: Instant,

    download_progress: Option<SharedDownloadProgress>,
    download_result: Option<SharedDownloadResult>,
    pub search_input: Option<String>,
    pub search_cursor: usize,
    pub search_source: SearchSource,
    pub search_results: Vec<SearchResult>,
    pub search_selected: usize,
    pub delete_confirm: bool,

    search_quality_updates: Option<Receiver<(String, String)>>,
}

impl App {
    pub fn new(source: PathBuf, force_silent: bool, picker: Picker) -> Result<Self> {
        let source_id = library::source_identity(&source);
        let tracks = library::scan(&source)?;
        let state_path = state::state_path()?;
        let saved = state::load(&state_path);
        let matching_source = saved.source == source_id;

        let current = if matching_source {
            tracks
                .iter()
                .position(|track| track_identity(&track.path) == saved.track)
                .unwrap_or(0)
        } else {
            0
        };
        let volume = if matching_source {
            saved.volume
        } else {
            SavedState::default().volume
        };
        let mode = if matching_source {
            saved.mode
        } else {
            PlayMode::Sequential
        };
        let start = if matching_source {
            Duration::from_millis(saved.position_ms).min(tracks[current].duration)
        } else {
            Duration::ZERO
        };
        let mut playback = Playback::new(volume, force_silent);
        if tracks[current].path.exists() {
            playback
                .load(&tracks[current].path, start, true)
                .context("无法载入第一首歌曲")?;
        }
        let status = if !tracks[current].path.exists() {
            Some("目录为空，按 / 搜索并下载歌曲".to_owned())
        } else {
            playback
                .unavailable_reason()
                .map(|reason| format!("{reason}；仍可浏览封面、歌词和队列"))
                .or_else(|| Some("已恢复，按 Space 开始播放".to_owned()))
        };

        let lyrics = tracks[current]
            .lyrics
            .as_deref()
            .map(Lyrics::parse)
            .unwrap_or_default();

        let status_started = status.as_ref().map(|_| Instant::now());

        Ok(Self {
            source_dir: source,
            source_id,
            tracks,
            current,
            queue_selected: current,
            queue_scroll: 0,
            queue_marquee_started: Instant::now(),
            queue_marquee_selected: current,
            queue_marquee_steps: 0,
            playback,
            mode,
            lyrics,
            lyric_scroll: 0,
            lyrics_focused: false,
            cover: None,
            picker,
            cover_index: None,
            cover_render_size: None,
            cover_source_index: None,
            cover_source: None,
            status,
            status_started,
            help_visible: false,
            needs_full_redraw: false,
            should_quit: false,
            finished: false,
            is_dark: true,
            state_path,
            last_save: Instant::now(),
            download_progress: None,
            download_result: None,
            search_input: None,
            search_cursor: 0,
            search_source: SearchSource::default(),
            search_results: Vec::new(),
            search_selected: 0,
            delete_confirm: false,
            search_quality_updates: None,
        })
    }

    fn set_status(&mut self, message: impl Into<String>) {
        self.status = Some(message.into());
        self.status_started = Some(Instant::now());
    }

    fn clear_status(&mut self) {
        self.status = None;
        self.status_started = None;
    }

    pub fn search_key(&mut self, key: char) {
        if let Some(input) = self.search_input.as_mut() {
            let byte = char_to_byte_index(input, self.search_cursor);
            input.insert(byte, key);
            self.search_cursor += 1;
        }
    }
    pub fn search_backspace(&mut self) {
        if self.search_cursor == 0 {
            return;
        }
        if let Some(input) = self.search_input.as_mut() {
            let start = char_to_byte_index(input, self.search_cursor - 1);
            let end = char_to_byte_index(input, self.search_cursor);

            input.replace_range(start..end, "");
            self.search_cursor -= 1;
        }
    }
    pub fn search_delete(&mut self) {
        if let Some(input) = self.search_input.as_mut() {
            let length = input.chars().count();
            if self.search_cursor >= length {
                return;
            }
            let start = char_to_byte_index(input, self.search_cursor);
            let end = char_to_byte_index(input, self.search_cursor + 1);
            input.replace_range(start..end, "");
        }
    }
    pub fn move_search_cursor(&mut self, delta: i32) {
        let length = self
            .search_input
            .as_ref()
            .map_or(0, |input| input.chars().count());

        self.search_cursor = if delta < 0 {
            self.search_cursor
                .saturating_sub(delta.unsigned_abs() as usize)
        } else {
            self.search_cursor
                .saturating_add(delta as usize)
                .min(length)
        };
    }
    pub fn search_cursor_home(&mut self) {
        self.search_cursor = 0;
    }
    pub fn search_cursor_end(&mut self) {
        self.search_cursor = self
            .search_input
            .as_ref()
            .map_or(0, |input| input.chars().count());
    }
    pub fn start_search(&mut self) {
        self.search_input = Some(String::new());
        self.search_cursor = 0;
        self.search_results.clear();
        self.search_quality_updates = None;
    }

    pub fn toggle_search_source(&mut self) {
        self.search_source = match self.search_source {
            SearchSource::Netease => SearchSource::Qq,
            SearchSource::Qq => SearchSource::Tidal,
            SearchSource::Tidal => SearchSource::Netease,
        };
        self.search_results.clear();
        self.search_quality_updates = None;
        self.set_status(format!("已切换到{}搜索", self.search_source.label()));
    }

    pub fn finish_search(&mut self) {
        let Some(query) = self.search_input.take() else {
            return;
        };
        if query.trim().is_empty() {
            return;
        }

        let searched = match self.search_source {
            SearchSource::Netease => {
                crate::netease::search(&query).map(|songs| {
                    songs
                        .into_iter()
                        .map(|song| {
                            let id = song.id();
                            SearchResult {
                                name: song.name,
                                artist: song.artists.unwrap_or_else(|| "未知艺术家".to_owned()),
                                album: String::new(),
                                source: SearchSource::Netease,
                                netease_id: Some(id),
                                qqmid: None,
                                qq_songid: None,
                                albummid: None,
                                tidal_id: None,
                                quality: None,
                            }
                        })
                        .collect()
                })
            }
            SearchSource::Qq => crate::qqmusic::search(&query).map(|songs| {
                songs
                    .into_iter()
                    .map(|song| SearchResult {
                        name: song.name,
                        artist: song.singer,
                        album: song.album,
                        source: SearchSource::Qq,
                        netease_id: None,
                        qqmid: Some(song.songmid),
                        qq_songid: Some(song.songid),
                        albummid: Some(song.albummid),
                        tidal_id: None,
                        quality: song.quality,
                    })
                    .collect()
            }),
            SearchSource::Tidal => {
                // 支持直接粘贴 Tidal 单曲链接（如 https://tidal.com/track/172282141/u）下载。
                // 以 http 开头识别为链接，解析成单曲；否则按关键词搜索。
                let is_url = query.trim_start().starts_with("http");
                let songs = if is_url {
                    crate::tidal::track_from_url(&query).map(|song| vec![song])
                } else {
                    crate::tidal::search(&query)
                };
                songs.map(|songs| {
                    songs
                        .into_iter()
                        .map(|song| SearchResult {
                            name: song.name.clone(),
                            artist: song.artists.clone().unwrap_or_else(|| "未知艺术家".to_owned()),
                            album: song.album.clone().unwrap_or_default(),
                            source: SearchSource::Tidal,
                            netease_id: None,
                            qqmid: None,
                            qq_songid: None,
                            albummid: None,
                            tidal_id: Some(song.id()),
                            quality: song.quality.clone(),
                        })
                        .collect()
                })
            },
        };
        match searched {
            Ok(results) => {
                self.search_results = results;
                self.search_selected = 0;
                self.search_input = Some(String::new());
                self.search_cursor = 0;
                self.set_status("方向键或 J/K 选择，Enter播放/下载，Esc取消");

                if self.search_source == SearchSource::Netease {
                    self.resolve_search_qualities();
                }
            }
            Err(e) => {
                self.set_status(format!("搜索失败：{e:#}"));
            }
        }
    }
    pub fn move_search(&mut self, d: i32) {
        let n = self.search_results.len() as i32;
        if n > 0 {
            self.search_selected = ((self.search_selected as i32 + d + n) % n) as usize;
        }
    }

    fn resolve_search_qualities(&mut self) {
        let (sender, receiver) = mpsc::channel();
        let songs = self.search_results.clone();
        for song in songs {
            if let Some(id) = song.netease_id {
                let sender = sender.clone();

                std::thread::spawn(move || {
                    if let Ok(quality) =
                        crate::netease::resolve_quality_for_id(&id)
                    {
                        let _ = sender.send((id, quality));
                    }
                });
            }
        }

        drop(sender);
        self.search_quality_updates = Some(receiver);
    }

    pub fn download_selected(&mut self) {
        let Some(entry) = self.search_results.get(self.search_selected).cloned() else {
            return;
        };

        let existing = match entry.source {
            SearchSource::Netease => entry.netease_id.as_deref().and_then(|id| {
                self.tracks
                    .iter()
                    .position(|track| track.netease_id.as_deref() == Some(id))
            }),
            SearchSource::Qq => entry.qqmid.as_deref().and_then(|mid| {
                self.tracks
                    .iter()
                    .position(|track| track.qqmid.as_deref() == Some(mid))
            }),
            SearchSource::Tidal => None,
        };
        if let Some(index) = existing {
            self.search_results.clear();
            self.search_input = None;
            self.search_quality_updates = None;
            self.needs_full_redraw = true;
            self.queue_selected = index;
            self.play_selected();
            self.set_status(format!("已在本地，直接播放：{}", entry.name));
            return;
        }
        self.search_results.clear();
        self.search_input = None;
        self.search_quality_updates = None;

        self.needs_full_redraw = true;
        let target = self.source_dir.clone();

        let progress = Arc::new(Mutex::new((0, 0)));
        let result = Arc::new(Mutex::new(None));
        let p = progress.clone();
        let r = result.clone();
        self.download_progress = Some(progress);
        self.download_result = Some(result);
        self.set_status("下载中 0%");

        let source = entry.source;
        std::thread::spawn(move || {
            let value = match source {
                SearchSource::Netease => {
                    let song = crate::netease::Song::new(
                        entry.netease_id.clone().unwrap_or_default(),
                        entry.name.clone(),
                        Some(entry.artist.clone()),
                    );
                    crate::netease::download(&song, &target, |d, t| {
                        *p.lock().unwrap() = (d, t)
                    })
                }
                SearchSource::Qq => {
                    let song = crate::qqmusic::Song {
                        name: entry.name.clone(),
                        singer: entry.artist.clone(),
                        album: entry.album.clone(),
                        songmid: entry.qqmid.clone().unwrap_or_default(),
                        songid: entry.qq_songid.clone().unwrap_or_default(),
                        albummid: entry.albummid.clone().unwrap_or_default(),
                        quality: entry.quality.clone(),
                    };
                    crate::qqmusic::download(&song, &target, |d, t| *p.lock().unwrap() = (d, t))
                }
                SearchSource::Tidal => {
                    let song = crate::tidal::Song::new(
                        entry.tidal_id.clone().unwrap_or_default(),
                        entry.name.clone(),
                        Some(entry.artist.clone()),
                    );
                    crate::tidal::download(&song, &target, |d, t| *p.lock().unwrap() = (d, t))
                }
            }
            .map_err(|e| format!("{e:#}"));

            *r.lock().unwrap() = Some(value);
        });
    }

    pub fn current_track(&self) -> &Track {
        &self.tracks[self.current]
    }

    pub fn cover_dimensions(&mut self) -> Option<(u32, u32)> {
        self.ensure_cover_source();

        self.cover_source
            .as_ref()
            .map(|source| (source.width(), source.height()))
    }

    pub fn set_help_visible(&mut self, visible: bool) {
        if self.help_visible == visible {
            return;
        }
        self.help_visible = visible;

        self.needs_full_redraw = true;
    }

    pub fn close_search(&mut self) {
        self.search_input = None;
        self.search_results.clear();
        self.search_quality_updates = None;
        self.clear_status();
    }

    pub fn prepare_cover(&mut self, width: u16, height: u16) {
        let render_size = (width, height);

        if self.cover_index == Some(self.current) && self.cover_render_size == Some(render_size) {
            return;
        }

        self.cover = None;
        self.cover_index = Some(self.current);
        self.cover_render_size = Some(render_size);

        let font_size = crate::ui::effective_font_size(&self.picker);
        let cw = u32::from(font_size.width).max(1);
        let ch = u32::from(font_size.height).max(1);
        if width == 0 || height == 0 || cw == 0 || ch == 0 {
            return;
        }
        self.ensure_cover_source();
        let Some(source) = self.cover_source.as_ref() else {
            return;
        };

        let target_width = u32::from(width) * cw;
        let target_height = u32::from(height) * ch;

        let delta = ch.saturating_sub(cw);
        let pad_x = delta;
        let pad_y = 0;

        let available_width = target_width.saturating_sub(pad_x * 2).max(1);
        let available_height = target_height.saturating_sub(pad_y * 2).max(1);

        let (fit_width, fit_height) = crate::ui::fitted_pixel_size(
            source.width(),
            source.height(),
            available_width,
            available_height,
        );
        let fit_width = fit_width.min(target_width);
        let fit_height = fit_height.min(target_height);
        let fitted =
            source.resize_exact(fit_width, fit_height, image::imageops::FilterType::Lanczos3);

        let offset_x = i64::from((target_width - fit_width) / 2);
        let offset_y = i64::from((target_height - fit_height) / 2);

        let mut canvas = image::DynamicImage::new_rgba8(target_width, target_height);

        image::imageops::overlay(&mut canvas, &fitted, offset_x, offset_y);
        self.cover = Some(self.picker.new_resize_protocol(canvas));
    }

    fn invalidate_cover(&mut self) {
        self.cover = None;
        self.cover_index = None;
        self.cover_render_size = None;
        self.cover_source = None;
        self.cover_source_index = None;
    }

    fn ensure_cover_source(&mut self) {
        if self.cover_source_index == Some(self.current) {
            return;
        }

        self.cover_source = self.tracks[self.current]
            .cover
            .as_deref()
            .and_then(|bytes| {
                let image = image::load_from_memory(bytes).ok()?;

                if image.width().max(image.height()) > 1024 {
                    Some(image.thumbnail(1024, 1024))
                } else {
                    Some(image)
                }
            });
        self.cover_source_index = Some(self.current);
    }

    pub fn move_queue(&mut self, delta: i32) {
        if self.tracks.is_empty() {
            return;
        }
        let last = self.tracks.len() - 1;
        self.queue_selected = if delta < 0 {
            self.queue_selected
                .saturating_sub(delta.unsigned_abs() as usize)
        } else {
            self.queue_selected.saturating_add(delta as usize).min(last)
        };
    }

    pub fn queue_marquee_step(&mut self, max_steps: usize) -> usize {
        if self.queue_marquee_selected != self.queue_selected
            || self.queue_marquee_steps != max_steps
        {
            self.queue_marquee_selected = self.queue_selected;
            self.queue_marquee_steps = max_steps;
            self.queue_marquee_started = Instant::now();
        }
        marquee_step(self.queue_marquee_started.elapsed(), max_steps)
    }

    pub fn play_selected(&mut self) {
        self.switch_to(self.queue_selected, false);
    }

    pub fn ask_delete(&mut self) {
        if self.download_result.is_some() {
            self.set_status("正在下载或写入标签，完成后再删除歌曲");
            return;
        }

        if self
            .tracks
            .get(self.queue_selected)
            .is_some_and(|t| t.path.exists())
        {
            self.delete_confirm = true;
            self.set_status(format!(
                "确认删除「{}」？按 y 确认，n/Esc 取消",
                self.tracks[self.queue_selected].title
            ));
        }
    }

    pub fn confirm_delete(&mut self, yes: bool) {
        self.delete_confirm = false;
        if !yes {
            self.set_status("已取消删除");
            return;
        }
        if self.download_result.is_some() {
            self.set_status("正在下载或写入标签，完成后再删除歌曲");
            return;
        }
        let Some(track) = self.tracks.get(self.queue_selected).cloned() else {
            return;
        };
        let current_path = self.tracks[self.current].path.clone();
        let deleting_current = track.path == current_path;
        if deleting_current {
            self.playback.stop();
        }

        if std::fs::remove_file(&track.path).is_err() {
            self.set_status("删除失败");
            return;
        }

        let target = track
            .path
            .parent()
            .unwrap_or_else(|| std::path::Path::new("."))
            .to_path_buf();
        if let Ok(tracks) = library::scan(&target) {
            self.tracks = tracks;

            self.current = self
                .tracks
                .iter()
                .position(|t| t.path == current_path)
                .unwrap_or(0);
            self.queue_selected = self.queue_selected.min(self.tracks.len().saturating_sub(1));
            self.queue_scroll = 0;
            self.invalidate_cover();
            if deleting_current && self.tracks[self.current].path.exists() {
                self.switch_to(self.current, false);
            } else if deleting_current {
                self.playback.stop();
                self.lyrics = Lyrics::default();
                self.finished = true;
            }
            self.set_status(format!("已删除：{}", track.title));
        }
    }

    pub fn toggle_pause(&mut self) {
        if !self.playback.is_available() {
            if let Some(reason) = self.playback.unavailable_reason() {
                self.set_status(format!("{reason}；当前无法播放声音"));
            }
            return;
        }
        self.finished = false;
        self.playback.toggle_pause();

        let _ = self.save();
        self.clear_status();
    }

    pub fn next_manual(&mut self) {
        let next = match self.mode {
            PlayMode::Shuffle => self.random_other_index(),
            _ => (self.current + 1) % self.tracks.len(),
        };
        self.switch_to(next, false);
    }

    pub fn previous(&mut self) {
        if self.playback.position() > Duration::from_secs(3) {
            if let Err(error) = self.playback.seek_relative(
                -(self.playback.position().as_secs() as i64),
                self.current_track().duration,
            ) {
                self.set_status(error.to_string());
            }
            return;
        }
        let previous = if self.current == 0 {
            self.tracks.len() - 1
        } else {
            self.current - 1
        };
        self.switch_to(previous, false);
    }

    pub fn seek(&mut self, seconds: i64) {
        if let Err(error) = self
            .playback
            .seek_relative(seconds, self.current_track().duration)
        {
            self.set_status(error.to_string());
        } else {
            let _ = self.save();
        }
    }

    pub fn cycle_mode(&mut self) {
        self.mode = self.mode.next();
        self.set_status(format!("播放模式：{}", self.mode.label()));
    }

    pub fn tick(&mut self) {
        if let Some(receiver) = &self.search_quality_updates {
            for (id, quality) in receiver.try_iter() {
                if let Some(song) = self
                    .search_results
                    .iter_mut()
                    .find(|song| song.netease_id.as_deref() == Some(&id))
                {
                    song.quality = Some(quality);
                }
            }
        }

        if let Some(p) = &self.download_progress {
            let (d, t) = *p.lock().unwrap();
            let status = if t > 0 && d < t {
                // 总量已知且未下载完 → 真实百分比
                format!("下载中 {:.0}%", d as f64 * 100.0 / t as f64)
            } else if t > 0 && d >= t {
                // 总量已知且已下载完 → 正在写标签
                "正在写入标签…".to_owned()
            } else if d > 0 {
                // 总量未知(HEAD 全失败)或总量低估溢出 → 显示已下载字节
                format!("下载中 {:.1} MB", d as f64 / 1_048_576.0)
            } else {
                "准备下载…".to_owned()
            };
            self.set_status(status);
        }

        let completed = self
            .download_result
            .as_ref()
            .and_then(|r| r.lock().ok()?.take());
        if let Some(value) = completed {
            self.download_progress = None;
            self.download_result = None;
            match value {
                Ok(path) => {
                    let target = path
                        .parent()
                        .unwrap_or(PathBuf::from("downloads").as_path())
                        .to_path_buf();

                    if let Ok(tracks) = library::scan(&target) {
                        self.tracks = tracks;
                        self.current = self.tracks.iter().position(|t| t.path == path).unwrap_or(0);
                        self.queue_selected = self.current;
                        self.queue_scroll = 0;
                        self.invalidate_cover();
                        self.switch_to(self.current, false);
                        self.needs_full_redraw = true;
                        self.set_status(format!("已下载并播放：{}", path.display()));
                    }
                }
                Err(e) => self.set_status(format!("下载失败：{e}")),
            }
        }

        if !self.playback.is_paused() && self.playback.is_empty() && !self.finished {
            match self.mode {
                PlayMode::RepeatOne => self.switch_to(self.current, false),
                PlayMode::Shuffle => {
                    let next = self.random_other_index();
                    self.switch_to(next, false);
                }
                PlayMode::Sequential if self.current + 1 < self.tracks.len() => {
                    self.switch_to(self.current + 1, false);
                }
                PlayMode::Sequential => {
                    self.finished = true;
                    self.set_status("播放队列已结束");
                }
            }
        }

        if self.last_save.elapsed() >= Duration::from_secs(5) {
            let _ = self.save();
            self.last_save = Instant::now();
        }
    }

    pub fn save(&self) -> Result<()> {
        let saved = SavedState {
            version: 1,
            source: self.source_id.clone(),
            track: track_identity(&self.current_track().path),
            position_ms: self.playback.position().as_millis() as u64,
            volume: self.playback.volume(),
            mode: self.mode,
        };
        state::save_atomic(&self.state_path, &saved)
    }

    pub fn scroll_lyrics(&mut self, delta: i32) {
        if delta < 0 {
            self.lyric_scroll = self
                .lyric_scroll
                .saturating_sub(delta.unsigned_abs() as usize);
        } else {
            self.lyric_scroll =
                (self.lyric_scroll + delta as usize).min(self.lyrics.lines.len().saturating_sub(1));
        }
    }

    fn switch_to(&mut self, index: usize, paused: bool) {
        let _ = self.save();
        if !self.tracks[index].path.exists() {
            return;
        }
        match self
            .playback
            .load(&self.tracks[index].path, Duration::ZERO, paused)
        {
            Ok(()) => {
                self.current = index;
                self.lyrics = self.tracks[index]
                    .lyrics
                    .as_deref()
                    .map(Lyrics::parse)
                    .unwrap_or_default();
                self.lyric_scroll = 0;
                if self.cover_index != Some(index) {
                    self.invalidate_cover();
                }
                self.finished = false;
                self.clear_status();
            }
            Err(error) => {
                self.set_status(format!(
                    "无法播放 {}：{error:#}",
                    self.tracks[index].path.display()
                ));
            }
        }
    }

    fn random_other_index(&self) -> usize {
        if self.tracks.len() <= 1 {
            return self.current;
        }
        let mut rng = rand::rng();
        let candidate = rng.random_range(0..self.tracks.len() - 1);
        if candidate >= self.current {
            candidate + 1
        } else {
            candidate
        }
    }
}

fn marquee_step(elapsed: Duration, max_steps: usize) -> usize {
    const PAUSE_MS: u128 = 900;
    const STEP_MS: u128 = 130;

    if max_steps == 0 {
        return 0;
    }
    let travel_ms = (max_steps as u128).saturating_mul(STEP_MS);
    let cycle_ms = PAUSE_MS
        .saturating_mul(2)
        .saturating_add(travel_ms.saturating_mul(2));

    let phase = elapsed.as_millis() % cycle_ms;

    if phase < PAUSE_MS {
        return 0;
    }
    let phase = phase - PAUSE_MS;
    if phase < travel_ms {
        return ((phase / STEP_MS) as usize + 1).min(max_steps);
    }
    let phase = phase - travel_ms;
    if phase < PAUSE_MS {
        return max_steps;
    }
    let phase = phase - PAUSE_MS;
    max_steps.saturating_sub(((phase / STEP_MS) as usize + 1).min(max_steps))
}

fn track_identity(path: &std::path::Path) -> String {
    path.canonicalize()
        .unwrap_or_else(|_| path.to_path_buf())
        .to_string_lossy()
        .to_string()
}

fn char_to_byte_index(value: &str, character_index: usize) -> usize {
    value
        .char_indices()
        .nth(character_index)
        .map_or(value.len(), |(byte, _)| byte)
}

#[cfg(test)]
mod tests {
    use super::char_to_byte_index;

    #[test]
    fn search_cursor_uses_character_positions() {
        assert_eq!(char_to_byte_index("鞠婧祎 花", 0), 0);
        assert_eq!(char_to_byte_index("鞠婧祎 花", 1), "鞠".len());
        assert_eq!(char_to_byte_index("鞠婧祎 花", 4), "鞠婧祎 ".len());
        assert_eq!(char_to_byte_index("鞠婧祎 花", 99), "鞠婧祎 花".len());
    }
}
