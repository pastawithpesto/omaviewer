//! Conservative desktop relationships. Hyprland exposes capture identity but
//! no parent metadata, so related windows require an explicit phone selection.
use crate::WindowView;
use serde_json::{json, Value};
use std::collections::HashSet;

const MAX_RETURN_DEPTH: usize = 8;

#[derive(Default)]
pub(crate) struct DesktopState {
    root: Option<WindowView>,
    returns: Vec<WindowView>,
    known: HashSet<String>,
}

impl DesktopState {
    pub(crate) fn select(&mut self, target: Option<&WindowView>, clients: &Value) {
        self.root = target.cloned();
        self.returns.clear();
        self.known = clients
            .as_array()
            .into_iter()
            .flatten()
            .filter_map(|c| c["stableId"].as_str().map(str::to_owned))
            .collect();
    }

    pub(crate) fn open_related(&mut self, current: &WindowView) {
        if self.root.is_none() {
            self.root = Some(current.clone());
        }
        if self.returns.len() == MAX_RETURN_DEPTH {
            self.returns.remove(0);
        }
        self.returns.push(current.clone());
    }

    pub(crate) fn return_target(
        &mut self,
        clients: &Value,
        monitors: &Value,
    ) -> Option<WindowView> {
        while let Some(saved) = self.returns.pop() {
            if let Some(live) = refreshed(&saved, clients, monitors) {
                return Some(live);
            }
        }
        None
    }

    pub(crate) fn has_return(&self) -> bool {
        !self.returns.is_empty()
    }

    pub(crate) fn describe(
        &self,
        current: Option<&WindowView>,
        monitor: Option<&str>,
        clients: &Value,
        monitors: &Value,
    ) -> Value {
        let selected = current.map(|target| describe_window(target, clients));
        let root = self
            .root
            .as_ref()
            .map(|target| describe_window(target, clients));
        let related: Vec<Value> = current
            .into_iter()
            .flat_map(|target| {
                let parent = find(target, clients);
                clients
                    .as_array()
                    .into_iter()
                    .flatten()
                    .filter_map(move |candidate| {
                        let stable_id = candidate["stableId"].as_str()?;
                        if stable_id == target.stable_id
                            || candidate["mapped"].as_bool() == Some(false)
                        {
                            return None;
                        }
                        let same_app = parent.is_some_and(|parent| {
                            let pid = parent["pid"].as_i64().filter(|pid| *pid > 0);
                            let class = parent["class"].as_str().filter(|class| !class.is_empty());
                            pid.is_some() && candidate["pid"].as_i64() == pid
                                || class.is_some() && candidate["class"].as_str() == class
                        });
                        let newly_focused = !self.known.contains(stable_id)
                            && candidate["focusHistoryID"].as_i64() == Some(0);
                        if !same_app && !newly_focused {
                            return None;
                        }
                        let view = view_from_client(candidate, monitors)?;
                        let mut value = describe_window(&view, clients);
                        value["relationship"] = json!("unverified");
                        Some(value)
                    })
            })
            .take(12)
            .collect();
        json!({
            "target": selected.unwrap_or_else(|| json!({"kind":"monitor", "output":monitor})),
            "remembered_monitor": monitor,
            "root": root,
            "return_stack": self.returns.iter().map(|target| describe_window(target, clients)).collect::<Vec<_>>(),
            "can_return": self.returns.iter().any(|target| find(target, clients).is_some()),
            "related": related,
            "capabilities": {
                "automatic_dialog_follow": false,
                "automatic_return_from_related": true,
                "native_dialog_ownership": false,
                "portal_dialog_ownership": false,
                "reason": "Hyprland does not expose verified parent relationships. Open related windows explicitly, or show their monitor."
            }
        })
    }
}

pub(crate) fn find<'a>(target: &WindowView, clients: &'a Value) -> Option<&'a Value> {
    clients.as_array()?.iter().find(|c| {
        c["address"].as_str() == Some(&target.address)
            && c["stableId"].as_str() == Some(&target.stable_id)
            && c["mapped"].as_bool() != Some(false)
    })
}

pub(crate) fn refreshed(
    target: &WindowView,
    clients: &Value,
    monitors: &Value,
) -> Option<WindowView> {
    view_from_client(find(target, clients)?, monitors)
}

fn view_from_client(client: &Value, monitors: &Value) -> Option<WindowView> {
    let monitor_id = client["monitor"].as_i64()?;
    let output =
        monitors.as_array()?.iter().find(|m| {
            m["id"].as_i64() == Some(monitor_id) && m["disabled"].as_bool() != Some(true)
        })?["name"]
            .as_str()?
            .to_owned();
    Some(WindowView {
        address: client["address"].as_str()?.to_owned(),
        stable_id: client["stableId"]
            .as_str()
            .filter(|id| !id.is_empty())?
            .to_owned(),
        output,
    })
}

fn describe_window(target: &WindowView, clients: &Value) -> Value {
    let client = find(target, clients);
    json!({"kind":"window", "address":target.address, "stable_id":target.stable_id,
        "output":target.output, "title":client.map(|c| c["title"].clone()).unwrap_or(Value::Null),
        "available":client.is_some()})
}

#[cfg(test)]
mod tests {
    use super::*;
    fn window(id: &str) -> WindowView {
        WindowView {
            address: format!("0x{id}"),
            stable_id: id.into(),
            output: "DP-1".into(),
        }
    }
    fn clients() -> Value {
        json!([
            {"address":"0x1","stableId":"1","pid":9,"class":"app","monitor":0,"title":"root"},
            {"address":"0x2","stableId":"2","pid":9,"class":"app","monitor":0,"title":"other"},
            {"address":"0x3","stableId":"3","pid":10,"class":"portal","monitor":0,"title":"chooser","focusHistoryID":0}
        ])
    }
    fn monitors() -> Value {
        json!([{"id":0,"name":"DP-1"}])
    }
    #[test]
    fn candidates_are_explicit_and_same_process_never_proves_parentage() {
        let mut state = DesktopState::default();
        let clients = clients();
        state.select(Some(&window("1")), &clients);
        let value = state.describe(Some(&window("1")), Some("DP-1"), &clients, &monitors());
        assert_eq!(value["related"].as_array().unwrap().len(), 1);
        assert_eq!(value["related"][0]["relationship"], "unverified");
        assert_eq!(value["capabilities"]["automatic_dialog_follow"], false);
        assert!(!state.has_return());
    }
    #[test]
    fn return_skips_closed_and_reused_addresses() {
        let mut state = DesktopState::default();
        state.open_related(&window("1"));
        state.open_related(&window("2"));
        let mut clients = clients();
        clients[1]["stableId"] = json!("replacement");
        assert_eq!(
            state
                .return_target(&clients, &monitors())
                .unwrap()
                .stable_id,
            "1"
        );
        assert!(!state.has_return());
    }
    #[test]
    fn return_refreshes_output_and_explicit_selection_clears_stack() {
        let mut state = DesktopState::default();
        state.open_related(&window("1"));
        let monitors = json!([{"id":0,"name":"HDMI-A-1"}]);
        assert_eq!(
            state.return_target(&clients(), &monitors).unwrap().output,
            "HDMI-A-1"
        );
        state.open_related(&window("2"));
        state.select(None, &clients());
        assert!(!state.has_return());
    }
}
