# Reviewed differences: alacritty vs Ghostty

Each engine is checked against its own blessed grids (`expected.<engine>.json`).
This file records the reviewed differences between the two engines' grids
(`copilot-canvas corpus diff`). Engine updates are one reviewed change together
with this file and the re-blessed grids.

Reviewed 2026-10-27 against Ghostty `521cf2c5d30b` and alacritty_terminal 0.26.

| Entry | Difference | Verdict |
|---|---|---|
| `claude-startup`, `opencode-startup` | Ghostty answers XTVERSION (`DCS >\|libghostty ST`); alacritty does not answer. alacritty reports an empty title reset where Ghostty reports nothing. | Ghostty is more complete; the agents only use the reply to pick features. |
| `opencode-startup`, `synth-modes` | With any-event mouse tracking (1003) Ghostty also reports button-event tracking (`mouse_drag`); with UTF-8 mouse (1005) it also sets SGR. | Equivalent for encoding: drags are reported in both; SGR wins over UTF-8 when both are set. |
| `synth-modes` (`kitty-pop`) | After push 1, set 3, push 8, pop, Ghostty restores flags 3; alacritty restores 1. | **alacritty bug**: the kitty spec's "set" replaces the top of the stack. Ghostty is correct. |
| `synth-osc` | Ghostty counts `OSC 9 ; text` (iTerm2 notification) as a notification; alacritty's OSC scan only knows `OSC 777`. | Ghostty is correct. |
| `synth-resize` | Same reflowed text, different viewport choice: on shrink Ghostty keeps one fewer line in scrollback; on widen alacritty pulls lines back from the scrollback and Ghostty does not. | Both valid reflow strategies; no text is lost. |
| `synth-sgr` | Ghostty reports blink and overline; SGR 21 is a double underline in Ghostty (ignored by alacritty). | Ghostty is more complete (protocol 8 carries both flags). |
| `synth-sync` | Clear screen (ED 2) does not push the screen into scrollback in Ghostty. | Matches xterm; alacritty's behaviour is its own choice. |
| `synth-unicode` | Regional indicators: each is two columns in Ghostty, one in alacritty; neither clusters flag pairs (grapheme mode 2027 is off). The keycap `1 FE0F 20E3` keeps FE0F only in alacritty. | Reviewed: ConPTY in front of both engines has its own width rules; the TUI heals width artifacts. Revisit when mode 2027 is enabled. |
| `vim` | alacritty reports four title events (save/restore title stack), Ghostty one (only real changes). | Same final title. |
| `opencode-startup`, `vim`, `synth-resize` | Soft-wrap state (the `F8_SOFTWRAP` marker run, reviewed 2026-10-29): full-screen programs paint rows into the next one, so most of their rows wrap in both engines; rows 13 and 19–22 of OpenCode's start screen wrap only in alacritty (the engines differ in when a repainted row keeps its wrap flag); vim's emoji row wraps only in Ghostty (its widths fill the row); `synth-resize` shifts with the viewport choice above. | Copying ignores wrap state on the alternate screen (every row is a line there). Shell output on the primary screen wraps identically in every entry. |
| `git`, `powershell`, `synth-bytewise`, `synth-screen` | none | identical |
