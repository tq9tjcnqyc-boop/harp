//! ui.rs —— Harp 的全部终端界面绘制。
//!
//! 职责：把 App 的内存状态渲染成 ratatui 控件（这里不处理音源/下载/播放，只做"画"）。
//!
//! 布局（主界面）：
//! ```text
//! ┌─ 左列 ─┬─ 歌词 ──────────┐
//! │ 封面    │                  │
//! │ 队列    │                  │
//! ├────────┴──────────────────┤
//! │ 播放器（进度条）             │
//! ├───────────────────────────┤
//! │ 状态栏                       │
//! └───────────────────────────┘
//! ```
//! 左列宽度固定（LEFT_WIDTH），封面在顶部、队列吃掉剩余高度；封面高度只跟"列宽+字体"
//! 有关、跟窗口高度无关，这是刻意设计——让上下拉伸窗口时封面像素尺寸恒定，避免 resize
//! 导致图片重传/重绘闪烁（见 stacked_left_areas）。
//!
//! 除主界面外还有三种全屏态：搜索编辑、搜索结果、帮助弹窗。
//!
//! 像素换算：封面/图片按"终端 cell 的像素尺寸"（宽×高）来算比例，cell 尺寸由
//! effective_font_size 从 picker 取。

use std::time::Duration;

use ratatui::Frame;
use ratatui::layout::{Alignment, Rect};
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, Borders, Clear, Gauge, Paragraph, Wrap};
use ratatui_image::{FontSize, Resize, StatefulImage};
use unicode_segmentation::UnicodeSegmentation;
use unicode_width::UnicodeWidthStr;

use crate::app::App;
use crate::model::SearchSource;

/// 返回终端 cell 的像素尺寸（宽/高）。封面按图片像素换算成"几个 cell"需要这个基准。
pub(crate) fn effective_font_size(picker: &ratatui_image::picker::Picker) -> FontSize {
    picker.font_size()
}

// ── 颜色常量 ──────────────────────────────────────────────
// 主题色都用 Color::Reset（随终端配色），不写死具体色值——遵循"配色随终端"。
const ACCENT: Color = Color::Reset; // 主强调色：封面/队列/歌词面板边框、标题
const ACTIVE: Color = Color::Reset; // 当前选中/聚焦态：被聚焦的面板、帮助标题
const MUTED: Color = Color::Reset; // 次要/提示文字：占位、状态栏、帮助正文
const PROGRESS_BORDER: Color = Color::Reset; // 播放器面板的边框色（进度条所在）
const QQ_ACCENT: Color = Color::Reset; // QQ 音乐来源的强调色（搜索标签用）

// ── 布局常量 ──────────────────────────────────────────────
const PANEL_GAP: u16 = 2; // 纵向面板间隙（队列↔封面、播放器↔状态栏等）
const PANEL_GAP_H: u16 = 5; // 横向间隙（左列↔歌词区）
const MIN_WINDOW_WIDTH: u16 = 80; // 终端最小宽度，低于则不渲染主界面、提示放大
const MIN_WINDOW_HEIGHT: u16 = 25; // 终端最小高度，同左

/// 每帧绘制入口。按当前 App 状态分发到不同界面。
///
/// 优先级从高到低：帮助弹窗 > 搜索（编辑/结果）> 主界面。
/// 主界面在宽高低于 MIN_* 时直接渲染"请放大窗口"提示并返回。
pub fn draw(frame: &mut Frame<'_>, app: &mut App) {
    let area = frame.area();
    if app.help_visible {
        render_help(frame, area);
        return;
    }
    if app.search_input.is_some() {
        if app.search_results.is_empty() {
            render_search_editor(frame, app, area);
        } else {
            render_search_results(frame, app, area);
        }
        return;
    }

    // 窗口太小，渲染提示而不是主界面（避免布局被挤爆）。
    if area.width < MIN_WINDOW_WIDTH || area.height < MIN_WINDOW_HEIGHT {
        frame.render_widget(
            Paragraph::new(format!(
                "Harp 需要至少 {MIN_WINDOW_WIDTH}×{MIN_WINDOW_HEIGHT} 的终端\n当前：{}×{}\n\n请放大窗口",
                area.width, area.height
            ))
            .alignment(Alignment::Center)
            .block(Block::default().borders(Borders::ALL).title(" Harp ")),
            area,
        );
        return;
    }

    // 先把整块区域切成四个子区域，再分别绘制。
    let (left_area, lyrics_area, player_area, status_area) = gapped_main_areas(area);

    // 封面高度需要字体 cell 尺寸和封面图尺寸（像素）参与换算。
    let font_size = effective_font_size(&app.picker);
    let dimensions = app.cover_dimensions();
    // 左列再切一次：封面（顶）+ 队列（底）。
    let (cover_area, queue_area) = stacked_left_areas(left_area, dimensions, font_size);

    render_cover(frame, app, cover_area);
    render_queue(frame, app, queue_area);
    render_lyrics(frame, app, lyrics_area);
    render_player(frame, app, player_area);
    render_status_line(frame, app, status_area);
}

