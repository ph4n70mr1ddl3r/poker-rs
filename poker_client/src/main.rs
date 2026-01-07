use bevy::prelude::*;
use bevy_egui::{egui, EguiContexts, EguiPlugin};
use chrono;
use futures::SinkExt;
use futures::StreamExt;
use std::env;
use std::sync::mpsc;
use std::sync::{Arc, Mutex};
use tokio::runtime::Runtime;
use tokio::time::{timeout, Duration};
use tokio_tungstenite::tungstenite::Message;
use uuid::Uuid;

pub const DEFAULT_SERVER_ADDR: &str = "ws://127.0.0.1:8080";
pub const ENV_SERVER_ADDR: &str = "POKER_SERVER_URL";
pub const CONNECTION_TIMEOUT_SECS: u64 = 10;
pub const MAX_RECONNECT_ATTEMPTS: u32 = 10;
pub const INITIAL_RECONNECT_DELAY_MS: u64 = 1000;
pub const MAX_RECONNECT_DELAY_MS: u64 = 30000;
pub const PING_INTERVAL_SECS: u64 = 30;

mod game;
mod network;

use game::PokerGameState;

#[derive(Debug, Clone)]
enum ClientNetworkMessage {
    Connected(String),
    PlayerIdConfirmed(String),
    GameStateUpdate(crate::game::GameStateUpdate),
    PlayerUpdates(Vec<crate::game::PlayerUpdate>),
    ActionRequired(crate::game::ActionRequiredUpdate),
    PlayerConnected(crate::game::PlayerConnectedUpdate),
    PlayerDisconnected(String),
    Showdown(crate::game::ShowdownUpdate),
    Chat(crate::game::ChatMessage),
    Error(String),
    Disconnected,
    Reconnecting(u32),
}

#[derive(Debug, Clone)]
struct ReconnectState {
    attempt: u32,
    delay_ms: u64,
}

impl ReconnectState {
    fn new() -> Self {
        Self {
            attempt: 0,
            delay_ms: INITIAL_RECONNECT_DELAY_MS,
        }
    }

    fn next_attempt(&mut self) -> Option<u64> {
        if self.attempt >= MAX_RECONNECT_ATTEMPTS {
            return None;
        }
        self.attempt += 1;
        let delay = std::cmp::min(self.delay_ms, MAX_RECONNECT_DELAY_MS);
        self.delay_ms = std::cmp::min(self.delay_ms * 2, MAX_RECONNECT_DELAY_MS);
        Some(delay)
    }

    fn reset(&mut self) {
        self.attempt = 0;
        self.delay_ms = INITIAL_RECONNECT_DELAY_MS;
    }
}

#[derive(Resource)]
struct AppState {
    game_state: PokerGameState,
    connected: bool,
}

impl Default for AppState {
    fn default() -> Self {
        Self {
            game_state: PokerGameState::new(),
            connected: false,
        }
    }
}

#[derive(Resource)]
struct NetworkResources {
    rx: Arc<Mutex<mpsc::Receiver<ClientNetworkMessage>>>,
    ui_tx: mpsc::Sender<String>,
    _runtime: Runtime,
    server_addr: String,
    reconnect_state: Arc<Mutex<ReconnectState>>,
}

fn main() {
    let args: Vec<String> = std::env::args().collect();
    let client_name = if args.len() > 1 {
        args[1].clone()
    } else {
        "Poker".to_string()
    };

    let server_addr = get_server_address();

    App::new()
        .add_plugins(DefaultPlugins.set(WindowPlugin {
            primary_window: Some(Window {
                title: format!("Texas Hold'em Poker - {}", client_name).into(),
                resolution: (1200., 700.).into(),
                ..default()
            }),
            ..default()
        }))
        .add_plugins(EguiPlugin)
        .insert_resource(AppState::default())
        .insert_resource(ServerConfig {
            address: server_addr,
        })
        .add_systems(Startup, setup_network)
        .add_systems(Update, (handle_network_messages, update_ui))
        .run();
}

