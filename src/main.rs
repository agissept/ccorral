// ccorral — terminal control panel for Claude Code sessions in tmux.
//
// Right panel = a REAL tmux client embedded in a PTY (via tui-term), attached to
// the source session with the claude window+pane selected and zoomed. A real
// client renders at the panel's size → clean, real-time interactive. Trade-off:
// it resizes the real Claude pane, so a resize storm CAN crash Claude — mitigated
// by debouncing switches, flooring the size, and never forwarding Ctrl-D.
//
// Run ccorral in its OWN window (not split with a claude pane).
// Keys: Ctrl+↑/↓ switch · Ctrl+L / F5 refresh · F2 toggle mouse · wheel scroll ·
// Ctrl+Q quit · Esc interrupt · other keys → the pane.
use std::io::{Read, Write};
use std::sync::mpsc::{channel, Receiver};
use std::thread;
use std::time::{Duration, Instant};

use crossterm::event::{
    self, Event, KeyCode, KeyEvent, KeyEventKind, KeyModifiers, MouseEventKind,
};
use portable_pty::{native_pty_system, CommandBuilder, MasterPty, PtySize};
use ratatui::layout::{Constraint, Direction, Layout};
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, Borders, List, ListItem, ListState, Paragraph};

const HELP: &[&str] = &[
    "Ctrl+↑/↓  switch",
    "Ctrl+G    go to window",
    "F6 / F7   restart / danger",
    "Ctrl+L    refresh",
    "F2        mouse toggle",
    "Ctrl+Q    quit · Esc stop",
];
use tui_term::widget::PseudoTerminal;
use vt100::Parser;

fn tmux(args: &[&str]) -> std::io::Result<std::process::Output> {
    std::process::Command::new("tmux").args(args).output()
}

fn err(e: impl ToString) -> std::io::Error {
    std::io::Error::other(e.to_string())
}

fn scroll(pane: &str, up: bool) {
    let _ = tmux(&["copy-mode", "-e", "-t", pane]);
    let dir = if up { "scroll-up" } else { "scroll-down" };
    let _ = tmux(&["send-keys", "-X", "-t", pane, "-N", "3", dir]);
}

fn write_status(home: &str, id: &str, state: &str) {
    let dir = format!("{home}/.cc-status");
    let _ = std::fs::create_dir_all(&dir);
    let _ = std::fs::write(format!("{dir}/{id}"), state);
}

fn read_sid(home: &str, id: &str) -> String {
    std::fs::read_to_string(format!("{home}/.cc-status/{id}.sid"))
        .unwrap_or_default()
        .trim()
        .to_string()
}

fn read_status(home: &str, id: &str) -> String {
    let path = format!("{home}/.cc-status/{id}");
    let s = std::fs::read_to_string(&path)
        .unwrap_or_default()
        .trim()
        .to_string();
    if matches!(s.as_str(), "active" | "perm") {
        let stale = std::fs::metadata(&path)
            .and_then(|m| m.modified())
            .map(|t| t.elapsed().map(|d| d > Duration::from_secs(150)).unwrap_or(true))
            .unwrap_or(true);
        if stale {
            return "idle".into();
        }
    }
    s
}

const SPIN: [&str; 12] = [
    "·", "✢", "✳", "✶", "✻", "✽", "✽", "✻", "✶", "✳", "✢", "·",
];

fn shimmer(text: &str, tick: u128) -> Vec<Span<'static>> {
    let n = text.chars().count().max(1) as isize;
    let speed = 140u128;
    let cycle_pos = (tick / speed) as isize;
    let cycle_len = n + 20;
    let glimmer = (cycle_pos % cycle_len) - 10;
    text.chars()
        .enumerate()
        .map(|(i, c)| {
            let d = (i as isize - glimmer).abs();
            let t = match d {
                0 => 1.0,
                1 => 0.5,
                _ => 0.0,
            };
            let r = (215.0 + 30.0 * t) as u8;
            let g = (119.0 + 30.0 * t) as u8;
            let b = (87.0 + 30.0 * t) as u8;
            Span::styled(c.to_string(), Style::default().fg(Color::Rgb(r, g, b)))
        })
        .collect()
}

fn short_path(path: &str) -> String {
    let parts: Vec<&str> = path.split('/').filter(|s| !s.is_empty()).collect();
    parts[parts.len().saturating_sub(2)..].join("/")
}

struct Pane {
    id: String,
    session: String,
    window: String,
    name: String,
    path: String,
    status: String,
}

