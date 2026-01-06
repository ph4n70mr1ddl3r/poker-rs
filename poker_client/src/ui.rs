use crate::game::ActionRequiredUpdate;
use crate::AppState;
use bevy_egui::egui;
use std::sync::mpsc;

fn render_poker_ui(ui: &mut egui::Ui, app_state: &mut AppState, ui_tx: &mpsc::Sender<String>) {
    ui.centered_and_justified(|ui| {
        ui.heading("Texas Hold'em Poker");
        ui.add_space(20.0);
        ui.label("Multiplayer Poker Game");
        ui.add_space(40.0);

        ui.label("Click the button below to connect to the server:");
        ui.add_space(20.0);

        if ui.button("Connect to Server").clicked() {
            let connect_msg = serde_json::json!({
                "type": "connect"
            });
            let _ = ui_tx.send(connect_msg.to_string());
        }

        ui.add_space(30.0);
        ui.label(
            egui::RichText::new("Server: ws://127.0.0.1:8080")
                .small()
                .color(egui::Color32::from_gray(150)),
        );

        ui.add_space(40.0);
        ui.separator();
        ui.add_space(20.0);
        ui.label("How to play:");
        ui.label("1. Connect to the server");
        ui.label("2. Wait for at least 2 players to join");
        ui.label("3. The game will automatically start");
        ui.label("4. Make your decisions when it's your turn");
    });
}

fn render_waiting_screen(ui: &mut egui::Ui, app_state: &mut AppState) {
    ui.centered_and_justified(|ui| {
        ui.heading("Waiting for Players");
        ui.add_space(20.0);

        let player_count = app_state.game_state.players.len();
        ui.label(format!(
            "{} player{} connected",
            player_count,
            if player_count != 1 { "s" } else { "" }
        ));

        ui.add_space(30.0);
        ui.spinner();
        ui.add_space(20.0);
        ui.label("Waiting for at least 2 players to start the game...");

        ui.add_space(30.0);
        ui.separator();
        ui.add_space(20.0);
        ui.heading("Players at Table:");
        ui.add_space(10.0);

        for (_, player) in &app_state.game_state.players {
            ui.horizontal(|ui| {
                ui.label("●");
                ui.label(&player.name);
                ui.label(format!("(${})", player.chips));
            });
        }

        ui.add_space(20.0);
        ui.label("Status messages:");
        for error in &app_state.game_state.errors {
            ui.colored_label(egui::Color32::from_gray(150), error);
        }
    });
}

fn render_players_panel(ui: &mut egui::Ui, app_state: &mut AppState) {
    ui.heading("Players");
    ui.separator();

    let player_count = app_state.game_state.players.len();
    ui.label(format!("{} at table", player_count));
    ui.add_space(10.0);

    egui::ScrollArea::vertical().show(ui, |ui| {
        for (player_id, player) in &app_state.game_state.players {
            let is_me = player_id == &app_state.game_state.my_id;
            let is_turn = app_state
                .game_state
                .action_required
                .as_ref()
                .map(|a| &a.player_id == player_id)
                .unwrap_or(false);

            let (status_icon, status_color) = if is_turn {
                ("⏳", egui::Color32::from_rgb(255, 165, 0))
            } else if player.is_folded {
                ("✗", egui::Color32::RED)
            } else if player.is_all_in {
                ("$", egui::Color32::from_rgb(0, 150, 0))
            } else {
                ("✓", egui::Color32::from_gray(150))
            };

            ui.horizontal(|ui| {
                ui.label(
                    egui::RichText::new(status_icon)
                        .color(status_color)
                        .size(16.0),
                );
                if is_me {
                    ui.label(
                        egui::RichText::new(&player.name)
                            .strong()
                            .color(egui::Color32::from_rgb(0, 120, 255)),
                    );
                } else {
                    ui.label(&player.name);
                }
                if is_me {
                    ui.label("(You)");
                }
            });

            ui.indent(player_id.as_str(), |ui| {
                ui.label(format!("Chips: ${}", player.chips));
                if player.current_bet > 0 {
                    ui.label(format!("Bet: ${}", player.current_bet));
                }
                if player.is_folded {
                    ui.label(
                        egui::RichText::new("FOLDED")
                            .color(egui::Color32::RED)
                            .small(),
                    );
                }
                if player.is_all_in {
                    ui.label(
                        egui::RichText::new("ALL-IN")
                            .color(egui::Color32::from_rgb(0, 150, 0))
                            .small(),
                    );
                }
            });

            ui.add_space(5.0);
        }
    });
}

