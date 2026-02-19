use std::collections::HashMap;
use std::sync::Arc;

use chrono::{DateTime, Utc};
use log::{debug, error, warn};
use parking_lot::Mutex;
use poker_protocol::{
    ChatMessage, ClientMessage, PlayerUpdate, ServerError, ServerMessage, ServerResult,
};
use tokio::sync::broadcast;
use tokio::sync::mpsc::Sender;
use tokio::sync::Semaphore;
use tokio::time::{timeout, Duration};
#[cfg(test)]
use uuid::Uuid;

use crate::game::PokerGame;

/// Timeout in milliseconds for sending broadcast messages to players
const BROADCAST_SEND_TIMEOUT_MS: u64 = 5000;
/// Maximum concurrent broadcast tasks to prevent resource exhaustion
const MAX_BROADCAST_TASKS: usize = 50;
/// Maximum concurrent send tasks to prevent resource exhaustion
const MAX_SEND_TASKS: usize = 100;

/// Type alias for player identifiers.
pub type PlayerId = String;

#[derive(Debug, Clone)]
pub struct ServerPlayer {
    pub name: String,
    pub chips: i32,
    pub connected: bool,
    pub ws_sender: Option<Sender<String>>,
    pub seated: bool,
    #[cfg(test)]
    pub session_token: String,
    pub session_created_at: DateTime<Utc>,
}

impl ServerPlayer {
    pub fn new(_id: PlayerId, name: String, chips: i32) -> Self {
        Self {
            name,
            chips,
            connected: false,
            ws_sender: None,
            seated: false,
            #[cfg(test)]
            session_token: Uuid::new_v4().to_string(),
            session_created_at: Utc::now(),
        }
    }

    pub fn is_session_expired(&self, expiry_hours: u64) -> bool {
        Utc::now()
            .signed_duration_since(self.session_created_at)
            .to_std()
            .map(|d| d > std::time::Duration::from_secs(expiry_hours * 3600))
            .unwrap_or(true)
    }
}

pub struct PokerServer {
    players: HashMap<PlayerId, ServerPlayer>,
    games: HashMap<String, Arc<Mutex<PokerGame>>>,
    player_sessions: HashMap<PlayerId, String>,
    tx: broadcast::Sender<ServerMessage>,
    connection_count: usize,
    ip_connections: HashMap<String, usize>,
    session_expiry_hours: u64,
    broadcast_semaphore: Arc<Semaphore>,
    send_semaphore: Arc<Semaphore>,
}

impl PokerServer {
    /// Creates a new empty poker server.
    pub fn new() -> Self {
        Self {
            players: HashMap::new(),
            games: HashMap::new(),
            player_sessions: HashMap::new(),
            tx: broadcast::channel(100).0,
            connection_count: 0,
            ip_connections: HashMap::new(),
            session_expiry_hours: 24,
            broadcast_semaphore: Arc::new(Semaphore::new(MAX_BROADCAST_TASKS)),
            send_semaphore: Arc::new(Semaphore::new(MAX_SEND_TASKS)),
        }
    }

    /// Sets the session token expiry duration in hours.
    #[cfg(test)]
    pub fn set_session_expiry_hours(&mut self, hours: u64) {
        self.session_expiry_hours = hours;
    }

    /// Checks if a new connection can be accepted from the given IP.
    ///
    /// # Arguments
    /// * `ip` - The IP address of the incoming connection
    ///
    /// # Returns
    /// `true` if the connection can be accepted, `false` otherwise
    pub fn can_accept_connection(&self, ip: &str) -> bool {
        self.connection_count < crate::MAX_CONNECTIONS
            && self
                .ip_connections
                .get(ip)
                .map(|c| *c < crate::MAX_CONNECTIONS_PER_IP)
                .unwrap_or(true)
    }

    /// Registers a new connection from an IP address.
    ///
    /// # Arguments
    /// * `ip` - The IP address of the incoming connection
    pub fn register_connection(&mut self, ip: &str) {
        self.connection_count += 1;
        *self.ip_connections.entry(ip.to_string()).or_insert(0) += 1;
    }

