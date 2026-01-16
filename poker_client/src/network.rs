use crate::game::{
    ActionRequiredUpdate, ChatMessage, GameStateUpdate, PlayerConnectedUpdate, PlayerUpdate,
    ShowdownUpdate,
};

#[derive(Debug, Clone)]
pub enum NetworkMessage {
    PlayerIdConfirmed(String),
    GameState(GameStateUpdate),
    PlayerUpdates(Vec<PlayerUpdate>),
    ActionRequired(ActionRequiredUpdate),
    PlayerConnected(PlayerConnectedUpdate),
    PlayerDisconnected(String),
    Showdown(ShowdownUpdate),
    Chat(ChatMessage),
    Error(String),
    Ping(u64),
    Pong(()),
}

pub fn parse_message(text: &str) -> Result<NetworkMessage, String> {
    let value: serde_json::Value =
        serde_json::from_str(text).map_err(|e| format!("Failed to parse JSON: {}", e))?;

    let type_str = value.get("type").and_then(|t| t.as_str()).unwrap_or("");

    parse_message_by_type(type_str, &value).map_err(|e| format!("Parse error: {}", e))
}

fn parse_message_by_type(
    type_str: &str,
    value: &serde_json::Value,
) -> Result<NetworkMessage, serde_json::Error> {
    match type_str {
        "GameStateUpdate" | "gamestate" => {
            serde_json::from_value::<GameStateUpdate>(value.clone()).map(NetworkMessage::GameState)
        }
        "PlayerUpdates" | "player_updates" => {
            serde_json::from_value::<Vec<PlayerUpdate>>(value.clone())
                .map(NetworkMessage::PlayerUpdates)
        }
        "ActionRequired" | "action" => {
            serde_json::from_value::<ActionRequiredUpdate>(value.clone())
                .map(NetworkMessage::ActionRequired)
        }
        "PlayerConnected" | "player_connected" => {
            serde_json::from_value::<PlayerConnectedUpdate>(value.clone())
                .map(NetworkMessage::PlayerConnected)
        }
        "PlayerDisconnected" | "player_disconnected" => {
            let player_id = value["player_id"]
                .as_str()
                .or(value["id"].as_str())
                .unwrap_or("unknown")
                .to_string();
            Ok(NetworkMessage::PlayerDisconnected(player_id))
        }
        "Showdown" | "showdown" => {
            serde_json::from_value::<ShowdownUpdate>(value.clone()).map(NetworkMessage::Showdown)
        }
        "Chat" | "chat" => {
            serde_json::from_value::<ChatMessage>(value.clone()).map(NetworkMessage::Chat)
        }
        "Error" | "error" => {
            let error_msg = value["message"]
                .as_str()
                .or(value.as_str())
                .unwrap_or("Unknown error")
                .to_string();
            Ok(NetworkMessage::Error(error_msg))
        }
        "Ping" | "ping" => {
            let timestamp = value["timestamp"]
                .as_u64()
                .or(value["id"].as_u64())
                .unwrap_or(0);
            Ok(NetworkMessage::Ping(timestamp))
        }
        "Pong" | "pong" => {
            let _timestamp = value["timestamp"]
                .as_u64()
                .or(value["id"].as_u64())
                .unwrap_or(0);
            Ok(NetworkMessage::Pong(()))
        }
        "Connected" | "connected" | "Reconnect" | "reconnect" => {
            let player_id = value["player_id"]
                .as_str()
                .or(value["id"].as_str())
                .unwrap_or("unknown")
                .to_string();
            Ok(NetworkMessage::PlayerIdConfirmed(player_id))
        }
        "" => {
            if let Some(connected) = value.get("Connected") {
                let player_id = connected
                    .as_str()
                    .or(value["player_id"].as_str())
                    .or(value["id"].as_str())
                    .unwrap_or("unknown")
                    .to_string();
                return Ok(NetworkMessage::PlayerIdConfirmed(player_id));
            }
            Ok(NetworkMessage::Error(
                "Unknown message format: missing type field".to_string(),
            ))
        }
        _ => Ok(NetworkMessage::Error(format!(
            "Unknown message type: {}",
            type_str
        ))),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_parse_game_state_update() {
        let json = r#"{"type": "GameStateUpdate", "game_id": "test", "hand_number": 1, "pot": 100, "side_pots": [], "community_cards": ["A♥", "K♠"], "current_street": "Flop", "dealer_position": 0}"#;
        let result = parse_message(json);
        assert!(result.is_ok());
        let msg = result.unwrap();
        match msg {
            NetworkMessage::GameState(update) => {
                assert_eq!(update.hand_number, 1);
                assert_eq!(update.pot, 100);
                assert_eq!(update.community_cards.len(), 2);
            }
            _ => panic!("Expected GameState message"),
        }
    }

    #[test]
    fn test_parse_action_required() {
        let json = r#"{"type": "ActionRequired", "player_id": "p1", "player_name": "Player1", "min_raise": 20, "current_bet": 10, "player_chips": 990}"#;
        let result = parse_message(json);
        assert!(result.is_ok());
        let msg = result.unwrap();
        match msg {
            NetworkMessage::ActionRequired(update) => {
                assert_eq!(update.player_id, "p1");
                assert_eq!(update.min_raise, 20);
            }
            _ => panic!("Expected ActionRequired message"),
        }
    }

    #[test]
    fn test_parse_player_connected() {
        let json = r#"{"type": "PlayerConnected", "player_id": "p1", "player_name": "Alice", "chips": 1000}"#;
        let result = parse_message(json);
        assert!(result.is_ok());
        let msg = result.unwrap();
        match msg {
            NetworkMessage::PlayerConnected(update) => {
                assert_eq!(update.player_id, "p1");
                assert_eq!(update.player_name, "Alice");
                assert_eq!(update.chips, 1000);
            }
            _ => panic!("Expected PlayerConnected message"),
        }
    }

    #[test]
    fn test_parse_chat() {
        let json = r#"{"type": "Chat", "player_id": "p1", "player_name": "Alice", "text": "Hello!", "timestamp": 1234567890}"#;
        let result = parse_message(json);
        assert!(result.is_ok());
        let msg = result.unwrap();
        match msg {
            NetworkMessage::Chat(chat) => {
                assert_eq!(chat.text, "Hello!");
            }
            _ => panic!("Expected Chat message"),
        }
    }

    #[test]
    fn test_parse_error() {
        let json = r#"{"type": "Error", "message": "Something went wrong"}"#;
        let result = parse_message(json);
        assert!(result.is_ok());
        let msg = result.unwrap();
        match msg {
            NetworkMessage::Error(err) => {
                assert_eq!(err, "Something went wrong");
            }
            _ => panic!("Expected Error message"),
        }
    }

    #[test]
    fn test_parse_unknown_type() {
        let json = r#"{"type": "UnknownType", "data": "test"}"#;
        let result = parse_message(json);
        assert!(result.is_ok());
        let msg = result.unwrap();
        match msg {
            NetworkMessage::Error(err) => {
                assert!(err.contains("Unknown message type"));
            }
            _ => panic!("Expected Error message for unknown type"),
        }
    }

    #[test]
    fn test_parse_invalid_json() {
        let json = "not valid json";
        let result = parse_message(json);
        assert!(result.is_err());
        let err = result.unwrap_err();
        assert!(err.contains("Failed to parse JSON"));
    }

    #[test]
    fn test_parse_connected_with_player_id() {
        let json = r#"{"type": "Connected", "player_id": "test-player-123"}"#;
        let result = parse_message(json);
        assert!(result.is_ok());
        let msg = result.unwrap();
        match msg {
            NetworkMessage::PlayerIdConfirmed(id) => {
                assert_eq!(id, "test-player-123");
            }
            _ => panic!("Expected PlayerIdConfirmed message"),
        }
    }

    #[test]
    fn test_parse_showdown() {
        let json = r#"{"type": "Showdown", "community_cards": ["A♥", "K♠", "Q♣", "J♦", "10♥"], "hands": [], "winners": ["p1"]}"#;
        let result = parse_message(json);
        assert!(result.is_ok());
        let msg = result.unwrap();
        match msg {
            NetworkMessage::Showdown(update) => {
                assert_eq!(update.winners, vec!["p1"]);
                assert_eq!(update.community_cards.len(), 5);
            }
            _ => panic!("Expected Showdown message"),
        }
    }

    #[test]
    fn test_parse_ping() {
        let json = r#"{"type": "Ping", "timestamp": 1234567890}"#;
        let result = parse_message(json);
        assert!(result.is_ok());
        let msg = result.unwrap();
        match msg {
            NetworkMessage::Ping(timestamp) => {
                assert_eq!(timestamp, 1234567890);
            }
            _ => panic!("Expected Ping message"),
        }
    }

    #[test]
    fn test_parse_pong() {
        let json = r#"{"type": "Pong", "timestamp": 1234567890}"#;
        let result = parse_message(json);
        assert!(result.is_ok());
        let msg = result.unwrap();
        match msg {
            NetworkMessage::Pong(()) => {}
            _ => panic!("Expected Pong message"),
        }
    }
}
