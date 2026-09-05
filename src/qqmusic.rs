use std::collections::HashMap;
use std::fs::File;
use std::io::{Read, Write};
use std::path::{Path, PathBuf};
use std::time::Duration;

use anyhow::{Context, Result, bail};
use image::GenericImageView;
use lofty::config::WriteOptions;
use lofty::file::{AudioFile, FileType, TaggedFileExt};
use lofty::picture::{MimeType, Picture, PictureType};
use lofty::probe::Probe;
use lofty::tag::{ItemKey, Tag, TagType};
use reqwest::header::{HeaderMap, SET_COOKIE};
use serde::{Deserialize, Serialize};

const SEARCH_LIMIT: usize = 10;
const SEARCH_URL: &str = "https://c.y.qq.com/soso/fcgi-bin/client_search_cp";
const PARSE_URL: &str = "https://u.y.qq.com/cgi-bin/musicu.fcg";
const LYRIC_URL: &str = "https://c.y.qq.com/lyric/fcgi-bin/fcg_query_lyric_new.fcg";
const QRC_URL: &str = "https://c.y.qq.com/qqmusic/fcgi-bin/lyric_download.fcg";
const COVER_URL_TEMPLATE: &str =
    "https://y.qq.com/music/photo_new/T002R{size}x{size}M000{albummid}.jpg";
const QQ_REFERER: &str = "https://y.qq.com/";
const QQ_UA: &str = "Mozilla/5.0 (iPhone; CPU iPhone OS 13_2_3 like Mac OS X)";
const MIN_COVER_WIDTH: u32 = 500;
const QQ_QR_SHOW_URL: &str = "https://ssl.ptlogin2.qq.com/ptqrshow";
const QQ_QR_POLL_URL: &str = "https://ssl.ptlogin2.qq.com/ptqrlogin";
const QQ_CHECK_SIG_URL: &str = "https://ssl.ptlogin2.graph.qq.com/check_sig";
const QQ_AUTHORIZE_URL: &str = "https://graph.qq.com/oauth2.0/authorize";
const WX_QR_URL: &str = "https://open.weixin.qq.com/connect/qrconnect";
const WX_POLL_URL: &str = "https://lp.open.weixin.qq.com/connect/l/qrconnect";

#[derive(Debug, Clone)]
pub struct Song {
    pub name: String,
    pub singer: String,
    pub album: String,
    pub songmid: String,
    pub songid: String,
    pub albummid: String,
    pub quality: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct Credential {
    #[serde(default)]
    openid: String,
    #[serde(default)]
    refresh_token: String,
    #[serde(default)]
    access_token: String,
    #[serde(default)]
    expired_at: i64,
    #[serde(default)]
    musicid: i64,
    #[serde(default)]
    musickey: String,
    #[serde(default)]
    unionid: String,
    #[serde(default)]
    str_musicid: String,
    #[serde(default)]
    refresh_key: String,
    #[serde(default, rename = "musickeyCreateTime")]
    musickey_create_time: i64,
    #[serde(default, rename = "keyExpiresIn")]
    key_expires_in: i64,
    #[serde(default, rename = "loginType")]
    login_type: i64,
}

impl Credential {
    fn musicid_string(&self) -> String {
        if !self.str_musicid.is_empty() {
            self.str_musicid.clone()
        } else {
            self.musicid.to_string()
        }
    }

    fn to_cookie_header(&self) -> String {
        let uin = self.musicid_string();
        format!(
            "uin={uin}; qqmusic_uin={uin}; qqmusic_key={}; qm_keyst={}",
            self.musickey, self.musickey
        )
    }
}

fn set_cookie_value(headers: &HeaderMap, name: &str) -> Option<String> {
    headers.get_all(SET_COOKIE).iter().find_map(|value| {
        let value = value.to_str().ok()?;
        let (pair, _) = value.split_once(';')?;
        let (key, val) = pair.split_once('=')?;
        (key == name && !val.is_empty()).then(|| val.to_owned())
    })
}

fn parse_ptui_args(text: &str) -> Vec<String> {
    let Some(start) = text.find('(') else {
        return Vec::new();
    };
    let Some(end) = text.rfind(')') else {
        return Vec::new();
    };
    let mut out = Vec::new();
    let mut cur = String::new();
    let mut in_quote = false;
    let mut escaped = false;
    for ch in text[start + 1..end].chars() {
        if escaped {
            cur.push(ch);
            escaped = false;
            continue;
        }
        if ch == '\\' && in_quote {
            escaped = true;
            continue;
        }
        if ch == '\'' {
            if in_quote {
                out.push(cur.clone());
                cur.clear();
            }
            in_quote = !in_quote;
            continue;
        }
        if in_quote {
            cur.push(ch);
        }
    }
    out
}

fn query_value(url: &str, key: &str) -> Option<String> {
    let needle = format!("{key}=");
    url.split(['?', '&']).find_map(|part| {
        let value = part.strip_prefix(&needle)?;
        Some(value.split('&').next().unwrap_or(value).to_owned())
    })
}

fn between<'a>(text: &'a str, left: &str, right: &str) -> Option<&'a str> {
    let start = text.find(left)? + left.len();
    let rest = &text[start..];
    let end = rest.find(right)?;
    Some(&rest[..end])
}

pub fn open_file(path: &Path) {
    #[cfg(target_os = "windows")]
    {
        let _ = std::process::Command::new("explorer").arg(path).spawn();
    }
    #[cfg(target_os = "macos")]
    {
        let _ = std::process::Command::new("open").arg(path).spawn();
    }
    #[cfg(all(unix, not(target_os = "macos")))]
    {
        let _ = std::process::Command::new("xdg-open").arg(path).spawn();
    }
}

fn save_auth_files(credential: &Credential) -> Result<()> {
    let dir = crate::home_dir().join(".harp");
    std::fs::create_dir_all(&dir).context("无法创建 ~/.harp 目录")?;
    let json = serde_json::to_string_pretty(credential).context("无法序列化登录凭证")?;
    std::fs::write(dir.join("qqmusic-credential.json"), json).context("无法写入 qqmusic-credential.json")?;
    std::fs::write(dir.join("cookie.txt"), credential.to_cookie_header()).context("无法写入 cookie.txt")?;
    Ok(())
}

