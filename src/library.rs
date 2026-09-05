use std::ffi::OsStr;
use std::fs::File;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result, bail};
use lofty::config::ParseOptions;
use lofty::file::{AudioFile, TaggedFileExt};
use lofty::flac::FlacFile;
use lofty::prelude::{Accessor, ItemKey};
use lofty::probe::Probe;
use pinyin::ToPinyin;
use walkdir::WalkDir;

use crate::model::Track;

pub fn scan(source: &Path) -> Result<Vec<Track>> {
    if !source.exists() {
        bail!("路径不存在：{}", source.display());
    }

    let mut tracks = Vec::new();

    let mut ingest = |path: PathBuf| match read_track(&path) {
        Ok(track) => tracks.push(track),
        Err(error) => eprintln!("跳过 {}：{error:#}", path.display()),
    };

    if source.is_file() {
        if !is_audio(source) {
            bail!("仅支持 FLAC 和 MP3 文件");
        }

        ingest(source.to_path_buf());
    } else {
        for entry in WalkDir::new(source)
            .follow_links(false)
            .into_iter()
            .filter_map(Result::ok)
            .filter(|entry| entry.file_type().is_file() && is_audio(entry.path()))
        {
            ingest(entry.into_path());
        }
    }

    tracks.sort_by_key(|track| pinyin_key(&track.title));

    if tracks.is_empty() && source.is_dir() {
        tracks.push(Track {
            path: source.join(".harp-empty"),
            title: "暂无歌曲".into(),
            artist: String::new(),
            album: String::new(),
            album_key: String::new(),
            duration: std::time::Duration::ZERO,
            disc: 0,
            number: 0,
            lyrics: None,
            cover: None,
            netease_id: None,
            qqmid: None,
        });
    }
    if tracks.is_empty() {
        bail!("没有找到可播放的 FLAC 或 MP3 文件");
    }
    Ok(tracks)
}

fn pinyin_key(title: &str) -> String {
    title
        .chars()
        .map(|ch| {
            ch.to_pinyin()
                .map(|p| p.plain().chars().next().unwrap_or(ch).to_ascii_lowercase())
                .unwrap_or_else(|| ch.to_ascii_lowercase())
        })
        .collect()
}

fn is_audio(path: &Path) -> bool {
    if path
        .file_name()
        .and_then(OsStr::to_str)
        .is_some_and(|name| name.starts_with(".harp-") && name.contains(".part."))
    {
        return false;
    }
    path.extension()
        .and_then(OsStr::to_str)
        .is_some_and(|ext| ext.eq_ignore_ascii_case("flac") || ext.eq_ignore_ascii_case("mp3"))
}

fn read_track(path: &Path) -> Result<Track> {
    let tagged = Probe::open(path)
        .with_context(|| format!("无法打开 {}", path.display()))?
        .read()
        .context("无法读取音频标签")?;
    let properties = tagged.properties();

    let tag = tagged.primary_tag().or_else(|| tagged.first_tag());

    let fallback_title = path
        .file_stem()
        .and_then(OsStr::to_str)
        .unwrap_or("未知曲目")
        .to_owned();

    let title = tag
        .and_then(|tag| tag.title())
        .map(|value| value.into_owned())
        .unwrap_or(fallback_title);
    let artist = tag
        .and_then(|tag| tag.artist())
        .map(|value| value.into_owned())
        .unwrap_or_default();
    let album = tag
        .and_then(|tag| tag.album())
        .map(|value| value.into_owned())
        .unwrap_or_default();

    let album_key = album.to_lowercase();
    let number = tag.and_then(|tag| tag.track()).unwrap_or(0);
    let disc = tag.and_then(|tag| tag.disk()).unwrap_or(0);

    let lyrics = find_raw_synced_lyrics(path).or_else(|| tag.and_then(find_lyrics));
    let netease_id = tag.and_then(|tag| {
        tag.get_strings(ItemKey::Comment)
            .find_map(|value| value.strip_prefix("NETEASE_ID=").map(str::to_owned))
    });

    let qqmid = tag.and_then(|tag| {
        tag.get_strings(ItemKey::Comment)
            .find_map(|value| value.strip_prefix("QQMID=").map(str::to_owned))
    });

    let cover = tag
        .and_then(|tag| {
            tag.pictures()
                .iter()
                .find(|picture| picture.pic_type() == lofty::picture::PictureType::CoverFront)
                .or_else(|| tag.pictures().first())
        })
        .map(|picture| picture.data().to_vec());

    Ok(Track {
        path: path.to_path_buf(),
        title,
        artist,
        album,
        album_key,
        duration: properties.duration(),
        disc,
        number,
        lyrics,
        cover,
        netease_id,
        qqmid,
    })
}

fn find_lyrics(tag: &lofty::tag::Tag) -> Option<String> {
    [ItemKey::Lyrics, ItemKey::UnsyncLyrics]
        .iter()
        .find_map(|key| tag.get_string(*key).map(str::to_owned))
}

fn find_raw_synced_lyrics(path: &Path) -> Option<String> {
    if !path
        .extension()
        .and_then(OsStr::to_str)
        .is_some_and(|extension| extension.eq_ignore_ascii_case("flac"))
    {
        return None;
    }
    let mut file = File::open(path).ok()?;
    let flac = FlacFile::read_from(&mut file, ParseOptions::new()).ok()?;
    flac.vorbis_comments()
        .and_then(|comments| comments.get("SYNCEDLYRICS"))
        .map(str::to_owned)
}

pub fn source_identity(source: &Path) -> String {
    source
        .canonicalize()
        .unwrap_or_else(|_| PathBuf::from(source))
        .to_string_lossy()
        .to_string()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn extension_is_case_insensitive() {
        assert!(is_audio(Path::new("music.FLAC")));
        assert!(is_audio(Path::new("music.mp3")));
        assert!(!is_audio(Path::new(".harp-123.part.flac")));
        assert!(!is_audio(Path::new(".harp-123.part.mp3")));
    }
}
