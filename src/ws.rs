use anyhow::Result;
use chrono::Local;
use futures_util::{SinkExt, StreamExt};
use std::{collections::HashMap, sync::Arc, time::Duration};
use tokio::sync::{mpsc, Mutex, Notify};
use tokio_tungstenite::{connect_async, tungstenite::protocol::Message};

use crate::app::{build_ws_url, App, ConnectionState, ContentType, LinkError};
use crate::model::{TrainItem, UpdateParams, WsMessage};

/// How long a page socket may stay silent before it is considered dead. The
/// server sends something at least every ~30s (keepAlive/update).
const READ_TIMEOUT: Duration = Duration::from_secs(90);
const MAX_BACKOFF: Duration = Duration::from_secs(30);

/// Events emitted by a single per-page WebSocket worker task back to the
/// coordinating loop in [`run_websocket`].
enum PageEvent {
    /// The page's socket connected successfully.
    Connected,
    /// The page failed to connect at all.
    Failed(LinkError),
    /// A connected page socket closed, errored or went silent.
    Ended(usize, LinkError),
    /// The page received a message that could not be parsed.
    Invalid,
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
    reconnect_rx: mpsc::Receiver<()>,
    notify: Arc<Notify>,
) -> Result<()> {
    run_websocket_with(app, reconnect_rx, notify, Arc::new(build_ws_url)).await
}

/// Builds the URL of one page: `(station_id, content_type, page)`.
type UrlBuilder = Arc<dyn Fn(&str, &ContentType, usize) -> String + Send + Sync>;

