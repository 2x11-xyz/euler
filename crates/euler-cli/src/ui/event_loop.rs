use super::dirty::{DirtyRegions, RedrawLevel, Region};
use crossterm::event::{KeyCode, KeyEvent, KeyModifiers, MouseEvent};
use std::time::{Duration, Instant};

pub const TARGET_FRAME_INTERVAL: Duration = Duration::from_millis(16);

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum InputEvent {
    Key(KeyEvent),
    Mouse(MouseEvent),
    Paste(String),
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum TerminalSignal {
    Interrupt,
    Terminate,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum UiEvent {
    Input(InputEvent),
    RenderRequested(Region, RedrawLevel),
    Resize { width: u16, height: u16 },
    FocusChanged(bool),
    Signal(TerminalSignal),
    Tick,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum UiAction {
    InputBatch(Vec<InputEvent>),
    Render(DirtyRegions),
    Resize { width: u16, height: u16 },
    FocusChanged(bool),
    InterruptCurrentTurn,
    Shutdown,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum EnterKeyIntent {
    Submit,
    InsertNewline,
}

pub fn enter_key_intent(event: &KeyEvent) -> Option<EnterKeyIntent> {
    if event.code != KeyCode::Enter {
        return None;
    }

    // Crossterm can represent Shift+Enter when the terminal sends that
    // modifier. Some terminals report plain Enter instead; input wiring should
    // keep a documented fallback binding rather than guessing terminal quirks.
    if event
        .modifiers
        .intersects(KeyModifiers::SHIFT | KeyModifiers::ALT)
    {
        Some(EnterKeyIntent::InsertNewline)
    } else {
        Some(EnterKeyIntent::Submit)
    }
}

#[derive(Debug)]
pub struct EventLoop {
    frame_interval: Duration,
    next_frame_at: Instant,
    dirty: DirtyRegions,
    pending_input: Vec<InputEvent>,
    pending_resize: Option<(u16, u16)>,
    pending_focus: Option<bool>,
    pending_interrupt: bool,
    pending_shutdown: bool,
}

impl EventLoop {
    pub fn new(now: Instant) -> Self {
        Self::with_frame_interval(now, TARGET_FRAME_INTERVAL)
    }

    pub fn with_frame_interval(now: Instant, frame_interval: Duration) -> Self {
        Self {
            frame_interval,
            next_frame_at: now,
            dirty: DirtyRegions::new(),
            pending_input: Vec::new(),
            pending_resize: None,
            pending_focus: None,
            pending_interrupt: false,
            pending_shutdown: false,
        }
    }

    pub fn push(&mut self, event: UiEvent) {
        match event {
            UiEvent::Input(input) => {
                self.pending_input.push(input);
                self.dirty.mark(Region::Input, RedrawLevel::Partial);
            }
            UiEvent::RenderRequested(region, level) => self.dirty.mark(region, level),
            UiEvent::Resize { width, height } => {
                self.pending_resize = Some((width, height));
                self.dirty.mark_resize();
            }
            UiEvent::FocusChanged(focused) => {
                self.pending_focus = Some(focused);
            }
            UiEvent::Signal(TerminalSignal::Interrupt) => {
                self.pending_interrupt = true;
            }
            UiEvent::Signal(TerminalSignal::Terminate) => {
                self.pending_shutdown = true;
            }
            UiEvent::Tick => {}
        }
    }

    pub fn drain_ready(&mut self, now: Instant) -> Vec<UiAction> {
        let mut actions = Vec::new();

        if self.pending_shutdown {
            self.pending_shutdown = false;
            self.pending_interrupt = false;
            self.pending_input.clear();
            self.pending_resize = None;
            self.pending_focus = None;
            let _ = self.dirty.take();
            self.next_frame_at = now + self.frame_interval;
            actions.push(UiAction::Shutdown);
            return actions;
        }

        if self.pending_interrupt {
            self.pending_interrupt = false;
            actions.push(UiAction::InterruptCurrentTurn);
        }

        if let Some(focused) = self.pending_focus.take() {
            actions.push(UiAction::FocusChanged(focused));
        }

        if !self.pending_input.is_empty() {
            actions.push(UiAction::InputBatch(std::mem::take(
                &mut self.pending_input,
            )));
        }

        if let Some((width, height)) = self.pending_resize.take() {
            // The resize action replays history and repaints the full canvas
            // unconditionally, so a queued Render in the same batch would only
            // paint the same frame twice. Consume the dirty state and re-arm
            // the frame gate instead.
            let _ = self.dirty.take();
            self.next_frame_at = now + self.frame_interval;
            actions.push(UiAction::Resize { width, height });
        }

        if now >= self.next_frame_at && self.dirty.any_stale() {
            self.next_frame_at = now + self.frame_interval;
            actions.push(UiAction::Render(self.dirty.take()));
        }

        actions
    }

    pub fn poll_timeout(&self, now: Instant) -> Duration {
        if self.pending_shutdown
            || self.pending_interrupt
            || self.pending_focus.is_some()
            || !self.pending_input.is_empty()
            || self.pending_resize.is_some()
        {
            return Duration::ZERO;
        }

        if !self.dirty.any_stale() {
            return self.frame_interval;
        }

        self.next_frame_at.saturating_duration_since(now)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crossterm::event::{KeyCode, KeyModifiers};

    fn key(c: char) -> InputEvent {
        InputEvent::Key(KeyEvent::new(KeyCode::Char(c), KeyModifiers::NONE))
    }

    #[test]
    fn tick_loop_coalesces_inputs_and_render_requests() {
        let start = Instant::now();
        let mut loop_state = EventLoop::new(start);

        loop_state.push(UiEvent::Input(key('a')));
        loop_state.push(UiEvent::Input(key('b')));
        loop_state.push(UiEvent::RenderRequested(
            Region::Transcript,
            RedrawLevel::Partial,
        ));
        loop_state.push(UiEvent::RenderRequested(Region::Status, RedrawLevel::Full));

        let actions = loop_state.drain_ready(start);

        assert_eq!(actions.len(), 2);
        assert!(matches!(
            &actions[0],
            UiAction::InputBatch(inputs) if inputs.len() == 2
        ));
        assert!(matches!(
            &actions[1],
            UiAction::Render(dirty)
                if dirty.level(Region::Input) == RedrawLevel::Partial
                    && dirty.level(Region::Transcript) == RedrawLevel::Partial
                    && dirty.level(Region::Status) == RedrawLevel::Full
        ));
    }

    #[test]
    fn frame_limit_defers_render_until_next_frame() {
        let start = Instant::now();
        let mut loop_state = EventLoop::new(start);

        loop_state.push(UiEvent::RenderRequested(
            Region::Transcript,
            RedrawLevel::Partial,
        ));
        assert!(matches!(
            loop_state.drain_ready(start).as_slice(),
            [UiAction::Render(_)]
        ));

        loop_state.push(UiEvent::RenderRequested(
            Region::Input,
            RedrawLevel::Partial,
        ));
        assert!(loop_state
            .drain_ready(start + Duration::from_millis(1))
            .is_empty());

        assert!(matches!(
            loop_state
                .drain_ready(start + TARGET_FRAME_INTERVAL)
                .as_slice(),
            [UiAction::Render(_)]
        ));
    }

    #[test]
    fn resize_coalesces_to_one_action_and_owns_the_repaint() {
        let start = Instant::now();
        let mut loop_state = EventLoop::new(start);

        loop_state.push(UiEvent::Resize {
            width: 80,
            height: 24,
        });
        loop_state.push(UiEvent::Resize {
            width: 100,
            height: 30,
        });

        let actions = loop_state.drain_ready(start);

        // The resize handler replays and repaints unconditionally; emitting a
        // Render for the same batch would paint the same frame twice.
        assert_eq!(
            actions,
            vec![UiAction::Resize {
                width: 100,
                height: 30
            }]
        );
        assert!(loop_state
            .drain_ready(start + TARGET_FRAME_INTERVAL)
            .is_empty());
    }

    #[test]
    fn signals_map_to_explicit_loop_actions() {
        let start = Instant::now();
        let mut loop_state = EventLoop::new(start);

        loop_state.push(UiEvent::Signal(TerminalSignal::Interrupt));
        loop_state.push(UiEvent::Signal(TerminalSignal::Terminate));

        assert_eq!(loop_state.drain_ready(start), vec![UiAction::Shutdown]);
    }

    #[test]
    fn shutdown_suppresses_pending_work_in_same_drain() {
        let start = Instant::now();
        let mut loop_state = EventLoop::new(start);

        loop_state.push(UiEvent::Input(key('a')));
        loop_state.push(UiEvent::Resize {
            width: 100,
            height: 30,
        });
        loop_state.push(UiEvent::RenderRequested(
            Region::Activity,
            RedrawLevel::Full,
        ));
        loop_state.push(UiEvent::Signal(TerminalSignal::Interrupt));
        loop_state.push(UiEvent::Signal(TerminalSignal::Terminate));

        assert_eq!(loop_state.drain_ready(start), vec![UiAction::Shutdown]);
        assert!(loop_state
            .drain_ready(start + TARGET_FRAME_INTERVAL)
            .is_empty());
    }

    #[test]
    fn no_dirty_overdue_frame_does_not_busy_loop() {
        let start = Instant::now();
        let loop_state = EventLoop::new(start);

        assert_eq!(
            loop_state.poll_timeout(start + TARGET_FRAME_INTERVAL),
            TARGET_FRAME_INTERVAL
        );
    }

    #[test]
    fn queued_non_render_work_polls_immediately_even_when_clean() {
        let start = Instant::now();
        let mut loop_state = EventLoop::new(start);

        loop_state.push(UiEvent::Signal(TerminalSignal::Interrupt));

        assert_eq!(
            loop_state.poll_timeout(start + TARGET_FRAME_INTERVAL),
            Duration::ZERO
        );
    }

    #[test]
    fn enter_key_intent_distinguishes_submit_and_newline_when_reported() {
        let enter = KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE);
        let shifted_enter = KeyEvent::new(KeyCode::Enter, KeyModifiers::SHIFT);
        let alt_enter = KeyEvent::new(KeyCode::Enter, KeyModifiers::ALT);
        let shifted_control_enter =
            KeyEvent::new(KeyCode::Enter, KeyModifiers::SHIFT | KeyModifiers::CONTROL);
        let tab = KeyEvent::new(KeyCode::Tab, KeyModifiers::SHIFT);

        assert_eq!(enter_key_intent(&enter), Some(EnterKeyIntent::Submit));
        assert_eq!(
            enter_key_intent(&shifted_enter),
            Some(EnterKeyIntent::InsertNewline)
        );
        assert_eq!(
            enter_key_intent(&alt_enter),
            Some(EnterKeyIntent::InsertNewline)
        );
        assert_eq!(
            enter_key_intent(&shifted_control_enter),
            Some(EnterKeyIntent::InsertNewline)
        );
        assert_eq!(enter_key_intent(&tab), None);
    }

    #[test]
    fn dirty_overdue_frame_polls_immediately() {
        let start = Instant::now();
        let mut loop_state = EventLoop::new(start);

        loop_state.push(UiEvent::RenderRequested(
            Region::Transcript,
            RedrawLevel::Partial,
        ));

        assert_eq!(
            loop_state.poll_timeout(start + TARGET_FRAME_INTERVAL),
            Duration::ZERO
        );
    }
}