fn render_table_panel(ui: &mut egui::Ui, app_state: &mut AppState) {
    ui.heading("Table");
    ui.separator();

    let card_size = egui::vec2(50.0, 70.0);
    let table_color = egui::Color32::from_rgb(34, 139, 34);

    egui::Frame::none().fill(table_color).show(ui, |ui| {
        ui.add_space(15.0);

        ui.vertical_centered(|ui| {
            let street = &app_state.game_state.current_street;
            let street_text = if street.is_empty() {
                "Pre-Game"
            } else {
                street
            };
            ui.label(
                egui::RichText::new(street_text)
                    .color(egui::Color32::from_rgb(255, 255, 200))
                    .size(20.0)
                    .strong(),
            );
        });

        ui.add_space(20.0);

        ui.horizontal_centered(|ui| {
            let community_cards = &app_state.game_state.community_cards;
            if community_cards.is_empty() {
                for _ in 0..5 {
                    render_card(ui, "[empty]", card_size, false);
                    ui.add_space(3.0);
                }
                ui.label(
                    egui::RichText::new("Cards will be dealt here")
                        .color(egui::Color32::from_gray(180)),
                );
            } else {
                for card in community_cards {
                    render_card(ui, card, card_size, true);
                    ui.add_space(5.0);
                }
            }
        });

        ui.add_space(25.0);

        ui.horizontal_centered(|ui| {
            let pot = app_state.game_state.pot;
            ui.label(
                egui::RichText::new(format!("POT: ${}", pot))
                    .color(egui::Color32::from_rgb(255, 255, 200))
                    .size(22.0)
                    .strong(),
            );
        });

        for side_pot in &app_state.game_state.side_pots {
            ui.horizontal_centered(|ui| {
                ui.label(
                    egui::RichText::new(format!("Side Pot: ${}", side_pot.0))
                        .color(egui::Color32::from_rgb(255, 200, 150))
                        .size(14.0),
                );
            });
        }

        ui.add_space(25.0);

        ui.heading("Your Hand");
        ui.add_space(5.0);

        if let Some(player) = app_state.game_state.get_my_player() {
            if player.hole_cards.is_empty() || player.hole_cards.iter().all(|c| c == "[hidden]") {
                ui.horizontal_centered(|ui| {
                    render_card(ui, "[hidden]", card_size, true);
                    ui.add_space(10.0);
                    render_card(ui, "[hidden]", card_size, true);
                });
                ui.add_space(10.0);
                ui.label(
                    egui::RichText::new("Waiting for cards...")
                        .color(egui::Color32::from_gray(200)),
                );
            } else {
                ui.horizontal_centered(|ui| {
                    for card in &player.hole_cards {
                        render_card(ui, card, card_size, true);
                        ui.add_space(10.0);
                    }
                });
            }
        } else {
            ui.label(
                egui::RichText::new("You haven't been dealt cards yet")
                    .color(egui::Color32::from_gray(180)),
            );
        }
    });

    ui.add_space(15.0);

    if let Some(ref showdown) = app_state.game_state.showdown {
        ui.heading("Showdown Results");
        ui.add_space(5.0);

        egui::ScrollArea::vertical()
            .max_height(120.0)
            .show(ui, |ui| {
                for (player_id, cards, hand_rank, description) in &showdown.hands {
                    let is_winner = showdown.winners.contains(player_id);

                    ui.horizontal(|ui| {
                        if is_winner {
                            ui.label("🏆");
                        }
                        let player_name = app_state
                            .game_state
                            .players
                            .get(player_id)
                            .map(|p| p.name.clone())
                            .unwrap_or_else(|| player_id.clone());
                        if is_winner {
                            ui.label(
                                egui::RichText::new(&player_name)
                                    .color(egui::Color32::GOLD)
                                    .strong(),
                            );
                        } else {
                            ui.label(&player_name);
                        }
                    });
                    ui.label(
                        egui::RichText::new(format!("{} - {}", hand_rank, description))
                            .color(egui::Color32::from_gray(180)),
                    );

                    ui.horizontal(|ui| {
                        for card in cards {
                            render_card(ui, card, egui::vec2(25.0, 35.0), true);
                            ui.add_space(2.0);
                        }
                    });
                    ui.add_space(5.0);
                }
            });
    }
}

