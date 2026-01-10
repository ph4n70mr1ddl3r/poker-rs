mod game;
mod server;

use crate::game::PokerGame;
use crate::server::PokerServer;
use futures::stream::StreamExt;
use futures::SinkExt;
use log::{debug, error, info, warn};
use parking_lot::Mutex;
use poker_protocol::{ClientMessage, ServerMessage};
use std::net::SocketAddr;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::Arc;
use std::time::Duration;
use tokio::net::TcpListener;
use tokio::signal;
use tokio::time::Instant;
use tokio_tungstenite::accept_async;
use tokio_tungstenite::tungstenite::Message;
use uuid::Uuid;

pub const SHUTDOWN_TIMEOUT_SECS: u64 = 5;

pub struct WindowedRateLimiter {
    messages: AtomicU64,
    window_start: AtomicU64,
    max_messages: u64,
    window_ms: u64,
}

impl WindowedRateLimiter {
    pub fn new(max_messages: u64, window_ms: u64) -> Self {
        Self {
            messages: AtomicU64::new(0),
            window_start: AtomicU64::new(0),
            max_messages,
            window_ms,
        }
    }

    pub async fn allow(&self) -> bool {
        let now_ms = tokio::time::Instant::now().elapsed().as_millis() as u64;

        loop {
            let window_start = self.window_start.load(Ordering::Acquire);
            let elapsed = now_ms.saturating_sub(window_start);

            if elapsed > self.window_ms {
                if self
                    .window_start
                    .compare_exchange(window_start, now_ms, Ordering::AcqRel, Ordering::Acquire)
                    .is_ok()
                {
                    self.messages.store(0, Ordering::Release);
                }
                continue;
            }

            let current = self.messages.load(Ordering::Acquire);
            if current >= self.max_messages {
                tokio::time::sleep(Duration::from_millis(10)).await;
                continue;
            }

            if self
                .messages
                .compare_exchange(current, current + 1, Ordering::AcqRel, Ordering::Acquire)
                .is_ok()
            {
                return true;
            }
        }
    }
}

impl Default for WindowedRateLimiter {
    fn default() -> Self {
        Self::new(100, 1000)
    }
}

struct RateLimiter {
    inner: WindowedRateLimiter,
}

impl RateLimiter {
    fn new() -> Self {
        Self {
            inner: WindowedRateLimiter::new(100, 1000),
        }
    }

    async fn allow(&self) -> bool {
        self.inner.allow().await
    }
}

impl Default for RateLimiter {
    fn default() -> Self {
        Self::new()
    }
}

struct ChatRateLimiter {
    inner: WindowedRateLimiter,
}

impl ChatRateLimiter {
    fn new() -> Self {
        Self {
            inner: WindowedRateLimiter::new(10, 10000),
        }
    }

    async fn allow(&self) -> bool {
        self.inner.allow().await
    }
}

impl Default for ChatRateLimiter {
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
const CHANNEL_CAPACITY: usize = 100;
const INACTIVITY_TIMEOUT_MS: u64 = 600000;
const MAX_CONNECTIONS: usize = 100;
const MAX_CONNECTIONS_PER_IP: usize = 5;
const SESSION_TOKEN_EXPIRY_HOURS: u64 = 24;
const MAX_BET_PER_HAND: i32 = 100000;

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

    #[allow(dead_code)]
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