    /// Unregisters a connection from an IP address.
    ///
    /// # Arguments
    /// * `ip` - The IP address of the disconnecting connection
    pub fn unregister_connection(&mut self, ip: &str) {
        self.connection_count = self.connection_count.saturating_sub(1);
        if let Some(count) = self.ip_connections.get_mut(ip) {
            *count = count.saturating_sub(1);
            if *count == 0 {
                self.ip_connections.remove(ip);
            }
        }
    }

    /// Creates a new poker game table.
    ///
    /// # Arguments
    /// * `game_id` - Unique identifier for this game table
    /// * `small_blind` - Small blind amount
    /// * `big_blind` - Big blind amount
    ///
    /// # Returns
    /// A new `Arc<Mutex<PokerGame>>` for the created game
    pub fn create_game(
        &mut self,
        game_id: String,
        small_blind: i32,
        big_blind: i32,
    ) -> Arc<Mutex<PokerGame>> {
        let game = Arc::new(Mutex::new(PokerGame::new(
            game_id.clone(),
            small_blind,
            big_blind,
            self.tx.clone(),
        )));
        self.games.insert(game_id.clone(), game.clone());
        game
    }

    /// Gets a reference to a game by its ID.
    ///
    /// # Arguments
    /// * `game_id` - The ID of the game to retrieve
    ///
    /// # Returns
    /// `Some(Arc<Mutex<PokerGame>>)` if found, `None` otherwise
    #[allow(dead_code)]
    pub fn get_game(&self, game_id: &str) -> Option<Arc<Mutex<PokerGame>>> {
        self.games.get(game_id).cloned()
    }

    /// Checks if a player is currently in a game.
    ///
    /// # Arguments
    /// * `player_id` - The player to check
    ///
    /// # Returns
    /// `true` if the player is seated in a game, `false` otherwise
    #[allow(dead_code)]
    pub fn is_player_in_game(&self, player_id: &str) -> bool {
        self.player_sessions.contains_key(player_id)
    }

    /// Registers a new player with the server (without connecting).
    ///
    /// # Arguments
    /// * `player_id` - Unique player identifier
    /// * `name` - Player's display name
    /// * `chips` - Starting chip amount
    pub fn register_player(&mut self, player_id: PlayerId, name: String, chips: i32) {
        if self.players.contains_key(&player_id) {
            debug!("Player {} already registered, skipping", player_id);
            return;
        }
        let player = ServerPlayer::new(player_id.clone(), name, chips);
        self.players.insert(player_id, player);
    }

    /// Connects a player to the server with a WebSocket sender.
    ///
    /// # Arguments
    /// * `player_id` - The player to connect
    /// * `ws_sender` - Channel sender for WebSocket messages to this player
    pub fn connect_player(&mut self, player_id: &str, ws_sender: Sender<String>) {
        if let Some(player) = self.players.get_mut(player_id) {
            player.connected = true;
            player.ws_sender = Some(ws_sender);
        }
    }

    /// Disconnects a player from the server.
    ///
    /// # Arguments
    /// * `player_id` - The player to disconnect
    pub fn disconnect_player(&mut self, player_id: &str) {
        if let Some(player) = self.players.get_mut(player_id) {
            player.connected = false;
            player.ws_sender = None;

            let disconnect_msg =
                ServerMessage::PlayerDisconnected(poker_protocol::PlayerDisconnectedUpdate {
                    player_id: player_id.to_string(),
                });
            let json = match disconnect_msg.to_unified_json() {
                Ok(json) => json,
                Err(e) => {
                    error!("Failed to serialize disconnect message: {}", e);
                    return;
                }
            };
            self.broadcast_to_game_by_player(player_id, &json);
        }

        self.player_sessions.remove(player_id);
    }