/// 将整个可用区域切成四个主面板：左列、歌词、播放器、状态栏。
///
/// 高度分配（从下往上算）：
/// 1. 状态栏固定在底部，占 STATUS_HEIGHT(1) 行。
/// 2. 播放器在状态栏上方，占 PLAYER_HEIGHT(3) 行，与状态栏隔 PANEL_GAP。
/// 3. 剩余高度给"左列 + 歌词"两块（它们等高，叫 content_height）。
/// 宽度分配：
/// - 左列固定 LEFT_WIDTH(21) 列。
/// - 歌词区从 left.right() + PANEL_GAP_H 一直铺到 area 右缘。
/// - 播放器/状态栏横跨整个宽度。
fn gapped_main_areas(area: Rect) -> (Rect, Rect, Rect, Rect) {
    const LEFT_WIDTH: u16 = 21;
    const PLAYER_HEIGHT: u16 = 3;
    const STATUS_HEIGHT: u16 = 1;

    // min 兜底：窗口太矮时先保证状态栏/播放器至少不越界（也走不到这，draw 已拦）。
    let player_height = PLAYER_HEIGHT.min(area.height);
    let status_height = STATUS_HEIGHT.min(area.height.saturating_sub(player_height));

    let status_y = area.bottom().saturating_sub(status_height);
    let player_y = status_y
        .saturating_sub(player_height)
        .saturating_sub(PANEL_GAP);
    let content_height = player_y.saturating_sub(area.y).saturating_sub(PANEL_GAP);
    let left_width = LEFT_WIDTH.min(area.width);
    let left = Rect::new(area.x, area.y, left_width, content_height);

    let lyrics_x = left.right().saturating_add(PANEL_GAP_H).min(area.right());
    let lyrics = Rect::new(
        lyrics_x,
        area.y,
        area.right().saturating_sub(lyrics_x),
        content_height,
    );
    let player = Rect::new(area.x, player_y, area.width, player_height);
    let status = Rect::new(area.x, status_y, area.width, status_height);
    (left, lyrics, player, status)
}

// ── 搜索态：编辑框 ────────────────────────────────────────
/// 渲染搜索输入框（无结果时）：中间一条输入行 + 下方操作提示 + 光标。
/// 光标用 ratatui 的 set_cursor_position 定位到文字末尾，让终端画真实的插入光标。
fn render_search_editor(frame: &mut Frame<'_>, app: &mut App, area: Rect) {
    let input = app.search_input.as_deref().unwrap_or_default();

    // 根据当前搜索来源显示标签和强调色。
    let (source_label, source_color) = match app.search_source {
        SearchSource::Netease => ("网易云", ACCENT),
        SearchSource::Qq => ("QQ音乐", QQ_ACCENT),
        SearchSource::Tidal => ("Tidal", ACCENT),
    };
    let block = Block::default()
        .borders(Borders::ALL)
        .title(Line::from(Span::styled(
            format!(" 搜索·{source_label} "),
            Style::default()
                .fg(source_color)
                .add_modifier(Modifier::BOLD),
        )));
    let inner = block.inner(area);

    frame.render_widget(block, area);

    // 输入行水平居中，宽最多 80，左右各留 2。
    let field_width = inner.width.saturating_sub(4).min(80);
    let field_x = inner.x + inner.width.saturating_sub(field_width) / 2;
    let field_y = inner.y + inner.height.saturating_sub(3) / 2;
    let field = Rect::new(field_x, field_y, field_width, 1);
    frame.render_widget(Paragraph::new(Line::raw(format!("搜索： {input}"))), field);
    frame.render_widget(
        // 输入行下方两行的操作提示。
        Paragraph::new(Line::from(Span::styled(
            "← / → 移动光标，回车搜索，Tab 切换来源，Esc 取消",
            Style::default().fg(MUTED),
        )))
        .alignment(Alignment::Center),
        Rect::new(inner.x, field_y.saturating_add(2), inner.width, 1),
    );

    // 光标定位：把"搜索："前缀 + 已输入部分算进去，找出字符末尾的列坐标。
    let prefix = input.chars().take(app.search_cursor).collect::<String>();
    let prefix_width = Line::raw(format!("搜索： {prefix}")).width() as u16;
    let cursor_x = field_x
        .saturating_add(prefix_width)
        .min(field.right().saturating_sub(1));

    frame.set_cursor_position((cursor_x, field_y));
}