pub fn login_qq() -> Result<()> {
    let client = reqwest::blocking::Client::builder()
        .connect_timeout(Duration::from_secs(10))
        .timeout(Duration::from_secs(35))
        .redirect(reqwest::redirect::Policy::none())
        .build()
        .context("无法创建登录客户端")?;

    let qr_resp = client
        .get(QQ_QR_SHOW_URL)
        .query(&[
            ("appid", "716027609"),
            ("e", "2"),
            ("l", "M"),
            ("s", "3"),
            ("d", "72"),
            ("v", "4"),
            ("t", "0.1"),
            ("daid", "383"),
            ("pt_3rd_aid", "100497308"),
        ])
        .header("Referer", "https://xui.ptlogin2.qq.com/")
        .header(
            "User-Agent",
            "Mozilla/5.0 (Windows NT 10.0; Win64; x64) AppleWebKit/537.36",
        )
        .send()
        .context("无法获取 QQ 登录二维码")?
        .error_for_status()
        .context("QQ 登录二维码接口返回错误")?;
    let qrsig = set_cookie_value(qr_resp.headers(), "qrsig").context("二维码响应缺少 qrsig")?;
    let qr_path = PathBuf::from("qq-login.png");
    std::fs::write(&qr_path, qr_resp.bytes().context("无法读取二维码图片")?)
        .context("无法保存 qq-login.png")?;
    open_file(&qr_path);
    println!("已打开 qq-login.png，请用 QQ 扫码并在手机上确认登录。");

    let mut sigx = String::new();
    let mut uin = String::new();
    for _ in 0..90 {
        std::thread::sleep(Duration::from_secs(2));
        let action = format!("0-0-{}", chrono_like_millis());
        let poll = client
            .get(QQ_QR_POLL_URL)
            .query(&[
                ("u1", "https://graph.qq.com/oauth2.0/login_jump"),
                ("ptqrtoken", &qrsig_token(&qrsig).to_string()),
                ("ptredirect", "0"),
                ("h", "1"),
                ("t", "1"),
                ("g", "1"),
                ("from_ui", "1"),
                ("ptlang", "2052"),
                ("action", &action),
                ("js_ver", "20102616"),
                ("js_type", "1"),
                ("pt_uistyle", "40"),
                ("aid", "716027609"),
                ("daid", "383"),
                ("pt_3rd_aid", "100497308"),
                ("has_onekey", "1"),
            ])
            .header("Referer", "https://xui.ptlogin2.qq.com/")
            .header("Cookie", format!("qrsig={qrsig}"))
            .header(
                "User-Agent",
                "Mozilla/5.0 (Windows NT 10.0; Win64; x64) AppleWebKit/537.36",
            )
            .send()
            .context("无法轮询 QQ 扫码状态")?
            .text()
            .context("无法读取 QQ 扫码状态")?;
        let args = parse_ptui_args(&poll);
        let code = args.first().map(String::as_str).unwrap_or("");
        match code {
            "66" => {
                println!("等待扫码...");
            }
            "67" => {
                println!("已扫码，等待手机确认...");
            }
            "0" => {
                let jump = args.get(2).context("QQ 登录成功但缺少跳转地址")?;
                sigx = query_value(jump, "ptsigx").context("跳转地址缺少 ptsigx")?;
                uin = query_value(jump, "uin").context("跳转地址缺少 uin")?;
                break;
            }
            "65" => bail!("二维码已过期，请重新运行 qqdl-ex --login"),
            "68" => bail!("手机端已取消登录"),
            other => bail!("QQ 扫码状态异常：code={other}, raw={poll}"),
        }
    }
    if sigx.is_empty() || uin.is_empty() {
        bail!("等待登录超时");
    }

    let check_sig = client
        .get(QQ_CHECK_SIG_URL)
        .query(&[
            ("uin", uin.as_str()),
            ("pttype", "1"),
            ("service", "ptqrlogin"),
            ("nodirect", "0"),
            ("ptsigx", sigx.as_str()),
            ("s_url", "https://graph.qq.com/oauth2.0/login_jump"),
            ("ptlang", "2052"),
            ("ptredirect", "100"),
            ("aid", "716027609"),
            ("daid", "383"),
            ("j_later", "0"),
            ("low_login_hour", "0"),
            ("regmaster", "0"),
            ("pt_login_type", "3"),
            ("pt_aid", "0"),
            ("pt_aaid", "16"),
            ("pt_light", "0"),
            ("pt_3rd_aid", "100497308"),
        ])
        .header("Referer", "https://xui.ptlogin2.qq.com/")
        .header(
            "User-Agent",
            "Mozilla/5.0 (Windows NT 10.0; Win64; x64) AppleWebKit/537.36",
        )
        .send()
        .context("无法完成 QQ check_sig")?;
    let p_skey =
        set_cookie_value(check_sig.headers(), "p_skey").context("check_sig 缺少 p_skey")?;
    let p_uin = set_cookie_value(check_sig.headers(), "p_uin").unwrap_or_else(|| format!("o{uin}"));
    let graph_cookie = format!("p_skey={p_skey}; p_uin={p_uin}; uin=o{uin}");
    let auth_time = chrono_like_millis().to_string();
    let ui = format!("qqdl-{}", auth_time);
    let authorize = client
        .post(QQ_AUTHORIZE_URL)
        .form(&[
            ("response_type", "code"),
            ("client_id", "100497308"),
            (
                "redirect_uri",
                "https://y.qq.com/portal/wx_redirect.html?login_type=1&surl=https://y.qq.com/",
            ),
            ("scope", "get_user_info,get_app_friends"),
            ("state", "state"),
            ("switch", ""),
            ("from_ptlogin", "1"),
            ("src", "1"),
            ("update_auth", "1"),
            ("openapi", "1010_1030"),
            ("g_tk", &gtk(&p_skey).to_string()),
            ("auth_time", &auth_time),
            ("ui", &ui),
        ])
        .header("Cookie", graph_cookie)
        .header("Referer", "https://graph.qq.com/oauth2.0/show")
        .header(
            "User-Agent",
            "Mozilla/5.0 (Windows NT 10.0; Win64; x64) AppleWebKit/537.36",
        )
        .send()
        .context("无法向 QQ 互联授权")?;
    let location = authorize
        .headers()
        .get("Location")
        .and_then(|v| v.to_str().ok())
        .unwrap_or_default()
        .to_owned();
    let code = query_value(&location, "code").context("QQ 互联授权没有返回 code")?;

    let payload = serde_json::json!({
        "comm": {
            "ct": 24,
            "cv": 4747474,
            "platform": "yqq.json",
            "format": "json",
            "inCharset": "utf-8",
            "outCharset": "utf-8",
            "notice": 0,
            "needNewCode": 1,
            "tmeLoginType": 2,
        },
        "req_0": {
            "module": "QQConnectLogin.LoginServer",
            "method": "QQLogin",
            "param": { "code": code },
        },
    });
    let login = client
        .post(PARSE_URL)
        .header("Referer", QQ_REFERER)
        .header("Origin", "https://y.qq.com")
        .header(
            "User-Agent",
            "Mozilla/5.0 (Windows NT 10.0; Win64; x64) AppleWebKit/537.36",
        )
        .json(&payload)
        .send()
        .context("无法换取 QQ 音乐登录凭证")?
        .error_for_status()
        .context("QQ 音乐登录接口返回错误")?
        .json::<LoginResponse>()
        .context("无法解析 QQ 音乐登录响应")?;
    let req = login.req_0.context("QQ 音乐登录响应缺少 req_0")?;
    let data = req.data.context("QQ 音乐登录响应缺少 data")?;
    if data
        .get("code")
        .and_then(|v| v.as_i64())
        .unwrap_or(req.code.unwrap_or(-1))
        != 0
    {
        bail!("QQ 音乐登录失败：{}", data);
    }
    let credential: Credential =
        serde_json::from_value(data).context("QQ 音乐登录成功但凭证字段无法解析")?;
    if credential.musicid <= 0 || credential.musickey.is_empty() {
        bail!("QQ 音乐登录成功但没有拿到 musicid/musickey：请确认使用 QQ 扫码登录");
    }
    save_auth_files(&credential)?;
    println!("登录成功：musicid={}", credential.musicid_string());
    println!("已写入 qqmusic-credential.json 和 cookie.txt，可以直接运行下载。");
    Ok(())
}

