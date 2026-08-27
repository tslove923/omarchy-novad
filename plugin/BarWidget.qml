// omarchy-novad's `bar-widget` entry point -- a faithful port of
// nova-npu's real Electron system-tray icon and right-click context
// menu (see ~/Work/nova-npu/electron/main.js's createTray()/
// buildContextMenuTemplate(), and electron/assets/tray-*.png) onto
// this plugin's own click-to-open popup pattern, replacing this
// file's original placeholder (a pulsing colored dot + "Jarvis" text
// label -- there was never any real icon art here before). Mirrors
// the `hass` plugin's Panel.qml shape: "Bar button plus popup panel.
// Owns [its own UI state]; the service owns [the shared state]" --
// extends the shared `Panel` base type from `qs.Ui` the same way
// hass's Panel.qml does, which provides the open/close/toggle
// lifecycle and an IpcHandler for free. This redesign only touches
// the icon artwork and adds the context menu; the surrounding
// architecture (Panel base, Service.qml as the single source of
// state/actions, the self-managed layer-shell popup instead of
// `qs.Ui.KeyboardPanel`) is unchanged from the original file -- see
// the "Quick-status popup" comment further down for why.
//
// ── Icon-state mapping ──────────────────────────────────────────────
//
// nova's tray had 6 states (idle/warming/listening/recording/
// transcribing/typing), each a 128x128 glossy sphere PNG differing
// only by color (`tray-warming.png` is byte-identical to
// `tray-idle.png` -- nova never actually drew a distinct "warming"
// look, just reused idle's). omarchy-novad's daemon exposes more,
// differently-shaped state machines: `PopupPhase` (src/popup/mod.rs)
// has 8 states, and the OpenClaw `ConversationPhase`
// (src/conversation/mod.rs) has 4 -- neither maps 1:1 onto nova's 5
// distinct colors (silver/cyan/purple/gold/green; "warming" is a
// no-op alias for idle, so only 5 image assets were worth copying
// into `icons/`, not 6). This widget groups the extra states onto
// nova's existing palette by what each state *means* for the icon to
// communicate, rather than inventing new artwork nova never had:
//
//   nova asset (color)         | popupPhase        | conversationPhase
//   ----------------------------|--------------------|-------------------
//   tray-idle.png    (silver)   | idle               | --
//                                | confirming (*)     | confirming (*)
//   tray-listening.png (cyan)   | listening          | listening
//   tray-recording.png (purple) | recording          | --
//   tray-transcribing.png (gold)| transcribing       | thinking
//                                | classifying        |
//                                | handing_off        |
//   tray-typing.png (green)     | ready              | speaking
//
// Reasoning per group:
//   - idle: direct match, nothing is happening.
//   - listening: direct match both places -- mic armed, waiting to
//     hear something (nova's "Listening for wake word" is the same
//     concept popupPhase's "listening" and conversationPhase's
//     "listening" both describe: waiting on the mic before capture
//     starts in earnest).
//   - recording: direct match -- voxtype is actively capturing the
//     utterance. No conversationPhase equivalent (the conversation
//     loop's mic capture doesn't have a distinct "recording vs.
//     listening" split the way the popup pipeline does).
//   - transcribing/classifying/handing_off, and thinking: grouped
//     under gold ("processing") because they're all the same kind of
//     moment from a glance-at-the-bar perspective -- the daemon is
//     doing automatic work with no interaction needed, whether that's
//     ASR (transcribing, nova's own gold state), local-LLM intent
//     routing (classifying -- nova never had this step at all;
//     intent classification didn't exist in nova-npu, see
//     src/classify/mod.rs), waiting on an OpenClaw handoff
//     (handing_off), or the conversation loop's LLM formulating a
//     reply (thinking). All four are "please wait, nothing to do yet".
//   - ready and speaking: grouped under green, nova's "typing" color.
//     nova's "typing" was the finishing step -- the transcribed text
//     actively being keystroked into the focused window. omarchy-novad
//     never types live (see popup::PopupState's doc comments -- text
//     is shown for the user to Insert by hand), so there's no literal
//     equivalent, but "ready" (text fully processed, about to be
//     delivered) and "speaking" (TTS actively delivering the reply)
//     are both the same *kind* of moment nova's "typing" was: the
//     result is being delivered right now. Reusing green here also
//     matches the color this widget already used for both before this
//     redesign (see the pre-existing `dotColor` property below).
//   - confirming (both popup and conversation): the one state with no
//     nova equivalent at all -- nova never paused for a "does this
//     look good?" approval, so there's no color in its palette that
//     means "waiting on you". Rather than invent tray art nova never
//     had, this reuses the idle (silver) sphere as the base -- the mic
//     genuinely is idle during a popup confirmation -- and adds a
//     small pulsing yellow ring around it (`needsAttention` below,
//     OmarchyTheme.yellow, matching the yellow this widget already
//     used for "confirming" pre-redesign). The ring is new UI chrome,
//     not ported artwork, which is exactly why it comes from
//     OmarchyTheme rather than from `icons/`.
//
// One deliberate departure from nova: nova's tray icon was a static
// image swap with no animation (Electron's `tray.setImage()` is a
// one-shot swap; see main.js's `updateTrayState()`). This widget's
// original placeholder pulsed its dot while busy, and that liveness
// cue is kept on the ported icon too (`SequentialAnimation on
// opacity`, gated on `root.busy`) -- this is an inline bar icon seen
// constantly out of the corner of the eye, not a tray icon tucked away
// in a menu users only glance at occasionally, so a subtle "yes, this
// is live" pulse earns its keep here in a way it didn't for nova.
//
// ── Context menu: kept / adapted / dropped from nova's tray menu ────
//
// nova's menu (electron/main.js's buildContextMenuTemplate()):
//   Start Recording | --- | Config | OpenClaw Chat | Training |
//   Setup Wizard | --- | Quit Nova
//
// This widget's menu:
//   OpenClaw Chat (toggles Start/Stop) | --- | Open Config File | ---
//   | Stop Listening | Quit omarchy-novad… (confirm-armed)
//
//   - "OpenClaw Chat": KEPT, adapted into a toggle. Direct real
//     equivalent: `omarchy-novad converse start`/`stop`
//     (Service.qml's startConversation()/stopConversation(), added
//     alongside this redesign -- stopConversation() already existed
//     for the quick-status popup's "Stop conversation" button).
//     ConversationPanel (Overlay.qml) is a permanent docked window
//     (always visible, chat box always present -- typing in it starts
//     the loop via `converse start --text`), so this item just toggles
//     the daemon loop itself; the label/action toggles based on
//     `conversationActive` rather than always being "start".
//   - "Config": ADAPTED to "Open Config File". There's no dedicated
//     config UI/window anywhere in this project (unlike nova, which
//     had `createConfigWindow()`) -- everything lives in one real
//     file, `~/.config/omarchy-novad/config.toml` (see plugin/
//     README.md's Configuration section). `xdg-open` on that path is
//     the closest real equivalent, and is an established pattern in
//     this codebase already (see src/router/web.rs's OPEN intent).
//   - "Start Recording": DROPPED. There is no CLI subcommand or IPC
//     action anywhere in src/main.rs's `Command` enum that manually
//     triggers a one-shot recording outside the wake word -- `detect`
//     is a long-running daemon, not something with an on-demand
//     "record once" hook the way nova's Electron app exposed over its
//     own `/api/v1/trigger` HTTP endpoint. The closest *real*
//     capability that manually kicks off voice capture is
//     `converse start`, which is already surfaced above as its own
//     "OpenClaw Chat" item -- a second menu entry pointing at the same
//     command under nova's old name would be a confusing duplicate,
//     not a distinct feature, and per this task's constraints, no new
//     backend capability was invented to give "Start Recording" a
//     real one-shot-dictation target of its own.
//   - "Training": DROPPED, no adaptation. src/wake/mod.rs's module
//     doc comment says this outright: "Only stock phrases are
//     supported — no custom wake-word training. nova-npu had its own
//     trainer (wake/trainer.py, ~1.1k lines)" and it was deliberately
//     not carried over in favor of openWakeWord's own pretrained
//     models. There's nothing left to adapt this to.
//   - "Setup Wizard": DROPPED. `plugin/setup` is real and does
//     genuinely useful work (installs/detects the systemd units), but
//     it's a one-time, terminal-oriented install script -- colored
//     `info`/`warn`/`fail` output meant to be read in a shell, `set
//     -e`, env-var overrides for machine-specific detection -- not an
//     interactive "wizard" a casual bar-menu click should fire
//     silently in the background with nowhere for its output to go.
//     The README already documents running it by hand once after
//     `omarchy plugin add`; re-running it from here would add a
//     footgun (a silent mid-session systemd unit rewrite/restart) for
//     no real benefit over just running it in a terminal again.
//   - "Quit Nova" -> split into "Stop Listening" (kept, scoped down)
//     and "Quit omarchy-novad…" (kept, gated behind confirmation).
//     nova's "Quit" stopped one systemd unit babysitting a single
//     Electron app (`nova-tray.service`) and quit that one process --
//     low-stakes, instantly relaunchable from a desktop icon. This
//     project's daemon is three separate systemd `--user` units
//     (`omarchy-novad-detect`/`-serve`/`-tts`, see plugin/systemd/),
//     and stopping all three means losing wake-word listening,
//     classification, *and* the conversation loop's TTS until someone
//     manually runs `systemctl --user start` on each -- a much bigger,
//     harder-to-undo action than nova's single-app quit, and not
//     something that belongs one accidental click away in a casual bar
//     menu. So: "Stop Listening" is the new, low-stakes default action
//     for "I want quiet for a while" -- it stops only
//     `omarchy-novad-detect.service` (just the wake-word listener;
//     `serve`/`tts` keep running so nothing needs reloading when
//     listening resumes), no confirmation needed since it's cheap and
//     obviously reversible with one `systemctl --user start` command.
//     The actual full-shutdown equivalent of nova's "Quit" is kept too
//     (stops all three units) since sometimes that's genuinely what's
//     wanted (freeing the NPU/GPU, debugging, closing a laptop lid for
//     a while), but gated behind a click-to-arm confirmation
//     (`quitArmed`, see below) rather than firing on the first click,
//     given how much more disruptive it is here than it was for nova.