    fn broadcast_to_game_by_player(&self, exclude_player_id: &str, message: &str) {
        let Some(game_id) = self.player_sessions.get(exclude_player_id) else {
            debug!(
                "No game found for player {} (not in any game)",
                exclude_player_id
            );
            return;
        };

        let Some(game) = self.games.get(game_id) else {
            error!(
                "Game {} not found for player {}",
                game_id, exclude_player_id
            );
            return;
        };

        let pg = game.lock();

        let players: Vec<(String, tokio::sync::mpsc::Sender<String>)> = {
            pg.get_players()
                .keys()
                .filter(|player_id| player_id.as_str() != exclude_player_id)
                .filter(|player_id| {
                    self.players
                        .get(player_id.as_str())
                        .map(|p| p.connected)
                        .unwrap_or(false)
                })
                .filter_map(|player_id| {
                    self.players
                        .get(player_id.as_str())
                        .and_then(|p| p.ws_sender.as_ref())
                        .map(|sender| (player_id.clone(), sender.clone()))
                })
                .collect()
        };

        drop(pg);

        if players.is_empty() {
            debug!(
                "No other connected players to broadcast to in game {}",
                game_id
            );
            return;
        }

        let timeout_duration = Duration::from_millis(BROADCAST_SEND_TIMEOUT_MS);
        let msg_arc = Arc::new(message.to_string());
        let semaphore = Arc::clone(&self.broadcast_semaphore);

        for (player_id, sender) in players {
            let msg = Arc::clone(&msg_arc);
            let sem = Arc::clone(&semaphore);
            tokio::spawn(async move {
                let permit = match sem.acquire().await {
                    Ok(permit) => permit,
                    Err(e) => {
                        error!("Failed to acquire broadcast semaphore: {}", e);
                        return;
                    }
                };
                if let Err(e) = timeout(timeout_duration, sender.send((*msg).clone())).await {
                    error!("Timeout sending to player {}: {}", player_id, e);
                }
                drop(permit);
            });
        }
    }

    pub fn seat_player(&mut self, player_id: &str, game_id: &str) -> ServerResult<()> {
        let player = self
            .players
            .get_mut(player_id)
            .ok_or(ServerError::PlayerNotFound(player_id.to_string()))?;

        let game = self
            .games
            .get(game_id)
            .ok_or(ServerError::GameNotFound(game_id.to_string()))?;

        if player.chips <= 0 {
            return Err(ServerError::NoChips);
        }

        if self.player_sessions.contains_key(player_id) {
            return Ok(());
        }

        player.seated = true;
        self.player_sessions
            .insert(player_id.to_string(), game_id.to_string());

        game.lock()
            .add_player(player_id.to_string(), player.name.clone(), player.chips);

        let connected_msg = ServerMessage::Connected(player_id.to_string());
        let json = serde_json::to_string(&connected_msg)
            .map_err(|e| ServerError::GameState(e.to_string()))?;
        if let Err(e) = self.send_to_player(player_id, json) {
            warn!("Failed to send connected message to {}: {}", player_id, e);
        }

        self.send_game_state_to_player(player_id, game_id)?;

        Ok(())
    }

    fn send_game_state_to_player(&self, player_id: &str, game_id: &str) -> ServerResult<()> {
        let game = self
            .games
            .get(game_id)
            .ok_or(ServerError::GameNotFound(game_id.to_string()))?;
        let poker_game = game.lock();

        let players: Vec<PlayerUpdate> = poker_game
            .players
            .values()
            .map(|p: &poker_protocol::PlayerState| PlayerUpdate {
                player_id: p.id.clone(),
                player_name: p.name.clone(),
                chips: p.chips,
                current_bet: p.current_bet,
                has_acted: p.has_acted,
                is_all_in: p.is_all_in,
                is_folded: p.is_folded,
                is_sitting_out: p.is_sitting_out,
                hole_cards: p.hole_cards.iter().map(|c| c.to_string()).collect(),
            })
            .collect();

        drop(poker_game);

        let game_state = ServerMessage::PlayerUpdates(players);
        let json = match game_state.to_unified_json() {
            Ok(json) => json,
            Err(e) => {
                error!("Failed to serialize game state: {}", e);
                return Err(ServerError::GameState(e.to_string()));
            }
        };
        if let Err(e) = self.send_to_player(player_id, json) {
            warn!("Failed to send game state to {}: {}", player_id, e);
        }

        Ok(())
    }

