use anyhow::{anyhow, Context, Result};
use comfy_table::{Cell, Table};
use regex::Regex;
use rusqlite::{Connection, Row};
use rusqlite::types::ValueRef;
use rustyline::{error::ReadlineError, DefaultEditor};
use std::fs::File;
use std::io::Write;
use crossterm::{
    event::{self, DisableMouseCapture, EnableMouseCapture, Event as CEvent, KeyCode, KeyEvent, KeyModifiers},
    execute,
    terminal::{disable_raw_mode, enable_raw_mode, EnterAlternateScreen, LeaveAlternateScreen},
};
use ratatui::{
    backend::CrosstermBackend,
    layout::{Constraint, Direction, Layout, Rect},
    style::{Color, Modifier, Style},
    widgets::{Block, Borders, List, ListItem, ListState, Paragraph, Row as RRow, Table as RTable},
    Terminal,
};
use ratatui::Frame;
use ratatui::widgets::Cell as TuiCell;
use std::time::{Duration, Instant};

struct DB {
    conn: Option<Connection>,
    table: Option<String>,
}

impl DB {
    fn new() -> Self { Self { conn: None, table: None } }

    fn open(&mut self, path: &str) -> Result<()> {
        // open (create if missing)
        self.conn = Some(Connection::open(path).context("open sqlite")?);
        self.conn.as_ref().unwrap().execute_batch("PRAGMA foreign_keys = ON;")?;
        Ok(())
    }

    fn ensure(&self) -> Result<&Connection> {
        self.conn.as_ref().ok_or_else(|| anyhow!("No DB open. Use .open <file.db>"))
    }

    fn list_tables(&self) -> Result<Vec<String>> {
        let c = self.ensure()?;
        let mut stmt = c.prepare("SELECT name FROM sqlite_master WHERE type='table' ORDER BY name")?;
        let iter = stmt.query_map([], |row| row.get::<_, String>(0))?;
        let mut out = Vec::new();
        for name in iter {
            out.push(name?);
        }
        Ok(out)
    }

    fn ddl(&self, table: &str) -> Result<Option<String>> {
        let c = self.ensure()?;
        let mut stmt = c.prepare("SELECT sql FROM sqlite_master WHERE type='table' AND name=?1")?;
        let mut rows = stmt.query([table])?;
        if let Some(row) = rows.next()? {
            let ddl: Option<String> = row.get(0)?;
            Ok(ddl)
        } else {
            Ok(None)
        }
    }

    fn foreign_keys(&self, table: &str) -> Result<Vec<(String, String, String, String)>> {
        // Returns vec of (from_col, ref_table, ref_col, on_update/delete info simplified)
        let c = self.ensure()?;
        let mut stmt = c.prepare(&format!("PRAGMA foreign_key_list(\"{}\")", table.replace('"', "\"\"")))?;
        let mut rows = stmt.query([])?;
        let mut out = Vec::new();
        while let Some(row) = rows.next()? {
            let from: String = row.get(3)?; // 'from' column
            let to_table: String = row.get(2)?; // 'table' referenced
            let to_col: String = row.get(4)?; // 'to' column
            // Optional read actions
            let on_update: Option<String> = row.get(5).ok();
            let on_delete: Option<String> = row.get(6).ok();
            let info = format!("upd:{}, del:{}", on_update.unwrap_or_default(), on_delete.unwrap_or_default());
            out.push((from, to_table, to_col, info));
        }
        Ok(out)
    }

    fn select(&self, sql: &str) -> Result<(Vec<String>, Vec<Vec<String>>)> {
        let c = self.ensure()?;
        let q = sql.trim();
        if !Regex::new(r#"(?i)^\s*select\b"#).unwrap().is_match(q) {
            return Err(anyhow!(".exportcsv expects a SELECT query"));
        }
        let mut stmt = c.prepare(q)?;
        let col_names = stmt.column_names().into_iter().map(|s| s.to_string()).collect::<Vec<_>>();
        let rows = stmt.query_map([], |row| Ok(extract_row(row)))?;
        let mut data = Vec::new();
        for r in rows { data.push(r?); }
        Ok((col_names, data))
    }

