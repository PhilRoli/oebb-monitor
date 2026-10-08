//! ÖBB Monitor — a terminal UI for live Austrian Federal Railways departure and
//! arrival boards, streamed over WebSocket.
//!
//! The program is split into a few small modules:
//! - [`debug`]: opt-in file logger and the `debug!` macro.
//! - [`model`]: serde types mirroring the WebSocket JSON payloads.
//! - [`app`]: application state, key-agnostic logic, and pure helpers.
//! - [`ws`]: the background task that maintains the live connection.
//! - [`ui`]: all rendering.
//! - [`lang`]: German/English UI strings and the language toggle.
//! - [`config`]: persisted settings (the chosen language).
//!
//! `main` owns terminal setup/teardown and the input/redraw event loop.

#[macro_use]
mod debug;
mod app;
mod config;
mod lang;
mod model;
mod ui;
mod ws;

use anyhow::Result;
use crossterm::{
    event::{Event, EventStream, KeyCode, KeyEvent, KeyEventKind},
    execute,
    terminal::{disable_raw_mode, enable_raw_mode, EnterAlternateScreen, LeaveAlternateScreen},
};
use futures_util::StreamExt;
use ratatui::{backend::CrosstermBackend, Terminal};
use std::{io, sync::Arc, time::Duration};
use tokio::sync::{mpsc, Mutex, Notify};

use app::{App, AppMode, ContentType};

/// The concrete terminal type used throughout the program.
type Tui = Terminal<CrosstermBackend<io::Stdout>>;

/// Put the terminal into the alternate screen + raw mode for TUI rendering.
fn setup_terminal() -> Result<Tui> {
    enable_raw_mode()?;
    let mut stdout = io::stdout();
    execute!(stdout, EnterAlternateScreen)?;
    let backend = CrosstermBackend::new(stdout);
    Ok(Terminal::new(backend)?)
}

/// Restore the user's terminal to its normal state. Safe to call on any exit
/// path (clean shutdown or error), so the loop never leaves a broken terminal.
fn restore_terminal(terminal: &mut Tui) -> Result<()> {
    disable_raw_mode()?;
    execute!(terminal.backend_mut(), LeaveAlternateScreen)?;
    terminal.show_cursor()?;
    Ok(())
}

#[tokio::main]
async fn main() -> Result<()> {
    if std::env::args().any(|a| a == "--version" || a == "-V") {
        println!("{} {}", env!("CARGO_PKG_NAME"), env!("CARGO_PKG_VERSION"));
        return Ok(());
    }

    debug!("Application starting");
    debug!("Debug mode: {}", debug::DEBUG.enabled);

    // Restore the terminal even if a panic unwinds past our normal cleanup.
    let original_hook = std::panic::take_hook();
    std::panic::set_hook(Box::new(move |info| {
        let _ = disable_raw_mode();
        let _ = execute!(io::stdout(), LeaveAlternateScreen);
        original_hook(info);
    }));

    let mut terminal = setup_terminal()?;
    // Run the app, then always restore the terminal before propagating any error.
    let result = run(&mut terminal).await;
    restore_terminal(&mut terminal)?;
    result
}

/// The main event loop: render, then wait for the next of three wake-ups —
/// a key press, a state-change notification from the WebSocket task, or a
/// periodic tick that keeps the status clock fresh.
async fn run(terminal: &mut Tui) -> Result<()> {
    let app = Arc::new(Mutex::new(App::new()));
    let (reconnect_tx, reconnect_rx) = mpsc::channel(10);
    let notify = Arc::new(Notify::new());

    debug!("Spawning WebSocket handler task");
    let ws_app = app.clone();
    let ws_notify = notify.clone();
    tokio::spawn(async move {
        let _ = ws::run_websocket(ws_app, reconnect_rx, ws_notify).await;
    });

    let mut events = EventStream::new();
    // Fallback redraw so the "last update" clock and connecting state animate
    // even when no other event arrives.
    let mut redraw_tick = tokio::time::interval(Duration::from_secs(1));

    debug!("Entering main event loop");
    loop {
        {
            let mut app = app.lock().await;
            terminal.draw(|f| ui::ui(f, &mut app))?;
        }

        tokio::select! {
            maybe_event = events.next() => {
                match maybe_event {
                    Some(Ok(Event::Key(key))) => {
                        // Ignore key-release events on terminals that report them.
                        if key.kind != KeyEventKind::Release
                            && handle_key(&app, &reconnect_tx, key).await
                        {
                            break;
                        }
                    }
                    // Resize or other events: just loop and redraw.
                    Some(Ok(_)) => {}
                    Some(Err(e)) => {
                        debug!("Input stream error: {}", e);
                    }
                    // Input stream ended; nothing left to drive the loop.
                    None => break,
                }
            }
            _ = notify.notified() => {}
            _ = redraw_tick.tick() => {}
        }
    }

    Ok(())
}

