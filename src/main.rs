use std::io; 
use std::path::PathBuf; 

use std::time::Duration; 

use anyhow::{Result, bail}; 
use clap::Parser; 
use crossterm::cursor::SetCursorStyle; 
use crossterm::event::{self, Event, KeyCode, KeyEvent, KeyEventKind, KeyModifiers}; 
use crossterm::execute; 
use crossterm::terminal::{
    EnterAlternateScreen, LeaveAlternateScreen, disable_raw_mode, enable_raw_mode,
    BeginSynchronizedUpdate, EndSynchronizedUpdate,
}; 
use ratatui::Terminal; 
use ratatui::backend::CrosstermBackend; 
use ratatui_image::picker::Picker; 
use ratatui_image::picker::cap_parser::QueryStdioOptions; 

use harp::app::App; 
use harp::ui;
use harp::cli;
use terminal_colorsaurus::{theme_mode, QueryOptions, ThemeMode}; 

#[derive(Debug, Parser)] 
#[command(
    name = "harp",
    version, 
    about = "FLAC/MP3 终端播放器",
    long_about = "播放本地 FLAC 和 MP3，显示内嵌封面与歌词，并支持通过本地网易云/QQ音乐/Tidal 搜索下载。",
    args_conflicts_with_subcommands = true
)]
struct Cli {
    /// 播放目录（不传则必须是子命令）
    path: Option<PathBuf>,

    #[arg(long)]
    no_audio: bool,

    #[arg(long)]
    login_net: bool,

    #[arg(long)]
    login_qq: bool,

    #[arg(long)]
    login_tidal: bool,

    #[arg(long, value_enum, default_value = "auto")]
    theme: ThemeArg,

    /// 子命令（非交互，Unix 可组合）：search / get
    #[command(subcommand)]
    command: Option<cli::Cmd>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, clap::ValueEnum)]
enum ThemeArg {
    Auto,
    Dark,
    Light,
}

struct TerminalGuard;

impl TerminalGuard {
    fn enter() -> Result<Self> {
        enable_raw_mode()?; 

        execute!(
            io::stdout(),
            EnterAlternateScreen,
            SetCursorStyle::DefaultUserShape
        )?;

        Ok(Self) 
    }
}

impl Drop for TerminalGuard {
    fn drop(&mut self) {
        let _ = disable_raw_mode(); 
        let _ = execute!(
            io::stdout(),
            SetCursorStyle::DefaultUserShape, 
            LeaveAlternateScreen              
        );
    }
}

fn main() -> Result<()> {
    let cli = Cli::parse();

    if cli.login_net {
        return run_netease_login();
    }
    if cli.login_qq {
        return run_qq_login();
    }
    if cli.login_tidal {
        return run_tidal_login();
    }

    // 非交互子命令：Unix 可组合的文本接口（search / get）
    if let Some(cmd) = cli.command {
        return cli::run(cmd);
    }

    let path = match cli.path {
        Some(p) => p,
        None => bail!("请提供播放目录，或使用子命令：harp search <词> / harp get <链接>"),
    };

    let is_dark = detect_is_dark(cli.theme);

    let _guard = TerminalGuard::enter()?;

    let picker = Picker::from_query_stdio_with_options(QueryStdioOptions {
        timeout: Duration::from_millis(300),
        ..Default::default() 
    })
    .unwrap_or_else(|_| Picker::halfblocks());

    let mut app = App::new(path, cli.no_audio, picker)?;
    app.is_dark = is_dark;

    let backend = CrosstermBackend::new(io::stdout());
    let mut terminal = Terminal::new(backend)?;
    terminal.clear()?; 

    let result = run(&mut terminal, &mut app);

    let save_result = app.save();

    result?;
    save_result?;
    Ok(())
}

fn run_netease_login() -> Result<()> {
    use anyhow::Context;
    let unikey = harp::netease_net::qr_unikey()?;
    let qr = harp::netease_net::qr_png(&unikey)?;
    println!("请用网易云音乐 App 扫弹出的二维码图片（已保存 {}，自动打开）：", qr.display());
    harp::qqmusic::open_file(&qr);
    for _ in 0..90 {
        std::thread::sleep(std::time::Duration::from_secs(2));
        let (code, cookie) = harp::netease_net::qr_poll(&unikey)?;
        match code {
            801 => println!("  等待扫码..."),
            802 => println!("  已扫码，请在手机上确认..."),
            803 => {
                let cookie = cookie.context("登录成功但未取到 cookie")?;
                let dir = harp::home_dir().join(".harp");
                std::fs::create_dir_all(&dir)?;
                let target = dir.join("netease_cookie.txt");
                std::fs::write(&target, &cookie).context("写入 cookie 失败")?;
                println!("  登录成功！cookie 已写入 {}", target.display());
                return Ok(());
            }
            other => anyhow::bail!("登录失败(code={other})"),
        }
    }
    anyhow::bail!("扫码超时，请重试");
}

fn run_qq_login() -> Result<()> {
    harp::qqmusic::login_wx()
}

fn run_tidal_login() -> Result<()> {
    harp::tidal::login()
}

