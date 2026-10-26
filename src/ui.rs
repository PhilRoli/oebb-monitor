//! All terminal rendering.
//!
//! [`ui`] dispatches to one of three screen renderers based on the current
//! [`AppMode`], with the key help optionally drawn on top. Rendering is a pure function of [`App`] state; nothing here
//! mutates application data beyond scroll state (the board, station list, and
//! detail view offsets).
//! All user-facing text comes from the active language's [`crate::lang::Tr`].

use chrono::Local;
use ratatui::{
    layout::{Alignment, Constraint, Direction, Layout, Rect},
    style::{Color, Modifier, Style},
    text::{Line, Span},
    widgets::{Block, Borders, Cell, Clear, List, ListItem, Paragraph, Row, Table, Wrap},
    Frame,
};

use crate::app::{
    calculate_delay, format_time, App, AppMode, ConnectionState, ContentType, LinkError,
};
use crate::lang::Tr;

/// Map a delay (minutes) to its colour: green on time, yellow up to 5 min late,
/// red beyond that.
fn delay_color(delay: i64) -> Color {
    if delay > 5 {
        Color::Red
    } else if delay > 0 {
        Color::Yellow
    } else {
        Color::Green
    }
}

/// Render the current frame, dispatching on the active mode.
pub fn ui(f: &mut Frame, app: &mut App) {
    match app.mode {
        AppMode::StationSelect => render_station_select(f, app),
        AppMode::TrainDetail => render_train_detail(f, app),
        AppMode::Normal | AppMode::Filter => render_main(f, app),
    }
    if app.show_help {
        render_help(f, app);
    }
}

/// Human-readable, translated description of a connection problem. Reasons
/// from the network stack are untranslated and truncated to stay on one line.
fn describe_error(tr: &Tr, err: &LinkError) -> String {
    let with_reason = |label: &str, reason: &str| {
        let reason: String = reason.chars().take(50).collect();
        if reason.is_empty() {
            label.to_string()
        } else {
            format!("{label} ({reason})")
        }
    };
    match err {
        LinkError::Connect(r) => with_reason(tr.err_connect, r),
        LinkError::Closed(r) => with_reason(tr.err_closed, r),
        LinkError::Timeout => tr.err_timeout.to_string(),
        LinkError::Parse => tr.err_parse.to_string(),
    }
}

/// A board column. Narrow terminals drop the less important ones first.
#[derive(Clone, Copy, PartialEq, Debug)]
enum Col {
    Num,
    Time,
    Actual,
    Delay,
    Train,
    Line,
    /// Destination on the departure board, origin on the arrival board.
    Place,
    Track,
    Sector,
    Remarks,
}

impl Col {
    const ALL: [Col; 10] = [
        Col::Num,
        Col::Time,
        Col::Actual,
        Col::Delay,
        Col::Train,
        Col::Line,
        Col::Place,
        Col::Track,
        Col::Sector,
        Col::Remarks,
    ];

    /// Dropped in this order until the rest fit; Time, Delay, Train, Place
    /// and Track always stay.
    const DROP_ORDER: [Col; 5] = [Col::Sector, Col::Actual, Col::Num, Col::Line, Col::Remarks];

    /// The narrowest this column may get.
    fn min_width(self) -> u16 {
        match self {
            Col::Num => 3,
            Col::Time | Col::Actual => 6,
            Col::Delay | Col::Train | Col::Line | Col::Sector => 8,
            Col::Track => 7,
            Col::Place | Col::Remarks => 15,
        }
    }
}

/// Widest the destination/origin column grows to before the rest goes to
/// the remarks.
const PLACE_MAX_WIDTH: u16 = 25;

/// Total width of `cols` at their minimum, including the 1-cell spacing.
fn min_table_width(cols: &[Col]) -> u16 {
    cols.iter().map(|c| c.min_width()).sum::<u16>() + cols.len().saturating_sub(1) as u16
}

/// The columns that fit in `width`, in display order.
fn board_columns(width: u16) -> Vec<Col> {
    let mut cols = Col::ALL.to_vec();
    for drop in Col::DROP_ORDER {
        if min_table_width(&cols) <= width {
            break;
        }
        cols.retain(|c| *c != drop);
    }
    cols
}

/// Whether the user asked for no colour (<https://no-color.org>). crossterm
/// already strips the colours; this is for styles that must then switch to
/// another way of standing out.
fn no_color() -> bool {
    std::env::var_os("NO_COLOR").is_some_and(|v| !v.is_empty())
}

/// Background of the selected row or list entry; reverse video without colour.
fn selection_style() -> Style {
    if no_color() {
        Style::default().add_modifier(Modifier::REVERSED)
    } else {
        Style::default().bg(Color::DarkGray)
    }
}

/// Style of a track number: reversed (so it reads even without colour) when
/// the train was moved to a different track than planned.
fn track_style(changed: bool) -> Style {
    let style = Style::default().fg(Color::Magenta);
    if changed {
        style.add_modifier(Modifier::REVERSED | Modifier::BOLD)
    } else {
        style
    }
}

/// How long ago the last update arrived, e.g. "vor 12 s" / "3 min ago".
fn format_age(tr: &Tr, secs: i64) -> String {
    let amount = match secs.max(0) {
        s if s < 60 => format!("{s} s"),
        s => format!("{} {}", s / 60, tr.min),
    };
    format!("{}{}{}", tr.ago_prefix, amount, tr.ago_suffix)
}

/// The feed pushes an update about every 30 s, so after three missed ones the
/// age turns yellow, and red beyond twice that.
const STALE_AFTER_SECS: i64 = 90;

/// Colour for an update age: fresh, getting stale, or stale.
fn age_color(secs: i64) -> Color {
    if secs > 2 * STALE_AFTER_SECS {
        Color::Red
    } else if secs > STALE_AFTER_SECS {
        Color::Yellow
    } else {
        Color::White
    }
}