fn get_server_address() -> String {
    env::var(ENV_SERVER_ADDR).unwrap_or_else(|_| {
        let args: Vec<String> = env::args().collect();
        if args.len() > 2 {
            args[2].clone()
        } else {
            DEFAULT_SERVER_ADDR.to_string()
        }
    })
}

async fn connect_with_retry(
    server_addr: &str,
    tx_for_network: &mpsc::Sender<ClientNetworkMessage>,
    reconnect_state: &Arc<Mutex<ReconnectState>>,
) -> Result<(), String> {
    loop {
        let delay = {
            let mut state = reconnect_state.lock();
            match state.next_attempt() {
                Some(d) => d,
                None => {
                    let _ = tx_for_network.send(ClientNetworkMessage::Error(
                        "Max reconnection attempts reached".to_string(),
                    ));
                    return Err("Max reconnection attempts reached".to_string());
                }
            }
        };

        info!(
            "Attempting to connect to {} (attempt {})...",
            server_addr,
            { reconnect_state.lock().attempt }
        );

        let connection_result = timeout(
            Duration::from_secs(CONNECTION_TIMEOUT_SECS),
            tokio_tungstenite::connect_async(server_addr),
        )
        .await;

        match connection_result {
            Ok(Ok((ws_stream, _))) => {
                info!("WebSocket handshake successful!");
                reconnect_state.lock().reset();
                return Ok((ws_stream, server_addr.to_string()));
            }
            Ok(Err(e)) => {
                warn!("Connection attempt failed: {}", e);
                let attempt = reconnect_state.lock().attempt;
                let _ = tx_for_network.send(ClientNetworkMessage::Error(format!(
                    "Connection failed (attempt {}): {}",
                    attempt, e
                )));
            }
            Err(_) => {
                warn!("Connection timed out");
                let attempt = reconnect_state.lock().attempt;
                let _ = tx_for_network.send(ClientNetworkMessage::Error(format!(
                    "Connection timed out (attempt {})",
                    attempt
                )));
            }
        }

        info!("Waiting {}ms before retry...", delay);
        tokio::time::sleep(Duration::from_millis(delay)).await;
    }
}

#[derive(Resource)]
struct ServerConfig {
    address: String,
}

