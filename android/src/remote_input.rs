//! Remote image gesture recognition and physical pointer forwarding.
use eframe::egui::{self, Event, Pos2, Rect, Response, TouchId, TouchPhase};
use ironrdp_pdu::input::fast_path::FastPathInputEvent;
use ironrdp_pdu::input::mouse::{MousePdu, PointerFlags};
use windowlink_shared::{DesktopSize, NormalizedPoint, Viewport};

const TAP_INTERVAL: f64 = 0.250;
const HOLD_INTERVAL: f64 = 0.400;
const TAP_RADIUS: f32 = 8.0;
const MOVE_SLOP: f32 = 4.0;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum InputMode {
    #[default]
    Touch,
    Scroll,
    Pan,
    Trackpad,
}

#[derive(Debug)]
struct Contact {
    id: TouchId,
    start: Pos2,
    previous: Pos2,
    target: [u16; 2],
    down_at: f64,
    second: bool,
    moved: bool,
    committed: bool,
}
#[derive(Debug)]
struct PendingTap {
    position: Pos2,
    target: [u16; 2],
    deadline: f64,
}
#[derive(Debug, PartialEq)]
struct Geometry {
    rect: Rect,
    desktop: DesktopSize,
    zoom: f64,
    center: NormalizedPoint,
    fit: bool,
}

#[derive(Debug, Default)]
pub struct RemoteInput {
    blocked_rect: Option<Rect>,
    mode: InputMode,
    previous_mode: InputMode,
    contact: Option<Contact>,
    pending: Option<PendingTap>,
    active_touches: Vec<TouchId>,
    suppress_until_touch_end: bool,
    right_click_armed: bool,
    buttons: [bool; 3],
    explicit_drag: bool,
    last_remote_point: Option<[u16; 2]>,
    cursor: Option<[u16; 2]>,
    geometry: Option<Geometry>,
    generation: u64,
    previous_generation: u64,
}

impl RemoteInput {
    pub fn new(mode: InputMode) -> Self {
        Self {
            mode,
            previous_mode: mode,
            ..Self::default()
        }
    }
    pub fn set_blocked_rect(&mut self, rect: Option<Rect>) {
        self.blocked_rect = rect;
    }
    pub fn mode(&self) -> InputMode {
        self.mode
    }
    pub fn set_mode(&mut self, mode: InputMode) {
        self.mode = mode;
    }
    /// Update for target/frame geometry replacement; handle cancels using the old point.
    pub fn set_generation(&mut self, generation: u64) {
        self.generation = generation;
    }
    pub fn cursor_position(&self) -> Option<[u16; 2]> {
        self.cursor
    }
    /// Reconcile with a current host cursor position; callers reject stale generations.
    pub fn set_cursor_position(&mut self, point: [u16; 2]) {
        self.cursor = Some(point);
    }
    pub fn drag_held(&self) -> bool {
        self.explicit_drag
    }
    /// Pan the crop to keep the stored Trackpad cursor visible without warping it.
    pub fn keep_cursor_visible(
        &self,
        viewport: &mut Viewport,
        desktop: DesktopSize,
        rect: Rect,
        fit: bool,
    ) {
        let Some(point) = self.cursor else {
            return;
        };
        if self.mode != InputMode::Trackpad || desktop.width == 0 || desktop.height == 0 {
            return;
        }
        let (min, max) = if fit {
            (
                viewport.to_source(NormalizedPoint::new(0.0, 0.0)),
                viewport.to_source(NormalizedPoint::new(1.0, 1.0)),
            )
        } else {
            viewport.aspect_bounds(desktop, rect.width() as f64 / rect.height().max(1.0) as f64)
        };
        let source = NormalizedPoint::new(
            point[0] as f64 / desktop.width.saturating_sub(1).max(1) as f64,
            point[1] as f64 / desktop.height.saturating_sub(1).max(1) as f64,
        );
        let span = NormalizedPoint::new(max.x - min.x, max.y - min.y);
        let margin = NormalizedPoint::new(span.x * 0.08, span.y * 0.08);
        let shift = NormalizedPoint::new(
            source.x - source.x.clamp(min.x + margin.x, max.x - margin.x),
            source.y - source.y.clamp(min.y + margin.y, max.y - margin.y),
        );
        if shift.x != 0.0 || shift.y != 0.0 {
            pan_viewport(
                viewport,
                NormalizedPoint::new(-shift.x / span.x, -shift.y / span.y),
                desktop,
                rect,
                fit,
            );
        }
    }