fn list_claude_panes() -> Vec<Pane> {
    let home = std::env::var("HOME").unwrap_or_default();
    let own = own_session();
    let mut v = Vec::new();
    if let Ok(o) = tmux(&[
        "list-panes", "-a",
        "-f", "#{==:#{pane_current_command},claude}",
        "-F", "#{pane_id}\t#{session_name}\t#{window_id}\t#{session_name}:#{window_index}.#{pane_index}\t#{pane_current_path}",
    ]) {
        for line in String::from_utf8_lossy(&o.stdout).lines() {
            let f: Vec<&str> = line.splitn(5, '\t').collect();
            if let [id, session, window, name, path] = f[..] {
                if own.as_deref() == Some(session) {
                    continue;
                }
                v.push(Pane {
                    id: id.into(),
                    session: session.into(),
                    window: window.into(),
                    name: name.into(),
                    path: path.into(),
                    status: read_status(&home, id),
                });
            }
        }
    }
    v
}

struct Embedded {
    parser: Parser,
    master: Box<dyn MasterPty + Send>,
    writer: Box<dyn Write + Send>,
    rx: Receiver<Vec<u8>>,
    rows: u16,
    cols: u16,
    status_top: bool,
    tty: String, // this attach client's tty, so we can detach it cleanly
}

impl Drop for Embedded {
    fn drop(&mut self) {
        // Detach the client CLEANLY before the PTY closes. Otherwise the abrupt
        // SIGHUP (from closing the PTY) kills the pane's program — confirmed:
        // EOF-sensitive programs (shells, REPLs) die on attach/detach churn.
        if !self.tty.is_empty() {
            let _ = tmux(&["detach-client", "-t", &self.tty]);
        }
    }
}

fn spawn_embedded(pane: &Pane, rows: u16, cols: u16) -> std::io::Result<Embedded> {
    let rows = rows.max(4);
    let cols = cols.max(20);
    let status_pos = |args: &[&str]| {
        tmux(args)
            .ok()
            .map(|o| String::from_utf8_lossy(&o.stdout).trim().to_string())
            .filter(|s| !s.is_empty())
    };
    let status_top = status_pos(&["show-options", "-t", &pane.session, "-v", "status-position"])
        .or_else(|| status_pos(&["show-options", "-gv", "status-position"]))
        .as_deref()
        == Some("top");

    let pair = native_pty_system()
        .openpty(PtySize { rows, cols, pixel_width: 0, pixel_height: 0 })
        .map_err(err)?;

    // Attach a real client to the EXISTING session; select + zoom the pane so a
    // sibling (non-claude) pane in the same window doesn't render alongside it.
    // `tty > file` records this client's tty (its stdin terminal = the PTY slave)
    // so we can detach it cleanly on drop. unset TMUX allows the nested attach.
    let ttyfile = format!("/tmp/ccview-{}.tty", pane.id.replace('%', ""));
    let script = format!(
        "unset TMUX; tty > '{tf}'; \
         tmux select-window -t '{w}'; \
         tmux select-pane -t '{p}'; \
         [ \"$(tmux display -pt '{p}' '#{{window_zoomed_flag}}')\" = 1 ] || tmux resize-pane -Z -t '{p}'; \
         exec tmux attach -t '{s}'",
        tf = ttyfile, s = pane.session, w = pane.window, p = pane.id,
    );
    let mut cmd = CommandBuilder::new("sh");
    cmd.args(["-c", &script]);
    pair.slave.spawn_command(cmd).map_err(err)?;
    drop(pair.slave);

    // read the client tty the script wrote (retry briefly — it's written async)
    let mut tty = String::new();
    for _ in 0..24 {
        if let Ok(s) = std::fs::read_to_string(&ttyfile) {
            let s = s.trim();
            if s.starts_with("/dev/") {
                tty = s.to_string();
                break;
            }
        }
        thread::sleep(Duration::from_millis(3));
    }
    let _ = std::fs::remove_file(&ttyfile);

    let mut reader = pair.master.try_clone_reader().map_err(err)?;
    let writer = pair.master.take_writer().map_err(err)?;
    let (tx, rx) = channel::<Vec<u8>>();
    thread::spawn(move || {
        let mut buf = [0u8; 8192];
        while let Ok(n) = reader.read(&mut buf) {
            if n == 0 || tx.send(buf[..n].to_vec()).is_err() {
                break;
            }
        }
    });

    Ok(Embedded {
        parser: Parser::new(rows, cols, 0),
        master: pair.master,
        writer,
        rx,
        rows,
        cols,
        status_top,
        tty,
    })
}