/// The main board: title with clock, the departures/arrivals table, station
/// notices (only when there are any), and a status line with connection state
/// and key hints.
fn render_main(f: &mut Frame, app: &mut App) {
    let tr = app.lang.tr();
    let area = f.area();

    // Notices get as many lines as they need, up to a third of the screen,
    // and no space at all when there are none.
    let notices_text: Vec<Line> = app
        .special_notices
        .iter()
        .flat_map(|notice| {
            // Notice text carries embedded newlines; ratatui does not break a
            // Line on them, so split here and indent the continuation lines.
            notice
                .text
                .default
                .split('\n')
                .map(str::trim)
                .filter(|l| !l.is_empty())
                .enumerate()
                .map(|(i, l)| Line::from(format!("{} {}", if i == 0 { "•" } else { " " }, l)))
                .collect::<Vec<_>>()
        })
        .collect();
    // No trimming, so the continuation lines keep their indent.
    let notices = Paragraph::new(notices_text)
        .style(Style::default().fg(Color::Yellow))
        .wrap(Wrap { trim: false });
    let notices_height = if app.special_notices.is_empty() {
        0
    } else {
        let lines = notices.line_count(area.width.saturating_sub(2)) as u16;
        lines.saturating_add(2).min((area.height / 3).max(3))
    };

    let chunks = Layout::default()
        .direction(Direction::Vertical)
        .constraints([
            Constraint::Length(3),
            Constraint::Min(5),
            Constraint::Length(notices_height),
            Constraint::Length(3),
        ])
        .split(area);

    render_title(f, app, chunks[0]);
    render_board(f, app, chunks[1]);

    if notices_height > 0 {
        let title = if app.special_notices.len() > 1 {
            format!("{} ({})", tr.block_notices, app.special_notices.len())
        } else {
            tr.block_notices.to_string()
        };
        let notices = notices.block(Block::default().borders(Borders::ALL).title(title));
        f.render_widget(notices, chunks[2]);
    }

    render_status(f, app, chunks[3]);
}

/// Board name and station, centred, with the current time on the right like
/// the clock on a station display.
fn render_title(f: &mut Frame, app: &App, area: Rect) {
    let tr = app.lang.tr();
    let board = match app.content_type {
        ContentType::Departure => tr.departures,
        ContentType::Arrival => tr.arrivals,
    };
    let title = Line::from(format!("🚂 {} - {}", board, app.station_name));
    let title_width = title.width() as u16;

    let block = Block::default().borders(Borders::ALL);
    let inner = block.inner(area);
    f.render_widget(block, area);
    f.render_widget(
        Paragraph::new(title)
            .style(
                Style::default()
                    .fg(Color::Cyan)
                    .add_modifier(Modifier::BOLD),
            )
            .alignment(Alignment::Center),
        inner,
    );

    // Only where it can't collide with the centred title.
    let clock = Local::now().format("%H:%M").to_string();
    let clock_width = clock.len() as u16 + 1;
    if inner.width >= title_width + 2 * clock_width {
        f.render_widget(
            Paragraph::new(clock)
                .style(Style::default().add_modifier(Modifier::BOLD))
                .alignment(Alignment::Right),
            inner,
        );
    }
}

/// The departures/arrivals table, filtered and scrolled to the selection.
fn render_board(f: &mut Frame, app: &mut App, area: Rect) {
    let tr = app.lang.tr();
    let editing = app.mode == AppMode::Filter;

    let mut block_title = tr.block_trains.to_string();
    if editing || !app.filter.is_empty() {
        block_title = format!(
            "{} · {}: {}{} ({}/{})",
            block_title,
            tr.filter_label,
            app.filter,
            if editing { "_" } else { "" },
            app.visible_items().len(),
            app.items.len()
        );
    }
    let block = Block::default().borders(Borders::ALL).title(block_title);
    let cols = board_columns(block.inner(area).width);

    let header = Row::new(cols.iter().map(|col| {
        let text = match col {
            Col::Num => "#",
            Col::Time => tr.col_time,
            Col::Actual => tr.col_actual,
            Col::Delay => tr.col_delay,
            Col::Train => tr.col_train,
            Col::Line => tr.col_line,
            Col::Place => match app.content_type {
                ContentType::Departure => tr.col_dest,
                ContentType::Arrival => tr.col_from,
            },
            Col::Track => tr.col_track,
            Col::Sector => tr.col_sector,
            Col::Remarks => tr.col_remarks,
        };
        Cell::from(text).style(
            Style::default()
                .fg(Color::Yellow)
                .add_modifier(Modifier::BOLD),
        )
    }))
    .height(1)
    .bottom_margin(1);

    let rows: Vec<Row> = app
        .visible_items()
        .into_iter()
        .enumerate()
        .map(|(idx, item)| {
            let is_selected = app.selected_train_index == Some(idx);
            let delay = calculate_delay(item);
            let cells = cols.iter().map(|col| match col {
                Col::Num => {
                    let number = match idx {
                        0..=8 => format!("{}", idx + 1),
                        9 => "0".to_string(),
                        _ => " ".to_string(),
                    };
                    Cell::from(number).style(if is_selected {
                        Style::default()
                            .fg(Color::Green)
                            .add_modifier(Modifier::BOLD)
                    } else {
                        Style::default().fg(Color::DarkGray)
                    })
                }
                Col::Time => Cell::from(format_time(&item.scheduled)),
                Col::Actual => Cell::from(match (delay, &item.expected) {
                    (Some(_), Some(expected)) => format_time(expected),
                    _ => "-".to_string(),
                }),
                Col::Delay => match delay {
                    Some(d) if d > 0 => {
                        Cell::from(format!("+{d}")).style(Style::default().fg(delay_color(d)))
                    }
                    Some(d) => Cell::from(d.to_string()).style(Style::default().fg(delay_color(d))),
                    None => Cell::from("-").style(Style::default().fg(Color::Gray)),
                },
                Col::Train => Cell::from(item.train.clone()),
                Col::Line => Cell::from(
                    item.line
                        .clone()
                        .or_else(|| item.product.clone())
                        .unwrap_or_default(),
                )
                .style(Style::default().fg(Color::Cyan)),
                Col::Place => Cell::from(
                    match app.content_type {
                        ContentType::Departure => item.destination.as_ref(),
                        ContentType::Arrival => item.origin.as_ref(),
                    }
                    .map(|d| d.default.clone())
                    .unwrap_or_else(|| "N/A".to_string()),
                ),
                Col::Track => Cell::from(item.track.clone().unwrap_or_else(|| "-".to_string()))
                    .style(track_style(item.track_changed())),
                Col::Sector => Cell::from(item.sector.clone().unwrap_or_else(|| "-".to_string())),
                Col::Remarks => Cell::from(
                    item.remarks
                        .as_ref()
                        .and_then(|r| r.first())
                        // Remark text can contain embedded newlines; flatten
                        // to a space so it stays on the single-height row.
                        .map(|r| r.text.default.replace('\n', " "))
                        .unwrap_or_default(),
                ),
            });
            let row = Row::new(cells);
            if is_selected {
                row.style(selection_style())
            } else {
                row
            }
        })
        .collect();

    // The destination grows up to its maximum, then the remarks take the rest.
    let spare = block
        .inner(area)
        .width
        .saturating_sub(min_table_width(&cols));
    let widths = cols.iter().map(|col| match col {
        Col::Place if cols.contains(&Col::Remarks) => {
            Constraint::Length((col.min_width() + spare).min(PLACE_MAX_WIDTH))
        }
        Col::Place | Col::Remarks => Constraint::Min(col.min_width()),
        _ => Constraint::Length(col.min_width()),
    });

    let table = Table::new(rows, widths).header(header).block(block);

    // The table scrolls to keep the selection visible; with no selection it
    // returns to the top rather than staying wherever it last scrolled to.
    app.board_state.select(app.selected_train_index);
    if app.selected_train_index.is_none() {
        *app.board_state.offset_mut() = 0;
    }
    f.render_stateful_widget(table, area, &mut app.board_state);
}