    pub fn right_click_armed(&self) -> bool {
        self.right_click_armed
    }
    pub fn arm_right_click(&mut self) {
        self.pending = None;
        if let Some(contact) = self.contact.as_mut() {
            contact.committed = true;
        }
        self.right_click_armed = true;
    }
    /// Explicit Trackpad dragging survives finger lifts and repositioning.
    pub fn start_drag(&mut self) -> Vec<FastPathInputEvent> {
        let mut out = self.release_all();
        self.previous_mode = self.mode;
        if self.mode == InputMode::Trackpad {
            if let Some(point) = self.cursor {
                self.emit_move(&mut out, point);
                self.button(&mut out, point, 0, true);
                self.explicit_drag = true;
            }
        }
        out
    }
    pub fn release_drag(&mut self) -> Vec<FastPathInputEvent> {
        self.release_all()
    }
    /// Hold progress and screen contact position for the discoverable feedback ring.
    pub fn hold_progress(&self, now: f64) -> Option<(Pos2, f32)> {
        self.contact
            .as_ref()
            .filter(|c| c.second && !c.moved && !c.committed)
            .map(|c| {
                (
                    c.start,
                    ((now - c.down_at) / HOLD_INTERVAL).clamp(0.0, 1.0) as f32,
                )
            })
    }
    pub fn release_all(&mut self) -> Vec<FastPathInputEvent> {
        let mut out = Vec::new();
        self.cancel(&mut out, true);
        out
    }
    /// Keep contact bookkeeping alive while the image is not being drawn.
    /// Otherwise a lift in Overview leaves a phantom contact on re-entry.
    pub fn observe_inactive(&mut self, ctx: &egui::Context) -> Vec<FastPathInputEvent> {
        let mut out = Vec::new();
        self.cancel(&mut out, true);
        ctx.input(|input| {
            if !input.focused {
                // Android may consume the cancellation during a lifecycle change.
                self.active_touches.clear();
            } else {
                for event in &input.raw.events {
                    if let Event::Touch { id, phase, .. } = event {
                        match phase {
                            TouchPhase::Start if !self.active_touches.contains(id) => {
                                self.active_touches.push(*id);
                            }
                            TouchPhase::End | TouchPhase::Cancel => {
                                self.active_touches.retain(|active| active != id);
                            }
                            _ => {}
                        }
                    }
                }
            }
        });
        self.suppress_until_touch_end = !self.active_touches.is_empty();
        out
    }
    fn cancel(&mut self, out: &mut Vec<FastPathInputEvent>, clear_armed: bool) {
        if let Some(point) = self.last_remote_point.or(self.cursor) {
            for index in 0..3 {
                if self.buttons[index] {
                    self.button(out, point, index, false);
                }
            }
        }
        self.buttons = [false; 3];
        self.explicit_drag = false;
        self.contact = None;
        self.pending = None;
        self.suppress_until_touch_end = !self.active_touches.is_empty();
        if clear_armed {
            self.right_click_armed = false;
        }
    }

