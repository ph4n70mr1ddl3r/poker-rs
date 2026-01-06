use crate::game::PokerGame;
use log::{debug, error};
use parking_lot::Mutex;
use poker_protocol::{ChatMessage, ClientMessage, PlayerUpdate, ServerMessage};
use poker_protocol::{ServerError, ServerResult};
use std::collections::HashMap;
use std::sync::Arc;
use tokio::sync::broadcast;
use tokio::sync::mpsc::Sender;

pub type PlayerId = String;

fn lock_server<'a>(
    server: &'a Arc<Mutex<PokerServer>>,
) -> parking_lot::MutexGuard<'a, PokerServer> {
    server.lock()
}

fn lock_game<'a>(game: &'a Arc<Mutex<PokerGame>>) -> parking_lot::MutexGuard<'a, PokerGame> {
    game.lock()
}

#[derive(Debug, Clone)]
pub struct ServerPlayer {
    pub name: String,
    pub chips: i32,
    pub connected: bool,
    pub ws_sender: Option<Sender<String>>,
    pub seated: bool,
}

impl ServerPlayer {
    pub fn new(_id: PlayerId, name: String, chips: i32) -> Self {
        Self {
            name,
            chips,
            connected: false,
            ws_sender: None,
            seated: false,
        }
    }
}

pub struct PokerServer {
    players: HashMap<PlayerId, ServerPlayer>,
    games: HashMap<String, Arc<Mutex<PokerGame>>>,
    player_sessions: HashMap<PlayerId, String>,
    tx: broadcast::Sender<ServerMessage>,
}

impl PokerServer {
    pub fn new() -> Self {
        Self {
            players: HashMap::new(),
            games: HashMap::new(),
            player_sessions: HashMap::new(),
            tx: broadcast::channel(100).0,
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

    pub fn register_player(&mut self, player_id: PlayerId, name: String, chips: i32) {
        let player = ServerPlayer::new(player_id.clone(), name, chips);
        self.players.insert(player_id, player);
    }

    pub fn connect_player(&mut self, player_id: &str, ws_sender: Sender<String>) {
        if let Some(player) = self.players.get_mut(player_id) {
            player.connected = true;
            player.ws_sender = Some(ws_sender);
        }
    }

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
            .map(|p| PlayerUpdate {
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

    pub fn broadcast_to_game(&self, game_id: &str, message: ServerMessage) {
        let json = match serde_json::to_string(&message) {
            Ok(json) => json,
            Err(e) => {
                error!("Failed to serialize message: {}", e);
                return;
            }
        };
        debug!(
            "Broadcasting to game {}: {} (len={})",
            game_id,
            json,
            json.len()
        );
        let json = Arc::new(json);
        let game = self.games.get(game_id);
        if let Some(game) = game {
            let pg = lock_game(game);
            let players = pg.get_players();
            debug!("Game has {} players", players.len());
            let json = Arc::clone(&json);
            for player_id in players.keys() {
                debug!("Checking player {}", player_id);
                if let Some(player) = self.players.get(player_id) {
                    if player.connected {
                        if let Some(ref sender) = player.ws_sender {
                            debug!(
                                "Sending to player {}: {} (len={})",
                                player_id,
                                json,
                                json.len()
                            );
                            let sender = sender.clone();
                            let send_json = Arc::clone(&json);
                            tokio::spawn(async move {
                                if let Err(e) = sender.send(send_json.as_str().to_string()).await {
                                    debug!("Failed to send to player: {}", e);
                                }
                            });
                        } else {
                            debug!("Player {} has no sender", player_id);
                        }
                    } else {
                        debug!("Player {} is not connected", player_id);
                    }
                } else {
                    debug!("Player {} not found in players map", player_id);
                }
            }
        } else {
            debug!("Game {} not found", game_id);
        }
    }

    pub fn send_to_player(&self, player_id: &str, message: String) {
        if let Some(player) = self.players.get(player_id) {
            if let Some(ref sender) = player.ws_sender {
                let sender = sender.clone();
                tokio::spawn(async move {
                    let _ = sender.send(message).await;
                });
            }
        }
    }
}

impl Default for PokerServer {
    fn default() -> Self {
        Self::new()
    }
}