    /// Handles a message from a player.
    ///
    /// # Arguments
    /// * `player_id` - The player sending the message
    /// * `message` - The message to process
    ///
    /// # Returns
    /// Result indicating success or error
    pub fn handle_message(&mut self, player_id: &str, message: ClientMessage) -> ServerResult<()> {
        match message {
            ClientMessage::Connect => {
                if self.player_sessions.contains_key(player_id) {
                    return Ok(());
                }
                self.seat_player(player_id, "main_table")?;
            }
            ClientMessage::Reconnect(existing_player_id) => {
                if existing_player_id != player_id {
                    warn!(
                        "Player {} attempted to reconnect as {}",
                        player_id, existing_player_id
                    );
                    return Err(ServerError::PlayerNotFound(existing_player_id));
                }

                if let Some(player) = self.players.get_mut(&existing_player_id) {
                    if player.is_session_expired(self.session_expiry_hours) {
                        warn!("Session expired for player {}", existing_player_id);
                        return Err(ServerError::SessionExpired);
                    }
                    player.connected = true;
                    if let Some(session) = self.player_sessions.get(&existing_player_id) {
                        self.send_game_state_to_player(&existing_player_id, session)?;
                    }
                } else {
                    return Err(ServerError::PlayerNotFound(existing_player_id));
                }
            }
            ClientMessage::Action(action) => {
                let session = self
                    .player_sessions
                    .get(player_id)
                    .ok_or(ServerError::PlayerNotInGame)?
                    .clone();

                if let Some(game) = self.games.get(&session) {
                    let mut poker_game = game.lock();
                    poker_game.handle_action(player_id, action)?;
                } else {
                    return Err(ServerError::GameNotFound(session));
                }
            }
            ClientMessage::Chat(text) => {
                let chat_msg = ChatMessage {
                    player_id: player_id.to_string(),
                    player_name: self
                        .players
                        .get(player_id)
                        .map(|p| p.name.clone())
                        .unwrap_or_default(),
                    text,
                    timestamp: chrono::Utc::now().timestamp_millis().max(0) as u64,
                };
                if let Err(e) = self.tx.send(ServerMessage::Chat(chat_msg)) {
                    error!("Failed to send chat message to broadcast channel: {}", e);
                }
            }
            ClientMessage::SitOut => {
                let session = self
                    .player_sessions
                    .get(player_id)
                    .ok_or(ServerError::PlayerNotInGame)?
                    .clone();

                let game = self
                    .games
                    .get(&session)
                    .ok_or_else(|| ServerError::GameNotFound(session.clone()))?;
                let mut poker_game = game.lock();
                poker_game.sit_out(player_id);
            }
            ClientMessage::Return => {
                let session = self
                    .player_sessions
                    .get(player_id)
                    .ok_or(ServerError::PlayerNotInGame)?
                    .clone();

                let game = self
                    .games
                    .get(&session)
                    .ok_or_else(|| ServerError::GameNotFound(session.clone()))?;
                let mut poker_game = game.lock();
                poker_game.return_to_game(player_id);
            }
        }

        Ok(())
    }

    /// Broadcasts a message to all connected players in a game.
    ///
    /// # Arguments
    /// * `game_id` - The game to broadcast to
    /// * `message` - The message to send
    pub fn broadcast_to_game(&self, game_id: &str, message: ServerMessage) {
        let json = match message.to_unified_json() {
            Ok(json) => json,
            Err(e) => {
                error!("Failed to serialize message to unified format: {}", e);
                match serde_json::to_string(&message) {
                    Ok(fallback) => fallback,
                    Err(e2) => {
                        error!("Failed to serialize message to fallback format: {}", e2);
                        error!("Message was: {:?}", message);
                        return;
                    }
                }
            }
        };

        let Some(game) = self.games.get(game_id) else {
            error!("Game {} not found for broadcast", game_id);
            return;
        };

        let pg = game.lock();

        let players: Vec<(String, tokio::sync::mpsc::Sender<String>)> = {
            pg.get_players()
                .keys()
                .filter(|player_id| {
                    self.players
                        .get(player_id.as_str())
                        .map(|p| p.connected)
                        .unwrap_or(false)
                })
                .filter_map(|player_id| {
                    self.players
                        .get(player_id.as_str())
                        .and_then(|p| p.ws_sender.as_ref())
                        .map(|sender| (player_id.clone(), sender.clone()))
                })
                .collect()
        };

        if players.is_empty() {
            debug!("No connected players to broadcast to in game {}", game_id);
            return;
        }

        let timeout_duration = Duration::from_millis(BROADCAST_SEND_TIMEOUT_MS);
        let msg_arc = Arc::new(json);
        let semaphore = Arc::clone(&self.broadcast_semaphore);

        for (player_id, sender) in players {
            let msg = Arc::clone(&msg_arc);
            let sender = sender.clone();
            let sem = Arc::clone(&semaphore);
            tokio::spawn(async move {
                let permit = match sem.acquire().await {
                    Ok(permit) => permit,
                    Err(e) => {
                        error!("Failed to acquire broadcast semaphore: {}", e);
                        return;
                    }
                };
                if let Err(e) = timeout(timeout_duration, sender.send((*msg).clone())).await {
                    error!("Timeout sending to player {}: {}", player_id, e);
                }
                drop(permit);
            });
        }
    }