/// Toggle the UI language and persist the choice for next launch.
fn toggle_language(app: &mut App) {
    app.lang = app.lang.toggle();
    debug!("Language toggled to {}", app.lang.code());
    // Tests must never overwrite the developer's real config file.
    #[cfg(not(test))]
    config::save_language(app.lang);
}

/// Apply a single key press to the app state, sending a reconnect signal when a
/// change requires re-fetching data. Returns `true` when the program should quit.
async fn handle_key(app: &Arc<Mutex<App>>, reconnect_tx: &mpsc::Sender<()>, key: KeyEvent) -> bool {
    let mut app = app.lock().await;

    match app.mode {
        AppMode::Normal => match key.code {
            KeyCode::Char('q') | KeyCode::Char('Q') => {
                debug!("Quit key pressed");
                return true;
            }
            KeyCode::Char('a') | KeyCode::Char('A') => {
                debug!("Switching to Arrivals");
                app.content_type = ContentType::Arrival;
                drop(app);
                let _ = reconnect_tx.send(()).await;
            }
            KeyCode::Char('d') | KeyCode::Char('D') => {
                debug!("Switching to Departures");
                app.content_type = ContentType::Departure;
                drop(app);
                let _ = reconnect_tx.send(()).await;
            }
            KeyCode::Char('s') | KeyCode::Char('S') => {
                debug!("Entering station select mode");
                app.enter_station_select();
            }
            KeyCode::Char('r') | KeyCode::Char('R') => {
                debug!("Manual refresh requested");
                drop(app);
                let _ = reconnect_tx.send(()).await;
            }
            KeyCode::Char(c) if c.is_ascii_digit() => {
                let digit = c.to_digit(10).unwrap() as usize;
                let index = if digit == 0 { 9 } else { digit - 1 };
                if index < app.items.len() {
                    debug!("Selecting train at index {}", index);
                    app.selected_train_index = Some(index);
                    app.selected_train_id = app.items.get(index).map(|t| t.id.clone());
                    app.detail_scroll = 0;
                    app.mode = AppMode::TrainDetail;
                }
            }
            KeyCode::Char('l') | KeyCode::Char('L') => toggle_language(&mut app),
            KeyCode::Up => app.select_relative(-1),
            KeyCode::Down => app.select_relative(1),
            KeyCode::Enter => {
                if let Some(idx) = app.selected_train_index {
                    debug!("Opening train detail");
                    app.selected_train_id = app.items.get(idx).map(|t| t.id.clone());
                    app.detail_scroll = 0;
                    app.mode = AppMode::TrainDetail;
                }
            }
            _ => {}
        },
        AppMode::TrainDetail => match key.code {
            KeyCode::Esc | KeyCode::Char('q') | KeyCode::Char('Q') => {
                debug!("Closing train detail");
                app.selected_train_id = None;
                app.mode = AppMode::Normal;
            }
            KeyCode::Char('l') | KeyCode::Char('L') => toggle_language(&mut app),
            KeyCode::Up => app.select_relative(-1),
            KeyCode::Down => app.select_relative(1),
            KeyCode::PageUp => {
                app.detail_scroll = app.detail_scroll.saturating_sub(3);
            }
            KeyCode::PageDown => {
                app.detail_scroll = app.detail_scroll.saturating_add(3);
            }
            _ => {}
        },
        AppMode::StationSelect => match key.code {
            KeyCode::Esc => {
                debug!("Exiting station select");
                app.exit_station_select();
            }
            KeyCode::Enter => {
                let before_station = app.station_id.clone();
                if app.select_station() {
                    debug!("Station changed: {} -> {}", before_station, app.station_id);
                    drop(app);
                    let _ = reconnect_tx.send(()).await;
                }
            }
            KeyCode::Up => {
                let i = app.station_list_state.selected().unwrap_or(0);
                if i > 0 {
                    app.station_list_state.select(Some(i - 1));
                }
            }
            KeyCode::Down => {
                let i = app.station_list_state.selected().unwrap_or(0);
                if i < app.filtered_stations.len().saturating_sub(1) {
                    app.station_list_state.select(Some(i + 1));
                }
            }
            KeyCode::Char(c) => {
                app.station_search.push(c);
                app.update_filtered_stations();
            }
            KeyCode::Backspace => {
                app.station_search.pop();
                app.update_filtered_stations();
            }
            _ => {}
        },
    }

    false
}

