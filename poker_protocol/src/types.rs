use serde::{Deserialize, Serialize};
use std::fmt;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum Suit {
    Clubs,
    Diamonds,
    Hearts,
    Spades,
}

impl fmt::Display for Suit {
    fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
        match self {
            Suit::Clubs => write!(f, "♣"),
            Suit::Diamonds => write!(f, "♦"),
            Suit::Hearts => write!(f, "♥"),
            Suit::Spades => write!(f, "♠"),
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum Rank {
    Two = 2,
    Three = 3,
    Four = 4,
    Five = 5,
    Six = 6,
    Seven = 7,
    Eight = 8,
    Nine = 9,
    Ten = 10,
    Jack = 11,
    Queen = 12,
    King = 13,
    Ace = 14,
}

impl Rank {
    pub fn from_u8(value: u8) -> Option<Self> {
        match value {
            2 => Some(Rank::Two),
            3 => Some(Rank::Three),
            4 => Some(Rank::Four),
            5 => Some(Rank::Five),
            6 => Some(Rank::Six),
            7 => Some(Rank::Seven),
            8 => Some(Rank::Eight),
            9 => Some(Rank::Nine),
            10 => Some(Rank::Ten),
            11 => Some(Rank::Jack),
            12 => Some(Rank::Queen),
            13 => Some(Rank::King),
            14 => Some(Rank::Ace),
            _ => None,
        }
    }
}

impl fmt::Display for Rank {
    fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
        match self {
            Rank::Two => write!(f, "2"),
            Rank::Three => write!(f, "3"),
            Rank::Four => write!(f, "4"),
            Rank::Five => write!(f, "5"),
            Rank::Six => write!(f, "6"),
            Rank::Seven => write!(f, "7"),
            Rank::Eight => write!(f, "8"),
            Rank::Nine => write!(f, "9"),
            Rank::Ten => write!(f, "10"),
            Rank::Jack => write!(f, "J"),
            Rank::Queen => write!(f, "Q"),
            Rank::King => write!(f, "K"),
            Rank::Ace => write!(f, "A"),
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct Card {
    pub suit: Suit,
    pub rank: Rank,
}

impl Card {
    pub fn new(suit: Suit, rank: Rank) -> Self {
        Self { suit, rank }
    }

    pub fn to_string(&self) -> String {
        format!("{}{}", self.rank, self.suit)
    }
}

impl fmt::Display for Card {
    fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
        write!(f, "{}{}", self.rank, self.suit)
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
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
            hole_cards: Vec::new(),
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct GameState {
    pub players: Vec<Player>,
    pub community_cards: Vec<String>,
    pub pot: i32,
    pub current_street: String,
    pub hand_number: i32,
}

impl Default for GameState {
    fn default() -> Self {
        Self::new()
    }
}

impl GameState {
    pub fn new() -> Self {
        Self {
            players: Vec::new(),
            community_cards: Vec::new(),
            pot: 0,
            current_street: String::new(),
            hand_number: 0,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum Street {
    Preflop,
    Flop,
    Turn,
    River,
    Showdown,
}

impl fmt::Display for Street {
    fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
        match self {
            Street::Preflop => write!(f, "Pre-Flop"),
            Street::Flop => write!(f, "Flop"),
            Street::Turn => write!(f, "Turn"),
            Street::River => write!(f, "River"),
            Street::Showdown => write!(f, "Showdown"),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum GameStage {
    WaitingForPlayers,
    PostingBlinds,
    DealingHoleCards,
    BettingRound(Street),
    Showdown,
    HandComplete,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PlayerState {
    pub id: String,
    pub name: String,
    pub chips: i32,
    pub current_bet: i32,
    pub hole_cards: Vec<Card>,
    pub has_acted: bool,
    pub is_all_in: bool,
    pub is_folded: bool,
    pub is_sitting_out: bool,
}

impl PlayerState {
    pub fn new(id: String, name: String, chips: i32) -> Self {
        Self {
            id,
            name,
            chips,
            current_bet: 0,
            hole_cards: Vec::new(),
            has_acted: false,
            is_all_in: false,
            is_folded: false,
            is_sitting_out: false,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
pub enum HandRank {
    HighCard,
    Pair,
    TwoPair,
    ThreeOfAKind,
    Straight,
    Flush,
    FullHouse,
    FourOfAKind,
    StraightFlush,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct HandEvaluation {
    pub rank: HandRank,
    pub primary_rank: i32,
    pub tiebreakers: Vec<i32>,
    pub description: String,
}

impl Ord for HandEvaluation {
    fn cmp(&self, other: &Self) -> std::cmp::Ordering {
        self.rank
            .cmp(&other.rank)
            .then_with(|| self.primary_rank.cmp(&other.primary_rank))
            .then_with(|| self.tiebreakers.cmp(&other.tiebreakers))
    }
}

impl PartialOrd for HandEvaluation {
    fn partial_cmp(&self, other: &Self) -> Option<std::cmp::Ordering> {
        Some(self.cmp(other))
    }
}

impl HandEvaluation {
    pub fn high_card(cards: &[Card]) -> Self {
        let mut ranks: Vec<_> = cards.iter().map(|c| c.rank as u8).collect();
        ranks.sort();
        ranks.reverse();

        let top_rank = ranks.first().map(|&r| r as i32).unwrap_or(0);
        let top_rank_display = ranks
            .first()
            .and_then(|&r| Rank::from_u8(r))
            .map(|r| r.to_string())
            .unwrap_or_else(|| "None".to_string());

        Self {
            rank: HandRank::HighCard,
            primary_rank: top_rank,
            tiebreakers: ranks.iter().take(5).map(|&r| r as i32).collect(),
            description: format!("High Card, {}", top_rank_display),
        }
    }

    pub fn pair(cards: &[Card], pair_rank: u8) -> Self {
        let mut ranks: Vec<_> = cards.iter().map(|c| c.rank as u8).collect();
        ranks.sort();
        ranks.reverse();

        let kickers: Vec<_> = ranks.iter().filter(|&&r| r != pair_rank).collect();
        let kickers: Vec<_> = kickers.iter().take(3).map(|&&r| r as i32).collect();

        let pair_rank_display = Rank::from_u8(pair_rank)
            .map(|r| r.to_string())
            .unwrap_or_else(|| "Unknown".to_string());

        Self {
            rank: HandRank::Pair,
            primary_rank: pair_rank as i32,
            tiebreakers: kickers,
            description: format!("Pair of {}", pair_rank_display),
        }
    }

    pub fn two_pair(cards: &[Card], high_pair: u8, low_pair: u8) -> Self {
        let mut ranks: Vec<_> = cards.iter().map(|c| c.rank as u8).collect();
        ranks.sort();
        ranks.reverse();

        let kicker = ranks
            .iter()
            .filter(|&&r| r != high_pair && r != low_pair)
            .max()
            .map(|&r| r as i32)
            .unwrap_or(0);

        let high_display = Rank::from_u8(high_pair)
            .map(|r| r.to_string())
            .unwrap_or_else(|| "Unknown".to_string());
        let low_display = Rank::from_u8(low_pair)
            .map(|r| r.to_string())
            .unwrap_or_else(|| "Unknown".to_string());

        Self {
            rank: HandRank::TwoPair,
            primary_rank: high_pair as i32,
            tiebreakers: vec![low_pair as i32, kicker],
            description: format!("Two Pair, {} and {}", high_display, low_display),
        }
    }

    pub fn three_of_a_kind(cards: &[Card], three_rank: u8) -> Self {
        let mut ranks: Vec<_> = cards.iter().map(|c| c.rank as u8).collect();
        ranks.sort();
        ranks.reverse();

        let kickers: Vec<_> = ranks
            .iter()
            .filter(|&&r| r != three_rank)
            .take(2)
            .map(|&r| r as i32)
            .collect();

        let three_rank_display = Rank::from_u8(three_rank)
            .map(|r| r.to_string())
            .unwrap_or_else(|| "Unknown".to_string());

        Self {
            rank: HandRank::ThreeOfAKind,
            primary_rank: three_rank as i32,
            tiebreakers: kickers,
            description: format!("Three of a Kind, {}", three_rank_display),
        }
    }

    pub fn straight(straight_high: u8) -> Self {
        let is_wheel = straight_high == 6;
        let description = if is_wheel {
            "5-4-3-2-A (Wheel)".to_string()
        } else {
            let straight_high_display = Rank::from_u8(straight_high)
                .map(|r| r.to_string())
                .unwrap_or_else(|| "Unknown".to_string());
            format!("{}", straight_high_display)
        };

        Self {
            rank: HandRank::Straight,
            primary_rank: straight_high as i32,
            tiebreakers: vec![straight_high as i32],
            description: format!("Straight, {}", description),
        }
    }

    pub fn flush(flush_cards: &[Card]) -> Self {
        let mut ranks: Vec<_> = flush_cards.iter().map(|c| c.rank as u8).collect();
        ranks.sort();
        ranks.reverse();

        let top_rank = ranks.first().map(|&r| r as i32).unwrap_or(0);
        let top_rank_display = ranks
            .first()
            .and_then(|&r| Rank::from_u8(r))
            .map(|r| r.to_string())
            .unwrap_or_else(|| "None".to_string());

        Self {
            rank: HandRank::Flush,
            primary_rank: top_rank,
            tiebreakers: ranks.iter().map(|&r| r as i32).collect(),
            description: format!("Flush, {}", top_rank_display),
        }
    }

    pub fn full_house(three_rank: u8, pair_rank: u8) -> Self {
        let three_rank_display = Rank::from_u8(three_rank)
            .map(|r| r.to_string())
            .unwrap_or_else(|| "Unknown".to_string());
        let pair_rank_display = Rank::from_u8(pair_rank)
            .map(|r| r.to_string())
            .unwrap_or_else(|| "Unknown".to_string());

        Self {
            rank: HandRank::FullHouse,
            primary_rank: three_rank as i32,
            tiebreakers: vec![pair_rank as i32],
            description: format!(
                "Full House, {} over {}",
                three_rank_display, pair_rank_display
            ),
        }
    }

    pub fn four_of_a_kind(cards: &[Card], four_rank: u8) -> Self {
        let mut ranks: Vec<_> = cards.iter().map(|c| c.rank as u8).collect();
        ranks.sort();
        ranks.reverse();

        let kicker = ranks
            .iter()
            .filter(|&&r| r != four_rank)
            .max()
            .map(|&r| r as i32)
            .unwrap_or(0);

        let four_rank_display = Rank::from_u8(four_rank)
            .map(|r| r.to_string())
            .unwrap_or_else(|| "Unknown".to_string());

        Self {
            rank: HandRank::FourOfAKind,
            primary_rank: four_rank as i32,
            tiebreakers: vec![kicker],
            description: format!("Four of a Kind, {}", four_rank_display),
        }
    }

    pub fn straight_flush(straight_high: u8) -> Self {
        let straight_high_display = Rank::from_u8(straight_high)
            .map(|r| r.to_string())
            .unwrap_or_else(|| "Unknown".to_string());

        Self {
            rank: HandRank::StraightFlush,
            primary_rank: straight_high as i32,
            tiebreakers: vec![straight_high as i32],
            description: format!("Straight Flush, {}", straight_high_display),
        }
    }
}
