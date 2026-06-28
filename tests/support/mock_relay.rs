#![allow(dead_code, reason = "support utilities used by future integration tests")]

use futures::{SinkExt, StreamExt};
use nostr::filter::MatchEventOptions;
use nostr::{Event, Filter, JsonUtil};
use serde_json::{Value, json};
use std::collections::HashMap;
use std::net::SocketAddr;
use std::sync::Arc;
use tokio::net::TcpListener;
use tokio::sync::{Mutex, RwLock, broadcast, mpsc};
use tokio::task::JoinHandle;
use tokio_tungstenite::accept_async;
use tokio_tungstenite::tungstenite::Message as WsMessage;

/// A lightweight in-process Nostr relay for integration tests.
///
/// The relay accepts WebSocket connections, stores published events in memory,
/// and forwards matching events to active REQ subscriptions. Tests can also
/// inject events directly via [`MockRelay::inject_event`].
#[derive(Clone)]
pub struct MockRelay {
    inner: Arc<MockRelayInner>,
}

struct MockRelayInner {
    addr: SocketAddr,
    events: RwLock<Vec<Event>>,
    new_event_tx: broadcast::Sender<Event>,
    shutdown_tx: Mutex<Option<mpsc::Sender<()>>>,
    handle: Mutex<Option<JoinHandle<()>>>,
}

impl MockRelay {
    /// Start a new mock relay on a random localhost port.
    pub async fn start() -> Result<Self, Box<dyn std::error::Error>> {
        let listener = TcpListener::bind("127.0.0.1:0").await?;
        let addr = listener.local_addr()?;

        let (new_event_tx, _new_event_rx) = broadcast::channel(256);
        let (shutdown_tx, mut shutdown_rx) = mpsc::channel(1);
        let inner = Arc::new(MockRelayInner {
            addr,
            events: RwLock::new(Vec::new()),
            new_event_tx,
            shutdown_tx: Mutex::new(Some(shutdown_tx)),
            handle: Mutex::new(None),
        });

        let relay = Self {
            inner: Arc::clone(&inner),
        };

        let inner_for_listener = Arc::clone(&inner);
        let handle = tokio::spawn(async move {
            loop {
                tokio::select! {
                    _ = shutdown_rx.recv() => break,
                    accept_result = listener.accept() => {
                        match accept_result {
                            Ok((stream, _)) => {
                                let relay_clone = MockRelay { inner: Arc::clone(&inner_for_listener) };
                                tokio::spawn(async move {
                                    let _ = relay_clone.handle_connection(stream).await;
                                });
                            }
                            Err(e) => {
                                eprintln!("mock relay accept error: {e}");
                            }
                        }
                    }
                }
            }
        });

        *inner.handle.lock().await = Some(handle);
        Ok(relay)
    }

    /// Return the WebSocket URL for this relay.
    pub fn url(&self) -> String {
        format!("ws://{}", self.inner.addr)
    }

    /// Return a copy of all events stored by the relay.
    pub async fn events(&self) -> Vec<Event> {
        self.inner.events.read().await.clone()
    }

    /// Inject an event into the relay and forward it to matching subscribers.
    pub async fn inject_event(&self, event: Event) {
        self.store_event(&event).await;
        let _ = self.inner.new_event_tx.send(event);
    }

    /// Wait until at least one event matching the predicate is stored, then
    /// return all stored events.
    pub async fn wait_for_event<F>(
        &self,
        predicate: F,
        timeout: std::time::Duration,
    ) -> Result<Vec<Event>, Box<dyn std::error::Error>>
    where
        F: Fn(&Event) -> bool,
    {
        let deadline = tokio::time::Instant::now() + timeout;
        loop {
            {
                let events = self.inner.events.read().await;
                if events.iter().any(&predicate) {
                    return Ok(events.clone());
                }
            }
            if tokio::time::Instant::now() >= deadline {
                return Err("timeout waiting for relay event".into());
            }
            tokio::time::sleep(std::time::Duration::from_millis(10)).await;
        }
    }