pub fn login_wx() -> Result<()> {
    let client = reqwest::blocking::Client::builder()
        .connect_timeout(Duration::from_secs(10))
        .timeout(Duration::from_secs(35))
        .build()
        .context("无法创建微信登录客户端")?;

    let page = client
        .get(WX_QR_URL)
        .query(&[
            ("appid", "wx48db31d50e334801"),
            (
                "redirect_uri",
                "https://y.qq.com/portal/wx_redirect.html?login_type=2&surl=https://y.qq.com/",
            ),
            ("response_type", "code"),
            ("scope", "snsapi_login"),
            ("state", "STATE"),
            (
                "href",
                "https://y.qq.com/mediastyle/music_v17/src/css/popup_wechat.css#wechat_redirect",
            ),
        ])
        .header(
            "User-Agent",
            "Mozilla/5.0 (Windows NT 10.0; Win64; x64) AppleWebKit/537.36",
        )
        .send()
        .context("无法获取微信登录页")?
        .error_for_status()
        .context("微信登录页返回错误")?
        .text()
        .context("无法读取微信登录页")?;
    let uuid = between(&page, "uuid=", "\"").context("微信登录页缺少 uuid")?;
    let qr_resp = client
        .get(format!("https://open.weixin.qq.com/connect/qrcode/{uuid}"))
        .header("Referer", WX_QR_URL)
        .header(
            "User-Agent",
            "Mozilla/5.0 (Windows NT 10.0; Win64; x64) AppleWebKit/537.36",
        )
        .send()
        .context("无法获取微信登录二维码")?
        .error_for_status()
        .context("微信二维码接口返回错误")?;
    let qr_path = PathBuf::from("wx-login.jpg");
    std::fs::write(&qr_path, qr_resp.bytes().context("无法读取微信二维码图片")?)
        .context("无法保存 wx-login.jpg")?;
    open_file(&qr_path);
    println!("已打开 wx-login.jpg，请用微信扫码并在手机上确认登录。");

    let mut wx_code = String::new();
    for _ in 0..90 {
        std::thread::sleep(Duration::from_secs(2));
        let now = chrono_like_millis().to_string();
        let status = client
            .get(WX_POLL_URL)
            .query(&[("uuid", uuid), ("_", now.as_str())])
            .header("Referer", "https://open.weixin.qq.com/")
            .header(
                "User-Agent",
                "Mozilla/5.0 (Windows NT 10.0; Win64; x64) AppleWebKit/537.36",
            )
            .send()
            .context("无法轮询微信扫码状态")?
            .text()
            .context("无法读取微信扫码状态")?;
        let errcode = between(&status, "window.wx_errcode=", ";").unwrap_or("");
        match errcode {
            "408" => println!("等待扫码..."),
            "404" => println!("已扫码，等待手机确认..."),
            "405" => {
                wx_code = between(&status, "window.wx_code='", "'")
                    .context("微信登录成功但缺少 code")?
                    .to_owned();
                break;
            }
            "402" => bail!("微信二维码已过期，请重新运行 qqdl-ex --login-wx"),
            "403" => bail!("手机端已取消微信登录"),
            other => bail!("微信扫码状态异常：code={other}, raw={status}"),
        }
    }
    if wx_code.is_empty() {
        bail!("等待微信登录超时");
    }

    let payload = serde_json::json!({
        "comm": {
            "ct": 24,
            "cv": 4747474,
            "platform": "yqq.json",
            "format": "json",
            "inCharset": "utf-8",
            "outCharset": "utf-8",
            "notice": 0,
            "needNewCode": 1,
            "tmeLoginType": 1,
        },
        "req_0": {
            "module": "music.login.LoginServer",
            "method": "Login",
            "param": {
                "code": wx_code,
                "strAppid": "wx48db31d50e334801",
            },
        },
    });
    let login = client
        .post(PARSE_URL)
        .header("Referer", QQ_REFERER)
        .header("Origin", "https://y.qq.com")
        .header(
            "User-Agent",
            "Mozilla/5.0 (Windows NT 10.0; Win64; x64) AppleWebKit/537.36",
        )
        .json(&payload)
        .send()
        .context("无法换取 QQ 音乐微信登录凭证")?
        .error_for_status()
        .context("QQ 音乐微信登录接口返回错误")?
        .json::<LoginResponse>()
        .context("无法解析 QQ 音乐微信登录响应")?;
    let req = login.req_0.context("QQ 音乐微信登录响应缺少 req_0")?;
    let data = req.data.context("QQ 音乐微信登录响应缺少 data")?;
    if data
        .get("code")
        .and_then(|v| v.as_i64())
        .unwrap_or(req.code.unwrap_or(-1))
        != 0
    {
        bail!("QQ 音乐微信登录失败：{}", data);
    }
    let credential: Credential =
        serde_json::from_value(data).context("QQ 音乐微信登录成功但凭证字段无法解析")?;
    if credential.musicid <= 0 || credential.musickey.is_empty() {
        bail!("QQ 音乐微信登录成功但没有拿到 musicid/musickey");
    }
    save_auth_files(&credential)?;
    println!("微信登录成功：musicid={}", credential.musicid_string());
    println!("已写入 qqmusic-credential.json 和 cookie.txt，可以直接运行下载。");
    Ok(())
}

