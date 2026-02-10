mod game;
mod server;

use futures::stream::StreamExt;
use futures::SinkExt;
use log::{debug, error, info, warn};
use parking_lot::Mutex;
use poker_protocol::{ClientMessage, HmacKey, NonceCache, ServerMessage, HMAC_SECRET_LEN};
use rand::prelude::SliceRandom;
use std::net::SocketAddr;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::Duration;
use tokio::net::TcpListener;
use tokio::signal;
use tokio::time::Instant;
use tokio_tungstenite::accept_async;
use tokio_tungstenite::tungstenite::Message;
use uuid::Uuid;

use crate::game::PokerGame;
use crate::server::PokerServer;

pub const SHUTDOWN_TIMEOUT_SECS: u64 = 5;
/// Default server bind address
pub const DEFAULT_SERVER_ADDR: &str = "127.0.0.1:8080";
/// Environment variable for server bind address
pub const ENV_SERVER_ADDR: &str = "POKER_SERVER_ADDR";

pub struct TokenBucketRateLimiter {
    tokens: std::sync::atomic::AtomicU64,
    last_update_ms: std::sync::atomic::AtomicU64,
    max_tokens: u64,
    refill_rate: u64,
}

impl TokenBucketRateLimiter {
    pub fn new(max_tokens: u64, refill_rate: u64) -> Self {
        let now_ms = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map_or(0, |d| d.as_millis() as u64);
        Self {
            tokens: std::sync::atomic::AtomicU64::new(max_tokens),
            last_update_ms: std::sync::atomic::AtomicU64::new(now_ms),
            max_tokens,
            refill_rate,
        }
    }

    pub fn allow(&self) -> bool {
        let now_ms = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map_or(0, |d| d.as_millis() as u64);

        loop {
            let tokens = self.tokens.load(std::sync::atomic::Ordering::Relaxed);
            let last_update_ms = self
                .last_update_ms
                .load(std::sync::atomic::Ordering::Relaxed);

            let elapsed_ms = now_ms.saturating_sub(last_update_ms);
            let refill = elapsed_ms
                .saturating_div(1000)
                .saturating_mul(self.refill_rate);

            let available_tokens = if refill > 0 {
                std::cmp::min(self.max_tokens, tokens.saturating_add(refill))
            } else {
                tokens
            };

            if available_tokens == 0 {
                return false;
            }

            let new_tokens = available_tokens.saturating_sub(1);
            let new_last_update_ms = if refill > 0 { now_ms } else { last_update_ms };

            if self
                .tokens
                .compare_exchange(
                    tokens,
                    new_tokens,
                    std::sync::atomic::Ordering::AcqRel,
                    std::sync::atomic::Ordering::Acquire,
                )
                .is_ok()
            {
                if refill > 0 {
                    self.last_update_ms
                        .store(new_last_update_ms, std::sync::atomic::Ordering::Release);
                }
                return true;
            }
        }
    }
}

struct RateLimiter {
    inner: TokenBucketRateLimiter,
}

impl RateLimiter {
    fn new() -> Self {
        Self {
            inner: TokenBucketRateLimiter::new(
                100, // max_tokens: Allow up to 100 actions
                10,  // refill_rate: Refill 10 tokens per second
            ),
        }
    }

    fn allow(&self) -> bool {
        self.inner.allow()
    }
}

impl Default for RateLimiter {
    fn default() -> Self {
        Self::new()
    }
}

struct ChatRateLimiter {
    inner: TokenBucketRateLimiter,
}

impl ChatRateLimiter {
    fn new() -> Self {
        Self {
            inner: TokenBucketRateLimiter::new(
                5, // max_tokens: Allow up to 5 chat messages
                1, // refill_rate: Refill 1 token per second (1 message/sec)
            ),
        }
    }

    fn allow(&self) -> bool {
        self.inner.allow()
    }
}

