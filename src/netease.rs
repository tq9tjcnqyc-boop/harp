use std::fs::File;
use std::io::{Read, Write};
use std::path::{Path, PathBuf};

use anyhow::{Context, Result, bail};
use lofty::config::WriteOptions;
use lofty::file::{AudioFile, FileType, TaggedFileExt};
use lofty::picture::{MimeType, Picture, PictureType};
use lofty::probe::Probe;
use lofty::tag::{ItemKey, Tag, TagType};
use serde::Deserialize;

const SEARCH_LIMIT: usize = 10;
const DEFAULT_LEVEL: &str = "lossless";

#[derive(Debug, Clone, Deserialize)]
pub struct Song {
    id: serde_json::Value,
    pub name: String,
    pub artists: Option<String>,
    #[serde(skip)]
    pub quality: Option<String>,
}

impl Song {
    pub fn id(&self) -> String {
        self.id.to_string().trim_matches('"').to_owned()
    }

    pub fn new(id: String, name: String, artists: Option<String>) -> Self {
        Self {
            id: serde_json::Value::String(id),
            name,
            artists,
            quality: None,
        }
    }
}

#[derive(Debug, Clone, Deserialize)]
struct UrlData {
    url: Option<String>,
    #[serde(rename = "type")]
    format: Option<String>,
    level: Option<String>,
    quality_name: Option<String>,
    bitrate: Option<u64>,
    size: Option<u64>,
}

#[derive(Deserialize)]
struct SongMetadata {
    name: Option<String>,
    ar_name: Option<String>,
    al_name: Option<String>,
    lyric: Option<String>,
    pic: Option<String>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum AudioFormat {
    Flac,
    Mp3,
}

impl AudioFormat {
    fn extension(self) -> &'static str {
        match self {
            Self::Flac => "flac",
            Self::Mp3 => "mp3",
        }
    }

    fn tag_type(self) -> TagType {
        match self {
            Self::Flac => TagType::VorbisComments,
            Self::Mp3 => TagType::Id3v2,
        }
    }
}

pub fn search(query: &str) -> Result<Vec<Song>> {
    crate::netease_net::search(query, SEARCH_LIMIT)
}

pub fn resolve_quality(song: &Song) -> Result<String> {
    resolve_quality_for_id(&song.id())
}

pub fn resolve_quality_for_id(id: &str) -> Result<String> {
    let data = resolve_url(id, DEFAULT_LEVEL)?;
    Ok(quality_label(&data))
}

pub fn download(
    song: &Song,
    target: &Path,
    mut progress: impl FnMut(u64, u64),
) -> Result<PathBuf> {
    let client = http_client()?;
    let id = song.id();
    let audio = resolve_audio(&id)?;

    let url = audio.url.as_deref().context("网易云未返回下载地址")?;
    let advertised_format = advertised_format(&audio)?;

    let mut response = client
        .get(url)
        .header("User-Agent", crate::netease_net::UA)
        .header("Referer", crate::netease_net::REFERER)
        .header(
            "Cookie",
            crate::netease_net::login_cookie()
                .unwrap_or_else(|| "os=pc; appver=; osver=; deviceId=pyncm!".to_owned()),
        )
        .send()
        .context("无法连接音频下载地址")?
        .error_for_status()
        .context("音频下载地址返回错误")?;

    let total = response
        .content_length()
        .or(audio.size)
        .context("服务器未返回文件大小")?;

    std::fs::create_dir_all(target).context("无法创建下载目录")?;

    let stem = sanitize_filename(&song.name);

    let temporary = target.join(format!(
        ".harp-{id}.part.{}",
        advertised_format.extension()
    ));

    let result = (|| -> Result<PathBuf> {
        let mut file = File::create(&temporary).context("无法创建下载文件")?;
        let mut downloaded = 0_u64;

        let mut buffer = [0_u8; 64 * 1024];
        progress(0, total);
        loop {
            let count = response.read(&mut buffer).context("下载音频时连接中断")?;
            if count == 0 {
                break;
            }

            file.write_all(&buffer[..count])
                .context("写入下载文件失败")?;
            downloaded += count as u64;

            progress(downloaded.min(total), total);
        }
        file.flush().context("刷新下载文件失败")?;

        drop(file);
        progress(total, total);

        let actual_format = detect_audio_format(&temporary)?;
        let quality = quality_label_for_format(&audio, actual_format);
        let tagged_temporary =
            target.join(format!(".harp-{id}.part.{}", actual_format.extension()));

        if tagged_temporary != temporary {
            std::fs::rename(&temporary, &tagged_temporary).context("无法修正下载文件的真实格式")?;
        }

        let metadata = request_metadata(&id)?;
        let cover = metadata.pic.as_deref().and_then(crate::netease_net::cover_bytes);
        write_metadata(
            &tagged_temporary,
            actual_format,
            &id,
            song,
            &metadata,
            cover,
        )?;

        let path = target.join(format!("{stem} [{quality}].{}", actual_format.extension()));
        std::fs::rename(&tagged_temporary, &path).context("无法完成下载文件")?;
        Ok(path)
    })();

    if result.is_err() {
        let _ = std::fs::remove_file(&temporary);
        let _ = std::fs::remove_file(target.join(format!(".harp-{id}.part.flac")));
        let _ = std::fs::remove_file(target.join(format!(".harp-{id}.part.mp3")));
    }
    result
}

