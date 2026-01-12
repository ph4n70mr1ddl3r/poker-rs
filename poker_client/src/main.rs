use bevy::prelude::*;
use bevy_egui::{egui, EguiContexts, EguiPlugin};
use futures::SinkExt;
use futures::StreamExt;
use std::env;
use std::sync::mpsc;
use std::sync::{Arc, Mutex};
use tokio::runtime::Handle;
use tokio::time::{timeout, Duration};
use tokio_tungstenite::tungstenite::Message;

pub const DEFAULT_SERVER_ADDR: &str = "ws://127.0.0.1:8080";
pub const ENV_SERVER_ADDR: &str = "POKER_SERVER_URL";
pub const CONNECTION_TIMEOUT_SECS: u64 = 10;
pub const MAX_RECONNECT_ATTEMPTS: u32 = 10;
pub const INITIAL_RECONNECT_DELAY_MS: u64 = 1000;
pub const MAX_RECONNECT_DELAY_MS: u64 = 30000;
pub const PING_INTERVAL_SECS: u64 = 30;
pub const MAX_MESSAGE_SIZE: usize = 4096;

mod game;
mod network;

use game::PokerGameState;

fn mutex_poison_error(context: &str) -> String {
    format!(
        "Mutex poisoned during {} - connection state corrupted. Please restart the application.",
        context
    )
}

#[derive(Debug, Clone)]
#[allow(dead_code)]
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
    raise_amount: Mutex<String>,
}

impl Default for AppState {
    fn default() -> Self {
        Self {
            game_state: PokerGameState::new(),
            connected: false,
            raise_amount: Mutex::new(String::new()),
        }
    }
}

#[derive(Debug, Clone, PartialEq)]
enum ConnectionState {
    Disconnected,
    Connecting,
    Connected,
    Reconnecting(u32),
}

#[derive(Resource)]
struct NetworkResources {
    rx: Arc<Mutex<mpsc::Receiver<ClientNetworkMessage>>>,
    ui_tx: mpsc::Sender<String>,
    runtime: Arc<Handle>,
    server_addr: String,
    reconnect_state: Arc<Mutex<ReconnectState>>,
    connection_state: Arc<Mutex<ConnectionState>>,
    network_task: Arc<Mutex<Option<tokio::task::JoinHandle<()>>>>,
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
                title: format!("Texas Hold'em Poker - {}", client_name),
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
) -> Result<
    (
        tokio_tungstenite::WebSocketStream<
            tokio_tungstenite::MaybeTlsStream<tokio::net::TcpStream>,
        >,
        String,
    ),
    String,
