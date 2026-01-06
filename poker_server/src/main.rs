mod game;
mod server;

use crate::server::PokerServer;
use futures::stream::StreamExt;
use futures::SinkExt;
use log::{debug, error, info, warn};
use poker_protocol::{ClientMessage, ServerMessage};
use std::net::SocketAddr;
use std::sync::{Arc, Mutex};
use tokio::net::TcpListener;
use tokio_tungstenite::accept_async;
use tokio_tungstenite::tungstenite::Message;
use uuid::Uuid;

use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{Duration, Instant};

struct RateLimiter {
    messages: AtomicU64,
    window_start: AtomicU64,
}

impl RateLimiter {
    const MAX_MESSAGES: u64 = 100;
    const WINDOW_MS: u64 = 1000;

    fn new() -> Self {
        Self {
            messages: AtomicU64::new(0),
            window_start: AtomicU64::new(0),
        }
    }

    fn allow(&self) -> bool {
        let now_ms = Instant::now().elapsed().as_millis() as u64;
        loop {
            let window_start = self.window_start.load(Ordering::Relaxed);
            let elapsed = now_ms.saturating_sub(window_start);

            if elapsed > Self::WINDOW_MS {
                if self
                    .window_start
                    .compare_exchange(window_start, now_ms, Ordering::Relaxed, Ordering::Relaxed)
                    .is_ok()
                {
                    self.messages.store(0, Ordering::Relaxed);
                }
            }

            let current = self.messages.load(Ordering::Relaxed);
            if current >= Self::MAX_MESSAGES {
                return false;
            }

            if self
                .messages
                .compare_exchange(current, current + 1, Ordering::Relaxed, Ordering::Relaxed)
                .is_ok()
            {
                return true;
            }
        }
    }
}

impl Default for RateLimiter {
    fn default() -> Self {
        Self::new()
    }
}

fn validate_action_amount(amount: i64, max_allowed: i32) -> Result<i32, String> {
    if amount <= 0 {
        return Err("Amount must be positive".to_string());
    }
    if amount > max_allowed as i64 {
        return Err(format!("Amount exceeds maximum allowed: {}", max_allowed));
    }
    if amount > i32::MAX as i64 {
        return Err("Amount too large".to_string());
    }
    Ok(amount as i32)
}

const MAX_MESSAGE_SIZE: usize = 4096;
const MAX_PLAYER_CHIPS: i32 = 1000000;
const STARTING_CHIPS: i32 = 1000;
const DEFAULT_SMALL_BLIND: i32 = 5;
const DEFAULT_BIG_BLIND: i32 = 10;
const CHANNEL_CAPACITY: usize = 100;
const CONNECTION_TIMEOUT_MS: u64 = 30000;
const INACTIVITY_TIMEOUT_MS: u64 = 600000;

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    env_logger::init();

    let server = Arc::new(Mutex::new(PokerServer::new()));

    let addr = "127.0.0.1:8080";
    let listener = TcpListener::bind(addr).await?;
    info!("Poker server listening on: {}", addr);

    let game = {
        let mut server_guard = server.lock().map_err(|_| "Failed to lock server")?;
        server_guard.create_game(
            "main_table".to_string(),
            DEFAULT_SMALL_BLIND,
            DEFAULT_BIG_BLIND,
        )
    };

    let broadcast_task = {
        let server = Arc::clone(&server);
        let mut rx = game
            .lock()
            .map_err(|_| "Failed to lock game")?
            .tx
            .subscribe();
        tokio::spawn(async move {
            while let Ok(msg) = rx.recv().await {
                if let Ok(s) = server.lock() {
                    s.broadcast_to_game("main_table", msg);
                }
            }
        })
    };

    let game_clone = Arc::clone(&game);
    tokio::spawn(async move {
        loop {
            tokio::time::sleep(tokio::time::Duration::from_secs(5)).await;
            let active_count = {
                if let Ok(g) = game_clone.lock() {
                    g.players.values().filter(|p| !p.is_sitting_out).count()
                } else {
                    0
                }
            };

            if active_count < 2 {
                if let Ok(mut g) = game_clone.lock() {
                    if !g.players.is_empty() {
                        g.game_stage = poker_protocol::GameStage::WaitingForPlayers;
                    }
                }
            }
        }
    });

    while let Ok((stream, addr)) = listener.accept().await {
        info!("New client connected: {}", addr);

        let server = Arc::clone(&server);
        let player_id = Uuid::new_v4().to_string();

        tokio::spawn(async move {
            if let Err(e) =
                handle_connection(stream, addr, Arc::clone(&server), player_id.clone()).await
            {
                error!("Error handling connection: {}", e);
            }

            if let Ok(mut s) = server.lock() {
                s.disconnect_player(&player_id);
            }
        });
    }

    broadcast_task.await?;
    Ok(())
}