fn chrono_like_millis() -> u128 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis())
        .unwrap_or(0)
}

#[derive(Deserialize)]
struct SearchResponse {
    data: Option<SearchData>,
}
#[derive(Deserialize)]
struct SearchData {
    song: Option<SearchSong>,
}
#[derive(Deserialize)]
struct SearchSong {
    list: Option<Vec<SearchItem>>,
}
#[derive(Deserialize)]
struct SearchItem {
    songmid: Option<String>,
    songid: Option<serde_json::Value>,
    songname: Option<String>,
    singer: Option<Vec<Singer>>,
    albumname: Option<String>,
    albummid: Option<String>,
}

#[derive(Deserialize)]
struct LoginResponse {
    req_0: Option<LoginReq>,
}

#[derive(Deserialize)]
struct LoginReq {
    code: Option<i64>,
    data: Option<serde_json::Value>,
}
#[derive(Deserialize)]
struct Singer {
    name: Option<String>,
}

pub fn search(query: &str) -> Result<Vec<Song>> {
    let client = http_client()?;
    let limit = SEARCH_LIMIT.to_string();
    let response = client
        .get(SEARCH_URL)
        .query(&[
            ("w", query),
            ("format", "json"),
            ("p", "1"),
            ("n", limit.as_str()),
        ])
        .header("Referer", QQ_REFERER)
        .header("User-Agent", QQ_UA)
        .send()
        .context("无法连接 QQ 音乐搜索接口")?
        .error_for_status()
        .context("QQ 音乐搜索接口返回错误")?
        .json::<SearchResponse>()
        .context("无法解析 QQ 音乐搜索结果")?;
    let list = response
        .data
        .and_then(|d| d.song)
        .and_then(|s| s.list)
        .unwrap_or_default();
    Ok(parse_search_items(list))
}

fn parse_search_items(list: Vec<SearchItem>) -> Vec<Song> {
    list.into_iter()
        .filter_map(|item| {
            let songmid = item.songmid.filter(|v| !v.is_empty())?;
            let albummid = item.albummid.filter(|v| !v.is_empty())?;
            let name = item.songname.unwrap_or_default();
            let album = item.albumname.unwrap_or_default();
            let singer = item
                .singer
                .map(|arr| {
                    arr.iter()
                        .filter_map(|s| s.name.clone())
                        .collect::<Vec<_>>()
                        .join(", ")
                })
                .unwrap_or_default();
            let songid = item
                .songid
                .map(|v| v.to_string().trim_matches('"').to_owned())
                .unwrap_or_default();
            Some(Song {
                name,
                singer,
                album,
                songmid,
                songid,
                albummid,
                quality: Some("Hi-Res优先".to_owned()),
            })
        })
        .take(SEARCH_LIMIT)
        .collect()
}

#[derive(Deserialize)]
struct ParseResponse {
    req_0: Option<ParseReq>,
    req_1: Option<ParseReq>,
}
#[derive(Deserialize)]
struct ParseReq {
    data: Option<ParseData>,
}
#[derive(Deserialize)]
struct ParseData {
    sip: Option<Vec<String>>,
    midurlinfo: Option<Vec<MidUrlInfo>>,
}
#[derive(Deserialize)]
struct MidUrlInfo {
    purl: Option<String>,
    result: Option<i64>,
    #[allow(dead_code)]
    msg: Option<String>,
    #[allow(dead_code)]
    pneedbuy: Option<i64>,
    #[allow(dead_code)]
    pneed: Option<i64>,
}

const QUALITY_KEYS: [&str; 3] = ["hires", "sq", "320"];

fn cookie_value(cookie: &str, name: &str) -> Option<String> {
    cookie.split(';').find_map(|part| {
        let (k, v) = part.trim().split_once('=')?;
        if k == name && !v.trim().is_empty() {
            Some(v.trim().to_string())
        } else {
            None
        }
    })
}

fn cookie_uin(cookie: &str) -> String {
    cookie_value(cookie, "uin")
        .or_else(|| cookie_value(cookie, "qqmusic_uin"))
        .map(|v| v.trim_start_matches('o').to_string())
        .filter(|v| v.chars().all(|c| c.is_ascii_digit()) && v != "0")
        .unwrap_or_else(|| "0".to_string())
}

fn gtk(value: &str) -> i64 {
    hash33(value, 5381)
}

fn qrsig_token(value: &str) -> i64 {
    hash33(value, 0)
}

fn hash33(value: &str, initial: i64) -> i64 {
    value
        .chars()
        .fold(initial, |hash, c| {
            hash.wrapping_add(hash.wrapping_shl(5)).wrapping_add(c as i64)
        })
        & 0x7fff_ffff
}

fn cookie_music_key(cookie: &str) -> String {
    cookie_value(cookie, "qqmusic_key")
        .or_else(|| cookie_value(cookie, "qm_keyst"))
        .unwrap_or_default()
}

fn cookie_guid(cookie: &str) -> String {
    cookie_value(cookie, "pgv_pvid")
        .filter(|v| v.chars().all(|c| c.is_ascii_digit()) && v != "0")
        .unwrap_or_else(|| {
            let nanos = std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map(|d| d.as_nanos() % 10_000_000_000)
                .unwrap_or(1);
            nanos.max(1).to_string()
        })
}

