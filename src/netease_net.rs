use anyhow::{Result, Context, bail};
use md5::Digest;
use qrcode::{Color, QrCode};
use rand::Rng;
use serde_json::Value;

use crate::netease::Song;

pub(crate) const UA: &str = "Mozilla/5.0 (Windows NT 10.0; WOW64) AppleWebKit/537.36 (KHTML, like Gecko) Safari/537.36 Chrome/91.0.4472.164 NeteaseMusicDesktop/2.10.2.200154";
pub(crate) const REFERER: &str = "https://music.163.com/";
const AES_KEY: &[u8] = b"e82ckenh8dichen8";

const CLOUDSEARCH_API: &str = "https://music.163.com/api/cloudsearch/pc";
const SONG_URL_V1: &str = "https://interface3.music.163.com/eapi/song/enhance/player/url/v1";
const SONG_DETAIL_V3: &str = "https://interface3.music.163.com/api/v3/song/detail";
const LYRIC_API: &str = "https://interface3.music.163.com/api/song/lyric";

fn http_client() -> Result<reqwest::blocking::Client> {
    reqwest::blocking::Client::builder()
        .connect_timeout(std::time::Duration::from_secs(5))
        .timeout(std::time::Duration::from_secs(30))
        .build()
        .context("无法创建网络客户端")
}

fn md5_hex(s: &str) -> String {
    let d = md5::Md5::digest(s.as_bytes());
    hex::encode(d)
}

fn aes_ecb_pkcs7(data: &str) -> Vec<u8> {
    use aes::cipher::{BlockEncrypt, KeyInit, generic_array::GenericArray};
    let cipher = aes::Aes128::new(GenericArray::from_slice(AES_KEY));
    let mut bytes = data.as_bytes().to_vec();
    let pad = 16 - bytes.len() % 16;
    bytes.extend(std::iter::repeat_n(pad as u8, pad));
    let mut out = Vec::with_capacity(bytes.len());
    for chunk in bytes.chunks_exact(16) {
        let mut block = GenericArray::clone_from_slice(chunk);
        cipher.encrypt_block(&mut block);
        out.extend_from_slice(&block);
    }
    out
}

fn eapi_encrypt(url: &str, payload: &str) -> String {
    let url_path = extract_path(url).replace("/eapi/", "/api/");
    let digest = md5_hex(&format!("nobody{}use{}md5forencrypt", url_path, payload));
    let params = format!("{}-36cd479b6b5-{}-36cd479b6b5-{}", url_path, payload, digest);
    hex::encode(aes_ecb_pkcs7(&params))
}

fn extract_path(url: &str) -> String {
    let after_scheme = url.split("://").nth(1).unwrap_or(url);
    let path = match after_scheme.find('/') {
        Some(i) => &after_scheme[i..],
        None => "/",
    };
    path.split('?').next().unwrap_or(path).to_owned()
}

fn url_payload(id: &str, quality: &str) -> String {
    let request_id: u32 = rand::rng().random_range(20_000_000..30_000_000);
    let header = format!(
        "{{\"os\": \"pc\", \"appver\": \"\", \"osver\": \"\", \"deviceId\": \"pyncm!\", \"requestId\": \"{}\"}}",
        request_id
    );
    let escaped = header.replace('"', "\\\"");
    format!(
        "{{\"ids\": [{}], \"level\": \"{}\", \"encodeType\": \"flac\", \"header\": \"{}\"}}",
        id, quality, escaped
    )
}

pub fn search(query: &str, limit: usize) -> Result<Vec<Song>> {
    let client = http_client()?;
    let lim = limit.to_string();
    let resp: Value = client
        .post(CLOUDSEARCH_API)
        .header("User-Agent", UA)
        .header("Referer", REFERER)
        .form(&[("s", query), ("type", "1"), ("limit", lim.as_str())])
        .send()
        .context("无法连接网易云搜索接口")?
        .error_for_status()
        .context("网易云搜索接口返回错误")?
        .json()
        .context("无法解析网易云搜索结果")?;

    let songs = resp["result"]["songs"].as_array().cloned().unwrap_or_default();
    let out = songs
        .into_iter()
        .take(limit)
        .map(|s| {
            let id = s["id"].to_string().trim_matches('"').to_owned();
            let name = s["name"].as_str().unwrap_or_default().to_owned();
            let artists = s["ar"]
                .as_array()
                .map(|arr| {
                    arr.iter()
                        .filter_map(|a| a["name"].as_str().map(str::to_owned))
                        .collect::<Vec<_>>()
                        .join("/")
                })
                .unwrap_or_default();
            Song::new(
                id,
                name,
                if artists.is_empty() {
                    None
                } else {
                    Some(artists)
                },
            )
        })
        .collect();
    Ok(out)
}