/// Connection state and data age on the left; key hints on the right, as
/// many as fit.
fn render_status(f: &mut Frame, app: &App, area: Rect) {
    let tr = app.lang.tr();

    let (update_text, update_color) = match &app.connection {
        ConnectionState::Connected => match app.last_update {
            Some(t) => {
                let secs = (Local::now() - t).num_seconds();
                (format_age(tr, secs), age_color(secs))
            }
            None => (tr.connected.to_string(), Color::Green),
        },
        ConnectionState::Connecting => (tr.connecting.to_string(), Color::Yellow),
        ConnectionState::Failed => (tr.connection_failed.to_string(), Color::Red),
    };

    let mut status_spans = vec![
        Span::styled(tr.last_update, Style::default().fg(Color::Gray)),
        Span::styled(update_text, Style::default().fg(update_color)),
    ];
    if let Some(err) = &app.last_error {
        status_spans.push(Span::styled(
            format!("  {}{}", tr.error_label, describe_error(tr, err)),
            Style::default().fg(Color::Red),
        ));
    }
    let status_line = Line::from(status_spans);
    let status_width = status_line.width() as u16;

    let block = Block::default()
        .borders(Borders::ALL)
        .title(tr.block_status);
    let inner = block.inner(area);
    f.render_widget(Paragraph::new(status_line).block(block), area);

    // Pick the most complete hint line that fits beside the status text.
    let available = inner.width.saturating_sub(status_width + 2);
    let hints = key_hints(app)
        .into_iter()
        .find(|line| line.width() as u16 <= available);
    if let Some(hints) = hints {
        f.render_widget(Paragraph::new(hints).alignment(Alignment::Right), inner);
    }
}

/// Key hint lines for the status bar, most complete first.
fn key_hints(app: &App) -> Vec<Line<'static>> {
    let tr = app.lang.tr();
    let key = |s: &'static str| {
        Span::styled(
            s,
            Style::default()
                .fg(Color::Green)
                .add_modifier(Modifier::BOLD),
        )
    };
    let help = vec![key("[?]"), Span::raw(tr.hint_help)];

    if app.mode == AppMode::Filter {
        return vec![Line::from(Span::raw(tr.filter_help)), Line::from(help)];
    }

    let commands = vec![
        key("[A]"),
        Span::raw(format!("{} ", tr.hint_arr)),
        key("[D]"),
        Span::raw(format!("{} ", tr.hint_dep)),
        key("[S]"),
        Span::raw(format!("{} ", tr.hint_stn)),
        key("[/]"),
        Span::raw(format!("{} ", tr.hint_filter)),
        key("[R]"),
        Span::raw(format!("{} ", tr.hint_ref)),
        key("[L]"),
        Span::raw(format!("{} ", tr.hint_lang)),
        key("[?]"),
        Span::raw(format!("{} ", tr.hint_help)),
        Span::styled(
            "[Q]",
            Style::default().fg(Color::Red).add_modifier(Modifier::BOLD),
        ),
        Span::raw(tr.hint_quit),
    ];
    let mut full = vec![
        key("1-9,0"),
        Span::raw("/"),
        key("↑↓"),
        Span::raw("+"),
        key("Enter"),
        Span::raw(format!(" {} ", tr.detail)),
    ];
    full.extend(commands.clone());

    vec![Line::from(full), Line::from(commands), Line::from(help)]
}

