//! Map a captured toplevel's pixels to the shared desktop pointer.
use std::time::{Duration, Instant};

use anyhow::{Context, Result};
use serde_json::Value;

use crate::display::geometry::PresentationGeometry;
use crate::hyprland;

#[derive(Clone, Debug)]
pub(super) struct WindowGeometry {
    at: [f64; 2],
    size: [f64; 2],
    // Logical desktop bounds; virtual pointers not bound to an output use these.
    bounds: [f64; 4],
    focused: bool,
}

impl WindowGeometry {
    fn from_json(
        clients: &Value,
        monitors: &Value,
        address: &str,
        stable_id: Option<&str>,
    ) -> Option<Self> {
        let client = clients
            .as_array()?
            .iter()
            .find(|c| c["address"].as_str() == Some(address))?;
        if stable_id.is_some_and(|id| client["stableId"].as_str() != Some(id)) {
            return None;
        }
        let pair = |field: &str| {
            let p = client[field].as_array()?;
            Some([p.first()?.as_f64()?, p.get(1)?.as_f64()?])
        };
        let at = pair("at")?;
        let size = pair("size")?;
        if !at.into_iter().chain(size).all(f64::is_finite) || size.iter().any(|v| *v <= 0.0) {
            return None;
        }
        let mut bounds = [
            f64::INFINITY,
            f64::INFINITY,
            f64::NEG_INFINITY,
            f64::NEG_INFINITY,
        ];
        for m in monitors.as_array()? {
            if m["disabled"].as_bool() == Some(true) {
                continue;
            }
            let (x, y) = (m["x"].as_f64()?, m["y"].as_f64()?);
            let scale = m["scale"].as_f64()?;
            let (mut w, mut h) = (m["width"].as_f64()?, m["height"].as_f64()?);
            if ![x, y, scale, w, h].into_iter().all(f64::is_finite)
                || scale <= 0.0
                || w <= 0.0
                || h <= 0.0
            {
                return None;
            }
            if m["transform"].as_u64().unwrap_or(0) % 2 == 1 {
                std::mem::swap(&mut w, &mut h);
            }
            bounds[0] = bounds[0].min(x);
            bounds[1] = bounds[1].min(y);
            bounds[2] = bounds[2].max(x + w / scale);
            bounds[3] = bounds[3].max(y + h / scale);
        }
        if !bounds.into_iter().all(f64::is_finite)
            || bounds[2] <= bounds[0]
            || bounds[3] <= bounds[1]
        {
            return None;
        }
        Some(Self {
            at,
            size,
            bounds,
            focused: client["focusHistoryID"].as_i64() == Some(0),
        })
    }

    pub(super) fn map(
        &self,
        presentation: PresentationGeometry,
        x: u16,
        y: u16,
    ) -> (u32, u32, u32, u32) {
        let (sx, sy) = presentation.map_presentation_point_to_source(u32::from(x), u32::from(y));
        let source = presentation.source();
        let width = (self.bounds[2] - self.bounds[0])
            .ceil()
            .clamp(1.0, f64::from(u32::MAX)) as u32;
        let height = (self.bounds[3] - self.bounds[1])
            .ceil()
            .clamp(1.0, f64::from(u32::MAX)) as u32;
        let px =
            self.at[0] + f64::from(sx) * self.size[0] / f64::from(source.width) - self.bounds[0];
        let py =
            self.at[1] + f64::from(sy) * self.size[1] / f64::from(source.height) - self.bounds[1];
        (
            px.floor().clamp(0.0, f64::from(width.saturating_sub(1))) as u32,
            py.floor().clamp(0.0, f64::from(height.saturating_sub(1))) as u32,
            width,
            height,
        )
    }
}

pub(super) struct WindowTarget {
    address: String,
    stable_id: String,
    geometry: Option<(Instant, WindowGeometry)>,
    // An explicit selection may focus its target once. Afterwards, a focus
    // transfer may be a dialog or the local user: neither permits stealing it
    // back. Re-selecting the window creates a new target and grants focus again.
    focus_established: bool,
    pub(super) last_position: Option<(u16, u16)>,
}