fn sanitize_player_name(name: &str) -> String {
    let mut result = String::with_capacity(name.len());
    for c in name.chars() {
        if c.is_alphanumeric() || c == '_' || c == '-' {
            result.push(c);
        } else if result.is_empty() {
            result.push('_');
        }
    }
    if result.is_empty() {
        result.push_str("Player");
    }
    if result.len() > 20 {
        result.truncate(20);
    }
    result
}

fn sanitize_chat_message(text: &str) -> String {
    let max_len = 500;
    let mut result = String::with_capacity(text.len().min(max_len));
    for c in text.chars().take(max_len) {
        if c.is_control() {
            result.push(' ');
        } else {
            result.push(c);
        }
    }
    result.trim().to_string()
}

async fn handle_connection(
    stream: tokio::net::TcpStream,
    _addr: SocketAddr,
    server: Arc<Mutex<PokerServer>>,
    player_id: String,
) -> Result<(), Box<dyn std::error::Error>> {
    let ws_stream = accept_async(stream).await?;
    debug!("WebSocket handshake completed for player: {}", player_id);

    let (write, read) = ws_stream.split();

    let rate_limiter = Arc::new(RateLimiter::new());

    let (tx, mut rx) = tokio::sync::mpsc::channel::<String>(CHANNEL_CAPACITY);
    let write_handle = tokio::spawn(async move {
        let mut sink = write;
        while let Some(msg) = rx.recv().await {
            if let Err(e) = sink.send(Message::Text(msg)).await {
                error!("Failed to send message: {}", e);
                break;
            }
        }
    });

    {
        let mut s = server.lock().map_err(|_| "Failed to lock server")?;
        let sanitized_name = sanitize_player_name(&format!("Player{}", &player_id[..8]));
        s.register_player(player_id.clone(), sanitized_name, STARTING_CHIPS);
        s.connect_player(&player_id, tx);
    }

    let server_for_read = Arc::clone(&server);
    let player_id_clone = player_id.clone();
    let rate_limiter_clone = Arc::clone(&rate_limiter);

    let read_task = tokio::spawn(async move {
        let mut stream = read;
        let mut last_activity = Instant::now();

        while let Some(result) = stream.next().await {
            if last_activity.elapsed() > Duration::from_millis(INACTIVITY_TIMEOUT_MS) {
                warn!("Player {} timed out due to inactivity", player_id_clone);
                break;
            }

            match result {
                Ok(Message::Text(text)) => {
                    last_activity = Instant::now();

                    if !rate_limiter_clone.allow() {
                        warn!("Player {} exceeded rate limit", player_id_clone);
                        let error_msg = ServerMessage::Error("Rate limit exceeded".to_string());
                        if let Ok(json) = serde_json::to_string(&error_msg) {
                            if let Ok(mut server) = server_for_read.lock() {
                                server.send_to_player(&player_id_clone, json);
                            }
                        }
                        break;
                    }

                    if text.len() > MAX_MESSAGE_SIZE {
                        warn!(
                            "Message from {} too large: {} bytes",
                            player_id_clone,
                            text.len()
                        );
                        let error_msg = ServerMessage::Error("Message too large".to_string());
                        if let Ok(json) = serde_json::to_string(&error_msg) {
                            if let Ok(mut server) = server_for_read.lock() {
                                server.send_to_player(&player_id_clone, json);
                            }
                        }
                        break;
                    }
                    debug!("Received from {}: {}", player_id_clone, text);

                    if let Ok(value) = serde_json::from_str::<serde_json::Value>(&text) {
                        if let Some(type_obj) = value.get("type") {
                            if let Some(type_str) = type_obj.as_str() {
                                match type_str {
                                    "connect" => {
                                        if let Ok(mut server) = server_for_read.lock() {
                                            if let Err(e) = server.handle_message(
                                                &player_id_clone,
                                                ClientMessage::Connect,
                                            ) {
                                                let error_msg = ServerMessage::Error(e);
                                                if let Ok(json) = serde_json::to_string(&error_msg)
                                                {
                                                    server.send_to_player(&player_id_clone, json);
                                                }
                                            }
                                        }
                                    }
                                    "action" => {
                                        if let Some(action_str) = value["action"].as_str() {
                                            if !rate_limiter_clone.allow() {
                                                warn!(
                                                    "Player {} action rate limited",
                                                    player_id_clone
                                                );
                                                break;
                                            }
                                            if let Some(action) =
                                                poker_protocol::PlayerAction::from_str(action_str)
                                            {
                                                if let Ok(mut server) = server_for_read.lock() {
                                                    if let Err(e) = server.handle_message(
                                                        &player_id_clone,
                                                        ClientMessage::Action(action),
                                                    ) {
                                                        let error_msg = ServerMessage::Error(e);
                                                        if let Ok(json) =
                                                            serde_json::to_string(&error_msg)
                                                        {
                                                            server.send_to_player(
                                                                &player_id_clone,
                                                                json,
                                                            );
                                                        }
                                                    }
                                                }
                                            } else {
                                                warn!("Unknown action: {}", action_str);
                                            }
                                        } else if let Some(amount_value) =
                                            value["action"]["Bet"].as_i64()
                                        {
                                            if !rate_limiter_clone.allow() {
                                                warn!(
                                                    "Player {} bet rate limited",
                                                    player_id_clone
                                                );
                                                break;
                                            }
                                            match validate_action_amount(
                                                amount_value,
                                                MAX_PLAYER_CHIPS,
                                            ) {
                                                Ok(amount) => {
                                                    if let Ok(mut server) = server_for_read.lock() {
                                                        if let Err(e) = server.handle_message(
                                                            &player_id_clone,
                                                            ClientMessage::Action(
                                                                poker_protocol::PlayerAction::Bet(
                                                                    amount,
                                                                ),
                                                            ),
                                                        ) {
                                                            let error_msg = ServerMessage::Error(e);
                                                            if let Ok(json) =
                                                                serde_json::to_string(&error_msg)
                                                            {
                                                                server.send_to_player(
                                                                    &player_id_clone,
                                                                    json,
                                                                );
                                                            }
                                                        }
                                                    }
                                                }
                                                Err(err_msg) => {
                                                    let error_msg = ServerMessage::Error(err_msg);
                                                    if let Ok(json) =
                                                        serde_json::to_string(&error_msg)
                                                    {
                                                        if let Ok(mut server) =
                                                            server_for_read.lock()
                                                        {
                                                            server.send_to_player(
                                                                &player_id_clone,
                                                                json,
                                                            );
                                                        }
                                                    }
                                                }
                                            }
                                        } else if let Some(amount_value) =
                                            value["action"]["Raise"].as_i64()
                                        {
                                            if !rate_limiter_clone.allow() {
                                                warn!(
                                                    "Player {} raise rate limited",
                                                    player_id_clone
                                                );
                                                break;
                                            }
                                            match validate_action_amount(
                                                amount_value,
                                                MAX_PLAYER_CHIPS,
                                            ) {
                                                Ok(amount) => {
                                                    if let Ok(mut server) = server_for_read.lock() {
                                                        if let Err(e) = server.handle_message(
                                                            &player_id_clone,
                                                            ClientMessage::Action(
                                                                poker_protocol::PlayerAction::Raise(
                                                                    amount,
                                                                ),
                                                            ),
                                                        ) {
                                                            let error_msg = ServerMessage::Error(e);
                                                            if let Ok(json) =
                                                                serde_json::to_string(&error_msg)
                                                            {
                                                                server.send_to_player(
                                                                    &player_id_clone,
                                                                    json,
                                                                );
                                                            }
                                                        }
                                                    }
                                                }
                                                Err(err_msg) => {
                                                    let error_msg = ServerMessage::Error(err_msg);
                                                    if let Ok(json) =
                                                        serde_json::to_string(&error_msg)
                                                    {
                                                        if let Ok(mut server) =
                                                            server_for_read.lock()
                                                        {
                                                            server.send_to_player(
                                                                &player_id_clone,
                                                                json,
                                                            );
                                                        }
                                                    }
                                                }
                                            }
                                        }
                                    }
                                    "chat" => {
                                        if let Some(chat_text) = value["text"].as_str() {
                                            let sanitized_text = sanitize_chat_message(chat_text);
                                            if let Ok(mut server) = server_for_read.lock() {
                                                if let Err(e) = server.handle_message(
                                                    &player_id_clone,
                                                    ClientMessage::Chat(sanitized_text),
                                                ) {
                                                    let error_msg = ServerMessage::Error(e);
                                                    if let Ok(json) =
                                                        serde_json::to_string(&error_msg)
                                                    {
                                                        server
                                                            .send_to_player(&player_id_clone, json);
                                                    }
                                                }
                                            }
                                        }
                                    }
                                    "sit_out" => {
                                        if let Ok(mut server) = server_for_read.lock() {
                                            if let Err(e) = server.handle_message(
                                                &player_id_clone,
                                                ClientMessage::SitOut,
                                            ) {
                                                let error_msg = ServerMessage::Error(e);
                                                if let Ok(json) = serde_json::to_string(&error_msg)
                                                {
                                                    server.send_to_player(&player_id_clone, json);
                                                }
                                            }
                                        }
                                    }
                                    "return" => {
                                        if let Ok(mut server) = server_for_read.lock() {
                                            if let Err(e) = server.handle_message(
                                                &player_id_clone,
                                                ClientMessage::Return,
                                            ) {
                                                let error_msg = ServerMessage::Error(e);
                                                if let Ok(json) = serde_json::to_string(&error_msg)
                                                {
                                                    server.send_to_player(&player_id_clone, json);
                                                }
                                            }
                                        }
                                    }
                                    _ => {
                                        warn!("Unknown message type: {}", type_str);
                                    }
                                }
                            }
                        }
                    } else if let Ok(client_msg) = serde_json::from_str::<ClientMessage>(&text) {
                        if let Ok(mut server) = server_for_read.lock() {
                            if let Err(e) = server.handle_message(&player_id_clone, client_msg) {
                                let error_msg = ServerMessage::Error(e);
                                if let Ok(json) = serde_json::to_string(&error_msg) {
                                    server.send_to_player(&player_id_clone, json);
                                }
                            }
                        }
                    }
                }
                Ok(Message::Close(_)) => {
                    debug!("Client {} disconnected", player_id_clone);
                    break;
                }
                Err(e) => {
                    error!("WebSocket error: {}", e);
                    break;
                }
                _ => {}
            }
        }
    });

    read_task.await?;
    drop(write_handle);

    Ok(())
}