    pub fn handle(
        &mut self,
        ctx: &egui::Context,
        response: &Response,
        image_rect: Rect,
        desktop: DesktopSize,
        viewport: &mut Viewport,
        fit_screen: bool,
    ) -> Vec<FastPathInputEvent> {
        let mut out = Vec::new();
        let (events, now, zoom, focused) =
            ctx.input(|i| (i.raw.events.clone(), i.time, i.zoom_delta(), i.focused));
        let geometry = Geometry {
            rect: image_rect,
            desktop,
            zoom: viewport.zoom(),
            center: viewport.center(),
            fit: fit_screen,
        };
        if self.mode != self.previous_mode
            || self.generation != self.previous_generation
            || self.geometry.as_ref().is_some_and(|old| *old != geometry)
            || !focused
        {
            self.cancel(&mut out, true);
        }
        self.previous_mode = self.mode;
        self.previous_generation = self.generation;
        if self.cursor.is_none() {
            self.cursor = Some([
                (desktop.width.saturating_sub(1) / 2).min(u16::MAX as u32) as u16,
                (desktop.height.saturating_sub(1) / 2).min(u16::MAX as u32) as u16,
            ]);
        }
        let has_touch = events.iter().any(|e| matches!(e, Event::Touch { .. }));
        let touch_input = has_touch || !self.active_touches.is_empty();
        // Detect pinch/cancel before any gesture or due timer can commit this frame.
        let mut touches = self.active_touches.clone();
        let mut multitouch = touches.len() > 1;
        let mut cancelled = false;
        for event in &events {
            if let Event::Touch { id, phase, .. } = event {
                match phase {
                    TouchPhase::Start => {
                        if !touches.contains(id) {
                            touches.push(*id);
                        }
                    }
                    TouchPhase::End | TouchPhase::Cancel => {
                        cancelled |= *phase == TouchPhase::Cancel;
                        touches.retain(|t| t != id);
                    }
                    TouchPhase::Move => {}
                }
                multitouch |= touches.len() > 1;
            }
        }
        if multitouch || cancelled || !focused {
            // Armed accessibility action survives pinch, but not explicit cancel/focus loss.
            self.cancel(&mut out, cancelled || !focused);
            self.suppress_until_touch_end = true;
        }
        if (zoom - 1.0).abs() > 0.001 && (multitouch || (!touch_input && response.hovered())) {
            self.cancel(&mut out, false);
            if touch_input {
                self.suppress_until_touch_end = true;
            }
            let anchor = ctx
                .input(|i| i.pointer.interact_pos())
                .filter(|p| image_rect.contains(*p))
                .unwrap_or(image_rect.center());
            zoom_viewport(
                viewport,
                (zoom as f64).clamp(0.5, 2.0),
                normalized(anchor, image_rect),
                desktop,
                image_rect,
                fit_screen,
            );
        }
        for event in &events {
            match event {
                Event::Touch { id, phase, pos, .. } => {
                    if *phase == TouchPhase::Start && !self.active_touches.contains(id) {
                        self.active_touches.push(*id);
                    }
                    if !self.suppress_until_touch_end && focused {
                        match phase {
                            TouchPhase::Start
                                if image_rect.contains(*pos)
                                    && !self.blocked_rect.is_some_and(|r| r.contains(*pos)) =>
                            {
                                if let Some(target) =
                                    map_position(*pos, image_rect, desktop, viewport, fit_screen)
                                {
                                    self.begin(*id, *pos, target, now, &mut out);
                                }
                            }
                            TouchPhase::Move | TouchPhase::End => {
                                if self.contact.as_ref().is_some_and(|c| c.id == *id) {
                                    self.motion(
                                        *pos, image_rect, desktop, viewport, fit_screen, &mut out,
                                    );
                                    if *phase == TouchPhase::End {
                                        self.end(now, &mut out);
                                    }
                                }
                            }
                            _ => {}
                        }
                    }
                    if matches!(phase, TouchPhase::End | TouchPhase::Cancel) {
                        self.active_touches.retain(|t| t != id);
                    }
                }
                // Touch backends also synthesize pointer events: ignore those duplicates.
                Event::PointerMoved(pos) if !touch_input && focused => {
                    if image_rect.contains(*pos) || self.buttons.iter().any(|held| *held) {
                        if let Some(point) = map_position(
                            clamp_position(*pos, image_rect),
                            image_rect,
                            desktop,
                            viewport,
                            fit_screen,
                        ) {
                            if self.mode != InputMode::Pan {
                                self.emit_move(&mut out, point);
                            }
                        }
                    }
                }
                Event::PointerButton {
                    pos,
                    button,
                    pressed,
                    ..
                } if !touch_input && focused => {
                    let index = match button {
                        egui::PointerButton::Primary => Some(0),
                        egui::PointerButton::Secondary => Some(1),
                        egui::PointerButton::Middle => Some(2),
                        _ => None,
                    };
                    if let Some(index) = index {
                        if (image_rect.contains(*pos)
                            && !self.blocked_rect.is_some_and(|r| r.contains(*pos))
                            && self.mode != InputMode::Pan)
                            || (!pressed && self.buttons[index])
                        {
                            if let Some(point) = map_position(
                                clamp_position(*pos, image_rect),
                                image_rect,
                                desktop,
                                viewport,
                                fit_screen,
                            ) {
                                self.emit_move(&mut out, point);
                                self.button(&mut out, point, index, *pressed);
                            }
                        }
                    }
                }
                Event::PointerGone if !touch_input => {
                    self.cancel(&mut out, true);
                }
                Event::MouseWheel { delta, .. }
                    if self.mode != InputMode::Pan && !self.suppress_until_touch_end =>
                {
                    if let Some(pos) = ctx
                        .input(|i| i.pointer.interact_pos())
                        .filter(|p| image_rect.contains(*p))
                    {
                        if let Some(point) =
                            map_position(pos, image_rect, desktop, viewport, fit_screen)
                        {
                            self.emit_move(&mut out, point);
                            scroll_events(&mut out, point, *delta);
                        }
                    }
                }
                _ => {}
            }
        }
        if self.active_touches.is_empty() {
            self.suppress_until_touch_end = false;
        }
        if !self.suppress_until_touch_end && focused {
            self.timers(now, &mut out);
        }
        if let Some(pending) = &self.pending {
            ctx.request_repaint_after(std::time::Duration::from_secs_f64(
                (pending.deadline - now).max(0.0),
            ));
        }
        if let Some((_, progress)) = self.hold_progress(now) {
            ctx.request_repaint_after(std::time::Duration::from_millis(if progress < 1.0 {
                16
            } else {
                0
            }));
        }
        self.geometry = Some(Geometry {
            rect: image_rect,
            desktop,
            zoom: viewport.zoom(),
            center: viewport.center(),
            fit: fit_screen,
        });
        out
    }
    fn begin(
        &mut self,
        id: TouchId,
        pos: Pos2,
        target: [u16; 2],
        now: f64,
        out: &mut Vec<FastPathInputEvent>,
    ) {
        let target = if self.mode == InputMode::Trackpad {
            self.cursor.unwrap_or(target)
        } else {
            target
        };
        let candidate = self.pending.take();
        let second = candidate
            .as_ref()
            .is_some_and(|p| now <= p.deadline && pos.distance(p.position) <= TAP_RADIUS);
        if let Some(previous) = candidate.filter(|_| !second) {
            self.click(out, previous.target, false);
        }
        self.contact = Some(Contact {
            id,
            start: pos,
            previous: pos,
            target,
            down_at: now,
            second,
            moved: false,
            committed: self.explicit_drag,
        });
    }
    fn motion(
        &mut self,
        pos: Pos2,
        rect: Rect,
        desktop: DesktopSize,
        viewport: &mut Viewport,
        fit: bool,
        out: &mut Vec<FastPathInputEvent>,
    ) {
        let Some(mut c) = self.contact.take() else {
            return;
        };
        let crossed = c.start.distance(pos) >= MOVE_SLOP;
        let moving = c.moved || crossed;
        if moving && (!c.committed || self.explicit_drag) {
            c.moved = true;
            c.second = false;
            let previous = if c.previous == c.start {
                c.start
            } else {
                c.previous
            };
            let delta = pos - previous;
            match self.mode {
                InputMode::Touch if !self.right_click_armed => {
                    if let Some(point) =
                        map_position(clamp_position(pos, rect), rect, desktop, viewport, fit)
                    {
                        if !self.buttons[0] {
                            self.emit_move(out, c.target);
                            self.button(out, c.target, 0, true);
                        }
                        self.emit_move(out, point);
                    }
                }
                InputMode::Trackpad if !self.right_click_armed => {
                    let (a, b) = if fit {
                        (
                            viewport.to_source(NormalizedPoint::new(0.0, 0.0)),
                            viewport.to_source(NormalizedPoint::new(1.0, 1.0)),
                        )
                    } else {
                        viewport.aspect_bounds(
                            desktop,
                            rect.width() as f64 / rect.height().max(1.0) as f64,
                        )
                    };
                    let point = self.cursor.unwrap_or(c.target);
                    let x = point[0] as f64
                        + delta.x as f64 / rect.width().max(1.0) as f64
                            * (b.x - a.x)
                            * desktop.width.saturating_sub(1) as f64;
                    let y = point[1] as f64
                        + delta.y as f64 / rect.height().max(1.0) as f64
                            * (b.y - a.y)
                            * desktop.height.saturating_sub(1) as f64;
                    self.emit_move(
                        out,
                        [
                            x.round().clamp(
                                0.0,
                                desktop.width.saturating_sub(1).min(u16::MAX as u32) as f64,
                            ) as u16,
                            y.round().clamp(
                                0.0,
                                desktop.height.saturating_sub(1).min(u16::MAX as u32) as f64,
                            ) as u16,
                        ],
                    );
                }
                InputMode::Pan => pan_viewport(
                    viewport,
                    NormalizedPoint::new(
                        delta.x as f64 / rect.width().max(1.0) as f64,
                        delta.y as f64 / rect.height().max(1.0) as f64,
                    ),
                    desktop,
                    rect,
                    fit,
                ),
                InputMode::Scroll => {
                    if let Some(point) =
                        map_position(clamp_position(pos, rect), rect, desktop, viewport, fit)
                    {
                        self.emit_move(out, point);
                        scroll_events(out, point, delta);
                    }
                }
                _ => {}
            }
        }
        // Accumulate sub-slop motion from the original contact until threshold crossed.
        if moving {
            c.previous = pos;
        }
        self.contact = Some(c);
        if self.mode == InputMode::Trackpad {
            self.keep_cursor_visible(viewport, desktop, rect, fit);
        }
    }
    fn end(&mut self, now: f64, out: &mut Vec<FastPathInputEvent>) {
        let Some(c) = self.contact.take() else {
            return;
        };
        if c.moved {
            if !self.explicit_drag && self.buttons[0] {
                if let Some(point) = self.last_remote_point {
                    self.button(out, point, 0, false);
                }
            }
        } else if !c.committed && matches!(self.mode, InputMode::Touch | InputMode::Trackpad) {
            let target = if self.mode == InputMode::Trackpad {
                self.cursor.unwrap_or(c.target)
            } else {
                c.target
            };
            if self.right_click_armed {
                self.click(out, target, true);
                self.right_click_armed = false;
            } else if c.second {
                if now - c.down_at >= HOLD_INTERVAL {
                    self.click(out, target, true);
                } else {
                    self.click(out, target, false);
                    self.click(out, target, false);
                }
            } else {
                self.pending = Some(PendingTap {
                    position: c.start,
                    target,
                    deadline: now + TAP_INTERVAL,
                });
            }
        }
    }
    fn timers(&mut self, now: f64, out: &mut Vec<FastPathInputEvent>) {
        if self.contact.as_ref().is_some_and(|c| {
            c.second && !c.moved && !c.committed && now - c.down_at >= HOLD_INTERVAL
        }) {
            let target = self.contact.as_ref().unwrap().target;
            self.click(out, target, true);
            self.contact.as_mut().unwrap().committed = true;
        }
        if self.pending.as_ref().is_some_and(|p| now >= p.deadline) {
            let p = self.pending.take().unwrap();
            self.click(out, p.target, false);
        }
    }
    fn emit_move(&mut self, out: &mut Vec<FastPathInputEvent>, point: [u16; 2]) {
        if self.last_remote_point != Some(point) {
            out.push(mouse_event(point, PointerFlags::MOVE, 0));
        }
        self.last_remote_point = Some(point);
        self.cursor = Some(point);
    }
    fn button(
        &mut self,
        out: &mut Vec<FastPathInputEvent>,
        point: [u16; 2],
        index: usize,
        down: bool,
    ) {
        let button = [
            PointerFlags::LEFT_BUTTON,
            PointerFlags::RIGHT_BUTTON,
            PointerFlags::MIDDLE_BUTTON_OR_WHEEL,
        ][index];
        out.push(mouse_event(
            point,
            if down {
                button | PointerFlags::DOWN
            } else {
                button
            },
            0,
        ));
        self.buttons[index] = down;
        self.last_remote_point = Some(point);
        self.cursor = Some(point);
    }
    fn click(&mut self, out: &mut Vec<FastPathInputEvent>, point: [u16; 2], right: bool) {
        self.emit_move(out, point);
        let index = usize::from(right);
        self.button(out, point, index, true);
        self.button(out, point, index, false);
    }
}
fn clamp_position(pos: Pos2, rect: Rect) -> Pos2 {
    Pos2::new(
        pos.x.clamp(rect.left(), rect.right()),
        pos.y.clamp(rect.top(), rect.bottom()),
    )
}
fn scroll_events(out: &mut Vec<FastPathInputEvent>, point: [u16; 2], delta: egui::Vec2) {
    let vertical = (delta.y * 3.0).round() as i16;
    let horizontal = (-delta.x * 3.0).round() as i16;
    if vertical != 0 {
        out.push(wheel_event(point, vertical, false));
    }
    if horizontal != 0 {
        out.push(wheel_event(point, horizontal, true));
    }
}

