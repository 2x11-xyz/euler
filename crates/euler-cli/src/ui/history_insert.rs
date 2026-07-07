use std::io::{self, Write};

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) struct HistoryInsertPlan {
    pub(crate) region_top_1_based: u16,
    pub(crate) region_bottom_1_based: u16,
    pub(crate) linefeed_count: u16,
    pub(crate) cursor_row_zero_based: u16,
    pub(crate) cursor_col_zero_based: u16,
}

pub(crate) fn plan_history_insert(
    screen_rows: u16,
    bottom_band_rows: u16,
    commit_rows: u16,
) -> Option<HistoryInsertPlan> {
    if commit_rows == 0 {
        return None;
    }
    let region_bottom = screen_rows.checked_sub(bottom_band_rows)?;
    if region_bottom <= commit_rows {
        return None;
    }
    Some(HistoryInsertPlan {
        region_top_1_based: 1,
        region_bottom_1_based: region_bottom,
        linefeed_count: commit_rows,
        cursor_row_zero_based: region_bottom.saturating_sub(1),
        cursor_col_zero_based: 0,
    })
}

pub(crate) fn emit_history_lines<T, W, F>(
    writer: &mut W,
    plan: HistoryInsertPlan,
    lines: &[T],
    mut write_line: F,
) -> io::Result<()>
where
    W: Write,
    F: FnMut(&mut W, &T) -> io::Result<()>,
{
    if usize::from(plan.linefeed_count) != lines.len() {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "history insert line count does not match plan",
        ));
    }
    write!(
        writer,
        "\x1b[{};{}r\x1b[{};{}H",
        plan.region_top_1_based,
        plan.region_bottom_1_based,
        plan.cursor_row_zero_based.saturating_add(1),
        plan.cursor_col_zero_based.saturating_add(1),
    )?;
    let mut line_result = Ok(());
    for line in lines {
        if let Err(error) = writer.write_all(b"\r\n") {
            line_result = Err(error);
            break;
        }
        if let Err(error) = write_line(writer, line) {
            line_result = Err(error);
            break;
        }
    }
    let reset_result = writer.write_all(b"\x1b[r");
    line_result.and(reset_result)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ui::test_backend::VT100Backend;

    #[test]
    fn plan_rejects_degenerate_or_empty_history_insert_geometry() {
        assert_eq!(plan_history_insert(34, 5, 0), None);
        assert_eq!(plan_history_insert(6, 6, 1), None);
        assert_eq!(plan_history_insert(4, 1, 3), None);
        assert_eq!(plan_history_insert(2, 0, 2), None);
    }

    #[test]
    fn plan_uses_scroll_region_above_bottom_band() {
        assert_eq!(
            plan_history_insert(34, 5, 2),
            Some(HistoryInsertPlan {
                region_top_1_based: 1,
                region_bottom_1_based: 29,
                linefeed_count: 2,
                cursor_row_zero_based: 28,
                cursor_col_zero_based: 0,
            })
        );
    }

    #[test]
    fn emit_history_lines_matches_codex_style_byte_contract() {
        let mut raw = Vec::new();
        let lines = ["alpha", "beta"];
        emit_history_lines(
            &mut raw,
            HistoryInsertPlan {
                region_top_1_based: 1,
                region_bottom_1_based: 9,
                linefeed_count: 2,
                cursor_row_zero_based: 8,
                cursor_col_zero_based: 0,
            },
            &lines,
            |writer, line| writer.write_all(line.as_bytes()),
        )
        .expect("emit history lines");

        assert_eq!(raw, b"\x1b[1;9r\x1b[9;1H\r\nalpha\r\nbeta\x1b[r");
        assert!(!raw.windows(3).any(|window| window == b"\x1b[S"));
        assert!(!raw.windows(3).any(|window| window == b"\x1b[J"));
        assert!(!raw.windows(4).any(|window| window == b"\x1b[3J"));
    }

    #[test]
    fn emit_history_lines_rejects_plan_line_count_mismatch() {
        let mut raw = Vec::new();
        let error = emit_history_lines(
            &mut raw,
            HistoryInsertPlan {
                region_top_1_based: 1,
                region_bottom_1_based: 9,
                linefeed_count: 2,
                cursor_row_zero_based: 8,
                cursor_col_zero_based: 0,
            },
            &["only-one"],
            |writer, line| writer.write_all(line.as_bytes()),
        )
        .expect_err("line count mismatch should fail");

        assert_eq!(error.kind(), io::ErrorKind::InvalidInput);
        assert!(raw.is_empty());
    }

    #[test]
    fn emit_history_lines_resets_scroll_region_after_line_write_failure() {
        let mut raw = Vec::new();
        let error = emit_history_lines(
            &mut raw,
            HistoryInsertPlan {
                region_top_1_based: 1,
                region_bottom_1_based: 9,
                linefeed_count: 2,
                cursor_row_zero_based: 8,
                cursor_col_zero_based: 0,
            },
            &["ok", "fail"],
            |writer, line| {
                if *line == "fail" {
                    return Err(io::Error::other("forced line write failure"));
                }
                writer.write_all(line.as_bytes())
            },
        )
        .expect_err("line write failure should propagate");

        assert_eq!(error.kind(), io::ErrorKind::Other);
        assert!(raw.ends_with(b"\x1b[r"));
        assert!(raw.windows("ok".len()).any(|window| window == b"ok"));
    }

    #[test]
    fn vt100_linefeed_insert_scrolls_history_region_without_touching_bottom_band() {
        let mut backend = VT100Backend::new(24, 6);
        write!(
            backend,
            "\x1b[1;1Hhistory-one\x1b[2;1Hhistory-two\x1b[3;1Hhistory-three\
             \x1b[4;1Hhistory-four\x1b[5;1H▌ prompt\x1b[6;1Hstatus"
        )
        .expect("seed vt100 screen");
        backend.clear_raw_output();

        let plan = plan_history_insert(6, 2, 2).expect("valid history insert plan");
        let lines = ["inserted-one", "inserted-two"];
        emit_history_lines(&mut backend, plan, &lines, |writer, line| {
            writer.write_all(line.as_bytes())
        })
        .expect("emit history lines");
        backend.flush().expect("flush vt100 backend");

        let raw = backend.raw_output();
        assert_eq!(
            raw,
            b"\x1b[1;4r\x1b[4;1H\r\ninserted-one\r\ninserted-two\x1b[r"
        );
        assert!(!raw.windows(3).any(|window| window == b"\x1b[S"));
        assert!(!raw.windows(3).any(|window| window == b"\x1b[J"));

        let rows = backend
            .screen_rows()
            .into_iter()
            .map(|row| row.trim_end().to_owned())
            .collect::<Vec<_>>();
        assert_eq!(rows[0], "history-three");
        assert_eq!(rows[1], "history-four");
        assert_eq!(rows[2], "inserted-one");
        assert_eq!(rows[3], "inserted-two");
        assert_eq!(rows[4], "▌ prompt");
        assert_eq!(rows[5], "status");
    }
}