#[derive(Debug, Clone, Default)]
pub struct NeteaseUrl {
    pub url: Option<String>,
    pub format: Option<String>, 
    pub level: Option<String>,  
    pub size: Option<u64>,
    pub bitrate: Option<u64>,
}

pub(crate) fn login_cookie() -> Option<String> {
    let p = crate::home_dir().join(".harp").join("netease_cookie.txt");
    let s = std::fs::read_to_string(&p).ok()?;
    let s = s.trim();
    if s.is_empty() { None } else { Some(s.to_owned()) }
}

pub fn resolve_url(id: &str, quality: &str) -> Result<NeteaseUrl> {
    let client = http_client()?;
    let payload = url_payload(id, quality);
    let params = eapi_encrypt(SONG_URL_V1, &payload);

    let cookie = login_cookie()
        .unwrap_or_else(|| "os=pc; appver=; osver=; deviceId=pyncm!".to_owned());
    let resp: Value = client
        .post(SONG_URL_V1)
        .header("User-Agent", UA)
        .header("Referer", REFERER)
        .header("Cookie", cookie)
        .form(&[("params", params)])
        .send()
        .context("无法连接网易云解析接口")?
        .error_for_status()
        .context("网易云解析接口返回错误")?
        .json()
        .context("无法解析网易云播放地址")?;

    if resp["code"] != 200 {
        bail!("网易云解析失败: {}", resp["message"].as_str().unwrap_or("未知错误"));
    }
    let d = resp["data"]
        .as_array()
        .and_then(|a| a.first())
        .cloned()
        .unwrap_or_default();
    Ok(NeteaseUrl {
        url: d["url"].as_str().map(str::to_owned),
        format: d["type"].as_str().map(str::to_owned),
        level: d["level"].as_str().map(str::to_owned),
        size: d["size"].as_u64(),
        bitrate: d["br"].as_u64(),
    })
}

#[derive(Debug, Clone, Default)]
pub struct NeteaseMeta {
    pub name: Option<String>,
    pub ar_name: Option<String>,
    pub al_name: Option<String>,
    pub lyric: Option<String>,
    pub pic: Option<String>,
}

pub fn song_info(id: &str) -> Result<NeteaseMeta> {
    let detail = song_detail(id)?;
    let s = detail["songs"]
        .as_array()
        .and_then(|a| a.first())
        .cloned()
        .unwrap_or_default();
    let lrc = lyric(id)?;
    Ok(NeteaseMeta {
        name: s["name"].as_str().map(str::to_owned),
        ar_name: s["ar"].as_array().map(|arr| {
            arr.iter()
                .filter_map(|x| x["name"].as_str())
                .collect::<Vec<_>>()
                .join("/")
        }),
        al_name: s["al"]["name"].as_str().map(str::to_owned),
        pic: s["al"]["picUrl"].as_str().map(str::to_owned),
        lyric: if lrc.is_empty() { None } else { Some(lrc) },
    })
}

pub fn cover_bytes(url: &str) -> Option<Vec<u8>> {
    http_client()
        .ok()?
        .get(url)
        .send()
        .ok()?
        .error_for_status()
        .ok()?
        .bytes()
        .ok()
        .map(|b| b.to_vec())
}

fn random_header() -> String {
    let id: u32 = rand::rng().random_range(20_000_000..30_000_000);
    format!(
        "{{\"os\": \"pc\", \"appver\": \"\", \"osver\": \"\", \"deviceId\": \"pyncm!\", \"requestId\": \"{}\"}}",
        id
    )
}

pub fn qr_unikey() -> Result<String> {
    const URL: &str = "https://interface3.music.163.com/eapi/login/qrcode/unikey";
    let client = http_client()?;
    let header = random_header().replace('"', "\\\"");
    let payload = format!("{{\"type\": 1, \"header\": \"{}\"}}", header);
    let params = eapi_encrypt(URL, &payload);
    let body = client
        .post(URL)
        .header("User-Agent", UA)
        .header("Referer", REFERER)
        .header("Cookie", "os=pc; appver=; osver=; deviceId=pyncm!")
        .form(&[("params", params)])
        .send()
        .context("无法连接网易云扫码接口")?
        .error_for_status()
        .context("网易云扫码接口返回错误")?
        .text()
        .context("无法读取网易云扫码响应")?;
    let resp: Value = serde_json::from_str(&body).context("无法解析网易云扫码响应")?;
    if resp["code"] != 200 {
        bail!("网易云扫码 unikey 失败: {}", resp["message"].as_str().unwrap_or("未知错误"));
    }
    Ok(resp["unikey"].as_str().unwrap_or_default().to_owned())
}

