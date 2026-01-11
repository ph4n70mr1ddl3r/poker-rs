use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::fmt;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

mod errors;
mod types;

pub use errors::{ConnectionError, ProtocolError, ServerError};
pub use types::{
    Card, GameStage, GameState, HandEvaluation, HandRank, Player, PlayerState, Rank, Street, Suit,
};
pub type ServerResult<T> = std::result::Result<T, ServerError>;

const HMAC_SECRET_LEN: usize = 32;
const MESSAGE_TIMESTAMP_MAX_DIFF_MS: u64 = 30000;
const NONCE_CACHE_SIZE: usize = 1000;
const NONCE_EXPIRY_MS: u64 = 60000;

pub struct NonceCache {
    data: Arc<Mutex<HashMap<u64, Instant>>>,
}

impl NonceCache {
    pub fn new() -> Self {
        Self {
            data: Arc::new(Mutex::new(HashMap::new())),
        }
    }

    pub fn is_duplicate(&self, nonce: u64) -> bool {
        let now = Instant::now();
        let expiry_duration = Duration::from_millis(NONCE_EXPIRY_MS);
        let mut data = match self.data.lock() {
            Ok(guard) => guard,
            Err(poisoned) => poisoned.into_inner(),
        };

        if data.contains_key(&nonce) {
            return true;
        }

        data.retain(|_, &mut timestamp| now.duration_since(timestamp) < expiry_duration);

        if data.len() >= NONCE_CACHE_SIZE {
            if let Some(oldest_nonce) = data
                .iter()
                .min_by_key(|(_, timestamp)| *timestamp)
                .map(|(nonce, _)| *nonce)
            {
                data.remove(&oldest_nonce);
            }
        }

        data.insert(nonce, now);
        false
    }
}

impl Default for NonceCache {
    fn default() -> Self {
        Self::new()
    }
}

#[derive(Debug, Clone)]
pub struct HmacKey([u8; HMAC_SECRET_LEN]);

impl HmacKey {
    pub fn new() -> Result<Self, ProtocolError> {
        ring::hmac::Key::generate(ring::hmac::HMAC_SHA256, &ring::rand::SystemRandom::new())
            .map_err(|_| ProtocolError::HmacKeyGeneration)
            .and_then(|key| {
                let tag = ring::hmac::sign(&key, &[]);
                let key_bytes = tag.as_ref();
                if key_bytes.len() >= HMAC_SECRET_LEN {
                    let mut array = [0u8; HMAC_SECRET_LEN];
                    array.copy_from_slice(&key_bytes[..HMAC_SECRET_LEN]);
                    Ok(Self(array))
                } else {
                    Err(ProtocolError::HmacKeyGeneration)
                }
            })
    }

    pub fn from_bytes(bytes: &[u8]) -> Option<Self> {
        if bytes.len() >= HMAC_SECRET_LEN {
            let mut array = [0u8; HMAC_SECRET_LEN];
            array.copy_from_slice(&bytes[..HMAC_SECRET_LEN]);
            Some(Self(array))
        } else {
            None
        }
    }

    pub fn sign(&self, message: &str) -> Vec<u8> {
        let key = ring::hmac::Key::new(ring::hmac::HMAC_SHA256, &self.0);
        ring::hmac::sign(&key, message.as_bytes()).as_ref().to_vec()
    }

    pub fn verify(&self, message: &str, signature: &[u8]) -> bool {
        if signature.len() != 32 {
            return false;
        }
        let key = ring::hmac::Key::new(ring::hmac::HMAC_SHA256, &self.0);
        ring::hmac::verify(&key, message.as_bytes(), signature).is_ok()
    }
}

impl Default for HmacKey {
    fn default() -> Self {
        Self::new().unwrap_or_else(|_| {
            let mut array = [0u8; HMAC_SECRET_LEN];
            for i in 0..HMAC_SECRET_LEN {
                array[i] = i as u8;
            }
            Self(array)
        })
    }
}