    /// Sends a message to a specific player.
    ///
    /// # Arguments
    /// * `player_id` - The target player
    /// * `message` - The message to send
    pub fn send_to_player(&self, player_id: &str, message: String) -> ServerResult<()> {
        let player = self
            .players
            .get(player_id)
            .ok_or_else(|| ServerError::PlayerNotFound(player_id.to_string()))?;

        let sender = player
            .ws_sender
            .as_ref()
            .ok_or_else(|| ServerError::PlayerNotConnected(player_id.to_string()))?;

        let sem = Arc::clone(&self.send_semaphore);
        let sender = sender.clone();
        let player_id_owned = player_id.to_string();

        tokio::spawn(async move {
            let permit = match sem.acquire().await {
                Ok(permit) => permit,
                Err(e) => {
                    error!("Failed to acquire send semaphore: {}", e);
                    return;
                }
            };
            if let Err(e) = sender.send(message).await {
                error!(
                    "Failed to send message to player {}: {}",
                    player_id_owned, e
                );
            }
            drop(permit);
        });

        Ok(())
    }

    /// Verifies a player's session token.
    /// Used during reconnection to validate that player owns session.
    #[cfg(test)]
    pub fn verify_session(&self, player_id: &str, token: &str) -> bool {
        self.players
            .get(player_id)
            .map(|p| p.session_token == token && !p.is_session_expired(self.session_expiry_hours))
            .unwrap_or(false)
    }
}

impl Default for PokerServer {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use poker_protocol::PlayerAction;

    #[test]
    fn test_server_new() {
        let server = PokerServer::new();
        assert!(server.players.is_empty());
        assert!(server.games.is_empty());
        assert_eq!(server.connection_count, 0);
    }

    #[test]
    fn test_can_accept_connection() {
        let server = PokerServer::new();
        assert!(server.can_accept_connection("127.0.0.1"));
        assert!(server.can_accept_connection("192.168.1.1"));
    }

    #[test]
    fn test_connection_limits() {
        let mut server = PokerServer::new();

        for i in 0..crate::MAX_CONNECTIONS {
            assert!(server.can_accept_connection(&format!("192.168.1.{}", i)));
            server.register_connection(&format!("192.168.1.{}", i));
        }

        assert!(!server.can_accept_connection("10.0.0.1"));
    }

    #[test]
    fn test_per_ip_connection_limits() {
        let mut server = PokerServer::new();

        for _ in 0..crate::MAX_CONNECTIONS_PER_IP {
            assert!(server.can_accept_connection("127.0.0.1"));
            server.register_connection("127.0.0.1");
        }

        assert!(!server.can_accept_connection("127.0.0.1"));
        assert!(server.can_accept_connection("192.168.1.1"));
    }

    #[test]
    fn test_unregister_connection() {
        let mut server = PokerServer::new();

        server.register_connection("127.0.0.1");
        assert_eq!(server.connection_count, 1);

        server.unregister_connection("127.0.0.1");
        assert_eq!(server.connection_count, 0);
    }

    #[test]
    fn test_register_player() {
        let mut server = PokerServer::new();
        server.register_player("player1".to_string(), "TestPlayer".to_string(), 1000);

        assert!(server.players.contains_key("player1"));
        let player = server.players.get("player1").unwrap();
        assert_eq!(player.name, "TestPlayer");
        assert_eq!(player.chips, 1000);
    }

    #[test]
    fn test_create_game() {
        let mut server = PokerServer::new();
        let game = server.create_game("test_game".to_string(), 5, 10);

        assert!(server.games.contains_key("test_game"));
        assert!(game.lock().players.is_empty());
    }