// ── 搜索态：结果列表 ──────────────────────────────────────
/// 渲染搜索结果列表：每行"▶ 序号. 标题 — 歌手 [音质]"，当前选中项高亮，
/// 底部一条操作提示。整个列表垂直居中。
fn render_search_results(frame: &mut Frame<'_>, app: &App, area: Rect) {
    let block = Block::default().borders(Borders::ALL).title(" 搜索结果 ");
    let inner = block.inner(area);
    frame.render_widget(block, area);

    // 选中行用"亮"，其它用"暗"。
    let (active_style, normal_style) = lyric_color_styles(app.is_dark);
    let mut lines = app
        .search_results
        .iter()
        .enumerate()
        .map(|(i, s)| {
            let text = search_result_line(
                i,
                i == app.search_selected,
                &s.name,
                &s.artist,
                s.quality.as_deref(),
            );
            let style = if i == app.search_selected {
                active_style
            } else {
                normal_style
            };
            Line::from(Span::styled(text, style))
        })
        .collect::<Vec<_>>();

    let hint = "方向键或 J/K 选择，Enter 播放/下载，Tab 切换来源，Esc 取消";
    let hint_width = UnicodeWidthStr::width(hint) as u16;
    let hint_x = inner.x + inner.width.saturating_sub(hint_width) / 2;
    frame.render_widget(
        Paragraph::new(Line::from(Span::styled(hint, Style::default().fg(MUTED)))),
        Rect::new(hint_x, inner.bottom().saturating_sub(1), hint_width, 1),
    );

    // 结果区扣掉底部提示行的高度，剩余部分用来垂直居中列表。
    let results_area = Rect::new(
        inner.x,
        inner.y,
        inner.width,
        inner.height.saturating_sub(2),
    );
    let pad_top = usize::from(results_area.height).saturating_sub(lines.len()) / 2;

    let mut padded = vec![Line::default(); pad_top];
    padded.append(&mut lines);
    frame.render_widget(
        Paragraph::new(padded).alignment(Alignment::Center),
        results_area,
    );
}

/// 拼接一行搜索结果文本。选中项前缀"▶ "，未选中用两个空格占位，保证选中/未选中行宽一致
/// （避免切换选中时整列抖动）。
fn search_result_line(
    index: usize,
    selected: bool,
    name: &str,
    artist: &str,
    quality: Option<&str>,
) -> String {
    format!(
        "{}{:>2}. {} — {} [{}]",
        if selected { "▶ " } else { "  " },
        index + 1,
        name,
        artist,
        quality.unwrap_or("…")
    )
}

// ── 封面 ─────────────────────────────────────────────────
/// 渲染封面面板（带边框和"封面"标题）。
/// 有封面图时用 StatefulImage 按 Resize::Fit 缩放塞进框内（保持长宽比、不变形）；
/// 没有图时用一个居中的占位（音乐符号 + 标题 + 歌手）。
fn render_cover(frame: &mut Frame<'_>, app: &mut App, area: Rect) {
    let title = app.current_track().title.clone();
    let artist = app.current_track().display_artist().to_owned();
    let block = Block::default()
        .borders(Borders::ALL)
        .border_style(Style::default().fg(ACCENT).add_modifier(Modifier::BOLD))
        .title(" 封面 ");
    let inner = block.inner(area);
    frame.render_widget(block, area);

    // prepare_cover 内部按 inner 尺寸把图片缩成协议对象；尺寸不变时它直接复用缓存（不重传）。
    app.prepare_cover(inner.width, inner.height);
    if let Some(cover) = app.cover.as_mut() {
        frame.render_stateful_widget(StatefulImage::new().resize(Resize::Fit(None)), inner, cover);
    } else {
        let placeholder = format!("\n\n\n  ♪\n\n{}\n\n{}", title, artist);
        frame.render_widget(
            Paragraph::new(placeholder)
                .alignment(Alignment::Center)
                .style(Style::default().fg(MUTED))
                .wrap(Wrap { trim: true }),
            inner,
        );
    }
}

/// 把左列切成封面（顶部）和队列（底部）两块。
///
/// 核心约定：封面高度只由"左列宽 + 字体 cell + 图片长宽比"决定，与窗口高度无关。
/// 这样上下拉伸窗口时封面像素尺寸恒定，resize 不会重新缩放/重传图片，彻底不闪。
/// 队列区吃掉剩余高度；窗口矮时队列先被压缩，封面保持不动。只有封面盖过整个左区
/// （竖长图 + 极矮窗口）才裁到 area.height 兜底，此时才可能重绘。
/// 低于 MIN_WINDOW_HEIGHT 的窗口由 draw() 拦截，不会走到这里。
fn stacked_left_areas(
    area: Rect,
    dimensions: Option<(u32, u32)>,
    font_size: FontSize,
) -> (Rect, Rect) {
    let cover_height = cover_frame_height(area.width, dimensions, font_size).min(area.height);
    let cover = Rect::new(area.x, area.y, area.width, cover_height);

    let queue_y = cover.bottom().saturating_add(PANEL_GAP).min(area.bottom());
    let queue = Rect::new(
        area.x,
        queue_y,
        area.width,
        area.bottom().saturating_sub(queue_y),
    );
    (cover, queue)
}

