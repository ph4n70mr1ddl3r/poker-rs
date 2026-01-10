use std::collections::{HashMap, VecDeque};
use std::sync::Mutex;

pub use poker_protocol::{
    ActionRequiredUpdate, ChatMessage, GameStateUpdate, PlayerConnectedUpdate, PlayerUpdate,
    ShowdownUpdate,
};

#[derive(Debug, Clone)]
pub struct Player {
    pub id: String,
    pub name: String,
    pub chips: i32,
    pub current_bet: i32,
    pub has_acted: bool,
    pub is_all_in: bool,
    pub is_folded: bool,
    pub is_sitting_out: bool,
    pub hole_cards: Vec<String>,
}

impl Player {
    pub fn new(id: String, name: String, chips: i32) -> Self {
        Self {
            id,
            name,
            chips,
            current_bet: 0,
            has_acted: false,
            is_all_in: false,
            is_folded: false,
            is_sitting_out: false,
            hole_cards: vec![],
        }
    }
}

pub struct PokerGameState {
    pub players: HashMap<String, Player>,
    pub community_cards: Vec<String>,
    pub pot: i32,
    pub side_pots: Vec<(i32, Vec<String>)>,
    pub current_street: String,
    pub hand_number: i32,
    pub dealer_position: usize,
    pub action_required: Option<ActionRequiredUpdate>,
    pub showdown: Option<ShowdownUpdate>,
    pub chat_messages: VecDeque<ChatMessage>,
    pub errors: VecDeque<String>,
    pub my_id: String,
    pub pending_chat: Mutex<String>,
}

impl PokerGameState {
    pub fn new() -> Self {
        Self {
            players: HashMap::new(),
            community_cards: Vec::new(),
            pot: 0,
            side_pots: Vec::new(),
            current_street: String::new(),
            hand_number: 0,
            dealer_position: 0,
            action_required: None,
            showdown: None,
            chat_messages: VecDeque::new(),
            errors: VecDeque::new(),
            my_id: String::new(),
            pending_chat: Mutex::new(String::new()),
        }
    }

    pub fn apply_update(&mut self, update: GameStateUpdate) {
        self.hand_number = update.hand_number;
        self.pot = update.pot;
        self.side_pots = update.side_pots;
        self.community_cards = update.community_cards;
        self.current_street = update.current_street;
        self.dealer_position = update.dealer_position;
    }

    pub fn set_action_required(&mut self, action: ActionRequiredUpdate) {
        self.action_required = Some(action);
    }

    pub fn add_player(&mut self, name: String, id: String, chips: i32) {
        if !self.players.contains_key(&id) {
            self.players
                .insert(id.clone(), Player::new(id, name, chips));
        }
    }

    pub fn show_showdown(&mut self, showdown: ShowdownUpdate) {
        let hands = showdown.hands.clone();
        self.showdown = Some(showdown);

        for (player_id, hole_cards, _, _) in hands {
            if let Some(player) = self.players.get_mut(&player_id) {
                player.hole_cards = hole_cards;
            }
        }
    }

    pub fn add_chat_message(&mut self, msg: ChatMessage) {
        self.chat_messages.push_back(msg);
        if self.chat_messages.len() > 50 {
            self.chat_messages.pop_front();
        }
    }

    pub fn add_error(&mut self, error: String) {
        self.errors.push_back(error);
        if self.errors.len() > 10 {
            self.errors.pop_front();
        }
    }

    pub fn update_player(&mut self, update: PlayerUpdate) {
        let player = self
            .players
            .entry(update.player_id.clone())
            .or_insert_with(|| Player {
                id: update.player_id.clone(),
                name: update.player_name.clone(),
                chips: update.chips,
                current_bet: 0,
                has_acted: false,
                is_all_in: false,
                is_folded: false,
                is_sitting_out: false,
                hole_cards: Vec::new(),
            });

        player.id = update.player_id;
        player.name = update.player_name;
        player.chips = update.chips;
        player.current_bet = update.current_bet;
        player.has_acted = update.has_acted;
        player.is_all_in = update.is_all_in;
        player.is_folded = update.is_folded;
        player.is_sitting_out = update.is_sitting_out;
        player.hole_cards = update
            .hole_cards
            .iter()
            .filter(|c| !c.is_empty())
            .cloned()
            .collect();
    }
}
