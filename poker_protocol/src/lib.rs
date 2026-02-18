use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::fmt;
use std::sync::Arc;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

mod errors;
mod types;

use parking_lot::Mutex;
use ring::rand::SecureRandom;

pub use errors::{ConnectionError, ProtocolError, ServerError};
pub use types::{Card, GameStage, HandEvaluation, HandRank, PlayerState, Rank, Street, Suit};
pub type ServerResult<T> = std::result::Result<T, ServerError>;

pub const HMAC_SECRET_LEN: usize = 32;
const MESSAGE_TIMESTAMP_MAX_DIFF_MS: u64 = 30000;
const NONCE_CACHE_SIZE: usize = 1000;
const NONCE_EXPIRY_MS: u64 = 60000;

/// Cache for tracking used nonces to prevent replay attacks.
/// Uses a time-based expiry to clean up old entries.
pub struct NonceCache {
    data: Arc<Mutex<HashMap<u64, Instant>>>,
}

impl NonceCache {
    pub fn new() -> Self {
        Self {
            data: Arc::new(Mutex::new(HashMap::new())),
        }
    }

    /// Checks if a nonce has been used recently.
    ///
    /// # Arguments
    /// * `nonce` - The nonce value to check
    ///
    /// # Returns
    /// `true` if the nonce is a duplicate or was recently used, `false` otherwise
    pub fn is_duplicate(&self, nonce: u64) -> bool {
        let now = Instant::now();
        let expiry_duration = Duration::from_millis(NONCE_EXPIRY_MS);
        let mut data = self.data.lock();

        data.retain(|_, &mut timestamp| now.duration_since(timestamp) < expiry_duration);

        if data.contains_key(&nonce) {
            return true;
        }

        while data.len() >= NONCE_CACHE_SIZE {
            if let Some((&oldest_nonce, _)) = data.iter().min_by_key(|(_, timestamp)| *timestamp) {
                data.remove(&oldest_nonce);
            } else {
                break;
            }
        }

        data.insert(nonce, now);
        false
    }

