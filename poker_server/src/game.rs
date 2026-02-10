use log::{debug, error};
use poker_protocol::{
    ActionRequiredUpdate, Card, GameStage, GameStateUpdate, HandEvaluation, HandRank, PlayerAction,
    PlayerConnectedUpdate, PlayerState, PlayerUpdate, Rank, ServerError, ServerMessage,
    ServerResult, ShowdownUpdate, Street, Suit,
};
use rand::seq::SliceRandom;
use rand::thread_rng;
use std::collections::HashMap;
use tokio::sync::broadcast;

use crate::MAX_BET_MULTIPLIER;

/// Maximum pot size to prevent integer overflow (half of i32::MAX)
const MAX_POT: i32 = i32::MAX / 2;

#[derive(Debug)]
pub struct PokerGame {
    pub game_id: String,
    pub small_blind: i32,
    pub big_blind: i32,
    pub players: HashMap<String, PlayerState>,
    pub community_cards: Vec<Card>,
    deck: Vec<Card>,
    pot: i32,
    side_pots: Vec<(i32, Vec<String>)>,
    current_street: Street,
    dealer_position: usize,
    current_player_id: Option<String>,
    min_raise: i32,
    pub tx: broadcast::Sender<ServerMessage>,
    pub game_stage: GameStage,
    hand_number: i32,
    max_bet_per_hand: i32,
}

impl PokerGame {
    /// Safely broadcasts a message to all connected players.
    /// Logs failures for monitoring purposes.
    fn broadcast_message(&self, message: ServerMessage) {
        match self.tx.send(message) {
            Ok(receiver_count) => {
                debug!("Broadcast sent to {} receivers", receiver_count);
            }
            Err(e) => {
                error!("Failed to broadcast message: {} (no active receivers)", e);
            }
        }
    }

    /// Creates a new poker game instance.
    ///
    /// # Arguments
    /// * `game_id` - Unique identifier for this game table
    /// * `small_blind` - Small blind amount
    /// * `big_blind` - Big blind amount
    /// * `tx` - Broadcast channel sender for game messages
    pub fn new(
        game_id: String,
        small_blind: i32,
        big_blind: i32,
        tx: broadcast::Sender<ServerMessage>,
    ) -> Self {
        Self {
            game_id,
            small_blind,
            big_blind,
            players: HashMap::new(),
            community_cards: Vec::new(),
            deck: Vec::new(),
            pot: 0,
            side_pots: Vec::new(),
            current_street: Street::Preflop,
            dealer_position: 0,
            current_player_id: None,
            min_raise: big_blind.saturating_mul(2),
            tx,
            game_stage: GameStage::WaitingForPlayers,
            hand_number: 0,
            max_bet_per_hand: crate::MAX_BET_PER_HAND,
        }
    }

    /// Safely calculates the new pot value with overflow protection
    /// Returns None if the amount would exceed the maximum pot size
    fn calculate_new_pot(&mut self, amount: i32) -> Option<i32> {
        if amount <= 0 {
            return None;
        }
        let new_pot = self.pot.checked_add(amount)?;
        if new_pot > MAX_POT {
            return None;
        }
        Some(new_pot)
    }

    /// Sets the maximum bet per hand.
    pub fn set_max_bet_per_hand(&mut self, max_bet: i32) {
        self.max_bet_per_hand = max_bet.max(0);
    }

    /// Returns a reference to the current players in the game.
    pub fn get_players(&self) -> &HashMap<String, PlayerState> {
        &self.players
    }

    /// Adds a new player to the game and starts a hand if enough players are present.
    ///
    /// # Arguments
    /// * `player_id` - Unique player identifier
    /// * `name` - Player's display name
    /// * `chips` - Starting chip amount
    pub fn add_player(&mut self, player_id: String, name: String, chips: i32) {
        let player = PlayerState::new(player_id.clone(), name.clone(), chips);
        self.players.insert(player_id.clone(), player);

        let update = ServerMessage::PlayerConnected(PlayerConnectedUpdate {
            player_id,
            player_name: name,
            chips,
        });
        self.broadcast_message(update);

        if self.players.len() == 2 {
            self.start_hand();
        }
    }

    /// Sets a player to sit out (they won't receive cards or be required to act).
    ///
    /// # Arguments
    /// * `player_id` - The ID of the player to sit out
    pub fn sit_out(&mut self, player_id: &str) {
        if let Some(player) = self.players.get_mut(player_id) {
            player.is_sitting_out = true;
        }
    }

    /// Returns a sitting-out player to the game.
    ///
    /// # Arguments
    /// * `player_id` - The ID of the player to return
    pub fn return_to_game(&mut self, player_id: &str) {
        if let Some(player) = self.players.get_mut(player_id) {
            player.is_sitting_out = false;
        }
    }

    fn create_deck(&mut self) {
        self.deck = Vec::with_capacity(52);
        for suit in [Suit::Clubs, Suit::Diamonds, Suit::Hearts, Suit::Spades] {
            for rank in [
                Rank::Two,
                Rank::Three,
                Rank::Four,
                Rank::Five,
                Rank::Six,
                Rank::Seven,
                Rank::Eight,
                Rank::Nine,
                Rank::Ten,
                Rank::Jack,
                Rank::Queen,
                Rank::King,
                Rank::Ace,
            ] {
                self.deck.push(Card::new(suit, rank));
            }
        }
        let mut rng = thread_rng();
        self.deck.shuffle(&mut rng);
    }

    fn deal_card(&mut self) -> Option<Card> {
        self.deck.pop()
    }

    fn post_blinds(&mut self) {
        let active_player_ids = self.get_active_player_ids();
        if active_player_ids.len() < 2 {
            debug!(
                "Cannot post blinds: only {} active players (need at least 2)",
                active_player_ids.len()
            );
            return;
        }

        let sb_idx = self.dealer_position % active_player_ids.len();
        let bb_idx = (sb_idx + 1) % active_player_ids.len();

        let sb_player_id = active_player_ids[sb_idx].clone();
        let bb_player_id = active_player_ids[bb_idx].clone();

        let mut total_pot = 0;

        if let Some(sb_player) = self.players.get_mut(&sb_player_id) {
            let sb_amount = self.small_blind.min(sb_player.chips);
            sb_player.chips -= sb_amount;
            sb_player.current_bet = sb_amount;
            total_pot += sb_amount;
        }

        if let Some(bb_player) = self.players.get_mut(&bb_player_id) {
            let bb_amount = self.big_blind.min(bb_player.chips);
            bb_player.chips -= bb_amount;
            bb_player.current_bet = bb_amount;
            total_pot += bb_amount;
        }

        self.pot = total_pot;
        self.min_raise = self.big_blind.saturating_mul(2);
    }

    fn deal_hole_cards(&mut self) {
        for _ in 0..2 {
            for player in self.players.values_mut() {
                if !player.is_sitting_out && player.chips > 0 {
                    if let Some(card) = self.deal_card() {
                        player.hole_cards.push(card);
                    }
                }
            }
        }
    }

    fn get_active_player_ids(&self) -> Vec<String> {
        self.players
            .values()
            .filter(|p| !p.is_folded && !p.is_sitting_out && p.chips > 0)
            .map(|p| p.id.clone())
            .collect()
    }