fn validate_action_amount(amount: i64, max_allowed: i32) -> Result<i32, String> {
    if amount <= 0 {
        return Err("Action amount must be positive".to_string());
    }
    let amount =
        i32::try_from(amount).map_err(|_| format!("Amount {} exceeds i32::MAX", amount))?;
    if amount > max_allowed {
        return Err(format!(
            "Amount {} exceeds maximum allowed: {}",
            amount, max_allowed
        ));
    }
    Ok(amount)
}

/// Maximum multiplier for bet relative to pot size (prevents oversized bets)
pub const MAX_BET_MULTIPLIER: i32 = 10;
/// Maximum allowed WebSocket message size in bytes (4KB)
pub const MAX_MESSAGE_SIZE: usize = 4096;
/// Maximum chips a player can have at any time
pub const MAX_PLAYER_CHIPS: i32 = 1000000;
/// Starting chips for new players
pub const STARTING_CHIPS: i32 = 1000;
/// Capacity for tokio mpsc channels used for message passing
const CHANNEL_CAPACITY: usize = 100;
/// Timeout for player inactivity in milliseconds (10 minutes)
pub const INACTIVITY_TIMEOUT_MS: u64 = 600000;
/// Maximum total concurrent connections to server
pub const MAX_CONNECTIONS: usize = 100;
/// Maximum concurrent connections from a single IP address
pub const MAX_CONNECTIONS_PER_IP: usize = 5;
/// Session token expiry time in hours
pub const SESSION_TOKEN_EXPIRY_HOURS: u64 = 24;
/// Maximum bet allowed per hand
pub const MAX_BET_PER_HAND: i32 = 100000;

#[derive(Clone)]
pub struct ServerConfig {
    pub max_player_chips: i32,
    pub starting_chips: i32,
    pub small_blind: i32,
    pub big_blind: i32,
    pub max_message_size: usize,
    pub inactivity_timeout_ms: u64,
    pub max_connections: usize,
    pub max_connections_per_ip: usize,
    pub session_token_expiry_hours: u64,
    pub max_bet_per_hand: i32,
    pub enable_hmac_verification: bool,
}

impl Default for ServerConfig {
    fn default() -> Self {
        Self {
            max_player_chips: MAX_PLAYER_CHIPS,
            starting_chips: STARTING_CHIPS,
            small_blind: 5,
            big_blind: 10,
            max_message_size: MAX_MESSAGE_SIZE,
            inactivity_timeout_ms: INACTIVITY_TIMEOUT_MS,
            max_connections: MAX_CONNECTIONS,
            max_connections_per_ip: MAX_CONNECTIONS_PER_IP,
            session_token_expiry_hours: SESSION_TOKEN_EXPIRY_HOURS,
            max_bet_per_hand: MAX_BET_PER_HAND,
            enable_hmac_verification: true,
        }
    }
}

impl ServerConfig {
    pub fn from_env() -> Self {
        Self {
            max_player_chips: std::env::var("POKER_MAX_PLAYER_CHIPS")
                .ok()
                .and_then(|v| v.parse().ok())
                .unwrap_or(MAX_PLAYER_CHIPS),
            starting_chips: std::env::var("POKER_STARTING_CHIPS")
                .ok()
                .and_then(|v| v.parse().ok())
                .unwrap_or(STARTING_CHIPS),
            small_blind: std::env::var("POKER_SMALL_BLIND")
                .ok()
                .and_then(|v| v.parse().ok())
                .unwrap_or(5),
            big_blind: std::env::var("POKER_BIG_BLIND")
                .ok()
                .and_then(|v| v.parse().ok())
                .unwrap_or(10),
            max_message_size: std::env::var("POKER_MAX_MESSAGE_SIZE")
                .ok()
                .and_then(|v| v.parse().ok())
                .unwrap_or(MAX_MESSAGE_SIZE),
            inactivity_timeout_ms: std::env::var("POKER_INACTIVITY_TIMEOUT_MS")
                .ok()
                .and_then(|v| v.parse().ok())
                .unwrap_or(INACTIVITY_TIMEOUT_MS),
            max_connections: std::env::var("POKER_MAX_CONNECTIONS")
                .ok()
                .and_then(|v| v.parse().ok())
                .unwrap_or(MAX_CONNECTIONS),
            max_connections_per_ip: std::env::var("POKER_MAX_CONNECTIONS_PER_IP")
                .ok()
                .and_then(|v| v.parse().ok())
                .unwrap_or(MAX_CONNECTIONS_PER_IP),
            session_token_expiry_hours: std::env::var("POKER_SESSION_TOKEN_EXPIRY_HOURS")
                .ok()
                .and_then(|v| v.parse().ok())
                .unwrap_or(SESSION_TOKEN_EXPIRY_HOURS),
            max_bet_per_hand: std::env::var("POKER_MAX_BET_PER_HAND")
                .ok()
                .and_then(|v| v.parse().ok())
                .unwrap_or(MAX_BET_PER_HAND),
            enable_hmac_verification: std::env::var("POKER_ENABLE_HMAC")
                .ok()
                .and_then(|v| v.parse().ok())
                .unwrap_or(true),
        }
    }
}