/// 计算封面外框的高度（格数）。输入左列宽（列）、图片尺寸、字体 cell 尺寸。
///
/// 现在固定为"正方形框"：高度只由列宽 + 字体决定（`ceils = 列宽 × cell宽 / cell高`），
/// 不再随图片长宽比漂移。图片用 Resize::Fit 塞进框内自然留白，所以正方形图严丝合缝、
/// 横/竖图上下或左右留白，外框永远接近正方。
///
/// 返回 clamp 在 [10, 26] 格的范围内（太小显示不下占位、太大挤占队列）。
fn cover_frame_height(area_width: u16, dimensions: Option<(u32, u32)>, font_size: FontSize) -> u16 {
    let cw = u32::from(font_size.width).max(1);
    let ch = u32::from(font_size.height).max(1);

    // 无图时 dimensions 给 (1,1) 也能走通；只有显式 (0,x) 才提前返回占位高度。
    let (img_w, img_h) = dimensions.unwrap_or((1, 1));
    if img_w == 0 || img_h == 0 {
        return 17;
    }
    // 封面框固定为正方形：高度只由列宽+字体决定，不随图片长宽比漂移，
    // 图片用 Resize::Fit 塞进框内自然留白，外框永远是正方。
    let height = ((u32::from(area_width) as f64 * cw as f64) / ch as f64).round() as u32;
    height.clamp(10, 26) as u16
}

/// 按可用区域缩放图片的像素尺寸，保持长宽比（返回最终要渲染的像素宽/高）。
/// 取"宽、高两个比例里的较小者"作为 scale，确保图片不超出可用区域也不变形。
pub(crate) fn fitted_pixel_size(
    image_width: u32,
    image_height: u32,
    available_width: u32,
    available_height: u32,
) -> (u32, u32) {
    let scale = (available_width as f64 / image_width as f64)
        .min(available_height as f64 / image_height as f64);
    (
        (image_width as f64 * scale).round().max(1.0) as u32,
        (image_height as f64 * scale).round().max(1.0) as u32,
    )
}

// ── 歌词 ─────────────────────────────────────────────────
/// 返回歌词面板亮/暗两套配色（当前句 vs 其它句），依据终端是否深色主题。
fn lyric_color_styles(is_dark: bool) -> (Style, Style) {
    if is_dark {
        (
            Style::default()
                .fg(Color::Gray)
                .add_modifier(Modifier::BOLD),
            Style::default().fg(Color::DarkGray),
        )
    } else {
        (
            Style::default()
                .fg(Color::DarkGray)
                .add_modifier(Modifier::BOLD),
            Style::default().fg(Color::Gray),
        )
    }
}