fn cookie_login_hint(cookie: &str) -> String {
    let mut names = Vec::new();
    for name in [
        "uin",
        "qqmusic_uin",
        "skey",
        "p_skey",
        "qqmusic_key",
        "wxopenid",
        "wxrefresh_token",
        "wxunionid",
        "wxuin",
        "tmeLoginType",
        "qm_keyst",
        "psrf_qqopenid",
        "psrf_qqaccess_token",
        "psrf_qqrefresh_token",
        "pgv_pvid",
    ] {
        if cookie_value(cookie, name).is_some() {
            names.push(name);
        }
    }
    if names.is_empty() {
        "无关键登录字段".into()
    } else {
        names.join(",")
    }
}

fn has_cookie(cookie: &str, name: &str) -> bool {
    cookie_value(cookie, name).is_some()
}

fn validate_cookie(cookie: &str) -> Result<()> {
    let has_music_key = has_cookie(cookie, "qqmusic_key") || has_cookie(cookie, "qm_keyst");
    let has_numeric_uin = cookie_value(cookie, "uin")
        .or_else(|| cookie_value(cookie, "qqmusic_uin"))
        .map(|v| {
            let v = v.trim_start_matches('o');
            v.chars().all(|c| c.is_ascii_digit()) && v != "0"
        })
        .unwrap_or(false);
    let has_qq_login = has_numeric_uin
        && ((has_cookie(cookie, "skey") || has_cookie(cookie, "p_skey"))
            || (has_cookie(cookie, "qqmusic_key") || has_cookie(cookie, "qm_keyst")));
    let has_wx_login = (has_cookie(cookie, "wxopenid") || has_cookie(cookie, "psrf_qqopenid"))
        && (has_cookie(cookie, "wxrefresh_token")
            || has_cookie(cookie, "psrf_qqrefresh_token")
            || has_cookie(cookie, "wxunionid"));

    if has_music_key && (has_qq_login || has_wx_login) {
        return Ok(());
    }

    let hint = cookie_login_hint(cookie);
    let has_qr_login_cookie = has_cookie(cookie, "qrsig")
        || has_cookie(cookie, "pt_login_sig")
        || has_cookie(cookie, "pt_guid_sig")
        || has_cookie(cookie, "pt_local_token");
    if has_qr_login_cookie && hint == "无关键登录字段" {
        bail!(
            "cookie.txt 不是 QQ 音乐登录态：只看到了扫码/登录页 cookie，没有 qqmusic_key、uin、wxopenid 等字段。\n  正确获取方式：打开 https://y.qq.com 并确认右上角已登录，然后用 tools/qqmusic-cookie-exporter 扩展导出；不要从登录二维码页面或 open.weixin.qq.com 请求里复制 Cookie。"
        );
    }

    bail!(
        "cookie.txt 缺少 QQ 音乐下载所需字段。当前字段=[{}]。\n  官方播放地址接口需要数字 uin/qqmusic_uin 和有效 qqmusic_key/qm_keyst。请运行 qqdl-ex --login-wx 重新生成官方凭证。",
        hint
    );
}

pub fn resolve_urls(songmid: &str, cookie: &str) -> Result<HashMap<String, String>> {
    let client = reqwest::blocking::Client::builder()
        .connect_timeout(std::time::Duration::from_secs(5))
        .timeout(std::time::Duration::from_secs(30))
        .http1_only()
        .build()
        .context("无法创建解析客户端")?;
    let file_config: HashMap<&str, (&str, &str)> = HashMap::from([
        ("hires", ("RS01", ".flac")), 
        ("sq", ("F000", ".flac")),    
        ("320", ("M800", ".mp3")),    
    ]);
    let qq_uin = cookie_uin(cookie);
    let guid = cookie_guid(cookie);
    let music_key = cookie_music_key(cookie);
    let login_hint = cookie_login_hint(cookie);
    let mut result = HashMap::new();
    let mut failures = Vec::new();
    for key in QUALITY_KEYS {
        let (prefix, ext) = file_config[key];
        let file = format!("{prefix}{songmid}{songmid}{ext}");
        let csrf = gtk(&music_key);
        let req_data = serde_json::json!({
            "req_1": {
                "module": "vkey.GetVkeyServer",
                "method": "CgiGetVkey",
                "param": {
                    "filename": [file],
                    "guid": guid.as_str(),
                    "songmid": [songmid],
                    "songtype": [0],
                    "uin": qq_uin.as_str(),
                    "loginflag": 1,
                    "platform": "20",
                },
            },
            "loginUin": qq_uin.as_str(),
            "comm": {
                "uin": qq_uin.as_str(),
                "format": "json",
                "ct": 24,
                "cv": 4747474,
                "platform": "yqq.json",
                "g_tk": csrf,
                "g_tk_new_20200303": csrf,
                "inCharset": "utf-8",
                "outCharset": "utf-8",
                "notice": 0,
                "needNewCode": 1,
                "authst": music_key.as_str(),
            },
        });
        let resp = client
            .post(PARSE_URL)
            .header("Referer", QQ_REFERER)
            .header("Origin", "https://y.qq.com")
            .header("User-Agent", "Mozilla/5.0 (Windows NT 10.0; Win64; x64) AppleWebKit/537.36 (KHTML, like Gecko) Chrome/123.0.0.0 Safari/537.36")
            .header("Cookie", cookie)
            .json(&req_data)
            .send()
            .context("无法连接 QQ 音乐解析接口")?
            .error_for_status()
            .context("QQ 音乐解析接口返回错误")?
            .json::<ParseResponse>()
            .context("无法解析接口返回")?;

        let data = resp.req_1.or(resp.req_0).and_then(|r| r.data);
        let sip = data.as_ref().and_then(|d| d.sip.clone());
        let info = data
            .as_ref()
            .and_then(|d| d.midurlinfo.as_ref())
            .and_then(|m| m.first());
        let purl = info.and_then(|m| m.purl.clone());
        let result_code = info.and_then(|m| m.result).unwrap_or(-999);
        if let (Some(sip), Some(purl)) = (sip, purl) {
            if !purl.is_empty() && sip.len() > 1 {
                let url = format!("{}{}", sip[1], purl).replace("http://", "https://");
                result.insert(key.to_string(), url);
            } else {
                failures.push(format!("{key}: purl空(result={result_code})"));
            }
        } else {
            failures.push(format!("{key}: 接口缺字段(result={result_code})"));
        }
    }
    if result.is_empty() {
        bail!(
            "解析失败：没有返回可用音源。cookie字段=[{}]，使用uin={}，guid={}，档位结果=[{}]。当前官方接口需要数字 uin/qqmusic_uin 和有效 qqmusic_key/qm_keyst；如果 cookie 里只有微信网页字段，网页可能能播，但 CLI 不能直接换出下载地址。请用新版 tools/qqmusic-cookie-exporter 导出凭证候选再排查。",
            login_hint,
            qq_uin,
            guid,
            failures.join("; ")
        );
    }
    Ok(result)
}

