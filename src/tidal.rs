use std::fs::File;
use std::io::{Read, Write};
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::atomic::{AtomicU64, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use anyhow::{Context, Result, bail};
use base64::Engine;
use base64::engine::general_purpose::STANDARD;
use lofty::config::WriteOptions;
use lofty::file::{AudioFile, FileType, TaggedFileExt};
use lofty::picture::{MimeType, Picture, PictureType};
use lofty::probe::Probe;
use lofty::tag::{ItemKey, Tag, TagType};
use serde::{Deserialize, Serialize};
use serde_json::Value;

const SEARCH_LIMIT: usize = 10;
const API_HOST: &str = "https://api.tidal.com/v1";
const AUTH_HOST: &str = "https://auth.tidal.com/v1/oauth2";
const LOGIN_HOST: &str = "https://login.tidal.com";

// 普通(旧) client：tiddl 用的公开客户端，只能放流到 LOSSLESS 44.1/16。
// 保留作 device flow 兜底。
const CLIENT_ID: &str = "4N3n6Q1x95LL5K7p";
const CLIENT_SECRET: &str = "oKOXfJW371cX6xaZ0PyhgGNBdNLlBZd4AKKYougMjik=";

// PKCE 播放器 client（Android 官方播放器，来自 python-tidal 的 client_id_pkce）。
// 能用 PKCE 授权码流放 HiRes(24bit) 流——这是拿 Max 音质的唯一路径。
// 坑(2026-09-03 实测)：换 token 时不能带 client_secret(PKCE 是公共客户端,带 secret 报
// invalid_client "issued to another client")，且必须带发起授权时的 client_unique_key。
const PKCE_CLIENT_ID: &str = "6BDSRdpK9hqEBTgU";
const PKCE_REDIRECT_URI: &str = "https://tidal.com/android/login/auth";

const DEVICE_SCOPE: &str = "r_usr+w_usr+w_sub";
const GRANT_DEVICE_CODE: &str = "urn:ietf:params:oauth:grant-type:device_code";
const DEFAULT_COUNTRY: &str = "US";
// HiRes 用 HI_RES_LOSSLESS；旧 client(44.1/16) 会自动降档,不影响。
const DEFAULT_QUALITY: &str = "HI_RES_LOSSLESS";
const TOKEN_REFRESH_SKEW_SECS: i64 = 300;

#[derive(Debug, Clone, Deserialize)]
pub struct Song {
    id: serde_json::Value,
    pub name: String,
    pub artists: Option<String>,
    #[serde(skip)]
    pub album: Option<String>,
    #[serde(skip)]
    pub cover: Option<String>,
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
            album: None,
            cover: None,
            quality: None,
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct Token {
    #[serde(default)]
    access_token: String,
    #[serde(default)]
    refresh_token: String,
    #[serde(default)]
    user_id: i64,
    #[serde(default)]
    country_code: String,
    #[serde(default)]
    expires_at: i64,
    // true = PKCE 播放器 client(能放 HiRes/24bit), false = 旧 device-flow client(仅44.1/16)
    #[serde(default)]
    is_pkce: bool,
}

// ---------- 公开接口 ----------

/// Tidal 登录（首选 PKCE 授权码流，能拿 HiRes/24bit 授权）。
///
/// PKCE 流程(2026-09-03 实测, 来自 python-tidal client_id_pkce)：
///   1. 生成 code_verifier / code_challenge(S256)
///   2. 打印授权 URL(login.tidal.com/authorize)，提示用户在浏览器打开并用账号登录
///   3. 授权后跳转到 PKCE_REDIRECT_URI 的"Oops"页，用户把整段 URL 粘贴回来
///   4. 从 URL 提取 code，用 code+verifier+client_unique_key 换 access_token(不带 secret)
///   5. 存 ~/.harp/tidal.json
pub fn login() -> Result<()> {
    let client = http_client()?;

    // 1) 生成 PKCE verifier/challenge
    let code_verifier = generate_code_verifier();
    let code_challenge = pkce_challenge(&code_verifier);
    let client_unique_key = gen_unique_key();

    // 2) 构造授权 URL
    let auth_url = format!(
        "{LOGIN_HOST}/authorize?response_type=code&redirect_uri={}&client_id={}&lang=EN&appMode=android&client_unique_key={}&code_challenge={}&code_challenge_method=S256&restrict_signup=true",
        urlencode(PKCE_REDIRECT_URI),
        PKCE_CLIENT_ID,
        client_unique_key,
        code_challenge,
    );

    println!("=== Tidal 登录(PKCE, 可下 HiRes/24bit) ===");
    println!("请用任意浏览器打开下面地址, 用你的 Tidal 账号登录授权:");
    println!();
    println!("  {auth_url}");
    println!();
    println!("登录后会跳到一个地址(可能显示 Oops/出错), 把地址栏整段 URL 复制后粘贴到下面, 回车:");
    println!();

    // 3) 读用户粘贴的 redirect URL
    let mut line = String::new();
    std::io::stdin()
        .read_line(&mut line)
        .context("读取输入失败")?;
    let redirect_url = line.trim().to_owned();
    if redirect_url.is_empty() {
        bail!("未输入回调 URL，登录取消");
    }

    // 4) 提取 code 并换 token
    let code = extract_code_from_url(&redirect_url)
        .context(format!("从回调 URL 解析 code 失败：{redirect_url}"))?;
    let token = exchange_code_for_token(&client, &code, &code_verifier, &client_unique_key)?;
    save_token(&token)?;
    println!("Tidal 登录成功！凭据已写入 ~/.harp/tidal.json");
    Ok(())
}

/// 生成 PKCE code_verifier（32 字节随机, base64url 无填充）。
fn generate_code_verifier() -> String {
    use rand::RngCore;
    let mut bytes = [0u8; 32];
    rand::rng().fill_bytes(&mut bytes);
    base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(bytes)
}

/// PKCE S256 challenge = base64url(sha256(verifier)) 无填充。
fn pkce_challenge(verifier: &str) -> String {
    use sha2::{Digest, Sha256};
    let digest = Sha256::digest(verifier.as_bytes());
    base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(digest)
}

fn gen_unique_key() -> String {
    let key = generate_code_verifier();
    key.chars().take(24).collect()
}

/// 简单 URL 编码（PKCE_REDIRECT_URI 等静态值足够；遇到需 encode 的再扩展）。
fn urlencode(s: &str) -> String {
    s.replace(':', "%3A")
        .replace('/', "%2F")
        .replace('?', "%3F")
        .replace('&', "%26")
        .replace('=', "%3D")
}

/// 从回调 URL 提取授权 code（`code=` 参数值）。
fn extract_code_from_url(url: &str) -> Option<String> {
    let after = url.split("code=").nth(1)?;
    Some(after.split('&').next()?.to_owned())
}

/// 用 PKCE 授权码换 access_token（不能带 client_secret）。
fn exchange_code_for_token(
    client: &reqwest::blocking::Client,
    code: &str,
    code_verifier: &str,
    client_unique_key: &str,
) -> Result<Token> {
    let resp = client
        .post(format!("{AUTH_HOST}/token"))
        .form(&[
            ("code", code),
            ("client_id", PKCE_CLIENT_ID),
            ("grant_type", "authorization_code"),
            ("redirect_uri", PKCE_REDIRECT_URI),
            ("scope", DEVICE_SCOPE),
            ("code_verifier", code_verifier),
            ("client_unique_key", client_unique_key),
        ])
        .send()
        .context("连接 Tidal token 接口失败")?
        .error_for_status()
        .with_context(|| format!("Tidal 换 token 失败（code 可能已过期，需重新授权）：{}", code))?;

    let body: Value = resp.json().context("无法解析 Tidal token 响应")?;
    let country = body["user"]["countryCode"]
        .as_str()
        .unwrap_or(DEFAULT_COUNTRY)
        .to_owned();
    Ok(Token {
        access_token: body["access_token"].as_str().unwrap_or_default().to_owned(),
        refresh_token: body["refresh_token"].as_str().unwrap_or_default().to_owned(),
        user_id: body["user_id"].as_i64().unwrap_or(0),
        country_code: country,
        expires_at: now_secs() + body["expires_in"].as_i64().unwrap_or(3600),
        is_pkce: true,
    })
}

/// Tidal 设备流登录（device flow）——改用可授权 HiRes/24bit 的 PKCE 客户端(6BDSRdpK9hqEBTgU)。
/// 一般不用：默认 `harp --login-tidal` 走的是标准 PKCE 授权码流(login(),更稳)；
/// 这里仅作无浏览器跳转场景的兜底。
pub fn login_device() -> Result<()> {
    let client = http_client()?;
    let auth: Value = client
        .post(format!("{AUTH_HOST}/device_authorization"))
        .form(&[("client_id", PKCE_CLIENT_ID), ("scope", DEVICE_SCOPE)])
        .send()
        .context("无法连接 Tidal 设备授权接口")?
        .error_for_status()
        .context("Tidal 设备授权接口返回错误")?
        .json()
        .context("无法解析设备授权响应")?;

    let device_code = auth["deviceCode"]
        .as_str()
        .context("设备授权响应缺少 deviceCode")?
        .to_owned();
    let user_code = auth["userCode"].as_str().unwrap_or_default().to_owned();
    let verification_uri = auth["verificationUriComplete"]
        .as_str()
        .or_else(|| auth["verificationUri"].as_str())
        .context("设备授权响应缺少验证地址")?
        .to_owned();
    let expires_in = auth["expiresIn"].as_i64().unwrap_or(900);
    let interval = auth["interval"].as_u64().unwrap_or(5).max(2);

    println!("请在浏览器打开：{verification_uri}");
    println!("输入设备码：{user_code}（该地址已自动尝试打开）");
    crate::qqmusic::open_file(&PathBuf::from(&verification_uri));

    let deadline = now_secs() + expires_in;
    while now_secs() < deadline {
        std::thread::sleep(Duration::from_secs(interval));
        if let Some(token) = try_token(&client, &device_code)? {
            save_token(&token)?;
            println!("Tidal 登录成功！凭据已写入 ~/.harp/tidal.json");
            return Ok(());
        }
        print!(".");
        use std::io::Write as _;
        let _ = std::io::stdout().flush();
    }
    bail!("等待 Tidal 授权超时，请重试");
}

/// 从 Tidal 分享/网页链接解析出单曲并构建 Song。
///
/// 支持形如 `https://tidal.com/track/172282141/u`、`https://tidal.com/track/172282141`、
/// `https://listen.tidal.com/track/172282141` 等。提取 `/track/{id}` 里的数字 id，
/// 再调 `/tracks/{id}` 拿详情构建出可下载的 Song。
pub fn track_from_url(url: &str) -> Result<Song> {
    let trimmed = url.trim();
    if !trimmed.starts_with("http") {
        bail!("不是有效的链接：{trimmed}（需形如 https://tidal.com/track/172282141/u）");
    }
    let id = extract_track_id(trimmed)
        .with_context(|| format!("无法从链接中解析 track id：{trimmed}"))?;

    let (token, country) = effective_token()?;
    let client = http_client()?;
    let v: Value = client
        .get(format!("{API_HOST}/tracks/{id}"))
        .query(&[("countryCode", country)])
        .bearer_auth(&token)
        .send()
        .with_context(|| format!("无法连接 Tidal 单曲接口(id={id})"))?
        .error_for_status()
        .with_context(|| format!("Tidal 单曲接口返回错误(id={id})，凭据或曲目是否有效？"))?
        .json()
        .context("无法解析 Tidal 单曲响应")?;

    let name = v["title"].as_str().unwrap_or_default().to_owned();
    if name.is_empty() {
        bail!("Tidal 返回的曲目标题为空");
    }
    let artists = v["artists"]
        .as_array()
        .map(|arr| {
            arr.iter()
                .filter_map(|a| a["name"].as_str().map(str::to_owned))
                .collect::<Vec<_>>()
                .join("/")
        })
        .filter(|s| !s.is_empty());

    let mut song = Song::new(id, name, artists);
    song.album = v["album"]["title"].as_str().map(str::to_owned);
    song.cover = v["album"]["cover"].as_str().map(str::to_owned);
    song.quality = Some(quality_from_metadata(&v["mediaMetadata"]));
    Ok(song)
}

/// 从 URL 里提取 `/track/{id}` 的数字 id（轻量字符串扫描，避免引入 regex）。
fn extract_track_id(url: &str) -> Option<String> {
    let marker = "/track/";
    let idx = url.find(marker)?;
    let after = &url[idx + marker.len()..];
    let digits: String = after
        .chars()
        .take_while(|c| c.is_ascii_digit())
        .collect();
    if digits.is_empty() {
        None
    } else {
        Some(digits)
    }
}

pub fn search(query: &str) -> Result<Vec<Song>> {
    // 输入若是 Tidal 链接(track/album/playlist URL)，解析出 ID 按 ID 取，而非把整串 URL 当文本关键词搜
    if let Some((kind, id)) = parse_tidal_link(query) {
        return search_by_id(&kind, &id);
    }
    let (token, country) = effective_token()?;
    let client = http_client()?;
    let resp: Value = client
        .get(format!("{API_HOST}/search"))
        .query(&[
            ("query", query),
            ("countryCode", country.as_str()),
            ("limit", &SEARCH_LIMIT.to_string()),
        ])
        .bearer_auth(&token)
        .send()
        .context("无法连接 Tidal 搜索接口")?
        .error_for_status()
        .context("Tidal 搜索接口返回错误，登录或凭据是否有效？")?
        .json()
        .context("无法解析 Tidal 搜索结果")?;

    let tracks = resp["tracks"]["items"].as_array().cloned().unwrap_or_default();
    let out = tracks
        .into_iter()
        .take(SEARCH_LIMIT)
        .map(|s: Value| track_to_song(&s))
        .collect();
    Ok(out)
}

// 把一条 Tidal 曲目 JSON 转成 Song（搜索 / 按 ID 取统一复用）
fn track_to_song(s: &Value) -> Song {
    let id = s["id"].to_string().trim_matches('"').to_owned();
    let name = s["title"].as_str().unwrap_or_default().to_owned();
    let artists = s["artists"]
        .as_array()
        .map(|arr| {
            arr.iter()
                .filter_map(|a| a["name"].as_str().map(str::to_owned))
                .collect::<Vec<_>>()
                .join("/")
        })
        .unwrap_or_default();
    let mut song = Song::new(
        id,
        name,
        if artists.is_empty() {
            None
        } else {
            Some(artists)
        },
    );
    song.album = s["album"]["title"].as_str().map(str::to_owned);
    song.cover = s["album"]["cover"].as_str().map(str::to_owned);
    song.quality = Some(quality_from_metadata(&s["mediaMetadata"]));
    song
}

// 识别 Tidal 链接（https://tidal.com/browse/track/123… 等），返回 (kind, id)
fn parse_tidal_link(query: &str) -> Option<(String, String)> {
    let q = query.trim();
    if !q.starts_with("http") || !q.contains("tidal.com") {
        return None;
    }
    let lower = q.to_lowercase();
    for (tag, kind) in [("/track/", "track"), ("/album/", "album"), ("/playlist/", "playlist")] {
        if let Some(pos) = lower.find(tag) {
            let rest = &q[pos + tag.len()..];
            let id: String = rest
                .split(|c: char| !c.is_alphanumeric() && c != '-')
                .next()
                .unwrap_or("")
                .to_owned();
            if !id.is_empty() {
                return Some((kind.to_owned(), id));
            }
        }
    }
    None
}

// 按类型+ID 取：track→单曲，album→专辑曲目，playlist→列表曲目
fn search_by_id(kind: &str, id: &str) -> Result<Vec<Song>> {
    let (token, country) = effective_token()?;
    let client = http_client()?;
    let url = match kind {
        "track" => format!("{API_HOST}/tracks/{id}"),
        "album" => format!("{API_HOST}/albums/{id}/tracks"),
        "playlist" => format!("{API_HOST}/playlists/{id}/items"),
        _ => bail!("不支持的 Tidal 链接类型: {kind}"),
    };
    let resp: Value = client
        .get(&url)
        .query(&[("countryCode", country.as_str())])
        .bearer_auth(&token)
        .send()
        .context("无法连接 Tidal 接口")?
        .error_for_status()
        .context("Tidal 接口返回错误，链接或凭据是否有效？")?
        .json()
        .context("无法解析 Tidal 响应")?;
    if kind == "track" {
        return Ok(vec![track_to_song(&resp)]);
    }
    // album / playlist：items 里取曲目（playlist 的 item 是 {item: track}）
    let items = resp["items"].as_array().cloned().unwrap_or_default();
    let songs = items
        .iter()
        .map(|i| {
            let track = if kind == "playlist" { &i["item"] } else { i };
            track_to_song(track)
        })
        .collect();
    Ok(songs)
}

// Tidal 的 /search 响应里 audioQuality 字段不可靠（对 HiRes 曲目也常显示 LOSSLESS），
// 真实可用音质在 mediaMetadata.tags（如 ["LOSSLESS","HIRES_LOSSLESS"]）。
// 用 tags 判断：含 HIRES/HI_RES → "hires"，否则 "lossless"，与 netease 命名一致。
fn quality_from_metadata(mm: &Value) -> String {
    let has_hires = mm["tags"]
        .as_array()
        .map(|a| {
            a.iter().any(|t| {
                let s = t.as_str().unwrap_or("").to_ascii_uppercase();
                s.contains("HIRES") || s.contains("HI_RES")
            })
        })
        .unwrap_or(false);
    if has_hires {
        "hires".to_owned()
    } else {
        "lossless".to_owned()
    }
}

pub fn download(
    song: &Song,
    target: &Path,
    mut progress: impl FnMut(u64, u64),
) -> Result<PathBuf> {
    let (token, country) = effective_token()?;
    let client = http_client()?;
    let id = song.id();

    // 坑(实测 2026-09-03): playbackinfopostpaywall 必须用 GET + query 参数，
    // 用 POST + json body 会报 405 Method not allowed。
    let stream: Value = client
        .get(format!("{API_HOST}/tracks/{id}/playbackinfopostpaywall"))
        .query(&[
            ("audioquality", DEFAULT_QUALITY),
            ("playbackmode", "STREAM"),
            ("assetpresentation", "FULL"),
        ])
        .bearer_auth(&token)
        .send()
        .context("无法连接 Tidal 播放授权接口")?
        .error_for_status()
        .with_context(|| format!("Tidal 播放授权失败(id={id})：可能需要订阅或已过期"))?
        .json()
        .context("无法解析 Tidal 播放流信息")?;

    let manifest_mime = stream["manifestMimeType"].as_str().unwrap_or_default();
    let manifest_b64 = stream["manifest"]
        .as_str()
        .context("播放流缺少 manifest")?;
    let audio_quality = stream["audioQuality"].as_str().unwrap_or(DEFAULT_QUALITY);

    let (urls, ext_hint) = parse_manifest(manifest_mime, manifest_b64, audio_quality)?;
    if urls.is_empty() {
        bail!("Tidal 未返回可下载的分片");
    }

    // 问题2：砍掉「对每个分片逐个 HEAD 求和」的慢预检（串行几百次网络往返）。
    // 只对单文件(bts/m3u8 通常只有一个 url)做一次 HEAD 拿总量，让进度能显示百分比；
    // 多分片(DASH)则完全不加预检开销，total 传 0，UI 会显示「下载中 X.X MB」字节进度
    // (app.rs 已支持 total=0 时按字节显示，无需改 UI)。
    let total_bytes: u64 = if urls.len() == 1 {
        client
            .head(&urls[0])
            .header("User-Agent", "harp/0.1.1")
            .send()
            .ok()
            .and_then(|r| r.error_for_status().ok().and_then(|ok| ok.content_length()))
            .unwrap_or(0)
    } else {
        0
    };

    std::fs::create_dir_all(target).context("无法创建下载目录")?;
    let stem = sanitize_filename(&song.name);
    let temporary = target.join(format!(".harp-{id}.part.{ext_hint}"));

    let result = (|| -> Result<PathBuf> {
        // 问题3：分片并发拉取，避免一个接一个串行拖慢网速。
        // 每个分片下载到各自临时文件(乱序)，全部拉完后按分片顺序拼回主文件。
        // 进度用原子计数，主线程轮询上报——progress 回调仍留在本线程，无需 Send/'static。
        let seg_count = urls.len();
        let mut seg_paths = Vec::with_capacity(seg_count);
        {
            let base = temporary
                .file_name()
                .map(|s| s.to_string_lossy().into_owned())
                .unwrap_or_default();
            for i in 0..seg_count {
                seg_paths.push(temporary.with_file_name(format!("{base}.seg{i}.part")));
            }
        }

        let downloaded = Arc::new(AtomicU64::new(0));
        let done_count = Arc::new(AtomicUsize::new(0));
        let first_error: Arc<Mutex<Option<String>>> = Arc::new(Mutex::new(None));

        let mut handles = Vec::with_capacity(seg_count);
        for (i, url) in urls.iter().enumerate() {
            let seg_client = client.clone();
            let seg_url = url.clone();
            let seg_path = seg_paths[i].clone();
            let seg_dl = downloaded.clone();
            let seg_done = done_count.clone();
            let seg_err = first_error.clone();
            handles.push(std::thread::spawn(move || {
                if let Err(e) = download_segment(&seg_client, &seg_url, &seg_path, &seg_dl) {
                    let msg = format!("下载 Tidal 分片失败：{e:#}");
                    let mut err = seg_err.lock().unwrap();
                    if err.is_none() {
                        *err = Some(msg);
                    }
                }
                seg_done.fetch_add(1, Ordering::SeqCst);
            }));
        }

        progress(0, total_bytes);
        // 轮询原子计数器上报进度，直到所有分片拉完
        loop {
            let d = downloaded.load(Ordering::SeqCst);
            let finished = done_count.load(Ordering::SeqCst) == seg_count;
            progress(d, total_bytes);
            if finished {
                break;
            }
            std::thread::sleep(Duration::from_millis(150));
        }
        for h in handles {
            let _ = h.join();
        }

        if let Some(msg) = first_error.lock().unwrap().clone() {
            for sp in &seg_paths {
                let _ = std::fs::remove_file(sp);
            }
            bail!("{msg}");
        }

        // 按分片顺序拼回主文件，再走原流程做格式检测/重封装/写标签
        let mut file = File::create(&temporary).context("无法创建下载文件")?;
        for sp in &seg_paths {
            let mut seg = File::open(sp).context("无法打开分片临时文件")?;
            std::io::copy(&mut seg, &mut file).context("拼接 Tidal 分片失败")?;
        }
        file.flush().context("刷新下载文件失败")?;
        drop(file);
        for sp in &seg_paths {
            let _ = std::fs::remove_file(sp);
        }

        let (actual_format, was_mp4) = detect_audio_format(&temporary)?;
        let ext = actual_format.extension();
        let temporary_named = target.join(format!(".harp-{id}.part.{ext}"));
        if temporary_named != temporary {
            std::fs::rename(&temporary, &temporary_named).context("无法修正下载文件的真实格式")?;
        }
        // Tidal 的 HI_RES_LOSSLESS 及部分 lossless 会把 FLAC 音轨包进 MP4 容器：mpv 能播
        // (走 ffmpeg 的 isomp4)，但 harp 只认 .flac/.mp3(rodio/symphonia 无 mp4 读取器，
        // 曲库扫描 is_audio 也不收 .m4a)。检测到 mp4-裹-FLAC 时，用 ffmpeg 无损重封装成
        // 真 .flac(-c:a copy 流拷贝,不重编码,采样率/位深/时长零降级)。
        if was_mp4 {
            remux_mp4_flac(&temporary_named)?;
        }

        let meta = fetch_track_meta(&client, &token, &country, &id);
        let cover = meta
            .cover
            .as_deref()
            .and_then(|url| cover_bytes(&client, url));
        let lyrics = meta.lyrics.as_deref().map(str::to_owned);

        write_metadata(&temporary_named, actual_format, &id, song, &meta, cover, lyrics)?;

        // 质量标签按实际返回的 audio_quality: HI_RES_LOSSLESS → hi-res, 否则 lossless
        let quality_label = if audio_quality.to_ascii_uppercase().contains("HI_RES") {
            "hi-res"
        } else {
            "lossless"
        };
        let path = target.join(format!("{stem} [{quality_label}].{ext}"));
        std::fs::rename(&temporary_named, &path).context("无法完成下载文件")?;
        Ok(path)
    })();

    if result.is_err() {
        let _ = std::fs::remove_file(&temporary);
    }
    result
}

/// 并发下载单个 Tidal 分片到临时文件，并把写入的字节累加进 `downloaded`（原子计数，主线程轮询上报进度）。
fn download_segment(
    client: &reqwest::blocking::Client,
    url: &str,
    path: &Path,
    downloaded: &AtomicU64,
) -> Result<()> {
    let mut resp = client
        .get(url)
        .header("User-Agent", "harp/0.1.1")
        .send()
        .context("连接 Tidal 分片地址失败")?
        .error_for_status()
        .context("Tidal 分片地址返回错误")?;
    let mut file = File::create(path).context("创建分片临时文件失败")?;
    let mut buffer = [0_u8; 64 * 1024];
    loop {
        let count = resp
            .read(&mut buffer)
            .context("下载 Tidal 分片时连接中断")?;
        if count == 0 {
            break;
        }
        file.write_all(&buffer[..count])
            .context("写入分片临时文件失败")?;
        downloaded.fetch_add(count as u64, Ordering::SeqCst);
    }
    file.flush().context("刷新分片临时文件失败")?;
    Ok(())
}

// ---------- Token 存取 ----------
fn token_path() -> PathBuf {
    crate::home_dir().join(".harp").join("tidal.json")
}

fn load_token() -> Result<Token> {
    let raw = std::fs::read_to_string(token_path())
        .context("未找到 Tidal 凭据，请先运行 harp --login-tidal")?;
    let token: Token = serde_json::from_str(&raw).context("Tidal 凭据文件格式损坏")?;
    if token.access_token.is_empty() {
        bail!("Tidal 凭据缺少 access_token");
    }
    Ok(token)
}

fn save_token(token: &Token) -> Result<()> {
    let dir = crate::home_dir().join(".harp");
    std::fs::create_dir_all(&dir).context("无法创建 ~/.harp 目录")?;
    let json = serde_json::to_string_pretty(token).context("无法序列化 Tidal 凭据")?;
    std::fs::write(token_path(), json).context("无法写入 tidal.json")?;
    Ok(())
}

fn effective_token() -> Result<(String, String)> {
    let mut token = load_token()?;
    if now_secs() >= token.expires_at - TOKEN_REFRESH_SKEW_SECS {
        if token.refresh_token.is_empty() {
            bail!("Tidal 凭据已过期且没有 refresh_token，请重新登录");
        }
        let client = http_client()?;
        let refreshed = refresh_token(&client, &token.refresh_token, token.is_pkce)?;
        token.access_token = refreshed.access_token;
        token.expires_at = now_secs() + refreshed.expires_in;
        if !refreshed.refresh_token.is_empty() {
            token.refresh_token = refreshed.refresh_token;
        }
        if !refreshed.country_code.is_empty() {
            token.country_code = refreshed.country_code;
        }
        save_token(&token)?;
    }
    let country = if token.country_code.is_empty() {
        DEFAULT_COUNTRY.to_owned()
    } else {
        token.country_code.clone()
    };
    Ok((token.access_token, country))
}

fn try_token(client: &reqwest::blocking::Client, device_code: &str) -> Result<Option<Token>> {
    // 设备流现在用 PKCE 公共客户端：不能带 client_secret（带 secret 会报 invalid_client），
    // 只用 client_id。拿到的是可放 HiRes/24bit 的授权，标记 is_pkce=true 走 PKCE 刷新。
    let resp = client
        .post(format!("{AUTH_HOST}/token"))
        .form(&[
            ("client_id", PKCE_CLIENT_ID),
            ("device_code", device_code),
            ("grant_type", GRANT_DEVICE_CODE),
            ("scope", DEVICE_SCOPE),
        ])
        .send()
        .context("连接 Tidal token 接口失败")?;

    let status = resp.status();
    let body: Value = resp
        .json()
        .context("无法解析 Tidal token 响应")?;

    if status.is_success() {
        let country = body["user"]["countryCode"]
            .as_str()
            .unwrap_or(DEFAULT_COUNTRY)
            .to_owned();
        return Ok(Some(Token {
            access_token: body["access_token"].as_str().unwrap_or_default().to_owned(),
            refresh_token: body["refresh_token"].as_str().unwrap_or_default().to_owned(),
            user_id: body["user_id"].as_i64().unwrap_or(0),
            country_code: country,
            expires_at: now_secs() + body["expires_in"].as_i64().unwrap_or(3600),
            is_pkce: true,
        }));
    }

    let status_code = body["status"]
        .as_i64()
        .map(|_| body["status"].as_i64().unwrap_or(status.as_u16() as i64));
    if matches!(status_code, Some(400) | Some(404)) {
        // 尚在等待用户授权（authorization_pending）或过期前状态
        Ok(None)
    } else {
        let msg = body["userMessage"]
            .as_str()
            .or_else(|| body["error_description"].as_str())
            .unwrap_or("未知错误");
        bail!("Tidal 授权失败（status={status}）：{msg}");
    }
}

struct Refreshed {
    access_token: String,
    expires_in: i64,
    country_code: String,
    refresh_token: String,
}

fn refresh_token(client: &reqwest::blocking::Client, refresh_token: &str, is_pkce: bool) -> Result<Refreshed> {
    let req = client.post(format!("{AUTH_HOST}/token")).form(&[
        ("refresh_token", refresh_token),
        ("grant_type", "refresh_token"),
        ("scope", DEVICE_SCOPE),
    ]);
    // PKCE 播放器 client: 公共客户端, 不带 client_secret, 用 PKCE client_id
    // 旧 device-flow client: 带 basic auth(旧 client_id/secret)
    let req = if is_pkce {
        req.form(&[("client_id", PKCE_CLIENT_ID)])
    } else {
        req.basic_auth(CLIENT_ID, Some(CLIENT_SECRET))
            .form(&[("client_id", CLIENT_ID)])
    };
    let resp = req
        .send()
        .context("刷新 Tidal token 失败")?
        .error_for_status()
        .context("刷新 Tidal token 返回错误")?
        .json::<Value>()
        .context("无法解析刷新后的 Tidal token")?;

    Ok(Refreshed {
        access_token: resp["access_token"].as_str().unwrap_or_default().to_owned(),
        expires_in: resp["expires_in"].as_i64().unwrap_or(3600),
        country_code: resp["user"]["countryCode"]
            .as_str()
            .unwrap_or_default()
            .to_owned(),
        refresh_token: resp["refresh_token"].as_str().unwrap_or_default().to_owned(),
    })
}

// ---------- manifest 解析 ----------

fn parse_manifest(
    mime: &str,
    manifest_b64: &str,
    audio_quality: &str,
) -> Result<(Vec<String>, String)> {
    let decoded = STANDARD
        .decode(manifest_b64)
        .context("manifest base64 解码失败")?;
    let text = String::from_utf8(decoded).context("manifest 不是 UTF-8")?;

    match mime {
        "application/vnd.tidal.bts" => parse_bts_manifest(&text, audio_quality),
        "application/dash+xml" => parse_dash_manifest(&text),
        _ if text.trim_start().starts_with("#EXTM3U") => {
            Ok((parse_m3u8_segments(&text), "m4a".to_owned()))
        }
        _ => bail!("暂不支持的 Tidal manifest 类型：{mime}"),
    }
}

fn parse_bts_manifest(text: &str, audio_quality: &str) -> Result<(Vec<String>, String)> {
    let v: Value = serde_json::from_str(text).context("解析 bts manifest 失败")?;
    let urls = v["urls"]
        .as_array()
        .map(|arr| {
            arr.iter()
                .filter_map(|u| u.as_str().map(str::to_owned))
                .collect::<Vec<_>>()
        })
        .unwrap_or_default();
    if urls.is_empty() {
        bail!("bts manifest 没有分片地址");
    }
    let codecs = v["codecs"].as_str().unwrap_or_default();
    let mime_type = v["mimeType"].as_str().unwrap_or_default();
    let ext = extension_from_codecs(codecs, mime_type, audio_quality);
    Ok((urls, ext))
}

fn parse_dash_manifest(text: &str) -> Result<(Vec<String>, String)> {
    let seg_start = find_sub(text, "<SegmentTemplate").context("未找到 SegmentTemplate")?;
    let seg_close = text[seg_start..]
        .find('>')
        .context("SegmentTemplate 未闭合")?;
    let seg_el = &text[seg_start..seg_start + seg_close];
    let media = attr(seg_el, "media").context("SegmentTemplate 缺少 media 属性")?;
    let codecs = attr(seg_el, "codecs").unwrap_or_default();
    // HiRes(dash+xml) 需要先下载 initialization segment, 再拼各分片；没有 init 就纯分片
    let init = attr(seg_el, "initialization");
    let start_number: usize = attr(seg_el, "startNumber")
        .and_then(|s| s.parse().ok())
        .unwrap_or(1);

    let mut total: usize = 0;
    if let Some(ts) = find_sub(text, "<SegmentTimeline") {
        let mut rest = &text[ts..];
        while let Some(p) = find_sub(rest, "<S ") {
            let el_end = rest[p..].find('>').map(|e| p + e).unwrap_or(rest.len());
            let el = &rest[p..el_end];
            if let Some(r) = attr(el, "r") {
                total += r.parse::<usize>().unwrap_or(0).saturating_add(1);
            } else {
                total += 1;
            }
            rest = &rest[el_end + 1..];
        }
    }

    let mut urls = Vec::new();
    if let Some(init) = init {
        urls.push(init); // init segment 放最前(含 mp4 容器头)
    }
    for i in start_number..start_number + total {
        urls.push(media.replace("$Number$", &i.to_string()));
    }
    let ext = extension_from_codecs(&codecs, "audio/mp4", "HI_RES_LOSSLESS");
    Ok((urls, ext))
}

fn parse_m3u8_segments(text: &str) -> Vec<String> {
    text.lines()
        .map(str::trim)
        .filter(|line| !line.is_empty() && !line.starts_with('#'))
        .map(str::to_owned)
        .collect()
}

fn extension_from_codecs(codecs: &str, mime_type: &str, audio_quality: &str) -> String {
    let codecs = codecs.to_ascii_lowercase();
    if codecs == "flac" {
        if audio_quality.eq_ignore_ascii_case("HI_RES_LOSSLESS") {
            "m4a".to_owned()
        } else {
            "flac".to_owned()
        }
    } else if codecs.starts_with("mp4")
        || codecs.contains("m4a")
        || codecs == "eac3"
        || codecs == "ac4"
    {
        "m4a".to_owned()
    } else if mime_type.contains("flac") {
        "flac".to_owned()
    } else if mime_type.contains("mp4") || mime_type.contains("aac") {
        "m4a".to_owned()
    } else {
        "flac".to_owned()
    }
}

// ---------- 元数据 ----------

struct TrackMeta {
    title: Option<String>,
    artist: Option<String>,
    album: Option<String>,
    cover: Option<String>,
    lyrics: Option<String>,
}

impl TrackMeta {
    fn empty() -> Self {
        Self {
            title: None,
            artist: None,
            album: None,
            cover: None,
            lyrics: None,
        }
    }
}

fn fetch_track_meta(
    client: &reqwest::blocking::Client,
    token: &str,
    country: &str,
    id: &str,
) -> TrackMeta {
    let mut meta = TrackMeta::empty();

    if let Ok(v) = client
        .get(format!("{API_HOST}/tracks/{id}"))
        .query(&[("countryCode", country)])
        .bearer_auth(token)
        .send()
        .and_then(|r| r.error_for_status())
        .and_then(reqwest::blocking::Response::json::<Value>)
    {
        meta.title = v["title"].as_str().map(str::to_owned);
        meta.album = v["album"]["title"].as_str().map(str::to_owned);
        meta.cover = v["album"]["cover"].as_str().map(str::to_owned);
        meta.artist = v["artists"]
            .as_array()
            .map(|arr| {
                arr.iter()
                    .filter_map(|a| a["name"].as_str().map(str::to_owned))
                    .collect::<Vec<_>>()
                    .join("/")
            })
            .filter(|s| !s.is_empty());
    }

    if let Ok(v) = client
        .get(format!("{API_HOST}/tracks/{id}/lyrics"))
        .query(&[("countryCode", country)])
        .bearer_auth(token)
        .send()
        .and_then(|r| r.error_for_status())
        .and_then(reqwest::blocking::Response::json::<Value>)
    {
        // 坑(实测 2026-09-03): Tidal /lyrics 端点返回两个字段——
        //   lyrics    = 纯文本歌词(无时间戳)
        //   subtitles = LRC 时间轴同步歌词, 形如 "[mm:ss.xx] 歌词行"
        // 优先用 subtitles(带逐行时间戳, Lyrics::parse 能识别成 synced),
        // 没有才回退 lyrics 纯文本。
        let lyr = v["subtitles"]
            .as_str()
            .map(str::trim)
            .filter(|s| !s.is_empty())
            .or_else(|| v["lyrics"].as_str().map(str::trim).filter(|s| !s.is_empty()));
        if let Some(lyr) = lyr {
            meta.lyrics = Some(lyr.to_owned());
        }
    }

    meta
}

/// Tidal 封面的 cover 字段有时返回完整 URL，有时只返回 UUID（形如
/// `5fbfd283-d9d6-450c-9474-6c7ac056a4c1`）。UUID 需拼成 CDN 图片 URL：
/// `https://resources.tidal.com/images/{uuid横杠换成斜杠}/{size}x{size}.jpg`。
/// 坑(实测 2026-09-03)：不拼就会把 UUID 当 URL 请求 → 无法下载 → 封面丢。
fn tidal_cover_url(cover: &str) -> String {
    let trimmed = cover.trim();
    // 已带 http/https 或结束是图片扩展名 → 直接当完整 URL 用
    if trimmed.starts_with("http") || trimmed.ends_with(".jpg") || trimmed.ends_with(".png") {
        return trimmed.to_owned();
    }
    let path = trimmed.replace('-', "/");
    format!("https://resources.tidal.com/images/{path}/1280x1280.jpg")
}

fn cover_bytes(client: &reqwest::blocking::Client, url: &str) -> Option<Vec<u8>> {
    let url = tidal_cover_url(url);
    client
        .get(url)
        .header("User-Agent", "harp/0.1.1")
        .send()
        .ok()?
        .error_for_status()
        .ok()?
        .bytes()
        .ok()
        .map(|b| b.to_vec())
}

fn write_metadata(
    path: &Path,
    format: AudioFormat,
    id: &str,
    song: &Song,
    meta: &TrackMeta,
    cover: Option<Vec<u8>>,
    lyrics: Option<String>,
) -> Result<()> {
    let tag_type = format.tag_type();
    let mut file = lofty::read_from_path(path).context("无法读取已下载音频")?;

    if file.tag(tag_type).is_none() {
        file.insert_tag(Tag::new(tag_type));
    }
    let tag = file.tag_mut(tag_type).context("无法创建音频标签")?;

    let title = meta
        .title
        .clone()
        .filter(|v| !v.is_empty())
        .unwrap_or_else(|| song.name.clone());
    tag.insert_text(ItemKey::TrackTitle, title);

    if let Some(artist) = meta
        .artist
        .as_deref()
        .filter(|v| !v.is_empty())
        .or(song.artists.as_deref())
    {
        tag.insert_text(ItemKey::TrackArtist, artist.to_owned());
    }
    if let Some(album) = meta
        .album
        .as_deref()
        .filter(|v| !v.is_empty())
        .or(song.album.as_deref())
    {
        tag.insert_text(ItemKey::AlbumTitle, album.to_owned());
    }

    tag.insert_text(ItemKey::Comment, format!("TIDAL_ID={id}"));

    if let Some(lyrics) = lyrics.filter(|v| !v.is_empty()) {
        tag.insert_text(
            if format == AudioFormat::Flac {
                ItemKey::Lyrics
            } else {
                ItemKey::UnsyncLyrics
            },
            lyrics,
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

// ---------- 工具 ----------

fn http_client() -> Result<reqwest::blocking::Client> {
    reqwest::blocking::Client::builder()
        .connect_timeout(Duration::from_secs(5))
        .timeout(Duration::from_secs(60))
        .build()
        .context("无法创建网络客户端")
}

fn now_secs() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0)
}

fn find_sub(haystack: &str, needle: &str) -> Option<usize> {
    haystack.find(needle)
}

fn attr(text: &str, name: &str) -> Option<String> {
    // 精确定位属性 `name=`(避免误匹配 mediaPresentationDuration 等含 name 前缀的属性)。
    // 从 name 后找 `=`(允许 name = 带空格), 取引号值。
    let mut search_from = 0;
    while let Some(start) = text[search_from..].find(name) {
        let start = search_from + start;
        let rest = &text[start + name.len()..];
        // 必须紧跟 `=` 或空格+`=`
        let eq = rest.find('=');
        if let Some(eq) = eq {
            let before_eq = &rest[..eq];
            if before_eq.trim().is_empty() {
                // 确认为 name= 的属性
                let after_eq = &rest[eq + 1..];
                let val = after_eq.trim_start_matches('"');
                let end = val.find('"')?;
                return Some(decode_xml_entities(&val[..end]));
            }
        }
        // 不是目标(如 mediaPresentationDuration), 继续往后找
        search_from = start + name.len();
    }
    None
}

/// 解码 XML 属性值里的实体(`&amp;`→`&` 等)，否则带签名 query 的 URL 会被破坏。
fn decode_xml_entities(s: &str) -> String {
    s.replace("&amp;", "&")
        .replace("&lt;", "<")
        .replace("&gt;", ">")
        .replace("&quot;", "\"")
        .replace("&#39;", "'")
        .replace("&apos;", "'")
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum AudioFormat {
    Flac,
    M4a,
}

impl AudioFormat {
    fn extension(self) -> &'static str {
        match self {
            Self::Flac => "flac",
            Self::M4a => "m4a",
        }
    }

    fn tag_type(self) -> TagType {
        match self {
            Self::Flac => TagType::VorbisComments,
            Self::M4a => TagType::Mp4Ilst,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn attr_decodes_xml_entity_and_quotes() {
        // 真实 Tidal MPD: url 里有 &amp;(签名参数)
        let text = r#"<SegmentTemplate timescale="44100" initialization="https://x.com/0.mp4?a=1&amp;b=2" media="https://x.com/$Number$.mp4?c=3&amp;d=4" startNumber="1">"#;
        assert_eq!(
            attr(text, "initialization").as_deref(),
            Some("https://x.com/0.mp4?a=1&b=2")
        );
        assert_eq!(
            attr(text, "media").as_deref(),
            Some("https://x.com/$Number$.mp4?c=3&d=4")
        );
        assert_eq!(attr(text, "startNumber").as_deref(), Some("1"));
    }

    #[test]
    fn attr_skips_prefix_like_media_presentation_duration() {
        // 顶层 MPD 有 mediaPresentationDuration=<...>, 不能误匹配成 media
        let text = r#"<MPD mediaPresentationDuration="PT3M52S"><SegmentTemplate media="https://x/$Number$.mp4?a=1&amp;b=2" startNumber="1">"#;
        assert_eq!(attr(text, "media").as_deref(), Some("https://x/$Number$.mp4?a=1&b=2"));
    }

    #[test]
    fn parse_dash_counts_segments_including_init() {
        let text = r#"<SegmentTemplate media="https://x/$Number$.mp4" startNumber="1"><SegmentTimeline><S d="100" r="2"/><S d="50" r="0"/></SegmentTimeline></SegmentTemplate>"#;
        let (urls, _ext) = parse_dash_manifest(text).unwrap();
        // 2+1 (r=2) + 1+0 (r=0) = 4 分片, 无 init 则 4 个 url
        assert_eq!(urls.len(), 4);
        assert!(urls[0].ends_with("1.mp4"));
        assert!(urls[3].ends_with("4.mp4"));
    }
}

/// 识别下载文件的真实格式。
///
/// 返回 `(AudioFormat, was_mp4)`：
/// - `AudioFormat`：真正要保存的格式(用于扩展名与标签类型)。
/// - `was_mp4`：磁盘上是不是 mp4 容器(为 true 说明是「Tidal 把 FLAC 包进 mp4」,
///   后面需要做一次无损重封装成真 .flac)。
///
/// 坑(实测 2026-09-03)：Tidal 的 HI_RES_LOSSLESS 与部分 lossless 用 MP4 容器裹 FLAC
/// 音轨——容器 `ftyp` 开头,但音频 codec_name=flac。这时必须识别成 Flac 并解壳,否则
/// dir 里全是 harp 播不了、曲库也忽略的 .m4a。
fn detect_audio_format(path: &Path) -> Result<(AudioFormat, bool)> {
    let probe = Probe::open(path)
        .context("无法打开下载文件")?
        .guess_file_type()
        .context("无法识别下载文件的真实格式")?;
    match probe.file_type() {
        Some(FileType::Flac) => Ok((AudioFormat::Flac, false)),
        Some(FileType::Mp4) | Some(FileType::Mpeg) => {
            // mp4/m4a 容器。若音频流实际是 FLAC → 脱壳成 .flac；否则(如 AAC/AC3)保持 .m4a。
            if audio_codec_is_flac(path)? {
                Ok((AudioFormat::Flac, true))
            } else {
                Ok((AudioFormat::M4a, false))
            }
        }
        Some(format) => bail!("下载内容实际是暂不支持的 {format:?} 格式"),
        None => bail!("无法识别下载内容的真实格式"),
    }
}

/// 用 ffprobe 读容器里音频流的实际编码。Tidal 常把 FLAC 音轨包进 MP4 容器,
/// 此时容器是 mp4 但 codec_name=flac。没有 ffprobe 时该下载会失败并提示——因为缺失
/// ffprobe 意味着也无法完成后面的重封装。
fn audio_codec_is_flac(path: &Path) -> Result<bool> {
    let output = Command::new("ffprobe")
        .args(["-v", "error", "-select_streams", "a:0", "-show_entries", "stream=codec_name", "-of", "csv=p=0"])
        .arg(path)
        .output()
        .context("未找到 ffprobe：无法判断 Tidal 容器内音频编码")?;
    if !output.status.success() {
        bail!("ffprobe 无法解析下载文件");
    }
    let codec = String::from_utf8_lossy(&output.stdout).trim().to_ascii_lowercase();
    Ok(codec == "flac")
}

/// 把 mp4 容器里的 FLAC 音轨无损重封装成原生 .flac(-c:a copy 流拷贝,不重编码,
/// 采样率/位深/时长均原样)。封面与文本标签随后由 write_metadata(lofty) 补写。
fn remux_mp4_flac(path: &Path) -> Result<()> {
    // 临时文件后缀不能影响 ffmpeg 猜输出格式 → 显式 -f flac
    let tmp = path.with_extension("flac.remux");
    let status = Command::new("ffmpeg")
        .args(["-y", "-v", "error", "-i"])
        .arg(path)
        .args(["-map", "0:a:0", "-c:a", "copy", "-vn", "-f", "flac"])
        .arg(&tmp)
        .status()
        .with_context(|| {
            format!("未找到 ffmpeg：无法将 {} 重封装为 .flac", path.display())
        })?;
    if !status.success() {
        let _ = std::fs::remove_file(&tmp);
        bail!("ffmpeg 重封装失败：{}", path.display());
    }
    std::fs::rename(&tmp, path).context("无法完成 .flac 重封装")?;
    Ok(())
}

pub(crate) fn sanitize_filename(name: &str) -> String {
    let value = name
        .chars()
        .filter(|character| !r#"<>:/\\|?*\"#.contains(*character))
        .collect::<String>();

    if value.trim().is_empty() {
        "未知歌曲".to_owned()
    } else {
        value
    }
}