/// The key reference, drawn over whatever screen is open.
fn render_help(f: &mut Frame, app: &App) {
    let tr = app.lang.tr();

    let key_width = tr
        .help
        .iter()
        .map(|(k, _)| k.chars().count())
        .max()
        .unwrap_or(0);
    let mut lines = Vec::new();
    for (keys, action) in tr.help {
        if keys.is_empty() {
            if !lines.is_empty() {
                lines.push(Line::from(""));
            }
            lines.push(Line::from(Span::styled(
                *action,
                Style::default()
                    .fg(Color::Cyan)
                    .add_modifier(Modifier::BOLD),
            )));
        } else {
            lines.push(Line::from(vec![
                Span::styled(
                    format!("  {keys:key_width$}  "),
                    Style::default()
                        .fg(Color::Green)
                        .add_modifier(Modifier::BOLD),
                ),
                Span::raw(*action),
            ]));
        }
    }
    lines.push(Line::from(""));
    lines.push(Line::from(Span::styled(
        tr.help_close,
        Style::default().fg(Color::Gray),
    )));

    // Just big enough for the content (plus border and padding), centred and
    // clipped to the screen.
    let screen = f.area();
    let width = (lines.iter().map(Line::width).max().unwrap_or(0) as u16 + 4).min(screen.width);
    let height = (lines.len() as u16 + 2).min(screen.height);
    let area = Rect {
        x: screen.x + (screen.width - width) / 2,
        y: screen.y + (screen.height - height) / 2,
        width,
        height,
    };

    f.render_widget(Clear, area);
    f.render_widget(
        Paragraph::new(lines)
            .wrap(Wrap { trim: false })
            .block(Block::default().borders(Borders::ALL).title(tr.help_title)),
        area,
    );
}

/// The centred station-picker popup: search field, filtered list, key hints.
fn render_station_select(f: &mut Frame, app: &mut App) {
    let tr = app.lang.tr();
    let area = f.area();
    let popup_area = centered_rect(60, 70, area);

    let chunks = Layout::default()
        .direction(Direction::Vertical)
        .constraints([
            Constraint::Length(3),
            Constraint::Min(10),
            Constraint::Length(3),
        ])
        .split(popup_area);

    let block = Block::default()
        .title(tr.select_station)
        .borders(Borders::ALL);
    f.render_widget(block, popup_area);

    let search_text = format!("{}{}_", tr.search, app.station_search);
    let search = Paragraph::new(search_text)
        .style(Style::default().fg(Color::Yellow))
        .block(Block::default().borders(Borders::ALL));
    f.render_widget(search, chunks[0]);

    // With an empty search the list opens with the recent stations, marked.
    let mark_recent = app.station_search.is_empty();
    let items: Vec<ListItem> = app
        .filtered_stations
        .iter()
        .map(|(id, name)| {
            let recent = mark_recent && app.recent_stations.contains(id);
            ListItem::new(format!("{} {}", if recent { "↻" } else { " " }, name))
        })
        .collect();

    let list_title = if app.total_filtered_count > app.filtered_stations.len() {
        format!(
            "{} ({} {} {} {})",
            tr.stations,
            app.filtered_stations.len(),
            tr.of,
            app.total_filtered_count,
            tr.shown
        )
    } else {
        format!("{} ({})", tr.stations, app.filtered_stations.len())
    };

    let list = List::new(items)
        .block(Block::default().borders(Borders::ALL).title(list_title))
        .highlight_style(selection_style().add_modifier(Modifier::BOLD))
        .highlight_symbol(">> ");

    f.render_stateful_widget(list, chunks[1], &mut app.station_list_state);

    let help = Paragraph::new(tr.station_help)
        .style(Style::default().fg(Color::Gray))
        .alignment(Alignment::Center)
        .block(Block::default().borders(Borders::ALL));
    f.render_widget(help, chunks[2]);
}

