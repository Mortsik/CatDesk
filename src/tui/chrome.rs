use ratatui::prelude::*;
use ratatui::widgets::{Block, Borders, Paragraph};

use crate::build_info;
use crate::state::{ToolMode, UiLanguage};
use crate::theme;
use crate::tui::text::terminal_cell_width;

pub(crate) fn draw_tui_header(f: &mut Frame, area: Rect, palette: &theme::Palette, title: &str) {
    let block = Block::default()
        .borders(Borders::ALL)
        .border_type(palette.border_type)
        .border_style(Style::default().fg(palette.border_fg));
    let inner = block.inner(area);
    f.render_widget(block, area);

    let version = format!(
        "{} ",
        build_info::version_label(build_info::VERSION, build_info::GIT_SHA)
    );
    let version_width = terminal_cell_width(&version) as u16;
    let columns = Layout::default()
        .direction(Direction::Horizontal)
        .constraints([Constraint::Min(0), Constraint::Length(version_width)])
        .split(inner);
    let style = Style::default()
        .fg(palette.header_fg)
        .add_modifier(Modifier::BOLD);
    f.render_widget(
        Paragraph::new(format!("  {title}")).style(style),
        columns[0],
    );
    f.render_widget(
        Paragraph::new(version)
            .style(style)
            .alignment(Alignment::Right),
        columns[1],
    );
}

pub(crate) fn draw_mode_select(
    f: &mut Frame,
    theme: &theme::ThemeDef,
    tool_mode: ToolMode,
    ui_language: UiLanguage,
) {
    let palette = theme.palette;
    let area = f.area();
    let zh = ui_language.is_traditional_chinese();

    let chunks = Layout::default()
        .direction(Direction::Vertical)
        .constraints([
            Constraint::Length(3),  // Header
            Constraint::Length(17), // Mode selection
            Constraint::Min(0),     // Spacer
        ])
        .split(area);

    draw_tui_header(
        f,
        chunks[0],
        &palette,
        if zh {
            "CatDesk - 讓 ChatGPT Web 成為程式開發代理 =w="
        } else {
            "CatDesk - Turns ChatGPT Web into a coding agent =w="
        },
    );

    let settings_detail = if zh {
        format!(
            " (主題 {}, 工具模式 {})",
            theme.label_for(true),
            tool_mode.label_for(ui_language)
        )
    } else {
        format!(" (theme {}, tool mode {})", theme.label, tool_mode.label())
    };
    let language_hint = if zh {
        " (切換至 English)"
    } else {
        " (switch to 繁體中文)"
    };

    let lines = vec![
        Line::from(""),
        Line::from(Span::styled(
            if zh {
                "  選擇模式"
            } else {
                "  Select mode"
            },
            Style::default()
                .fg(palette.title_fg)
                .add_modifier(Modifier::BOLD),
        )),
        Line::from(""),
        Line::from(vec![
            Span::styled(
                "  [1] ",
                Style::default()
                    .fg(palette.key_fg)
                    .add_modifier(Modifier::BOLD),
            ),
            Span::styled(
                if zh {
                    "控制電腦   "
                } else {
                    "Control Computer   "
                },
                Style::default().fg(palette.primary_fg),
            ),
            Span::styled(
                if zh {
                    "(本機工具)"
                } else {
                    "(local tools)"
                },
                Style::default().fg(palette.muted_fg),
            ),
        ]),
        Line::from(vec![
            Span::styled(
                "  [2] ",
                Style::default()
                    .fg(palette.key_fg)
                    .add_modifier(Modifier::BOLD),
            ),
            Span::styled(
                if zh {
                    "控制瀏覽器   "
                } else {
                    "Control Browser    "
                },
                Style::default().fg(palette.primary_fg),
            ),
            Span::styled(
                "(chrome-devtools-mcp)",
                Style::default().fg(palette.muted_fg),
            ),
        ]),
        Line::from(vec![
            Span::styled(
                "  [3] ",
                Style::default()
                    .fg(palette.key_fg)
                    .add_modifier(Modifier::BOLD),
            ),
            Span::styled(
                if zh { "兩者皆用" } else { "Both" },
                Style::default().fg(palette.primary_fg),
            ),
        ]),
        Line::from(vec![
            Span::styled(
                "  [l] ",
                Style::default()
                    .fg(palette.key_fg)
                    .add_modifier(Modifier::BOLD),
            ),
            Span::styled(
                if zh { "語言：" } else { "Language: " },
                Style::default().fg(palette.primary_fg),
            ),
            Span::styled(
                ui_language.label(),
                Style::default()
                    .fg(palette.secondary_fg)
                    .add_modifier(Modifier::BOLD),
            ),
            Span::styled(language_hint, Style::default().fg(palette.muted_fg)),
        ]),
        Line::from(vec![
            Span::styled(
                "  [s] ",
                Style::default()
                    .fg(palette.key_fg)
                    .add_modifier(Modifier::BOLD),
            ),
            Span::styled(
                if zh { "設定" } else { "Settings" },
                Style::default().fg(palette.primary_fg),
            ),
            Span::styled(settings_detail, Style::default().fg(palette.muted_fg)),
        ]),
        Line::from(""),
        Line::from(vec![
            Span::styled("  [q] ", Style::default().fg(palette.danger_fg)),
            Span::styled(
                if zh { "離開" } else { "Quit" },
                Style::default().fg(palette.muted_fg),
            ),
        ]),
    ];

    let select = Paragraph::new(lines).block(
        Block::default()
            .title(if zh { " 模式 " } else { " Mode " })
            .borders(Borders::ALL)
            .border_type(palette.border_type)
            .border_style(Style::default().fg(palette.border_fg)),
    );
    f.render_widget(select, chunks[1]);
}