pub fn qr_poll(unikey: &str) -> Result<(i64, Option<String>)> {
    const URL: &str = "https://interface3.music.163.com/eapi/login/qrcode/client/login";
    let client = http_client()?;
    let header = random_header().replace('"', "\\\"");
    let payload = format!("{{\"key\": \"{}\", \"type\": 1, \"header\": \"{}\"}}", unikey, header);
    let params = eapi_encrypt(URL, &payload);
    let resp = client
        .post(URL)
        .header("User-Agent", UA)
        .header("Referer", REFERER)
        .header("Cookie", "os=pc; appver=; osver=; deviceId=pyncm!")
        .form(&[("params", params)])
        .send()
        .context("无法连接网易云扫码接口")?
        .error_for_status()
        .context("网易云扫码接口返回错误")?;
    let music_u = resp
        .headers()
        .get_all(reqwest::header::SET_COOKIE)
        .iter()
        .filter_map(|v| v.to_str().ok())
        .find_map(|s| {
            s.split(';')
                .next()
                .and_then(|x| x.trim().strip_prefix("MUSIC_U=").map(|v| v.to_owned()))
        });
    let text = resp.text().context("无法读取网易云扫码响应")?;
    let v: Value = serde_json::from_str(&text).context("无法解析网易云扫码响应")?;
    let code = v["code"].as_i64().unwrap_or(-1);
    let cookie = (code == 803)
        .then(|| music_u.map(|m| format!("MUSIC_U={m};os=pc;appver=;osver=;deviceId=pyncm!")))
        .flatten();
    Ok((code, cookie))
}

pub fn qr_png(unikey: &str) -> Result<std::path::PathBuf> {
    let url = format!("https://music.163.com/login?codekey={unikey}");
    let code = QrCode::new(url.as_bytes()).context("二维码生成失败")?;
    let w = code.width() as u32;
    let colors = code.to_colors();
    let mut img = image::RgbaImage::new(w, w);
    for (i, c) in colors.iter().enumerate() {
        let px = if *c == Color::Dark { [0u8, 0, 0, 255] } else { [255u8, 255, 255, 255] };
        img.put_pixel((i as u32) % w, (i as u32) / w, image::Rgba(px));
    }
    let big = image::imageops::resize(&img, w * 8, w * 8, image::imageops::FilterType::Nearest);
    let dir = crate::home_dir().join(".harp");
    std::fs::create_dir_all(&dir)?;
    let path = dir.join("netease_qr.png");
    big.save(&path).context("保存二维码图片失败")?;
    Ok(path)
}

pub fn song_detail(id: &str) -> Result<Value> {
    let client = http_client()?;
    let c = format!("[{{\"id\":{}, \"v\":0}}]", id);
    let resp: Value = client
        .post(SONG_DETAIL_V3)
        .header("User-Agent", UA)
        .header("Referer", REFERER)
        .form(&[("c", c)])
        .send()
        .context("无法连接网易云详情接口")?
        .error_for_status()
        .context("网易云详情接口返回错误")?
        .json()
        .context("无法解析网易云详情")?;
    if resp["code"] != 200 {
        bail!("网易云详情失败: {}", resp["message"].as_str().unwrap_or("未知错误"));
    }
    Ok(resp)
}

pub fn lyric(id: &str) -> Result<String> {
    let client = http_client()?;
    let resp: Value = client
        .post(LYRIC_API)
        .header("User-Agent", UA)
        .header("Referer", REFERER)
        .form(&[
            ("id", id),
            ("cp", "false"),
            ("tv", "0"),
            ("lv", "0"),
            ("rv", "0"),
            ("kv", "0"),
            ("yv", "0"),
            ("ytv", "0"),
            ("yrv", "0"),
        ])
        .send()
        .context("无法连接网易云歌词接口")?
        .error_for_status()
        .context("网易云歌词接口返回错误")?
        .json()
        .context("无法解析网易云歌词")?;
    if resp["code"] != 200 {
        return Ok(String::new());
    }
    Ok(resp["lrc"]["lyric"].as_str().unwrap_or_default().to_owned())
}

pub fn cover_url(id: &str) -> Result<Option<String>> {
    let detail = song_detail(id)?;
    Ok(detail["songs"]
        .as_array()
        .and_then(|a| a.first())
        .and_then(|s| s["al"]["picUrl"].as_str())
        .map(str::to_owned))
}
