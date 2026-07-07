use ratatui::layout::Rect;
use ratatui::text::Line;

#[derive(Clone, Copy)]
pub(crate) struct AppAreas {
    pub transcript: Rect,
    pub ask: Rect,
    pub activity: Rect,
    pub bottom: Rect,
    pub notice: Rect,
    pub status: Rect,
}

pub(crate) fn layout(
    area: Rect,
    composer_height: u16,
    notice_height: u16,
    ask_height: u16,
    activity_height: u16,
) -> AppAreas {
    let mut remaining = area.height;
    let status_height = allocate(&mut remaining, 1);
    let bottom_height = allocate(&mut remaining, composer_height);
    let ask_height = allocate(&mut remaining, ask_height);
    let notice_height = allocate(&mut remaining, notice_height);
    let activity_height = allocate(&mut remaining, activity_height);

    let mut top = area.y + area.height;
    let status = take_from_bottom(area, &mut top, status_height);
    let notice = take_from_bottom(area, &mut top, notice_height);
    let bottom = take_from_bottom(area, &mut top, bottom_height);
    let activity = take_from_bottom(area, &mut top, activity_height);
    let ask = take_from_bottom(area, &mut top, ask_height);
    let transcript = Rect::new(area.x, area.y, area.width, remaining);
    AppAreas {
        transcript,
        ask,
        activity,
        bottom,
        notice,
        status,
    }
}

fn allocate(remaining: &mut u16, requested_height: u16) -> u16 {
    let height = requested_height.min(*remaining);
    *remaining = remaining.saturating_sub(height);
    height
}

fn take_from_bottom(area: Rect, top: &mut u16, requested_height: u16) -> Rect {
    let height = requested_height.min(top.saturating_sub(area.y));
    *top = top.saturating_sub(height);
    Rect::new(area.x, *top, area.width, height)
}

pub(crate) fn string_lines(lines: Vec<String>) -> Vec<Line<'static>> {
    lines.into_iter().map(Line::from).collect()
}