/// The centred detail popup for the selected train: timing, platform, operator,
/// remarks, intermediate stops, and physical formation with amenities.
fn render_train_detail(f: &mut Frame, app: &mut App) {
    let tr = app.lang.tr();
    let lang = app.lang;
    let area = f.area();
    let popup_area = centered_rect(80, 85, area);

    // Owned, so the scroll clamp below can update the app while the content
    // is still borrowed from the train.
    let train = app.selected_train().cloned();

    if let Some(train) = &train {
        let chunks = Layout::default()
            .direction(Direction::Vertical)
            .constraints([
                Constraint::Length(3),
                Constraint::Min(5),
                Constraint::Length(3),
            ])
            .split(popup_area);

        let block = Block::default().borders(Borders::ALL);
        f.render_widget(block, popup_area);

        let line = train
            .line
            .as_deref()
            .or(train.product.as_deref())
            .unwrap_or("?");
        // Where it goes on the departure board, where it comes from on the
        // arrival board.
        let (place, joiner) = match app.content_type {
            ContentType::Departure => (train.destination.as_ref(), "→"),
            ContentType::Arrival => (train.origin.as_ref(), tr.from_word),
        };
        let place = place.map(|d| d.default.as_str()).unwrap_or("N/A");
        let title_text = format!(
            "🚂 {} {} - {} {} {}",
            tr.train, train.train, line, joiner, place
        );
        let title = Paragraph::new(title_text)
            .style(
                Style::default()
                    .fg(Color::Cyan)
                    .add_modifier(Modifier::BOLD),
            )
            .alignment(Alignment::Center)
            .block(Block::default().borders(Borders::ALL));
        f.render_widget(title, chunks[0]);

        let mut content_lines = Vec::new();

        let sched_label = match app.content_type {
            ContentType::Departure => tr.departure,
            ContentType::Arrival => tr.arrival,
        };
        content_lines.push(Line::from(vec![
            Span::styled(
                format!("{}: ", sched_label),
                Style::default().fg(Color::Yellow),
            ),
            Span::raw(format_time(&train.scheduled)),
        ]));

        if let Some(delay) = calculate_delay(train) {
            let delay_text = if delay > 0 {
                format!("+{} {}", delay, tr.min)
            } else {
                format!("{} {}", delay, tr.min)
            };
            content_lines.push(Line::from(vec![
                Span::styled(
                    format!("{}: ", tr.delay),
                    Style::default().fg(Color::Yellow),
                ),
                Span::styled(delay_text, Style::default().fg(delay_color(delay))),
            ]));
        }

        if let Some(track) = &train.track {
            content_lines.push(Line::from(vec![
                Span::styled(
                    format!("{}: ", tr.track),
                    Style::default().fg(Color::Yellow),
                ),
                Span::styled(track.clone(), track_style(train.track_changed())),
                Span::raw(" "),
                Span::styled(
                    train
                        .sector
                        .as_ref()
                        .map(|s| format!("{} {}", tr.sector, s))
                        .unwrap_or_default(),
                    Style::default().fg(Color::Magenta),
                ),
                Span::styled(
                    if train.track_changed() {
                        format!(" {}", tr.track_changed)
                    } else {
                        String::new()
                    },
                    Style::default()
                        .fg(Color::Magenta)
                        .add_modifier(Modifier::BOLD),
                ),
            ]));
        }

        if let Some(operator) = &train.operator {
            content_lines.push(Line::from(vec![
                Span::styled(
                    format!("{}: ", tr.operator),
                    Style::default().fg(Color::Yellow),
                ),
                Span::raw(operator.clone()),
            ]));
        }

        if let Some(remarks) = &train.remarks {
            if !remarks.is_empty() {
                content_lines.push(Line::from(Span::styled(
                    format!("{}:", tr.remarks),
                    Style::default().fg(Color::Yellow),
                )));
                for remark in remarks {
                    // A single remark may carry embedded newlines (e.g.
                    // "Kurzzug\nSektor B einsteigen"); render each as its own
                    // line since ratatui does not split them within a Line.
                    for line in remark.text.default.split('\n') {
                        content_lines.push(Line::from(Span::styled(
                            line.to_string(),
                            Style::default().fg(Color::Red),
                        )));
                    }
                }
            }
        }

        content_lines.push(Line::from(""));

        // Prioritized stops render independently of the full via list, so they
        // still show when the detailed via string is missing.
        let major_stops = train.major_stops();
        if !major_stops.is_empty() {
            content_lines.push(Line::from(Span::styled(
                format!("{}: {}", tr.major_stops, major_stops.join(" → ")),
                Style::default()
                    .fg(Color::Yellow)
                    .add_modifier(Modifier::BOLD),
            )));
        }

        let stops = train.all_stops();
        if !stops.is_empty() {
            content_lines.push(Line::from(Span::styled(
                format!("{}:", tr.all_stops),
                Style::default()
                    .fg(Color::Cyan)
                    .add_modifier(Modifier::BOLD),
            )));
            content_lines.push(Line::from(format!("  {}", stops.join(" → "))));
        }

        content_lines.push(Line::from(""));

        if let Some(formation) = &train.formation {
            if !formation.is_empty() {
                content_lines.push(Line::from(Span::styled(
                    format!("{}:", tr.formation),
                    Style::default()
                        .fg(Color::Cyan)
                        .add_modifier(Modifier::BOLD),
                )));

                for wagon in formation {
                    let is_locomotive = wagon
                        .car_type
                        .as_deref()
                        .map(|t| t.iter().any(|c| c == "engine"))
                        .unwrap_or(false)
                        || wagon.wagon_number.is_none();
                    let mut wagon_line = if is_locomotive {
                        vec![Span::styled(
                            format!("  🚂 {}", tr.locomotive),
                            Style::default().fg(Color::Red).add_modifier(Modifier::BOLD),
                        )]
                    } else {
                        vec![
                            Span::raw(format!("  {} ", tr.coach)),
                            Span::styled(
                                wagon.wagon_number.as_deref().unwrap_or("?"),
                                Style::default().fg(Color::Yellow),
                            ),
                        ]
                    };

                    // Car type (sleeper / couchette / seated / restaurant) and
                    // passenger class (1./2. class, Business, Comfort) — most
                    // relevant for night and long-distance trains.
                    if !is_locomotive {
                        if let Some(label) = wagon
                            .car_type
                            .as_deref()
                            .and_then(|t| lang.car_type_label(t))
                        {
                            wagon_line.push(Span::styled(
                                format!(" {}", label),
                                Style::default().fg(Color::Blue),
                            ));
                        }
                        if let Some(class) =
                            wagon.symbol.as_deref().and_then(|s| lang.class_label(s))
                        {
                            wagon_line.push(Span::styled(
                                format!(" · {}", class),
                                Style::default().fg(Color::Green),
                            ));
                        }
                    }

                    if wagon.closed == Some(true) {
                        wagon_line.push(Span::styled(
                            format!(" {}", tr.closed),
                            Style::default().fg(Color::DarkGray),
                        ));
                    }

                    if let Some(sector) = &wagon.sector {
                        wagon_line.push(Span::styled(
                            format!(" [{} {}]", tr.sector, sector),
                            Style::default().fg(Color::Magenta),
                        ));
                    }
                    if let Some(dest) = &wagon.destination {
                        if !dest.is_empty() {
                            wagon_line.push(Span::styled(
                                format!(" → {}", dest),
                                Style::default().fg(Color::Cyan),
                            ));
                        }
                    }

                    if !is_locomotive {
                        wagon_line.push(Span::raw(": "));
                    }

                    if let Some(icons) = &wagon.icons {
                        let icon_strs: Vec<String> =
                            icons.iter().map(|icon| lang.icon_label(icon)).collect();
                        wagon_line.push(Span::styled(
                            icon_strs.join(" | "),
                            Style::default().fg(Color::Green),
                        ));
                    }

                    content_lines.push(Line::from(wagon_line));
                }
            }
        }

        let content_block = Block::default().borders(Borders::ALL).title(tr.details);
        let inner = content_block.inner(chunks[1]);
        let content = Paragraph::new(content_lines).wrap(Wrap { trim: true });

        // Clamp the scroll so the last line can't leave the bottom edge, and
        // so scrolling back up responds immediately after overshooting.
        let max_scroll = content
            .line_count(inner.width)
            .saturating_sub(inner.height as usize);
        app.detail_scroll = app
            .detail_scroll
            .min(max_scroll.try_into().unwrap_or(u16::MAX));

        let content = content.block(content_block).scroll((app.detail_scroll, 0));
        f.render_widget(content, chunks[1]);

        let help = Paragraph::new(tr.detail_help)
            .style(Style::default().fg(Color::Gray))
            .alignment(Alignment::Center)
            .block(Block::default().borders(Borders::ALL));
        f.render_widget(help, chunks[2]);
    } else {
        let msg = Paragraph::new(tr.no_train)
            .alignment(Alignment::Center)
            .block(Block::default().borders(Borders::ALL));
        f.render_widget(msg, popup_area);
    }
}