/// 渲染歌词面板。
///
/// 两种模式：
/// - 同步歌词(且未聚焦)：以"当前运行到的句子"为中心，上下各补半屏，形成一个跟随窗口，
///   当前句高亮、前面的词逐个点亮（逐字跟随，见 active_word）。
/// - 非同步/已聚焦：按 app.lyric_scroll 手动滚动，取一个视口高度的窗口。
/// 每行逐字用 Span 渲染，因此可以做到"唱到的字变色"。
fn render_lyrics(frame: &mut Frame<'_>, app: &App, area: Rect) {
    // 聚焦歌词时在标题加圆点提示，并用 ACTIVE 边框；否则用 ACCENT。
    let focus = if app.lyrics_focused { " ●" } else { "" };
    let block = Block::default()
        .borders(Borders::ALL)
        .border_style(
            Style::default()
                .fg(if app.lyrics_focused { ACTIVE } else { ACCENT })
                .add_modifier(Modifier::BOLD),
        )
        .title(format!(" 歌词{focus} "));
    let inner = block.inner(area);
    frame.render_widget(block, area);

    if app.lyrics.lines.is_empty() {
        frame.render_widget(
            Paragraph::new("此文件没有内嵌歌词")
                .alignment(Alignment::Center)
                .style(Style::default().fg(MUTED)),
            inner,
        );
        return;
    }

    let active = app.lyrics.active_index(app.playback.position());
    let viewport = inner.height as usize;

    // 第一遍：把每句渲染成 Line（含逐字着色），同时记录"每句在 rendered 里的下标"，
    // 以及"当前句在 rendered 里的下标"，供下面做跟随窗口定位。
    let mut rendered: Vec<Line> = Vec::new();
    let mut original_to_render: Vec<usize> = Vec::new();
    let mut active_render_pos: Option<usize> = None;
    for (index, line) in app.lyrics.lines.iter().enumerate() {
        original_to_render.push(rendered.len());

        let (active_style, normal_style) = lyric_color_styles(app.is_dark);

        let line_render: Line = if Some(index) == active && !line.words.is_empty() {
            // 当前句：逐字拆分，唱到 word_idx 之前的字用亮色、之后用暗色。
            match app.lyrics.active_word(index, app.playback.position()) {
                None => Line::from(Span::styled(line.text.clone(), active_style)),
                Some(word_idx) => {
                    let spans = line
                        .words
                        .iter()
                        .enumerate()
                        .map(|(wi, word)| {
                            let style = if wi <= word_idx {
                                active_style
                            } else {
                                normal_style
                            };
                            Span::styled(word.text.clone(), style)
                        })
                        .collect::<Vec<_>>();
                    Line::from(spans)
                }
            }
        } else {
            // 非当前句：整句一个颜色；若是下一句仍稍微有点区分也可以，这里只分亮/暗。
            let style = if Some(index) == active {
                active_style
            } else {
                normal_style
            };
            Line::from(Span::styled(line.text.clone(), style))
        };

        if Some(index) == active {
            let this = rendered.len();
            rendered.push(line_render);
            active_render_pos = Some(this);
        } else {
            rendered.push(line_render);
        }
    }

    // 第二遍：从 rendered 里切出一个 viewport 高的窗口。
    let window: Vec<Line> = if app.lyrics.synced && !app.lyrics_focused {
        // 同步跟随：当前句居中，窗口上下各 half 行；开头/结尾不够时用空行补足。
        let half = viewport.saturating_sub(1) / 2;
        let center = active_render_pos.unwrap_or(0);
        let start_i = center as isize - half as isize;
        let end_i = start_i + viewport as isize;
        let mut win: Vec<Line> = Vec::new();

        if start_i < 0 {
            for _ in 0..(-start_i) {
                win.push(Line::from(""));
            }
        }

        let s = start_i.max(0) as usize;
        let mut e = end_i.max(0) as usize;
        if e > rendered.len() {
            e = rendered.len();
        }
        if s < rendered.len() {
            win.extend(rendered[s..e].iter().cloned());
        }

        while win.len() < viewport {
            win.push(Line::from(""));
        }
        win
    } else {
        // 手动滚动：从 original_to_render[lyric_scroll] 对应位置开始取满 viewport。
        let start_render = original_to_render
            .get(
                app.lyric_scroll
                    .min(original_to_render.len().saturating_sub(1)),
            )
            .copied()
            .unwrap_or(0);
        rendered
            .iter()
            .skip(start_render)
            .take(viewport)
            .cloned()
            .collect()
    };
    frame.render_widget(
        Paragraph::new(window)
            .alignment(Alignment::Center)
            .wrap(Wrap { trim: true }),
        inner,
    );
}

// ── 队列 ─────────────────────────────────────────────────
/// 渲染队列面板。每行一个"前缀(▶当前/▷选中/空格) + 标题"，配跑马灯/截断。
/// 同时负责维护选中项始终可见（自动滚到视口内的滚动位置）。
/// 注意：这里会修改 app.queue_scroll，所以接收 &mut App。
fn render_queue(frame: &mut Frame<'_>, app: &mut App, area: Rect) {
    let visible = area.height.saturating_sub(2) as usize;
    let max_offset = app.tracks.len().saturating_sub(visible);

    // 选中项滚出视口上方 → 把视口起点往上提；下方 → 往下推。保证选中项一直在视野里。
    if app.queue_selected < app.queue_scroll {
        app.queue_scroll = app.queue_selected;
    }

    if visible > 0 && app.queue_selected >= app.queue_scroll + visible {
        app.queue_scroll = app.queue_selected + 1 - visible;
    }

    app.queue_scroll = app.queue_scroll.min(max_offset);
    let offset = app.queue_scroll;
    let block = Block::default()
        .borders(Borders::ALL)
        .border_style(Style::default().fg(ACCENT).add_modifier(Modifier::BOLD))
        .title(format!(" 队列 {} 首 ", app.tracks.len()));
    let inner = block.inner(area);
    frame.render_widget(block, area);

    let title_width = usize::from(inner.width.saturating_sub(3));
    let content_x = inner.x.saturating_add(1);

    // 选中项标题若超宽则跑马灯；先算需要的总步数，再看当前步数（由 App 按时间推进）。
    let max_marquee_steps = app
        .tracks
        .get(app.queue_selected)
        .map_or(0, |track| marquee_steps(&track.title, title_width));
    let marquee_step = app.queue_marquee_step(max_marquee_steps);
    let (active_style, normal_style) = lyric_color_styles(app.is_dark);

    for (row, (index, track)) in app
        .tracks
        .iter()
        .enumerate()
        .skip(offset)
        .take(visible)
        .enumerate()
    {
        let prefix = if index == app.current {
            "▶ "
        } else if index == app.queue_selected {
            "▷ "
        } else {
            "  "
        };
        let style = if index == app.queue_selected {
            active_style
        } else {
            normal_style
        };
        let y = inner.y + row as u16;

        frame.render_widget(
            Paragraph::new(prefix).style(style),
            Rect::new(content_x, y, inner.width.min(2), 1),
        );
        if title_width > 0 {
            // 选中项且超宽 → 跑马灯滚动；否则截断到 title_width。
            let title = if index == app.queue_selected && max_marquee_steps > 0 {
                marquee_window(&track.title, marquee_step, title_width)
            } else {
                truncate_width(&track.title, title_width)
            };
            frame.render_widget(
                Paragraph::new(title).style(style),
                Rect::new(
                    content_x.saturating_add(2),
                    y,
                    inner.width.saturating_sub(3),
                    1,
                ),
            );
        }
    }
}