fn render_card(ui: &mut egui::Ui, card: &str, size: egui::Vec2, show_borders: bool) {
    let (text, color) = parse_card(card);
    let rect = ui.available_rect_before_wrap();
    let painter = ui.painter();

    if card == "[empty]" {
        painter.rect_filled(rect.expand(2.0), 4.0, egui::Color32::from_rgb(40, 80, 40));
        return;
    }

    if show_borders {
        painter.rect_filled(
            rect.expand(2.0),
            4.0,
            egui::Color32::from_rgb(255, 255, 255),
        );
    }
    painter.rect_filled(rect, 4.0, egui::Color32::from_rgb(250, 250, 250));

    let font_size = size.y * 0.5;
    painter.text(
        rect.center(),
        egui::Align2::CENTER_CENTER,
        text,
        egui::FontId::proportional(font_size),
        color,
    );
}

fn parse_card(card: &str) -> (String, egui::Color32) {
    if card == "[hidden]" {
        return ("?".to_string(), egui::Color32::GRAY);
    }

    let chars: Vec<char> = card.chars().collect();
    let mut result = String::new();
    let mut color = egui::Color32::BLACK;

    for c in &chars {
        match c {
            '♥' | '♦' => color = egui::Color32::RED,
            '♣' | '♠' => color = egui::Color32::BLACK,
            _ => {}
        }
        result.push(*c);
    }

    (result, color)
}

fn render_actions_panel(ui: &mut egui::Ui, app_state: &mut AppState, ui_tx: &mpsc::Sender<String>) {
    ui.heading("Your Actions");
    ui.separator();

    if let Some(action) = app_state.game_state.action_required.clone() {
        if action.player_id == app_state.game_state.my_id {
            render_action_buttons(ui, app_state, &action, ui_tx);
        } else {
            ui.label(
                egui::RichText::new(format!("Waiting for {}...", action.player_name))
                    .color(egui::Color32::from_rgb(255, 165, 0)),
            );
            ui.add_space(20.0);
            render_chat_section(ui, app_state, ui_tx);
            ui.add_space(20.0);
            render_errors_section(ui, app_state);
        }
    } else {
        ui.label(
            egui::RichText::new("Waiting for next hand...").color(egui::Color32::from_gray(150)),
        );
        ui.add_space(20.0);
        render_chat_section(ui, app_state, ui_tx);
        ui.add_space(20.0);
        render_errors_section(ui, app_state);
    }
}