import QtQuick
import Quickshell
import Quickshell.Io
import Quickshell.Wayland
import qs.Ui

Panel {
    id: root

    // `moduleName` is injected by the host bar (see shell/plugins/bar/
    // Bar.qml's `injectProps`) to this widget's canonical plugin id;
    // this default only matters for the (unsupported) case of loading
    // this file outside the bar host, e.g. a qmllint pass.
    moduleName: "io.github.tslove923.omarchy-novad"
    ipcTarget: "omarchy-novad"

    // Service.qml is mounted once per session; this widget is mounted
    // once per monitor, same relationship the hass plugin's Panel.qml
    // documents: "Widgets reach them through `bar.shell.serviceFor(...)`".
    readonly property var novad: bar && bar.shell ? bar.shell.serviceFor(root.moduleName) : null
    readonly property bool serviceReady: novad !== null

    readonly property string popupPhase: serviceReady ? novad.popupPhase : "idle"
    readonly property bool conversationActive: serviceReady && novad.conversationActive
    readonly property string conversationPhase: serviceReady ? novad.conversationPhase : ""
    readonly property string conversationPendingText: serviceReady ? novad.conversationPendingText : ""

    // "Busy" whenever there's anything happening worth a glance: a
    // popup phase other than idle, or an active OpenClaw conversation.
    readonly property bool busy: popupPhase !== "idle" || conversationActive

    // Whether the daemon is waiting on the user specifically (vs. just
    // busy doing automatic work) -- see the icon-state mapping comment
    // above for why this doesn't get its own ported sphere color.
    readonly property bool needsAttention: conversationActive
        ? conversationPhase === "confirming"
        : popupPhase === "confirming"

    // See the icon-state mapping table in this file's header comment.
    readonly property string iconState: {
        if (conversationActive) {
            switch (conversationPhase) {
            case "listening": return "listening";
            case "confirming": return "idle";
            case "thinking": return "transcribing";
            case "speaking": return "typing";
            default: return "listening";
            }
        }
        switch (popupPhase) {
        case "listening": return "listening";
        case "recording": return "recording";
        case "transcribing": return "transcribing";
        case "classifying": return "transcribing";
        case "handing_off": return "transcribing";
        case "confirming": return "idle";
        case "ready": return "typing";
        default: return "idle";
        }
    }

    readonly property string iconSource: "icons/tray-" + iconState + ".png"

    // Themed (not ported) dot color for the quick-status popup's
    // header line below -- unchanged from before this redesign. Kept
    // distinct from the bar button's ported sphere icon: this dot
    // lives inside the detail card, not on the bar icon itself.
    readonly property color dotColor: {
        if (conversationActive) {
            switch (conversationPhase) {
            case "listening": return OmarchyTheme.accent;
            case "confirming": return OmarchyTheme.yellow;
            case "thinking": return OmarchyTheme.magenta;
            case "speaking": return OmarchyTheme.green;
            default: return OmarchyTheme.accent;
            }
        }
        switch (popupPhase) {
        case "recording": return OmarchyTheme.red;
        case "transcribing": return OmarchyTheme.accent;
        case "classifying": return OmarchyTheme.magenta;
        case "handing_off": return OmarchyTheme.accent;
        case "confirming": return OmarchyTheme.yellow;
        case "ready": return OmarchyTheme.green;
        default: return OmarchyTheme.muted;
        }
    }

    readonly property string statusLabel: {
        if (conversationActive) {
            switch (conversationPhase) {
            case "listening": return "Listening…";
            case "confirming": return "Confirming…";
            case "thinking": return "Thinking…";
            case "speaking": return "Speaking…";
            default: return "Conversing";
            }
        }
        switch (popupPhase) {
        case "listening": return "Listening…";
        case "recording": return "Recording…";
        case "transcribing": return "Transcribing…";
        case "classifying": return "Thinking…";
        case "handing_off": return "Asking OpenClaw…";
        case "confirming": return "Confirm pending";
        case "ready": return "Ready to insert";
        default: return "Idle";
        }
    }

    // Local UI-only state for the context menu -- not part of the
    // shared Panel `opened` lifecycle (that's the quick-status popup's
    // job below) since a bar widget only gets one `opened`/IPC-managed
    // popout from the base class, and the two surfaces need to be
    // independently toggleable from left- vs. right-click.
    property bool contextMenuOpen: false
    // Click-to-arm confirmation for the "Quit omarchy-novad…" row --
    // see this file's header comment for why a full quit here is
    // gated where nova's single-app "Quit Nova" never needed to be.
    property bool quitArmed: false

    function closeContextMenu() {
        contextMenuOpen = false;
        quitArmed = false;
    }

    function openConfigFile() {
        const home = Quickshell.env("HOME") || "";
        _openConfigProcess.command = ["xdg-open", home + "/.config/omarchy-novad/config.toml"];
        _openConfigProcess.running = true;
    }
    property Process _openConfigProcess: Process { running: false }

    // Stops only the wake-word listener -- `serve`/`tts` are left
    // running (see header comment). Resuming is a plain `systemctl
    // --user start omarchy-novad-detect` from a terminal; this widget
    // doesn't try to track live systemd unit state to offer a
    // one-click "resume" here too, matching its existing scope as a
    // small companion to the full overlay rather than a systemd
    // control panel.
    function stopListening() {
        _stopListeningProcess.command = ["systemctl", "--user", "stop", "omarchy-novad-detect.service"];
        _stopListeningProcess.running = true;
    }
    property Process _stopListeningProcess: Process { running: false }

    // Stops all three units. `omarchy-novad-tts.service` may not be
    // installed at all on a machine that skipped the optional Kokoro
    // TTS setup (see plugin/README.md's Install section) -- systemctl
    // still stops the other two units fine in that case; the tts
    // failure is expected and harmless.
    function quitOmarchyNovad() {
        _quitProcess.command = ["systemctl", "--user", "stop",
            "omarchy-novad-detect.service", "omarchy-novad-serve.service", "omarchy-novad-tts.service"];
        _quitProcess.running = true;
    }
    property Process _quitProcess: Process { running: false }

    implicitWidth: iconWrap.width + 12
    // Must match what every first-party module gets from WidgetButton
    // (`implicitHeight: barSize`, i.e. `bar.barSize`/Style.bar.sizeHorizontal)
    // -- the bar's Row lays modules out top-aligned with no cross-axis
    // centering of its own (plain QtQuick Row, not RowLayout), so a
    // shorter implicitHeight here sits flush with the row's top edge
    // instead of centered in the bar. This was hardcoded to 22 (the
    // icon's own pixel size) before, which is smaller than the bar's
    // actual height and is exactly why the icon rendered skewed toward
    // the top of the bar.
    implicitHeight: root.bar ? root.bar.barSize : 26

    // ── Bar button: the ported sphere icon (see header comment for
    //    the state->asset mapping), plus a themed attention ring for
    //    states nova's tray never had. ──
    Item {
        id: iconWrap
        anchors.centerIn: parent
        width: 22
        height: 22

        Rectangle {
            id: attentionRing
            anchors.centerIn: parent
            width: 20; height: 20
            radius: width / 2
            color: "transparent"
            border.width: 2
            border.color: OmarchyTheme.yellow
            visible: root.needsAttention

            SequentialAnimation on opacity {
                running: attentionRing.visible
                loops: Animation.Infinite
                NumberAnimation { to: 0.35; duration: 500 }
                NumberAnimation { to: 1.0; duration: 500 }
            }
        }

        Image {
            id: icon
            anchors.centerIn: parent
            width: 16; height: 16
            source: root.iconSource
            fillMode: Image.PreserveAspectFit
            smooth: true
            mipmap: true

            SequentialAnimation on opacity {
                running: root.busy
                loops: Animation.Infinite
                NumberAnimation { to: 0.55; duration: 600 }
                NumberAnimation { to: 1.0; duration: 600 }
            }
        }
    }

    MouseArea {
        anchors.fill: parent
        acceptedButtons: Qt.LeftButton | Qt.RightButton
        cursorShape: Qt.PointingHandCursor
        onClicked: (mouse) => {
            if (mouse.button === Qt.RightButton) {
                root.close();
                root.contextMenuOpen = !root.contextMenuOpen;
                if (!root.contextMenuOpen) root.quitArmed = false;
            } else {
                root.closeContextMenu();
                root.toggle();
            }
        }
    }

    // ── Quick-status popup -- self-managed layer-shell surface, same
    //    pattern PopupCard.qml/ConversationPanel.qml use (a plain
    //    `PanelWindow` child, gated on `root.opened` from the `Panel`
    //    base instead of the host's own KeyboardPanel dropdown helper
    //    -- kept intentionally simple since this popup only ever shows
    //    a couple of lines of status and two buttons). Docks near the
    //    top-right corner rather than tracking the bar icon's exact
    //    position (`qs.Ui.KeyboardPanel` does that properly for
    //    first-party widgets, at the cost of real integration with the
    //    bar's own popout-coordination internals) -- reasonable for a
    //    single small third-party widget, revisit if that ever feels
    //    imprecise in practice. Left-click only; right-click opens the
    //    separate context menu below instead. ──
    PanelWindow {
        id: statusPopup
        visible: root.opened

        anchors { top: true; right: true }
        color: "transparent"
        exclusionMode: ExclusionMode.Ignore

        WlrLayershell.namespace: "omarchy-novad-bar-status"
        WlrLayershell.layer: WlrLayer.Overlay
        WlrLayershell.keyboardFocus: WlrKeyboardFocus.None

        implicitWidth: card.width + 24
        implicitHeight: card.height + 24

        mask: Region {
            x: card.x
            y: card.y
            width: card.width
            height: card.height
        }

        Rectangle {
            id: card
            width: 280
            implicitHeight: column.implicitHeight + 24
            height: implicitHeight
            x: 12
            y: 12
            radius: 10
            color: OmarchyTheme.background

            MouseArea { anchors.fill: parent } // swallow clicks so they don't fall through

            Column {
                id: column
                width: parent.width - 28
                x: 14
                y: 12
                spacing: 8

                Row {
                    spacing: 8
                    Rectangle {
                        width: 8; height: 8; radius: 4
                        color: root.dotColor
                        anchors.verticalCenter: parent.verticalCenter
                    }
                    Text {
                        text: root.statusLabel
                        color: OmarchyTheme.foreground
                        font.pixelSize: 13
                        font.weight: Font.Medium
                        anchors.verticalCenter: parent.verticalCenter
                    }
                }

                Text {
                    width: parent.width
                    text: root.conversationActive && root.conversationPendingText.length > 0
                        ? root.conversationPendingText
                        : (root.serviceReady && root.novad.popupText.length > 0 ? root.novad.popupText : "Nothing pending.")
                    color: OmarchyTheme.muted
                    font.pixelSize: 12
                    wrapMode: Text.Wrap
                    maximumLineCount: 4
                    elide: Text.ElideRight
                }

                Row {
                    spacing: 6
                    anchors.right: parent.right

                    PopupButton {
                        label: "Dismiss"
                        tint: OmarchyTheme.red
                        visible: root.popupPhase !== "idle"
                        onClicked: if (root.serviceReady) root.novad.respond("deny")
                    }
                    PopupButton {
                        label: "Stop conversation"
                        tint: OmarchyTheme.red
                        visible: root.conversationActive
                        onClicked: if (root.serviceReady) root.novad.stopConversation()
                    }
                }
            }
        }
    }

    // ── Context menu -- the ported nova tray menu, right-click only.
    //    Same self-managed-PanelWindow shape as statusPopup above
    //    (see that comment for why); a separate surface rather than a
    //    second mode of the same card since nova's menu and this
    //    widget's quick-status card serve genuinely different
    //    purposes (menu = occasional actions; card = at-a-glance
    //    status + the two most common one-click actions) and were
    //    never meant to compete for the same click. ──
    PanelWindow {
        id: contextMenu
        visible: root.contextMenuOpen

        anchors { top: true; right: true }
        color: "transparent"
        exclusionMode: ExclusionMode.Ignore

        WlrLayershell.namespace: "omarchy-novad-bar-menu"
        WlrLayershell.layer: WlrLayer.Overlay
        WlrLayershell.keyboardFocus: WlrKeyboardFocus.None

        implicitWidth: menuCard.width + 24
        implicitHeight: menuCard.height + 24

        mask: Region {
            x: menuCard.x
            y: menuCard.y
            width: menuCard.width
            height: menuCard.height
        }

        Rectangle {
            id: menuCard
            width: 240
            implicitHeight: menuColumn.implicitHeight + 16
            height: implicitHeight
            x: 12
            y: 12
            radius: 10
            color: OmarchyTheme.background

            MouseArea { anchors.fill: parent } // swallow clicks so they don't fall through

            Column {
                id: menuColumn
                width: parent.width - 16
                x: 8
                y: 8
                spacing: 2

                ContextMenuItem {
                    label: root.conversationActive ? "Stop OpenClaw Chat" : "OpenClaw Chat"
                    tint: root.conversationActive ? OmarchyTheme.red : OmarchyTheme.foreground
                    onClicked: {
                        if (root.serviceReady) {
                            if (root.conversationActive) root.novad.stopConversation();
                            else root.novad.startConversation();
                        }
                        root.closeContextMenu();
                    }
                }

                Rectangle { width: parent.width; height: 1; color: OmarchyTheme.muted; opacity: 0.25 }

                ContextMenuItem {
                    label: "Open Config File"
                    tint: OmarchyTheme.foreground
                    onClicked: {
                        root.openConfigFile();
                        root.closeContextMenu();
                    }
                }

                Rectangle { width: parent.width; height: 1; color: OmarchyTheme.muted; opacity: 0.25 }

                ContextMenuItem {
                    label: "Stop Listening"
                    tint: OmarchyTheme.foreground
                    onClicked: {
                        root.stopListening();
                        root.closeContextMenu();
                    }
                }

                ContextMenuItem {
                    label: root.quitArmed ? "Click again to confirm quit" : "Quit omarchy-novad…"
                    tint: OmarchyTheme.red
                    onClicked: {
                        if (root.quitArmed) {
                            root.quitOmarchyNovad();
                            root.closeContextMenu();
                        } else {
                            root.quitArmed = true;
                            quitDisarmTimer.restart();
                        }
                    }
                }
            }
        }

        // Disarms the "Quit" confirmation a few seconds after the
        // first click if it isn't followed up -- an armed-forever quit
        // button left behind in a menu someone forgot they opened
        // would be worse than requiring the two clicks again later.
        Timer {
            id: quitDisarmTimer
            interval: 4000
            onTriggered: root.quitArmed = false
        }
    }
}