> {
    loop {
        let delay = {
            let mut state = reconnect_state
                .lock()
                .map_err(|_| mutex_poison_error("reconnection state access"))?;
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

        let attempt = {
            reconnect_state
                .lock()
                .map_err(|_| mutex_poison_error("attempt number retrieval"))?
                .attempt
        };

        info!(
            "Attempting to connect to {} (attempt {})...",
            server_addr, attempt
        );

        let connection_result = timeout(
            Duration::from_secs(CONNECTION_TIMEOUT_SECS),
            tokio_tungstenite::connect_async(server_addr),
        )
        .await;

        match connection_result {
            Ok(Ok((ws_stream, _))) => {
                info!("WebSocket handshake successful!");
                {
                    let mut state = reconnect_state
                        .lock()
                        .map_err(|_| mutex_poison_error("state reset"))?;
                    state.reset();
                }
                return Ok((ws_stream, server_addr.to_string()));
            }
            Ok(Err(e)) => {
                warn!("Connection attempt failed: {}", e);
                let attempt = match reconnect_state.lock() {
                    Ok(guard) => guard.attempt,
                    Err(_) => {
                        let _ = tx_for_network.send(ClientNetworkMessage::Error(
                            mutex_poison_error("error reporting"),
                        ));
                        continue;
                    }
                };
                let _ = tx_for_network.send(ClientNetworkMessage::Error(format!(
                    "Connection failed (attempt {}): {}",
                    attempt, e
                )));
            }
            Err(_) => {
                warn!("Connection timed out");
                let attempt = match reconnect_state.lock() {
                    Ok(guard) => guard.attempt,
                    Err(_) => {
                        let _ = tx_for_network.send(ClientNetworkMessage::Error(
                            mutex_poison_error("timeout error reporting"),
                        ));
                        continue;
                    }
                };
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
    let runtime = Arc::new(Handle::current());
    let (tx, rx) = mpsc::channel::<ClientNetworkMessage>();
    let (ui_tx, ui_rx) = mpsc::channel::<String>();

    info!("Created mpsc channels");

    let _tx_for_network = tx.clone();
    let rx_arc = Arc::new(Mutex::new(rx));
    let server_addr = server_config.address.clone();
    let reconnect_state = Arc::new(Mutex::new(ReconnectState::new()));
    let connection_state = Arc::new(Mutex::new(ConnectionState::Disconnected));
    let network_task = Arc::new(Mutex::new(None::<tokio::task::JoinHandle<()>>));

    let reconnect_state_clone = reconnect_state.clone();
    let server_addr_clone = server_addr.clone();
    let connection_state_clone = connection_state.clone();
    let _network_task_clone = network_task.clone();
    let tx_for_reconnect = tx.clone();
    let ui_tx_for_task = ui_tx.clone();
    let tx_for_network_clone = tx.clone();
    let runtime_clone = runtime.clone();

    let task = runtime_clone.spawn(async move {
        let tx = tx_for_network_clone;
        let ui_tx = ui_tx_for_task;

        {
            let mut state = match connection_state_clone.lock() {
                Ok(guard) => guard,
                Err(_) => {
                    let _ = tx.send(ClientNetworkMessage::Error(mutex_poison_error(
                        "initial connection state setup",
                    )));
                    return;
                }
            };
            *state = ConnectionState::Connecting;
        }
        let (ws_stream, connected_addr) =
            match connect_with_retry(&server_addr_clone, &tx, &reconnect_state_clone).await {
                Ok(result) => result,
                Err(e) => {
                    error!("Failed to connect: {}", e);
                    let _ = tx.send(ClientNetworkMessage::Disconnected);
                    let _ = tx_for_reconnect.send(ClientNetworkMessage::Error(e));
                    {
                        let mut state = match connection_state_clone.lock() {
                            Ok(guard) => guard,
                            Err(_) => return,
                        };
                        *state = ConnectionState::Disconnected;
                    }
                    return;
                }
            };

        info!("Connected to server at {}", connected_addr);
        {
            let mut state = match connection_state_clone.lock() {
                Ok(guard) => guard,
                Err(_) => {
                    let _ = tx.send(ClientNetworkMessage::Error(mutex_poison_error(
                        "post-connection state update",
                    )));
                    return;
                }
            };
            *state = ConnectionState::Connected;
        }

        let (mut write, read) = ws_stream.split();

        let connect_msg = serde_json::json!({
            "type": "connect"
        });
        info!("Sending connect message: {}", connect_msg);
        if let Err(e) = write
            .send(Message::Text(connect_msg.to_string().into()))
            .await
        {
            error!("Failed to send connect message: {}", e);
            let _ = tx.send(ClientNetworkMessage::Error(
                "Failed to send connect message".to_string(),
            ));
            let _ = tx.send(ClientNetworkMessage::Disconnected);
            return;
        }

        let (write_tx, mut write_rx) = tokio::sync::mpsc::channel::<String>(100);
        let write_task = tokio::spawn(async move {
            while let Some(msg) = write_rx.recv().await {
                debug!("Sending to server: {} bytes", msg.len());
                let _ = write.send(Message::Text(msg.into())).await;
            }
        });

        let tx_clone = tx.clone();
        let ui_tx_for_pong = ui_tx.clone();
        let read_task = tokio::spawn(async move {
            let mut read = read;
            while let Some(result) = read.next().await {
                match result {
                    Ok(Message::Text(text)) => {
                        if text.len() > MAX_MESSAGE_SIZE {
                            error!("Received message too large: {} bytes", text.len());
                            let _ = tx_clone
                                .send(ClientNetworkMessage::Error("Message too large".to_string()));
                            let _ = tx_clone.send(ClientNetworkMessage::Disconnected);
                            break;
                        }
                        debug!("Received from server: {} bytes", text.len());
                        if let Ok(server_msg) = crate::network::parse_message(&text) {
                            debug!("Parsed message type");
                            if let crate::network::NetworkMessage::Ping(timestamp) = server_msg {
                                let pong_msg = serde_json::json!({
                                    "type": "Pong",
                                    "timestamp": timestamp
                                });
                                let _ = ui_tx_for_pong.send(pong_msg.to_string());
                            }
                            let client_msg = convert_message(server_msg);
                            let send_result = tx_clone.send(client_msg);
                            debug!("Sent to main thread: {:?}", send_result);
                        }
                    }
                    Ok(Message::Close(_)) => {
                        let _ = tx_clone.send(ClientNetworkMessage::Disconnected);
                        break;
                    }
                    Err(e) => {
                        error!("WebSocket error: {}", e);
                        let _ = tx_clone.send(ClientNetworkMessage::Error(e.to_string()));
                        let _ = tx_clone.send(ClientNetworkMessage::Disconnected);
                        break;
                    }
                    _ => {}
                }
            }
        });

        let ui_rx_for_task = ui_rx;
        let write_tx_for_forward = write_tx.clone();
        let forward_task = tokio::spawn(async move {
            while let Ok(msg) = ui_rx_for_task.recv() {
                let _ = write_tx_for_forward.send(msg).await;
            }
        });

        let mut ping_interval = tokio::time::interval(Duration::from_secs(PING_INTERVAL_SECS));
        let ping_tx = tx.clone();
        let ping_task = tokio::spawn(async move {
            loop {
                ping_interval.tick().await;
                let ping_msg = serde_json::json!({
                    "type": "Ping",
                    "timestamp": 0
                });
                if let Err(e) = write_tx.send(ping_msg.to_string()).await {
                    error!("Failed to queue ping: {}", e);
                    let _ = ping_tx.send(ClientNetworkMessage::Error("Ping failed".to_string()));
                    let _ = ping_tx.send(ClientNetworkMessage::Disconnected);
                    break;
                }
            }
        });

        let _ = tokio::join!(read_task, write_task, forward_task, ping_task);

        let _ = tx.send(ClientNetworkMessage::Disconnected);
        {
            let mut state = match connection_state_clone.lock() {
                Ok(guard) => guard,
                Err(_) => return,
            };
            *state = ConnectionState::Disconnected;
        }
    });

    {
        let mut task_guard = match network_task.lock() {
            Ok(guard) => guard,
            Err(_) => {
                error!("Failed to acquire network_task lock");
                return;
            }
        };
        *task_guard = Some(task);
    }

    commands.insert_resource(NetworkResources {
        rx: rx_arc,
        ui_tx,
        runtime,
        server_addr,
        reconnect_state,
        connection_state,
        network_task,
    });
}

fn handle_network_messages(
    mut app_state: ResMut<AppState>,
    network_res: Res<NetworkResources>,
    mut commands: Commands,
) {
    let rx = match network_res.rx.lock() {
        Ok(guard) => guard,
        Err(_) => {
            let error_msg = mutex_poison_error("network channel access");
            error!("{}", error_msg);
            app_state.game_state.add_error(error_msg);
            app_state.connected = false;
            return;
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
                    info!("Disconnected - triggering reconnection...");
                    trigger_reconnection(&network_res, &mut commands);
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
                    app_state.game_state.players.remove(&player_id);
                    if app_state.game_state.my_id == player_id {
                        app_state.connected = false;
                        trigger_reconnection(&network_res, &mut commands);
                    }
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
            trigger_reconnection(&network_res, &mut commands);
        }
    }
}

fn trigger_reconnection(network_res: &Res<NetworkResources>, _commands: &mut Commands) {
    let server_addr = network_res.server_addr.clone();
    let reconnect_state = network_res.reconnect_state.clone();
    let connection_state = network_res.connection_state.clone();
    let network_task = network_res.network_task.clone();
    let runtime = network_res.runtime.clone();

    let attempt = {
        let state = match reconnect_state.lock() {
            Ok(guard) => guard,
            Err(_) => return,
        };
        state.attempt + 1
    };

    {
        let mut state = match connection_state.lock() {
            Ok(guard) => guard,
            Err(_) => return,
        };
        *state = ConnectionState::Reconnecting(attempt);
    }

    let (tx, _) = mpsc::channel::<ClientNetworkMessage>();
    let tx_clone = tx.clone();
    let ui_tx = network_res.ui_tx.clone();

    let task = runtime.spawn(async move {
        let (ws_stream, connected_addr) =
            match connect_with_retry(&server_addr, &tx_clone, &reconnect_state).await {
                Ok(result) => result,
                Err(e) => {
                    error!("Reconnection failed: {}", e);
                    {
                        let mut state = match connection_state.lock() {
                            Ok(guard) => guard,
                            Err(_) => return,
                        };
                        *state = ConnectionState::Disconnected;
                    }
                    let _ = tx_clone.send(ClientNetworkMessage::Error(format!(
                        "Reconnection failed: {}",
                        e
                    )));
                    return;
                }
            };

        info!("Reconnected to server at {}", connected_addr);
        {
            let mut state = match connection_state.lock() {
                Ok(guard) => guard,
                Err(_) => return,
            };
            *state = ConnectionState::Connected;
        }

        let (mut write, read) = ws_stream.split();

        let connect_msg = serde_json::json!({
            "type": "connect"
        });
        let _ = write
            .send(Message::Text(connect_msg.to_string().into()))
            .await;

        let (_write_tx, mut write_rx) = tokio::sync::mpsc::channel::<String>(100);
        let write_task = tokio::spawn(async move {
            while let Some(msg) = write_rx.recv().await {
                let _ = write.send(Message::Text(msg.into())).await;
            }
        });

        let tx_clone = tx.clone();
        let ui_tx_clone = ui_tx.clone();
        let read_task = tokio::spawn(async move {
            let mut read = read;
            while let Some(result) = read.next().await {
                match result {
                    Ok(Message::Text(text)) => {
                        if text.len() > MAX_MESSAGE_SIZE {
                            error!("Received message too large: {} bytes", text.len());
                            let _ = tx_clone
                                .send(ClientNetworkMessage::Error("Message too large".to_string()));
                            break;
                        }
                        if let Ok(server_msg) = crate::network::parse_message(&text) {
                            if let crate::network::NetworkMessage::Ping(timestamp) = server_msg {
                                let pong_msg = serde_json::json!({
                                    "type": "Pong",
                                    "timestamp": timestamp
                                });
                                let _ = ui_tx_clone.send(pong_msg.to_string());
                            }
                            let client_msg = convert_message(server_msg);
                            let _ = tx_clone.send(client_msg);
                        }
                    }
                    Ok(Message::Close(_)) => {
                        let _ = tx_clone.send(ClientNetworkMessage::Disconnected);
                        break;
                    }
                    Err(e) => {
                        error!("WebSocket error during reconnection: {}", e);
                        let _ = tx_clone.send(ClientNetworkMessage::Error(e.to_string()));
                        let _ = tx_clone.send(ClientNetworkMessage::Disconnected);
                        break;
                    }
                    _ => {}
                }
            }
        });

        let _ = tokio::join!(read_task, write_task);

        let _ = tx.send(ClientNetworkMessage::Disconnected);
        {
            let mut state = match connection_state.lock() {
                Ok(guard) => guard,
                Err(_) => return,
            };
            *state = ConnectionState::Disconnected;
        }
    });

    {
        let mut task_guard = match network_task.lock() {
            Ok(guard) => guard,
            Err(_) => return,
        };
        if let Some(old_task) = task_guard.take() {
            old_task.abort();
        }
        *task_guard = Some(task);
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

                    ui.add_space(10.0);

                    ui.label("Raise: $");
                    let raise_amount_str = {
                        match app_state.raise_amount.try_lock() {
                            Ok(guard) => (*guard).clone(),
                            Err(_) => {
                                ui.label("System busy, try again");
                                return;
                            }
                        }
                    };
                    let mut raise_amount_str = raise_amount_str;
                    let raise_input =
                        egui::TextEdit::singleline(&mut raise_amount_str).desired_width(80.0);
                    ui.add(raise_input);

                    let raise_amount_result = raise_amount_str.parse::<i32>();
                    let raise_amount_clamped = raise_amount_result
                        .ok()
                        .map(|v| v.clamp(1, action_player_chips))
                        .unwrap_or(0);
                    let is_valid_raise = raise_amount_clamped > 0;

                    let raise_btn = egui::Button::new("Raise")
                        .fill(egui::Color32::from_rgb(0, 100, 200))
                        .min_size(egui::Vec2::new(100.0, 40.0));
                    let can_raise = is_valid_raise
                        && raise_amount_clamped >= action_min_raise
                        && raise_amount_clamped <= action_player_chips;
                    if ui.add_enabled(can_raise, raise_btn).clicked() {
                        if let Ok(msg) = serde_json::to_string(&serde_json::json!({
                            "type": "action",
                            "action": "Raise",
                             "amount": raise_amount_clamped
                        })) {
                            let _ = network_res.ui_tx.send(msg);
                            info!("Sent Raise action: ${}", raise_amount_clamped);
                            if let Ok(mut guard) = app_state.raise_amount.try_lock() {
                                guard.clear();
                            }
                        }
                    }

                    ui.add_space(10.0);

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
                ui.add_space(10.0);
                let mut pending_chat_text =
                    if let Ok(guard) = app_state.game_state.pending_chat.try_lock() {
                        guard.clone()
                    } else {
                        ui.label("System busy, try again");
                        return;
                    };
                let chat_input = egui::TextEdit::singleline(&mut pending_chat_text)
                    .desired_width(180.0)
                    .hint_text("Type a message...");
                if ui.add(chat_input).lost_focus()
                    && ui.input(|i| i.key_pressed(egui::Key::Enter))
                    && !pending_chat_text.is_empty()
                {
                    if let Ok(msg) = serde_json::to_string(&serde_json::json!({
                        "type": "chat",
                        "text": pending_chat_text
                    })) {
                        let _ = network_res.ui_tx.send(msg);
                        info!("Sent chat message");
                        if let Ok(mut guard) = app_state.game_state.pending_chat.try_lock() {
                            guard.clear();
                        }
                    }
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
        crate::network::NetworkMessage::Ping(_) => {
            ClientNetworkMessage::Error("Unexpected Ping".to_string())
        }
        crate::network::NetworkMessage::Pong(_) => {
            ClientNetworkMessage::Error("Unexpected Pong".to_string())
        }
    }
}
