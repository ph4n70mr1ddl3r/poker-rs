use thiserror::Error;

#[derive(Debug, Error)]
pub enum ProtocolError {
    #[error("JSON serialization error")]
    JsonSerialize,

    #[error("JSON deserialization error: {0}")]
    JsonDeserialize(String),

    #[error("Invalid message type: {0}")]
    InvalidMessageType(String),

    #[error("Missing required field: {0}")]
    MissingField(String),

    #[error("Invalid action amount: {0}")]
    InvalidAmount(String),
}

impl From<serde_json::Error> for ProtocolError {
    fn from(e: serde_json::Error) -> Self {
        ProtocolError::JsonDeserialize(e.to_string())
    }
}

#[derive(Debug, Error)]
pub enum ServerError {
    #[error("Player not found: {0}")]
    PlayerNotFound(String),

    #[error("Game not found: {0}")]
    GameNotFound(String),

    #[error("Player not in a game")]
    PlayerNotInGame,

    #[error("Not your turn")]
    NotYourTurn,

    #[error("Cannot check, must call")]
    CannotCheck,

    #[error("Cannot bet, must call or raise")]
    CannotBet,

    #[error("Cannot raise, must call first")]
    CannotRaise,

    #[error("Invalid bet amount: {0}")]
    InvalidBet(String),

    #[error("Invalid raise amount: {0}")]
    InvalidRaise(String),

    #[error("Minimum bet is {0}")]
    MinBet(i32),

    #[error("Minimum raise is to {0}")]
    MinRaise(i32),

    #[error("Bet amount {0} exceeds your chips ({1})")]
    BetExceedsChips(i32, i32),

    #[error("Raise requires {0} more chips, but you only have {1}")]
    RaiseInsufficientChips(i32, i32),

    #[error("Player has no chips")]
    NoChips,

    #[error("Amount must be positive")]
    InvalidAmount,

    #[error("Amount exceeds maximum allowed: {0}")]
    AmountExceedsMax(i32),

    #[error("Amount too large")]
    AmountTooLarge,

    #[error("Mutex lock failed")]
    LockFailed,

    #[error("Game state error: {0}")]
    GameState(String),
}

impl From<String> for ServerError {
    fn from(s: String) -> Self {
        ServerError::GameState(s)
    }
}

impl From<&str> for ServerError {
    fn from(s: &str) -> Self {
        ServerError::GameState(s.to_string())
    }
}

#[derive(Debug, Error)]
pub enum ConnectionError {
    #[error("Connection refused: {0}")]
    ConnectionRefused(String),

    #[error("Connection timeout")]
    Timeout,

    #[error("WebSocket error: {0}")]
    WebSocket(String),

    #[error("Disconnected")]
    Disconnected,

    #[error("Server error: {0}")]
    Server(String),
}
