use poker_protocol::{
    ActionRequiredUpdate, Card, GameStage, GameStateUpdate, HandEvaluation, HandRank, PlayerAction,
    PlayerConnectedUpdate, PlayerState, PlayerUpdate, Rank, ServerError, ServerMessage,
    ServerResult, ShowdownUpdate, Street, Suit,
};
use rand::seq::SliceRandom;
use rand::thread_rng;
use std::collections::HashMap;
use tokio::sync::broadcast;

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
    current_player_position: usize,
    min_raise: i32,
    pub tx: broadcast::Sender<ServerMessage>,
    pub game_stage: GameStage,
    hand_number: i32,
}

impl PokerGame {
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
            current_player_position: 0,
            min_raise: big_blind * 2,
            tx,
            game_stage: GameStage::WaitingForPlayers,
            hand_number: 0,
        }
    }

    pub fn get_players(&self) -> &HashMap<String, PlayerState> {
        &self.players
    }

    pub fn add_player(&mut self, player_id: String, name: String, chips: i32) {
        let player = PlayerState::new(player_id.clone(), name.clone(), chips);
        self.players.insert(player_id.clone(), player);

        let update = ServerMessage::PlayerConnected(PlayerConnectedUpdate {
            player_id,
            player_name: name,
            chips,
        });
        let _ = self.tx.send(update);

        if self.players.len() == 2 {
            self.start_hand();
        }
    }

    pub fn sit_out(&mut self, player_id: &str) {
        if let Some(player) = self.players.get_mut(player_id) {
            player.is_sitting_out = true;
        }
    }

    pub fn return_to_game(&mut self, player_id: &str) {
        if let Some(player) = self.players.get_mut(player_id) {
            player.is_sitting_out = false;
        }
    }

    fn create_deck(&mut self) {
        self.deck = Vec::new();
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
        let active_positions = self.get_active_player_positions();
        if active_positions.len() < 2 {
            return;
        }

        let sb_position = active_positions[0];
        let bb_position = active_positions[1];

        let sb_player_id = self
            .players
            .values()
            .nth(sb_position)
            .map(|p| p.id.clone())
            .unwrap();

        let bb_player_id = self
            .players
            .values()
            .nth(bb_position)
            .map(|p| p.id.clone())
            .unwrap();

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
        self.min_raise = self.big_blind * 2;
    }

    fn deal_hole_cards(&mut self) {
        let player_ids: Vec<String> = self.players.keys().cloned().collect();

        for _ in 0..2 {
            for player_id in &player_ids {
                let card = {
                    if let Some(player) = self.players.get_mut(player_id) {
                        if !player.is_sitting_out && player.chips > 0 {
                            self.deal_card()
                        } else {
                            None
                        }
                    } else {
                        None
                    }
                };
                if let Some(c) = card {
                    if let Some(player) = self.players.get_mut(player_id) {
                        player.hole_cards.push(c);
                    }
                }
            }
        }
    }

    fn get_active_player_positions(&self) -> Vec<usize> {
        self.players
            .values()
            .enumerate()
            .filter(|(_, p)| !p.is_folded && !p.is_sitting_out && p.chips > 0)
            .map(|(i, _)| i)
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
            player.has_acted_this_round = false;
        }

        self.community_cards.clear();
        self.side_pots.clear();
        self.pot = 0;

        self.post_blinds();
        self.deal_hole_cards();
        self.current_street = Street::Preflop;
        self.game_stage = GameStage::BettingRound(Street::Preflop);

        let active_positions = self.get_active_player_positions();
        if active_positions.len() >= 2 {
            self.current_player_position = active_positions[1];
        }

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
        let _ = self.tx.send(ServerMessage::GameStateUpdate(update));

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
        let _ = self.tx.send(ServerMessage::PlayerUpdates(players));
    }

    fn request_action(&mut self) {
        let active_positions = self.get_active_player_positions();
        if active_positions.is_empty() {
            return;
        }

        let player_idx = active_positions
            .iter()
            .position(|&pos| pos == self.current_player_position)
            .unwrap_or(0);

        let action_update = ActionRequiredUpdate {
            player_id: self
                .players
                .values()
                .nth(active_positions[player_idx])
                .map(|p| p.id.clone())
                .unwrap_or_default(),
            player_name: self
                .players
                .values()
                .nth(active_positions[player_idx])
                .map(|p| p.name.clone())
                .unwrap_or_default(),
            min_raise: self.min_raise,
            current_bet: self.get_current_bet(),
            player_chips: self
                .players
                .values()
                .nth(active_positions[player_idx])
                .map(|p| p.chips)
                .unwrap_or(0),
        };

        let msg = ServerMessage::ActionRequired(action_update);
        let _ = self.tx.send(msg);
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
        let active_positions = self.get_active_player_positions();
        if active_positions.is_empty() {
            return None;
        }

        if let Some(idx) = active_positions
            .iter()
            .position(|&pos| pos == self.current_player_position)
        {
            self.players.values().nth(active_positions[idx])
        } else {
            self.players.values().nth(active_positions[0])
        }
    }

    pub fn handle_action(&mut self, player_id: &str, action: PlayerAction) -> ServerResult<()> {
        let current_player = self
            .get_player_to_act()
            .ok_or_else(|| ServerError::GameState("No player to act".to_string()))?;

        if current_player.id != player_id {
            return Err(ServerError::NotYourTurn);
        }

        let current_bet = self.get_current_bet();

        let player = self
            .players
            .get_mut(player_id)
            .ok_or_else(|| ServerError::PlayerNotFound(player_id.to_string()))?;
        let player_call_amount = current_bet - player.current_bet;

        match action {
            PlayerAction::Fold => {
                player.is_folded = true;
                player.has_acted = true;
            }
            PlayerAction::Check => {
                if player_call_amount > 0 {
                    return Err(ServerError::CannotCheck);
                }
                player.has_acted = true;
            }
            PlayerAction::Call => {
                let call_amount = player_call_amount.min(player.chips);
                player.chips -= call_amount;
                player.current_bet += call_amount;
                self.pot += call_amount;
                player.has_acted = true;

                if player.chips == 0 {
                    player.is_all_in = true;
                }
            }
            PlayerAction::Bet(amount) => {
                if player_call_amount > 0 {
                    return Err(ServerError::CannotBet);
                }
                if amount > player.chips {
                    return Err(ServerError::BetExceedsChips(amount, player.chips));
                }
                if amount < self.min_raise && player.chips > self.min_raise {
                    return Err(ServerError::MinBet(self.min_raise));
                }

                let bet_amount = amount;
                player.chips -= bet_amount;
                player.current_bet = bet_amount;
                self.pot += bet_amount;
                self.min_raise = bet_amount * 2;
                player.has_acted = true;

                if player.chips == 0 {
                    player.is_all_in = true;
                }
            }
            PlayerAction::Raise(amount) => {
                let total_bet = current_bet + amount;
                if total_bet < self.min_raise {
                    return Err(ServerError::MinRaise(self.min_raise));
                }

                let required_chips = total_bet - player.current_bet;
                if required_chips > player.chips {
                    return Err(ServerError::RaiseInsufficientChips(
                        required_chips,
                        player.chips,
                    ));
                }

                let actual_raise = required_chips.min(player.chips);

                player.chips -= actual_raise;
                player.current_bet += actual_raise;
                self.pot += actual_raise;
                self.min_raise = player.current_bet * 2;
                player.has_acted = true;

                if player.chips == 0 {
                    player.is_all_in = true;
                }
            }
            PlayerAction::AllIn => {
                let all_in_amount = player.chips;
                player.chips = 0;
                player.current_bet += all_in_amount;
                self.pot += all_in_amount;
                player.is_all_in = true;
                player.has_acted = true;

                if all_in_amount > current_bet {
                    self.min_raise = player.current_bet * 2;
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
        let active_positions = self.get_active_player_positions();
        if active_positions.is_empty() {
            self.end_hand();
            return;
        }

        if let Some(current_idx) = active_positions
            .iter()
            .position(|&pos| pos == self.current_player_position)
        {
            let next_idx = (current_idx + 1) % active_positions.len();
            self.current_player_position = active_positions[next_idx];
        } else {
            self.current_player_position = active_positions[0];
        }

        for player in self.players.values_mut() {
            player.has_acted = false;
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
        for _ in 0..count {
            if let Some(card) = self.deal_card() {
                self.community_cards.push(card);
            }
        }
    }

    fn calculate_side_pots(&self) -> Vec<(i32, Vec<String>)> {
        let mut pots = Vec::new();
        let mut players: Vec<_> = self.players.values().filter(|p| !p.is_folded).collect();

        if players.len() < 2 {
            return pots;
        }

        players.sort_by_key(|p| p.current_bet);

        let min_bet = players[0].current_bet;
        let total_in_min_pot: i32 = players.iter().map(|p| p.current_bet.min(min_bet)).sum();
        let min_pot_contributors: Vec<String> = players
            .iter()
            .filter(|p| p.current_bet >= min_bet)
            .map(|p| p.id.clone())
            .collect();

        if total_in_min_pot > 0 {
            pots.push((total_in_min_pot, min_pot_contributors));
        }

        let mut remaining_players: Vec<_> =
            players.iter().filter(|p| p.current_bet > min_bet).collect();

        while remaining_players.len() >= 2 {
            let next_min = remaining_players[0].current_bet;
            let contribution = remaining_players
                .iter()
                .map(|p| p.current_bet.min(next_min))
                .sum();
            let contributors: Vec<String> = remaining_players
                .iter()
                .filter(|p| p.current_bet >= next_min)
                .map(|p| p.id.clone())
                .collect();

            if contribution > 0 && !contributors.is_empty() {
                pots.push((contribution, contributors));
            }

            remaining_players = remaining_players
                .iter()
                .filter(|p| p.current_bet > next_min)
                .cloned()
                .collect();
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
            .map(|p| (*p, self.evaluate_hand(*p)))
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

        let msg = ServerMessage::Showdown(showdown_update);
        let _ = self.tx.send(msg);

        self.end_hand();
    }

    fn evaluate_hand(&self, player: &PlayerState) -> HandEvaluation {
        let mut all_cards = player.hole_cards.clone();
        all_cards.extend(self.community_cards.clone());

        if all_cards.is_empty() {
            return HandEvaluation {
                rank: HandRank::HighCard,
                primary_rank: 0,
                tiebreakers: vec![],
                description: "No cards".to_string(),
            };
        }

        if all_cards.len() < 5 {
            return HandEvaluation {
                rank: HandRank::HighCard,
                primary_rank: all_cards.iter().map(|c| c.rank as i32).max().unwrap_or(0),
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
            return self
                .check_straight_from_cards(&flush_cards)
                .map(|mut eval| {
                    eval.rank = HandRank::StraightFlush;
                    eval.description = format!("Straight Flush, {}", eval.description);
                    eval
                });
        }
        None
    }

    fn check_four_of_a_kind(&self, cards: &[Card]) -> Option<HandEvaluation> {
        let ranks: Vec<_> = cards.iter().map(|c| c.rank as u8).collect();
        let mut rank_counts: Vec<(u8, usize)> = (2..=14)
            .map(|r| (r, ranks.iter().filter(|&&x| x == r).count()))
            .collect();
        rank_counts.sort_by_key(|&(_, count)| std::cmp::Reverse(count));

        if let Some((rank, 4)) = rank_counts.first() {
            let kicker = ranks.iter().filter(|&&r| r != *rank).max().unwrap_or(&0);
            return Some(HandEvaluation {
                rank: HandRank::FourOfAKind,
                primary_rank: *rank as i32,
                tiebreakers: vec![*kicker as i32],
                description: format!("Four of a Kind, {}", Rank::from_u8(*rank).unwrap()),
            });
        }
        None
    }

    fn check_full_house(&self, cards: &[Card]) -> Option<HandEvaluation> {
        let ranks: Vec<_> = cards.iter().map(|c| c.rank as u8).collect();
        let mut rank_counts: Vec<(u8, usize)> = (2..=14)
            .map(|r| (r, ranks.iter().filter(|&&x| x == r).count()))
            .collect();
        rank_counts.sort_by_key(|&(_, count)| std::cmp::Reverse(count));

        let three_kind = rank_counts.iter().find(|&&(_, count)| count >= 3);
        let pair = rank_counts
            .iter()
            .find(|&&(rank, count)| count >= 2 && Some(rank) != three_kind.map(|&(r, _)| r));

        if let (Some(&(three_rank, _)), Some(&(pair_rank, _))) = (three_kind, pair) {
            return Some(HandEvaluation {
                rank: HandRank::FullHouse,
                primary_rank: three_rank as i32,
                tiebreakers: vec![pair_rank as i32],
                description: format!(
                    "Full House, {} over {}",
                    Rank::from_u8(three_rank).unwrap(),
                    Rank::from_u8(pair_rank).unwrap()
                ),
            });
        }
        None
    }

    fn check_flush(&self, cards: &[Card]) -> Option<HandEvaluation> {
        if let Some(flush_cards) = self.get_flush_cards(cards) {
            let mut sorted: Vec<_> = flush_cards.iter().map(|c| c.rank as u8).collect();
            sorted.sort();
            sorted.reverse();

            return Some(HandEvaluation {
                rank: HandRank::Flush,
                primary_rank: sorted[0] as i32,
                tiebreakers: sorted.iter().map(|&r| r as i32).collect(),
                description: format!("Flush, {}", Rank::from_u8(sorted[0]).unwrap()),
            });
        }
        None
    }

    fn get_flush_cards(&self, cards: &[Card]) -> Option<Vec<Card>> {
        for suit in [Suit::Clubs, Suit::Diamonds, Suit::Hearts, Suit::Spades] {
            let flush_cards: Vec<Card> = cards.iter().filter(|c| c.suit == suit).cloned().collect();
            if flush_cards.len() >= 5 {
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

        let mut straight_high = 0;
        let mut current_high = ranks[0];
        let mut consecutive = 1;

        for i in 1..ranks.len() {
            if ranks[i] == ranks[i - 1] + 1 {
                consecutive += 1;
                current_high = ranks[i];
            } else if ranks[i] != ranks[i - 1] {
                if consecutive >= 5 && current_high > straight_high {
                    straight_high = current_high;
                }
                consecutive = 1;
                current_high = ranks[i];
            }
        }

        if consecutive >= 5 && current_high > straight_high {
            straight_high = current_high;
        }

        let has_wheel = ranks.contains(&2)
            && ranks.contains(&3)
            && ranks.contains(&4)
            && ranks.contains(&5)
            && ranks.contains(&14);

        if has_wheel && straight_high < 6 {
            return Some(HandEvaluation {
                rank: HandRank::Straight,
                primary_rank: 6,
                tiebreakers: vec![6, 5, 4, 3, 2],
                description: "Straight, 6-5-4-3-2 (Wheel)".to_string(),
            });
        }

        if straight_high > 0 {
            return Some(HandEvaluation {
                rank: HandRank::Straight,
                primary_rank: straight_high as i32,
                tiebreakers: vec![straight_high as i32],
                description: format!("Straight, {}", Rank::from_u8(straight_high).unwrap()),
            });
        }

        None
    }

    fn check_three_of_a_kind(&self, cards: &[Card]) -> Option<HandEvaluation> {
        let ranks: Vec<_> = cards.iter().map(|c| c.rank as u8).collect();
        let mut rank_counts: Vec<(u8, usize)> = (2..=14)
            .map(|r| (r, ranks.iter().filter(|&&x| x == r).count()))
            .collect();
        rank_counts.sort_by_key(|&(_, count)| std::cmp::Reverse(count));

        if let Some((rank, 3)) = rank_counts.first() {
            let mut kickers: Vec<_> = ranks.iter().filter(|&&r| r != *rank).collect();
            kickers.sort();
            kickers.reverse();

            return Some(HandEvaluation {
                rank: HandRank::ThreeOfAKind,
                primary_rank: *rank as i32,
                tiebreakers: kickers.iter().take(2).map(|&&r| r as i32).collect(),
                description: format!("Three of a Kind, {}", Rank::from_u8(*rank).unwrap()),
            });
        }
        None
    }

    fn check_two_pair(&self, cards: &[Card]) -> Option<HandEvaluation> {
        let ranks: Vec<_> = cards.iter().map(|c| c.rank as u8).collect();
        let mut rank_counts: Vec<(u8, usize)> = (2..=14)
            .map(|r| (r, ranks.iter().filter(|&&x| x == r).count()))
            .collect();
        rank_counts.sort_by_key(|&(_, count)| std::cmp::Reverse(count));

        let pairs: Vec<_> = rank_counts
            .iter()
            .filter(|&&(_, count)| count >= 2)
            .collect();

        if pairs.len() >= 2 {
            let high_pair = pairs[0].0;
            let low_pair = pairs[1].0;
            let kicker = ranks
                .iter()
                .filter(|&&r| r != high_pair && r != low_pair)
                .max()
                .unwrap_or(&0);

            return Some(HandEvaluation {
                rank: HandRank::TwoPair,
                primary_rank: high_pair as i32,
                tiebreakers: vec![low_pair as i32, *kicker as i32],
                description: format!(
                    "Two Pair, {} and {}",
                    Rank::from_u8(high_pair).unwrap(),
                    Rank::from_u8(low_pair).unwrap()
                ),
            });
        }
        None
    }

    fn check_pair(&self, cards: &[Card]) -> Option<HandEvaluation> {
        let ranks: Vec<_> = cards.iter().map(|c| c.rank as u8).collect();
        let mut rank_counts: Vec<(u8, usize)> = (2..=14)
            .map(|r| (r, ranks.iter().filter(|&&x| x == r).count()))
            .collect();
        rank_counts.sort_by_key(|&(_, count)| std::cmp::Reverse(count));

        if let Some((rank, 2)) = rank_counts.first() {
            let mut kickers: Vec<_> = ranks.iter().filter(|&&r| r != *rank).collect();
            kickers.sort();
            kickers.reverse();

            return Some(HandEvaluation {
                rank: HandRank::Pair,
                primary_rank: *rank as i32,
                tiebreakers: kickers.iter().take(3).map(|&&r| r as i32).collect(),
                description: format!("Pair of {}", Rank::from_u8(*rank).unwrap()),
            });
        }
        None
    }

    fn end_hand(&mut self) {
        self.game_stage = GameStage::HandComplete;
        self.broadcast_game_state();

        self.dealer_position = (self.dealer_position + 1) % std::cmp::max(self.players.len(), 1);

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
        assert_eq!(eval.primary_rank, 6);
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
        assert_eq!(p_updated.chips, initial_chips);
        assert_eq!(p_updated.current_bet, initial_bet);
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
        assert_eq!(p_updated.chips, initial_chips);
        assert_eq!(p_updated.current_bet, initial_bet);
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
}