    /// Stop the relay and wait for the listener task to finish.
    pub async fn stop(self) {
        if let Some(tx) = self.inner.shutdown_tx.lock().await.take() {
            let _ = tx.send(()).await;
        }
        if let Some(handle) = self.inner.handle.lock().await.take() {
            let _ = handle.await;
        }
    }

    async fn handle_connection(
        &self,
        stream: tokio::net::TcpStream,
    ) -> Result<(), Box<dyn std::error::Error>> {
        let ws_stream = accept_async(stream).await?;
        let mut new_event_rx = self.inner.new_event_tx.subscribe();
        let (mut ws_tx, mut ws_rx) = ws_stream.split();
        let (out_tx, mut out_rx) = mpsc::unbounded_channel::<Value>();

        let mut subscriptions: HashMap<String, Filter> = HashMap::new();

        loop {
            tokio::select! {
                Some(msg) = ws_rx.next() => {
                    match msg {
                        Ok(WsMessage::Text(text)) => {
                            self.process_client_message(&text, &out_tx, &mut subscriptions).await;
                        }
                        Ok(WsMessage::Close(_)) | Err(_) => break,
                        _ => {}
                    }
                }
                Ok(event) = new_event_rx.recv() => {
                    for (sub_id, filter) in &subscriptions {
                        if filter.match_event(&event, MatchEventOptions::new()) {
                            let _ = out_tx.send(json!(["EVENT", sub_id, serde_json::to_value(&event).unwrap_or(Value::Null)]));
                        }
                    }
                }
                Some(msg) = out_rx.recv() => {
                    if ws_tx.send(WsMessage::Text(msg.to_string().into())).await.is_err() {
                        break;
                    }
                }
            }
        }

        Ok(())
    }

    async fn process_client_message(
        &self,
        text: &str,
        out_tx: &mpsc::UnboundedSender<Value>,
        subscriptions: &mut HashMap<String, Filter>,
    ) {
        let parsed: Value = match serde_json::from_str(text) {
            Ok(v) => v,
            Err(_) => return,
        };

        let arr = match parsed.as_array() {
            Some(a) if !a.is_empty() => a,
            _ => return,
        };

        let cmd = match arr[0].as_str() {
            Some(s) => s,
            None => return,
        };

        match cmd {
            "EVENT" => {
                if let Some(event_value) = arr.get(1) {
                    if let Ok(event) = Event::from_json(event_value.to_string()) {
                        self.store_event(&event).await;
                        let _ = self.inner.new_event_tx.send(event);
                    }
                }
            }
            "REQ" => {
                if arr.len() < 3 {
                    return;
                }
                let sub_id = match arr[1].as_str() {
                    Some(s) => s.to_string(),
                    None => return,
                };
                let filter_value = arr[2].clone();
                let filter: Filter = match Filter::from_json(filter_value.to_string()) {
                    Ok(f) => f,
                    Err(_) => return,
                };

                subscriptions.insert(sub_id.clone(), filter.clone());

                // Send matching stored events.
                let events = self.inner.events.read().await.clone();
                for event in events {
                    if filter.match_event(&event, MatchEventOptions::new()) {
                        let _ = out_tx.send(json!([
                            "EVENT",
                            sub_id.clone(),
                            serde_json::to_value(&event).unwrap_or(Value::Null)
                        ]));
                    }
                }
                let _ = out_tx.send(json!(["EOSE", sub_id]));
            }
            "CLOSE" => {
                if let Some(sub_id) = arr.get(1).and_then(Value::as_str) {
                    subscriptions.remove(sub_id);
                }
            }
            _ => {}
        }
    }

    async fn store_event(&self, event: &Event) {
        self.inner.events.write().await.push(event.clone());
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use nostr::{EventBuilder, Keys};

    #[tokio::test]
    async fn relay_stores_and_returns_events() -> Result<(), Box<dyn std::error::Error>> {
        let relay = MockRelay::start().await?;
        let keys = Keys::generate();
        let event = EventBuilder::text_note("hello").sign(&keys).await?;

        relay.inject_event(event.clone()).await;
        let events = relay.events().await;
        assert_eq!(events.len(), 1);
        assert_eq!(events[0].content, "hello");
        Ok(())
    }
}