struct ShutdownState {
    should_shutdown: Arc<AtomicBool>,
}

impl ShutdownState {
    fn new() -> Self {
        Self {
            should_shutdown: Arc::new(AtomicBool::new(false)),
        }
    }

    fn is_shutdown_requested(&self) -> bool {
        self.should_shutdown.load(Ordering::Relaxed)
    }

    fn request_shutdown(&self) {
        self.should_shutdown.store(true, Ordering::Relaxed);
    }
}

impl Clone for ShutdownState {
    fn clone(&self) -> Self {
        Self {
            should_shutdown: self.should_shutdown.clone(),
        }
    }
}

async fn wait_for_shutdown_signal() {
    #[cfg(unix)]
    {
        let ctrl_c = async {
            signal::ctrl_c().await.expect("Failed to listen for Ctrl+C");
        };

        let terminate = async {
            signal::unix::signal(signal::unix::SignalKind::terminate())
                .expect("Failed to install signal handler")
                .recv()
                .await;
        };

        tokio::select! {
            _ = ctrl_c => {},
            _ = terminate => {},
        }
    }
    #[cfg(not(unix))]
    {
        signal::ctrl_c().await.expect("Failed to listen for Ctrl+C");
    }
}

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    env_logger::init();

    let config = ServerConfig::default();
    let server = Arc::new(Mutex::new(PokerServer::new()));
    let shutdown_state = ShutdownState::new();

    let hmac_key = if config.enable_hmac_verification {
        Arc::new(HmacKey::new().unwrap_or_else(|_| HmacKey::default()))
    } else {
        Arc::new(HmacKey::from_bytes(&[0u8; HMAC_SECRET_LEN]).unwrap())
    };
    let nonce_cache = Arc::new(NonceCache::new());

    let addr = std::env::var(ENV_SERVER_ADDR).unwrap_or_else(|_| DEFAULT_SERVER_ADDR.to_string());
    let listener = TcpListener::bind(&addr).await?;
    info!("Poker server listening on: {}", addr);

    let game = {
        let mut server_guard = server.lock();
        server_guard.create_game(
            "main_table".to_string(),
            config.small_blind,
            config.big_blind,
        )
    };

    let broadcast_task = {
        let server = Arc::clone(&server);
        let mut rx = game.lock().tx.subscribe();
        tokio::spawn(async move {
            while let Ok(msg) = rx.recv().await {
                let s = server.lock();
                s.broadcast_to_game("main_table", msg);
            }
        })
    };

    let game_clone: Arc<Mutex<PokerGame>> = Arc::clone(&game);
    let shutdown_flag = shutdown_state.should_shutdown.clone();
    let inactivity_task = tokio::spawn(async move {
        while !shutdown_flag.load(Ordering::Relaxed) {
            tokio::time::sleep(tokio::time::Duration::from_secs(5)).await;
            let active_count = {
                let g = game_clone.lock();
                g.players.values().filter(|p| !p.is_sitting_out).count()
            };

            if active_count < 2 {
                let mut g = game_clone.lock();
                if !g.players.is_empty() {
                    g.game_stage = poker_protocol::GameStage::WaitingForPlayers;
                }
            }
        }
    });

    let shutdown_signal = shutdown_state.should_shutdown.clone();

    let _signal_task = tokio::spawn(async move {
        wait_for_shutdown_signal().await;
        shutdown_signal.store(true, Ordering::Relaxed);
    });

    let mut active_connections: Vec<tokio::task::JoinHandle<()>> = Vec::new();
    let mut cleanup_counter = 0;
    const CLEANUP_INTERVAL: usize = 5;

    while !shutdown_state.is_shutdown_requested() {
        let result = listener.accept().await;

        let (stream, addr) = match result {
            Ok((stream, addr)) => (stream, addr),
            Err(e) => {
                if !shutdown_state.is_shutdown_requested() {
                    error!("Failed to accept connection: {}", e);
                }
                continue;
            }
        };

        info!("New client connected: {}", addr);

        let server = Arc::clone(&server);
        let player_id = Uuid::new_v4().to_string();
        let shutdown_flag = shutdown_state.should_shutdown.clone();
        let hmac_key = hmac_key.clone();
        let nonce_cache = nonce_cache.clone();

        let handle = tokio::spawn(async move {
            if shutdown_flag.load(Ordering::Relaxed) {
                return;
            }

            if let Err(e) = handle_connection(
                stream,
                addr,
                Arc::clone(&server),
                player_id.clone(),
                hmac_key,
                nonce_cache,
            )
            .await
            {
                error!("Error handling connection: {}", e);
            }
        });

        active_connections.push(handle);

        cleanup_counter += 1;
        if cleanup_counter >= CLEANUP_INTERVAL {
            cleanup_counter = 0;
            active_connections.retain(|handle| !handle.is_finished());
        }
    }

    info!("Shutdown signal received, initiating graceful shutdown...");

    info!("Waiting for active connections to finish...");
    let shutdown_deadline = Instant::now() + Duration::from_secs(SHUTDOWN_TIMEOUT_SECS);

    for handle in active_connections {
        if Instant::now() >= shutdown_deadline {
            warn!("Shutdown timeout reached, forcing close of remaining connections");
            break;
        }
        let _ = tokio::time::timeout(Duration::from_secs(2), handle).await;
    }

    info!("Shutting down broadcast task...");
    drop(broadcast_task);
    inactivity_task.abort();

    info!("Server shutdown complete");
    Ok(())
}