fn key_to_bytes(key: KeyEvent) -> Vec<u8> {
    let ctrl = key.modifiers.contains(KeyModifiers::CONTROL);
    match key.code {
        KeyCode::Char('d') if ctrl => Vec::new(), // never send Ctrl-D (would exit Claude)
        KeyCode::Char(c) if ctrl => vec![(c.to_ascii_uppercase() as u8).wrapping_sub(0x40) & 0x1f],
        KeyCode::Char(c) => c.to_string().into_bytes(),
        KeyCode::Enter => vec![b'\r'],
        KeyCode::Backspace => vec![0x7f],
        KeyCode::Tab => vec![b'\t'],
        KeyCode::BackTab => vec![0x1b, b'[', b'Z'],
        KeyCode::Esc => vec![0x1b],
        KeyCode::Up => vec![0x1b, b'[', b'A'],
        KeyCode::Down => vec![0x1b, b'[', b'B'],
        KeyCode::Right => vec![0x1b, b'[', b'C'],
        KeyCode::Left => vec![0x1b, b'[', b'D'],
        KeyCode::Home => vec![0x1b, b'[', b'H'],
        KeyCode::End => vec![0x1b, b'[', b'F'],
        KeyCode::Delete => vec![0x1b, b'[', b'3', b'~'],
        KeyCode::PageUp => vec![0x1b, b'[', b'5', b'~'],
        KeyCode::PageDown => vec![0x1b, b'[', b'6', b'~'],
        _ => Vec::new(),
    }
}

fn alive_pane_ids() -> std::collections::HashSet<String> {
    let mut s = std::collections::HashSet::new();
    if let Ok(o) = tmux(&["list-panes", "-a", "-F", "#{pane_id}"]) {
        for l in String::from_utf8_lossy(&o.stdout).lines() {
            s.insert(l.trim().to_string());
        }
    }
    s
}

// Live claude panes, plus any previously-known pane that stopped being claude
// (crashed → now a shell) but whose pane still exists — kept, marked "dead", so
// a crashed session doesn't vanish from the list.
fn refresh_panes(prev: &[Pane]) -> Vec<Pane> {
    let mut out = list_claude_panes();
    let live: std::collections::HashSet<String> = out.iter().map(|p| p.id.clone()).collect();
    let alive = alive_pane_ids();
    for p in prev {
        if !live.contains(&p.id) && alive.contains(&p.id) {
            out.push(Pane {
                id: p.id.clone(),
                session: p.session.clone(),
                window: p.window.clone(),
                name: p.name.clone(),
                path: p.path.clone(),
                status: "dead".into(),
            });
        }
    }
    out
}

fn step(state: &mut ListState, len: usize, delta: isize) {
    if len == 0 {
        return;
    }
    let cur = state.selected().unwrap_or(0) as isize;
    state.select(Some((cur + delta).rem_euclid(len as isize) as usize));
}

// ccorral's own tmux session. Panes in it can't be embedded: attaching a second
// client to a session tmux keeps all its clients on the same window, so the
// embedded view gets dragged onto ccorral's own window → infinite hall of mirrors.
fn own_session() -> Option<String> {
    let pane = std::env::var("TMUX_PANE").ok()?;
    let o = tmux(&["display", "-pt", &pane, "#{session_name}"]).ok()?;
    let s = String::from_utf8_lossy(&o.stdout).trim().to_string();
    (!s.is_empty()).then_some(s)
}

fn main() -> std::io::Result<()> {
    let mut terminal = ratatui::init();
    let _ = crossterm::execute!(std::io::stdout(), crossterm::event::EnableMouseCapture);
    let result = run(&mut terminal);
    let _ = crossterm::execute!(std::io::stdout(), crossterm::event::DisableMouseCapture);
    ratatui::restore();
    result
}

