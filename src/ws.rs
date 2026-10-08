use anyhow::Result;
use chrono::Local;
use futures_util::{SinkExt, StreamExt};
use std::{collections::HashMap, sync::Arc, time::Duration};
use tokio::sync::{mpsc, Mutex, Notify};
use tokio_tungstenite::{connect_async, tungstenite::protocol::Message};

use crate::app::{build_ws_url, App, ConnectionState, ContentType};
use crate::model::{TrainItem, UpdateParams, WsMessage};

/// How long a page socket may stay silent before it is considered dead.
const READ_TIMEOUT: Duration = Duration::from_secs(90);
const MAX_BACKOFF: Duration = Duration::from_secs(30);

/// Events emitted by a single per-page WebSocket worker task back to the
/// coordinating loop in [`run_websocket`].
enum PageEvent {
    /// The page's socket connected successfully.
    Connected,
    /// The page failed to connect at all.
    Failed,
    /// A connected page socket closed, errored or went silent.
    Ended(usize),
    /// The page delivered a fresh `update` payload, already narrowed to the
    /// items of the content type its socket was opened for.
    Update(usize, Vec<TrainItem>, UpdateParams),
}

/// Backoff delay with up to ~25% jitter so parallel clients don't sync up.
fn jittered(base: Duration) -> Duration {
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.subsec_nanos())
        .unwrap_or(0);
    base + base.mul_f64(f64::from(nanos % 1000) / 4000.0)
}

