//! Renders assistant replies as Markdown. Tables are drawn here with a plain
//! `egui::Grid` of labels: `egui_commonmark` 0.26 sizes table columns from the
//! header only, so longer body cells overlap the next column. Everything else
//! goes through `egui_commonmark`.

use eframe::egui::{self, text::LayoutJob, Align, FontSelection, Rect, RichText, Ui};
use egui_commonmark::{CommonMarkCache, CommonMarkViewer};
use pulldown_cmark::{Event, Options, Parser, Tag, TagEnd};

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Span {
    pub text: String,
    pub bold: bool,
    pub italic: bool,
    pub code: bool,
}

pub type Cell = Vec<Span>;

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Table {
    pub header: Vec<Cell>,
    pub rows: Vec<Vec<Cell>>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Block<'a> {
    Markdown(&'a str),
    Table(Table),
}

fn current_cell(t: &mut Table, in_head: bool) -> Option<&mut Cell> {
    if in_head {
        t.header.last_mut()
    } else {
        t.rows.last_mut().and_then(|r| r.last_mut())
    }
}

/// Splits a reply into Markdown runs and tables, keeping the order. A table
/// still being streamed is closed by the parser at the end of the input, so
/// partial tables render too.
pub fn split(text: &str) -> Vec<Block<'_>> {
    let mut blocks = Vec::new();
    let mut last = 0;
    let mut table: Option<(usize, Table)> = None;
    let (mut in_head, mut bold, mut italic) = (false, false, false);
    let options = Options::ENABLE_TABLES | Options::ENABLE_STRIKETHROUGH;
    for (event, range) in Parser::new_ext(text, options).into_offset_iter() {
        if let Event::Start(Tag::Table(_)) = event {
            table = Some((range.start, Table::default()));
            continue;
        }
        let Some((start, t)) = table.as_mut() else {
            continue;
        };
        let push = |t: &mut Table, text: &str, code: bool| {
            if let Some(cell) = current_cell(t, in_head) {
                cell.push(Span {
                    text: text.to_string(),
                    bold,
                    italic,
                    code,
                });
            }
        };
        match event {
            Event::End(TagEnd::Table) => {
                let start = *start;
                let (_, t) = table.take().expect("inside a table");
                if !text[last..start].trim().is_empty() {
                    blocks.push(Block::Markdown(&text[last..start]));
                }
                blocks.push(Block::Table(t));
                last = range.end;
            }
            Event::Start(Tag::TableHead) => in_head = true,
            Event::End(TagEnd::TableHead) => in_head = false,
            Event::Start(Tag::TableRow) => t.rows.push(Vec::new()),
            Event::Start(Tag::TableCell) => {
                if in_head {
                    t.header.push(Vec::new());
                } else if let Some(row) = t.rows.last_mut() {
                    row.push(Vec::new());
                }
            }
            Event::Start(Tag::Strong) => bold = true,
            Event::End(TagEnd::Strong) => bold = false,
            Event::Start(Tag::Emphasis) => italic = true,
            Event::End(TagEnd::Emphasis) => italic = false,
            Event::Text(s) => push(t, &s, false),
            Event::Code(s) => push(t, &s, true),
            Event::SoftBreak | Event::HardBreak => push(t, " ", false),
            _ => {}
        }
    }
    if !text[last..].trim().is_empty() {
        blocks.push(Block::Markdown(&text[last..]));
    }
    blocks
}

pub fn render(ui: &mut Ui, id: egui::Id, text: &str, cache: &mut CommonMarkCache) {
    for (n, block) in split(text).into_iter().enumerate() {
        match block {
            Block::Markdown(md) => {
                CommonMarkViewer::new().show_with_id(id.with(n), ui, cache, md);
            }
            Block::Table(t) => {
                render_table(ui, id.with(n), &t);
            }
        }
    }
}

fn cell_job(ui: &Ui, cell: &Cell, header: bool) -> LayoutJob {
    let mut job = LayoutJob::default();
    for span in cell {
        let mut rt = RichText::new(&span.text);
        if span.bold || header {
            rt = rt.strong();
        }
        if span.italic {
            rt = rt.italics();
        }
        if span.code {
            rt = rt.code();
        }
        rt.append_to(&mut job, ui.style(), FontSelection::Default, Align::Center);
    }
    job
}