/// Compute a rectangle centred within `r`, sized as the given percentages of
/// its width and height.
fn centered_rect(percent_x: u16, percent_y: u16, r: Rect) -> Rect {
    let popup_layout = Layout::default()
        .direction(Direction::Vertical)
        .constraints([
            Constraint::Percentage((100 - percent_y) / 2),
            Constraint::Percentage(percent_y),
            Constraint::Percentage((100 - percent_y) / 2),
        ])
        .split(r);

    Layout::default()
        .direction(Direction::Horizontal)
        .constraints([
            Constraint::Percentage((100 - percent_x) / 2),
            Constraint::Percentage(percent_x),
            Constraint::Percentage((100 - percent_x) / 2),
        ])
        .split(popup_layout[1])[1]
}

#[cfg(test)]
mod tests {

    use super::*;

    #[test]
    fn delay_colors() {
        assert_eq!(delay_color(0), Color::Green);
        assert_eq!(delay_color(-3), Color::Green);
        assert_eq!(delay_color(3), Color::Yellow);
        assert_eq!(delay_color(5), Color::Yellow);
        assert_eq!(delay_color(6), Color::Red);
    }

    use crate::lang::Lang;
    use crate::model::WsMessage;
    use ratatui::{backend::TestBackend, Terminal};

    const FIXTURE: &str = include_str!("../tests/fixtures/update_departures.json");

    /// An app loaded from the departure fixture, with English strings.
    fn loaded_app() -> App {
        let data = serde_json::from_str::<WsMessage>(FIXTURE)
            .unwrap()
            .into_update()
            .unwrap()
            .unwrap()
            .data;
        let mut app = App::new();
        app.lang = Lang::En;
        app.items = data.departures.unwrap();
        app.special_notices = data.special_notices.unwrap();
        app.connection = ConnectionState::Connected;
        app
    }

    /// Render one frame and return the buffer as newline-joined text.
    fn render(app: &mut App, width: u16, height: u16) -> String {
        let mut terminal = Terminal::new(TestBackend::new(width, height)).unwrap();
        terminal.draw(|f| ui(f, app)).unwrap();
        let buf = terminal.backend().buffer().clone();
        (0..height)
            .map(|y| {
                (0..width)
                    .map(|x| buf[(x, y)].symbol().to_string())
                    .collect::<String>()
            })
            .collect::<Vec<_>>()
            .join("\n")
    }

    #[test]
    fn main_board_shows_station_trains_delay_and_notices() {
        let mut app = loaded_app();
        let out = render(&mut app, 140, 40);
        assert!(out.contains(app.lang.tr().departures), "{out}");
        assert!(out.contains(&app.station_name));
        assert!(out.contains("RJX 65"));
        assert!(out.contains("REX 1"));
        assert!(out.contains("Salzburg Hbf"));
        assert!(out.contains("+7"), "delay column");
        assert!(out.contains("Bauarbeiten"), "special notice");
    }

    #[test]
    fn main_board_arrivals_use_origin_column_and_title() {
        let mut app = loaded_app();
        app.content_type = ContentType::Arrival;
        app.items[0].origin = Some(crate::model::Destination {
            default: "Wien Hbf".into(),
        });
        let out = render(&mut app, 140, 40);
        assert!(out.contains(Lang::En.tr().arrivals));
        assert!(out.contains(Lang::En.tr().col_from));
        assert!(out.contains("Wien Hbf"));
    }

    #[test]
    fn german_strings_are_used_when_selected() {
        let mut app = loaded_app();
        app.lang = Lang::De;
        let out = render(&mut app, 140, 40);
        assert!(out.contains(Lang::De.tr().departures));
        assert!(!out.contains(Lang::En.tr().departures));
    }

    #[test]
    fn status_bar_reflects_connection_state() {
        let mut app = loaded_app();
        let tr = app.lang.tr();

        app.connection = ConnectionState::Connecting;
        assert!(render(&mut app, 140, 40).contains(tr.connecting));

        app.connection = ConnectionState::Failed;
        assert!(render(&mut app, 140, 40).contains(tr.connection_failed));

        app.connection = ConnectionState::Connected;
        app.last_update = None;
        assert!(render(&mut app, 140, 40).contains(tr.connected));

        app.last_update = Some(chrono::Local::now());
        let out = render(&mut app, 140, 40);
        assert!(out.contains(tr.last_update));
        assert!(!out.contains(tr.connecting));
    }

