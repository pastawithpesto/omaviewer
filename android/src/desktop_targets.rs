//! Small control-plane watcher for related windows and return navigation.
use eframe::egui;
use serde_json::{Value, json};
use std::{
    sync::mpsc,
    thread,
    time::{Duration, Instant},
};

enum Event {
    State(Result<Value, String>),
    Changed(Result<Value, String>),
}
pub struct DesktopTargets {
    state: Value,
    active_target: Value,
    generation: Option<u64>,
    instance_id: Option<String>,
    last_poll: Instant,
    polling: bool,
    switching: bool,
    changed: bool,
    error: String,
    tx: mpsc::Sender<Event>,
    rx: mpsc::Receiver<Event>,
}
impl Default for DesktopTargets {
    fn default() -> Self {
        let (tx, rx) = mpsc::channel();
        Self {
            state: Value::Null,
            active_target: Value::Null,
            generation: None,
            instance_id: None,
            last_poll: Instant::now() - Duration::from_secs(2),
            polling: false,
            switching: false,
            changed: false,
            error: String::new(),
            tx,
            rx,
        }
    }
}
impl DesktopTargets {
    pub fn key(&self) -> String {
        let target = &self.active_target;
        // A window keeps its viewport when it moves to another monitor.
        if target["kind"] == "window" {
            format!("window:{}", target["stable_id"])
        } else {
            format!("monitor:{}", target["output"])
        }
    }
    pub fn ack_generation(&mut self, generation: Option<u64>) {
        if let Some(generation) = generation {
            if self.generation.is_none_or(|current| generation >= current) {
                self.generation = Some(generation);
                self.last_poll = Instant::now() - Duration::from_secs(2);
            }
        }
    }
    pub fn ack_target(&mut self, generation: Option<u64>, target: Value) {
        if generation
            .zip(self.generation)
            .is_some_and(|(new, old)| new < old)
        {
            return;
        }
        self.ack_generation(generation);
        self.active_target = target;
    }
    pub fn poll(&mut self, host: &str, port: u16, token: &str) -> bool {
        while let Ok(event) = self.rx.try_recv() {
            match event {
                Event::State(result) => {
                    self.polling = false;
                    if let Ok(state) = result {
                        if let Some(instance) = state["instance_id"].as_str() {
                            if self
                                .instance_id
                                .as_deref()
                                .is_some_and(|previous| previous != instance)
                            {
                                self.generation = None;
                                self.changed = true;
                            }
                            self.instance_id = Some(instance.into());
                        }
                        let generation = state["generation"].as_u64();
                        // Ignore replies older than an acknowledged explicit selection.
                        if generation
                            .zip(self.generation)
                            .is_some_and(|(new, old)| new < old)
                        {
                            continue;
                        }
                        if state["status"] == "ready" {
                            if self.generation.is_some() && generation != self.generation {
                                self.changed = true;
                            }
                            self.generation = generation;
                            self.active_target = state["target"].clone();
                        }
                        self.state = state;
                    }
                }
                Event::Changed(result) => {
                    self.switching = false;
                    match result {
                        Ok(value) => {
                            let generation = value["generation"].as_u64();
                            if generation
                                .zip(self.generation)
                                .is_some_and(|(new, old)| new < old)
                            {
                                continue;
                            }
                            self.generation = generation;
                            self.state["target"] = value["target"].clone();
                            self.active_target = value["target"].clone();
                            self.changed = true;
                            self.error.clear();
                        }
                        Err(error) => self.error = error,
                    }
                    self.last_poll = Instant::now() - Duration::from_secs(2);
                }
            }
        }
        if !self.polling && !self.switching && self.last_poll.elapsed() >= Duration::from_secs(1) {
            self.polling = true;
            self.last_poll = Instant::now();
            let (host, token, tx) = (host.to_owned(), token.to_owned(), self.tx.clone());
            thread::spawn(move || {
                let result = request(&host, port, &token, "GET", "/desktop-state", &Value::Null);
                let _ = tx.send(Event::State(result));
            });
        }
        std::mem::take(&mut self.changed)
    }
    pub fn ui(&mut self, ui: &mut egui::Ui, host: &str, port: u16, token: &str) {
        let mut action = None;
        ui.horizontal_wrapped(|ui| {
            if self.state["can_return"] == true
                && ui
                    .add_enabled(!self.switching, egui::Button::new("Return to app"))
                    .clicked()
            {
                action = Some(("/target/return", json!({})));
            }
            if let Some(items) = self.state["related"].as_array() {
                for item in items.iter().take(3) {
                    let title = item["title"].as_str().unwrap_or("Related window");
                    let label: String = title.chars().take(28).collect();
                    if ui
                        .add_enabled(
                            !self.switching,
                            egui::Button::new(format!("Related: {label}")),
                        )
                        .clicked()
                    {
                        action = Some((
                            "/target/related",
                            json!({"address":item["address"],"stable_id":item["stable_id"]}),
                        ));
                    }
                }
            }
        });
        if !self.error.is_empty() {
            ui.label(&self.error);
        }
        if let Some(error) = self.state["error"].as_str() {
            ui.label(error);
        }
        if let Some((path, body)) = action {
            self.switching = true;
            let (host, token, tx) = (host.to_owned(), token.to_owned(), self.tx.clone());
            thread::spawn(move || {
                let _ = tx.send(Event::Changed(request(
                    &host, port, &token, "POST", path, &body,
                )));
            });
        }
    }
}
fn request(
    host: &str,
    port: u16,
    token: &str,
    method: &str,
    path: &str,
    body: &Value,
) -> Result<Value, String> {
    let bytes = if body.is_null() {
        Vec::new()
    } else {
        body.to_string().into_bytes()
    };
    let (_, bytes) =
        crate::window_picker::request(host, port, token, method, path, "application/json", &bytes)?;
    serde_json::from_slice(&bytes).map_err(|error| error.to_string())
}