fn http_client() -> Result<reqwest::blocking::Client> {
    reqwest::blocking::Client::builder()
        .connect_timeout(std::time::Duration::from_secs(5))
        .timeout(std::time::Duration::from_secs(30))
        .build()
        .context("无法创建网络客户端")
}

fn resolve_url(id: &str, level: &str) -> Result<UrlData> {
    let r = crate::netease_net::resolve_url(id, level)
        .with_context(|| format!("网易云解析失败(id={id})"))?;
    Ok(UrlData {
        url: r.url,
        format: r.format,
        level: r.level,
        quality_name: None,
        bitrate: r.bitrate,
        size: r.size,
    })
}

fn resolve_audio(id: &str) -> Result<UrlData> {
    if let Ok(data) = resolve_url(id, DEFAULT_LEVEL)
        && data.url.is_some()
    {
        return Ok(data);
    }
    let data = resolve_url(id, "exhigh")?;
    if data.url.is_none() {
        bail!("网易云没有返回可下载的音频地址");
    }
    Ok(data)
}

fn request_metadata(id: &str) -> Result<SongMetadata> {
    let m = crate::netease_net::song_info(id)?;
    Ok(SongMetadata {
        name: m.name,
        ar_name: m.ar_name,
        al_name: m.al_name,
        lyric: m.lyric,
        pic: m.pic,
    })
}

fn write_metadata(
    path: &Path,
    format: AudioFormat,
    id: &str,
    song: &Song,
    metadata: &SongMetadata,
    cover: Option<Vec<u8>>,
) -> Result<()> {
    let is_mp3 = format == AudioFormat::Mp3;
    let tag_type = format.tag_type();

    let mut file = lofty::read_from_path(path).context("无法读取已下载音频")?;

    if file.tag(tag_type).is_none() {
        file.insert_tag(Tag::new(tag_type));
    }
    let tag = file.tag_mut(tag_type).context("无法创建音频标签")?;

    tag.insert_text(
        ItemKey::TrackTitle,
        metadata
            .name
            .clone()
            .filter(|value| !value.is_empty())
            .unwrap_or_else(|| song.name.clone()),
    );

    if let Some(artist) = metadata
        .ar_name
        .as_deref()
        .filter(|value| !value.is_empty())
        .or(song.artists.as_deref())
    {
        tag.insert_text(ItemKey::TrackArtist, artist.to_owned());
    }
    if let Some(album) = metadata
        .al_name
        .as_deref()
        .filter(|value| !value.is_empty())
    {
        tag.insert_text(ItemKey::AlbumTitle, album.to_owned());
    }

    tag.insert_text(ItemKey::Comment, format!("NETEASE_ID={id}"));
    if let Some(lyrics) = metadata.lyric.as_deref().filter(|value| !value.is_empty()) {
        tag.insert_text(
            if is_mp3 {
                ItemKey::UnsyncLyrics
            } else {
                ItemKey::Lyrics
            },
            lyrics.to_owned(),
        );
    }
    if let Some(bytes) = cover {
        let mime = cover_mime(&bytes);

        let picture = Picture::unchecked(bytes)
            .pic_type(PictureType::CoverFront)
            .mime_type(mime)
            .build();
        tag.set_picture(0, picture);
    }

    file.save_to_path(path, WriteOptions::default())
        .context("写入音频元数据失败")?;
    Ok(())
}