/// Returns each drawn cell's rect, row by row with the header first, so tests
/// can check the layout.
pub fn render_table(ui: &mut Ui, id: egui::Id, table: &Table) -> Vec<Vec<Rect>> {
    let columns = table
        .rows
        .iter()
        .map(Vec::len)
        .chain([table.header.len()])
        .max()
        .unwrap_or(0);
    let mut rects = Vec::new();
    ui.add_space(4.0);
    egui::Frame::group(ui.style()).show(ui, |ui| {
        egui::ScrollArea::horizontal()
            .id_salt(id.with("scroll"))
            .show(ui, |ui| {
                egui::Grid::new(id.with("grid"))
                    .striped(true)
                    .num_columns(columns)
                    .spacing([18.0, 6.0])
                    .show(ui, |ui| {
                        let empty = Cell::new();
                        let rows = std::iter::once((&table.header, true))
                            .chain(table.rows.iter().map(|r| (r, false)));
                        for (row, header) in rows {
                            let mut row_rects = Vec::new();
                            for c in 0..columns {
                                let cell = row.get(c).unwrap_or(&empty);
                                let job = cell_job(ui, cell, header);
                                row_rects.push(ui.add(egui::Label::new(job).extend()).rect);
                            }
                            rects.push(row_rects);
                            ui.end_row();
                        }
                    });
            });
    });
    ui.add_space(4.0);
    rects
}

#[cfg(test)]
mod tests {
    use super::*;

    const REPLY: &str = "Lo que necesitarías hacer:\n\n| Variante | Precio | Stock |\n|---|---|---|\n| Tenedor 3 puntas | 12,00 € | 100 |\n| Tenedor 4 puntas | **14,00 €** | 100 |\n\nRecuerda marcar \"Gestionar stock\".";

    fn text(cell: &Cell) -> String {
        cell.iter().map(|s| s.text.as_str()).collect()
    }

    #[test]
    fn replies_split_into_markdown_and_tables_in_order() {
        let blocks = split(REPLY);
        assert_eq!(blocks.len(), 3);
        assert!(matches!(blocks[0], Block::Markdown(m) if m.starts_with("Lo que")));
        let Block::Table(t) = &blocks[1] else {
            panic!("{blocks:?}");
        };
        assert_eq!(
            t.header.iter().map(text).collect::<Vec<_>>(),
            ["Variante", "Precio", "Stock"]
        );
        assert_eq!(text(&t.rows[0][0]), "Tenedor 3 puntas");
        assert!(t.rows[1][1][0].bold);
        assert!(matches!(blocks[2], Block::Markdown(m) if m.contains("Gestionar stock")));
    }

    #[test]
    fn a_table_cut_off_mid_stream_still_parses() {
        let partial = "| A | B |\n|---|---|\n| one | tw";
        let blocks = split(partial);
        let Block::Table(t) = &blocks[0] else {
            panic!("{blocks:?}");
        };
        assert_eq!(t.rows.len(), 1);
        assert_eq!(text(&t.rows[0][1]), "tw");
    }

    #[test]
    fn plain_text_is_a_single_markdown_block() {
        assert_eq!(split("## Hi\n\n- a"), vec![Block::Markdown("## Hi\n\n- a")]);
        assert!(split("").is_empty());
    }

    #[test]
    fn table_columns_fit_their_widest_cell() {
        let Block::Table(table) = split(REPLY).remove(1) else {
            panic!();
        };
        let ctx = egui::Context::default();
        let mut rects = Vec::new();
        // The grid settles column widths over the first frames.
        for _ in 0..3 {
            let mut out = ctx.run_ui(egui::RawInput::default(), |ui| {
                rects = render_table(ui, ui.scope_id().with("t"), &table);
            });
            out.textures_delta.clear();
        }
        assert_eq!(rects.len(), 3);
        for row in &rects {
            for pair in row.windows(2) {
                assert!(
                    pair[0].right() <= pair[1].left(),
                    "cell ends at {} but the next column starts at {}",
                    pair[0].right(),
                    pair[1].left()
                );
            }
        }
        let first_col_right = rects.iter().map(|r| r[0].right()).fold(0.0, f32::max);
        assert!(
            rects[0][1].left() >= first_col_right,
            "the header's second column starts after the widest first-column cell"
        );
    }
}