fn detect_is_dark(mode: ThemeArg) -> bool {
    match mode {
        ThemeArg::Dark => true,
        ThemeArg::Light => false,
        ThemeArg::Auto => !matches!(theme_mode(QueryOptions::default()), Ok(ThemeMode::Light)),
    }
}

fn run(terminal: &mut Terminal<CrosstermBackend<io::Stdout>>, app: &mut App) -> Result<()> {
    draw_and_sync_cursor(terminal, app)?;

    while !app.should_quit {
        if event::poll(Duration::from_millis(100))? {
            match event::read()? {
                // Resize is the big one: previously it was swallowed by the
                // `if let Event::Key` chain, so the redraw lagged a whole poll
                // tick (~100ms) while the terminal already reflowed to the new
                // size. On drag/resize that stale frame is exactly the "flash".
                // Now: redraw immediately, inside one synchronized pass.
                Event::Resize(_, _) => {
                    app.tick();
                    if !app.should_quit {
                        draw_and_sync_cursor(terminal, app)?;
                    }
                    continue;
                }
                Event::Key(key) if key.kind != KeyEventKind::Release => handle_key(app, key),
                _ => {}
            }
        }

        app.tick();

        if !app.should_quit {
            draw_and_sync_cursor(terminal, app)?;
        }
    }
    Ok(())
}

fn draw_and_sync_cursor(
    terminal: &mut Terminal<CrosstermBackend<io::Stdout>>,
    app: &mut App,
) -> Result<()> {
    execute!(io::stdout(), BeginSynchronizedUpdate)?;

    if app.needs_full_redraw {
        terminal.clear()?;
        app.needs_full_redraw = false;
    }

    terminal.draw(|frame| ui::draw(frame, app))?;

    execute!(io::stdout(), EndSynchronizedUpdate)?;
    Ok(())
}

fn handle_key(app: &mut App, key: KeyEvent) {
    if app.help_visible {
        if matches!(key.code, KeyCode::Esc | KeyCode::Char('?')) {
            app.set_help_visible(false);
        }
        return; 
    }

    if app.delete_confirm {
        match key.code {
            KeyCode::Char('y' | 'Y') | KeyCode::Enter => app.confirm_delete(true),
            KeyCode::Char('n' | 'N') | KeyCode::Esc => app.confirm_delete(false),
            _ => {} 
        }
        return;
    }

    if app.search_input.is_some() {
        if !app.search_results.is_empty() {
            match key.code {
                KeyCode::Esc => app.close_search(), 
                KeyCode::Up | KeyCode::Left | KeyCode::Char('k' | 'K') => app.move_search(-1), 
                KeyCode::Down | KeyCode::Right | KeyCode::Char('j' | 'J') => app.move_search(1), 
                KeyCode::Enter => app.download_selected(), 

                KeyCode::Tab => app.toggle_search_source(),
                _ => {}
            }
        } else {
            match key.code {
                KeyCode::Esc => app.close_search(),
                KeyCode::Enter => app.finish_search(), 
                KeyCode::Tab => app.toggle_search_source(), 
                KeyCode::Left => app.move_search_cursor(-1), 
                KeyCode::Right => app.move_search_cursor(1), 
                KeyCode::Home => app.search_cursor_home(), 
                KeyCode::End => app.search_cursor_end(), 
                KeyCode::Backspace => app.search_backspace(), 
                KeyCode::Delete => app.search_delete(), 
                KeyCode::Char(c) => app.search_key(c), 
                _ => {}
            }
        }
        return;
    }

    match key.code {
        KeyCode::Up if !app.lyrics_focused => app.move_queue(-1),
        KeyCode::Down if !app.lyrics_focused => app.move_queue(1),
        KeyCode::Enter if !app.lyrics_focused => app.play_selected(),

        KeyCode::Delete | KeyCode::Backspace if !app.lyrics_focused => app.ask_delete(),
        KeyCode::Char('/') => app.start_search(), 
        KeyCode::Char('?') => app.set_help_visible(true), 
        KeyCode::Char('q' | 'Q') => app.should_quit = true, 
        KeyCode::Char(' ') => app.toggle_pause(), 
        KeyCode::Char('n' | 'N') => app.next_manual(), 
        KeyCode::Char('p' | 'P') => app.previous(), 
        KeyCode::Char('m' | 'M') => app.playback.toggle_mute(), 
        KeyCode::Char('r' | 'R') => app.cycle_mode(), 
        KeyCode::Char('l' | 'L') => {
            app.lyrics_focused = !app.lyrics_focused;
            app.status = None;
        }

        KeyCode::Char('+' | '=') => app.playback.adjust_volume(0.05),
        KeyCode::Char('-' | '_') => app.playback.adjust_volume(-0.05),

        KeyCode::Left => app.seek(if key.modifiers.contains(KeyModifiers::SHIFT) {
            -30
        } else {
            -5
        }),
        KeyCode::Right => app.seek(if key.modifiers.contains(KeyModifiers::SHIFT) {
            30
        } else {
            5
        }),

        KeyCode::Up if app.lyrics_focused => app.scroll_lyrics(-1),
        KeyCode::Down if app.lyrics_focused => app.scroll_lyrics(1),
        _ => {} 
    }
}