fn generate_player_name(player_id: &str) -> String {
    const PLAYER_NAME_PREFIXES: &[&str] = &[
        "Rabbit", "Fox", "Bear", "Wolf", "Eagle", "Lion", "Tiger", "Hawk", "Shark", "Snake",
        "Panther", "Cobra", "Viper", "Jaguar", "Falcon", "Lynx",
    ];
    const PLAYER_NAME_SUFFIXES: &[&str] = &[
        "Ace", "King", "Queen", "Jack", "Ten", "Nine", "Eight", "Seven", "Six", "Five", "Four",
        "Three", "Two", "Bird", "Runner", "Hunter",
    ];

    let mut rng = rand::thread_rng();
    let prefix = PLAYER_NAME_PREFIXES.choose(&mut rng).unwrap_or(&"Player");
    let suffix = PLAYER_NAME_SUFFIXES.choose(&mut rng).unwrap_or(&"");

    format!(
        "{}{}{}",
        prefix,
        suffix,
        &player_id.chars().take(4).collect::<String>().to_uppercase()
    )
}

fn sanitize_chat_message(text: &str) -> String {
    let max_len = 500;
    let mut result = String::with_capacity(text.len().min(max_len));
    for c in text.chars().take(max_len) {
        if c.is_control() && !c.is_whitespace() {
            continue;
        }
        if c == '\t' || c == '\n' || c == '\r' {
            result.push(' ');
        } else if !c.is_control() {
            result.push(c);
        }
    }
    result.trim().to_string()
}

struct MessageHandler {
    server: Arc<Mutex<PokerServer>>,
    player_id: String,
    rate_limiter: Arc<RateLimiter>,
    chat_rate_limiter: Arc<ChatRateLimiter>,
}