fn setup_network(mut commands: Commands, server_config: Res<ServerConfig>) {
    let rt = match Runtime::new() {
        Ok(rt) => rt,
        Err(e) => {
            error!("Failed to create Tokio runtime: {}", e);
            return;
        }
    };
    let (tx, rx) = mpsc::channel::<ClientNetworkMessage>();
    let (ui_tx, ui_rx) = mpsc::channel::<String>();

    info!("Created mpsc channels");

    let tx_for_network = tx.clone();
    let rx_arc = Arc::new(Mutex::new(rx));
    let server_addr = server_config.address.clone();
    let reconnect_state = Arc::new(Mutex::new(ReconnectState::new()));

    let reconnect_state_clone = reconnect_state.clone();
    let server_addr_clone = server_addr.clone();

    rt.spawn(async move {
        let (ws_stream, connected_addr) =
            match connect_with_retry(&server_addr_clone, &tx_for_network, &reconnect_state_clone)
                .await
            {
                Ok(result) => result,
                Err(e) => {
                    error!("Failed to connect: {}", e);
                    return;
                }
            };

        info!("Connected to server at {}", connected_addr);

        let (mut write, read) = ws_stream.split();

        let player_id = Uuid::new_v4().to_string();
        let send_result = tx_for_network.send(ClientNetworkMessage::Connected(player_id.clone()));
        info!("Sent Connected message: {:?}", send_result);

        let connect_msg = serde_json::json!({
            "type": "connect",
            "player_id": player_id
        });
        info!("Sending connect message: {}", connect_msg);
        let _ = write.send(Message::Text(connect_msg.to_string())).await;

        let (write_tx, mut write_rx) = tokio::sync::mpsc::channel::<String>(100);
        let write_task = tokio::spawn(async move {
            while let Some(msg) = write_rx.recv().await {
                info!("Sending to server: {}", msg);
                let _ = write.send(Message::Text(msg)).await;
            }
        });

        let read_task = tokio::spawn(async move {
            let mut read = read;
            while let Some(result) = read.next().await {
                match result {
                    Ok(Message::Text(text)) => {
                        info!("Received from server: {} bytes", text.len());
                        if let Ok(server_msg) = crate::network::parse_message(&text) {
                            info!("Parsed message type");
                            let client_msg = convert_message(server_msg);
                            let send_result = tx_for_network.send(client_msg);
                            info!("Sent to main thread: {:?}", send_result);
                        }
                    }
                    Ok(Message::Close(_)) => {
                        let _ = tx_for_network.send(ClientNetworkMessage::Disconnected);
                        break;
                    }
                    Err(e) => {
                        error!("WebSocket error: {}", e);
                        let _ = tx_for_network.send(ClientNetworkMessage::Error(e.to_string()));
                        break;
                    }
                    _ => {}
                }
            }
        });

        let ui_rx = ui_rx;
        let write_tx = write_tx;
        let forward_task = tokio::spawn(async move {
            while let Ok(msg) = ui_rx.recv() {
                let _ = write_tx.send(msg).await;
            }
        });

        let mut ping_interval = tokio::time::interval(Duration::from_secs(PING_INTERVAL_SECS));
        let ping_tx_for_ping = tx_for_network.clone();
        let ping_task = tokio::spawn(async move {
            loop {
                ping_interval.tick().await;
                let timestamp = chrono::Utc::now().timestamp_millis() as u64;
                let ping_msg = serde_json::json!({
                    "type": "Ping",
                    "timestamp": timestamp
                });
                info!("Sending Ping #{}", timestamp);
                if let Err(e) = write.send(Message::Text(ping_msg.to_string())).await {
                    error!("Failed to send ping: {}", e);
                    let _ = ping_tx_for_ping
                        .send(ClientNetworkMessage::Error("Ping failed".to_string()));
                    break;
                }
            }
        });

        let _ = tokio::join!(read_task, write_task, forward_task, ping_task);

        let _ = tx_for_network.send(ClientNetworkMessage::Disconnected);
    });

    commands.insert_resource(NetworkResources {
        rx: rx_arc,
        ui_tx,
        _runtime: rt,
        server_addr,
        reconnect_state,
    });
}

