# ccorral

Terminal control panel (Rust TUI) for managing multiple Claude Code sessions in tmux.
Left panel = session list; right panel = a **real tmux client** embedded in a PTY (via
tui-term), attached to the selected session's claude pane, zoomed and rendered at panel size.

## Build / run

No `cargo` on PATH outside the dev shell — use nix:

- Dev shell: `nix develop` (or `direnv allow`, `.envrc` = `use flake`), then `cargo build`/`cargo run`.
- One-off build: `nix build` → `result/bin/ccorral`.
- Ad-hoc: `nix shell nixpkgs#cargo nixpkgs#gcc -c cargo build` (needs gcc for the linker).

Run ccorral in its **own** tmux window — not split next to a claude pane (embedding its
own window causes an infinite hall-of-mirrors).

## Keys

Ctrl+↑/↓ switch · Ctrl+L / F5 refresh · F2 toggle mouse · wheel scroll · Ctrl+Q quit ·
Esc interrupt · other keys → the pane.

## Gotcha

Embedding resizes the real Claude pane; a resize storm can crash Claude. Mitigated by
debouncing switches, flooring the size, and never forwarding Ctrl-D. See the header
comment in `src/main.rs`.