impl WindowTarget {
    pub(super) fn new(address: String, stable_id: String) -> Self {
        let address = format!(
            "0x{}",
            address
                .strip_prefix("0x")
                .unwrap_or(&address)
                .to_ascii_lowercase()
        );
        Self {
            focus_established: hyprland::clients().ok().is_some_and(|clients| {
                clients.as_array().is_some_and(|items| items.iter().any(|client|
                    client["address"].as_str() == Some(&address)
                    && client["stableId"].as_str() == Some(&stable_id)
                    && client["focusHistoryID"].as_i64() == Some(0)))
            }),
            address,
            stable_id,
            geometry: None,
            last_position: None,
        }
    }

    /// Refresh on button/key actions, and at most once per 30 ms for motion.
    /// A missing target blocks new input rather than sending it to another app.
    pub(super) fn prepare(&mut self, force: bool) -> Result<WindowGeometry> {
        if force
            || self
                .geometry
                .as_ref()
                .is_none_or(|(t, _)| t.elapsed() >= Duration::from_millis(30))
        {
            let geometry = WindowGeometry::from_json(
                &hyprland::clients()?,
                &hyprland::monitors()?,
                &self.address,
                Some(&self.stable_id),
            )
            .context("selected window is no longer available")?;
            self.geometry = Some((Instant::now(), geometry));
        }
        let mut geometry = self
            .geometry
            .as_ref()
            .context("window geometry unavailable")?
            .1
            .clone();
        if !geometry.focused {
            anyhow::ensure!(
                !self.focus_established,
                "focus moved to another window; choose it in Related windows or re-select this window"
            );
            hyprland::focus_window(&self.address)?;
            geometry = WindowGeometry::from_json(
                &hyprland::clients()?,
                &hyprland::monitors()?,
                &self.address,
                Some(&self.stable_id),
            )
            .context("selected window closed while focusing")?;
            self.geometry = Some((Instant::now(), geometry.clone()));
        }
        anyhow::ensure!(geometry.focused, "selected window could not receive focus");
        self.focus_established = true;
        Ok(geometry)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::display::geometry::Size;
    use serde_json::json;


    #[test]
    fn window_mapping_handles_scale_negative_output_and_letterboxing() {
        let clients =
            json!([{"address":"0x1", "at":[-1100,100],"size":[800,600],"focusHistoryID":0}]);
        let monitors = json!([
            {"x":-1280,"y":0,"width":2560,"height":1440,"scale":2},
            {"x":0,"y":0,"width":1920,"height":1080,"scale":1}
        ]);
        let g = WindowGeometry::from_json(&clients, &monitors, "0x1", None).unwrap();
        let fit = PresentationGeometry::new(
            Size::new(1600, 1200).unwrap(),
            Size::new(1280, 720).unwrap(),
        );
        assert_eq!(g.map(fit, 640, 360), (580, 400, 3200, 1080));
        assert_eq!(g.map(fit, 0, 0), (180, 100, 3200, 1080));
    }

    #[test]
    fn last_pixel_stays_inside_scaled_window_and_reused_address_is_rejected() {
        let clients = json!([{"address":"0x1","stableId":"new","at":[100,100],"size":[800,600],"focusHistoryID":0}]);
        let monitors = json!([{"x":0,"y":0,"width":1920,"height":1080,"scale":1}]);
        assert!(WindowGeometry::from_json(&clients, &monitors, "0x1", Some("old")).is_none());
        let g = WindowGeometry::from_json(&clients, &monitors, "0x1", Some("new")).unwrap();
        let fit = PresentationGeometry::new(
            Size::new(1600, 1200).unwrap(),
            Size::new(1600, 1200).unwrap(),
        );
        assert_eq!(g.map(fit, 1599, 1199), (899, 699, 1920, 1080));
    }

    #[test]
    fn closed_or_invalid_window_never_maps_to_desktop() {
        assert!(WindowGeometry::from_json(&json!([]), &json!([]), "0x1", None).is_none());
        let c = json!([{"address":"0x1","at":[0,0],"size":[0,100]}]);
        assert!(WindowGeometry::from_json(&c, &json!([]), "0x1", None).is_none());
    }
}