fn handle_network_messages(mut app_state: ResMut<AppState>, network_res: Res<NetworkResources>) {
    let rx = match network_res.rx.lock() {
        Ok(guard) => guard,
        Err(poisoned) => {
            error!("Mutex poisoned, attempting to recover...");
            match poisoned.into_inner() {
                Some(guard) => guard,
                None => {
                    error!("Cannot recover from mutex poisoning - channel lost");
                    app_state
                        .game_state
                        .add_error("Network channel error - please restart".to_string());
                    return;
                }
            }
        }
    };
    match rx.try_recv() {
        Ok(message) => {
            drop(rx);
            info!("Got message: {:?}", message);
            match message {
                ClientNetworkMessage::Connected(id) => {
                    app_state.connected = true;
                    info!("Connected with temp ID: {}", id);
                }
                ClientNetworkMessage::PlayerIdConfirmed(id) => {
                    app_state.connected = true;
                    app_state.game_state.my_id = id.clone();
                    info!("Server confirmed my player ID: {}", id);
                }
                ClientNetworkMessage::Error(msg) => {
                    error!("Error: {}", msg);
                    app_state.game_state.add_error(msg);
                }
                ClientNetworkMessage::Disconnected => {
                    app_state.connected = false;
                    info!("Disconnected");
                }
                ClientNetworkMessage::Reconnecting(attempt) => {
                    app_state.connected = false;
                    info!("Reconnecting (attempt {})...", attempt);
                    app_state.game_state.add_error(format!(
                        "Disconnected. Reconnecting (attempt {})...",
                        attempt
                    ));
                }
                ClientNetworkMessage::PlayerDisconnected(player_id) => {
                    info!("Player disconnected: {}", player_id);
                }
                ClientNetworkMessage::PlayerConnected(update) => {
                    info!(
                        "Player connected: {} ({})",
                        update.player_name, update.player_id
                    );
                    if app_state.game_state.my_id.is_empty() {
                        app_state.game_state.my_id = update.player_id.clone();
                        info!("Set my_id to server ID: {}", update.player_id);
                    }
                    app_state.game_state.add_player(
                        update.player_name.clone(),
                        update.player_id.clone(),
                        update.chips,
                    );
                }
                ClientNetworkMessage::PlayerUpdates(updates) => {
                    info!("Player updates received: {} players", updates.len());

                    for update in updates {
                        app_state.game_state.update_player(update);
                    }

                    info!("My ID is: {}", app_state.game_state.my_id);
                }
                ClientNetworkMessage::GameStateUpdate(update) => {
                    info!(
                        "Game state update: hand #{}, pot: ${}",
                        update.hand_number, update.pot
                    );
                    app_state.game_state.apply_update(update);
                }
                ClientNetworkMessage::ActionRequired(update) => {
                    info!(
                        "Action required from: {} (my_id: {})",
                        update.player_id, app_state.game_state.my_id
                    );
                    app_state.game_state.set_action_required(update);
                }
                ClientNetworkMessage::Showdown(update) => {
                    info!("Showdown! Winners: {:?}", update.winners);
                    app_state.game_state.show_showdown(update);
                }
                ClientNetworkMessage::Chat(msg) => {
                    info!("Chat from {}: {}", msg.player_name, msg.text);
                    app_state.game_state.add_chat_message(msg);
                }
            }
        }
        Err(mpsc::TryRecvError::Empty) => {}
        Err(mpsc::TryRecvError::Disconnected) => {
            info!("Channel disconnected!");
        }
    }
}