    fn preview(&self, table: &str, n: usize) -> Result<QueryResult> {
        self.ensure()?;
        let safe = table.replace('"', "\"\"");
        let sql = format!("SELECT * FROM \"{}\" LIMIT {}", safe, n);
        self.run(&sql)
    }

    fn run(&self, sql: &str) -> Result<QueryResult> {
        let c = self.ensure()?;
        let is_query = Regex::new(r#"(?i)^\s*(select|pragma)\b"#).unwrap().is_match(sql);
        if is_query {
            let mut stmt = c.prepare(sql)?;
            let col_names = stmt.column_names().into_iter().map(|s| s.to_string()).collect::<Vec<_>>();
            let rows = stmt.query_map([], |row| Ok(extract_row(row)))?;
            let mut data = Vec::new();
            for r in rows {
                data.push(r?);
            }
            Ok(QueryResult::Rows { cols: col_names, rows: data })
        } else {
            let affected = c.execute(sql, [])?;
            Ok(QueryResult::Ack { rowcount: affected })
        }
    }
}

fn extract_row(row: &Row) -> Vec<String> {
    let count = row.as_ref().column_count();
    (0..count)
        .map(|i| match row.get_ref(i) {
            Ok(ValueRef::Null) => String::new(),
            Ok(ValueRef::Integer(n)) => n.to_string(),
            Ok(ValueRef::Real(f)) => f.to_string(),
            Ok(ValueRef::Text(bytes)) => String::from_utf8_lossy(bytes).into_owned(),
            Ok(ValueRef::Blob(_)) => String::from("<blob>"),
            Err(_) => String::new(),
        })
        .collect()
}

enum QueryResult {
    Rows { cols: Vec<String>, rows: Vec<Vec<String>> },
    Ack  { rowcount: usize },
}

fn write_csv(path: &str, cols: &[String], rows: &[Vec<String>]) -> Result<()> {
    let mut f = File::create(path).with_context(|| format!("create {}", path))?;
    // header
    f.write_all(cols.join(",").as_bytes())?;
    f.write_all(b"\n")?;
    // simple CSV escaping: wrap fields containing commas or quotes with quotes and escape quotes
    for r in rows {
        let mut first = true;
        for cell in r {
            if !first { f.write_all(b",")?; }
            first = false;
            let needs_quote = cell.contains(',') || cell.contains('"') || cell.contains('\n');
            if needs_quote {
                let escaped = cell.replace('"', "\"\"");
                f.write_all(b"\"")?;
                f.write_all(escaped.as_bytes())?;
                f.write_all(b"\"")?;
            } else {
                f.write_all(cell.as_bytes())?;
            }
        }
        f.write_all(b"\n")?;
    }
    Ok(())
}

fn print_result(res: QueryResult) {
    match res {
        QueryResult::Rows { cols, rows } => {
            if rows.is_empty() {
                println!("[0 rows]");
                return;
            }
            let mut table = Table::new();
            table.set_header(cols.iter().map(|c| Cell::new(c)));
            for r in &rows {
                table.add_row(r.clone());
            }
            println!("{table}");
            println!("[{} row(s)]", rows.len());
        }
        QueryResult::Ack { rowcount } => {
            println!("OK, {} row(s) affected.", rowcount);
        }
    }
}

fn main() -> Result<()> {
    println!("fast-sql-term (Rust) — .help for commands");

    let mut rl = DefaultEditor::new()?;
    let mut db = DB::new();

    loop {
        let prompt = if db.conn.is_some() { "sql> " } else { "sql(no-db)> " };
        let readline = rl.readline(prompt);
        match readline {
            Ok(line) => {
                let line = line.trim();
                if line.is_empty() { continue; }
                rl.add_history_entry(line)?;

                if line.starts_with('.') {
                    if let Err(e) = handle_meta(&mut db, line) {
                        eprintln!("Error: {e}");
                    }
                    continue;
                }

                match db.run(line) {
                    Ok(res) => print_result(res),
                    Err(e) => eprintln!("Error: {e}"),
                }
            }
            Err(ReadlineError::Interrupted) => { println!("^C"); continue; }
            Err(ReadlineError::Eof) => { println!(); break; }
            Err(err) => { eprintln!("Readline error: {err}"); break; }
        }
    }
    Ok(())
}

fn handle_meta(db: &mut DB, line: &str) -> Result<()> {
    let mut parts = line.split_whitespace();
    let cmd = parts.next().unwrap_or("");
    match cmd {
        ".help" => {
            println!(
                "Meta commands:
  .open <file.db>      open/create SQLite DB
  .ls                  list tables
  .ddl <table>         show CREATE TABLE
  .preview <table> [n] preview first n rows (default 50)
  .quit                exit
SQL:
  Type any SQL and press Enter.
.fk [table]          list foreign keys (defaults to current table)
.goto <table>        jump to referenced table name
.find <pattern>      filter tables by substring (case-insensitive)
.exportcsv <path> <SELECT ...>
                     run a SELECT and write results to CSV
.ui                   open interactive UI (arrow keys to navigate, Enter to preview, D to show DDL, F for FKs, Q to quit)"
            );
        }
        ".open" => {
            let path = parts.next().ok_or_else(|| anyhow!("Usage: .open <file.db>"))?;
            db.open(path)?; println!("Opened {path}");
            db.table = None;
        }
        ".ls" => {
            let tables = db.list_tables()?;
            if tables.is_empty() { println!("<no tables>"); }
            else { for t in tables { println!("{t}"); } }
        }
        ".ddl" => {
            let t = parts.next().ok_or_else(|| anyhow!("Usage: .ddl <table>"))?;
            match db.ddl(t)? { Some(s) => println!("{s}"), None => println!("(no ddl)") }
        }
        ".preview" => {
            let table = parts.next().ok_or_else(|| anyhow!("Usage: .preview <table> [n]"))?;
            let n = parts.next().unwrap_or("50").parse::<usize>().unwrap_or(50);
            let res = db.preview(table, n)?;
            print_result(res);
            db.table = Some(table.to_string());
        }
        ".fk" => {
            let t = if let Some(tt) = parts.next() {
                tt.to_string()
            } else {
                db.table.clone().ok_or_else(|| anyhow!("No table selected; usage: .fk [table]"))?
            };
            let fks = db.foreign_keys(&t)?;
            if fks.is_empty() {
                println!("(no foreign keys on {})", t);
            } else {
                let mut table = Table::new();
                table.set_header(vec!["from", "ref_table", "ref_col", "actions"]);
                for (from, rt, rc, info) in fks {
                    table.add_row(vec![from, rt, rc, info]);
                }
                println!("{table}");
            }
        }
        ".goto" => {
            let target = parts.next().ok_or_else(|| anyhow!("Usage: .goto <table>"))?;
            db.table = Some(target.to_string());
            println!("table: {}", target);
        }
        ".find" => {
            let pat = parts.next().ok_or_else(|| anyhow!("Usage: .find <pattern>"))?.to_lowercase();
            let names = db.list_tables()?;
            let mut hits: Vec<String> = names.into_iter().filter(|n| n.to_lowercase().contains(&pat)).collect();
            if hits.is_empty() { println!("(no matches)"); }
            else { hits.sort(); for h in hits { println!("{h}"); } }
        }
        ".exportcsv" => {
            let path = parts.next().ok_or_else(|| anyhow!("Usage: .exportcsv <path> <SELECT ...>"))?;
            let rest: String = parts.collect::<Vec<_>>().join(" ");
            if rest.is_empty() { return Err(anyhow!("Provide a SELECT query after the path")); }
            let (cols, rows) = db.select(&rest)?;
            write_csv(path, &cols, &rows)?;
            println!("wrote {}, {} rows", path, rows.len());
        }
        ".ui" => {
            run_ui(db)?;
        }
        ".quit" | ".q" | ".exit" => { std::process::exit(0); }
        _ => println!("Unknown meta. Try .help"),
    }
    Ok(())
}

fn run_ui(db: &mut DB) -> Result<()> {
    db.ensure()?; // need an open DB
    const PREVIEW_LIMIT: usize = 1000;
    // gather tables
    let mut tables = db.list_tables()?;
    if tables.is_empty() {
        println!("<no tables>");
        return Ok(());
    }
    let mut selected: usize = 0;
    let mut list_state = ListState::default();
    list_state.select(Some(selected));
    let mut right_offset: usize = 0; // vertical scroll for right pane
    let mut mode: &str = "preview"; // or "ddl" or "fk"
    // cached data
    let mut preview_cols: Vec<String> = Vec::new();
    let mut preview_rows: Vec<Vec<String>> = Vec::new();
    let mut ddl_text: Option<String> = None;
    let mut fk_rows: Vec<(String, String, String, String)> = Vec::new();

    // initial load
    {
        db.table = Some(tables[selected].clone());
        if let Ok(res) = db.preview(&tables[selected], PREVIEW_LIMIT) {
            if let QueryResult::Rows { cols, rows } = res {
                preview_cols = cols;
                preview_rows = rows;
            }
        }
        ddl_text = db.ddl(&tables[selected])?;
        fk_rows = db.foreign_keys(&tables[selected])?;
    }

    // setup terminal
    enable_raw_mode()?;
    let mut stdout = std::io::stdout();
    execute!(stdout, EnterAlternateScreen, EnableMouseCapture)?;
    let backend = CrosstermBackend::new(stdout);
    let mut terminal = Terminal::new(backend)?;
    terminal.clear()?;

    let tick_rate = Duration::from_millis(200);
    let mut last_tick = Instant::now();

    let res = loop {
        // draw
        terminal.draw(|f| {
            // overall: top help bar + main area
            let layout = Layout::default()
                .direction(Direction::Vertical)
                .constraints([Constraint::Length(4), Constraint::Min(1)].as_ref())
                .split(f.size());

            // help / nav tools at top (multi-line)
            let help = "UI Navigator Controls
↑/↓  Move selection    Enter  Preview rows    D  Show DDL    F  Show Foreign Keys
PgUp/PgDn/Home/End  Scroll left list     Shift+↑/Shift+↓  Scroll right pane     Shift+PgUp/PgDn  Faster scroll    Q  Quit UI
Preview loads up to 1000 rows for scrolling. Empty views will display: 'There is no data for this type'.";
            let help_block = Block::default().borders(Borders::ALL).title("Controls — UI Navigator");
            let help_para = Paragraph::new(help).block(help_block);
            f.render_widget(help_para, layout[0]);

            // main split: left tables, right content
            let panes = Layout::default()
                .direction(Direction::Horizontal)
                .constraints([Constraint::Percentage(30), Constraint::Percentage(70)].as_ref())
                .split(layout[1]);

            draw_left(f, panes[0], &tables, &mut list_state, selected);
            draw_right(f, panes[1], mode, &tables[selected], &preview_cols, &preview_rows, ddl_text.as_deref(), &fk_rows, right_offset);
        })?;

        let timeout = tick_rate.saturating_sub(last_tick.elapsed());
        if crossterm::event::poll(timeout)? {
            if let CEvent::Key(key) = event::read()? {
                match key.code {
                    KeyCode::Up if key.modifiers.contains(KeyModifiers::SHIFT) => {
                        // Scroll right pane up by 1
                        right_offset = right_offset.saturating_sub(1);
                    }
                    KeyCode::Down if key.modifiers.contains(KeyModifiers::SHIFT) => {
                        // Scroll right pane down by 1 (cap roughly to content length)
                        let total_len = match mode {
                            "ddl" => ddl_text.as_deref().map(|s| s.lines().count()).unwrap_or(0),
                            "fk" => fk_rows.len(),
                            _ => preview_rows.len(),
                        };
                        if right_offset + 1 < total_len { right_offset += 1; }
                    }
                    KeyCode::PageUp if key.modifiers.contains(KeyModifiers::SHIFT) => {
                        right_offset = right_offset.saturating_sub(10);
                    }
                    KeyCode::PageDown if key.modifiers.contains(KeyModifiers::SHIFT) => {
                        let total_len = match mode {
                            "ddl" => ddl_text.as_deref().map(|s| s.lines().count()).unwrap_or(0),
                            "fk" => fk_rows.len(),
                            _ => preview_rows.len(),
                        };
                        right_offset = (right_offset + 10).min(total_len.saturating_sub(1));
                    }
                    KeyCode::Char('q') | KeyCode::Char('Q') => break Ok(()),
                    KeyCode::Up => {
                        if selected > 0 { selected -= 1; }
                        list_state.select(Some(selected));
                        right_offset = 0;
                        db.table = Some(tables[selected].clone());
                        if let Ok(res) = db.preview(&tables[selected], PREVIEW_LIMIT) {
                            if let QueryResult::Rows { cols, rows } = res {
                                preview_cols = cols;
                                preview_rows = rows;
                            }
                        }
                        ddl_text = db.ddl(&tables[selected])?;
                        fk_rows = db.foreign_keys(&tables[selected])?;
                    }
                    KeyCode::Down => {
                        if selected + 1 < tables.len() { selected += 1; }
                        list_state.select(Some(selected));
                        right_offset = 0;
                        db.table = Some(tables[selected].clone());
                        if let Ok(res) = db.preview(&tables[selected], PREVIEW_LIMIT) {
                            if let QueryResult::Rows { cols, rows } = res {
                                preview_cols = cols;
                                preview_rows = rows;
                            }
                        }
                        ddl_text = db.ddl(&tables[selected])?;
                        fk_rows = db.foreign_keys(&tables[selected])?;
                    }
                    KeyCode::PageUp => {
                        let step = 10usize;
                        selected = selected.saturating_sub(step);
                        list_state.select(Some(selected));
                        right_offset = 0;
                        db.table = Some(tables[selected].clone());
                        if let Ok(res) = db.preview(&tables[selected], PREVIEW_LIMIT) {
                            if let QueryResult::Rows { cols, rows } = res {
                                preview_cols = cols; preview_rows = rows;
                            }
                        }
                        ddl_text = db.ddl(&tables[selected])?;
                        fk_rows = db.foreign_keys(&tables[selected])?;
                    }
                    KeyCode::PageDown => {
                        let step = 10usize;
                        selected = (selected + step).min(tables.len().saturating_sub(1));
                        list_state.select(Some(selected));
                        right_offset = 0;
                        db.table = Some(tables[selected].clone());
                        if let Ok(res) = db.preview(&tables[selected], PREVIEW_LIMIT) {
                            if let QueryResult::Rows { cols, rows } = res {
                                preview_cols = cols; preview_rows = rows;
                            }
                        }
                        ddl_text = db.ddl(&tables[selected])?;
                        fk_rows = db.foreign_keys(&tables[selected])?;
                    }
                    KeyCode::Home => {
                        selected = 0;
                        list_state.select(Some(selected));
                        right_offset = 0;
                        db.table = Some(tables[selected].clone());
                        if let Ok(res) = db.preview(&tables[selected], PREVIEW_LIMIT) {
                            if let QueryResult::Rows { cols, rows } = res {
                                preview_cols = cols; preview_rows = rows;
                            }
                        }
                        ddl_text = db.ddl(&tables[selected])?;
                        fk_rows = db.foreign_keys(&tables[selected])?;
                    }
                    KeyCode::End => {
                        if !tables.is_empty() {
                            selected = tables.len() - 1;
                            list_state.select(Some(selected));
                            right_offset = 0;
                            db.table = Some(tables[selected].clone());
                            if let Ok(res) = db.preview(&tables[selected], PREVIEW_LIMIT) {
                                if let QueryResult::Rows { cols, rows } = res {
                                    preview_cols = cols; preview_rows = rows;
                                }
                            }
                            ddl_text = db.ddl(&tables[selected])?;
                            fk_rows = db.foreign_keys(&tables[selected])?;
                        }
                    }
                    KeyCode::Enter => {
                        mode = "preview";
                        right_offset = 0;
                    }
                    KeyCode::Char('d') | KeyCode::Char('D') => {
                        mode = "ddl";
                        right_offset = 0;
                    }
                    KeyCode::Char('f') | KeyCode::Char('F') => {
                        mode = "fk";
                        right_offset = 0;
                    }
                    _ => {}
                }
            }
        }
        if last_tick.elapsed() >= tick_rate {
            last_tick = Instant::now();
        }
    };

    // teardown terminal
    disable_raw_mode()?;
    let mut stdout2 = std::io::stdout();
    execute!(stdout2, LeaveAlternateScreen, DisableMouseCapture)?;
    terminal.show_cursor()?;

    res
}

fn draw_left(f: &mut Frame, area: Rect, tables: &[String], state: &mut ListState, selected: usize) {
    let items: Vec<ListItem> = tables.iter().map(|t| ListItem::new(t.clone())).collect();
    let title = format!("Tables ({})", tables.len());
    let list = List::new(items)
        .block(Block::default().borders(Borders::ALL).title(title))
        .highlight_style(Style::default().fg(Color::Cyan).add_modifier(Modifier::BOLD))
        .highlight_symbol("› ");
    f.render_stateful_widget(list, area, state);
}

fn draw_right(
    f: &mut Frame,
    area: Rect,
    mode: &str,
    table_name: &str,
    cols: &[String],
    rows: &[Vec<String>],
    ddl: Option<&str>,
    fks: &[(String, String, String, String)],
    right_offset: usize,
) {
    let title = format!("{} — {}", table_name, match mode { "ddl" => "DDL", "fk" => "Foreign Keys", _ => "Preview" });
    let block = Block::default().borders(Borders::ALL).title(title);
    let inner = block.inner(area);
    f.render_widget(block, area);

    match mode {
        "ddl" => {
            let content = ddl.unwrap_or("");
            let text = if content.is_empty() {
                "There is no data for this type (DDL)".to_string()
            } else {
                content.to_string()
            };
            let p = Paragraph::new(text)
                .wrap(ratatui::widgets::Wrap { trim: false })
                .scroll((right_offset as u16, 0));
            f.render_widget(p, inner);
        }
        "fk" => {
            if fks.is_empty() {
                let p = Paragraph::new("There is no data for this type (Foreign Keys)")
                    .wrap(ratatui::widgets::Wrap { trim: false })
                    .scroll((right_offset as u16, 0));
                f.render_widget(p, inner);
            } else {
                let header = vec!["from", "ref_table", "ref_col", "actions"];
                let header_cells = header
                    .iter()
                    .map(|h| TuiCell::from(*h).style(Style::default().add_modifier(Modifier::BOLD)));

                let max_rows = inner.height.saturating_sub(3) as usize;
                let start = right_offset.min(fks.len().saturating_sub(1));
                let end = (start + max_rows).min(fks.len());
                let slice = if start < end { &fks[start..end] } else { &[] };

                let rows_vec: Vec<RRow> = slice
                    .iter()
                    .map(|(from, rt, rc, acts)| RRow::new(vec![from.clone(), rt.clone(), rc.clone(), acts.clone()]))
                    .collect();
                let widths = [
                    Constraint::Length(16),
                    Constraint::Length(24),
                    Constraint::Length(16),
                    Constraint::Min(10),
                ];
                let table = RTable::new(rows_vec, widths).header(RRow::new(header_cells));
                f.render_widget(table, inner);
            }
        }
        _ => {
            if cols.is_empty() {
                let p = Paragraph::new("There is no data for this type (Preview)")
                    .wrap(ratatui::widgets::Wrap { trim: false });
                f.render_widget(p, inner);
            } else {
                let header_cells = cols
                    .iter()
                    .map(|c| TuiCell::from(c.clone()).style(Style::default().fg(Color::Cyan).add_modifier(Modifier::BOLD)));

                let max_rows = inner.height.saturating_sub(3) as usize;
                let total = rows.len();
                if total == 0 {
                    let p = Paragraph::new("There is no data for this type (Preview)")
                        .wrap(ratatui::widgets::Wrap { trim: false });
                    f.render_widget(p, inner);
                } else {
                    let start = right_offset.min(total.saturating_sub(1));
                    let end = (start + max_rows).min(total);

                    let rrows: Vec<RRow> = rows[start..end]
                        .iter()
                        .map(|r| RRow::new(r.iter().cloned().collect::<Vec<_>>()))
                        .collect();

                    let widths: Vec<Constraint> = cols.iter().map(|_| Constraint::Min(8)).collect();
                    let table = RTable::new(rrows, widths).header(RRow::new(header_cells));
                    f.render_widget(table, inner);
                }
            }
        }
    }
}