impl MessageHandler {
    fn new(
        server: Arc<Mutex<PokerServer>>,
        player_id: String,
        rate_limiter: Arc<RateLimiter>,
        chat_rate_limiter: Arc<ChatRateLimiter>,
    ) -> Self {
        Self {
            server,
            player_id,
            rate_limiter,
            chat_rate_limiter,
        }
    }

    async fn handle_connect(&self) {
        let mut server = self.server.lock();
        if let Err(e) = server.handle_message(&self.player_id, ClientMessage::Connect) {
            self.send_error(&e.to_string());
        }
    }

    async fn handle_action(&self, value: &serde_json::Value) {
        if !self.rate_limiter.allow() {
            warn!("Player {} action rate limited", self.player_id);
            return;
        }

        if let Some(action_value) = value.get("action") {
            if let Some(action_str) = action_value.as_str() {
                if let Some(action) = poker_protocol::PlayerAction::parse_action(action_str) {
                    self.send_action(action);
                } else if let Some(action) =
                    poker_protocol::PlayerAction::from_value(&value["action"], None)
                {
                    self.send_action(action);
                } else {
                    warn!("Unknown action: {}", action_str);
                }
            } else if let Some(amount_value) = value["action"]["Bet"].as_i64() {
                self.handle_bet(amount_value);
            } else if let Some(amount_value) = value["action"]["Raise"].as_i64() {
                self.handle_raise(amount_value);
            }
        }
    }

    fn handle_bet(&self, amount_value: i64) {
        match validate_action_amount(amount_value, MAX_PLAYER_CHIPS) {
            Ok(amount) => {
                self.send_action(poker_protocol::PlayerAction::Bet(amount));
            }
            Err(err_msg) => {
                self.send_error(&err_msg);
            }
        }
    }

    fn handle_raise(&self, amount_value: i64) {
        match validate_action_amount(amount_value, MAX_PLAYER_CHIPS) {
            Ok(amount) => {
                self.send_action(poker_protocol::PlayerAction::Raise(amount));
            }
            Err(err_msg) => {
                self.send_error(&err_msg);
            }
        }
    }

    fn send_action(&self, action: poker_protocol::PlayerAction) {
        let mut server = self.server.lock();
        if let Err(e) = server.handle_message(&self.player_id, ClientMessage::Action(action)) {
            self.send_error(&e.to_string());
        }
    }

    fn send_error(&self, error: &str) {
        let error_msg = ServerMessage::Error(error.to_string());
        if let Ok(json) = serde_json::to_string(&error_msg) {
            let server = self.server.lock();
            if let Err(e) = server.send_to_player(&self.player_id, json) {
                warn!("Failed to send error to {}: {}", self.player_id, e);
            }
        }
    }

    async fn handle_chat(&self, value: &serde_json::Value) {
        if !self.chat_rate_limiter.allow() {
            warn!("Player {} chat rate limited", self.player_id);
            self.send_error("Chat rate limit exceeded. Please wait before sending more messages.");
            return;
        }
        if let Some(chat_text) = value["text"].as_str() {
            let sanitized_text = sanitize_chat_message(chat_text);
            let mut server = self.server.lock();
            if let Err(e) =
                server.handle_message(&self.player_id, ClientMessage::Chat(sanitized_text))
            {
                self.send_error(&e.to_string());
            }
        }
    }

    async fn handle_sit_out(&self) {
        let mut server = self.server.lock();
        if let Err(e) = server.handle_message(&self.player_id, ClientMessage::SitOut) {
            self.send_error(&e.to_string());
        }
    }

    async fn handle_return(&self) {
        let mut server = self.server.lock();
        if let Err(e) = server.handle_message(&self.player_id, ClientMessage::Return) {
            self.send_error(&e.to_string());
        }
    }

    async fn handle_ping(&self, timestamp: u64) {
        let pong_msg = ServerMessage::Pong(timestamp);
        if let Ok(json) = serde_json::to_string(&pong_msg) {
            let server = self.server.lock();
            if let Err(e) = server.send_to_player(&self.player_id, json) {
                warn!("Failed to send pong to {}: {}", self.player_id, e);
            }
        }
    }