/// Build an RDP wheel event. Rotation uses signed 9-bit units, range -256..=255.
pub fn wheel_event(point: [u16; 2], units: i16, horizontal: bool) -> FastPathInputEvent {
    let units = units.clamp(-256, 255);
    let axis = if horizontal {
        PointerFlags::HORIZONTAL_WHEEL
    } else {
        PointerFlags::VERTICAL_WHEEL
    };
    FastPathInputEvent::MouseEvent(MousePdu {
        flags: axis,
        number_of_wheel_rotation_units: units,
        x_position: point[0],
        y_position: point[1],
    })
}

fn mouse_event(point: [u16; 2], flags: PointerFlags, wheel: i16) -> FastPathInputEvent {
    FastPathInputEvent::MouseEvent(MousePdu {
        flags,
        number_of_wheel_rotation_units: wheel,
        x_position: point[0],
        y_position: point[1],
    })
}

fn normalized(pos: Pos2, rect: Rect) -> NormalizedPoint {
    NormalizedPoint::new(
        ((pos.x - rect.left()) / rect.width().max(1.0)) as f64,
        ((pos.y - rect.top()) / rect.height().max(1.0)) as f64,
    )
}

fn map_position(
    pos: Pos2,
    rect: Rect,
    desktop: DesktopSize,
    viewport: &Viewport,
    fit: bool,
) -> Option<[u16; 2]> {
    // Drop touches that land outside the streamed image (letterbox or pan
    // dead space) instead of clamping them onto the desktop edge — a tap in
    // the letterbox would otherwise teleport the remote cursor.
    if !rect.contains(pos) {
        return None;
    }
    let p = normalized(pos, rect);
    let mapped = if fit {
        viewport.map_touch(p, desktop)
    } else {
        viewport.map_touch_aspect(
            p,
            desktop,
            rect.width() as f64 / rect.height().max(1.0) as f64,
        )
    }?;
    Some([mapped.x as u16, mapped.y as u16])
}