fn update_ui(
    mut contexts: EguiContexts,
    app_state: ResMut<AppState>,
    network_res: Res<NetworkResources>,
) {
    let ctx = contexts.ctx_mut();

    egui::CentralPanel::default().show(ctx, |ui| {
        ui.set_min_size(egui::Vec2::new(800.0, 600.0));

        ui.horizontal(|ui| {
            ui.heading("Texas Hold'em Poker");
            ui.add_space(20.0);
            let status_color = if app_state.connected {
                egui::Color32::GREEN
            } else {
                egui::Color32::RED
            };
            ui.label(
                egui::RichText::new(if app_state.connected {
                    "● Connected"
                } else {
                    "○ Disconnected"
                })
                .color(status_color),
            );
            ui.add_space(20.0);
            ui.label(format!(
                "Hand #{} | {}",
                app_state.game_state.hand_number, app_state.game_state.current_street
            ));
        });

        ui.add_space(10.0);
        ui.separator();
        ui.add_space(10.0);

        let available = ui.available_rect_before_wrap();
        let table_center = available.center();
        let table_width = available.width() * 0.9;
        let table_height = available.height() * 0.7;

        let table_rect =
            egui::Rect::from_center_size(table_center, egui::vec2(table_width, table_height));
        ui.painter()
            .rect_filled(table_rect, 30.0, egui::Color32::from_rgb(0, 100, 0));
        ui.painter().rect_stroke(
            table_rect,
            30.0,
            egui::Stroke::new(6.0, egui::Color32::from_rgb(139, 69, 19)),
        );

        let card_spacing = 50.0;
        let card_size = egui::Vec2::new(45.0, 63.0);
        let cards_start = table_center.x - (card_spacing * 2.0);
        let cards_y = table_center.y - 31.5;

        for (i, card) in app_state.game_state.community_cards.iter().enumerate() {
            let card_rect = egui::Rect::from_min_size(
                egui::pos2(cards_start + i as f32 * card_spacing, cards_y),
                card_size,
            );
            draw_card(ui.painter(), card_rect, card);
        }

        ui.painter().text(
            egui::pos2(table_center.x, table_center.y + 60.0),
            egui::Align2::CENTER_CENTER,
            format!("POT: ${}", app_state.game_state.pot),
            egui::FontId::proportional(24.0),
            egui::Color32::from_rgb(255, 215, 0),
        );

        let villain_y = table_rect.top() + 40.0;
        let villain_pos = egui::pos2(table_center.x, villain_y);

        let mut villain_opt = None;
        let mut hero_opt = None;
        for (id, player) in &app_state.game_state.players {
            if id == &app_state.game_state.my_id {
                hero_opt = Some(player);
            } else {
                villain_opt = Some(player);
            }
        }

        if let Some(villain) = villain_opt {
            let villain_rect = egui::Rect::from_center_size(villain_pos, egui::vec2(180.0, 80.0));
            ui.painter()
                .rect_filled(villain_rect, 8.0, egui::Color32::from_rgb(30, 30, 50));
            ui.painter().rect_stroke(
                villain_rect,
                8.0,
                egui::Stroke::new(2.0, egui::Color32::from_rgb(100, 100, 150)),
            );

            ui.painter().text(
                egui::pos2(villain_pos.x, villain_pos.y - 20.0),
                egui::Align2::CENTER_CENTER,
                &villain.name,
                egui::FontId::proportional(16.0),
                egui::Color32::WHITE,
            );
            ui.painter().text(
                egui::pos2(villain_pos.x, villain_pos.y + 5.0),
                egui::Align2::CENTER_CENTER,
                format!("${} | Bet: ${}", villain.chips, villain.current_bet),
                egui::FontId::proportional(14.0),
                egui::Color32::from_rgb(200, 200, 200),
            );

            if !villain.hole_cards.is_empty()
                && (app_state.game_state.action_required.is_none()
                    || villain.is_folded
                    || villain.is_all_in)
            {
                let card1_rect = egui::Rect::from_center_size(
                    egui::pos2(villain_pos.x - 30.0, villain_pos.y + 35.0),
                    egui::Vec2::new(35.0, 49.0),
                );
                let card2_rect = egui::Rect::from_center_size(
                    egui::pos2(villain_pos.x + 30.0, villain_pos.y + 35.0),
                    egui::Vec2::new(35.0, 49.0),
                );
                draw_card(ui.painter(), card1_rect, &villain.hole_cards[0]);
                if villain.hole_cards.len() > 1 {
                    draw_card(ui.painter(), card2_rect, &villain.hole_cards[1]);
                }
            } else if villain.hole_cards.is_empty() {
                let card1_rect = egui::Rect::from_center_size(
                    egui::pos2(villain_pos.x - 30.0, villain_pos.y + 35.0),
                    egui::Vec2::new(35.0, 49.0),
                );
                let card2_rect = egui::Rect::from_center_size(
                    egui::pos2(villain_pos.x + 30.0, villain_pos.y + 35.0),
                    egui::Vec2::new(35.0, 49.0),
                );
                draw_back_of_card(ui.painter(), card1_rect);
                draw_back_of_card(ui.painter(), card2_rect);
            }
        }

        if let Some(hero) = hero_opt {
            let hero_y = table_rect.bottom() - 40.0;
            let hero_pos = egui::pos2(table_center.x, hero_y);

            let hero_rect = egui::Rect::from_center_size(hero_pos, egui::vec2(200.0, 100.0));
            ui.painter()
                .rect_filled(hero_rect, 8.0, egui::Color32::from_rgb(30, 50, 80));
            ui.painter().rect_stroke(
                hero_rect,
                8.0,
                egui::Stroke::new(2.0, egui::Color32::from_rgb(100, 150, 200)),
            );

            ui.painter().text(
                egui::pos2(hero_pos.x, hero_pos.y - 25.0),
                egui::Align2::CENTER_CENTER,
                format!("{} (You)", hero.name),
                egui::FontId::proportional(16.0),
                egui::Color32::from_rgb(100, 200, 255),
            );
            ui.painter().text(
                egui::pos2(hero_pos.x, hero_pos.y),
                egui::Align2::CENTER_CENTER,
                format!("${} | Bet: ${}", hero.chips, hero.current_bet),
                egui::FontId::proportional(14.0),
                egui::Color32::from_rgb(200, 200, 200),
            );

            if !hero.hole_cards.is_empty() {
                let card1_rect = egui::Rect::from_center_size(
                    egui::pos2(hero_pos.x - 35.0, hero_pos.y + 35.0),
                    egui::Vec2::new(40.0, 56.0),
                );
                let card2_rect = egui::Rect::from_center_size(
                    egui::pos2(hero_pos.x + 35.0, hero_pos.y + 35.0),
                    egui::Vec2::new(40.0, 56.0),
                );
                draw_card(ui.painter(), card1_rect, &hero.hole_cards[0]);
                if hero.hole_cards.len() > 1 {
                    draw_card(ui.painter(), card2_rect, &hero.hole_cards[1]);
                }
            }
        }

        ui.add_space(ui.available_rect_before_wrap().height() - 100.0);
        ui.separator();
        ui.add_space(10.0);

        if let Some(action) = &app_state.game_state.action_required {
            let is_my_turn = action.player_id == app_state.game_state.my_id;
            let action_min_raise = action.min_raise;
            let action_current_bet = action.current_bet;
            let action_player_chips = action.player_chips;

            if is_my_turn {
                ui.colored_label(egui::Color32::GREEN, "YOUR TURN!");
                ui.label(format!(
                    "Min raise: ${} | Your chips: ${}",
                    action_min_raise, action_player_chips
                ));

                ui.add_space(15.0);
                ui.horizontal(|ui| {
                    let fold_btn = egui::Button::new("Fold")
                        .fill(egui::Color32::RED)
                        .min_size(egui::Vec2::new(100.0, 40.0));
                    if ui.add(fold_btn).clicked() {
                        if let Ok(msg) = serde_json::to_string(&serde_json::json!({
                            "type": "action",
                            "action": "Fold"
                        })) {
                            let _ = network_res.ui_tx.send(msg);
                            info!("Sent Fold action");
                        }
                    }

                    let can_check = action_current_bet == 0;
                    let call_amount = action_current_bet;

                    if can_check {
                        let check_btn = egui::Button::new("Check")
                            .fill(egui::Color32::from_rgb(0, 150, 0))
                            .min_size(egui::Vec2::new(100.0, 40.0));
                        if ui.add(check_btn).clicked() {
                            if let Ok(msg) = serde_json::to_string(&serde_json::json!({
                                "type": "action",
                                "action": "Check"
                            })) {
                                let _ = network_res.ui_tx.send(msg);
                                info!("Sent Check action");
                            }
                        }
                    } else {
                        let call_btn = egui::Button::new(format!("Call ${}", call_amount))
                            .fill(egui::Color32::from_rgb(0, 150, 0))
                            .min_size(egui::Vec2::new(120.0, 40.0));
                        if ui.add(call_btn).clicked() {
                            if let Ok(msg) = serde_json::to_string(&serde_json::json!({
                                "type": "action",
                                "action": "Call"
                            })) {
                                let _ = network_res.ui_tx.send(msg);
                                info!("Sent Call action");
                            }
                        }
                    }

                    let allin_btn = egui::Button::new("All-In")
                        .fill(egui::Color32::from_rgb(255, 165, 0))
                        .min_size(egui::Vec2::new(100.0, 40.0));
                    if ui.add(allin_btn).clicked() {
                        if let Ok(msg) = serde_json::to_string(&serde_json::json!({
                            "type": "action",
                            "action": "AllIn"
                        })) {
                            let _ = network_res.ui_tx.send(msg);
                            info!("Sent AllIn action");
                        }
                    }
                });
            } else {
                ui.label(
                    egui::RichText::new(format!("Waiting for {}...", action.player_name))
                        .color(egui::Color32::from_rgb(255, 165, 0)),
                );
            }
        } else {
            ui.label("Waiting for next hand...");
        }

        ui.allocate_ui_at_rect(
            egui::Rect::from_min_size(
                egui::pos2(available.right() - 200.0, available.top() + 50.0),
                egui::vec2(180.0, 200.0),
            ),
            |ui| {
                ui.heading("Chat");
                for msg in app_state.game_state.chat_messages.iter().rev().take(6) {
                    ui.label(
                        egui::RichText::new(format!("{}: {}", msg.player_name, msg.text))
                            .size(12.0),
                    );
                }
            },
        );

        for error in &app_state.game_state.errors {
            ui.add_space(10.0);
            ui.colored_label(egui::Color32::RED, format!("Error: {}", error));
        }
    });
}