// ── 文本截断 / 跑马灯（按字素宽度，避免把中文劈半） ──────────
/// 计算一个文本若要完整滚完需要多少步（去掉前面逐步隐藏的字符）。
/// 思路：从第 0 个字素开始，只要剩余文本仍比 width 长就步进一次。
fn marquee_steps(text: &str, width: usize) -> usize {
    if width == 0 {
        return 0;
    }
    let mut remaining = UnicodeWidthStr::width(text);
    let mut steps = 0;
    for grapheme in UnicodeSegmentation::graphemes(text, true) {
        if remaining <= width {
            break;
        }
        remaining = remaining.saturating_sub(UnicodeWidthStr::width(grapheme));
        steps += 1;
    }
    steps
}

/// 取出跑马灯指定起点的一帧文本：从 start 个字素开始，拼到不超过 width 为止。
fn marquee_window(text: &str, start: usize, width: usize) -> String {
    let mut result = String::new();
    let mut used = 0;

    for grapheme in UnicodeSegmentation::graphemes(text, true).skip(start) {
        let grapheme_width = UnicodeWidthStr::width(grapheme);
        if used + grapheme_width > width {
            break;
        }
        result.push_str(grapheme);
        used += grapheme_width;
    }
    result
}

/// 按显示宽度截断文本，末尾补"…"；不劈开任何字素（中文/emoji 安全）。
fn truncate_width(text: &str, max_width: usize) -> String {
    if UnicodeWidthStr::width(text) <= max_width {
        return text.to_owned();
    }
    let ellipsis = "…";
    let ellipsis_width = UnicodeWidthStr::width(ellipsis);

    let limit = max_width.saturating_sub(ellipsis_width);
    let mut result = String::new();
    let mut used = 0;
    for grapheme in UnicodeSegmentation::graphemes(text, true) {
        let grapheme_width = UnicodeWidthStr::width(grapheme);
        if used + grapheme_width > limit {
            break;
        }
        result.push_str(grapheme);
        used += grapheme_width;
    }
    if max_width >= ellipsis_width {
        result.push_str(ellipsis);
    }
    result
}

// ── 播放器 ───────────────────────────────────────────────
/// 进度条配色（深/浅主题）。
fn gauge_color_style(is_dark: bool) -> Style {
    if is_dark {
        Style::default()
            .fg(Color::Gray)
            .add_modifier(Modifier::BOLD)
            .bg(Color::DarkGray)
    } else {
        Style::default()
            .fg(Color::DarkGray)
            .add_modifier(Modifier::BOLD)
            .bg(Color::Gray)
    }
}