#[cfg(test)]
mod tests {

    use super::*;
    use crate::lang::Lang;
    use crate::model::TrainItem;
    use crossterm::event::KeyModifiers;

    fn key(code: KeyCode) -> KeyEvent {
        KeyEvent::new(code, KeyModifiers::NONE)
    }

    fn ch(c: char) -> KeyEvent {
        key(KeyCode::Char(c))
    }

    struct Harness {
        app: Arc<Mutex<App>>,
        tx: mpsc::Sender<()>,
        rx: mpsc::Receiver<()>,
    }

    impl Harness {
        fn new(trains: usize) -> Self {
            let mut app = App::new();
            app.lang = Lang::En;
            app.items = (0..trains)
                .map(|i| TrainItem {
                    id: format!("t{i}"),
                    train: format!("T {i}"),
                    scheduled: "2024-01-01T10:00:00+01:00".into(),
                    ..Default::default()
                })
                .collect();
            let (tx, rx) = mpsc::channel(10);
            Self {
                app: Arc::new(Mutex::new(app)),
                tx,
                rx,
            }
        }

        /// Press a key; returns whether the app asked to quit.
        async fn press(&self, k: KeyEvent) -> bool {
            handle_key(&self.app, &self.tx, k).await
        }

        fn reconnects(&mut self) -> usize {
            let mut n = 0;
            while self.rx.try_recv().is_ok() {
                n += 1;
            }
            n
        }
    }

    #[tokio::test]
    async fn q_quits_from_normal_but_only_closes_detail() {
        let h = Harness::new(2);
        assert!(h.press(ch('q')).await);
        assert!(h.press(ch('Q')).await);

        h.press(ch('1')).await;
        assert_eq!(h.app.lock().await.mode, AppMode::TrainDetail);
        assert!(!h.press(ch('q')).await);
        assert_eq!(h.app.lock().await.mode, AppMode::Normal);
    }

    #[tokio::test]
    async fn a_and_d_switch_content_type_and_reconnect() {
        let mut h = Harness::new(0);
        h.press(ch('a')).await;
        assert_eq!(h.app.lock().await.content_type, ContentType::Arrival);
        assert_eq!(h.reconnects(), 1);
        h.press(ch('D')).await;
        assert_eq!(h.app.lock().await.content_type, ContentType::Departure);
        assert_eq!(h.reconnects(), 1);
    }

    #[tokio::test]
    async fn r_requests_reconnect_without_changing_state() {
        let mut h = Harness::new(1);
        h.press(ch('r')).await;
        h.press(ch('R')).await;
        assert_eq!(h.reconnects(), 2);
        assert_eq!(h.app.lock().await.mode, AppMode::Normal);
    }

    #[tokio::test]
    async fn digits_open_detail_for_that_train() {
        let h = Harness::new(12);
        h.press(ch('3')).await;
        {
            let app = h.app.lock().await;
            assert_eq!(app.mode, AppMode::TrainDetail);
            assert_eq!(app.selected_train_index, Some(2));
            assert_eq!(app.selected_train_id.as_deref(), Some("t2"));
        }
        h.press(key(KeyCode::Esc)).await;
        h.press(ch('0')).await; // 0 means the 10th row
        assert_eq!(h.app.lock().await.selected_train_index, Some(9));
    }

    #[tokio::test]
    async fn digit_beyond_list_is_ignored() {
        let h = Harness::new(2);
        h.press(ch('5')).await;
        let app = h.app.lock().await;
        assert_eq!(app.mode, AppMode::Normal);
        assert_eq!(app.selected_train_index, None);
    }

    #[tokio::test]
    async fn arrows_move_selection_and_enter_opens_detail() {
        let h = Harness::new(3);
        h.press(key(KeyCode::Enter)).await; // nothing selected yet
        assert_eq!(h.app.lock().await.mode, AppMode::Normal);

        // The first arrow press only selects the first row.
        h.press(key(KeyCode::Down)).await;
        assert_eq!(h.app.lock().await.selected_train_index, Some(0));
        h.press(key(KeyCode::Down)).await;
        h.press(key(KeyCode::Down)).await;
        h.press(key(KeyCode::Up)).await;
        assert_eq!(h.app.lock().await.selected_train_index, Some(1));

        h.press(key(KeyCode::Enter)).await;
        let app = h.app.lock().await;
        assert_eq!(app.mode, AppMode::TrainDetail);
        assert_eq!(app.selected_train_id.as_deref(), Some("t1"));
    }