    #[test]
    fn empty_board_renders() {
        let mut app = App::new();
        app.lang = Lang::En;
        let out = render(&mut app, 120, 30);
        assert!(out.contains(&app.station_name));
    }

    #[test]
    fn train_detail_shows_fields_and_formation() {
        let mut app = loaded_app();
        app.mode = AppMode::TrainDetail;
        app.selected_train_index = Some(0);
        app.selected_train_id = Some(app.items[0].id.clone());
        let out = render(&mut app, 140, 60);
        assert!(out.contains("RJX 65"));
        assert!(out.contains("Salzburg Hbf"));
        assert!(out.contains("+7"));
        assert!(out.contains("ÖBB"), "operator");
        assert!(out.contains("Verspätung"), "remark");
        assert!(out.contains("1st class"), "decoded wagon class");
        assert!(
            out.contains("Major stops: St. Pölten Hbf → Linz/Donau Hbf"),
            "major stops split out of the feed's joined entry"
        );
        assert!(out.contains(app.lang.tr().detail_help));
    }

    #[test]
    fn train_detail_minimal_train_renders() {
        let mut app = loaded_app();
        app.mode = AppMode::TrainDetail;
        app.selected_train_index = Some(1);
        let out = render(&mut app, 140, 40);
        assert!(out.contains("REX 1"));
    }

    #[test]
    fn train_detail_without_selection_says_so() {
        let mut app = loaded_app();
        app.mode = AppMode::TrainDetail;
        app.selected_train_index = None;
        assert!(render(&mut app, 120, 40).contains(app.lang.tr().no_train));
    }

    #[test]
    fn train_detail_with_stale_index_does_not_panic() {
        let mut app = loaded_app();
        app.mode = AppMode::TrainDetail;
        app.selected_train_index = Some(99);
        assert!(render(&mut app, 120, 40).contains(app.lang.tr().no_train));
    }

    #[test]
    fn detail_scroll_past_end_does_not_panic() {
        let mut app = loaded_app();
        app.mode = AppMode::TrainDetail;
        app.selected_train_index = Some(0);
        app.detail_scroll = u16::MAX;
        let out = render(&mut app, 120, 40);
        // Clamped so the end of the content stays on screen, and scrolling
        // back up takes effect immediately.
        assert!(app.detail_scroll < 10, "{}", app.detail_scroll);
        assert!(out.contains("1st class"), "{out}");
    }

    #[test]
    fn station_select_shows_search_and_results() {
        let mut app = loaded_app();
        app.enter_station_select();
        for c in "salz".chars() {
            app.station_search.push(c);
        }
        app.update_filtered_stations();
        let out = render(&mut app, 120, 40);
        assert!(out.contains("Search: salz_"), "search prompt and text");
        let first = &app.filtered_stations[0].1;
        assert!(out.contains(first.as_str()));
    }

    #[test]
    fn station_select_with_no_matches_renders() {
        let mut app = loaded_app();
        app.enter_station_select();
        app.station_search = "zzzzzz".into();
        app.update_filtered_stations();
        render(&mut app, 120, 40);
    }

    #[test]
    fn every_mode_survives_tiny_and_odd_terminal_sizes() {
        for (w, h) in [(1, 1), (10, 3), (20, 5), (40, 10), (80, 24), (300, 100)] {
            for mode in [
                AppMode::Normal,
                AppMode::TrainDetail,
                AppMode::StationSelect,
            ] {
                for lang in [Lang::De, Lang::En] {
                    let mut app = loaded_app();
                    app.lang = lang;
                    app.mode = mode.clone();
                    app.selected_train_index = Some(0);
                    if mode == AppMode::StationSelect {
                        app.enter_station_select();
                    }
                    render(&mut app, w, h);
                }
            }
        }
    }

    #[test]
    fn many_trains_render_with_selection() {
        let mut app = loaded_app();
        let template = app.items[1].clone();
        app.items = (0..200)
            .map(|i| crate::model::TrainItem {
                id: format!("t{i}"),
                train: format!("Zug {i}"),
                ..template.clone()
            })
            .collect();
        app.selected_train_index = Some(150);
        let out = render(&mut app, 140, 30);
        assert!(out.contains("Zug 150"), "selected row scrolled into view");
        assert!(!out.contains("Zug 0 "), "top rows scrolled away");

        // Without a selection the board starts from the top again.
        app.selected_train_index = None;
        let out = render(&mut app, 140, 30);
        assert!(out.contains("Zug 0 "));
    }

    #[test]
    fn status_bar_shows_last_error_with_reason_in_both_languages() {
        let mut app = loaded_app();
        app.last_error = Some(LinkError::Connect("dns failure".into()));
        let out = render(&mut app, 160, 40);
        assert!(out.contains("Error: Cannot connect (dns failure)"), "{out}");

        app.lang = Lang::De;
        app.last_error = Some(LinkError::Timeout);
        let out = render(&mut app, 160, 40);
        assert!(out.contains("Fehler: Keine Daten empfangen"));

        app.last_error = Some(LinkError::Parse);
        assert!(render(&mut app, 160, 40).contains(Lang::De.tr().err_parse));
        app.last_error = None;
        assert!(!render(&mut app, 160, 40).contains(Lang::De.tr().error_label));
    }

    #[test]
    fn notices_panel_only_appears_with_notices_and_splits_lines() {
        let mut app = loaded_app();
        let title = app.lang.tr().block_notices;
        app.special_notices[0].text.default = "Bauarbeiten\nGrund: Gleisarbeiten".into();
        let out = render(&mut app, 140, 40);
        assert!(out.contains(title));
        assert!(out.contains("• Bauarbeiten"), "{out}");
        assert!(out.contains("  Grund: Gleisarbeiten"), "continuation line");

        app.special_notices.clear();
        assert!(!render(&mut app, 140, 40).contains(title));
    }