    fn start_hand(&mut self) {
        self.hand_number += 1;
        self.create_deck();

        for player in self.players.values_mut() {
            player.current_bet = 0;
            player.hole_cards.clear();
            player.has_acted = false;
            player.is_all_in = false;
            player.is_folded = false;
        }

        self.community_cards.clear();
        self.side_pots.clear();
        self.pot = 0;

        self.post_blinds();
        self.deal_hole_cards();
        self.current_street = Street::Preflop;
        self.game_stage = GameStage::BettingRound(Street::Preflop);

        let active_player_ids = self.get_active_player_ids();
        self.current_player_id = active_player_ids
            .get(1)
            .cloned()
            .or_else(|| active_player_ids.first().cloned());

        self.broadcast_game_state();
        self.request_action();
    }

    fn broadcast_game_state(&self) {
        let update = GameStateUpdate {
            game_id: self.game_id.clone(),
            hand_number: self.hand_number,
            pot: self.pot,
            side_pots: self.side_pots.clone(),
            community_cards: self.community_cards.iter().map(|c| c.to_string()).collect(),
            current_street: self.current_street.to_string(),
            dealer_position: self.dealer_position,
        };
        self.broadcast_message(ServerMessage::GameStateUpdate(update));
        let players: Vec<PlayerUpdate> = self
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
                hole_cards: if p.hole_cards.is_empty() {
                    vec!["[hidden]".to_string()]
                } else {
                    p.hole_cards.iter().map(|c| c.to_string()).collect()
                },
            })
            .collect();
        self.broadcast_message(ServerMessage::PlayerUpdates(players));
    }

    fn request_action(&mut self) {
        let active_player_ids = self.get_active_player_ids();
        if active_player_ids.is_empty() {
            return;
        }

        let player_id = self
            .current_player_id
            .clone()
            .or_else(|| active_player_ids.first().cloned())
            .unwrap_or_default();

        let player = self.players.get(&player_id);

        let action_update = ActionRequiredUpdate {
            player_id: player.map(|p| p.id.clone()).unwrap_or_default(),
            player_name: player.map(|p| p.name.clone()).unwrap_or_default(),
            min_raise: self.min_raise,
            current_bet: self.get_current_bet(),
            player_chips: player.map(|p| p.chips).unwrap_or(0),
        };

        self.broadcast_message(ServerMessage::ActionRequired(action_update));
    }

    fn get_current_bet(&self) -> i32 {
        self.players
            .values()
            .filter(|p| !p.is_folded)
            .map(|p| p.current_bet)
            .max()
            .unwrap_or(0)
    }

    fn get_player_to_act(&self) -> Option<&PlayerState> {
        let active_player_ids = self.get_active_player_ids();
        if active_player_ids.is_empty() {
            return None;
        }

        if let Some(ref player_id) = self.current_player_id {
            if active_player_ids.contains(player_id) {
                return self.players.get(player_id);
            }
        }

        active_player_ids
            .first()
            .and_then(|id| self.players.get(id))
    }

    /// Validates a bet amount before processing
    ///
    /// # Arguments
    /// * `player` - The player placing the bet
    /// * `amount` - The bet amount to validate
    /// * `current_bet` - The current highest bet in the round
    /// * `pot` - The current pot size
    ///
    /// # Returns
    /// `Ok(())` if the bet is valid, or an error otherwise
    fn validate_bet_amount(
        &self,
        player: &PlayerState,
        amount: i32,
        current_bet: i32,
        pot: i32,
    ) -> ServerResult<()> {
        if amount <= 0 {
            return Err(ServerError::InvalidAmount);
        }

        if current_bet > 0 && amount < current_bet {
            return Err(ServerError::CannotBet);
        }

        if amount > player.chips {
            return Err(ServerError::BetExceedsChips(amount, player.chips));
        }

        let max_bet = pot.saturating_mul(MAX_BET_MULTIPLIER);
        if amount > max_bet && player.chips > max_bet {
            return Err(ServerError::InvalidBet(format!(
                "Bet exceeds maximum allowed: {} (pot: {})",
                max_bet, pot
            )));
        }

        if amount > self.max_bet_per_hand {
            return Err(ServerError::InvalidBet(format!(
                "Bet exceeds table maximum: {}",
                self.max_bet_per_hand
            )));
        }

        if amount < self.min_raise && player.chips > self.min_raise {
            return Err(ServerError::MinBet(self.min_raise));
        }

        Ok(())
    }

    /// Validates a raise amount before processing
    ///
    /// # Arguments
    /// * `player` - The player raising
    /// * `total_bet` - The total bet amount after the raise
    ///
    /// # Returns
    /// `Ok(())` if the raise is valid, or an error otherwise
    fn validate_raise_amount(&self, player: &PlayerState, total_bet: i32) -> ServerResult<()> {
        if total_bet <= player.current_bet {
            return Err(ServerError::InvalidRaise(
                "Raise amount must increase the bet".to_string(),
            ));
        }

        if total_bet < self.min_raise {
            return Err(ServerError::MinRaise(self.min_raise));
        }

        let required_chips = total_bet.saturating_sub(player.current_bet);
        if required_chips > player.chips {
            return Err(ServerError::RaiseInsufficientChips(
                required_chips,
                player.chips,
            ));
        }

        if total_bet > self.max_bet_per_hand {
            return Err(ServerError::InvalidBet(format!(
                "Raise exceeds table maximum: {}",
                self.max_bet_per_hand
            )));
        }

        Ok(())
    }

    /// Processes a player's action in the game.
    ///
    /// # Arguments
    /// * `player_id` - The ID of the player taking the action
    /// * `action` - The action being taken (fold, check, call, bet, raise, all-in)
    ///
    /// # Returns
    /// * `Ok(())` if the action was processed successfully
    /// * `Err(ServerError)` if the action is invalid or it's not the player's turn
    pub fn handle_action(&mut self, player_id: &str, action: PlayerAction) -> ServerResult<()> {
        let current_bet = self.get_current_bet();

        let active_player_ids = self.get_active_player_ids();
        let current_player_id = self.current_player_id.as_ref();

        let is_player_turn = match current_player_id {
            Some(id) => id == player_id,
            None => active_player_ids
                .first()
                .map(|id| id == player_id)
                .unwrap_or(false),
        };

        if !is_player_turn {
            return Err(ServerError::NotYourTurn);
        }

        let pot = self.pot;

        match action {
            PlayerAction::Fold => {
                if let Some(player) = self.players.get_mut(player_id) {
                    player.is_folded = true;
                    player.has_acted = true;
                }
            }
            PlayerAction::Check => {
                let player_call_amount = current_bet
                    - self
                        .players
                        .get(player_id)
                        .map(|p| p.current_bet)
                        .unwrap_or(0);
                if player_call_amount > 0 {
                    return Err(ServerError::CannotCheck);
                }
                if let Some(player) = self.players.get_mut(player_id) {
                    player.has_acted = true;
                }
            }
            PlayerAction::Call => {
                let player = self
                    .players
                    .get_mut(player_id)
                    .ok_or_else(|| ServerError::PlayerNotFound(player_id.to_string()))?;

                let player_current_bet = player.current_bet;
                let player_chips = player.chips;

                let call_amount = current_bet
                    .saturating_sub(player_current_bet)
                    .min(player_chips);

                if call_amount > 0 {
                    let new_pot = self.calculate_new_pot(call_amount).ok_or_else(|| {
                        ServerError::InvalidBet("Pot size exceeds maximum allowed".to_string())
                    })?;
                    self.pot = new_pot;
                }

                player.chips = player.chips.saturating_sub(call_amount);
                player.current_bet = player.current_bet.saturating_add(call_amount);
                player.has_acted = true;

                if player.chips == 0 {
                    player.is_all_in = true;
                }
            }
            PlayerAction::Bet(amount) => {
                let player = self
                    .players
                    .get(player_id)
                    .ok_or_else(|| ServerError::PlayerNotFound(player_id.to_string()))?;

                self.validate_bet_amount(player, amount, current_bet, pot)?;

                if current_bet > 0 && amount < self.min_raise {
                    return Err(ServerError::MinRaise(self.min_raise));
                }

                let bet_amount = amount;
                let new_pot = self.calculate_new_pot(bet_amount).ok_or_else(|| {
                    ServerError::InvalidBet("Pot size exceeds maximum allowed".to_string())
                })?;

                let player = self
                    .players
                    .get_mut(player_id)
                    .ok_or_else(|| ServerError::PlayerNotFound(player_id.to_string()))?;
                player.chips = player.chips.saturating_sub(bet_amount);
                player.current_bet = bet_amount;
                self.pot = new_pot;
                self.min_raise = bet_amount.saturating_mul(2);
                player.has_acted = true;

                if player.chips == 0 {
                    player.is_all_in = true;
                }
            }
            PlayerAction::Raise(amount) => {
                let total_bet = current_bet.saturating_add(amount);

                let player = self
                    .players
                    .get(player_id)
                    .ok_or_else(|| ServerError::PlayerNotFound(player_id.to_string()))?;

                self.validate_raise_amount(player, total_bet)?;

                let actual_raise = total_bet.saturating_sub(player.current_bet);

                let new_pot = self.calculate_new_pot(actual_raise).ok_or_else(|| {
                    ServerError::InvalidBet("Pot size exceeds maximum allowed".to_string())
                })?;

                let player = self
                    .players
                    .get_mut(player_id)
                    .ok_or_else(|| ServerError::PlayerNotFound(player_id.to_string()))?;
                player.chips = player.chips.saturating_sub(actual_raise);
                player.current_bet = player.current_bet.saturating_add(actual_raise);
                self.pot = new_pot;
                self.min_raise = player.current_bet.saturating_mul(2);
                player.has_acted = true;

                if player.chips == 0 {
                    player.is_all_in = true;
                }
            }
            PlayerAction::AllIn => {
                let player = self
                    .players
                    .get(player_id)
                    .ok_or_else(|| ServerError::PlayerNotFound(player_id.to_string()))?;

                let all_in_amount = player.chips;
                let new_bet = player.current_bet.saturating_add(all_in_amount);
                let total_bet = new_bet;

                if current_bet > 0 && total_bet < current_bet.saturating_add(self.min_raise) {
                    return Err(ServerError::MinRaise(self.min_raise));
                }

                let new_pot = self.calculate_new_pot(all_in_amount).ok_or_else(|| {
                    ServerError::InvalidBet("Pot size exceeds maximum allowed".to_string())
                })?;

                let player = self
                    .players
                    .get_mut(player_id)
                    .ok_or_else(|| ServerError::PlayerNotFound(player_id.to_string()))?;
                player.chips = 0;
                player.current_bet = new_bet;
                self.pot = new_pot;
                player.is_all_in = true;
                player.has_acted = true;

                let new_total_bet = player.current_bet;
                if new_total_bet > current_bet {
                    let potential_min_raise = new_total_bet.saturating_mul(2);
                    if potential_min_raise > self.min_raise {
                        self.min_raise = potential_min_raise;
                    }
                }
            }
        }

        self.broadcast_game_state();
        self.advance_action();

        Ok(())
    }

    fn all_players_acted(&self) -> bool {
        self.players
            .values()
            .filter(|p| !p.is_folded && !p.is_all_in)
            .all(|p| p.has_acted)
    }

    fn bets_equalized(&self) -> bool {
        let active_players: Vec<_> = self.players.values().filter(|p| !p.is_folded).collect();
        if active_players.is_empty() {
            return true;
        }
        let target_bet = active_players
            .iter()
            .map(|p| p.current_bet)
            .max()
            .unwrap_or(0);
        active_players
            .iter()
            .all(|p| p.current_bet == target_bet || p.is_all_in)
    }

    fn should_advance_street(&self) -> bool {
        self.all_players_acted() && self.bets_equalized()
    }

    fn advance_action(&mut self) {
        let active_player_ids = self.get_active_player_ids();
        if active_player_ids.is_empty() {
            self.end_hand();
            return;
        }

        if let Some(ref current_id) = self.current_player_id {
            if let Some(current_idx) = active_player_ids.iter().position(|id| id == current_id) {
                let next_idx = (current_idx + 1) % active_player_ids.len();
                self.current_player_id = Some(active_player_ids[next_idx].clone());
            } else {
                self.current_player_id = active_player_ids.first().cloned();
            }
        } else {
            self.current_player_id = active_player_ids.first().cloned();
        }

        if self.current_street != Street::Showdown && self.should_advance_street() {
            match self.current_street {
                Street::Preflop => {
                    self.current_street = Street::Flop;
                    self.deal_community_cards(3);
                }
                Street::Flop => {
                    self.current_street = Street::Turn;
                    self.deal_community_cards(1);
                }
                Street::Turn => {
                    self.current_street = Street::River;
                    self.deal_community_cards(1);
                }
                Street::River => {
                    self.current_street = Street::Showdown;
                    self.showdown();
                    return;
                }
                Street::Showdown => {
                    return;
                }
            }

            self.broadcast_game_state();
            self.request_action();
        } else {
            self.request_action();
        }
    }

    fn deal_community_cards(&mut self, count: usize) {
        if self.current_street == Street::Showdown {
            error!("Cannot deal community cards during showdown");
            return;
        }

        if count == 0 || count > 5 {
            error!("Invalid community card count: {}", count);
            return;
        }

        let max_cards = match self.current_street {
            Street::Preflop => 5,
            Street::Flop => 4,
            Street::Turn => 5,
            Street::River => 5,
            Street::Showdown => 0,
        };

        if self.community_cards.len() + count > max_cards {
            error!(
                "Cannot deal {} cards, would exceed maximum of {}",
                count, max_cards
            );
            return;
        }

        for _ in 0..count {
            if let Some(card) = self.deal_card() {
                self.community_cards.push(card);
            }
        }
    }

    fn calculate_side_pots(&self) -> Vec<(i32, Vec<String>)> {
        let mut pots = Vec::new();
        let players: Vec<_> = self.players.values().filter(|p| !p.is_folded).collect();

        if players.is_empty() {
            return pots;
        }

        let mut players_vec: Vec<_> = players.iter().collect();
        players_vec.sort_by_key(|&p| p.current_bet);

        let min_bet = players_vec.first().map(|p| p.current_bet).unwrap_or(0);

        let main_pot_players: Vec<String> = players_vec.iter().map(|p| p.id.clone()).collect();
        let main_pot_amount = min_bet.saturating_mul(main_pot_players.len() as i32);
        pots.push((main_pot_amount, main_pot_players));

        let mut current_level = min_bet;

        for player in players_vec.iter() {
            let excess = player.current_bet - current_level;
            if excess > 0 {
                let eligible_players: Vec<String> = players_vec
                    .iter()
                    .filter(|p| p.current_bet >= player.current_bet)
                    .map(|p| p.id.clone())
                    .collect();

                let side_pot_amount = excess.saturating_mul(eligible_players.len() as i32);
                pots.push((side_pot_amount, eligible_players));
            }
            current_level = player.current_bet;
        }

        pots
    }

    fn showdown(&mut self) {
        let active_players: Vec<&PlayerState> =
            self.players.values().filter(|p| !p.is_folded).collect();

        if active_players.is_empty() {
            self.end_hand();
            return;
        }

        let side_pots = self.calculate_side_pots();

        let mut hand_evals: Vec<(&PlayerState, HandEvaluation)> = active_players
            .iter()
            .map(|p| (p, self.evaluate_hand(p)))
            .collect();

        if hand_evals.is_empty() {
            self.end_hand();
            return;
        }

        hand_evals.sort_by(|a, b| b.1.cmp(&a.1));

        let best_eval = &hand_evals[0].1;
        let winners: Vec<&PlayerState> = hand_evals
            .iter()
            .filter(|(_, eval)| {
                eval.rank == best_eval.rank && eval.tiebreakers == best_eval.tiebreakers
            })
            .map(|(p, _)| *p)
            .collect();

        let winner_ids: Vec<String> = winners.iter().map(|p| p.id.clone()).collect();
        let winner_count = winners.len() as i32;

        if winner_count == 0 {
            self.end_hand();
            return;
        }

        let showdown_update = ShowdownUpdate {
            community_cards: self.community_cards.iter().map(|c| c.to_string()).collect(),
            hands: hand_evals
                .iter()
                .map(|(p, eval)| {
                    (
                        p.id.clone(),
                        p.hole_cards.iter().map(|c| c.to_string()).collect(),
                        format!("{:?}", eval.rank),
                        eval.description.clone(),
                    )
                })
                .collect(),
            winners: winner_ids.clone(),
        };

        let winnings_distribution: Vec<(String, i32)> = {
            let mut distributed = Vec::new();
            for (pot_amount, eligible_players) in side_pots {
                let pot_winner_ids: Vec<String> = winners
                    .iter()
                    .filter(|w| eligible_players.contains(&w.id))
                    .map(|w| w.id.clone())
                    .collect();

                if pot_winner_ids.is_empty() {
                    continue;
                }

                let winner_count_in_pot = pot_winner_ids.len() as i32;
                if winner_count_in_pot == 0 {
                    continue;
                }
                let pot_per_winner = pot_amount / winner_count_in_pot;
                let remainder = pot_amount % winner_count_in_pot;

                for (i, winner_id) in pot_winner_ids.iter().enumerate() {
                    let mut winnings = pot_per_winner;
                    if i < remainder as usize {
                        winnings += 1;
                    }
                    distributed.push((winner_id.clone(), winnings));
                }
            }
            distributed
        };

        for (winner_id, winnings) in winnings_distribution {
            if let Some(player) = self.players.get_mut(&winner_id) {
                player.chips += winnings;
            }
        }

        self.broadcast_message(ServerMessage::Showdown(showdown_update));

        self.end_hand();
    }

    fn evaluate_hand(&self, player: &PlayerState) -> HandEvaluation {
        let mut all_cards: Vec<Card> = player
            .hole_cards
            .iter()
            .chain(self.community_cards.iter())
            .cloned()
            .collect();

        if all_cards.is_empty() {
            return HandEvaluation {
                rank: HandRank::HighCard,
                primary_rank: 0,
                tiebreakers: vec![],
                description: "No cards".to_string(),
            };
        }

        if all_cards.len() < 5 {
            let top_rank = all_cards.iter().map(|c| c.rank as i32).max().unwrap_or(0);

            return HandEvaluation {
                rank: HandRank::HighCard,
                primary_rank: top_rank,
                tiebreakers: all_cards.iter().map(|c| c.rank as i32).collect(),
                description: format!("High Card ({} cards)", all_cards.len()),
            };
        }

        if let Some(eval) = self.check_straight_flush(&all_cards) {
            return eval;
        }
        if let Some(eval) = self.check_four_of_a_kind(&all_cards) {
            return eval;
        }
        if let Some(eval) = self.check_full_house(&all_cards) {
            return eval;
        }
        if let Some(eval) = self.check_flush(&all_cards) {
            return eval;
        }
        if let Some(eval) = self.check_straight(&all_cards) {
            return eval;
        }
        if let Some(eval) = self.check_three_of_a_kind(&all_cards) {
            return eval;
        }
        if let Some(eval) = self.check_two_pair(&all_cards) {
            return eval;
        }
        if let Some(eval) = self.check_pair(&all_cards) {
            return eval;
        }

        HandEvaluation::high_card(&all_cards)
    }

    fn check_straight_flush(&self, cards: &[Card]) -> Option<HandEvaluation> {
        if let Some(flush_cards) = self.get_flush_cards(cards) {
            let ranks: Vec<u8> = flush_cards.iter().map(|c| c.rank as u8).collect();

            let has_wheel = ranks.contains(&2)
                && ranks.contains(&3)
                && ranks.contains(&4)
                && ranks.contains(&5)
                && ranks.contains(&14);

            if has_wheel {
                return Some(HandEvaluation::straight_flush(5));
            }

            return self
                .check_straight_from_cards(&flush_cards)
                .map(|eval| HandEvaluation::straight_flush(eval.primary_rank as u8));
        }
        None
    }

    fn check_four_of_a_kind(&self, cards: &[Card]) -> Option<HandEvaluation> {
        let mut rank_counts = HashMap::<u8, usize>::new();
        for card in cards {
            *rank_counts.entry(card.rank as u8).or_insert(0) += 1;
        }

        for (rank, &count) in &rank_counts {
            if count == 4 {
                return Some(HandEvaluation::four_of_a_kind(cards, *rank));
            }
        }
        None
    }

    fn check_full_house(&self, cards: &[Card]) -> Option<HandEvaluation> {
        let mut rank_counts = HashMap::<u8, usize>::new();
        for card in cards {
            *rank_counts.entry(card.rank as u8).or_insert(0) += 1;
        }

        let mut three_of_kind: Option<u8> = None;
        let mut pairs: Vec<u8> = Vec::new();

        for (&rank, &count) in &rank_counts {
            if count >= 3 {
                if three_of_kind.is_none() {
                    three_of_kind = Some(rank);
                } else {
                    pairs.push(rank);
                }
            } else if count >= 2 {
                pairs.push(rank);
            }
        }

        if let Some(three_rank) = three_of_kind {
            if let Some(pair_rank) = pairs.into_iter().find(|&r| r != three_rank) {
                return Some(HandEvaluation::full_house(three_rank, pair_rank));
            }
        }

        None
    }

    fn check_flush(&self, cards: &[Card]) -> Option<HandEvaluation> {
        if let Some(flush_cards) = self.get_flush_cards(cards) {
            return Some(HandEvaluation::flush(&flush_cards));
        }
        None
    }

    fn get_flush_cards(&self, cards: &[Card]) -> Option<Vec<Card>> {
        for suit in [Suit::Clubs, Suit::Diamonds, Suit::Hearts, Suit::Spades] {
            let mut flush_cards: Vec<Card> =
                cards.iter().filter(|c| c.suit == suit).cloned().collect();
            if flush_cards.len() >= 5 {
                flush_cards.sort_by_key(|c| c.rank as u8);
                flush_cards.reverse();
                flush_cards.truncate(5);
                return Some(flush_cards);
            }
        }
        None
    }

    fn check_straight(&self, cards: &[Card]) -> Option<HandEvaluation> {
        self.check_straight_from_cards(cards)
    }

    fn check_straight_from_cards(&self, cards: &[Card]) -> Option<HandEvaluation> {
        let mut ranks: Vec<_> = cards.iter().map(|c| c.rank as u8).collect();
        ranks.sort();
        ranks.dedup();

        if ranks.is_empty() {
            return None;
        }

        let has_wheel = ranks.contains(&2)
            && ranks.contains(&3)
            && ranks.contains(&4)
            && ranks.contains(&5)
            && ranks.contains(&14);

        if has_wheel {
            return Some(HandEvaluation::straight_with_wheel());
        }

        let mut straight_high = 0;
        let mut consecutive = 1;

        for i in 1..ranks.len() {
            if ranks[i] == ranks[i - 1] + 1 {
                consecutive += 1;
            } else if ranks[i] != ranks[i - 1] {
                if consecutive >= 5 && ranks[i - 1] > straight_high {
                    straight_high = ranks[i - 1];
                }
                consecutive = 1;
            }
        }

        if consecutive >= 5 && ranks[ranks.len() - 1] > straight_high {
            straight_high = ranks[ranks.len() - 1];
        }

        if straight_high > 0 {
            return Some(HandEvaluation::straight(straight_high));
        }

        None
    }

    fn check_three_of_a_kind(&self, cards: &[Card]) -> Option<HandEvaluation> {
        let mut rank_counts = HashMap::<u8, usize>::new();
        for card in cards {
            *rank_counts.entry(card.rank as u8).or_insert(0) += 1;
        }

        for (rank, &count) in &rank_counts {
            if count >= 3 {
                return Some(HandEvaluation::three_of_a_kind(cards, *rank));
            }
        }
        None
    }

    fn check_two_pair(&self, cards: &[Card]) -> Option<HandEvaluation> {
        let mut rank_counts = HashMap::<u8, usize>::new();
        for card in cards {
            *rank_counts.entry(card.rank as u8).or_insert(0) += 1;
        }

        let mut pairs: Vec<u8> = rank_counts
            .iter()
            .filter(|(_, &count)| count >= 2)
            .map(|(&rank, _)| rank)
            .collect();

        if pairs.len() >= 2 {
            pairs.sort_by(|a, b| b.cmp(a));
            return Some(HandEvaluation::two_pair(cards, pairs[0], pairs[1]));
        }
        None
    }

    fn check_pair(&self, cards: &[Card]) -> Option<HandEvaluation> {
        let mut rank_counts = HashMap::<u8, usize>::new();
        for card in cards {
            *rank_counts.entry(card.rank as u8).or_insert(0) += 1;
        }

        for (rank, &count) in &rank_counts {
            if count >= 2 {
                return Some(HandEvaluation::pair(cards, *rank));
            }
        }
        None
    }

    fn end_hand(&mut self) {
        self.game_stage = GameStage::HandComplete;
        self.broadcast_game_state();

        let active_player_ids: Vec<&str> = self
            .players
            .values()
            .filter(|p| p.chips > 0 && !p.is_sitting_out)
            .map(|p| p.id.as_str())
            .collect();

        if !active_player_ids.is_empty() {
            let current_dealer = active_player_ids
                .get(self.dealer_position % active_player_ids.len())
                .or(active_player_ids.first());

            if let Some(current) = current_dealer {
                if let Some(current_idx) = active_player_ids.iter().position(|id| *id == *current) {
                    let next_idx = (current_idx + 1) % active_player_ids.len();
                    self.dealer_position = next_idx;
                }
            }
        }

        let active_players: Vec<&PlayerState> = self
            .players
            .values()
            .filter(|p| p.chips > 0 && !p.is_sitting_out)
            .collect();

        if active_players.len() >= 2 {
            self.start_hand();
        } else {
            self.game_stage = GameStage::WaitingForPlayers;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use poker_protocol::{Card, HandEvaluation, HandRank, PlayerState, Rank, Suit};

    pub fn card(rank: Rank, suit: Suit) -> Card {
        Card::new(suit, rank)
    }

    pub fn create_player_with_cards(cards: Vec<Card>) -> PlayerState {
        let mut player = PlayerState::new("test".to_string(), "Test".to_string(), 1000);
        player.hole_cards = cards;
        player
    }

    #[test]
    fn test_royal_flush() {
        let cards = vec![
            card(Rank::Ace, Suit::Hearts),
            card(Rank::King, Suit::Hearts),
            card(Rank::Queen, Suit::Hearts),
            card(Rank::Jack, Suit::Hearts),
            card(Rank::Ten, Suit::Hearts),
        ];
        let poker_game = PokerGame::new(
            "test".to_string(),
            5,
            10,
            tokio::sync::broadcast::channel(100).0,
        );
        let player = create_player_with_cards(cards);
        let eval = poker_game.evaluate_hand(&player);
        assert_eq!(eval.rank, HandRank::StraightFlush);
    }

    #[test]
    fn test_straight_flush() {
        let cards = vec![
            card(Rank::Nine, Suit::Spades),
            card(Rank::Ten, Suit::Spades),
            card(Rank::Jack, Suit::Spades),
            card(Rank::Queen, Suit::Spades),
            card(Rank::King, Suit::Spades),
        ];
        let poker_game = PokerGame::new(
            "test".to_string(),
            5,
            10,
            tokio::sync::broadcast::channel(100).0,
        );
        let player = create_player_with_cards(cards);
        let eval = poker_game.evaluate_hand(&player);
        assert_eq!(eval.rank, HandRank::StraightFlush);
    }

    #[test]
    fn test_four_of_a_kind() {
        let cards = vec![
            card(Rank::Ace, Suit::Hearts),
            card(Rank::Ace, Suit::Diamonds),
            card(Rank::Ace, Suit::Clubs),
            card(Rank::Ace, Suit::Spades),
            card(Rank::King, Suit::Hearts),
        ];
        let poker_game = PokerGame::new(
            "test".to_string(),
            5,
            10,
            tokio::sync::broadcast::channel(100).0,
        );
        let player = create_player_with_cards(cards);
        let eval = poker_game.evaluate_hand(&player);
        assert_eq!(eval.rank, HandRank::FourOfAKind);
        assert_eq!(eval.primary_rank, 14); // Ace
    }

    #[test]
    fn test_full_house() {
        let cards = vec![
            card(Rank::King, Suit::Hearts),
            card(Rank::King, Suit::Diamonds),
            card(Rank::King, Suit::Clubs),
            card(Rank::Queen, Suit::Hearts),
            card(Rank::Queen, Suit::Diamonds),
        ];
        let poker_game = PokerGame::new(
            "test".to_string(),
            5,
            10,
            tokio::sync::broadcast::channel(100).0,
        );
        let player = create_player_with_cards(cards);
        let eval = poker_game.evaluate_hand(&player);
        assert_eq!(eval.rank, HandRank::FullHouse);
        assert_eq!(eval.primary_rank, 13); // Kings
    }

    #[test]
    fn test_flush() {
        let cards = vec![
            card(Rank::Ace, Suit::Hearts),
            card(Rank::King, Suit::Hearts),
            card(Rank::Queen, Suit::Hearts),
            card(Rank::Jack, Suit::Hearts),
            card(Rank::Nine, Suit::Hearts),
        ];
        let poker_game = PokerGame::new(
            "test".to_string(),
            5,
            10,
            tokio::sync::broadcast::channel(100).0,
        );
        let player = create_player_with_cards(cards);
        let eval = poker_game.evaluate_hand(&player);
        assert_eq!(eval.rank, HandRank::Flush);
    }

    #[test]
    fn test_straight() {
        let cards = vec![
            card(Rank::Ten, Suit::Hearts),
            card(Rank::Jack, Suit::Diamonds),
            card(Rank::Queen, Suit::Clubs),
            card(Rank::King, Suit::Spades),
            card(Rank::Ace, Suit::Hearts),
        ];
        let poker_game = PokerGame::new(
            "test".to_string(),
            5,
            10,
            tokio::sync::broadcast::channel(100).0,
        );
        let player = create_player_with_cards(cards);
        let eval = poker_game.evaluate_hand(&player);
        assert_eq!(eval.rank, HandRank::Straight);
    }

    #[test]
    fn test_three_of_a_kind() {
        let cards = vec![
            card(Rank::Seven, Suit::Hearts),
            card(Rank::Seven, Suit::Diamonds),
            card(Rank::Seven, Suit::Clubs),
            card(Rank::King, Suit::Spades),
            card(Rank::Two, Suit::Hearts),
        ];
        let poker_game = PokerGame::new(
            "test".to_string(),
            5,
            10,
            tokio::sync::broadcast::channel(100).0,
        );
        let player = create_player_with_cards(cards);
        let eval = poker_game.evaluate_hand(&player);
        assert_eq!(eval.rank, HandRank::ThreeOfAKind);
        assert_eq!(eval.primary_rank, 7);
    }

    #[test]
    fn test_two_pair() {
        let cards = vec![
            card(Rank::Ace, Suit::Hearts),
            card(Rank::Ace, Suit::Diamonds),
            card(Rank::King, Suit::Clubs),
            card(Rank::King, Suit::Spades),
            card(Rank::Queen, Suit::Hearts),
        ];
        let poker_game = PokerGame::new(
            "test".to_string(),
            5,
            10,
            tokio::sync::broadcast::channel(100).0,
        );
        let player = create_player_with_cards(cards);
        let eval = poker_game.evaluate_hand(&player);
        assert_eq!(eval.rank, HandRank::TwoPair);
    }

    #[test]
    fn test_pair() {
        let cards = vec![
            card(Rank::Jack, Suit::Hearts),
            card(Rank::Jack, Suit::Diamonds),
            card(Rank::Ace, Suit::Clubs),
            card(Rank::King, Suit::Spades),
            card(Rank::Queen, Suit::Hearts),
        ];
        let poker_game = PokerGame::new(
            "test".to_string(),
            5,
            10,
            tokio::sync::broadcast::channel(100).0,
        );
        let player = create_player_with_cards(cards);
        let eval = poker_game.evaluate_hand(&player);
        assert_eq!(eval.rank, HandRank::Pair);
        assert_eq!(eval.primary_rank, 11); // Jacks
    }

    #[test]
    fn test_high_card() {
        let cards = vec![
            card(Rank::Ace, Suit::Hearts),
            card(Rank::King, Suit::Diamonds),
            card(Rank::Queen, Suit::Clubs),
            card(Rank::Jack, Suit::Spades),
            card(Rank::Nine, Suit::Hearts),
        ];
        let poker_game = PokerGame::new(
            "test".to_string(),
            5,
            10,
            tokio::sync::broadcast::channel(100).0,
        );
        let player = create_player_with_cards(cards);
        let eval = poker_game.evaluate_hand(&player);
        assert_eq!(eval.rank, HandRank::HighCard);
    }

    #[test]
    fn test_hand_comparison() {
        let straight_flush = HandEvaluation {
            rank: HandRank::StraightFlush,
            primary_rank: 10,
            tiebreakers: vec![10],
            description: "Straight Flush".to_string(),
        };

        let four_of_a_kind = HandEvaluation {
            rank: HandRank::FourOfAKind,
            primary_rank: 14,
            tiebreakers: vec![13],
            description: "Four of a Kind".to_string(),
        };

        assert!(straight_flush > four_of_a_kind);
        assert!(four_of_a_kind < straight_flush);
    }

    #[test]
    fn test_card_display() {
        assert_eq!(format!("{}", Card::new(Suit::Hearts, Rank::Ace)), "A♥");
        assert_eq!(format!("{}", Card::new(Suit::Spades, Rank::Ten)), "10♠");
        assert_eq!(format!("{}", Card::new(Suit::Diamonds, Rank::King)), "K♦");
    }

    #[test]
    fn test_rank_from_u8() {
        assert_eq!(Rank::from_u8(2), Some(Rank::Two));
        assert_eq!(Rank::from_u8(10), Some(Rank::Ten));
        assert_eq!(Rank::from_u8(14), Some(Rank::Ace));
        assert_eq!(Rank::from_u8(1), None);
        assert_eq!(Rank::from_u8(15), None);
    }

    #[test]
    fn test_check_straight_empty_cards() {
        let poker_game = PokerGame::new(
            "test".to_string(),
            5,
            10,
            tokio::sync::broadcast::channel(100).0,
        );
        let result = poker_game.check_straight_from_cards(&[]);
        assert!(result.is_none());
    }

    #[test]
    fn test_check_straight_single_card() {
        let cards = vec![Card::new(Suit::Hearts, Rank::Ace)];
        let poker_game = PokerGame::new(
            "test".to_string(),
            5,
            10,
            tokio::sync::broadcast::channel(100).0,
        );
        let result = poker_game.check_straight_from_cards(&cards);
        assert!(result.is_none());
    }

    #[test]
    fn test_sit_out_and_return() {
        let tx = tokio::sync::broadcast::channel(100).0;
        let mut game = PokerGame::new("test".to_string(), 5, 10, tx);
        game.add_player("p1".to_string(), "Player1".to_string(), 1000);

        game.sit_out("p1");
        let p1 = game.players.get("p1").unwrap();
        assert!(p1.is_sitting_out);

        game.return_to_game("p1");
        let p1 = game.players.get("p1").unwrap();
        assert!(!p1.is_sitting_out);
    }

    #[test]
    fn test_check_wheel_straight() {
        let cards = vec![
            Card::new(Suit::Hearts, Rank::Ace),
            Card::new(Suit::Diamonds, Rank::Two),
            Card::new(Suit::Clubs, Rank::Three),
            Card::new(Suit::Spades, Rank::Four),
            Card::new(Suit::Hearts, Rank::Five),
        ];
        let poker_game = PokerGame::new(
            "test".to_string(),
            5,
            10,
            tokio::sync::broadcast::channel(100).0,
        );
        let result = poker_game.check_straight_from_cards(&cards);
        assert!(result.is_some());
        let eval = result.unwrap();
        assert_eq!(eval.rank, HandRank::Straight);
        assert_eq!(eval.primary_rank, 5);
        assert!(eval.description.contains("Wheel"));
    }

    #[test]
    fn test_betting_round_check_and_call() {
        let tx = tokio::sync::broadcast::channel(100).0;
        let mut game = PokerGame::new("test".to_string(), 5, 10, tx);
        game.add_player("p1".to_string(), "Player1".to_string(), 1000);
        game.add_player("p2".to_string(), "Player2".to_string(), 1000);

        let player_to_act = game.get_player_to_act();
        assert!(player_to_act.is_some());

        let player_id = player_to_act.unwrap().id.clone();
        let player = game.players.get(&player_id).unwrap();
        let initial_chips = player.chips;
        let initial_bet = player.current_bet;

        let result = game.handle_action(&player_id, PlayerAction::Call);
        assert!(result.is_ok());

        let p_updated = game.players.get(&player_id).unwrap();
        assert!(p_updated.chips <= initial_chips);
        assert!(p_updated.current_bet >= initial_bet);
    }

    #[test]
    fn test_betting_round_fold() {
        let tx = tokio::sync::broadcast::channel(100).0;
        let mut game = PokerGame::new("test".to_string(), 5, 10, tx);
        game.add_player("p1".to_string(), "Player1".to_string(), 1000);
        game.add_player("p2".to_string(), "Player2".to_string(), 1000);

        let player_to_act = game.get_player_to_act().unwrap();
        let player_id = player_to_act.id.clone();
        let result = game.handle_action(&player_id, PlayerAction::Fold);
        assert!(result.is_ok());

        let p1 = game.players.get("p1").unwrap();
        let p2 = game.players.get("p2").unwrap();

        assert!(p1.is_folded || p2.is_folded);
    }

    #[test]
    fn test_betting_round_call() {
        let tx = tokio::sync::broadcast::channel(100).0;
        let mut game = PokerGame::new("test".to_string(), 5, 10, tx);
        game.add_player("p1".to_string(), "Player1".to_string(), 1000);
        game.add_player("p2".to_string(), "Player2".to_string(), 1000);

        let player_to_act = game.get_player_to_act().unwrap();
        let player_id = player_to_act.id.clone();
        let player = game.players.get(&player_id).unwrap();
        let initial_chips = player.chips;
        let initial_bet = player.current_bet;

        let result = game.handle_action(&player_id, PlayerAction::Call);
        assert!(result.is_ok());

        let p_updated = game.players.get(&player_id).unwrap();
        assert!(p_updated.chips <= initial_chips);
        assert!(p_updated.current_bet >= initial_bet);
    }

    #[test]
    fn test_betting_round_raise() {
        let tx = tokio::sync::broadcast::channel(100).0;
        let mut game = PokerGame::new("test".to_string(), 5, 10, tx);
        game.add_player("p1".to_string(), "Player1".to_string(), 1000);
        game.add_player("p2".to_string(), "Player2".to_string(), 1000);

        let player_to_act = game.get_player_to_act().unwrap();
        let player_id = player_to_act.id.clone();
        let player = game.players.get(&player_id).unwrap();
        let initial_chips = player.chips;
        let initial_bet = player.current_bet;

        let result = game.handle_action(&player_id, PlayerAction::Raise(20));
        assert!(result.is_ok());

        let p_updated = game.players.get(&player_id).unwrap();
        assert_eq!(p_updated.chips, initial_chips - 20);
        assert_eq!(p_updated.current_bet, initial_bet + 20);
    }

    #[test]
    fn test_betting_round_all_in() {
        let tx = tokio::sync::broadcast::channel(100).0;
        let mut game = PokerGame::new("test".to_string(), 5, 10, tx);
        game.add_player("p1".to_string(), "Player1".to_string(), 100);
        game.add_player("p2".to_string(), "Player2".to_string(), 1000);

        let player_to_act = game.get_player_to_act().unwrap();
        let player_id = player_to_act.id.clone();

        let result = game.handle_action(&player_id, PlayerAction::AllIn);
        assert!(result.is_ok());

        let player = game.players.get(&player_id).unwrap();
        assert_eq!(player.chips, 0);
        assert!(player.is_all_in);
    }

    #[test]
    fn test_fewer_than_five_cards_high_card() {
        let tx = tokio::sync::broadcast::channel(100).0;
        let poker_game = PokerGame::new("test".to_string(), 5, 10, tx);
        let player = create_player_with_cards(vec![
            Card::new(Suit::Hearts, Rank::Ace),
            Card::new(Suit::Diamonds, Rank::King),
            Card::new(Suit::Clubs, Rank::Queen),
        ]);
        let eval = poker_game.evaluate_hand(&player);
        assert_eq!(eval.rank, HandRank::HighCard);
        assert_eq!(eval.primary_rank, 14);
    }

    #[test]
    fn test_empty_hand() {
        let tx = tokio::sync::broadcast::channel(100).0;
        let poker_game = PokerGame::new("test".to_string(), 5, 10, tx);
        let player = create_player_with_cards(vec![]);
        let eval = poker_game.evaluate_hand(&player);
        assert_eq!(eval.rank, HandRank::HighCard);
        assert_eq!(eval.primary_rank, 0);
    }

    #[test]
    fn test_side_pots_calculation() {
        let tx = tokio::sync::broadcast::channel(100).0;
        let mut game = PokerGame::new("test".to_string(), 5, 10, tx);
        game.add_player("p1".to_string(), "Player1".to_string(), 100);
        game.add_player("p2".to_string(), "Player2".to_string(), 200);
        game.add_player("p3".to_string(), "Player3".to_string(), 300);

        if let Some(p1) = game.players.get_mut("p1") {
            p1.current_bet = 50;
        }
        if let Some(p2) = game.players.get_mut("p2") {
            p2.current_bet = 100;
        }
        if let Some(p3) = game.players.get_mut("p3") {
            p3.current_bet = 100;
        }

        let pots = game.calculate_side_pots();
        assert!(!pots.is_empty());

        let main_pot = pots
            .iter()
            .find(|(amount, players)| players.len() == 3 && *amount == 150);
        assert!(
            main_pot.is_some(),
            "Should have main pot with all 3 players"
        );

        if let Some(p2) = game.players.get_mut("p2") {
            p2.is_folded = true;
        }

        let pots_after_fold = game.calculate_side_pots();
        assert!(!pots_after_fold.is_empty());
    }

    #[test]
    fn test_all_in_pot_distribution() {
        let tx = tokio::sync::broadcast::channel(100).0;
        let mut game = PokerGame::new("test".to_string(), 5, 10, tx);
        game.add_player("p1".to_string(), "Player1".to_string(), 100);
        game.add_player("p2".to_string(), "Player2".to_string(), 500);

        if let Some(p1) = game.players.get_mut("p1") {
            p1.current_bet = 100;
            p1.is_all_in = true;
        }
        if let Some(p2) = game.players.get_mut("p2") {
            p2.current_bet = 200;
        }

        let pots = game.calculate_side_pots();
        assert!(!pots.is_empty());

        let side_pot = pots.iter().find(|(_amount, players)| {
            players.contains(&"p1".to_string()) && players.contains(&"p2".to_string())
        });
        assert!(side_pot.is_some());
    }

    #[test]
    fn test_validate_bet_amount_positive() {
        let tx = tokio::sync::broadcast::channel(100).0;
        let game = PokerGame::new("test".to_string(), 5, 10, tx);
        let player = PlayerState::new("p1".to_string(), "Player1".to_string(), 1000);

        let result = game.validate_bet_amount(&player, 100, 0, 100);
        assert!(result.is_ok());
    }

    #[test]
    fn test_validate_bet_amount_zero() {
        let tx = tokio::sync::broadcast::channel(100).0;
        let game = PokerGame::new("test".to_string(), 5, 10, tx);
        let player = PlayerState::new("p1".to_string(), "Player1".to_string(), 1000);

        assert!(game.validate_bet_amount(&player, 0, 100, 100).is_err());
        assert!(game.validate_bet_amount(&player, -100, 100, 100).is_err());
    }

    #[test]
    fn test_validate_bet_amount_exceeds_chips() {
        let tx = tokio::sync::broadcast::channel(100).0;
        let game = PokerGame::new("test".to_string(), 5, 10, tx);
        let player = PlayerState::new("p1".to_string(), "Player1".to_string(), 100);

        assert!(game.validate_bet_amount(&player, 200, 100, 100).is_err());
    }

    #[test]
    fn test_validate_raise_amount_success() {
        let tx = tokio::sync::broadcast::channel(100).0;
        let mut game = PokerGame::new("test".to_string(), 5, 10, tx);
        game.add_player("p1".to_string(), "Player1".to_string(), 1000);

        let player = game.players.get("p1").unwrap();
        let result = game.validate_raise_amount(player, 50, 0);
        assert!(result.is_ok());
    }

    #[test]
    fn test_validate_raise_amount_below_minimum() {
        let tx = tokio::sync::broadcast::channel(100).0;
        let mut game = PokerGame::new("test".to_string(), 5, 10, tx);
        game.add_player("p1".to_string(), "Player1".to_string(), 1000);

        let player = game.players.get("p1").unwrap();
        game.min_raise = 100;
        let result = game.validate_raise_amount(player, 50, 0);
        assert!(result.is_err());
    }

    #[test]
    fn test_all_in_partial_call() {
        let tx = tokio::sync::broadcast::channel(100).0;
        let mut game = PokerGame::new("test".to_string(), 5, 10, tx);
        game.add_player("p1".to_string(), "Player1".to_string(), 1000);
        game.add_player("p2".to_string(), "Player2".to_string(), 200);

        let player_to_act = game.get_player_to_act().unwrap();
        let player_id = player_to_act.id.clone();

        game.handle_action(&player_id, PlayerAction::Raise(100))
            .unwrap();

        let next_player = game.get_player_to_act();
        assert!(
            next_player.is_some(),
            "Should have a player to act after raise"
        );
        let next_player = next_player.unwrap();
        let next_player_id = next_player.id.clone();

        let result = game.handle_action(&next_player_id, PlayerAction::AllIn);
        assert!(
            result.is_ok(),
            "All-in should succeed after raise: {:?}",
            result
        );

        let p = game.players.get(&next_player_id).unwrap();
        assert_eq!(p.chips, 0, "Player should have 0 chips after all-in");
        assert!(p.is_all_in, "Player should be marked as all-in");
    }

    #[test]
    fn test_calculate_side_pots_all_in_scenario() {
        let tx = tokio::sync::broadcast::channel(100).0;
        let mut game = PokerGame::new("test".to_string(), 5, 10, tx);
        game.add_player("p1".to_string(), "Player1".to_string(), 50);
        game.add_player("p2".to_string(), "Player2".to_string(), 100);
        game.add_player("p3".to_string(), "Player3".to_string(), 100);

        if let Some(p1) = game.players.get_mut("p1") {
            p1.current_bet = 50;
            p1.is_all_in = true;
        }
        if let Some(p2) = game.players.get_mut("p2") {
            p2.current_bet = 100;
        }
        if let Some(p3) = game.players.get_mut("p3") {
            p3.current_bet = 100;
        }

        let pots = game.calculate_side_pots();

        let main_pot_amount: i32 = pots
            .iter()
            .find(|(_, players)| players.len() == 3)
            .map(|(amount, _)| *amount)
            .unwrap_or(0);
        assert_eq!(
            main_pot_amount, 150,
            "Main pot should have 50 from each player"
        );

        let side_pot_amount: i32 = pots
            .iter()
            .filter(|(_, players)| players.len() == 2)
            .map(|(amount, _)| *amount)
            .sum();
        assert_eq!(
            side_pot_amount, 100,
            "Side pot should have 50 from p2 and p3"
        );
    }

    #[test]
    fn test_calculate_new_pot_overflow() {
        let tx = tokio::sync::broadcast::channel(100).0;
        let mut game = PokerGame::new("test".to_string(), 5, 10, tx);
        game.pot = MAX_POT - 100;

        let result = game.calculate_new_pot(200);
        assert!(
            result.is_none(),
            "Should fail when pot would exceed MAX_POT"
        );
    }

    #[test]
    fn test_calculate_new_pot_success() {
        let tx = tokio::sync::broadcast::channel(100).0;
        let mut game = PokerGame::new("test".to_string(), 5, 10, tx);
        game.pot = 100;

        let new_pot = game.calculate_new_pot(50);
        assert!(new_pot.is_some());
        assert_eq!(new_pot.unwrap(), 150);

        game.pot = new_pot.unwrap();
        assert_eq!(game.pot, 150);
    }

    #[test]
    fn test_calculate_new_pot_negative() {
        let tx = tokio::sync::broadcast::channel(100).0;
        let mut game = PokerGame::new("test".to_string(), 5, 10, tx);

        assert!(game.calculate_new_pot(-50).is_none());
    }

    #[test]
    fn test_bet_validation_negative_amount() {
        let tx = tokio::sync::broadcast::channel(100).0;
        let mut game = PokerGame::new("test".to_string(), 5, 10, tx);
        game.add_player("p1".to_string(), "Player1".to_string(), 1000);
        game.add_player("p2".to_string(), "Player2".to_string(), 1000);

        let player_to_act = game.get_player_to_act().unwrap();
        let player_id = player_to_act.id.clone();

        let result = game.handle_action(&player_id, PlayerAction::Bet(-100));
        assert!(result.is_err());
        let err = result.unwrap_err().to_string();
        assert!(err.contains("InvalidAmount") || err.contains("positive"));
    }

    #[test]
    fn test_bet_validation_exceeds_chips() {
        let tx = tokio::sync::broadcast::channel(100).0;
        let mut game = PokerGame::new("test".to_string(), 5, 10, tx);
        game.add_player("p1".to_string(), "Player1".to_string(), 100);
        game.add_player("p2".to_string(), "Player2".to_string(), 1000);

        let player_to_act = game.get_player_to_act().unwrap();
        let player_id = player_to_act.id.clone();

        let result = game.handle_action(&player_id, PlayerAction::Bet(500));
        assert!(result.is_err());
        let err = result.unwrap_err().to_string();
        assert!(
            err.contains("BetExceedsChips") || err.contains("chips") || err.contains("exceeds")
        );
    }

    #[test]
    fn test_bet_validation_exceeds_max_bet() {
        let tx = tokio::sync::broadcast::channel(100).0;
        let mut game = PokerGame::new("test".to_string(), 5, 10, tx);
        game.set_max_bet_per_hand(100);
        game.add_player("p1".to_string(), "Player1".to_string(), 1000);
        game.add_player("p2".to_string(), "Player2".to_string(), 1000);

        let player_to_act = game.get_player_to_act().unwrap();
        let player_id = player_to_act.id.clone();

        let result = game.handle_action(&player_id, PlayerAction::Bet(200));
        assert!(result.is_err());
        let err = result.unwrap_err().to_string();
        assert!(err.contains("Invalid") || err.contains("exceeds") || err.contains("maximum"));
    }

    #[test]
    fn test_all_in_handling() {
        let tx = tokio::sync::broadcast::channel(100).0;
        let mut game = PokerGame::new("test".to_string(), 5, 10, tx);
        game.add_player("p1".to_string(), "Player1".to_string(), 50);
        game.add_player("p2".to_string(), "Player2".to_string(), 1000);

        let player_to_act = game.get_player_to_act().unwrap();
        let player_id = player_to_act.id.clone();

        let result = game.handle_action(&player_id, PlayerAction::AllIn);
        assert!(result.is_ok(), "All-in should succeed: {:?}", result);

        let p = game.players.get(&player_id).unwrap();
        assert_eq!(p.chips, 0);
        assert!(p.is_all_in);
        assert!(p.has_acted);
    }
}