pub fn fetch_lyric(songid: &str, songmid: &str, cookie: &str) -> Option<String> {
    let client = http_client().ok()?;
    let mut query = vec![("format", "json"), ("nobase64", "1"), ("songtype", "0")];
    if !songid.trim().is_empty() {
        query.push(("musicid", songid));
    } else {
        query.push(("songmid", songmid));
    }
    let resp = client
        .get(LYRIC_URL)
        .query(&query)
        .header("Referer", QQ_REFERER)
        .header("User-Agent", QQ_UA)
        .header("Cookie", cookie)
        .send()
        .ok()?;
    let data: serde_json::Value = resp.json().ok()?;
    let lyric = data.get("lyric")?.as_str()?;
    if lyric.trim().is_empty() {
        None
    } else {
        Some(lyric.to_owned())
    }
}

pub fn fetch_karaoke_lyric(songid: &str, cookie: &str) -> Option<String> {
    if songid.trim().is_empty() {
        return None;
    }
    let client = http_client().ok()?;
    let xml = client
        .get(QRC_URL)
        .query(&[
            ("version", "15"),
            ("miniversion", "82"),
            ("lrctype", "4"),
            ("musicid", songid),
        ])
        .header("Referer", QQ_REFERER)
        .header("User-Agent", QQ_UA)
        .header("Cookie", cookie)
        .send()
        .ok()?
        .error_for_status()
        .ok()?
        .text()
        .ok()?;
    decode_qrc(&xml)
}

fn decode_qrc(xml: &str) -> Option<String> {
    let xml = xml.replace("<!--", "").replace("-->", "");
    let hex = extract_qrc_hex(&xml)?;
    let mut data = crate::des::hex_to_bytes(hex)?;
    crate::des::decrypt_qrc(&mut data);

    let mut decoder = flate2::read::ZlibDecoder::new(&data[..]);
    let mut out = Vec::new();
    std::io::Read::read_to_end(&mut decoder, &mut out).ok()?;
    let text = String::from_utf8_lossy(&out);

    let lyric_content = extract_lyric_content(&text)?;
    Some(html_unescape(&lyric_content))
}

fn extract_qrc_hex(xml: &str) -> Option<&str> {
    if let Some(h) = between(xml, "<![CDATA[", "]]>") {
        let h = h.trim();
        if !h.is_empty() {
            return Some(h);
        }
    }

    let s = between(xml, "<content", "</content>")?;
    let gt = s.find('>')?;
    let h = s[gt + 1..].trim();
    if h.is_empty() {
        None
    } else {
        Some(h)
    }
}

fn extract_lyric_content(xml: &str) -> Option<String> {
    const MARK: &str = "LyricContent=\"";
    let start = xml.find(MARK)? + MARK.len();
    let rest = &xml[start..];
    let end = rest.find('"')?;
    Some(rest[..end].to_string())
}

fn html_unescape(s: &str) -> String {
    s.replace("&apos;", "'")
        .replace("&quot;", "\"")
        .replace("&amp;", "&")
        .replace("&#10;", "\n")
}

fn lrc_from_qrc(qrc: &str) -> String {
    let mut out = String::new();
    for line in qrc.lines() {
        let line = line.trim_end_matches(['\r']);
        if line.trim().is_empty() {
            continue;
        }

        if let Some(inner) = line.strip_prefix('[')
            && let Some(close) = inner.find(']')
        {
            let head = &inner[..close];

            if !head.contains(',') {
                out.push_str(line);
                out.push('\n');
                continue;
            }
            let start: u64 = head
                .split(',')
                .next()
                .and_then(|s| s.parse().ok())
                .unwrap_or(0);
            let rest = &inner[close + 1..];
            out.push('[');
            out.push_str(&fmt_lrc_time(start));
            out.push(']');
            out.push_str(&qrc_line_tokens(rest));
            out.push('\n');
            continue;
        }
        out.push_str(line);
        out.push('\n');
    }
    out.trim_end().to_string()
}

fn qrc_line_tokens(body: &str) -> String {
    let chars: Vec<char> = body.chars().collect();
    let mut text = String::new();
    let mut out = String::new();
    let mut i = 0;
    while i < chars.len() {
        if let Some((off, end)) = timing_end(&chars, i) {
            if !text.is_empty() {
                out.push('<');
                out.push_str(&fmt_lrc_time(off));
                out.push('>');
                out.push_str(&text);
                text.clear();
            }
            i = end;
        } else {
            text.push(chars[i]);
            i += 1;
        }
    }
    if !text.is_empty() {
        out.push_str(&text);
    }
    out
}

fn timing_end(chars: &[char], i: usize) -> Option<(u64, usize)> {
    let mut j = i + 1;
    if j >= chars.len() || !chars[j].is_ascii_digit() {
        return None;
    }
    let mut off: u64 = 0;
    while j < chars.len() && chars[j].is_ascii_digit() {
        off = off * 10 + (chars[j] as u64 - '0' as u64);
        j += 1;
    }
    if j >= chars.len() || chars[j] != ',' {
        return None;
    }
    j += 1;
    if j >= chars.len() || !chars[j].is_ascii_digit() {
        return None;
    }
    let mut duration: u64 = 0;
    while j < chars.len() && chars[j].is_ascii_digit() {
        duration = duration * 10 + (chars[j] as u64 - '0' as u64);
        j += 1;
    }
    if j >= chars.len() || chars[j] != ')' {
        return None;
    }
    Some((off, j + 1))
}

