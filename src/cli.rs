use std::io::Write;
use std::path::PathBuf;

use anyhow::{Context, Result};
use clap::{Args, Subcommand, ValueEnum};

use crate::{netease, qqmusic, tidal};

/// 搜索/下载的非交互 CLI 子命令。
/// 输出制表符分隔文本（id\ttitle\tartist\tquality），Unix 风格可 grep/awk/管道组合。
#[derive(Debug, Clone, Copy, PartialEq, Eq, ValueEnum)]
pub enum Source {
    /// Tidal
    Tidal,
    /// 网易云
    Netease,
    /// QQ 音乐
    Qq,
}

#[derive(Debug, Args)]
pub struct SearchArgs {
    /// 关键词，或直接贴 Tidal 链接（track/album/playlist）
    pub query: String,

    /// 音源
    #[arg(long, value_enum, default_value = "tidal")]
    pub source: Source,
}

#[derive(Debug, Args)]
pub struct GetArgs {
    /// 链接、ID 或关键词
    pub spec: String,

    /// 下载目录（默认 ./downloads）
    #[arg(long)]
    pub out: Option<PathBuf>,

    /// 音源
    #[arg(long, value_enum, default_value = "tidal")]
    pub source: Source,
}

#[derive(Debug, Subcommand)]
pub enum Cmd {
    /// 搜索：输出 编号、标题、歌手、音质（制表符分隔）
    Search(SearchArgs),
    /// 下载：链接/ID → FLAC 到目录
    Get(GetArgs),
}

pub fn run(cmd: Cmd) -> Result<()> {
    match cmd {
        Cmd::Search(a) => cmd_search(&a),
        Cmd::Get(a) => cmd_get(&a),
    }
}

fn print_row(id: &str, title: &str, artist: &str, quality: &str) {
    println!("{id}\t{title}\t{artist}\t{quality}");
}

fn progress_cb(done: u64, total: u64) {
    if total > 0 {
        eprint!("\r下载中 {:.1}/{:.1} MB", done as f64 / 1048576.0, total as f64 / 1048576.0);
    } else {
        eprint!("\r下载中 {:.1} MB", done as f64 / 1048576.0);
    }
    let _ = std::io::stderr().flush();
}

fn cmd_search(a: &SearchArgs) -> Result<()> {
    match a.source {
        Source::Tidal => {
            for s in tidal::search(&a.query)? {
                print_row(
                    &s.id(),
                    &s.name,
                    s.artists.as_deref().unwrap_or("未知"),
                    s.quality.as_deref().unwrap_or("-"),
                );
            }
        }
        Source::Netease => {
            for s in netease::search(&a.query)? {
                print_row(
                    &s.id(),
                    &s.name,
                    s.artists.as_deref().unwrap_or("未知"),
                    s.quality.as_deref().unwrap_or("-"),
                );
            }
        }
        Source::Qq => {
            for s in qqmusic::search(&a.query)? {
                print_row(
                    &s.songmid,
                    &s.name,
                    &s.singer,
                    s.quality.as_deref().unwrap_or("-"),
                );
            }
        }
    }
    Ok(())
}

fn cmd_get(a: &GetArgs) -> Result<()> {
    let dir = a.out.clone().unwrap_or_else(|| PathBuf::from("downloads"));
    let path = match a.source {
        Source::Tidal => {
            let song = tidal::search(&a.spec)?
                .into_iter()
                .next()
                .context("未找到匹配歌曲（可贴 Tidal 链接或关键词）")?;
            tidal::download(&song, &dir, progress_cb)?
        }
        Source::Netease => {
            let song = netease::search(&a.spec)?
                .into_iter()
                .next()
                .context("未找到匹配歌曲（可贴关键词）")?;
            netease::download(&song, &dir, progress_cb)?
        }
        Source::Qq => {
            let song = qqmusic::search(&a.spec)?
                .into_iter()
                .next()
                .context("未找到匹配歌曲（可贴关键词）")?;
            qqmusic::download(&song, &dir, progress_cb)?
        }
    };
    println!("\n已保存: {}", path.display());
    Ok(())
}
