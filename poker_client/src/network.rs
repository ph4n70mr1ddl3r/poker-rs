use crate::game::{
    ActionRequiredUpdate, ChatMessage, GameStateUpdate, PlayerConnectedUpdate, PlayerUpdate,
    ShowdownUpdate,
};
use serde_json;

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
}

pub fn parse_message(text: &str) -> Result<NetworkMessage, serde_json::Error> {
    if let Ok(value) = serde_json::from_str::<serde_json::Value>(text) {
        if let Some(type_obj) = value.get("type") {
            if let Some(type_str) = type_obj.as_str() {
                return parse_by_type(type_str, &value);
            }
        }

        Ok(NetworkMessage::Error("Unknown message format".to_string()))
    } else {
        Ok(NetworkMessage::Error("Failed to parse JSON".to_string()))
    }
}

fn parse_by_type(
    type_str: &str,
    value: &serde_json::Value,
) -> Result<NetworkMessage, serde_json::Error> {
    match type_str {
        "GameStateUpdate" | "gamestate" => {
            serde_json::from_value::<GameStateUpdate>(value.clone()).map(NetworkMessage::GameState)
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
        "Connected" | "connected" => {
            let player_id = value["player_id"]
                .as_str()
                .or(value["id"].as_str())
                .unwrap_or("unknown")
                .to_string();
            Ok(NetworkMessage::PlayerIdConfirmed(player_id))
        }
        "Reconnect" | "reconnect" => {
            let player_id = value["player_id"]
                .as_str()
                .or(value["id"].as_str())
                .unwrap_or("unknown")
                .to_string();
            Ok(NetworkMessage::PlayerIdConfirmed(player_id))
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
            _ => panic!("Expected GameState"),
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
            _ => panic!("Expected ActionRequired"),
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
            _ => panic!("Expected PlayerConnected"),
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
            _ => panic!("Expected Chat"),
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
            _ => panic!("Expected Error"),
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
            _ => panic!("Expected Error"),
        }
    }

    #[test]
    fn test_parse_invalid_json() {
        let json = "not valid json";
        let result = parse_message(json);
        assert!(result.is_ok());
        let msg = result.unwrap();
        match msg {
            NetworkMessage::Error(err) => {
                assert!(err.contains("Failed to parse JSON"));
            }
            _ => panic!("Expected Error"),
        }
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
            _ => panic!("Expected PlayerIdConfirmed"),
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
            _ => panic!("Expected Showdown"),
        }
    }
}