    async fn handle_client_message(&self, client_msg: ClientMessage) {
        let mut server = self.server.lock();
        if let Err(e) = server.handle_message(&self.player_id, client_msg) {
            self.send_error(&e.to_string());
        }
    }
}

async fn handle_connection(
    stream: tokio::net::TcpStream,
    addr: SocketAddr,
    server: Arc<Mutex<PokerServer>>,
    player_id: String,
    hmac_key: Arc<HmacKey>,
    nonce_cache: Arc<NonceCache>,
) -> Result<(), Box<dyn std::error::Error>> {
    let ip = addr.ip().to_string();

    let can_accept = {
        let s = server.lock();
        s.can_accept_connection(&ip)
    };

    if !can_accept {
        warn!("Connection rejected from {}: too many connections", ip);
        return Ok(());
    }

    {
        let mut s = server.lock();
        s.register_connection(&ip);
    }

    let ws_stream = accept_async(stream).await?;
    debug!("WebSocket handshake completed for player: {}", player_id);

    let (write, read) = ws_stream.split();

    let rate_limiter = Arc::new(RateLimiter::new());
    let chat_rate_limiter = Arc::new(ChatRateLimiter::new());

    let (tx, mut rx) = tokio::sync::mpsc::channel::<String>(CHANNEL_CAPACITY);
    let write_handle = tokio::spawn(async move {
        let mut sink = write;
        while let Some(msg) = rx.recv().await {
            if let Err(e) = sink.send(Message::Text(msg.into())).await {
                error!("Failed to send message: {}", e);
                break;
            }
        }
    });

    {
        let mut s = server.lock();
        let player_name = generate_player_name(&player_id);
        s.register_player(player_id.clone(), player_name, STARTING_CHIPS);
        s.connect_player(&player_id, tx);
    }

    let server_for_read = Arc::clone(&server);
    let server_for_cleanup = Arc::clone(&server);
    let player_id_clone = player_id.clone();
    let player_id_for_cleanup = player_id.clone();
    let rate_limiter_clone = Arc::clone(&rate_limiter);
    let chat_rate_limiter_clone = Arc::clone(&chat_rate_limiter);
    let rate_limiter_for_handler = Arc::clone(&rate_limiter_clone);
    let hmac_key_clone = hmac_key.clone();
    let nonce_cache_clone = nonce_cache.clone();
    let handler = MessageHandler::new(
        server,
        player_id.clone(),
        rate_limiter_for_handler,
        chat_rate_limiter_clone,
    );

    let read_task = tokio::spawn(async move {
        let mut stream = read;
        let mut last_activity = Instant::now();
        let server_for_read = server_for_read;
        let player_id = player_id_clone;
        let config = ServerConfig::default();

        while let Some(result) = stream.next().await {
            if last_activity.elapsed() > Duration::from_millis(INACTIVITY_TIMEOUT_MS) {
                warn!("Player {} timed out due to inactivity", player_id);
                break;
            }

            match result {
                Ok(Message::Text(text)) => {
                    last_activity = Instant::now();

                    if !rate_limiter_clone.allow() {
                        warn!("Player {} exceeded rate limit", player_id);
                        let error_msg = ServerMessage::Error("Rate limit exceeded".to_string());
                        if let Ok(json) = serde_json::to_string(&error_msg) {
                            let server = server_for_read.lock();
                            if let Err(e) = server.send_to_player(&player_id, json) {
                                warn!("Failed to send rate limit error: {}", e);
                            }
                        }
                        break;
                    }

                    if text.len() > MAX_MESSAGE_SIZE {
                        warn!("Message from {} too large: {} bytes", player_id, text.len());
                        let error_msg = ServerMessage::Error("Message too large".to_string());
                        if let Ok(json) = serde_json::to_string(&error_msg) {
                            let server = server_for_read.lock();
                            if let Err(e) = server.send_to_player(&player_id, json) {
                                warn!("Failed to send size error: {}", e);
                            }
                        }
                        break;
                    }
                    debug!("Received from {}: {}", player_id, text);

                    let use_hmac = config.enable_hmac_verification;
                    if use_hmac {
                        if let Ok(signed_msg) =
                            serde_json::from_str::<poker_protocol::SignedMessage>(&text)
                        {
                            match signed_msg.verify(&hmac_key_clone, &nonce_cache_clone) {
                                Ok(client_msg) => {
                                    handler.handle_client_message(client_msg).await;
                                }
                                Err(e) => {
                                    warn!(
                                        "HMAC verification failed for player {}: {}",
                                        player_id, e
                                    );
                                    let error_msg = ServerMessage::Error(
                                        "Invalid message signature".to_string(),
                                    );
                                    if let Ok(json) = serde_json::to_string(&error_msg) {
                                        let server = server_for_read.lock();
                                        let _ = server.send_to_player(&player_id, json);
                                    }
                                    break;
                                }
                            }
                        } else {
                            warn!(
                                "Player {} sent unsigned message when HMAC is required",
                                player_id
                            );
                            let error_msg =
                                ServerMessage::Error("Message signing is required".to_string());
                            if let Ok(json) = serde_json::to_string(&error_msg) {
                                let server = server_for_read.lock();
                                let _ = server.send_to_player(&player_id, json);
                            }
                            break;
                        }
                    } else if let Ok(value) = serde_json::from_str::<serde_json::Value>(&text) {
                        if let Some(type_obj) = value.get("type") {
                            if let Some(type_str) = type_obj.as_str() {
                                match type_str {
                                    "Connect" => {
                                        handler.handle_connect().await;
                                    }
                                    "Action" => {
                                        handler.handle_action(&value).await;
                                    }
                                    "Chat" => {
                                        handler.handle_chat(&value).await;
                                    }
                                    "SitOut" => {
                                        handler.handle_sit_out().await;
                                    }
                                    "Return" => {
                                        handler.handle_return().await;
                                    }
                                    "Ping" => {
                                        if let Some(ts) = value["timestamp"].as_u64() {
                                            handler.handle_ping(ts).await;
                                        }
                                    }
                                    _ => {
                                        warn!("Unknown message type: {}", type_str);
                                    }
                                }
                            }
                        }
                    } else if let Ok(client_msg) = serde_json::from_str::<ClientMessage>(&text) {
                        handler.handle_client_message(client_msg).await;
                    }
                }
                Ok(Message::Close(_)) => {
                    debug!("Client {} disconnected", player_id);
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

    let read_result = read_task.await;
    drop(write_handle);

    {
        let mut s = server_for_cleanup.lock();
        s.disconnect_player(&player_id_for_cleanup);
        s.unregister_connection(&ip);
    }

    read_result.map_err(|e| Box::new(e) as Box<dyn std::error::Error>)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_rate_limiter_allow() {
        let limiter = RateLimiter::new();
        for _ in 0..100 {
            assert!(limiter.allow());
        }
        assert!(!limiter.allow());
    }

    #[tokio::test]
    async fn test_rate_limiter_refill() {
        let limiter = RateLimiter::new();
        for _ in 0..100 {
            assert!(limiter.allow());
        }
        assert!(!limiter.allow());

        tokio::time::sleep(tokio::time::Duration::from_millis(1500)).await;

        assert!(limiter.allow());
    }

    #[test]
    fn test_chat_rate_limiter_allow() {
        let limiter = ChatRateLimiter::new();
        for _ in 0..5 {
            assert!(limiter.allow());
        }
        assert!(!limiter.allow());
    }

    #[test]
    fn test_validate_action_amount_positive() {
        assert!(validate_action_amount(100, 1000).is_ok());
        assert_eq!(validate_action_amount(100, 1000).unwrap(), 100);
    }

    #[test]
    fn test_validate_action_amount_zero() {
        assert!(validate_action_amount(0, 1000).is_err());
        assert!(validate_action_amount(-100, 1000).is_err());
    }

    #[test]
    fn test_validate_action_amount_exceeds_max() {
        assert!(validate_action_amount(1001, 1000).is_err());
    }

    #[test]
    fn test_validate_action_amount_too_large() {
        assert!(validate_action_amount(i64::MAX, 1000).is_err());
    }

    #[test]
    fn test_sanitize_chat_message() {
        assert_eq!(sanitize_chat_message("Hello World"), "Hello World");
    }

    #[test]
    fn test_sanitize_chat_message_controls() {
        let result = sanitize_chat_message("Hello\x00World");
        assert!(!result.contains('\x00'));
    }

    #[test]
    fn test_sanitize_chat_message_max_length() {
        let long_msg = "A".repeat(1000);
        let result = sanitize_chat_message(&long_msg);
        assert!(result.len() <= 500);
    }

    #[test]
    fn test_server_config_default() {
        let config = ServerConfig::default();
        assert_eq!(config.max_player_chips, MAX_PLAYER_CHIPS);
        assert_eq!(config.starting_chips, STARTING_CHIPS);
        assert_eq!(config.small_blind, 5);
        assert_eq!(config.big_blind, 10);
    }

    #[test]
    fn test_shutdown_state() {
        let state = ShutdownState::new();
        assert!(!state.is_shutdown_requested());
        state.request_shutdown();
        assert!(state.is_shutdown_requested());
    }

    #[test]
    fn test_shutdown_state_clone() {
        let state = ShutdownState::new();
        let clone = state.clone();
        state.request_shutdown();
        assert!(clone.is_shutdown_requested());
    }

    #[tokio::test]
    async fn test_integration_player_connect_and_action() {
        let tx = tokio::sync::broadcast::channel(100).0;
        let mut game = PokerGame::new("test_int".to_string(), 5, 10, tx);

        assert_eq!(
            game.game_stage,
            poker_protocol::GameStage::WaitingForPlayers
        );

        game.add_player("p1".to_string(), "Player1".to_string(), 1000);
        game.add_player("p2".to_string(), "Player2".to_string(), 1000);

        assert_eq!(game.players.len(), 2);
        assert!(game.get_players().contains_key("p1"));
        assert!(game.get_players().contains_key("p2"));
    }

    #[tokio::test]
    async fn test_integration_player_sit_out() {
        let tx = tokio::sync::broadcast::channel(100).0;
        let mut game = PokerGame::new("test_sitout".to_string(), 5, 10, tx);

        game.add_player("p1".to_string(), "Player1".to_string(), 1000);

        assert!(!game.get_players().get("p1").unwrap().is_sitting_out);

        game.sit_out("p1");
        assert!(game.get_players().get("p1").unwrap().is_sitting_out);

        game.return_to_game("p1");
        assert!(!game.get_players().get("p1").unwrap().is_sitting_out);
    }

    #[tokio::test]
    async fn test_integration_max_bet_setting() {
        let tx = tokio::sync::broadcast::channel(100).0;
        let mut game = PokerGame::new("test_maxbet".to_string(), 5, 10, tx);

        game.set_max_bet_per_hand(500);

        let players = game.get_players();
        assert!(players.values().all(|p| p.current_bet == 0));
    }

    #[tokio::test]
    async fn test_integration_protocol_serialization() {
        let _action_msg = r#"{"Action":"Fold"}"#;
        let parsed = poker_protocol::PlayerAction::parse_action("Fold");
        assert!(parsed.is_some());

        let error_response = poker_protocol::ServerMessage::Error("Test error".to_string());
        let serialized = serde_json::to_string(&error_response);
        assert!(serialized.is_ok());
        assert!(serialized.unwrap().contains("error"));
    }

    #[tokio::test]
    async fn test_integration_card_creation() {
        use poker_protocol::{Card, Rank, Suit};

        let mut player =
            poker_protocol::PlayerState::new("test".to_string(), "Test".to_string(), 1000);
        player.hole_cards = vec![
            Card::new(Suit::Hearts, Rank::Ace),
            Card::new(Suit::Diamonds, Rank::King),
        ];

        assert_eq!(player.hole_cards.len(), 2);
        assert_eq!(player.hole_cards[0].to_string(), "A♥");
    }
}