    pub fn clear(&self) {
        let mut data = self.data.lock();
        data.clear();
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
    /// Generates a new random HMAC-SHA256 key.
    ///
    /// # Returns
    /// `Ok(Self)` on success, `Err(ProtocolError::HmacKeyGeneration)` on failure
    pub fn new() -> Result<Self, ProtocolError> {
        let mut array = [0u8; HMAC_SECRET_LEN];
        let rng = ring::rand::SystemRandom::new();
        rng.fill(&mut array)
            .map_err(|_| ProtocolError::HmacKeyGeneration)?;
        Ok(Self(array))
    }

    /// Creates an HMAC key from raw byte data.
    ///
    /// # Arguments
    /// * `bytes` - Raw key bytes (must be at least 32 bytes)
    ///
    /// # Returns
    /// `Some(Self)` if bytes are sufficient, `None` otherwise
    pub fn from_bytes(bytes: &[u8]) -> Option<Self> {
        if bytes.len() >= HMAC_SECRET_LEN {
            let mut array = [0u8; HMAC_SECRET_LEN];
            array.copy_from_slice(&bytes[..HMAC_SECRET_LEN]);
            Some(Self(array))
        } else {
            None
        }
    }

    /// Signs a message using HMAC-SHA256.
    ///
    /// # Arguments
    /// * `message` - The message to sign
    ///
    /// # Returns
    /// The signature as a 32-byte vector
    pub fn sign(&self, message: &str) -> Vec<u8> {
        let key = ring::hmac::Key::new(ring::hmac::HMAC_SHA256, &self.0);
        ring::hmac::sign(&key, message.as_bytes()).as_ref().to_vec()
    }

    /// Verifies an HMAC signature for a message.
    ///
    /// # Arguments
    /// * `message` - The original message that was signed
    /// * `signature` - The signature to verify (must be exactly 32 bytes)
    ///
    /// # Returns
    /// `true` if the signature is valid, `false` otherwise
    pub fn verify(&self, message: &str, signature: &[u8]) -> bool {
        if signature.len() != HMAC_SECRET_LEN {
            return false;
        }
        let key = ring::hmac::Key::new(ring::hmac::HMAC_SHA256, &self.0);
        ring::hmac::verify(&key, message.as_bytes(), signature).is_ok()
    }
}

impl Default for HmacKey {
    fn default() -> Self {
        let mut array = [0u8; HMAC_SECRET_LEN];
        let rng = ring::rand::SystemRandom::new();
        if rng.fill(&mut array).is_err() {
            let _ = getrandom::getrandom(&mut array);
        }
        Self(array)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_nonce_cache_duplicate() {
        let cache = NonceCache::new();
        assert!(!cache.is_duplicate(12345));
        assert!(cache.is_duplicate(12345));
    }

    #[test]
    fn test_nonce_cache_clear() {
        let cache = NonceCache::new();
        assert!(!cache.is_duplicate(12345));
        cache.clear();
        assert!(!cache.is_duplicate(12345));
    }

    #[test]
    fn test_hmac_key_default() {
        let key = HmacKey::default();
        let signature = key.sign("test message");
        assert!(!signature.is_empty());
        assert!(key.verify("test message", &signature));
    }

    #[test]
    fn test_hmac_key_from_bytes() {
        let bytes = vec![1u8; 32];
        let key = HmacKey::from_bytes(&bytes);
        assert!(key.is_some());
    }

    #[test]
    fn test_hmac_key_from_bytes_too_short() {
        let bytes = vec![1u8; 10];
        let key = HmacKey::from_bytes(&bytes);
        assert!(key.is_none());
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

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum PlayerAction {
    Fold,
    Check,
    Call,
    Bet(i32),
    Raise(i32),
    AllIn,
}

impl PlayerAction {
    fn parse_amount_action(
        value: &serde_json::Value,
        key: &str,
        max_chips: Option<i32>,
    ) -> Option<(i32, Self)> {
        let amount = value.get(key).and_then(|v| v.as_i64())?;
        if amount <= 0 {
            return None;
        }
        let amount = amount as i32;
        if let Some(max) = max_chips {
            if amount > max {
                return None;
            }
        }
        let action = match key {
            "Bet" => PlayerAction::Bet(amount),
            "Raise" => PlayerAction::Raise(amount),
            _ => return None,
        };
        Some((amount, action))
    }

    /// Parses a player action from a JSON value.
    ///
    /// # Arguments
    /// * `value` - The JSON value to parse
    /// * `max_chips` - Optional maximum chip limit for bet/raise amounts
    ///
    /// # Returns
    /// `Some(PlayerAction)` if valid, `None` otherwise
    pub fn from_value(value: &serde_json::Value, max_chips: Option<i32>) -> Option<Self> {
        if let Some(action_str) = value.as_str() {
            match action_str {
                "Fold" => Some(PlayerAction::Fold),
                "Check" => Some(PlayerAction::Check),
                "Call" => Some(PlayerAction::Call),
                "AllIn" => Some(PlayerAction::AllIn),
                _ => None,
            }
        } else if let Some((_, action)) = Self::parse_amount_action(value, "Bet", max_chips) {
            Some(action)
        } else if let Some((_, action)) = Self::parse_amount_action(value, "Raise", max_chips) {
            Some(action)
        } else {
            None
        }
    }

    /// Parses a player action from a JSON value with a maximum chip limit.
    ///
    /// # Arguments
    /// * `value` - The JSON value to parse
    /// * `max_chips` - Maximum allowed chips for bet/raise
    ///
    /// # Returns
    /// `Some(PlayerAction)` if valid, `None` otherwise
    pub fn from_value_with_max(value: &serde_json::Value, max_chips: i32) -> Option<Self> {
        Self::from_value(value, Some(max_chips))
    }

    /// Parses a player action from a string.
    ///
    /// # Arguments
    /// * `s` - The string to parse (supports quoted and unquoted)
    ///
    /// # Returns
    /// `Some(PlayerAction)` if valid, `None` otherwise
    pub fn parse_action(s: &str) -> Option<Self> {
        let action_str = s.trim().trim_matches('"');
        match action_str {
            "Fold" => Some(PlayerAction::Fold),
            "Check" => Some(PlayerAction::Check),
            "Call" => Some(PlayerAction::Call),
            "AllIn" => Some(PlayerAction::AllIn),
            _ => None,
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

/// A signed message with HMAC authentication, timestamp, and nonce for replay protection.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SignedMessage {
    /// The JSON-serialized message content
    pub message: String,
    /// The HMAC signature (32 bytes)
    pub signature: Vec<u8>,
    /// Message creation timestamp in milliseconds since UNIX_EPOCH
    pub timestamp: u64,
    /// Unique nonce for this message
    pub nonce: u64,
}

impl SignedMessage {
    /// Creates a new signed message.
    pub fn new(message: String, signature: Vec<u8>, timestamp: u64, nonce: u64) -> Self {
        Self {
            message,
            signature,
            timestamp,
            nonce,
        }
    }

    /// Creates and signs a new message.
    ///
    /// # Arguments
    /// * `message` - The client message to sign
    /// * `key` - The HMAC key to use for signing
    /// * `nonce` - A unique nonce for this message
    ///
    /// # Returns
    /// `Ok(Self)` on success, `Err(ProtocolError)` on serialization failure
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

    /// Verifies a signed message's authenticity and freshness.
    ///
    /// # Arguments
    /// * `key` - The HMAC key to verify the signature
    /// * `nonce_cache` - Cache to check for duplicate/nonce reuse
    ///
    /// # Returns
    /// `Ok(ClientMessage)` on success, `Err(ProtocolError)` on failure
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

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
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

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
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

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct GameStateUpdate {
    pub game_id: String,
    pub hand_number: i32,
    pub pot: i32,
    pub side_pots: Vec<(i32, Vec<String>)>,
    pub community_cards: Vec<String>,
    pub current_street: String,
    pub dealer_position: usize,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
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

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ActionRequiredUpdate {
    pub player_id: String,
    pub player_name: String,
    pub min_raise: i32,
    pub current_bet: i32,
    pub player_chips: i32,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PlayerConnectedUpdate {
    pub player_id: String,
    pub player_name: String,
    pub chips: i32,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PlayerDisconnectedUpdate {
    pub player_id: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ShowdownUpdate {
    pub community_cards: Vec<String>,
    pub hands: Vec<(String, Vec<String>, String, String)>,
    pub winners: Vec<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ChatMessage {
    pub player_id: String,
    pub player_name: String,
    pub text: String,
    pub timestamp: u64,
}

impl ServerMessage {
    /// Converts the message to a unified JSON format with a "type" field.
    ///
    /// # Returns
    /// `Ok(JSON string)` on success, `Err(ProtocolError::JsonSerialize)` on failure
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
                        result.extend(obj.iter().map(|(k, v)| (k.clone(), v.clone())));
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
                        result.extend(obj.iter().map(|(k, v)| (k.clone(), v.clone())));
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
                        result.extend(obj.iter().map(|(k, v)| (k.clone(), v.clone())));
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
                        result.extend(obj.iter().map(|(k, v)| (k.clone(), v.clone())));
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
