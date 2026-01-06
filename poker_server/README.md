# Poker Application with Rust and Bevy

A complete No Limit Hold'em poker application with a WebSocket server and Bevy game client.

## Project Structure

```
testbevy/
├── poker_server/
│   ├── Cargo.toml
│   └── src/
│       ├── main.rs
│       ├── game.rs
│       ├── server.rs
│       └── messages.rs
│
└── poker_client/
    ├── Cargo.toml
    └── src/
        ├── main.rs
        ├── game.rs
        ├── ui.rs
        └── network.rs
```

## Features

### Server Features
- WebSocket server using tokio-tungstenite
- No Limit Hold'em poker logic
- Headsup (2 players) table
- Game states: preflop, flop, turn, river, showdown
- Actions: fold, check, call, bet, raise, all-in
- Complete pot calculation and betting rounds
- Hand evaluation with rankings:
  - High Card
  - Pair
  - Two Pair
  - Three of a Kind
  - Straight
  - Flush
  - Full House
  - Four of a Kind
  - Straight Flush
- Small blind and big blind posting
- Button position rotation
- Handle multiple client connections

### Client Features
- Bevy 0.13+ for rendering
- bevy_egui for UI components
- Clean UI showing cards, chips, pot, and actions
- WebSocket client using tokio-tungstenite
- Display game state from server
- Clickable action buttons (fold, check, bet, raise, call, all-in)
- Card visualization with proper colors
- Chat functionality
- Error display

## Protocol

JSON messages over WebSocket:

### Client -> Server
```json
{"Action":"Fold"}
{"Action":"Check"}
{"Action":"Call"}
{"Action":"Bet","amount":100}
{"Action":"Raise","amount":200}
{"Action":"AllIn"}
{"Chat":"Hello!"}
```

### Server -> Client
```json
{"GameStateUpdate":{"game_id":"main_table","hand_number":1,"pot":20,...}}
{"ActionRequired":{"player_id":"...","min_raise":20,"current_bet":10,...}}
{"PlayerConnected":{"player_id":"...","player_name":"..."}}
{"Showdown":{"community_cards":["Ah","Kh","Qh"],"hands":[...],"winners":["..."]}}
{"Chat":{"player_id":"...","player_name":"...","text":"Hello!","timestamp":..."}}
```

## Requirements

- Rust 1.70 or later
- Cargo package manager
- Windows, macOS, or Linux

## Building

### Install Rust (if not installed)

```bash
curl --proto '=https' --tlsv1.2 -sSf https://sh.rustup.rs | sh
```

### Build the Server

```bash
cd poker_server
cargo build --release
```

### Build the Client

```bash
cd poker_client
cargo build --release
```

## Running

### Start the Server

```bash
cd poker_server
cargo run --release
```

The server will listen on `ws://127.0.0.1:8080`

### Start the Client

```bash
cd poker_client
cargo run --release
```

The client window will open. Click "Connect" to join the poker table.

## How to Play

1. Start the server first
2. Start the client (can run multiple instances for testing)
3. Click "Connect" in the client
4. When 2 players are connected, the game automatically starts
5. Wait for your turn (highlighted in the UI)
6. Use the action buttons to play:
   - **Fold**: Surrender your hand
   - **Check**: Pass when no bet is required
   - **Call**: Match the current bet
   - **Bet**: Place chips in the pot
   - **Raise**: Increase the current bet
   - **All-In**: Bet all your remaining chips
7. Watch the showdown to see who wins!

## Betting Rules

- Small blind and big blind are posted automatically
- Minimum bet is the big blind
- Minimum raise is 2x the current bet
- All-in can be done at any time with any amount
- Side pots are calculated when players go all-in

## Hand Evaluation

The server evaluates hands using the standard poker hand rankings:
1. Straight Flush (highest)
2. Four of a Kind
3. Full House
4. Flush
5. Straight
6. Three of a Kind
7. Two Pair
8. One Pair
9. High Card (lowest)

Ties are broken by comparing kickers appropriately.

## Architecture

### Server Architecture
- **main.rs**: Entry point, WebSocket handler, connection management
- **server.rs**: Player management, message routing, broadcast system
- **game.rs**: Core poker logic, hand evaluation, betting rounds
- **messages.rs**: Protocol message definitions

### Client Architecture
- **main.rs**: Bevy app setup, network thread, UI rendering
- **game.rs**: Game state management, message parsing
- **ui.rs**: egui-based rendering, action buttons
- **network.rs**: WebSocket connection, async message handling

## Customization

### Changing Blind Levels
Edit `poker_server/src/main.rs`:
```rust
let game = server.lock().unwrap().create_game(
    "main_table".to_string(),
    5,  // small blind
    10, // big blind
);
```

### Changing Starting Chips
Edit the chip amount when registering players:
```rust
s.register_player(player_id.clone(), name, 1000); // 1000 chips
```

## Troubleshooting

### Connection Issues
- Ensure the server is running before the client
- Check that port 8080 is not blocked by firewall
- Verify the WebSocket URL is correct

### Build Errors
- Ensure Rust 1.70+ is installed
- Run `cargo update` to update dependencies
- Clear cargo cache: `cargo clean`

### Performance
- For better performance, build with `--release` flag
- The server handles connections in separate async tasks
- Client network operations run in a dedicated thread
