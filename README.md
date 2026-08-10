# ccorral

Terminal control panel (Rust TUI) for managing multiple [Claude Code](https://claude.com/claude-code)
sessions running in tmux.

- **Left panel** — list of Claude Code sessions.
- **Right panel** — a *real* tmux client embedded in a PTY (via [tui-term](https://crates.io/crates/tui-term)),
  attached to the selected session's claude pane, zoomed and rendered at panel size for
  clean, real-time interaction.

## Build & run

`cargo` lives in the nix dev shell, not on your global PATH.

```sh
# dev shell (or `direnv allow` — .envrc is `use flake`)
nix develop
cargo run

# one-off release build
nix build          # -> result/bin/ccorral
```

Run ccorral in its **own** tmux window — not split next to a claude pane. Embedding its
own window causes an infinite hall-of-mirrors.

## Keys

| Key | Action |
|-----|--------|
| Ctrl+↑ / Ctrl+↓ | switch session |
| Ctrl+L / F5 | refresh |
| F2 | toggle mouse |
| wheel | scroll |
| Esc | interrupt |
| Ctrl+Q | quit |
| _other_ | forwarded to the pane |

## Caveat

Embedding resizes the real Claude pane, so a resize storm *can* crash Claude. Mitigated by
debouncing switches, flooring the size, and never forwarding Ctrl-D.

## License

MIT