fn fmt_lrc_time(ms: u64) -> String {
    let cs = ms / 10;
    let minutes = cs / 6000;
    let seconds = (cs % 6000) / 100;
    let centis = cs % 100;
    format!("{minutes:02}:{seconds:02}.{centis:02}")
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

pub fn download(song: &Song, target: &Path, mut progress: impl FnMut(u64, u64)) -> Result<PathBuf> {
    let cookie = load_cookie()?;
    let urls = resolve_urls(&song.songmid, &cookie).context("解析 QQ 音乐音源失败")?;
    let mut errors = Vec::new();
    for quality_key in ["hires", "sq", "320"] {
        let Some(url) = urls.get(quality_key) else {
            errors.push(format!("{quality_key}: 没有返回地址"));
            continue;
        };
        match download_resolved(song, url, quality_key, target, &cookie, &mut progress) {
            Ok(path) => return Ok(path),
            Err(e) => errors.push(format!("{quality_key}: {e:#}")),
        }
    }
    bail!("QQ 音乐没有可用音源：{}", errors.join("; "))
}

fn download_resolved(
    song: &Song,
    url: &str,
    quality_key: &str,
    dir: &Path,
    cookie: &str,
    mut progress: impl FnMut(u64, u64),
) -> Result<PathBuf> {
    let client = http_client()?;
    let mut response = client
        .get(url)
        .header("Referer", QQ_REFERER)
        .header("User-Agent", QQ_UA)
        .send()
        .context("无法连接音频下载地址")?
        .error_for_status()
        .context("音频下载地址返回错误")?;
    let total = response.content_length().unwrap_or(0);

    std::fs::create_dir_all(dir).context("无法创建下载目录")?;
    let temp_ext = if quality_key == "320" { "mp3" } else { "flac" };
    let temporary = dir.join(format!(".harp-qq-{}.part.{temp_ext}", song.songmid));

    let result = (|| -> Result<PathBuf> {
        let mut file = File::create(&temporary).context("无法创建下载文件")?;
        let mut downloaded = 0_u64;
        let mut buffer = [0_u8; 64 * 1024];
        progress(0, total);
        loop {
            let count = response.read(&mut buffer).context("下载时连接中断")?;
            if count == 0 {
                break;
            }
            file.write_all(&buffer[..count]).context("写入失败")?;
            downloaded += count as u64;
            progress(downloaded, total);
        }
        progress(total, total);
        file.flush().context("刷新失败")?;
        drop(file);

        let actual_format = detect_audio_format(&temporary)?;
        let tagged_temporary = dir.join(format!(
            ".harp-qq-{}.part.{}",
            song.songmid,
            actual_format.extension()
        ));
        if tagged_temporary != temporary {
            std::fs::rename(&temporary, &tagged_temporary).context("修正格式失败")?;
        }

        let cover = download_cover(&client, &song.albummid);

        let lyric = fetch_karaoke_lyric(&song.songid, cookie)
            .map(|qrc| lrc_from_qrc(&qrc))
            .or_else(|| fetch_lyric(&song.songid, &song.songmid, cookie));

        write_metadata(
            &tagged_temporary,
            actual_format,
            song,
            cover,
            lyric.as_deref(),
        )?;

        let stem = sanitize_filename(&format!("{} - {} ({})", song.singer, song.name, song.album));
        let path = dir.join(format!("{stem}.{}", actual_format.extension()));
        std::fs::rename(&tagged_temporary, &path).context("完成下载失败")?;
        Ok(path)
    })();

    if result.is_err() {
        let _ = std::fs::remove_file(&temporary);
        let _ = std::fs::remove_file(dir.join(format!(".harp-qq-{}.part.flac", song.songmid)));
        let _ = std::fs::remove_file(dir.join(format!(".harp-qq-{}.part.mp3", song.songmid)));
    }
    result
}

fn http_client() -> Result<reqwest::blocking::Client> {
    reqwest::blocking::Client::builder()
        .connect_timeout(std::time::Duration::from_secs(5))
        .timeout(std::time::Duration::from_secs(300))
        .build()
        .context("无法创建网络客户端")
}

fn download_cover(
    client: &reqwest::blocking::Client,
    albummid: &str,
) -> Option<(Vec<u8>, (u32, u32))> {
    for size in ["1500", "800", "500", "300"] {
        let url = COVER_URL_TEMPLATE
            .replace("{size}", size)
            .replace("{albummid}", albummid);
        let bytes = match client
            .get(&url)
            .header("Referer", QQ_REFERER)
            .header("User-Agent", QQ_UA)
            .send()
            .ok()?
            .error_for_status()
            .ok()?
            .bytes()
            .ok()?
        {
            b if b.len() > 1000 => b,
            _ => continue,
        };
        if let Ok(image) = image::load_from_memory(&bytes) {
            let dims = image.dimensions();
            if dims.0 >= MIN_COVER_WIDTH {
                return Some((bytes.to_vec(), dims));
            }
        }
    }
    None
}

fn write_metadata(
    path: &Path,
    format: AudioFormat,
    song: &Song,
    cover: Option<(Vec<u8>, (u32, u32))>,
    lyric: Option<&str>,
) -> Result<()> {
    let is_mp3 = format == AudioFormat::Mp3;
    let tag_type = format.tag_type();
    let mut file = lofty::read_from_path(path).context("无法读取已下载音频")?;
    if file.tag(tag_type).is_none() {
        file.insert_tag(Tag::new(tag_type));
    }
    let tag = file.tag_mut(tag_type).context("无法创建标签")?;
    tag.insert_text(ItemKey::TrackTitle, song.name.clone());
    if !song.singer.is_empty() {
        tag.insert_text(ItemKey::TrackArtist, song.singer.clone());
    }
    if !song.album.is_empty() {
        tag.insert_text(ItemKey::AlbumTitle, song.album.clone());
        tag.insert_text(ItemKey::AlbumArtist, song.singer.clone());
    }
    tag.insert_text(ItemKey::Comment, format!("QQMID={}", song.songmid));
    if let Some(lyric) = lyric.filter(|v| !v.is_empty()) {
        tag.insert_text(
            if is_mp3 {
                ItemKey::UnsyncLyrics
            } else {
                ItemKey::Lyrics
            },
            lyric.to_owned(),
        );
    }
    if let Some((bytes, _)) = cover {
        let mime = match image::guess_format(&bytes).ok() {
            Some(image::ImageFormat::Png) => MimeType::Png,
            Some(image::ImageFormat::Gif) => MimeType::Gif,
            Some(image::ImageFormat::Bmp) => MimeType::Bmp,
            Some(image::ImageFormat::Tiff) => MimeType::Tiff,
            Some(image::ImageFormat::WebP) => MimeType::Unknown("image/webp".to_owned()),
            _ => MimeType::Jpeg,
        };
        let picture = Picture::unchecked(bytes)
            .pic_type(PictureType::CoverFront)
            .mime_type(mime)
            .build();
        tag.set_picture(0, picture);
    }
    file.save_to_path(path, WriteOptions::default())
        .context("写入元数据失败")?;
    Ok(())
}

fn detect_audio_format(path: &Path) -> Result<AudioFormat> {
    let probe = Probe::open(path)
        .context("无法打开下载文件")?
        .guess_file_type()
        .context("无法识别下载文件的真实格式")?;
    match probe.file_type() {
        Some(FileType::Flac) => Ok(AudioFormat::Flac),
        Some(FileType::Mpeg) => Ok(AudioFormat::Mp3),
        Some(f) => bail!("暂不支持的格式 {f:?}"),
        None => bail!("无法识别下载内容的真实格式"),
    }
}

pub fn sanitize_filename(name: &str) -> String {
    let value = name
        .chars()
        .filter(|c| !r#"<>:"/\|?*"#.contains(*c))
        .collect::<String>()
        .trim()
        .to_owned();
    if value.is_empty() {
        "未知歌曲".to_owned()
    } else {
        value
    }
}

pub fn load_cookie() -> Result<String> {
    let mut candidates = Vec::new();

    candidates.push(crate::home_dir().join(".harp/cookie.txt"));
    candidates.push(crate::home_dir().join("qqdl-ex/cookie.txt"));
    if let Ok(exe) = std::env::current_exe()
        && let Some(dir) = exe.parent()
    {
        candidates.push(dir.join("cookie.txt"));
    }
    candidates.push(PathBuf::from("cookie.txt"));
    for path in candidates {
        if path.exists() {
            let text = std::fs::read_to_string(&path).context("读取 cookie.txt 失败")?;
            let text = text.trim();
            if !text.is_empty() {
                validate_cookie(text)?;
                return Ok(text.to_owned());
            }
        }
    }
    bail!(
        "找不到 cookie.txt（会查 Harp 程序同目录、当前目录、~/qqdl-ex/cookie.txt）。\n  推荐方式：先运行新版 qqdl-ex --login-wx 生成官方凭证，Harp 会自动复用 ~/qqdl-ex/cookie.txt。"
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    fn item(songmid: &str, albummid: &str, name: &str, album: &str, singer: &str) -> SearchItem {
        SearchItem {
            songmid: Some(songmid.to_owned()),
            songid: Some(serde_json::Value::from(3585884)),
            songname: Some(name.to_owned()),
            singer: Some(vec![Singer {
                name: Some(singer.to_owned()),
            }]),
            albumname: Some(album.to_owned()),
            albummid: Some(albummid.to_owned()),
        }
    }

    #[test]
    fn search_items_filter_out_incomplete_entries() {
        let songs = parse_search_items(vec![
            item("mid-ok", "album-ok", "明明就", "十二新作", "周杰伦"),
            item("mid-live", "", "明明就 (Live)", "", "周杰伦"),
            item("", "album-x", "未知", "未知", "未知"),
        ]);
        assert_eq!(songs.len(), 1);
        assert_eq!(songs[0].name, "明明就");
        assert_eq!(songs[0].singer, "周杰伦");
        assert_eq!(songs[0].quality.as_deref(), Some("Hi-Res优先"));
    }

    #[test]
    fn songid_number_is_converted_to_string() {
        let songs = parse_search_items(vec![item("mid-1", "album-1", "歌", "专辑", "歌手")]);
        assert_eq!(songs[0].songid, "3585884");
    }

    #[test]
    fn cookie_uin_prefers_numeric_music_uin() {
        let cookie = "qqmusic_uin=123456; wxuin=999; qqmusic_key=abc; qm_keyst=abc";
        assert_eq!(cookie_uin(cookie), "123456");
        assert!(validate_cookie(cookie).is_ok());
    }

    #[test]
    fn validate_cookie_rejects_weak_browser_cookie() {
        let cookie = "wxuin=123; tmeLoginType=1; qm_keyst=abc";
        let err = validate_cookie(cookie).unwrap_err().to_string();
        assert!(err.contains("缺少 QQ 音乐下载所需字段"));
    }

    #[test]
    fn parse_response_accepts_musicu_shape() {
        let json = r#"{
            "req_1": {
                "data": {
                    "sip": ["http://a/", "http://b/"],
                    "midurlinfo": [{"purl": "F000midmid.flac?vkey=x", "result": 0}]
                }
            }
        }"#;
        let parsed: ParseResponse = serde_json::from_str(json).unwrap();
        let data = parsed.req_1.unwrap().data.unwrap();
        assert_eq!(data.sip.unwrap()[1], "http://b/");
        let info = data.midurlinfo.unwrap();
        assert_eq!(info[0].result, Some(0));
        assert_eq!(info[0].purl.as_deref(), Some("F000midmid.flac?vkey=x"));
    }

    #[test]
    fn lrc_from_qrc_produces_word_timestamps() {
        let qrc = "[0,2000]晴(0,160)天(160,160) (320,160)-(480,160) 周(640,160)";
        let lrc = lrc_from_qrc(qrc);

        assert!(lrc.starts_with("[00:00.00]<00:00.00>晴<00:00.16>天"), "实际: {lrc:?}");

        assert!(lrc.contains("<00:00.32> "), "空格字应带时间: {lrc:?}");
        assert!(lrc.contains("<00:00.48>-"), "横杠字应带时间: {lrc:?}");
    }

    #[test]
    fn lrc_from_qrc_keeps_metadata_lines() {
        let qrc = "[ti:晴天]\n[ar:周杰伦]\n[0,2000]晴(0,160)天(160,160)";
        let lrc = lrc_from_qrc(qrc);
        assert!(lrc.contains("[ti:晴天]"));
        assert!(lrc.contains("[ar:周杰伦]"));

        assert!(lrc.contains("[00:00.00]<00:00.00>晴"));
    }
}