fn cover_mime(bytes: &[u8]) -> MimeType {
    match image::guess_format(bytes).ok() {
        Some(image::ImageFormat::Png) => MimeType::Png,
        Some(image::ImageFormat::Gif) => MimeType::Gif,
        Some(image::ImageFormat::Bmp) => MimeType::Bmp,
        Some(image::ImageFormat::Tiff) => MimeType::Tiff,
        Some(image::ImageFormat::WebP) => MimeType::Unknown("image/webp".to_owned()),
        _ => MimeType::Jpeg,
    }
}

fn advertised_format(data: &UrlData) -> Result<AudioFormat> {
    if data.level.as_deref().is_some_and(|level| {
        level.eq_ignore_ascii_case("lossless") || level.eq_ignore_ascii_case("hires")
    }) || data.bitrate.is_some_and(|bitrate| bitrate > 500_000)
    {
        return Ok(AudioFormat::Flac);
    }
    match data
        .format
        .as_deref()
        .map(str::to_ascii_lowercase)
        .as_deref()
    {
        Some("flac") => Ok(AudioFormat::Flac),
        Some("mp3" | "mpeg") => Ok(AudioFormat::Mp3),

        Some(format) => bail!("暂不支持网易云返回的 {format} 格式"),
        None => bail!("网易云没有返回音频格式"),
    }
}

fn quality_label(data: &UrlData) -> String {
    advertised_format(data)
        .map(|format| quality_label_for_format(data, format))
        .unwrap_or_else(|_| "未知音质".to_owned())
}

fn quality_label_for_format(data: &UrlData, format: AudioFormat) -> String {
    if format == AudioFormat::Mp3 {
        return "mp3".to_owned();
    }

    if data
        .level
        .as_deref()
        .is_some_and(|level| level.eq_ignore_ascii_case("hires"))
        || data.quality_name.as_deref().is_some_and(|name| {
            let name = name.to_ascii_lowercase();
            name.contains("hi-res") || name.contains("hires")
        })
        || data.bitrate.is_some_and(|bitrate| bitrate > 1_411_000)
    {
        "hires".to_owned()
    } else {
        "lossless".to_owned()
    }
}

fn detect_audio_format(path: &Path) -> Result<AudioFormat> {
    let probe = Probe::open(path)
        .context("无法打开下载文件")?
        .guess_file_type()
        .context("无法识别下载文件的真实格式")?;
    match probe.file_type() {
        Some(FileType::Flac) => Ok(AudioFormat::Flac),
        Some(FileType::Mpeg) => Ok(AudioFormat::Mp3),
        Some(format) => bail!("下载内容实际是暂不支持的 {format:?} 格式"),
        None => bail!("无法识别下载内容的真实格式"),
    }
}