    #[test]
    fn notices_panel_is_capped_at_a_third_of_the_screen() {
        let mut app = loaded_app();
        let notice = app.special_notices[0].clone();
        app.special_notices = vec![notice; 30];
        let out = render(&mut app, 140, 30);
        assert!(out.contains("Notices (30)"));
        assert!(out.contains("RJX 65"), "the board keeps its rows");
    }

    #[test]
    fn changed_track_is_reversed_on_board_and_noted_in_detail() {
        assert!(track_style(true).add_modifier.contains(Modifier::REVERSED));
        assert!(!track_style(false).add_modifier.contains(Modifier::REVERSED));

        let mut app = loaded_app();
        app.items[0].flags = Some(vec!["TRACKCHANGE".into()]);
        app.mode = AppMode::TrainDetail;
        app.selected_train_index = Some(0);
        assert!(render(&mut app, 140, 40).contains("(track changed)"));
        app.items[0].flags = Some(vec!["AIRPORT".into()]);
        assert!(!render(&mut app, 140, 40).contains("(track changed)"));
    }

    #[test]
    fn narrow_boards_drop_the_least_important_columns() {
        assert_eq!(board_columns(200), Col::ALL);
        let at_80 = board_columns(78);
        assert!(!at_80.contains(&Col::Sector) && !at_80.contains(&Col::Actual));
        assert!(at_80.contains(&Col::Remarks));
        assert!(min_table_width(&at_80) <= 78);
        assert_eq!(
            board_columns(10),
            [Col::Time, Col::Delay, Col::Train, Col::Place, Col::Track]
        );

        let mut app = loaded_app();
        let out = render(&mut app, 80, 30);
        assert!(!out.contains("SECTOR"), "{out}");
        assert!(out.contains("Salzburg Hbf") && out.contains("TRACK"));
    }

    #[test]
    fn key_hints_shrink_to_fit_the_status_bar() {
        let mut app = loaded_app();
        let out = render(&mut app, 160, 40);
        assert!(
            out.contains("Enter Detail") && out.contains("[?]Help"),
            "{out}"
        );
        let out = render(&mut app, 100, 40);
        assert!(
            !out.contains("Enter Detail") && out.contains("[A]rr"),
            "{out}"
        );
        let out = render(&mut app, 60, 40);
        assert!(!out.contains("[A]rr") && out.contains("[?]Help"), "{out}");
    }

    #[test]
    fn help_overlay_draws_over_any_mode() {
        for mode in [AppMode::Normal, AppMode::TrainDetail, AppMode::Filter] {
            let mut app = loaded_app();
            app.mode = mode;
            app.selected_train_index = Some(0);
            app.show_help = true;
            let out = render(&mut app, 140, 50);
            let tr = app.lang.tr();
            assert!(out.contains(tr.help_title), "{out}");
            assert!(out.contains("Toggle language"));
            assert!(out.contains(tr.help_close));
        }
    }

    #[test]
    fn filter_shows_in_board_title_with_counts() {
        let mut app = loaded_app();
        app.mode = AppMode::Filter;
        app.filter = "rjx".into();
        let out = render(&mut app, 140, 40);
        assert!(out.contains("Trains · Filter: rjx_ (1/2)"), "{out}");
        assert!(out.contains(app.lang.tr().filter_help));
        assert!(!out.contains("REX 1"));

        app.mode = AppMode::Normal;
        assert!(render(&mut app, 140, 40).contains("Filter: rjx (1/2)"));
    }

    #[test]
    fn arrival_detail_title_names_the_origin() {
        let mut app = loaded_app();
        app.content_type = ContentType::Arrival;
        app.mode = AppMode::TrainDetail;
        app.selected_train_index = Some(0);
        let out = render(&mut app, 140, 40);
        assert!(out.contains("RJX from Wien Hbf"), "{out}");
    }

    #[test]
    fn title_shows_the_clock_when_there_is_room() {
        let mut app = loaded_app();
        let clock = Local::now().format("%H:%M").to_string();
        // The minute may tick over mid-test; accept the next one too.
        let next = (Local::now() + chrono::Duration::minutes(1))
            .format("%H:%M")
            .to_string();
        let out = render(&mut app, 140, 40);
        assert!(out.contains(&clock) || out.contains(&next));
    }

    #[test]
    fn update_age_is_relative_and_coloured_by_staleness() {
        assert_eq!(format_age(Lang::En.tr(), 12), "12 s ago");
        assert_eq!(format_age(Lang::De.tr(), 12), "vor 12 s");
        assert_eq!(format_age(Lang::En.tr(), 185), "3 min ago");
        assert_eq!(format_age(Lang::En.tr(), -4), "0 s ago", "clock skew");
        assert_eq!(age_color(30), Color::White);
        assert_eq!(age_color(STALE_AFTER_SECS + 1), Color::Yellow);
        assert_eq!(age_color(2 * STALE_AFTER_SECS + 1), Color::Red);

        let mut app = loaded_app();
        app.last_update = Some(Local::now() - chrono::Duration::seconds(5));
        assert!(render(&mut app, 140, 40).contains("s ago"));
    }

    #[test]
    fn station_list_marks_recents_without_numbering() {
        let mut app = loaded_app();
        app.recent_stations = vec!["8101114".into()];
        app.enter_station_select();
        let out = render(&mut app, 120, 40);
        assert!(out.contains("↻ Salzburg Hbf"), "{out}");
        assert!(!out.contains(" 1. "));
    }

    #[test]
    fn error_reasons_are_truncated_to_one_line() {
        let tr = Lang::En.tr();
        let long = "x".repeat(500);
        let text = describe_error(tr, &LinkError::Closed(long));
        assert!(text.len() < 100, "{text}");
        assert_eq!(
            describe_error(tr, &LinkError::Closed(String::new())),
            tr.err_closed
        );
    }
}