    let addr = "127.0.0.1:8080";
    let listener = TcpListener::bind(addr).await?;
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
    let shutdown_clone = shutdown_state.should_shutdown.clone();
    let inactivity_task = tokio::spawn(async move {
        while !shutdown_clone.load(Ordering::Relaxed) {
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

    let shutdown_flag = shutdown_state.should_shutdown.clone();

    let _signal_task = tokio::spawn(async move {
        wait_for_shutdown_signal().await;
        shutdown_flag.store(true, Ordering::Relaxed);
    });

    let mut active_connections: Vec<tokio::task::JoinHandle<()>> = Vec::new();
    let mut cleanup_counter = 0;

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

        let handle = tokio::spawn(async move {
            if shutdown_flag.load(Ordering::Relaxed) {
                return;
            }

            if let Err(e) =
                handle_connection(stream, addr, Arc::clone(&server), player_id.clone()).await
            {
                error!("Error handling connection: {}", e);
            }

            let mut s = server.lock();
            s.disconnect_player(&player_id);
        });

        active_connections.push(handle);

        cleanup_counter += 1;
        if cleanup_counter >= 10 {
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
    let _ = inactivity_task.abort();

    info!("Server shutdown complete");
    Ok(())
}

fn sanitize_player_name(name: &str) -> String {
    let max_len = 20;
    let mut result = String::with_capacity(name.len().min(max_len * 2));
    let mut has_valid_char = false;

    for c in name.chars() {
        if c.is_alphanumeric() || c == '_' || c == '-' {
            result.push(c);
            has_valid_char = true;
        } else if result.is_empty() {
            result.push('_');
        } else {
            result.push('_');
        }
        if result.len() >= max_len {
            break;
        }
    }

    if !has_valid_char && result.chars().all(|c| c == '_') {
        result.clear();
        result.push_str("Player");
    }

    result
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
        if !self.rate_limiter.allow().await {
            warn!("Player {} action rate limited", self.player_id);
            return;
        }

        if let Some(action_str) = value["action"].as_str() {
            if let Some(action) = poker_protocol::PlayerAction::from_str(action_str) {
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
            server.send_to_player(&self.player_id, json);
        }
    }

    async fn handle_chat(&self, value: &serde_json::Value) {
        if !self.chat_rate_limiter.allow().await {
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
            server.send_to_player(&self.player_id, json);
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
            if let Err(e) = sink.send(Message::Text(msg)).await {
                error!("Failed to send message: {}", e);
                break;
            }
        }
    });

    {
        let mut s = server.lock();
        let sanitized_name = sanitize_player_name(&format!("Player{}", &player_id[..8]));
        s.register_player(player_id.clone(), sanitized_name, STARTING_CHIPS);
        s.connect_player(&player_id, tx);
    }

    let server_for_read = Arc::clone(&server);
    let server_for_cleanup = Arc::clone(&server);
    let player_id_clone = player_id.clone();
    let rate_limiter_clone = Arc::clone(&rate_limiter);
    let chat_rate_limiter_clone = Arc::clone(&chat_rate_limiter);
    let rate_limiter_for_handler = Arc::clone(&rate_limiter_clone);
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

        while let Some(result) = stream.next().await {
            if last_activity.elapsed() > Duration::from_millis(INACTIVITY_TIMEOUT_MS) {
                warn!("Player {} timed out due to inactivity", player_id_clone);
                break;
            }

            match result {
                Ok(Message::Text(text)) => {
                    last_activity = Instant::now();

                    if !rate_limiter_clone.allow().await {
                        warn!("Player {} exceeded rate limit", player_id_clone);
                        let error_msg = ServerMessage::Error("Rate limit exceeded".to_string());
                        if let Ok(json) = serde_json::to_string(&error_msg) {
                            let server = server_for_read.lock();
                            server.send_to_player(&player_id_clone, json);
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
                            let server = server_for_read.lock();
                            server.send_to_player(&player_id_clone, json);
                        }
                        break;
                    }
                    debug!("Received from {}: {}", player_id_clone, text);

                    if let Ok(value) = serde_json::from_str::<serde_json::Value>(&text) {
                        if let Some(type_obj) = value.get("type") {
                            if let Some(type_str) = type_obj.as_str() {
                                match type_str {
                                    "connect" => {
                                        handler.handle_connect().await;
                                    }
                                    "action" => {
                                        handler.handle_action(&value).await;
                                    }
                                    "chat" => {
                                        handler.handle_chat(&value).await;
                                    }
                                    "sit_out" => {
                                        handler.handle_sit_out().await;
                                    }
                                    "return" => {
                                        handler.handle_return().await;
                                    }
                                    "ping" => {
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

    let read_result = read_task.await;
    drop(write_handle);

    {
        let mut s = server_for_cleanup.lock();
        s.unregister_connection(&ip);
    }

    read_result.map_err(|e| Box::new(e) as Box<dyn std::error::Error>)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn test_rate_limiter_allow() {
        let limiter = RateLimiter::new();
        for _ in 0..100 {
            assert!(limiter.allow().await);
        }
        assert!(!limiter.allow().await);
    }

    #[tokio::test]
    async fn test_rate_limiter_window_reset() {
        let limiter = RateLimiter::new();
        for _ in 0..100 {
            assert!(limiter.allow().await);
        }
        assert!(!limiter.allow().await);

        tokio::time::sleep(tokio::time::Duration::from_millis(1100)).await;

        assert!(limiter.allow().await);
    }

    #[tokio::test]
    async fn test_chat_rate_limiter_allow() {
        let limiter = ChatRateLimiter::new();
        for _ in 0..10 {
            assert!(limiter.allow().await);
        }
        assert!(!limiter.allow().await);
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
    fn test_sanitize_player_name_alphanumeric() {
        assert_eq!(sanitize_player_name("TestPlayer123"), "TestPlayer123");
    }

    #[test]
    fn test_sanitize_player_name_special_chars() {
        assert_eq!(sanitize_player_name("Test@Player#123"), "Test_Player_123");
    }

    #[test]
    fn test_sanitize_player_name_empty() {
        assert_eq!(sanitize_player_name("@#$%"), "Player");
    }

    #[test]
    fn test_sanitize_player_name_too_long() {
        let long_name = "A".repeat(50);
        let result = sanitize_player_name(&long_name);
        assert!(result.len() <= 20);
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
}