fn run(terminal: &mut ratatui::DefaultTerminal) -> std::io::Result<()> {
    let home = std::env::var("HOME").unwrap_or_default();
    let mut panes = list_claude_panes();
    let mut state = ListState::default();
    if !panes.is_empty() {
        state.select(Some(0));
    }
    let mut embedded: Option<Embedded> = None;
    let mut mouse_on = true;
    let mut started = false;
    let mut pending_switch: Option<Instant> = None;
    let mut last_list = Instant::now();
    let spin_start = Instant::now();

    loop {
        if last_list.elapsed() >= Duration::from_millis(300) {
            panes = refresh_panes(&panes);
            if state.selected().unwrap_or(0) >= panes.len() {
                state.select((!panes.is_empty()).then_some(0));
            }
            last_list = Instant::now();
        }

        let chunks = Layout::default()
            .direction(Direction::Horizontal)
            .constraints([Constraint::Length(32), Constraint::Min(10)])
            .split(terminal.size().map(|s| s.into()).unwrap_or_default());
        let inner = Block::default().borders(Borders::ALL).inner(chunks[1]);

        // open the first pane on startup
        if !started {
            started = true;
            pending_switch = Some(Instant::now() - Duration::from_secs(1));
        }
        // Debounced (re)attach: holding Ctrl+↑/↓ only steps the selection (cheap);
        // the actual attach happens once the selection settles, so buffered
        // key-repeats don't queue slow attaches and overshoot.
        if let Some(t) = pending_switch {
            if t.elapsed() >= Duration::from_millis(150) {
                embedded = match state.selected().and_then(|i| panes.get(i)) {
                    Some(p) => Some(spawn_embedded(p, inner.height, inner.width)?),
                    None => None,
                };
                pending_switch = None;
            }
        }

        if let Some(emb) = embedded.as_mut() {
            while let Ok(bytes) = emb.rx.try_recv() {
                emb.parser.process(&bytes);
            }
            if inner.width != emb.cols || inner.height != emb.rows {
                emb.rows = inner.height.max(4);
                emb.cols = inner.width.max(20);
                emb.parser.screen_mut().set_size(emb.rows, emb.cols);
                let _ = emb.master.resize(PtySize {
                    rows: emb.rows,
                    cols: emb.cols,
                    pixel_width: 0,
                    pixel_height: 0,
                });
            }
        }

        let tick = spin_start.elapsed().as_millis();
        let frame = (tick / 120) as usize;

        terminal.draw(|f| {
            let items: Vec<ListItem> = panes
                .iter()
                .map(|p| {
                    let name = if p.status == "active" {
                        let mut spans = vec![
                            Span::styled(SPIN[frame % SPIN.len()], Style::default().fg(Color::Rgb(215, 119, 87))),
                            Span::raw(" "),
                        ];
                        spans.extend(shimmer(&p.name, tick));
                        Line::from(spans)
                    } else if p.status == "perm" {
                        let yellow = Style::default().fg(Color::Rgb(240, 200, 40));
                        Line::from(vec![
                            Span::styled("●", yellow),
                            Span::styled(format!(" {}", p.name), yellow),
                        ])
                    } else if p.status == "dead" {
                        let red = Style::default().fg(Color::Rgb(230, 80, 80));
                        Line::from(vec![
                            Span::styled("✖", red),
                            Span::styled(format!(" {} (crashed)", p.name), red),
                        ])
                    } else {
                        Line::from(vec![
                            Span::styled("●", Style::default().fg(Color::Rgb(166, 227, 161))),
                            Span::raw(format!(" {}", p.name)),
                        ])
                    };
                    ListItem::new(vec![
                        name,
                        Line::from(Span::styled(
                            format!("  {}", short_path(&p.path)),
                            Style::default().add_modifier(Modifier::DIM),
                        )),
                    ])
                })
                .collect();
            // left column = list on top, shortcut help box below it
            let left = Layout::default()
                .direction(Direction::Vertical)
                .constraints([Constraint::Min(3), Constraint::Length(HELP.len() as u16 + 2)])
                .split(chunks[0]);
            let list = List::new(items)
                .block(Block::default().borders(Borders::ALL).title(" claude "))
                .highlight_style(Style::default().bg(Color::Rgb(69, 71, 90)));
            f.render_stateful_widget(list, left[0], &mut state);

            let help: Vec<Line> = HELP
                .iter()
                .map(|s| Line::from(Span::styled(*s, Style::default().add_modifier(Modifier::DIM))))
                .collect();
            f.render_widget(
                Paragraph::new(help).block(Block::default().borders(Borders::ALL).title(" keys ")),
                left[1],
            );

            let title = match state.selected().and_then(|i| panes.get(i)) {
                Some(p) => {
                    let status = if p.status == "active" {
                        Span::styled(SPIN[frame % SPIN.len()], Style::default().fg(Color::Rgb(215, 119, 87)))
                    } else if p.status == "perm" {
                        Span::styled("●", Style::default().fg(Color::Rgb(240, 200, 40)))
                    } else if p.status == "dead" {
                        Span::styled("✖", Style::default().fg(Color::Rgb(230, 80, 80)))
                    } else {
                        Span::styled("●", Style::default().fg(Color::Rgb(166, 227, 161)))
                    };
                    Line::from(vec![
                        Span::raw(" "),
                        status,
                        Span::raw(format!(" {}  ", p.name)),
                        Span::styled(short_path(&p.path), Style::default().add_modifier(Modifier::DIM)),
                        Span::raw(" "),
                    ])
                }
                None => Line::from(" terminal "),
            };
            let block = Block::default().borders(Borders::ALL).title(title);
            f.render_widget(&block, chunks[1]);
            if let Some(emb) = embedded.as_ref() {
                f.render_widget(PseudoTerminal::new(emb.parser.screen()), inner);
                // blank tmux's status row (top or bottom edge of the pane)
                if inner.height > 0 && inner.width > 0 {
                    let sy = if emb.status_top { inner.y } else { inner.y + inner.height - 1 };
                    let buf = f.buffer_mut();
                    for x in inner.x..inner.x + inner.width {
                        if let Some(c) = buf.cell_mut((x, sy)) {
                            c.reset();
                        }
                    }
                }
            }
        })?;

        if !event::poll(Duration::from_millis(20))? {
            continue;
        }
        let ev = event::read()?;
        if let Event::Mouse(m) = ev {
            if matches!(m.kind, MouseEventKind::ScrollUp | MouseEventKind::ScrollDown) {
                if let Some(p) = state.selected().and_then(|i| panes.get(i)) {
                    scroll(&p.id, matches!(m.kind, MouseEventKind::ScrollUp));
                }
            }
            continue;
        }
        let Event::Key(key) = ev else {
            continue;
        };
        if key.kind != KeyEventKind::Press {
            continue;
        }
        let ctrl = key.modifiers.contains(KeyModifiers::CONTROL);

        if ctrl && key.code == KeyCode::Char('q') {
            break;
        }
        if ctrl && matches!(key.code, KeyCode::Up | KeyCode::Down) {
            step(&mut state, panes.len(), if key.code == KeyCode::Up { -1 } else { 1 });
            pending_switch = Some(Instant::now()); // debounce; attach after it settles
            continue;
        }
        // Ctrl+G: jump the REAL terminal (outer client) to the selected pane's
        // window, so you can work in it directly. ccorral keeps running.
        if ctrl && key.code == KeyCode::Char('g') {
            if let Some(p) = state.selected().and_then(|i| panes.get(i)) {
                let _ = tmux(&["switch-client", "-t", &p.session]);
                let _ = tmux(&["select-window", "-t", &p.window]);
                let _ = tmux(&["select-pane", "-t", &p.id]);
            }
            continue;
        }
        // Ctrl+R: restart Claude in the selected pane (kill it, relaunch with
        // --continue to resume the same conversation).
        // F6 restart · F7 restart in dangerous mode (skip permissions).
        // F-keys are used (not Ctrl) so they don't collide with Claude's Ctrl keys.
        if matches!(key.code, KeyCode::F(6) | KeyCode::F(7)) {
            let dangerous = key.code == KeyCode::F(7);
            if let Some(p) = state.selected().and_then(|i| panes.get(i)) {
                let id = p.id.clone();
                let sid = read_sid(&home, &id);
                let mut cmd = if sid.is_empty() {
                    "claude --continue".to_string()
                } else {
                    format!("claude --resume {sid}")
                };
                if dangerous {
                    cmd.push_str(" --dangerously-skip-permissions");
                }
                let _ = tmux(&["respawn-pane", "-k", "-c", &p.path, "-t", &id]);
                let _ = tmux(&["send-keys", "-t", &id, "-l", &cmd]);
                let _ = tmux(&["send-keys", "-t", &id, "Enter"]);
            }
            continue;
        }
        if key.code == KeyCode::F(5) || (ctrl && matches!(key.code, KeyCode::Char('l') | KeyCode::Char('L'))) {
            // force a repaint: shrink the PTY; the resize block grows it back → SIGWINCH
            if let Some(emb) = embedded.as_mut() {
                let shrink = emb.rows.saturating_sub(2).max(4);
                emb.rows = shrink;
                emb.parser.screen_mut().set_size(shrink, emb.cols);
                let _ = emb.master.resize(PtySize { rows: shrink, cols: emb.cols, pixel_width: 0, pixel_height: 0 });
            }
            let _ = terminal.clear();
            continue;
        }
        if key.code == KeyCode::F(2) {
            mouse_on = !mouse_on;
            let _ = if mouse_on {
                crossterm::execute!(std::io::stdout(), crossterm::event::EnableMouseCapture)
            } else {
                crossterm::execute!(std::io::stdout(), crossterm::event::DisableMouseCapture)
            };
            continue;
        }

        if let Some(emb) = embedded.as_mut() {
            let _ = emb.writer.write_all(&key_to_bytes(key)); // Ctrl-D blocked in key_to_bytes
            let _ = emb.writer.flush();
            if key.code == KeyCode::Esc {
                if let Some(p) = state.selected().and_then(|i| panes.get(i)) {
                    write_status(&home, &p.id, "idle");
                }
            }
        }
    }
    Ok(())
}
