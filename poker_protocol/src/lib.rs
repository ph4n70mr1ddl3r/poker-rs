use serde::{Deserialize, Serialize};
use std::fmt;

mod types;

pub use types::{
    Card, GameStage, GameState, HandEvaluation, HandRank, Player, PlayerState, Rank, Street, Suit,
};

#[derive(Debug, Clone, Serialize, Deserialize)]
pub enum ClientMessage {
    Connect,
    Action(PlayerAction),
    Chat(String),
    SitOut,
    Return,
}

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
    pub fn from_str(s: &str) -> Option<Self> {
        match s {
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

impl fmt::Display for ClientMessage {
    fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
        match self {
            ClientMessage::Connect => write!(f, "Connect"),
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