/// 渲染播放器面板：标题行（左侧"状态 歌名 — 歌手"，右侧"模式 · 音量"）+ 居中的进度条。
fn render_player(frame: &mut Frame<'_>, app: &App, area: Rect) {
    let track = app.current_track();

    // 进度不超过时长（防浮点越界）。
    let position = app.playback.position().min(track.duration);

    let ratio = if track.duration.is_zero() {
        0.0
    } else {
        position.as_secs_f64() / track.duration.as_secs_f64()
    };

    let state = if !app.playback.is_available() {
        "静音浏览"
    } else if app.playback.is_paused() {
        "暂停"
    } else {
        "播放"
    };
    let volume = if app.playback.muted() {
        "已静音".to_owned()
    } else {
        format!("音量 {}%", (app.playback.volume() * 100.0).round())
    };
    let info = format!("{} · {}", app.mode.label(), volume);

    // 左侧标题按剩余宽度截断（保留 info 的宽度 + 2 个空格给右边标题）。
    let info_width = UnicodeWidthStr::width(info.as_str()) + 2;

    let left_budget = area
        .width
        .saturating_sub(2)
        .saturating_sub(info_width as u16)
        .max(4) as usize;
    let song = truncate_width(
        &format!("{state} {} — {}", track.title, track.display_artist()),
        left_budget,
    );
    let block = Block::default()
        .borders(Borders::ALL)
        .border_style(
            Style::default()
                .fg(PROGRESS_BORDER)
                .add_modifier(Modifier::BOLD),
        )
        // 标题行：左侧歌名状态、右侧模式音量，两端对齐。
        .title(Line::from(format!(" {song} ")).left_aligned())
        .title(Line::from(format!(" {info} ")).right_aligned());
    let inner = block.inner(area);
    frame.render_widget(block, area);
    let progress_area = centered_progress_area(inner);
    frame.render_widget(
        Gauge::default()
            .ratio(ratio.clamp(0.0, 1.0))
            .gauge_style(gauge_color_style(app.is_dark))
            .label(format!(
                "{} / {}",
                format_duration(position),
                format_duration(track.duration)
            )),
        progress_area,
    );
}

/// 把进度条区域锁定到面板垂直居中的一行（高度 1，宽度占满）。
fn centered_progress_area(area: Rect) -> Rect {
    Rect::new(
        area.x,
        area.y + area.height.saturating_sub(1) / 2,
        area.width,
        area.height.min(1),
    )
}

// ── 状态栏 ───────────────────────────────────────────────
/// 渲染底部状态栏：显示临时状态（3 秒内刷新），过期后回退到"?"帮助提示。
/// delete_confirm 状态下强制一直显示当前状态，不因超时消失（方便看删除确认文案）。
fn render_status_line(frame: &mut Frame<'_>, app: &App, area: Rect) {
    const STATUS_TIMEOUT: Duration = Duration::from_secs(3);
    let fresh = app
        .status_started
        .is_some_and(|started| started.elapsed() < STATUS_TIMEOUT);

    let text = match app.status.as_deref() {
        Some(status) if !status.is_empty() && (app.delete_confirm || fresh) => status.to_owned(),
        _ => "? 帮助".to_owned(),
    };
    frame.render_widget(
        Paragraph::new(text)
            .style(Style::default().fg(MUTED))
            .alignment(Alignment::Center),
        area,
    );
}

// ── 帮助弹窗 ─────────────────────────────────────────────
/// 渲染全屏帮助：先 Clear 清掉主界面残影，再画一个居中的快捷键弹窗。
fn render_help(frame: &mut Frame<'_>, area: Rect) {
    let popup = help_popup(area);
    let header = |text| {
        Line::from(Span::styled(
            text,
            Style::default().fg(ACTIVE).add_modifier(Modifier::BOLD),
        ))
    };
    let lines = vec![
        header("播放"),
        Line::raw("  Space  播放/暂停       N / P  下一首 / 上一首"),
        Line::raw("  ← / →  跳转 5 秒      Shift+← / →  跳转 30 秒"),
        Line::raw("  + / -  调整音量       M  静音      R  播放模式"),
        Line::raw(""),
        header("队列与歌词"),
        Line::raw("  ↑ / ↓  选择歌曲       Enter  播放选中歌曲"),
        Line::raw("  Delete  删除歌曲      L  聚焦并滚动歌词"),
        Line::raw(""),
        header("其他"),
        Line::raw("  /  搜索和下载         Q  保存状态并退出"),
        Line::raw("  Tab  切换搜索源"),
        Line::raw("  ? 或 Esc  关闭帮助"),
    ];

    frame.render_widget(Clear, area);
    frame.render_widget(
        Paragraph::new(lines).block(
            Block::default()
                .borders(Borders::ALL)
                .border_style(Style::default().fg(ACCENT).add_modifier(Modifier::BOLD))
                .title(Line::raw(" Harp 快捷键 ").alignment(Alignment::Center)),
        ),
        popup,
    );
}

/// 计算帮助弹窗的区域：在给定区域内居中，宽度/高度各不小于 64×16。
/// 取水平、垂直 margin 里较小的那个，保证弹窗不会有一边越界。
fn help_popup(area: Rect) -> Rect {
    const MIN_WIDTH: u16 = 64;
    const MIN_HEIGHT: u16 = 16;

    let horizontal_margin = area.width.saturating_sub(MIN_WIDTH) / 2;
    let vertical_margin = area.height.saturating_sub(MIN_HEIGHT) / 2;
    let margin = horizontal_margin.min(vertical_margin);
    Rect::new(
        area.x + margin,
        area.y + margin,
        area.width.saturating_sub(margin * 2),
        area.height.saturating_sub(margin * 2),
    )
}

