//! All terminal rendering.
//!
//! [`ui`] dispatches to one of three screen renderers based on the current
//! [`AppMode`]. Rendering is a pure function of [`App`] state; nothing here
//! mutates application data beyond the list widget's own scroll/selection.
//! All user-facing text comes from the active language's [`crate::lang::Tr`].

use ratatui::{
    layout::{Alignment, Constraint, Direction, Layout, Rect},
    style::{Color, Modifier, Style},
    text::{Line, Span},
    widgets::{Block, Borders, Cell, List, ListItem, Paragraph, Row, Table, Wrap},
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
        AppMode::Normal => render_main(f, app),
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

/// The main board: title, the departures/arrivals table, station notices,
/// and a status line with connection state and key hints.
fn render_main(f: &mut Frame, app: &mut App) {
    let tr = app.lang.tr();

    let chunks = Layout::default()
        .direction(Direction::Vertical)
        .constraints([
            Constraint::Length(3),
            Constraint::Min(10),
            Constraint::Length(5),
            Constraint::Length(3),
        ])
        .split(f.area());

    let title_text = match app.content_type {
        ContentType::Departure => format!("🚂 {} - {}", tr.departures, app.station_name),
        ContentType::Arrival => format!("🚂 {} - {}", tr.arrivals, app.station_name),
    };

    let title = Paragraph::new(title_text)
        .style(
            Style::default()
                .fg(Color::Cyan)
                .add_modifier(Modifier::BOLD),
        )
        .alignment(Alignment::Center)
        .block(Block::default().borders(Borders::ALL));
    f.render_widget(title, chunks[0]);

    let header_cells: Vec<Cell> = [
        "#",
        tr.col_time,
        tr.col_actual,
        tr.col_delay,
        tr.col_train,
        tr.col_line,
        if app.content_type == ContentType::Departure {
            tr.col_dest
        } else {
            tr.col_from
        },
        tr.col_track,
        tr.col_sector,
        tr.col_remarks,
    ]
    .iter()
    .map(|h| {
        Cell::from(*h).style(
            Style::default()
                .fg(Color::Yellow)
                .add_modifier(Modifier::BOLD),
        )
    })
    .collect();

    let header = Row::new(header_cells).height(1).bottom_margin(1);

    let rows = app.items.iter().enumerate().map(|(idx, item)| {
        let scheduled_time = format_time(&item.scheduled);

        let (actual_time, delay_str, delay_col) = if let Some(delay) = calculate_delay(item) {
            let actual = item
                .expected
                .as_ref()
                .map(|e| format_time(e))
                .unwrap_or_else(|| "-".to_string());
            let delay_text = if delay > 0 {
                format!("+{}", delay)
            } else {
                delay.to_string()
            };
            (actual, delay_text, delay_color(delay))
        } else {
            ("-".to_string(), "-".to_string(), Color::Gray)
        };

        let train = item.train.clone();
        let line = item
            .line
            .clone()
            .or_else(|| item.product.clone())
            .unwrap_or_default();

        let dest = match app.content_type {
            ContentType::Departure => item
                .destination
                .as_ref()
                .map(|d| d.default.clone())
                .unwrap_or_else(|| "N/A".to_string()),
            ContentType::Arrival => item
                .origin
                .as_ref()
                .map(|o| o.default.clone())
                .unwrap_or_else(|| "N/A".to_string()),
        };

        let track = item.track.clone().unwrap_or_else(|| "-".to_string());
        let sector = item.sector.clone().unwrap_or_else(|| "-".to_string());
        let remarks = item
            .remarks
            .as_ref()
            .and_then(|r| r.first())
            // Remark text can contain embedded newlines; flatten to a space so
            // it stays on the single-height table row.
            .map(|r| r.text.default.replace('\n', " "))
            .unwrap_or_default();

        let number_str = match idx {
            0..=8 => format!("{}", idx + 1),
            9 => "0".to_string(),
            _ => " ".to_string(),
        };
        let is_selected = app.selected_train_index == Some(idx);

        let mut row = Row::new(vec![
            Cell::from(number_str).style(if is_selected {
                Style::default()
                    .fg(Color::Green)
                    .add_modifier(Modifier::BOLD)
            } else {
                Style::default().fg(Color::DarkGray)
            }),
            Cell::from(scheduled_time),
            Cell::from(actual_time),
            Cell::from(delay_str).style(Style::default().fg(delay_col)),
            Cell::from(train),
            Cell::from(line).style(Style::default().fg(Color::Cyan)),
            Cell::from(dest),
            Cell::from(track).style(Style::default().fg(Color::Magenta)),
            Cell::from(sector),
            Cell::from(remarks),
        ]);

        if is_selected {
            row = row.style(Style::default().bg(Color::DarkGray));
        }

        row
    });

    let table = Table::new(
        rows,
        [
            Constraint::Length(3),
            Constraint::Length(6),
            Constraint::Length(6),
            Constraint::Length(8),
            Constraint::Length(8),
            Constraint::Length(8),
            Constraint::Length(25),
            Constraint::Length(7),
            Constraint::Length(8),
            Constraint::Min(15),
        ],
    )
    .header(header)
    .block(
        Block::default()
            .borders(Borders::ALL)
            .title(tr.block_trains),
    );

    f.render_widget(table, chunks[1]);

    // Ratatui's Paragraph wraps for us (correctly accounting for character
    // width), so we just emit one bullet line per notice and let it flow.
    let notices_text: Vec<Line> = app
        .special_notices
        .iter()
        .map(|notice| Line::from(format!("• {}", notice.text.default)))
        .collect();

    let notices = Paragraph::new(notices_text)
        .style(Style::default().fg(Color::Yellow))
        .block(
            Block::default()
                .borders(Borders::ALL)
                .title(tr.block_notices),
        )
        .wrap(Wrap { trim: true });
    f.render_widget(notices, chunks[2]);

    let (update_text, update_color) = match &app.connection {
        ConnectionState::Connected => match app.last_update {
            Some(t) => (t.format("%H:%M:%S").to_string(), Color::White),
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
    let status_text = vec![Line::from(status_spans)];

    let status = Paragraph::new(status_text).block(
        Block::default()
            .borders(Borders::ALL)
            .title(tr.block_status),
    );
    f.render_widget(status, chunks[3]);

    let shortcuts_width = 64u16;
    let shortcuts_x = chunks[3].x + chunks[3].width.saturating_sub(shortcuts_width + 2);
    let shortcuts_y = chunks[3].y + 1;

    if shortcuts_x > chunks[3].x {
        let key = |s: &'static str| {
            Span::styled(
                s,
                Style::default()
                    .fg(Color::Green)
                    .add_modifier(Modifier::BOLD),
            )
        };
        let shortcuts = Line::from(vec![
            key("1-9,0"),
            Span::raw("/"),
            key("↑↓"),
            Span::raw("+"),
            key("Enter"),
            Span::raw(format!(" {} ", tr.detail)),
            key("[A]"),
            Span::raw(format!("{} ", tr.hint_arr)),
            key("[D]"),
            Span::raw(format!("{} ", tr.hint_dep)),
            key("[S]"),
            Span::raw(format!("{} ", tr.hint_stn)),
            key("[R]"),
            Span::raw(format!("{} ", tr.hint_ref)),
            key("[L]"),
            Span::raw(format!("{} ", tr.hint_lang)),
            Span::styled(
                "[Q]",
                Style::default().fg(Color::Red).add_modifier(Modifier::BOLD),
            ),
            Span::raw(tr.hint_quit),
        ]);

        let shortcuts_area = Rect {
            x: shortcuts_x,
            y: shortcuts_y,
            width: shortcuts_width,
            height: 1,
        };

        let shortcuts_widget = Paragraph::new(shortcuts).alignment(Alignment::Right);
        f.render_widget(shortcuts_widget, shortcuts_area);
    }
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
        .borders(Borders::ALL)
        .style(Style::default().bg(Color::Black));
    f.render_widget(block, popup_area);

    let search_text = format!("{}{}_", tr.search, app.station_search);
    let search = Paragraph::new(search_text)
        .style(Style::default().fg(Color::Yellow))
        .block(Block::default().borders(Borders::ALL));
    f.render_widget(search, chunks[0]);

    let items: Vec<ListItem> = app
        .filtered_stations
        .iter()
        .enumerate()
        .map(|(i, (_, name))| ListItem::new(format!("{:2}. {}", i + 1, name)))
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
        .highlight_style(
            Style::default()
                .bg(Color::DarkGray)
                .add_modifier(Modifier::BOLD),
        )
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

    let train = app.selected_train_index.and_then(|idx| app.items.get(idx));

    if let Some(train) = train {
        let chunks = Layout::default()
            .direction(Direction::Vertical)
            .constraints([
                Constraint::Length(3),
                Constraint::Min(5),
                Constraint::Length(3),
            ])
            .split(popup_area);

        let block = Block::default()
            .borders(Borders::ALL)
            .style(Style::default().bg(Color::Black));
        f.render_widget(block, popup_area);

        let line = train
            .line
            .as_deref()
            .or(train.product.as_deref())
            .unwrap_or("?");
        let dest = train
            .destination
            .as_ref()
            .or(train.origin.as_ref())
            .map(|d| d.default.as_str())
            .unwrap_or("N/A");
        let title_text = format!("🚂 {} {} - {} → {}", tr.train, train.train, line, dest);
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
                Span::styled(track.clone(), Style::default().fg(Color::Magenta)),
                Span::raw(" "),
                Span::styled(
                    train
                        .sector
                        .as_ref()
                        .map(|s| format!("{} {}", tr.sector, s))
                        .unwrap_or_default(),
                    Style::default().fg(Color::Magenta),
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
        if let Some(prioritized) = &train.prioritized_vias {
            if !prioritized.is_empty() {
                content_lines.push(Line::from(Span::styled(
                    format!("{}: {}", tr.major_stops, prioritized.join(" ~ ")),
                    Style::default()
                        .fg(Color::Yellow)
                        .add_modifier(Modifier::BOLD),
                )));
            }
        }

        if let Some(via) = &train.via {
            let stops: Vec<String> = via
                .default
                .split('~')
                .map(|s| s.replace("&#8203;", "").trim().to_string())
                .filter(|s| !s.is_empty())
                .collect();

            if !stops.is_empty() {
                content_lines.push(Line::from(Span::styled(
                    format!("{}:", tr.all_stops),
                    Style::default()
                        .fg(Color::Cyan)
                        .add_modifier(Modifier::BOLD),
                )));
                content_lines.push(Line::from(format!("  {}", stops.join(" → "))));
            }
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

        let content = Paragraph::new(content_lines)
            .block(Block::default().borders(Borders::ALL).title(tr.details))
            .wrap(Wrap { trim: true })
            .scroll((app.detail_scroll, 0));
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
            .params
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
        render(&mut app, 120, 40);
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
        assert!(out.contains("Zug"));
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