fn render_action_buttons(
    ui: &mut egui::Ui,
    app_state: &mut AppState,
    action: &ActionRequiredUpdate,
    ui_tx: &mpsc::Sender<String>,
) {
    let player_chips = action.player_chips;
    let min_raise = action.min_raise;
    let call_amount = action.current_bet;
    let can_check = call_amount == 0;

    ui.heading("It's Your Turn!");
    ui.add_space(10.0);

    ui.label(format!("Chips: ${}", player_chips));
    if call_amount > 0 {
        ui.label(format!("To call: ${}", call_amount));
    }
    ui.label(format!("Min raise: ${}", min_raise));

    ui.add_space(15.0);
    ui.separator();
    ui.add_space(10.0);
    ui.label("Quick Actions:");
    ui.add_space(5.0);

    ui.horizontal_wrapped(|ui| {
        if ui.button("🗑️ Fold").clicked() {
            let action_msg = serde_json::json!({
                "type": "action",
                "action": "Fold"
            });
            let _ = ui_tx.send(action_msg.to_string());
            app_state.game_state.action_required = None;
        }

        if can_check {
            if ui.button("✓ Check").clicked() {
                let action_msg = serde_json::json!({
                    "type": "action",
                    "action": "Check"
                });
                let _ = ui_tx.send(action_msg.to_string());
                app_state.game_state.action_required = None;
            }
        } else {
            if ui.button(format!("📞 Call ${}", call_amount)).clicked() {
                let action_msg = serde_json::json!({
                    "type": "action",
                    "action": "Call"
                });
                let _ = ui_tx.send(action_msg.to_string());
                app_state.game_state.action_required = None;
            }
        }

        if ui.button("💰 All-IN").clicked() {
            let action_msg = serde_json::json!({
                "type": "action",
                "action": "AllIn"
            });
            let _ = ui_tx.send(action_msg.to_string());
            app_state.game_state.action_required = None;
        }
    });

    ui.add_space(15.0);
    ui.separator();
    ui.add_space(10.0);
    ui.label("Raise / Bet:");
    ui.add_space(5.0);

    ui.label(format!("Min Raise: ${}", min_raise));
    ui.label(format!("Your Chips: ${}", player_chips));
    ui.add_space(5.0);

    ui.label("Raise to:");
    let mut raise_amount = min_raise;
    ui.add(egui::Slider::new(&mut raise_amount, min_raise..=player_chips).show_value(false));
    ui.horizontal(|ui| {
        if ui.button("⬆️ Raise").clicked() && raise_amount >= min_raise {
            let action_msg = serde_json::json!({
                "type": "action",
                "action": {"Raise": raise_amount}
            });
            let _ = ui_tx.send(action_msg.to_string());
            app_state.game_state.action_required = None;
        }
        ui.label(format!("${}", raise_amount));
    });

    ui.add_space(10.0);

    ui.label("Bet:");
    let mut bet_amount = if player_chips > 0 {
        player_chips / 2
    } else {
        0
    };
    ui.add(egui::Slider::new(&mut bet_amount, 0..=player_chips).show_value(false));
    ui.horizontal(|ui| {
        if ui.button("🎯 Bet").clicked() && bet_amount > 0 {
            let action_msg = serde_json::json!({
                "type": "action",
                "action": {"Bet": bet_amount}
            });
            let _ = ui_tx.send(action_msg.to_string());
            app_state.game_state.action_required = None;
        }
        ui.label(format!("${}", bet_amount));
    });

    ui.add_space(20.0);
    render_chat_section(ui, app_state, ui_tx);
    ui.add_space(20.0);
    render_errors_section(ui, app_state);
}

fn render_chat_section(ui: &mut egui::Ui, app_state: &mut AppState, ui_tx: &mpsc::Sender<String>) {
    ui.heading("Chat");
    ui.separator();

    egui::ScrollArea::vertical()
        .max_height(120.0)
        .show(ui, |ui| {
            for msg in &app_state.game_state.chat_messages {
                ui.label(format!("{}: {}", msg.player_name, msg.text));
            }
            if app_state.game_state.chat_messages.is_empty() {
                ui.label(egui::RichText::new("No messages").color(egui::Color32::from_gray(120)));
            }
        });

    ui.add_space(5.0);

    ui.text_edit_singleline(&mut app_state.chat_text);

    if ui.button("Send").clicked() && !app_state.chat_text.is_empty() {
        let chat_msg = serde_json::json!({
            "type": "chat",
            "text": app_state.chat_text.clone()
        });
        let _ = ui_tx.send(chat_msg.to_string());
        app_state.chat_text.clear();
    }
}

fn render_errors_section(ui: &mut egui::Ui, app_state: &mut AppState) {
    ui.heading("Messages");
    ui.separator();

    egui::ScrollArea::vertical()
        .max_height(60.0)
        .show(ui, |ui| {
            for error in &app_state.game_state.errors {
                ui.colored_label(egui::Color32::RED, error);
            }
            if app_state.game_state.errors.is_empty() {
                ui.label(egui::RichText::new("No messages").color(egui::Color32::from_gray(120)));
            }
        });
}
