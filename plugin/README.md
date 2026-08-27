# omarchy-novad (Omarchy plugin)

Omarchy shell plugin front-end for [omarchy-novad](https://github.com/tslove923/omarchy-novad),
an NPU-accelerated wake-word voice assistant: "hey jarvis" → dictation →
local intent classification → routed command (or handed off to OpenClaw
for anything needing real reasoning). This plugin is the UI + service
wiring; the actual daemon is a separate Rust binary this plugin's
`setup` script wires into systemd `--user` units.

```
schemaVersion 1, kinds: ["service", "bar-widget", "overlay"]
```

## What's in here

| Kind | Entry point | What it does |
|---|---|---|
| `service` | `Service.qml` | Owns all shared UI state: watches the daemon's `popup-state.json` and `conversation-state.json` (under `$XDG_RUNTIME_DIR/omarchy-novad/`), and runs `omarchy-novad respond`/`converse` for every action. Mounted once per session; `bar-widget` and `overlay` both read off it rather than watching the daemon's files themselves. |
| `bar-widget` | `BarWidget.qml` | A small ported nova-npu tray icon in the bar (the glossy sphere PNGs from `electron/assets/tray-*.png`, colored by the current phase — see `BarWidget.qml`'s header comment for the exact state mapping) plus nova's tray context menu, adapted to this project's real capabilities. Left-click opens a quick-status popup (latest status line, one-click Dismiss/Stop-conversation); right-click opens the context menu (OpenClaw Chat, Open Config File, Stop Listening, Quit omarchy-novad…). |
| `overlay` | `Overlay.qml` | The full UI: `PopupCard.qml` (dictation review / command confirmation, centered near the top of screen) and `ConversationPanel.qml` (the OpenClaw voice-conversation transcript, docked to the right edge). Both show themselves automatically whenever the daemon has something to show — this overlay is `keepLoaded: true` and mounted for the whole shell session, not summoned on demand. |

`PopupCard.qml`/`ConversationPanel.qml` are a direct port of this
repo's standalone `quickshell/OmarchyNovadPopup.qml` /
`quickshell/OpenClawConversation.qml` onto the Omarchy plugin host
contract (host-injected `Item`s instead of their own `qs -p quickshell`
process) — same visual design, same animated conic-gradient border
(`AnimatedBorder.qml`), same Enter-to-send edit boxes, same
chat-style scrolling transcript with a bottom-pinned status/confirm
bar. `OmarchyTheme.qml` is the same live `colors.toml` reader too,
kept as this plugin's own copy rather than switched to the host
shell's `qs.Commons.Color` singleton, since `Color` has no
red/green/yellow/magenta equivalent this UI's phase colors need.

The standalone `qs -p quickshell` dev harness (`quickshell/` at the repo
root, `omarchy-novad popup-demo`, etc.) is untouched and keeps working —
this plugin is an additional, independently maintained port of that UI,
not a replacement for it. See the main repo's README for that workflow.

### Design note: one overlay, not two

The dictation-review popup and the OpenClaw conversation transcript
share one `overlay` entry point instead of two. They already coexisted
side by side in the standalone build, share one `Service` instance,
never compete for the same screen space (popup centered near the top,
conversation docked right), and the `io.github.spencerbull.omapilot`
plugin's `Ambient.qml` is precedent for one overlay entry point owning
multiple independent on-screen surfaces. Split them if a future surface
here ever needs independent enable/disable from `shell.json`.

## Install

```
omarchy plugin add https://github.com/tslove923/omarchy-novad-plugin   # or wherever you host this
~/.config/omarchy/plugins/io.github.tslove923.omarchy-novad/setup
```

`omarchy plugin add` only clones files — run `setup` yourself afterward.
It:

1. Locates your built `omarchy-novad` binary (build it first — see the
   main repo's README "Setup" section; `cargo build --release`).
2. Detects an installed OpenVINO GenAI SDK runtime (for `serve`) and
   your classifier model directory.
3. Writes and installs `omarchy-novad-serve.service` and
   `omarchy-novad-detect.service` as systemd `--user` units, then
   enables and starts them.
4. If it can find a real omarchy-novad source checkout with the Kokoro
   TTS model files already downloaded (see `tts-prototype/README.md`
   in that checkout), also installs, enables, and starts
   `omarchy-novad-tts.service` — needed only for `omarchy-novad converse
   start`'s voice loop. Skipped (with an explanation) otherwise; you
   can always run it by hand later, or rerun `setup` once the model
   files are in place.

Every detection step can be overridden with an environment variable if
the defaults don't match your machine — see the comments at the top of
`setup` (`OMARCHY_NOVAD_BIN`, `OMARCHY_NOVAD_OV_SDK_VERSION`,
`OMARCHY_NOVAD_MODEL_DIR`, `OMARCHY_NOVAD_MODEL_ID`,
`OMARCHY_NOVAD_REPO_DIR`, etc.).

Safe to rerun — every step is idempotent.

## Configuration

This plugin has no settings of its own in `shell.json`; the daemon's
own config file, `~/.config/omarchy-novad/config.toml`, is where
everything lives:

- Home Assistant token (for "turn on the living room lights")
- BlueBubbles server URL/password (for "text mom I'm running late")
- Telegram API credentials (for "telegram sarah are you free tonight")
- OpenClaw gateway env file (`~/.config/openclaw-novad.env`) for the
  `EXTERNAL`/`CODING` handoff and `converse start`
- `[tts]` voice choice for the conversation loop's spoken replies

See the main omarchy-novad README's Configuration section and the
setup guides it links to (Home Assistant, BlueBubbles, Telegram,
OpenClaw) for what each of those actually needs.

## Remove

```
~/.config/omarchy/plugins/io.github.tslove923.omarchy-novad/remove
omarchy plugin remove io.github.tslove923.omarchy-novad
```

`remove` stops and disables the three systemd units and deletes the
installed unit files. It leaves the `omarchy-novad` binary, your
models, and `config.toml` in place in case you reinstall later.

## Troubleshooting

- **Bar widget/overlay show nothing, ever.** Check `journalctl --user
  -u omarchy-novad-detect -f` — if the unit isn't running, the state
  files under `$XDG_RUNTIME_DIR/omarchy-novad/` never get written and
  every property on `Service.qml` just stays at its idle default.
- **Buttons (Approve/Deny/Insert/Cancel/Stop) do nothing.** `Service.qml`
  runs `omarchy-novad respond`/`converse` via `Quickshell.Io.Process`,
  which execs directly — no shell, no `~/Work/.../omarchy-novad`
  shortcut. It silently no-ops if `omarchy-novad` isn't on the
  `omarchy-shell` process's own PATH. Either put it on PATH globally,
  or override `Service.qml`'s `novadBinary` property with an absolute
  path (there's no `shell.json` setting for this yet — edit the
  installed copy directly, or open an issue if you'd like one added).
- **Colors look wrong / didn't update after `omarchy theme set`.**
  `OmarchyTheme.qml` reads `~/.local/state/omarchy/current/theme/colors.toml`
  directly with a `FileView` watcher; if a theme doesn't define
  `red`/`green`/`yellow`/`magenta`, those fall back to the bundled
  Catppuccin Mocha values.
- **OpenClaw handoffs fail with "gateway assistant is unavailable"
  / "can't connect to the gateway"** even though OpenClaw itself works
  fine interactively. Check `journalctl --user -u omarchy-novad-detect`
  for `openclaw: command not found` — `scripts/openclaw-handoff` execs
  the bare `openclaw` command and needs it on the *systemd user
  service's* PATH, not just an interactive shell's. This is why
  Requirements below assumes the official installer: it places its
  wrapper at `~/.local/bin/openclaw`, which is already on that PATH. A
  hand-built or custom-located `openclaw` needs its own symlink into
  `~/.local/bin` (same fix as the PATH note above for `omarchy-novad`
  itself).

## Requirements

Same as the main project: an Intel NPU/iGPU with OpenVINO GenAI, a
build of [voxtype](https://github.com/peteonrails/voxtype) with the
external-trigger silence-timeout fix, and (optionally) OpenClaw,
installed via the official installer:

```
curl -fsSL https://openclaw.ai/install.sh | bash
```

for the `EXTERNAL`/`CODING` handoff and voice conversation loop. This
plugin assumes that installer's default layout (its wrapper lands at
`~/.local/bin/openclaw`, already on both the graphical shell's and the
systemd user services' PATH) rather than symlinking a custom
`openclaw` location itself. See the main repo's README "Requirements"
section for the full list and links.

## Credits

Ports [nova-npu](https://github.com/tslove923/nova-npu)'s (MIT) UI onto
the Omarchy plugin host contract. See the main omarchy-novad repo for
the daemon this plugin is a front-end for.
