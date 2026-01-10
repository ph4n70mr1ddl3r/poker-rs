use crate::game::PokerGame;
use log::error;
use parking_lot::Mutex;
use poker_protocol::{ChatMessage, ClientMessage, PlayerUpdate, ServerMessage};
use poker_protocol::{ServerError, ServerResult};
use std::collections::HashMap;
use std::sync::Arc;
use tokio::sync::broadcast;
use tokio::sync::mpsc::Sender;
use tokio::time::{timeout, Duration};
use uuid::Uuid;

fn lock_game(game: &Arc<Mutex<PokerGame>>) -> parking_lot::MutexGuard<'_, PokerGame> {
    game.lock()
}

pub const MAX_CONNECTIONS: usize = 100;
pub const MAX_CONNECTIONS_PER_IP: usize = 5;
const BROADCAST_SEND_TIMEOUT_MS: u64 = 5000;

pub type PlayerId = String;

#[derive(Debug, Clone)]
pub struct ServerPlayer {
    pub name: String,
    pub chips: i32,
    pub connected: bool,
    pub ws_sender: Option<Sender<String>>,
    pub seated: bool,
    pub session_token: String,
}

impl ServerPlayer {
    pub fn new(_id: PlayerId, name: String, chips: i32) -> Self {
        Self {
            name,
            chips,
            connected: false,
            ws_sender: None,
            seated: false,
            session_token: Uuid::new_v4().to_string(),
        }
    }
}

pub struct PokerServer {
    players: HashMap<PlayerId, ServerPlayer>,
    games: HashMap<String, Arc<Mutex<PokerGame>>>,
    player_sessions: HashMap<PlayerId, String>,
    tx: broadcast::Sender<ServerMessage>,
    connection_count: usize,
    ip_connections: HashMap<String, usize>,
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
        }
    }

    /// Checks if a new connection can be accepted from the given IP.
    ///
    /// # Arguments
    /// * `ip` - The IP address of the incoming connection
    ///
    /// # Returns
    /// `true` if the connection can be accepted, `false` otherwise
    pub fn can_accept_connection(&self, ip: &str) -> bool {
        self.connection_count < MAX_CONNECTIONS
            && self
                .ip_connections
                .get(ip)
                .map(|c| *c < MAX_CONNECTIONS_PER_IP)
                .unwrap_or(true)
    }

    pub fn register_connection(&mut self, ip: &str) {
        self.connection_count += 1;
        *self.ip_connections.entry(ip.to_string()).or_insert(0) += 1;
    }

    pub fn unregister_connection(&mut self, ip: &str) {
        self.connection_count = self.connection_count.saturating_sub(1);
        if let Some(count) = self.ip_connections.get_mut(ip) {
            *count = count.saturating_sub(1);
            if *count == 0 {
                self.ip_connections.remove(ip);
            }
        }
    }

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

    /// Registers a new player with the server (without connecting).
    ///
    /// # Arguments
    /// * `player_id` - Unique player identifier
    /// * `name` - Player's display name
    /// * `chips` - Starting chip amount
    pub fn register_player(&mut self, player_id: PlayerId, name: String, chips: i32) {
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
            player.session_token = Uuid::new_v4().to_string();
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

        let mut poker_game = lock_game(game);

        poker_game.add_player(player_id.to_string(), player.name.clone(), player.chips);

        drop(poker_game);

        let connected_msg = ServerMessage::Connected(player_id.to_string());
        let json = serde_json::to_string(&connected_msg)
            .map_err(|e| ServerError::GameState(e.to_string()))?;
        self.send_to_player(player_id, json);

        self.send_game_state_to_player(player_id, game_id)?;

        Ok(())
    }

    fn send_game_state_to_player(&self, player_id: &str, game_id: &str) -> ServerResult<()> {
        let game = self
            .games
            .get(game_id)
            .ok_or(ServerError::GameNotFound(game_id.to_string()))?;
        let poker_game = lock_game(game);

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
        let json = serde_json::to_string(&game_state)
            .map_err(|e| ServerError::GameState(e.to_string()))?;
        self.send_to_player(player_id, json);

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
                if let Some(player) = self.players.get_mut(&existing_player_id) {
                    player.connected = true;
                    player.session_token = Uuid::new_v4().to_string();
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
                    let mut poker_game = lock_game(game);
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
                    timestamp: chrono::Utc::now().timestamp(),
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

                if let Some(game) = self.games.get(&session) {
                    let mut poker_game = lock_game(game);
                    poker_game.sit_out(player_id);
                }
            }
            ClientMessage::Return => {
                let session = self
                    .player_sessions
                    .get(player_id)
                    .ok_or(ServerError::PlayerNotInGame)?
                    .clone();

                if let Some(game) = self.games.get(&session) {
                    let mut poker_game = lock_game(game);
                    poker_game.return_to_game(player_id);
                }
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
        let json = match serde_json::to_string(&message) {
            Ok(json) => json,
            Err(e) => {
                error!("Failed to serialize message: {}", e);
                return;
            }
        };

        let game = self.games.get(game_id);
        if let Some(game) = game {
            let pg = lock_game(game);

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

            drop(pg);

            if players.is_empty() {
                return;
            }

            let timeout_duration = Duration::from_millis(BROADCAST_SEND_TIMEOUT_MS);
            let msg_for_send = json;

            for (player_id, sender) in players {
                let msg = msg_for_send.clone();
                let sender = sender.clone();
                let player_id = player_id.clone();
                tokio::spawn(async move {
                    if let Err(e) = tokio::time::timeout(timeout_duration, sender.send(msg)).await {
                        error!("Timeout sending to player {}: {}", player_id, e);
                    }
                });
            }
        }
    }

    /// Sends a message to a specific player.
    ///
    /// # Arguments
    /// * `player_id` - The target player
    /// * `message` - The message to send
    pub fn send_to_player(&self, player_id: &str, message: String) {
        if let Some(player) = self.players.get(player_id) {
            if let Some(ref sender) = player.ws_sender {
                let sender = sender.clone();
                let player_id = player_id.to_string();
                let _ = tokio::spawn(async move {
                    if let Err(e) = sender.send(message).await {
                        error!("Failed to send message to player {}: {}", player_id, e);
                    }
                });
            }
        }
    }

    pub fn verify_session(&self, player_id: &str, token: &str) -> bool {
        self.players
            .get(player_id)
            .map(|p| p.session_token == token)
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

        for i in 0..MAX_CONNECTIONS {
            assert!(server.can_accept_connection(&format!("192.168.1.{}", i)));
            server.register_connection(&format!("192.168.1.{}", i));
        }

        assert!(!server.can_accept_connection("10.0.0.1"));
    }

    #[test]
    fn test_per_ip_connection_limits() {
        let mut server = PokerServer::new();

        for i in 0..MAX_CONNECTIONS_PER_IP {
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
}
