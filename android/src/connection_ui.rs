use crate::{WindowLinkApp, storage, theme};
use eframe::egui::{self, RichText};

impl WindowLinkApp {
    pub(crate) fn draw_connection(&mut self, ui: &mut egui::Ui, palette: theme::Palette) {
        let width = ui.available_width().min(560.0);
        egui::Frame::new().inner_margin(18.0).show(ui, |ui| {
            ui.set_width((width - 36.0).max(1.0));
            ui.horizontal(|ui| {
                ui.label(RichText::new("OMAVIEWER").strong().color(palette.accent));
                ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                    self.settings_menu(ui)
                });
            });
            ui.add_space(20.0);
            ui.horizontal(|ui| {
                ui.heading("Desktops");
                if !self.editing_connection && ui.button("+ Add").clicked() {
                    self.new_desktop();
                }
            });
            ui.add_space(12.0);
            if !self.editing_connection && !self.saved_connections.is_empty() {
                let mut connect = None;
                let mut edit = None;
                for (index, desktop) in self.saved_connections.iter().enumerate() {
                    let card_width = ui.available_width();
                    egui::Frame::new()
                        .fill(palette.surface)
                        .corner_radius(14.0)
                        .inner_margin(14.0)
                        .show(ui, |ui| {
                            ui.set_width((card_width - 28.0).max(1.0));
                            ui.label(RichText::new(&desktop.name).size(22.0).strong());
                            if desktop.name != desktop.host {
                                ui.label(RichText::new(&desktop.host).color(palette.muted));
                            }
                            ui.add_space(8.0);
                            ui.horizontal(|ui| {
                                if ui
                                    .add_sized(
                                        [140.0, 48.0],
                                        egui::Button::new(
                                            RichText::new("Connect").color(palette.background),
                                        )
                                        .fill(palette.accent),
                                    )
                                    .clicked()
                                {
                                    connect = Some(index);
                                }
                                if ui
                                    .add_sized([80.0, 48.0], egui::Button::new("Edit"))
                                    .clicked()
                                {
                                    edit = Some(index);
                                }
                            });
                        });
                    ui.add_space(10.0);
                }
                if let Some(index) = edit {
                    self.apply_saved(index);
                    self.editing_original = Some((self.host.clone(), self.port.clone()));
                    self.editing_connection = true;
                }
                if let Some(index) = connect {
                    self.apply_saved(index);
                    self.start_connection();
                }
            } else {
                Self::connection_field(
                    ui,
                    palette,
                    "Name",
                    &mut self.connection_name,
                    "Home computer",
                    false,
                );
                Self::connection_field(
                    ui,
                    palette,
                    "Address",
                    &mut self.host,
                    "Tailscale address",
                    false,
                );
                Self::connection_field(
                    ui,
                    palette,
                    "Password",
                    &mut self.password,
                    "Desktop password",
                    true,
                );
                ui.collapsing("Connection settings", |ui| {
                    Self::connection_field(
                        ui,
                        palette,
                        "Username",
                        &mut self.username,
                        "windowlink",
                        false,
                    );
                    Self::connection_field(
                        ui,
                        palette,
                        "Desktop port",
                        &mut self.port,
                        "3389",
                        false,
                    );
                    Self::connection_field(
                        ui,
                        palette,
                        "Control address",
                        &mut self.control_host,
                        "Same as desktop",
                        false,
                    );
                    Self::connection_field(
                        ui,
                        palette,
                        "Control port",
                        &mut self.control_port,
                        "3391",
                        false,
                    );
                });
                ui.add_space(12.0);
                ui.horizontal_wrapped(|ui| {
                    let ready = !self.host.trim().is_empty() && !self.password.is_empty();
                    if ui
                        .add_enabled(
                            ready,
                            egui::Button::new(
                                RichText::new("Save and connect").color(palette.background),
                            )
                            .min_size(egui::vec2(160.0, 48.0))
                            .fill(palette.accent),
                        )
                        .clicked()
                    {
                        self.remember_connection = true;
                        self.start_connection();
                        self.editing_connection = false;
                    }
                    if ui
                        .add_enabled(
                            ready,
                            egui::Button::new("Save").min_size(egui::vec2(70.0, 48.0)),
                        )
                        .clicked()
                    {
                        self.remember_connection = true;
                        self.remember_current();
                        self.editing_connection = false;
                    }
                    if ui.button("Cancel").clicked() {
                        self.editing_connection = false;
                        self.editing_original = None;
                    }
                });
                if self.editing_original.is_some() && ui.button("Remove saved desktop").clicked() {
                    if let Some((host, port)) = self.editing_original.take() {
                        self.saved_connections
                            .retain(|entry| !entry.matches(&host, &port));
                        if let Err(error) =
                            storage::store(&self.connections_file, &self.saved_connections)
                        {
                            self.status = error;
                        }
                    }
                    self.editing_connection = false;
                }
                ui.collapsing("Setup help", |ui| {
                    ui.label(
                        "Use the Tailscale address and password from OmaViewer on your computer.",
                    );
                });
            }
            if !self.status.is_empty()
                && self.status != "Disconnected"
                && !self.status.starts_with("Loaded saved connection")
                && !self.status.starts_with("Enter the host details")
            {
                ui.add_space(12.0);
                ui.label(RichText::new(&self.status).color(palette.muted));
            }
        });
    }

    pub(crate) fn draw_desktop_switcher(&mut self, ui: &mut egui::Ui, _palette: theme::Palette) {
        let mut selected = None;
        ui.horizontal(|ui| {
            egui::ComboBox::from_id_salt("saved-desktop-switcher")
                .selected_text(if self.connection_name.is_empty() {
                    &self.host
                } else {
                    &self.connection_name
                })
                .width((ui.available_width() * 0.58).max(80.0))
                .show_ui(ui, |ui| {
                    for (index, desktop) in self.saved_connections.iter().enumerate() {
                        if ui
                            .selectable_label(
                                desktop.matches(&self.host, &self.port),
                                &desktop.name,
                            )
                            .clicked()
                        {
                            selected = Some(index);
                        }
                    }
                });
            if ui.button("Manage").clicked() {
                self.disconnect();
                self.editing_connection = false;
            }
        });
        if let Some(index) = selected {
            let is_current = self.saved_connections[index].matches(&self.host, &self.port);
            if !is_current {
                let target = self.saved_connections[index].clone();
                self.disconnect();
                if let Some(updated_index) = self
                    .saved_connections
                    .iter()
                    .position(|entry| entry.matches(&target.host, &target.port))
                {
                    self.apply_saved(updated_index);
                }
                self.start_connection();
            }
        }
    }

    fn new_desktop(&mut self) {
        self.cancel_reconnect();
        self.host.clear();
        self.password.clear();
        self.connection_name.clear();
        self.preferred_view.clear();
        self.control_host.clear();
        self.username = "windowlink".into();
        self.port = "3389".into();
        self.control_port = "3391".into();
        self.editing_original = None;
        self.editing_connection = true;
        self.status.clear();
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn saved_desktops_fit_portrait_and_landscape() {
        for size in [egui::vec2(360.0, 800.0), egui::vec2(800.0, 360.0)] {
            let ctx = egui::Context::default();
            theme::apply(&ctx, theme::Theme::TokyoNight.palette());
            let mut app = WindowLinkApp::default();
            app.saved_connections = vec![
                storage::SavedConnection {
                    name: "Home computer".into(),
                    host: "100.64.0.1".into(),
                    port: "3389".into(),
                    ..Default::default()
                },
                storage::SavedConnection {
                    name: "Studio computer".into(),
                    host: "100.64.0.2".into(),
                    port: "3389".into(),
                    ..Default::default()
                },
            ];
            for _ in 0..3 {
                let mut output = ctx.run_ui(
                    egui::RawInput {
                        screen_rect: Some(egui::Rect::from_min_size(egui::Pos2::ZERO, size)),
                        ..Default::default()
                    },
                    |ctx| {
                        egui::CentralPanel::default().show(ctx, |ui| {
                            egui::ScrollArea::vertical().show(ui, |ui| {
                                app.draw_connection(ui, theme::Theme::TokyoNight.palette());
                                assert!(
                                    ui.min_rect().right() <= size.x,
                                    "saved desktop cards overflow {size:?}: {:?}",
                                    ui.min_rect()
                                );
                            });
                        });
                    },
                );
                output.textures_delta.clear();
            }
        }
    }
}