    #[test]
    fn test_verify_session() {
        let mut server = PokerServer::new();
        server.register_player("player1".to_string(), "TestPlayer".to_string(), 1000);

        let token = server.players.get("player1").unwrap().session_token.clone();
        assert!(server.verify_session("player1", &token));
        assert!(!server.verify_session("player1", "wrong_token"));
    }

    #[test]
    fn test_disconnect_player() {
        let mut server = PokerServer::new();
        server.register_player("player1".to_string(), "TestPlayer".to_string(), 1000);
        server.disconnect_player("player1");

        let player = server.players.get("player1").unwrap();
        assert!(!player.connected);
    }

    #[test]
    fn test_session_expiration() {
        let mut server = PokerServer::new();
        server.set_session_expiry_hours(24);
        server.register_player("player1".to_string(), "TestPlayer".to_string(), 1000);

        let token = server.players.get("player1").unwrap().session_token.clone();
        assert!(server.verify_session("player1", &token));

        server.set_session_expiry_hours(0);
        assert!(!server.verify_session("player1", &token));
    }

    #[tokio::test]
    async fn test_handle_reconnect_message() {
        let mut server = PokerServer::new();
        server.register_player("player1".to_string(), "TestPlayer".to_string(), 1000);
        server.create_game("main_table".to_string(), 5, 10);

        {
            let player = server.players.get_mut("player1").unwrap();
            player.connected = false;
        }

        let result =
            server.handle_message("player1", ClientMessage::Reconnect("player1".to_string()));
        assert!(result.is_ok());

        let player = server.players.get("player1").unwrap();
        assert!(player.connected);
    }

    #[tokio::test]
    async fn test_handle_reconnect_invalid_player() {
        let mut server = PokerServer::new();
        server.create_game("main_table".to_string(), 5, 10);

        let result = server.handle_message(
            "nonexistent",
            ClientMessage::Reconnect("nonexistent".to_string()),
        );
        assert!(result.is_err());
        assert!(matches!(
            result.unwrap_err(),
            ServerError::PlayerNotFound(_)
        ));
    }

    #[tokio::test]
    async fn test_handle_action_player_not_in_game() {
        let mut server = PokerServer::new();

        let result = server.handle_message("player1", ClientMessage::Action(PlayerAction::Fold));
        assert!(result.is_err());
        assert!(matches!(result.unwrap_err(), ServerError::PlayerNotInGame));
    }

    #[tokio::test]
    async fn test_handle_chat_message() {
        let mut server = PokerServer::new();
        server.register_player("player1".to_string(), "TestPlayer".to_string(), 1000);
        server.create_game("main_table".to_string(), 5, 10);
        server
            .handle_message("player1", ClientMessage::Connect)
            .unwrap();

        let result = server.handle_message(
            "player1",
            ClientMessage::Chat("Hello everyone!".to_string()),
        );
        assert!(result.is_ok());
    }

    #[tokio::test]
    async fn test_handle_sit_out() {
        let mut server = PokerServer::new();
        server.register_player("player1".to_string(), "TestPlayer".to_string(), 1000);
        server.create_game("main_table".to_string(), 5, 10);
        server
            .handle_message("player1", ClientMessage::Connect)
            .unwrap();

        let result = server.handle_message("player1", ClientMessage::SitOut);
        assert!(result.is_ok());

        let game = server.games.get("main_table").unwrap();
        let poker_game = game.lock();
        let player = poker_game.players.get("player1");
        assert!(player.map(|p| p.is_sitting_out).unwrap_or(false));
    }

    #[tokio::test]
    async fn test_handle_return() {
        let mut server = PokerServer::new();
        server.register_player("player1".to_string(), "TestPlayer".to_string(), 1000);
        server.create_game("main_table".to_string(), 5, 10);
        server
            .handle_message("player1", ClientMessage::Connect)
            .unwrap();
        server
            .handle_message("player1", ClientMessage::SitOut)
            .unwrap();

        let result = server.handle_message("player1", ClientMessage::Return);
        assert!(result.is_ok());

        let game = server.games.get("main_table").unwrap();
        let poker_game = game.lock();
        let player = poker_game.players.get("player1");
        assert!(!player.map(|p| p.is_sitting_out).unwrap_or(true));
    }
}
