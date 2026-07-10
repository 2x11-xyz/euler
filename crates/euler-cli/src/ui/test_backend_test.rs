use ratatui::{
    backend::{Backend, ClearType, CrosstermBackend, WindowSize},
    buffer::Cell,
    layout::{Position, Size},
};
use std::{
    cell::RefCell,
    io::{self, Write},
    rc::Rc,
};

const DEFAULT_SCROLLBACK_ROWS: usize = 1_000;

pub struct VT100Backend {
    parser: SharedParser,
    inner: CrosstermBackend<SharedParser>,
    size: Size,
    write_error: bool,
    raw_output: Vec<u8>,
}

impl VT100Backend {
    pub fn new(width: u16, height: u16) -> Self {
        let parser = SharedParser::new(height, width, DEFAULT_SCROLLBACK_ROWS);
        Self {
            inner: CrosstermBackend::new(parser.clone()),
            parser,
            size: Size::new(width, height),
            write_error: false,
            raw_output: Vec::new(),
        }
    }

    pub fn screen_contents(&self) -> String {
        self.parser.with_screen(|screen| screen.contents())
    }

    pub fn screen_rows(&self) -> Vec<String> {
        self.parser
            .with_screen(|screen| screen.rows(0, self.size.width).collect())
    }

    pub fn scrollback_rows(&self) -> Vec<String> {
        self.parser.scrollback_rows(self.size.width)
    }

    pub fn cursor_position(&self) -> Position {
        self.parser.with_screen(|screen| {
            let (row, col) = screen.cursor_position();
            Position::new(col, row)
        })
    }

    pub fn resize(&mut self, width: u16, height: u16) {
        self.size = Size::new(width, height);
        self.parser.resize(height, width);
    }

    pub fn set_write_error(&mut self, write_error: bool) {
        self.write_error = write_error;
    }

    pub fn clear_raw_output(&mut self) {
        self.raw_output.clear();
    }

    pub fn raw_output(&self) -> &[u8] {
        &self.raw_output
    }
}

#[derive(Clone)]
struct SharedParser(Rc<RefCell<vt100::Parser>>);

impl SharedParser {
    fn new(height: u16, width: u16, scrollback_rows: usize) -> Self {
        Self(Rc::new(RefCell::new(vt100::Parser::new(
            height,
            width,
            scrollback_rows,
        ))))
    }

    fn resize(&self, height: u16, width: u16) {
        self.0.borrow_mut().set_size(height, width);
    }

    fn with_screen<R>(&self, f: impl FnOnce(&vt100::Screen) -> R) -> R {
        f(self.0.borrow().screen())
    }

    fn scrollback_rows(&self, width: u16) -> Vec<String> {
        let mut parser = self.0.borrow_mut();
        let original = parser.screen().scrollback();
        parser.set_scrollback(usize::MAX);
        let max = parser.screen().scrollback();
        let (height, _) = parser.screen().size();
        let max = max.min(usize::from(height));
        let mut rows = Vec::new();
        for offset in (1..=max).rev() {
            parser.set_scrollback(offset);
            if let Some(row) = parser.screen().rows(0, width).next() {
                rows.push(row);
            }
        }
        parser.set_scrollback(0);
        rows.extend(parser.screen().rows(0, width));
        parser.set_scrollback(original);
        rows
    }
}

impl Write for SharedParser {
    fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
        self.0.borrow_mut().write(buf)
    }

    fn flush(&mut self) -> io::Result<()> {
        self.0.borrow_mut().flush()
    }
}

impl Backend for VT100Backend {
    type Error = io::Error;

    fn draw<'a, I>(&mut self, content: I) -> io::Result<()>
    where
        I: Iterator<Item = (u16, u16, &'a Cell)>,
    {
        self.inner.draw(content)
    }

    fn hide_cursor(&mut self) -> io::Result<()> {
        self.inner.hide_cursor()
    }

    fn show_cursor(&mut self) -> io::Result<()> {
        self.inner.show_cursor()
    }

    fn get_cursor_position(&mut self) -> io::Result<Position> {
        Ok(self.cursor_position())
    }

    fn set_cursor_position<P: Into<Position>>(&mut self, position: P) -> io::Result<()> {
        self.inner.set_cursor_position(position)
    }

    fn clear(&mut self) -> io::Result<()> {
        self.inner.clear()
    }

    fn clear_region(&mut self, clear_type: ClearType) -> io::Result<()> {
        self.inner.clear_region(clear_type)
    }

    fn append_lines(&mut self, n: u16) -> io::Result<()> {
        self.inner.append_lines(n)
    }

    fn size(&self) -> io::Result<Size> {
        Ok(self.size)
    }

    fn window_size(&mut self) -> io::Result<WindowSize> {
        Ok(WindowSize {
            columns_rows: self.size,
            pixels: Size::new(0, 0),
        })
    }

    fn flush(&mut self) -> io::Result<()> {
        if self.write_error {
            return Err(io::Error::other("forced VT100 backend write failure"));
        }
        Backend::flush(&mut self.inner)
    }
}

impl Write for VT100Backend {
    fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
        if self.write_error {
            return Err(io::Error::other("forced VT100 backend write failure"));
        }
        self.raw_output.extend_from_slice(buf);
        self.inner.write(buf)
    }

    fn flush(&mut self) -> io::Result<()> {
        if self.write_error {
            return Err(io::Error::other("forced VT100 backend write failure"));
        }
        Write::flush(&mut self.inner)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use ratatui::{
        layout::Rect,
        text::Line,
        widgets::{Block, Paragraph, Widget},
        Terminal, TerminalOptions, Viewport,
    };

    #[test]
    fn vt100_backend_renders_ratatui_output_into_screen_contents() {
        let mut backend = VT100Backend::new(10, 2);
        backend.resize(20, 4);
        let mut terminal = Terminal::new(backend).expect("terminal");

        terminal
            .draw(|frame| {
                let area = Rect::new(0, 0, 20, 4);
                frame.render_widget(
                    Paragraph::new(Line::from("Euler VT100")).block(Block::bordered()),
                    area,
                );
            })
            .expect("draw");

        let contents = terminal.backend().screen_contents();
        assert!(contents.contains("Euler VT100"));
    }

    #[test]
    fn vt100_inline_insert_before_places_history_above_viewport() {
        // This is the stable boundary for inline history ordering: it
        // exercises ratatui's Inline viewport and insert_before behavior
        // against a VT100 parser without depending on a host terminal
        // emulator's scrollback retention policy.
        let backend = VT100Backend::new(30, 6);
        let mut terminal = Terminal::with_options(
            backend,
            TerminalOptions {
                viewport: Viewport::Inline(2),
            },
        )
        .expect("inline terminal");

        terminal
            .draw(|frame| {
                frame.render_widget(Paragraph::new("live\nfooter"), frame.area());
            })
            .expect("draw live viewport");
        terminal
            .insert_before(1, |buf| {
                Paragraph::new("history row").render(buf.area, buf);
            })
            .expect("insert history");
        terminal
            .draw(|frame| {
                frame.render_widget(Paragraph::new("live\nfooter"), frame.area());
            })
            .expect("redraw live viewport");

        let contents = terminal.backend().screen_contents();
        assert!(contents.contains("history row"));
        assert!(contents.contains("footer"));
        let history_row = contents.find("history row").expect("history row");
        let footer = contents.find("footer").expect("footer");
        assert!(history_row < footer);
    }
}