pub(crate) fn sanitize_filename(name: &str) -> String {
    let value = name
        .chars()
        .filter(|character| !r#"<>:/\\|?*\""#.contains(*character))
        .collect::<String>();

    if value.trim().is_empty() {
        "未知歌曲".to_owned()
    } else {
        value
    }
}

#[cfg(test)]
mod tests {
    use std::process::Command;

    use super::*;

    fn url_data(format: &str, level: &str, bitrate: u64) -> UrlData {
        UrlData {
            url: Some("https://example.invalid/audio".to_owned()),
            format: Some(format.to_owned()),
            level: Some(level.to_owned()),
            quality_name: None,
            bitrate: Some(bitrate),
            size: Some(1),
        }
    }

    #[test]
    fn quality_uses_actual_format_and_level() {
        assert_eq!(quality_label(&url_data("MP3", "exhigh", 320_000)), "mp3");
        assert_eq!(
            quality_label(&url_data("FLAC", "lossless", 1_000_000)),
            "lossless"
        );
        assert_eq!(
            quality_label(&url_data("FLAC", "hires", 2_000_000)),
            "hires"
        );
    }

    #[test]
    fn extensions_are_normalized() {
        assert_eq!(
            advertised_format(&url_data("FLAC", "lossless", 1_000_000))
                .unwrap()
                .extension(),
            "flac",
        );
        assert_eq!(
            advertised_format(&url_data("MP3", "exhigh", 320_000))
                .unwrap()
                .extension(),
            "mp3",
        );

        assert_eq!(
            advertised_format(&url_data("MP3", "lossless", 1_671_639)).unwrap(),
            AudioFormat::Flac,
        );
    }

    #[test]
    fn mp3_metadata_round_trip() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("test.mp3");

        let generated = Command::new("ffmpeg")
            .args([
                "-v",
                "error",
                "-f",
                "lavfi",
                "-i",
                "anullsrc=r=44100:cl=mono",
                "-t",
                "0.1",
                path.to_str().unwrap(),
            ])
            .status();

        if !generated.is_ok_and(|status| status.success()) {
            return;
        }

        let song = Song {
            id: serde_json::json!(123456),
            name: "测试歌曲".to_owned(),
            artists: Some("测试歌手".to_owned()),
            quality: Some("mp3".to_owned()),
        };
        let metadata = SongMetadata {
            name: Some("测试歌曲".to_owned()),
            ar_name: Some("测试歌手".to_owned()),
            al_name: Some("测试专辑".to_owned()),
            lyric: Some("[00:00.00]测试歌词".to_owned()),
            pic: None,
        };

        let mut png = std::io::Cursor::new(Vec::new());
        image::DynamicImage::new_rgb8(1, 1)
            .write_to(&mut png, image::ImageFormat::Png)
            .unwrap();
        write_metadata(
            &path,
            AudioFormat::Mp3,
            "123456",
            &song,
            &metadata,
            Some(png.into_inner()),
        )
        .unwrap();

        let tracks = crate::library::scan(directory.path()).unwrap();
        assert_eq!(tracks[0].title, "测试歌曲");
        assert_eq!(tracks[0].artist, "测试歌手");
        assert_eq!(tracks[0].album, "测试专辑");
        assert_eq!(tracks[0].netease_id.as_deref(), Some("123456"));
        assert_eq!(tracks[0].lyrics.as_deref(), Some("[00:00.00]测试歌词"));
        assert!(tracks[0].cover.is_some());

        assert!(rodio::Decoder::try_from(File::open(&path).unwrap()).is_ok());
    }

    #[test]
    fn flac_metadata_round_trip() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("test.flac");
        let generated = Command::new("ffmpeg")
            .args([
                "-v",
                "error",
                "-f",
                "lavfi",
                "-i",
                "anullsrc=r=44100:cl=mono",
                "-t",
                "0.1",
                path.to_str().unwrap(),
            ])
            .status();
        if !generated.is_ok_and(|status| status.success()) {
            return;
        }

        let misleading_path = directory.path().join("actually-flac.mp3");
        std::fs::rename(&path, &misleading_path).unwrap();
        assert_eq!(
            detect_audio_format(&misleading_path).unwrap(),
            AudioFormat::Flac
        );
        std::fs::rename(&misleading_path, &path).unwrap();

        let song = Song {
            id: serde_json::json!(654321),
            name: "无损测试".to_owned(),
            artists: Some("测试歌手".to_owned()),
            quality: Some("lossless".to_owned()),
        };
        let metadata = SongMetadata {
            name: Some("无损测试".to_owned()),
            ar_name: Some("测试歌手".to_owned()),
            al_name: Some("测试专辑".to_owned()),
            lyric: Some("[00:00.00]无损歌词".to_owned()),
            pic: None,
        };
        write_metadata(&path, AudioFormat::Flac, "654321", &song, &metadata, None).unwrap();

        let tracks = crate::library::scan(directory.path()).unwrap();
        assert_eq!(tracks[0].title, "无损测试");
        assert_eq!(tracks[0].netease_id.as_deref(), Some("654321"));
        assert_eq!(tracks[0].lyrics.as_deref(), Some("[00:00.00]无损歌词"));
    }
}