/// # Poker Protocol
///
/// All messages are JSON-encoded and sent over WebSocket.
///
/// ## Client Messages (unified format with "type" field)
///
/// ```json
/// {"type": "Connect"}
/// ```
/// Connect to the server and join the game.
///
/// ```json
/// {"type": "Action", "action": "Fold"}
/// {"type": "Action", "action": "Check"}
/// {"type": "Action", "action": "Call"}
/// {"type": "Action", "action": "AllIn"}
/// {"type": "Action", "action": "Bet", "amount": 100}
/// {"type": "Action", "action": "Raise", "amount": 50}
/// ```
/// Perform a poker action.
///
/// ```json
/// {"type": "Chat", "text": "Hello everyone!"}
/// ```
/// Send a chat message.
///
/// ```json
/// {"type": "SitOut"}
/// ```
/// Sit out from the current hand.
///
/// ```json
/// {"type": "Return"}
/// ```
/// Return to the game after sitting out.
///
/// ```json
/// {"type": "Ping", "timestamp": 1234567890}
/// ```
/// Send a ping for keep-alive.
///
/// ## Server Messages
///
/// ```json
/// {"type": "Connected", "player_id": "player_id_here"}
/// ```
/// Confirmation of connection with player ID.
///
/// ```json
/// {"type": "GameStateUpdate", "game_id": "main_table", "hand_number": 1, "pot": 0, "side_pots": [], "community_cards": [], "current_street": "Pre-Flop", "dealer_position": 0}
/// ```
/// Current game state update.
///
/// ```json
/// {"type": "PlayerUpdates", "players": [{"player_id": "...", "player_name": "Player1", "chips": 1000, "current_bet": 0, "has_acted": false, "is_all_in": false, "is_folded": false, "is_sitting_out": false, "hole_cards": ["[hidden]"]}]}
/// ```
/// Update on all players' states.
///
/// ```json
/// {"type": "ActionRequired", "player_id": "...", "player_name": "Player1", "min_raise": 20, "current_bet": 10, "player_chips": 990}
/// ```
/// Request for player action.
///
/// ```json
/// {"type": "PlayerConnected", "player_id": "...", "player_name": "Player1", "chips": 1000}
/// ```
/// Notification of a new player connecting.
///
/// ```json
/// {"type": "PlayerDisconnected", "player_id": "..."}
/// ```
/// Notification of a player disconnecting.
///
/// ```json
/// {"type": "Showdown", "community_cards": ["A♥", "K♠", "Q♦"], "hands": [["player_id", ["A♥", "K♠"], "Pair", "Pair of Aces"]], "winners": ["player_id"]}
/// ```
/// Showdown results after the final betting round.
///
/// ```json
/// {"type": "Chat", "player_id": "...", "player_name": "Player1", "text": "Hello!", "timestamp": 1234567890}
/// ```
/// Chat message from another player.
///
/// ```json
/// {"type": "Error", "message": "Invalid bet amount"}
/// ```
/// Error message from the server.
///

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub enum PlayerAction {
    Fold,
    Check,
    Call,
    Bet(i32),
    Raise(i32),
    AllIn,
}

impl PlayerAction {
    pub fn from_value(value: &serde_json::Value, max_chips: Option<i32>) -> Option<Self> {
        if let Some(action_str) = value.as_str() {
            match action_str {
                "Fold" => Some(PlayerAction::Fold),
                "Check" => Some(PlayerAction::Check),
                "Call" => Some(PlayerAction::Call),
                "AllIn" => Some(PlayerAction::AllIn),
                _ => None,
            }
        } else if let Some(bet_amount) = value.get("Bet").and_then(|v| v.as_i64()) {
            if bet_amount > 0 {
                let amount = bet_amount as i32;
                if let Some(max) = max_chips {
                    if amount > max {
                        return None;
                    }
                }
                Some(PlayerAction::Bet(amount))
            } else {
                None
            }
        } else if let Some(raise_amount) = value.get("Raise").and_then(|v| v.as_i64()) {
            if raise_amount > 0 {
                let amount = raise_amount as i32;
                if let Some(max) = max_chips {
                    if amount > max {
                        return None;
                    }
                }
                Some(PlayerAction::Raise(amount))
            } else {
                None
            }
        } else {
            None
        }
    }

    pub fn from_value_with_max(value: &serde_json::Value, max_chips: i32) -> Option<Self> {
        Self::from_value(value, Some(max_chips))
    }