    #[tokio::test]
    async fn detail_navigation_scroll_and_close() {
        let h = Harness::new(3);
        h.press(ch('1')).await;
        h.press(key(KeyCode::Down)).await;
        assert_eq!(h.app.lock().await.selected_train_id.as_deref(), Some("t1"));

        h.press(key(KeyCode::PageDown)).await;
        h.press(key(KeyCode::PageDown)).await;
        assert_eq!(h.app.lock().await.detail_scroll, 6);
        h.press(key(KeyCode::PageUp)).await;
        assert_eq!(h.app.lock().await.detail_scroll, 3);
        h.press(key(KeyCode::PageUp)).await;
        h.press(key(KeyCode::PageUp)).await; // saturates at 0
        assert_eq!(h.app.lock().await.detail_scroll, 0);

        h.press(key(KeyCode::Esc)).await;
        let app = h.app.lock().await;
        assert_eq!(app.mode, AppMode::Normal);
        assert_eq!(app.selected_train_id, None);
    }

    #[tokio::test]
    async fn l_toggles_language_in_both_modes() {
        let h = Harness::new(1);
        h.press(ch('l')).await;
        assert_eq!(h.app.lock().await.lang, Lang::De);
        h.press(ch('1')).await;
        h.press(ch('L')).await;
        assert_eq!(h.app.lock().await.lang, Lang::En);
    }

    #[tokio::test]
    async fn station_select_flow_types_filters_and_commits() {
        let mut h = Harness::new(0);
        h.press(ch('s')).await;
        assert_eq!(h.app.lock().await.mode, AppMode::StationSelect);

        for c in "salzburg".chars() {
            h.press(ch(c)).await;
        }
        {
            let app = h.app.lock().await;
            assert_eq!(app.station_search, "salzburg");
            assert!(!app.filtered_stations.is_empty());
        }
        h.press(key(KeyCode::Backspace)).await;
        assert_eq!(h.app.lock().await.station_search, "salzbur");

        h.press(key(KeyCode::Down)).await;
        h.press(key(KeyCode::Up)).await;
        let expected = {
            let app = h.app.lock().await;
            app.filtered_stations[0].clone()
        };
        h.press(key(KeyCode::Enter)).await;
        let app = h.app.lock().await;
        assert_eq!(app.mode, AppMode::Normal);
        assert_eq!((app.station_id.clone(), app.station_name.clone()), expected);
        drop(app);
        assert_eq!(h.reconnects(), 1);
    }

    #[tokio::test]
    async fn station_select_esc_cancels_without_reconnect() {
        let mut h = Harness::new(0);
        let before = h.app.lock().await.station_id.clone();
        h.press(ch('s')).await;
        h.press(key(KeyCode::Esc)).await;
        let app = h.app.lock().await;
        assert_eq!(app.mode, AppMode::Normal);
        assert_eq!(app.station_id, before);
        drop(app);
        assert_eq!(h.reconnects(), 0);
    }

    #[tokio::test]
    async fn station_select_enter_with_no_match_stays_and_does_not_reconnect() {
        let mut h = Harness::new(0);
        h.press(ch('s')).await;
        for c in "zzzzqq".chars() {
            h.press(ch(c)).await;
        }
        h.press(key(KeyCode::Enter)).await;
        assert_eq!(h.app.lock().await.mode, AppMode::StationSelect);
        assert_eq!(h.reconnects(), 0);
    }

    #[tokio::test]
    async fn station_select_cursor_stays_in_bounds() {
        let h = Harness::new(0);
        h.press(ch('s')).await;
        h.press(key(KeyCode::Up)).await;
        assert_eq!(h.app.lock().await.station_list_state.selected(), Some(0));
        for _ in 0..100 {
            h.press(key(KeyCode::Down)).await;
        }
        let app = h.app.lock().await;
        assert_eq!(
            app.station_list_state.selected(),
            Some(app.filtered_stations.len() - 1)
        );
    }

    #[tokio::test]
    async fn station_select_swallows_command_letters_as_search_text() {
        let mut h = Harness::new(0);
        h.press(ch('s')).await;
        assert!(!h.press(ch('q')).await);
        h.press(ch('a')).await;
        let app = h.app.lock().await;
        assert_eq!(app.station_search, "qa");
        assert_eq!(app.content_type, ContentType::Departure);
        drop(app);
        assert_eq!(h.reconnects(), 0);
    }

    #[tokio::test]
    async fn unknown_keys_are_ignored() {
        let mut h = Harness::new(1);
        for k in [key(KeyCode::F(5)), ch('x'), key(KeyCode::Tab)] {
            assert!(!h.press(k).await);
        }
        assert_eq!(h.reconnects(), 0);
        assert_eq!(h.app.lock().await.mode, AppMode::Normal);
    }
}
