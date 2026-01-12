# AGENTS.md - Poker RS Development Guide

This file provides guidelines for AI agents working on this Texas Hold'em Poker implementation.

## Project Structure

Three-crate Rust workspace:
- **poker_protocol**: Shared types, serialization, game state
- **poker_server**: WebSocket server, game logic, player management
- **poker_client**: Bevy-based GUI client with Egui UI

## Build/Lint/Test Commands

```bash
# Build all crates
cargo build --all

# Build specific crate
cargo build -p poker_server
cargo build -p poker_client

# Run in release mode
cargo build --all --release

# Run all tests
cargo test --all

# Run single test
cargo test -p poker_server test_validate_bet_amount
cargo test -p poker_protocol test_parse_game_state_update
cargo test test_hand_comparison  # runs matching test name across all crates

# Run tests in specific file
cargo test --all -- poker_server::game::tests

# Check code (like clippy)
cargo check --all
cargo clippy --all

# Format code
cargo fmt
cargo fmt -- poker_protocol/src/lib.rs

# Audit dependencies
cargo audit
```

## Code Style Guidelines

### Imports
```rust
// 1. std imports
use std::collections::HashMap;
use std::sync::Arc;

// 2. External crates (alphabetically within groups)
use log::{debug, error};
use serde::{Deserialize, Serialize};
use tokio::sync::broadcast;

// 3. crate:: imports
use crate::game::PokerGame;

// 4. pub use re-exports at end
pub use types::{Card, PlayerState};
```

### Types and Structs
- Use `pub struct` for public types, keep fields `pub` for simple data types
- Use `#[derive(Debug, Clone)]` for most data types
- Use `#[allow(dead_code)]` sparingly for unused code paths
- Type aliases for common patterns: `pub type ServerResult<T> = Result<T, ServerError>;`

### Naming Conventions
```rust
// Structs/Enums: PascalCase
pub struct PokerGame { ... }
pub enum GameStage { ... }

// Constants: SCREAMING_SNAKE_CASE
const MAX_POT: i32 = i32::MAX / 2;
pub const MAX_BET_MULTIPLIER: i32 = 10;

// Functions/Variables: snake_case
fn calculate_new_pot(&mut self, amount: i32) -> Option<i32> { ... }
let current_player_id = ...

// Private fields: snake_case
pub struct PokerGame {
    pub game_id: String,
    deck: Vec<Card>,  // private field
}
```

### Error Handling
- Protocol errors: Use `thiserror` derive (`#[derive(Error, Debug)]`)
- Server errors: Use `anyhow` for propagating errors
- Never silently ignore errors; use `?` or handle explicitly
- Use `parking_lot::Mutex` instead of `std::sync::Mutex` for better performance
- Handle mutex poisoning gracefully with context messages

```rust
// Protocol errors with thiserror
#[derive(Error, Debug)]
pub enum ServerError {
    #[error("Invalid bet amount: {0}")]
    InvalidAmount,
    #[error("Player not found: {0}")]
    PlayerNotFound(String),
}

// Server result type
pub type ServerResult<T> = std::result::Result<T, ServerError>;
```

### Documentation
- Use `///` for doc comments on public APIs
- Document function purpose, arguments, and return values
- Include examples where helpful

```rust
/// Creates a new poker game instance.
///
/// # Arguments
/// * `game_id` - Unique identifier for this game table
/// * `small_blind` - Small blind amount
/// * `big_blind` - Big blind amount
/// * `tx` - Broadcast channel sender for game messages
pub fn new(game_id: String, small_blind: i32, big_blind: i32, tx: Sender<ServerMessage>) -> Self
```

### Async/Await Patterns
- Use `tokio::time::timeout` for operations with timeouts
- Use `tokio::sync::broadcast` for game state updates to all players
- Use `tokio::sync::mpsc` for per-player message sending
- Prefer non-blocking operations where possible

### Tests
- Place unit tests in `#[cfg(test)] mod tests` within the same file
- Integration tests can go in the same file or `tests/` directory
- Use descriptive test names: `test_descriptive_action_name`

```rust
#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_royal_flush() {
        let cards = vec![...];
        let eval = poker_game.evaluate_hand(&player);
        assert_eq!(eval.rank, HandRank::StraightFlush);
    }
}
```

### Game Logic Conventions
- Use `saturating_add/sub` for chip calculations to prevent overflow
- Validate bet amounts before processing
- Use `checked_add` for pot calculations
- Document configuration constants with their purposes

### Protocol Conventions
- Messages use serde_json for JSON serialization
- Protocol supports both typed `ClientMessage`/`ServerMessage` enums and raw JSON
- HMAC signing available for message authentication (configurable)
- Nonce cache prevents replay attacks

## Common Tasks

### Adding a New Protocol Message
1. Add enum variant to `ClientMessage` or `ServerMessage` in `poker_protocol/src/lib.rs`
2. Add serialization with `#[serde(tag = "type", content = "...")]`
3. Update message handling in `poker_server/src/server.rs`
4. Update parsing in `poker_client/src/network.rs`

### Adding a New Server Action
1. Add action handling in `poker_server/src/game.rs::handle_action`
2. Add validation if needed
3. Broadcast game state update
4. Advance action to next player

### Modifying Game Rules
1. Update `poker_server/src/game.rs` for server-side logic
2. Update `poker_protocol/src/types.rs` if types change
3. Add/update tests in `#[cfg(test)] mod tests`
4. Ensure client can still parse state updates

## Key Files
- Protocol types: `poker_protocol/src/types.rs`
- Protocol errors: `poker_protocol/src/errors.rs`
- Server main: `poker_server/src/main.rs`
- Game logic: `poker_server/src/game.rs`
- Client network: `poker_client/src/network.rs`
- Client game state: `poker_client/src/game.rs`