    pub fn parse_action(s: &str) -> Option<Self> {
        if let Some(action_str) = s.strip_prefix('\"').and_then(|s| s.strip_suffix('\"')) {
            match action_str {
                "Fold" => Some(PlayerAction::Fold),
                "Check" => Some(PlayerAction::Check),
                "Call" => Some(PlayerAction::Call),
                "AllIn" => Some(PlayerAction::AllIn),
                _ => None,
            }
        } else {
            match s {
                "Fold" => Some(PlayerAction::Fold),
                "Check" => Some(PlayerAction::Check),
                "Call" => Some(PlayerAction::Call),
                "AllIn" => Some(PlayerAction::AllIn),
                _ => None,
            }
        }
    }
}

impl fmt::Display for PlayerAction {
    fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
        match self {
            PlayerAction::Fold => write!(f, "Fold"),
            PlayerAction::Check => write!(f, "Check"),
            PlayerAction::Call => write!(f, "Call"),
            PlayerAction::Bet(amount) => write!(f, "Bet({})", amount),
            PlayerAction::Raise(amount) => write!(f, "Raise({})", amount),
            PlayerAction::AllIn => write!(f, "AllIn"),
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SignedMessage {
    pub message: String,
    pub signature: Vec<u8>,
    pub timestamp: u64,
    pub nonce: u64,
}

impl SignedMessage {
    pub fn new(message: String, signature: Vec<u8>, timestamp: u64, nonce: u64) -> Self {
        Self {
            message,
            signature,
            timestamp,
            nonce,
        }
    }

    pub fn create(
        message: &ClientMessage,
        key: &HmacKey,
        nonce: u64,
    ) -> Result<Self, ProtocolError> {
        let timestamp = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map_err(|_| ProtocolError::TimestampError)?
            .as_millis() as u64;

        let message_json =
            serde_json::to_string(message).map_err(|_| ProtocolError::JsonSerialize)?;

        let signature = key.sign(&format!("{}{}{}", timestamp, nonce, message_json));

        Ok(Self {
            message: message_json,
            signature,
            timestamp,
            nonce,
        })
    }

    pub fn verify(
        &self,
        key: &HmacKey,
        nonce_cache: &NonceCache,
    ) -> Result<ClientMessage, ProtocolError> {
        let now = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map_err(|_| ProtocolError::TimestampError)?
            .as_millis() as u64;

        if now.saturating_sub(self.timestamp) > MESSAGE_TIMESTAMP_MAX_DIFF_MS {
            return Err(ProtocolError::MessageExpired);
        }

        if nonce_cache.is_duplicate(self.nonce) {
            return Err(ProtocolError::DuplicateNonce);
        }

        if !key.verify(
            &format!("{}{}{}", self.timestamp, self.nonce, self.message),
            &self.signature,
        ) {
            return Err(ProtocolError::InvalidSignature);
        }

        serde_json::from_str(&self.message)
            .map_err(|e| ProtocolError::JsonDeserialize(e.to_string()))
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub enum ClientMessage {
    Connect,
    Reconnect(String),
    Action(PlayerAction),
    Chat(String),
    SitOut,
    Return,
}

impl fmt::Display for ClientMessage {
    fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
        match self {
            ClientMessage::Connect => write!(f, "Connect"),
            ClientMessage::Reconnect(id) => write!(f, "Reconnect({})", id),
            ClientMessage::Action(a) => write!(f, "Action({})", a),
            ClientMessage::Chat(t) => write!(f, "Chat({})", t),
            ClientMessage::SitOut => write!(f, "SitOut"),
            ClientMessage::Return => write!(f, "Return"),
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub enum ServerMessage {
    Connected(String),
    Ping(u64),
    Pong(u64),
    GameStateUpdate(GameStateUpdate),
    PlayerUpdates(Vec<PlayerUpdate>),
    ActionRequired(ActionRequiredUpdate),
    PlayerConnected(PlayerConnectedUpdate),
    PlayerDisconnected(PlayerDisconnectedUpdate),
    Showdown(ShowdownUpdate),
    Chat(ChatMessage),
    Error(String),
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct GameStateUpdate {
    pub game_id: String,
    pub hand_number: i32,
    pub pot: i32,
    pub side_pots: Vec<(i32, Vec<String>)>,
    pub community_cards: Vec<String>,
    pub current_street: String,
    pub dealer_position: usize,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PlayerUpdate {
    pub player_id: String,
    pub player_name: String,
    pub chips: i32,
    pub current_bet: i32,
    pub has_acted: bool,
    pub is_all_in: bool,
    pub is_folded: bool,
    pub is_sitting_out: bool,
    pub hole_cards: Vec<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ActionRequiredUpdate {
    pub player_id: String,
    pub player_name: String,
    pub min_raise: i32,
    pub current_bet: i32,
    pub player_chips: i32,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PlayerConnectedUpdate {
    pub player_id: String,
    pub player_name: String,
    pub chips: i32,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PlayerDisconnectedUpdate {
    pub player_id: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ShowdownUpdate {
    pub community_cards: Vec<String>,
    pub hands: Vec<(String, Vec<String>, String, String)>,
    pub winners: Vec<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ChatMessage {
    pub player_id: String,
    pub player_name: String,
    pub text: String,
    pub timestamp: i64,
}

impl ServerMessage {
    pub fn to_unified_json(&self) -> Result<String, ProtocolError> {
        let value = match self {
            ServerMessage::Connected(player_id) => {
                serde_json::json!({ "type": "Connected", "player_id": player_id })
            }
            ServerMessage::Ping(timestamp) => {
                serde_json::json!({ "type": "Ping", "timestamp": timestamp })
            }
            ServerMessage::Pong(timestamp) => {
                serde_json::json!({ "type": "Pong", "timestamp": timestamp })
            }
            ServerMessage::GameStateUpdate(update) => serde_json::to_value(update)
                .map_err(|_| ProtocolError::JsonSerialize)?
                .as_object()
                .map_or_else(
                    || Err(ProtocolError::JsonSerialize),
                    |obj| {
                        let mut result = serde_json::Map::new();
                        result.insert(
                            "type".to_string(),
                            serde_json::Value::String("GameStateUpdate".to_string()),
                        );
                        result.extend(obj.clone());
                        Ok(serde_json::Value::Object(result))
                    },
                )?,
            ServerMessage::PlayerUpdates(updates) => {
                let updates_json: Vec<serde_json::Value> = updates
                    .iter()
                    .map(serde_json::to_value)
                    .collect::<Result<_, _>>()
                    .map_err(|_| ProtocolError::JsonSerialize)?;
                serde_json::json!({
                    "type": "PlayerUpdates",
                    "players": updates_json
                })
            }
            ServerMessage::ActionRequired(update) => serde_json::to_value(update)
                .map_err(|_| ProtocolError::JsonSerialize)?
                .as_object()
                .map_or_else(
                    || Err(ProtocolError::JsonSerialize),
                    |obj| {
                        let mut result = serde_json::Map::new();
                        result.insert(
                            "type".to_string(),
                            serde_json::Value::String("ActionRequired".to_string()),
                        );
                        result.extend(obj.clone());
                        Ok(serde_json::Value::Object(result))
                    },
                )?,
            ServerMessage::PlayerConnected(update) => serde_json::to_value(update)
                .map_err(|_| ProtocolError::JsonSerialize)?
                .as_object()
                .map_or_else(
                    || Err(ProtocolError::JsonSerialize),
                    |obj| {
                        let mut result = serde_json::Map::new();
                        result.insert(
                            "type".to_string(),
                            serde_json::Value::String("PlayerConnected".to_string()),
                        );
                        result.extend(obj.clone());
                        Ok(serde_json::Value::Object(result))
                    },
                )?,
            ServerMessage::PlayerDisconnected(update) => {
                serde_json::json!({
                    "type": "PlayerDisconnected",
                    "player_id": update.player_id
                })
            }
            ServerMessage::Showdown(update) => serde_json::to_value(update)
                .map_err(|_| ProtocolError::JsonSerialize)?
                .as_object()
                .map_or_else(
                    || Err(ProtocolError::JsonSerialize),
                    |obj| {
                        let mut result = serde_json::Map::new();
                        result.insert(
                            "type".to_string(),
                            serde_json::Value::String("Showdown".to_string()),
                        );
                        result.extend(obj.clone());
                        Ok(serde_json::Value::Object(result))
                    },
                )?,
            ServerMessage::Chat(msg) => {
                serde_json::json!({
                    "type": "Chat",
                    "player_id": msg.player_id,
                    "player_name": msg.player_name,
                    "text": msg.text,
                    "timestamp": msg.timestamp
                })
            }
            ServerMessage::Error(err_msg) => {
                serde_json::json!({
                    "type": "Error",
                    "message": err_msg
                })
            }
        };
        serde_json::to_string(&value).map_err(|_| ProtocolError::JsonSerialize)
    }
}