fn draw_card(painter: &egui::Painter, rect: egui::Rect, card: &str) {
    painter.rect_filled(rect, 4.0, egui::Color32::WHITE);
    painter.rect_stroke(rect, 4.0, egui::Stroke::new(1.0, egui::Color32::BLACK));

    if card.is_empty() || card.len() < 2 {
        return;
    }

    let suit_char = card.chars().last().unwrap_or('?');
    let color = match suit_char {
        '♥' => egui::Color32::RED,
        '♦' => egui::Color32::from_rgb(0, 100, 200),
        '♣' | '♠' => egui::Color32::BLACK,
        _ => egui::Color32::BLACK,
    };

    let rank: String = card.chars().take(card.len() - 1).collect();
    let card_text = format!("{}{}", rank, suit_char);

    painter.text(
        rect.center(),
        egui::Align2::CENTER_CENTER,
        card_text,
        egui::FontId::proportional(18.0),
        color,
    );
}

fn draw_back_of_card(painter: &egui::Painter, rect: egui::Rect) {
    painter.rect_filled(rect, 4.0, egui::Color32::from_rgb(200, 50, 50));
    painter.rect_stroke(rect, 4.0, egui::Stroke::new(2.0, egui::Color32::BLACK));
    painter.text(
        rect.center(),
        egui::Align2::CENTER_CENTER,
        "♠",
        egui::FontId::proportional(24.0),
        egui::Color32::from_rgb(150, 0, 0),
    );
}

