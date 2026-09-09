# voice-node

The ambient "light bleeding up from the bottom edge" voice visualizer,
ported verbatim (structure and tuning unchanged) from
[spencerbull/omarchy-omapilot](https://github.com/spencerbull/omarchy-omapilot)'s
`components/{VoiceNode,VoiceWave,ThinkingScanner}.qml` and
`components/{StateColor,StatePhrases}.js`, MIT licensed (see
`LICENSE.omapilot`, © 2026 Spencer Bull).

`VoiceNode.qml` is deliberately presentational -- see its own doc
comment -- taking a simple `phase` ("dormant" | "listening" |
"thinking" | "answering" | "error") plus a handful of display
properties (`transcript`, `status`, `speaking`, `playbackLevel`, ...)
and owning its own `PanelWindow`/layer-shell surface internally. That
made it portable as-is: nothing here references OmaPilot's own broker,
IPC, or store. The only shared dependency, `qs.Commons`, is the
Omarchy shell's own theme module (`Color`, `Style`), available to any
plugin -- not something specific to OmaPilot either.

`../VoiceVisualizer.qml` is omarchy-novad's own code (not ported):
it wires this component to `Service.qml`'s existing
`popupPhase`/`conversationPhase` state, mapping this project's phases
onto the four this component understands.

Not modified from the upstream source except this file and the
one-line attribution comment at the top of each copied file --
keeping that diff minimal makes pulling in upstream improvements a
plain file copy, not a re-port.