#[cfg(test)]
mod tests {
    use super::*;
    fn state(generation: u64, status: &str, id: &str) -> Value {
        json!({"generation":generation,"status":status,"target":{"kind":"window","stable_id":id,"output":"DP-1"}})
    }
    fn deliver(targets: &mut DesktopTargets, event: Event) -> bool {
        targets.last_poll = Instant::now();
        targets.tx.send(event).unwrap();
        targets.poll("unused", 0, "")
    }
    #[test]
    fn switching_snapshot_does_not_relabel_the_old_capture_viewport() {
        let mut targets = DesktopTargets::default();
        assert!(!deliver(
            &mut targets,
            Event::State(Ok(state(1, "ready", "root")))
        ));
        let root_key = targets.key();
        assert!(!deliver(
            &mut targets,
            Event::State(Ok(state(2, "switching", "dialog")))
        ));
        assert_eq!(targets.key(), root_key);
        assert!(deliver(
            &mut targets,
            Event::State(Ok(state(2, "ready", "dialog")))
        ));
        assert_ne!(targets.key(), root_key);
    }
    #[test]
    fn delayed_related_reply_cannot_replace_a_newer_explicit_selection() {
        let mut targets = DesktopTargets::default();
        targets.ack_target(Some(5), state(5, "ready", "explicit")["target"].clone());
        let key = targets.key();
        assert!(!deliver(
            &mut targets,
            Event::Changed(Ok(state(4, "ready", "old")))
        ));
        assert!(!deliver(
            &mut targets,
            Event::State(Ok(state(4, "ready", "old")))
        ));
        assert_eq!(targets.key(), key);
        assert_eq!(targets.generation, Some(5));
    }
    #[test]
    fn restarted_host_can_reset_its_generation() {
        let mut targets = DesktopTargets::default();
        let mut old = state(20, "ready", "old");
        old["instance_id"] = json!("host-a");
        assert!(!deliver(&mut targets, Event::State(Ok(old))));
        let mut new = state(0, "ready", "new");
        new["instance_id"] = json!("host-b");
        assert!(deliver(&mut targets, Event::State(Ok(new))));
        assert_eq!(targets.generation, Some(0));
        assert_eq!(targets.key(), "window:\"new\"");
    }
    #[test]
    fn moving_window_keeps_its_viewport_identity() {
        let mut targets = DesktopTargets::default();
        deliver(&mut targets, Event::State(Ok(state(1, "ready", "root"))));
        let key = targets.key();
        let mut moved = state(2, "ready", "root");
        moved["target"]["output"] = json!("HDMI-A-1");
        assert!(deliver(&mut targets, Event::State(Ok(moved))));
        assert_eq!(targets.key(), key);
    }
}