fn convert_message(msg: crate::network::NetworkMessage) -> ClientNetworkMessage {
    match msg {
        crate::network::NetworkMessage::PlayerIdConfirmed(id) => {
            ClientNetworkMessage::PlayerIdConfirmed(id)
        }
        crate::network::NetworkMessage::GameState(update) => {
            ClientNetworkMessage::GameStateUpdate(update)
        }
        crate::network::NetworkMessage::PlayerUpdates(updates) => {
            ClientNetworkMessage::PlayerUpdates(updates)
        }
        crate::network::NetworkMessage::ActionRequired(update) => {
            ClientNetworkMessage::ActionRequired(update)
        }
        crate::network::NetworkMessage::PlayerConnected(update) => {
            ClientNetworkMessage::PlayerConnected(update)
        }
        crate::network::NetworkMessage::PlayerDisconnected(id) => {
            ClientNetworkMessage::PlayerDisconnected(id)
        }
        crate::network::NetworkMessage::Showdown(update) => ClientNetworkMessage::Showdown(update),
        crate::network::NetworkMessage::Chat(msg) => ClientNetworkMessage::Chat(msg),
        crate::network::NetworkMessage::Error(msg) => ClientNetworkMessage::Error(msg),
        crate::network::NetworkMessage::Ping(timestamp) => {
            info!("Received Ping #{}", timestamp);
            let pong_msg = serde_json::json!({
                "type": "Pong",
                "timestamp": timestamp
            });
            let _ = network_res.ui_tx.send(pong_msg.to_string());
        }
        crate::network::NetworkMessage::Pong(timestamp) => {
            info!("Received Pong #{}", timestamp);
        }
    }
}