/// 把时长格式化成分:秒（如 125 秒 → "02:05"）。
fn format_duration(duration: Duration) -> String {
    let seconds = duration.as_secs();
    format!("{:02}:{:02}", seconds / 60, seconds % 60)
}

// ── 单元测试 ─────────────────────────────────────────────
#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn formats_duration() {
        assert_eq!(format_duration(Duration::from_secs(125)), "02:05");
    }

    #[test]
    fn progress_bar_is_one_centered_row() {
        assert_eq!(
            centered_progress_area(Rect::new(3, 5, 40, 3)),
            Rect::new(3, 6, 40, 1)
        );
    }

    #[test]
    fn marquee_reveals_the_complete_suffix() {
        // 跑马灯总步数应能让最后一帧露出完整后缀（不劈字素）。
        let title = "AB中文CD";
        let steps = marquee_steps(title, 6);
        assert_eq!(steps, 2);
        assert_eq!(marquee_window(title, 0, 6), "AB中文");
        assert_eq!(marquee_window(title, steps, 6), "中文CD");
    }

    #[test]
    fn truncate_adds_ellipsis_without_breaking_graphemes() {
        // 截断时中文按整字保留，加 "…"。
        assert_eq!(truncate_width("短标题", 10), "短标题");
        assert_eq!(truncate_width("很长的标题需要截断", 8), "很长的…");
        assert_eq!(truncate_width("abc", 2), "a…");
        assert_eq!(truncate_width("", 2), "");
    }

    #[test]
    fn search_result_selection_keeps_line_width() {
        // 选中/未选中行宽必须一致，否则切换选中时整列会抖。
        let selected = search_result_line(0, true, "爱错 (Live)", "张碧晨", Some("FLAC"));
        let unselected = search_result_line(0, false, "爱错 (Live)", "张碧晨", Some("FLAC"));
        assert_eq!(
            UnicodeWidthStr::width(selected.as_str()),
            UnicodeWidthStr::width(unselected.as_str())
        );
    }

    #[test]
    fn cover_frame_is_fixed_size_across_window_heights() {
        // 封面高度与窗口高度无关：从 MIN_WINDOW_HEIGHT 到 60 行，封面框高度恒定。
        let font = FontSize::new(10, 20);
        let dims = Some((600, 600));
        let rects: Vec<Rect> = (MIN_WINDOW_HEIGHT..=60)
            .map(|h| {
                let area = Rect::new(0, 0, 31, h);
                let (cover, _) = stacked_left_areas(area, dims, font);
                cover
            })
            .collect();
        assert!(rects.iter().all(|r| r.height == rects[0].height));
        assert_eq!(rects[0].height, 16);
    }

    #[test]
    fn cover_frame_height_is_square() {
        // 封面框恒为正方形：正方形图、竖图、横图算出同样高度（留白由 Fit 处理）。
        let font = FontSize::new(10, 20);
        let square = cover_frame_height(32, Some((600, 600)), font);
        let portrait = cover_frame_height(32, Some((600, 800)), font);
        let landscape = cover_frame_height(32, Some((800, 600)), font);
        assert_eq!(portrait, square);
        assert_eq!(square, landscape);
    }

    #[test]
    fn queue_is_stacked_below_top_aligned_cover() {
        // 队列在封面正下方、顶部对齐、宽度一致、间隔 PANEL_GAP，且至少留出 3 行。
        let area = Rect::new(0, 0, 36, 24);
        let (cover, queue) = stacked_left_areas(area, Some((600, 600)), FontSize::new(10, 20));
        assert_eq!(cover.y, area.y);
        assert_eq!(queue.y - cover.bottom(), PANEL_GAP);
        assert_eq!(queue.bottom(), area.bottom());
        assert_eq!(cover.width, queue.width);
        assert!(queue.height >= 3);
    }

    #[test]
    fn main_panels_have_uniform_gaps() {
        // 主面板之间的间距与高度对齐都应符合约定。
        let area = Rect::new(0, 0, 80, 32);
        let (left, lyrics, player, status) = gapped_main_areas(area);
        assert_eq!(lyrics.x - left.right(), PANEL_GAP_H);
        assert_eq!(player.y - left.bottom(), PANEL_GAP);
        assert_eq!(lyrics.bottom(), left.bottom());
        assert_eq!(status.y - player.bottom(), PANEL_GAP);
        assert_eq!(status.height, 1);
        assert_eq!(status.bottom(), area.bottom());
    }

    #[test]
    fn help_popup_has_equal_outer_margins() {
        // 帮助弹窗四周留白等距（正居中）。
        let area = Rect::new(2, 3, 80, 24);
        let popup = help_popup(area);
        let margin = popup.x - area.x;
        assert_eq!(popup.y - area.y, margin);
        assert_eq!(area.right() - popup.right(), margin);
        assert_eq!(area.bottom() - popup.bottom(), margin);
    }
}