/// [`run_websocket`] with an injectable URL builder, so tests can point the
/// whole loop at a local server.
async fn run_websocket_with(
    app: Arc<Mutex<App>>,
    mut reconnect_rx: mpsc::Receiver<()>,
    notify: Arc<Notify>,
    url_for: UrlBuilder,
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
            let url = url_for(&station_id, &content_type, page);
            debug!("Spawning task for page {}: {}", page, url);
            let tx = page_tx.clone();
            let page_type = content_type.clone();
            let task = tokio::spawn(run_page(page, url, page_type, tx, READ_TIMEOUT));

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
                        Some(PageEvent::Failed(err)) => {
                            failed_count += 1;
                            let mut app = app.lock().await;
                            app.last_error = Some(err);
                            if failed_count >= max_pages && !connected_any {
                                debug!("All {} pages failed to connect", max_pages);
                                app.connection = ConnectionState::Failed;
                            }
                            notify.notify_one();
                        }
                        Some(PageEvent::Invalid) => {
                            app.lock().await.last_error = Some(LinkError::Parse);
                            notify.notify_one();
                        }
                        Some(PageEvent::Ended(page, err)) => {
                            // One dead page means silently stale data, so
                            // restart the whole set.
                            debug!("Page {} ended ({:?}), reconnecting", page, err);
                            {
                                let mut app = app.lock().await;
                                app.connection = ConnectionState::Connecting;
                                app.last_error = Some(err);
                            }
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

                            let merged = merge_pages(&per_page);

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
                            app.last_error = None;
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

/// Pull the board for `content_type` out of an update payload.
fn extract_items(params: &mut UpdateParams, content_type: &ContentType) -> Vec<TrainItem> {
    match content_type {
        ContentType::Departure => params.data.departures.take(),
        ContentType::Arrival => params.data.arrivals.take(),
    }
    .unwrap_or_default()
}

/// Union of all pages' items, deduplicated by id (lowest page wins) and sorted
/// by scheduled time.
fn merge_pages(per_page: &HashMap<usize, Vec<TrainItem>>) -> Vec<TrainItem> {
    let mut pages: Vec<_> = per_page.keys().copied().collect();
    pages.sort_unstable();
    let mut seen = std::collections::HashSet::new();
    let mut merged: Vec<TrainItem> = pages
        .into_iter()
        .flat_map(|p| per_page[&p].iter())
        .filter(|item| seen.insert(item.id.clone()))
        .cloned()
        .collect();
    merged.sort_by(|a, b| a.scheduled.cmp(&b.scheduled));
    merged
}

/// Drive one page's socket until it closes, errors or stays silent for
/// `read_timeout`, forwarding events to the coordinator.
async fn run_page(
    page: usize,
    url: String,
    content_type: ContentType,
    tx: mpsc::Sender<PageEvent>,
    read_timeout: Duration,
) {
    debug!("Page {} task started, connecting...", page);
    let ws_stream = match connect_async(&url).await {
        Ok((ws_stream, _)) => ws_stream,
        Err(e) => {
            debug!("Page {} failed to connect: {}", page, e);
            let _ = tx
                .send(PageEvent::Failed(LinkError::Connect(e.to_string())))
                .await;
            return;
        }
    };
    debug!("Page {} connected successfully", page);
    let _ = tx.send(PageEvent::Connected).await;
    let (mut write, mut read) = ws_stream.split();
    let mut msg_count = 0;
    let end = loop {
        let msg = match tokio::time::timeout(read_timeout, read.next()).await {
            Ok(Some(msg)) => msg,
            Ok(None) => break LinkError::Closed(String::new()),
            Err(_) => {
                debug!("Page {} timed out after {:?}", page, read_timeout);
                break LinkError::Timeout;
            }
        };
        match msg {
            Ok(Message::Text(text)) => {
                msg_count += 1;
                match serde_json::from_str::<WsMessage>(&text).map(WsMessage::into_update) {
                    Ok(Some(Ok(mut params))) => {
                        // Narrow to the type this socket was opened for,
                        // not whatever the UI has selected now.
                        let items = extract_items(&mut params, &content_type);
                        let _ = tx.send(PageEvent::Update(page, items, params)).await;
                    }
                    // Other methods (loadUrl, keepAlive, ...) are expected noise.
                    Ok(None) => {}
                    Ok(Some(Err(_))) | Err(_) => {
                        debug!("Page {} failed to parse message", page);
                        let _ = tx.send(PageEvent::Invalid).await;
                    }
                }
            }
            Ok(Message::Ping(payload)) => {
                if let Err(e) = write.send(Message::Pong(payload)).await {
                    break LinkError::Closed(e.to_string());
                }
            }
            Ok(Message::Close(reason)) => {
                debug!("Page {} WebSocket closed: {:?}", page, reason);
                break LinkError::Closed(String::new());
            }
            Err(e) => {
                debug!("Page {} WebSocket error: {}", page, e);
                break LinkError::Closed(e.to_string());
            }
            _ => {}
        }
    };
    debug!("Page {} task ending after {} messages", page, msg_count);
    let _ = tx.send(PageEvent::Ended(page, end)).await;
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

#[cfg(test)]
mod tests {

    use super::*;
    use futures_util::{SinkExt, StreamExt};
    use tokio::net::TcpListener;
    use tokio_tungstenite::accept_async;

    const DEPARTURES: &str = include_str!("../tests/fixtures/update_departures.json");
    const ARRIVALS: &str = include_str!("../tests/fixtures/update_arrivals.json");
    const NON_UPDATE: &str = include_str!("../tests/fixtures/non_update.json");

    fn train(id: &str, scheduled: &str) -> TrainItem {
        TrainItem {
            id: id.to_string(),
            train: id.to_string(),
            scheduled: scheduled.to_string(),
            ..Default::default()
        }
    }

    // ---- pure helpers -------------------------------------------------

    #[test]
    fn merge_sorts_by_scheduled_time() {
        let mut pages = HashMap::new();
        pages.insert(1, vec![train("b", "2024-01-01T10:10:00+01:00")]);
        pages.insert(2, vec![train("a", "2024-01-01T10:00:00+01:00")]);
        let ids: Vec<_> = merge_pages(&pages).into_iter().map(|t| t.id).collect();
        assert_eq!(ids, ["a", "b"]);
    }

    #[test]
    fn merge_dedupes_by_id_preferring_lowest_page() {
        let mut first = train("x", "2024-01-01T10:00:00+01:00");
        first.track = Some("1".into());
        let mut second = train("x", "2024-01-01T10:00:00+01:00");
        second.track = Some("2".into());
        let mut pages = HashMap::new();
        pages.insert(2, vec![second]);
        pages.insert(1, vec![first]);
        let merged = merge_pages(&pages);
        assert_eq!(merged.len(), 1);
        assert_eq!(merged[0].track.as_deref(), Some("1"));
    }

    #[test]
    fn merge_reflects_replaced_page_so_changes_and_departures_apply() {
        let mut pages = HashMap::new();
        let mut old = train("x", "2024-01-01T10:00:00+01:00");
        old.expected = None;
        pages.insert(1, vec![old, train("gone", "2024-01-01T10:01:00+01:00")]);
        assert_eq!(merge_pages(&pages).len(), 2);

        // A fresh update for the same page replaces it wholesale.
        let mut updated = train("x", "2024-01-01T10:00:00+01:00");
        updated.expected = Some("2024-01-01T10:09:00+01:00".into());
        pages.insert(1, vec![updated]);
        let merged = merge_pages(&pages);
        assert_eq!(merged.len(), 1);
        assert_eq!(
            merged[0].expected.as_deref(),
            Some("2024-01-01T10:09:00+01:00")
        );
    }

    #[test]
    fn merge_empty() {
        assert!(merge_pages(&HashMap::new()).is_empty());
    }

    #[test]
    fn extract_items_picks_requested_type_only() {
        let msg: WsMessage = serde_json::from_str(DEPARTURES).unwrap();
        let mut params = msg.into_update().unwrap().unwrap();
        assert!(extract_items(&mut params, &ContentType::Arrival).is_empty());
        assert_eq!(extract_items(&mut params, &ContentType::Departure).len(), 2);
    }

    #[test]
    fn jitter_stays_within_25_percent() {
        for _ in 0..50 {
            let d = jittered(Duration::from_secs(8));
            assert!(d >= Duration::from_secs(8) && d <= Duration::from_secs(10));
        }
    }

    #[tokio::test]
    async fn backoff_doubles_and_caps() {
        let (_tx, mut rx) = mpsc::channel(1);
        let mut backoff = Duration::from_millis(1);
        tokio::time::pause();
        assert!(backoff_or_reconnect(&mut backoff, 0, &mut rx).await);
        assert_eq!(backoff, Duration::from_millis(2));
        backoff = MAX_BACKOFF;
        assert!(backoff_or_reconnect(&mut backoff, 0, &mut rx).await);
        assert_eq!(backoff, MAX_BACKOFF);
    }

    #[tokio::test]
    async fn backoff_resets_after_successful_session() {
        let (_tx, mut rx) = mpsc::channel(1);
        tokio::time::pause();
        let mut backoff = Duration::from_secs(16);
        assert!(backoff_or_reconnect(&mut backoff, 3, &mut rx).await);
        assert_eq!(backoff, Duration::from_secs(2));
    }

    #[tokio::test]
    async fn manual_reconnect_interrupts_backoff() {
        let (tx, mut rx) = mpsc::channel(1);
        tx.send(()).await.unwrap();
        let mut backoff = Duration::from_secs(30);
        let started = std::time::Instant::now();
        assert!(!backoff_or_reconnect(&mut backoff, 0, &mut rx).await);
        assert!(started.elapsed() < Duration::from_secs(5));
        assert_eq!(backoff, Duration::from_secs(1));
    }

    // ---- run_page against a local WebSocket server ---------------------

    /// Bind a local server, run `handler` on the first accepted connection,
    /// and return its `ws://` URL.
    async fn serve<F, Fut>(handler: F) -> String
    where
        F: FnOnce(tokio_tungstenite::WebSocketStream<tokio::net::TcpStream>) -> Fut
            + Send
            + 'static,
        Fut: std::future::Future<Output = ()> + Send,
    {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        tokio::spawn(async move {
            let (stream, _) = listener.accept().await.unwrap();
            let ws = accept_async(stream).await.unwrap();
            handler(ws).await;
        });
        format!("ws://{}", addr)
    }

    async fn next_event(rx: &mut mpsc::Receiver<PageEvent>) -> PageEvent {
        tokio::time::timeout(Duration::from_secs(5), rx.recv())
            .await
            .expect("timed out waiting for event")
            .expect("channel closed")
    }

    #[tokio::test]
    async fn page_forwards_updates_then_reports_end_on_close() {
        let url = serve(|mut ws| async move {
            ws.send(Message::text(NON_UPDATE)).await.unwrap(); // ignored
            ws.send(Message::text("garbage")).await.unwrap(); // ignored
            ws.send(Message::text(DEPARTURES)).await.unwrap();
            ws.close(None).await.unwrap();
        })
        .await;

        let (tx, mut rx) = mpsc::channel(10);
        tokio::spawn(run_page(
            3,
            url,
            ContentType::Departure,
            tx,
            Duration::from_secs(5),
        ));

        assert!(matches!(next_event(&mut rx).await, PageEvent::Connected));
        assert!(matches!(next_event(&mut rx).await, PageEvent::Invalid));
        match next_event(&mut rx).await {
            PageEvent::Update(page, items, params) => {
                assert_eq!(page, 3);
                assert_eq!(items.len(), 2);
                assert_eq!(params.data.special_notices.unwrap().len(), 1);
            }
            _ => panic!("expected Update"),
        }
        assert!(matches!(
            next_event(&mut rx).await,
            PageEvent::Ended(3, LinkError::Closed(_))
        ));
    }

    #[tokio::test]
    async fn page_uses_its_own_content_type_not_the_payload_mix() {
        // Departure payload read by an arrival socket yields no items.
        let url = serve(|mut ws| async move {
            ws.send(Message::text(DEPARTURES)).await.unwrap();
            ws.send(Message::text(ARRIVALS)).await.unwrap();
            ws.close(None).await.unwrap();
        })
        .await;
        let (tx, mut rx) = mpsc::channel(10);
        tokio::spawn(run_page(
            1,
            url,
            ContentType::Arrival,
            tx,
            Duration::from_secs(5),
        ));

        assert!(matches!(next_event(&mut rx).await, PageEvent::Connected));
        let PageEvent::Update(_, first, _) = next_event(&mut rx).await else {
            panic!("expected Update")
        };
        assert!(first.is_empty());
        let PageEvent::Update(_, second, _) = next_event(&mut rx).await else {
            panic!("expected Update")
        };
        assert_eq!(second[0].id, "a1");
    }

    #[tokio::test]
    async fn page_answers_ping_with_matching_pong() {
        let (pong_tx, pong_rx) = tokio::sync::oneshot::channel();
        let url = serve(|mut ws| async move {
            ws.send(Message::Ping(b"hello".to_vec().into()))
                .await
                .unwrap();
            while let Some(Ok(msg)) = ws.next().await {
                if let Message::Pong(p) = msg {
                    let _ = pong_tx.send(p.to_vec());
                    break;
                }
            }
            let _ = ws.close(None).await;
        })
        .await;
        let (tx, mut rx) = mpsc::channel(10);
        tokio::spawn(run_page(
            1,
            url,
            ContentType::Departure,
            tx,
            Duration::from_secs(5),
        ));

        let payload = tokio::time::timeout(Duration::from_secs(5), pong_rx)
            .await
            .expect("no pong received")
            .unwrap();
        assert_eq!(payload, b"hello");
        // Drain until the page ends so the task finishes cleanly.
        while !matches!(next_event(&mut rx).await, PageEvent::Ended(..)) {}
    }

    #[tokio::test]
    async fn silent_socket_times_out_and_ends() {
        let url = serve(|ws| async move {
            // Hold the connection open without sending anything.
            tokio::time::sleep(Duration::from_secs(10)).await;
            drop(ws);
        })
        .await;
        let (tx, mut rx) = mpsc::channel(10);
        tokio::spawn(run_page(
            2,
            url,
            ContentType::Departure,
            tx,
            Duration::from_millis(150),
        ));

        assert!(matches!(next_event(&mut rx).await, PageEvent::Connected));
        assert!(matches!(
            next_event(&mut rx).await,
            PageEvent::Ended(2, LinkError::Timeout)
        ));
    }

    #[tokio::test]
    async fn connect_failure_reports_failed() {
        // Bind then drop to get a port nothing is listening on.
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        drop(listener);

        let (tx, mut rx) = mpsc::channel(10);
        tokio::spawn(run_page(
            1,
            format!("ws://{}", addr),
            ContentType::Departure,
            tx,
            Duration::from_secs(1),
        ));
        assert!(matches!(
            next_event(&mut rx).await,
            PageEvent::Failed(LinkError::Connect(_))
        ));
    }

    // ---- the whole loop against a local server ------------------------

    use std::sync::atomic::{AtomicUsize, Ordering};

    const DEPARTURES_CHANGED: &str = r#"{"method":"update","params":{"data":{"departures":[
        {"id":"rj-65-20240101-1000","train":"RJX 65","scheduled":"2024-01-01T10:00:00+01:00",
         "expected":"2024-01-01T10:25:00+01:00"}]}}}"#;

    /// What the local server does with each accepted connection.
    #[derive(Clone, Copy)]
    enum Script {
        /// Send the board for the requested content type, then idle.
        Board,
        /// Send the board, then replace it with a changed single-train board.
        BoardThenChange,
        /// Close immediately, but only for the first `n` connections.
        DropFirst(usize),
    }

    /// Serve connections forever; returns the base URL and a connection counter.
    async fn serve_loop(script: Script) -> (String, Arc<AtomicUsize>) {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let count = Arc::new(AtomicUsize::new(0));
        let seen = count.clone();
        tokio::spawn(async move {
            loop {
                let (stream, _) = listener.accept().await.unwrap();
                let nth = seen.fetch_add(1, Ordering::SeqCst);
                tokio::spawn(async move {
                    let mut uri = String::new();
                    let ws = tokio_tungstenite::accept_hdr_async(
                        stream,
                        |req: &tokio_tungstenite::tungstenite::handshake::server::Request, resp| {
                            uri = req.uri().to_string();
                            Ok(resp)
                        },
                    )
                    .await;
                    let Ok(mut ws) = ws else { return };
                    let board = if uri.contains("contentType=arrival") {
                        ARRIVALS
                    } else {
                        DEPARTURES
                    };
                    match script {
                        Script::DropFirst(n) if nth < n => {
                            let _ = ws.close(None).await;
                            return;
                        }
                        Script::BoardThenChange => {
                            let _ = ws.send(Message::text(board)).await;
                            tokio::time::sleep(Duration::from_millis(150)).await;
                            let _ = ws.send(Message::text(DEPARTURES_CHANGED)).await;
                        }
                        _ => {
                            let _ = ws.send(Message::text(board)).await;
                        }
                    }
                    // Keep the connection open until the client goes away.
                    while ws.next().await.is_some() {}
                });
            }
        });
        (format!("ws://{}", addr), count)
    }

    struct Loop {
        app: Arc<Mutex<App>>,
        reconnect: mpsc::Sender<()>,
        task: tokio::task::JoinHandle<()>,
    }

    impl Drop for Loop {
        fn drop(&mut self) {
            self.task.abort();
        }
    }

    fn start_loop(base: String) -> Loop {
        let mut app = App::new();
        app.max_pages = 2;
        let app = Arc::new(Mutex::new(app));
        let (reconnect, rx) = mpsc::channel(10);
        let url_for: UrlBuilder = Arc::new(move |_station, ct, page| {
            let ct = match ct {
                ContentType::Departure => "departure",
                ContentType::Arrival => "arrival",
            };
            format!("{base}/?contentType={ct}&page={page}")
        });
        let task = tokio::spawn({
            let app = app.clone();
            async move {
                let _ = run_websocket_with(app, rx, Arc::new(Notify::new()), url_for).await;
            }
        });
        Loop {
            app,
            reconnect,
            task,
        }
    }

    /// Poll the app state until `pred` holds, or fail after 10 seconds.
    async fn wait_for(app: &Arc<Mutex<App>>, what: &str, pred: impl Fn(&App) -> bool) {
        let deadline = tokio::time::Instant::now() + Duration::from_secs(10);
        loop {
            if pred(&*app.lock().await) {
                return;
            }
            assert!(tokio::time::Instant::now() < deadline, "timed out: {what}");
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
    }

    #[tokio::test]
    async fn loop_publishes_board_and_notices() {
        let (base, count) = serve_loop(Script::Board).await;
        let l = start_loop(base);
        wait_for(&l.app, "items", |a| a.items.len() == 2).await;
        let app = l.app.lock().await;
        assert_eq!(app.connection, ConnectionState::Connected);
        assert_eq!(app.special_notices.len(), 1);
        assert!(app.last_update.is_some() && app.last_error.is_none());
        assert_eq!(app.items[0].id, "rj-65-20240101-1000", "sorted by time");
        drop(app);
        assert_eq!(count.load(Ordering::SeqCst), 2, "one socket per page");
    }

    #[tokio::test]
    async fn loop_applies_changes_to_trains_already_on_the_board() {
        let (base, _) = serve_loop(Script::BoardThenChange).await;
        let l = start_loop(base);
        wait_for(&l.app, "initial board", |a| a.items.len() == 2).await;
        wait_for(&l.app, "changed expected time", |a| {
            a.items
                .iter()
                .any(|i| i.expected.as_deref() == Some("2024-01-01T10:25:00+01:00"))
        })
        .await;
    }

    #[tokio::test]
    async fn loop_switching_to_arrivals_replaces_the_board() {
        let (base, _) = serve_loop(Script::Board).await;
        let l = start_loop(base);
        wait_for(&l.app, "departures", |a| a.items.len() == 2).await;

        l.app.lock().await.content_type = ContentType::Arrival;
        l.reconnect.send(()).await.unwrap();
        wait_for(&l.app, "arrivals", |a| {
            a.items.len() == 1 && a.items[0].id == "a1"
        })
        .await;
    }

    #[tokio::test]
    async fn loop_reconnects_after_the_server_drops_it() {
        // Both first connections (one per page) are closed straight away.
        let (base, count) = serve_loop(Script::DropFirst(2)).await;
        let l = start_loop(base);
        wait_for(&l.app, "recovered board", |a| {
            a.items.len() == 2 && a.connection == ConnectionState::Connected
        })
        .await;
        assert!(count.load(Ordering::SeqCst) >= 4);
        assert!(
            l.app.lock().await.last_error.is_none(),
            "cleared on success"
        );
    }

    #[tokio::test]
    async fn loop_reports_failed_when_nothing_is_listening() {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        drop(listener);
        let l = start_loop(format!("ws://{}", addr));
        wait_for(&l.app, "failed state", |a| {
            a.connection == ConnectionState::Failed
                && matches!(a.last_error, Some(LinkError::Connect(_)))
        })
        .await;
    }

    #[tokio::test]
    async fn loop_manual_reconnect_opens_fresh_sockets() {
        let (base, count) = serve_loop(Script::Board).await;
        let l = start_loop(base);
        wait_for(&l.app, "board", |a| a.items.len() == 2).await;
        l.reconnect.send(()).await.unwrap();
        let deadline = tokio::time::Instant::now() + Duration::from_secs(10);
        while count.load(Ordering::SeqCst) < 4 {
            assert!(tokio::time::Instant::now() < deadline, "no new sockets");
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
    }

    /// Talks to the real ÖBB endpoint (TLS, handshake, payload shape).
    /// Run with `cargo test -- --ignored live`.
    #[tokio::test]
    #[ignore = "needs network access to meine.oebb.at"]
    async fn live_endpoint_delivers_a_departure_board() {
        let url = build_ws_url("8101001", &ContentType::Departure, 1);
        let (tx, mut rx) = mpsc::channel(10);
        let task = tokio::spawn(run_page(
            1,
            url,
            ContentType::Departure,
            tx,
            Duration::from_secs(20),
        ));

        let event = tokio::time::timeout(Duration::from_secs(20), rx.recv())
            .await
            .expect("no event within 20s")
            .unwrap();
        assert!(
            matches!(event, PageEvent::Connected),
            "TLS/handshake failed"
        );
        loop {
            match tokio::time::timeout(Duration::from_secs(20), rx.recv()).await {
                Ok(Some(PageEvent::Update(_, items, _))) => {
                    assert!(!items.is_empty(), "empty departure board");
                    break;
                }
                Ok(Some(PageEvent::Invalid)) => panic!("payload no longer parses"),
                Ok(Some(_)) => {}
                _ => panic!("no update within 20s"),
            }
        }
        task.abort();
    }
}