pub(crate) fn render_toast(f: &mut Frame, palette: theme::Palette, msg: &str, pos: (u16, u16)) {
    let area = f.area();
    let (col, row) = pos;
    let label = format!(" {msg} ");
    let w = u16::try_from(terminal_cell_width(&label))
        .unwrap_or(u16::MAX)
        .min(area.width);
    let x = col.saturating_add(1).min(area.width.saturating_sub(w));
    let y = if row > 0 { row - 1 } else { row + 1 }.min(area.height.saturating_sub(1));
    let toast_area = Rect::new(x, y, w, 1);
    let toast_widget = Paragraph::new(label).style(
        Style::default()
            .bg(palette.toast_bg)
            .fg(palette.toast_fg)
            .add_modifier(Modifier::BOLD),
    );
    f.render_widget(toast_widget, toast_area);
}

/// Cross-module serialization for tests that collide on process-global state.
///
/// The test binary runs every module's tests in parallel threads of one
/// process, so two categories can race each other even though neither can
/// race within its own module:
///
/// * env mutators (linux_sandbox) temporarily rewrite `PATH`/`HOME` of the
///   whole test process while they hold their guard;
/// * child-process spawners (handoff git tests) resolve binaries such as
///   `git` through `PATH` at spawn time.
///
/// Interleaving the two breaks spawns with ENOENT mid-test, which surfaces as
/// `git.available == false` or a panic on a git setup `expect`. One shared
/// mutex serializes both sides; see `bd catdesk-cqq`.

pub(crate) fn centered_rect(percent_x: u16, height: u16, area: Rect) -> Rect {
    let width = area
        .width
        .saturating_mul(percent_x)
        .saturating_div(100)
        .max(44);
    let width = width.min(area.width.saturating_sub(2).max(1));
    let popup_height = height.min(area.height.saturating_sub(2).max(1));
    let x = area.x + area.width.saturating_sub(width) / 2;
    let y = area.y + area.height.saturating_sub(popup_height) / 2;
    Rect::new(x, y, width, popup_height)
}