fn pan_viewport(
    viewport: &mut Viewport,
    delta: NormalizedPoint,
    desktop: DesktopSize,
    rect: Rect,
    fit: bool,
) {
    if fit {
        viewport.pan(delta);
    } else {
        viewport.pan_aspect(
            delta,
            desktop,
            rect.width() as f64 / rect.height().max(1.0) as f64,
        );
    }
}

fn zoom_viewport(
    viewport: &mut Viewport,
    factor: f64,
    anchor: NormalizedPoint,
    desktop: DesktopSize,
    rect: Rect,
    fit: bool,
) {
    if fit {
        viewport.zoom_at(factor, anchor);
    } else {
        viewport.zoom_at_aspect(
            factor,
            anchor,
            desktop,
            rect.width() as f64 / rect.height().max(1.0) as f64,
        );
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use egui::{RawInput, Sense, Vec2};
    fn touch(id: u64, phase: TouchPhase, x: f32, y: f32) -> Event {
        Event::Touch {
            device_id: egui::TouchDeviceId(1),
            id: TouchId(id),
            phase,
            pos: Pos2::new(x, y),
            force: None,
        }
    }
    fn frame(
        ctx: &egui::Context,
        state: &mut RemoteInput,
        viewport: &mut Viewport,
        time: f64,
        events: Vec<Event>,
    ) -> Vec<FastPathInputEvent> {
        let mut result = Vec::new();
        let mut output = ctx.run_ui(
            RawInput {
                time: Some(time),
                events,
                screen_rect: Some(Rect::from_min_size(Pos2::ZERO, Vec2::splat(200.0))),
                ..Default::default()
            },
            |ui| {
                let rect = Rect::from_min_size(Pos2::ZERO, Vec2::splat(100.0));
                let response =
                    ui.interact(rect, egui::Id::new("remote-image"), Sense::click_and_drag());
                result = state.handle(
                    ui.ctx(),
                    &response,
                    rect,
                    DesktopSize::new(1000, 800),
                    viewport,
                    true,
                );
            },
        );
        output.textures_delta.clear();
        result
    }
    fn buttons(events: &[FastPathInputEvent]) -> Vec<PointerFlags> {
        events
            .iter()
            .filter_map(|e| match e {
                FastPathInputEvent::MouseEvent(mouse) if mouse.flags != PointerFlags::MOVE => {
                    Some(mouse.flags)
                }
                _ => None,
            })
            .collect()
    }
    fn setup() -> (egui::Context, RemoteInput, Viewport) {
        (
            egui::Context::default(),
            RemoteInput::default(),
            Viewport::new(),
        )
    }
    fn first_tap(ctx: &egui::Context, state: &mut RemoteInput, viewport: &mut Viewport) {
        assert!(
            buttons(&frame(
                ctx,
                state,
                viewport,
                0.0,
                vec![touch(1, TouchPhase::Start, 20.0, 20.0)]
            ))
            .is_empty()
        );
        assert!(
            buttons(&frame(
                ctx,
                state,
                viewport,
                0.05,
                vec![touch(1, TouchPhase::End, 20.0, 20.0)]
            ))
            .is_empty()
        );
    }
    #[test]
    fn single_tap_waits_for_intertap_deadline() {
        let (ctx, mut state, mut viewport) = setup();
        first_tap(&ctx, &mut state, &mut viewport);
        assert!(buttons(&frame(&ctx, &mut state, &mut viewport, 0.299, vec![])).is_empty());
        assert_eq!(
            buttons(&frame(&ctx, &mut state, &mut viewport, 0.301, vec![])),
            [
                PointerFlags::LEFT_BUTTON | PointerFlags::DOWN,
                PointerFlags::LEFT_BUTTON
            ]
        );
        assert!(buttons(&frame(&ctx, &mut state, &mut viewport, 1.0, vec![])).is_empty());
    }
    #[test]
    fn quick_second_tap_emits_two_left_pairs_at_second_origin() {
        let (ctx, mut state, mut viewport) = setup();
        first_tap(&ctx, &mut state, &mut viewport);
        frame(
            &ctx,
            &mut state,
            &mut viewport,
            0.1,
            vec![touch(2, TouchPhase::Start, 23.0, 20.0)],
        );
        let events = frame(
            &ctx,
            &mut state,
            &mut viewport,
            0.15,
            vec![touch(2, TouchPhase::End, 23.0, 20.0)],
        );
        assert_eq!(
            buttons(&events),
            [
                PointerFlags::LEFT_BUTTON | PointerFlags::DOWN,
                PointerFlags::LEFT_BUTTON,
                PointerFlags::LEFT_BUTTON | PointerFlags::DOWN,
                PointerFlags::LEFT_BUTTON
            ]
        );
        assert!(events.iter().all(
            |event| matches!(event, FastPathInputEvent::MouseEvent(m) if m.x_position == 230)
        ));
    }
    #[test]
    fn stationary_second_hold_fires_right_once_and_consumes_lift_and_motion() {
        let (ctx, mut state, mut viewport) = setup();
        first_tap(&ctx, &mut state, &mut viewport);
        frame(
            &ctx,
            &mut state,
            &mut viewport,
            0.1,
            vec![touch(2, TouchPhase::Start, 20.0, 20.0)],
        );
        assert!(state.hold_progress(0.3).is_some());
        let held = frame(&ctx, &mut state, &mut viewport, 0.501, vec![]);
        assert_eq!(
            buttons(&held),
            [
                PointerFlags::RIGHT_BUTTON | PointerFlags::DOWN,
                PointerFlags::RIGHT_BUTTON
            ]
        );
        assert!(
            frame(
                &ctx,
                &mut state,
                &mut viewport,
                0.6,
                vec![touch(2, TouchPhase::Move, 40.0, 20.0)]
            )
            .is_empty()
        );
        assert!(
            frame(
                &ctx,
                &mut state,
                &mut viewport,
                0.7,
                vec![touch(2, TouchPhase::End, 40.0, 20.0)]
            )
            .is_empty()
        );
    }
    #[test]
    fn release_after_hold_deadline_commits_right_without_timer_frame() {
        let (ctx, mut state, mut viewport) = setup();
        first_tap(&ctx, &mut state, &mut viewport);
        frame(
            &ctx,
            &mut state,
            &mut viewport,
            0.1,
            vec![touch(2, TouchPhase::Start, 20.0, 20.0)],
        );
        assert_eq!(
            buttons(&frame(
                &ctx,
                &mut state,
                &mut viewport,
                0.6,
                vec![touch(2, TouchPhase::End, 20.0, 20.0)]
            )),
            [
                PointerFlags::RIGHT_BUTTON | PointerFlags::DOWN,
                PointerFlags::RIGHT_BUTTON
            ]
        );
    }
    #[test]
    fn movement_on_deadline_wins_and_drag_releases_beyond_image() {
        let (ctx, mut state, mut viewport) = setup();
        first_tap(&ctx, &mut state, &mut viewport);
        frame(
            &ctx,
            &mut state,
            &mut viewport,
            0.1,
            vec![touch(2, TouchPhase::Start, 20.0, 20.0)],
        );
        let moved = frame(
            &ctx,
            &mut state,
            &mut viewport,
            0.5,
            vec![touch(2, TouchPhase::Move, 120.0, 20.0)],
        );
        assert_eq!(
            buttons(&moved),
            [PointerFlags::LEFT_BUTTON | PointerFlags::DOWN]
        );
        let mice: Vec<_> = moved
            .iter()
            .filter_map(|e| match e {
                FastPathInputEvent::MouseEvent(m) => Some(m),
                _ => None,
            })
            .collect();
        assert_eq!(mice[0].x_position, 200);
        assert_eq!(mice.last().unwrap().x_position, 999);
        assert_eq!(
            buttons(&frame(
                &ctx,
                &mut state,
                &mut viewport,
                0.6,
                vec![touch(2, TouchPhase::End, 140.0, 20.0)]
            )),
            [PointerFlags::LEFT_BUTTON]
        );
        assert!(state.release_all().is_empty());
    }
    #[test]
    fn pinch_on_hold_deadline_cancels_before_right_click() {
        let (ctx, mut state, mut viewport) = setup();
        first_tap(&ctx, &mut state, &mut viewport);
        frame(
            &ctx,
            &mut state,
            &mut viewport,
            0.1,
            vec![touch(2, TouchPhase::Start, 20.0, 20.0)],
        );
        assert!(
            frame(
                &ctx,
                &mut state,
                &mut viewport,
                0.5,
                vec![touch(3, TouchPhase::Start, 200.0, 200.0)]
            )
            .is_empty()
        );
        assert!(
            frame(
                &ctx,
                &mut state,
                &mut viewport,
                0.6,
                vec![
                    touch(3, TouchPhase::End, 200.0, 200.0),
                    touch(2, TouchPhase::End, 20.0, 20.0)
                ]
            )
            .is_empty()
        );
        assert!(frame(&ctx, &mut state, &mut viewport, 1.0, vec![]).is_empty());
    }
    #[test]
    fn mode_generation_and_cancel_discard_buffered_clicks() {
        for cause in 0..3 {
            let (ctx, mut state, mut viewport) = setup();
            first_tap(&ctx, &mut state, &mut viewport);
            match cause {
                0 => state.set_mode(InputMode::Scroll),
                1 => state.set_generation(2),
                _ => {
                    state.release_all();
                }
            }
            assert!(frame(&ctx, &mut state, &mut viewport, 1.0, vec![]).is_empty());
        }
    }
    #[test]
    fn armed_fallback_survives_movement_and_consumes_only_stationary_tap() {
        let (ctx, mut state, mut viewport) = setup();
        state.arm_right_click();
        frame(
            &ctx,
            &mut state,
            &mut viewport,
            0.0,
            vec![touch(1, TouchPhase::Start, 20.0, 20.0)],
        );
        assert!(
            frame(
                &ctx,
                &mut state,
                &mut viewport,
                0.1,
                vec![
                    touch(1, TouchPhase::Move, 40.0, 20.0),
                    touch(1, TouchPhase::End, 40.0, 20.0)
                ]
            )
            .is_empty()
        );
        assert!(state.right_click_armed());
        assert_eq!(
            buttons(&frame(
                &ctx,
                &mut state,
                &mut viewport,
                0.2,
                vec![
                    touch(2, TouchPhase::Start, 20.0, 20.0),
                    touch(2, TouchPhase::End, 20.0, 20.0)
                ]
            )),
            [
                PointerFlags::RIGHT_BUTTON | PointerFlags::DOWN,
                PointerFlags::RIGHT_BUTTON
            ]
        );
    }
    #[test]
    fn trackpad_relative_strokes_and_explicit_drag_survive_finger_lifts() {
        let (ctx, _, mut viewport) = setup();
        let mut state = RemoteInput::new(InputMode::Trackpad);
        state.set_cursor_position([400, 300]);
        frame(
            &ctx,
            &mut state,
            &mut viewport,
            0.0,
            vec![touch(1, TouchPhase::Start, 80.0, 80.0)],
        );
        assert_eq!(state.cursor_position(), Some([400, 300]));
        frame(
            &ctx,
            &mut state,
            &mut viewport,
            0.1,
            vec![
                touch(1, TouchPhase::Move, 90.0, 80.0),
                touch(1, TouchPhase::End, 90.0, 80.0),
            ],
        );
        assert_eq!(state.cursor_position(), Some([500, 300]));
        assert_eq!(
            buttons(&state.start_drag()),
            [PointerFlags::LEFT_BUTTON | PointerFlags::DOWN]
        );
        frame(
            &ctx,
            &mut state,
            &mut viewport,
            0.2,
            vec![
                touch(2, TouchPhase::Start, 20.0, 20.0),
                touch(2, TouchPhase::End, 20.0, 20.0),
            ],
        );
        assert!(state.drag_held());
        frame(
            &ctx,
            &mut state,
            &mut viewport,
            0.3,
            vec![
                touch(3, TouchPhase::Start, 20.0, 20.0),
                touch(3, TouchPhase::Move, 30.0, 20.0),
                touch(3, TouchPhase::End, 30.0, 20.0),
            ],
        );
        assert!(state.drag_held());
        assert_eq!(state.cursor_position(), Some([600, 300]));
        assert_eq!(buttons(&state.release_drag()), [PointerFlags::LEFT_BUTTON]);
    }
    #[test]
    fn physical_secondary_and_middle_buttons_preserve_down_and_release() {
        let (ctx, mut state, mut viewport) = setup();
        let event = |button, pressed| Event::PointerButton {
            pos: Pos2::new(20.0, 20.0),
            button,
            pressed,
            modifiers: egui::Modifiers::default(),
        };
        let events = frame(
            &ctx,
            &mut state,
            &mut viewport,
            0.0,
            vec![
                event(egui::PointerButton::Secondary, true),
                event(egui::PointerButton::Middle, true),
            ],
        );
        assert_eq!(
            buttons(&events),
            [
                PointerFlags::RIGHT_BUTTON | PointerFlags::DOWN,
                PointerFlags::MIDDLE_BUTTON_OR_WHEEL | PointerFlags::DOWN
            ]
        );
        assert_eq!(
            buttons(&state.release_all()),
            [
                PointerFlags::RIGHT_BUTTON,
                PointerFlags::MIDDLE_BUTTON_OR_WHEEL
            ]
        );
    }
}