/// Drives the live data connection for the lifetime of the program.
///
/// Each iteration opens `max_pages` parallel WebSocket connections for the
/// currently selected station and content type, merges their incremental
/// updates into [`App::items`], and keeps reconnecting. A reconnect is
/// triggered either by a signal on `reconnect_rx` (station change, A/D switch,
/// manual refresh) or by the sockets closing on their own.
///
/// `notify` is pulsed whenever the rendered state changes so the UI loop can
/// redraw without busy-polling.
pub async fn run_websocket(
    app: Arc<Mutex<App>>,
    mut reconnect_rx: mpsc::Receiver<()>,
    notify: Arc<Notify>,
) -> Result<()> {
    debug!("WebSocket handler started");
    let mut active_tasks: Vec<tokio::task::JoinHandle<()>> = Vec::new();
    let mut prev: Option<(String, ContentType)> = None;
    // Exponential backoff between self-initiated reconnects; reset on success.
    let mut backoff = Duration::from_secs(1);
    let mut iteration = 0;

    loop {
        iteration += 1;
        debug!("=== WebSocket iteration {} ===", iteration);

        for task in active_tasks.drain(..) {
            task.abort();
        }

        let (max_pages, station_id, content_type) = {
            let app_guard = app.lock().await;
            (
                app_guard.max_pages,
                app_guard.station_id.clone(),
                app_guard.content_type.clone(),
            )
        };

        // Only wipe the visible board when the station or direction actually
        // changed. For a plain refresh/reconnect we keep the stale rows on
        // screen until fresh data lands, avoiding a jarring blank flash.
        let target = (station_id.clone(), content_type.clone());
        let params_changed = prev.as_ref() != Some(&target);
        {
            let mut app_guard = app.lock().await;
            if params_changed {
                debug!(
                    "Station/content changed, clearing {} items",
                    app_guard.items.len()
                );
                app_guard.items.clear();
                app_guard.selected_train_index = None;
                app_guard.selected_train_id = None;
            }
            app_guard.connection = ConnectionState::Connecting;
            app_guard.last_update = None;
        }
        prev = Some(target);
        notify.notify_one();

        debug!(
            "Station: {}, ContentType: {:?}, Pages: {}",
            station_id, content_type, max_pages
        );

        let (page_tx, mut page_rx) = mpsc::channel(100);

        for page in 1..=max_pages {
            let url = build_ws_url(&station_id, &content_type, page);
            debug!("Spawning task for page {}: {}", page, url);
            let tx = page_tx.clone();
            let page_type = content_type.clone();

            let task = tokio::spawn(async move {
                debug!("Page {} task started, connecting...", page);
                match connect_async(&url).await {
                    Ok((ws_stream, _)) => {
                        debug!("Page {} connected successfully", page);
                        let _ = tx.send(PageEvent::Connected).await;
                        let (mut write, mut read) = ws_stream.split();
                        let mut msg_count = 0;

                        loop {
                            let msg = match tokio::time::timeout(READ_TIMEOUT, read.next()).await {
                                Ok(Some(msg)) => msg,
                                Ok(None) => break,
                                Err(_) => {
                                    debug!("Page {} timed out after {:?}", page, READ_TIMEOUT);
                                    break;
                                }
                            };
                            match msg {
                                Ok(Message::Text(text)) => {
                                    msg_count += 1;
                                    if let Ok(mut ws_msg) = serde_json::from_str::<WsMessage>(&text)
                                    {
                                        if ws_msg.method.as_deref() == Some("update") {
                                            if let Some(mut params) = ws_msg.params.take() {
                                                // Narrow to the type this socket was opened
                                                // for, not whatever the UI has selected now.
                                                let items = match page_type {
                                                    ContentType::Departure => {
                                                        params.data.departures.take()
                                                    }
                                                    ContentType::Arrival => {
                                                        params.data.arrivals.take()
                                                    }
                                                }
                                                .unwrap_or_default();
                                                let _ = tx
                                                    .send(PageEvent::Update(page, items, params))
                                                    .await;
                                            }
                                        }
                                    } else {
                                        debug!("Page {} failed to parse message", page);
                                    }
                                }
                                Ok(Message::Ping(payload)) => {
                                    if write.send(Message::Pong(payload)).await.is_err() {
                                        break;
                                    }
                                }
                                Ok(Message::Close(reason)) => {
                                    debug!("Page {} WebSocket closed: {:?}", page, reason);
                                    break;
                                }
                                Err(e) => {
                                    debug!("Page {} WebSocket error: {}", page, e);
                                    break;
                                }
                                _ => {}
                            }
                        }
                        debug!("Page {} task ending after {} messages", page, msg_count);
                        let _ = tx.send(PageEvent::Ended(page)).await;
                    }
                    Err(e) => {
                        debug!("Page {} failed to connect: {}", page, e);
                        let _ = tx.send(PageEvent::Failed).await;
                    }
                }
            });

            active_tasks.push(task);
        }

        drop(page_tx);
        debug!("Spawned {} tasks, now listening for updates", max_pages);

        // Items are tracked per page and rebuilt from scratch on every update,
        // so changed trains are refreshed and departed ones drop off.
        let mut per_page: HashMap<usize, Vec<TrainItem>> = HashMap::new();
        let mut update_count = 0;
        let mut failed_count = 0;
        let mut connected_any = false;

        loop {
            tokio::select! {
                event = page_rx.recv() => {
                    match event {
                        Some(PageEvent::Connected) => {
                            connected_any = true;
                            app.lock().await.connection = ConnectionState::Connected;
                            notify.notify_one();
                        }
                        Some(PageEvent::Failed) => {
                            failed_count += 1;
                            if failed_count >= max_pages && !connected_any {
                                debug!("All {} pages failed to connect", max_pages);
                                app.lock().await.connection = ConnectionState::Failed;
                                notify.notify_one();
                            }
                        }
                        Some(PageEvent::Ended(page)) => {
                            // One dead page means silently stale data, so
                            // restart the whole set.
                            debug!("Page {} ended, reconnecting", page);
                            app.lock().await.connection = ConnectionState::Connecting;
                            notify.notify_one();
                            if !backoff_or_reconnect(&mut backoff, update_count, &mut reconnect_rx).await {
                                debug!("Reconnect signalled during backoff");
                            }
                            break;
                        }
                        Some(PageEvent::Update(page, new_items, params)) => {
                            update_count += 1;
                            debug!("Received update #{} from page {}", update_count, page);

                            per_page.insert(page, new_items);

                            let mut merged: Vec<TrainItem> = Vec::new();
                            let mut seen = std::collections::HashSet::new();
                            let mut pages: Vec<_> = per_page.keys().copied().collect();
                            pages.sort_unstable();
                            for p in pages {
                                for item in &per_page[&p] {
                                    if seen.insert(item.id.clone()) {
                                        merged.push(item.clone());
                                    }
                                }
                            }
                            merged.sort_by(|a, b| a.scheduled.cmp(&b.scheduled));

                            let mut app = app.lock().await;
                            debug!("Merged items: {} -> {}", app.items.len(), merged.len());
                            app.items = merged;

                            // Re-sync the selected index from its id after the
                            // sort, so the detail view keeps tracking the right
                            // train as rows shift around.
                            if let Some(id) = app.selected_train_id.clone() {
                                app.selected_train_index =
                                    app.items.iter().position(|i| i.id == id);
                            }

                            if let Some(notices) = params.data.special_notices {
                                app.special_notices = notices;
                            }
                            app.last_update = Some(Local::now());
                            app.connection = ConnectionState::Connected;
                            notify.notify_one();
                        }
                        None => {
                            // Every page socket closed; back off and reconnect.
                            debug!(
                                "All page channels closed after {} updates, reconnecting",
                                update_count
                            );
                            backoff_or_reconnect(&mut backoff, update_count, &mut reconnect_rx).await;
                            break;
                        }
                    }
                }
                _ = reconnect_rx.recv() => {
                    debug!("!!! RECONNECT SIGNAL RECEIVED after {} updates !!!", update_count);
                    backoff = Duration::from_secs(1);
                    break;
                }
            }
        }
    }
}

/// Waits out the (jittered) backoff, but returns early with `false` if a
/// manual reconnect arrives, so pressing R never has to wait for the delay.
/// Returns `true` if the full delay elapsed.
async fn backoff_or_reconnect(
    backoff: &mut Duration,
    update_count: usize,
    reconnect_rx: &mut mpsc::Receiver<()>,
) -> bool {
    if update_count > 0 {
        *backoff = Duration::from_secs(1);
    }
    let delay = jittered(*backoff);
    debug!("Reconnecting in {:?}", delay);
    tokio::select! {
        _ = tokio::time::sleep(delay) => {
            *backoff = (*backoff * 2).min(MAX_BACKOFF);
            true
        }
        _ = reconnect_rx.recv() => {
            *backoff = Duration::from_secs(1);
            false
        }
    }
}
