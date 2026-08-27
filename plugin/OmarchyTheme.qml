// Reads the active Omarchy theme's colors.toml and exposes it as
// plain QML properties, live-reloading on theme changes.
//
// Path: ~/.local/state/omarchy/current/theme/colors.toml (the symlink
// omarchy-theme-set repoints on every `omarchy theme set` -- see
// $(which omarchy-theme-set)'s CURRENT_THEME_PATH). NOT
// ~/.config/omarchy/current/theme/ -- that path doesn't exist on this
// system; voxtype's own Theme.qml has a stale comment citing it,
// worth fixing there too.
//
// Format is flat TOML (`key = "value"`, no tables/arrays), so this
// hand-rolls a one-line-at-a-time parser rather than pulling in a TOML
// library -- there's no bundled QML TOML adapter in this Quickshell/Qt
// build, and the format here doesn't need one. Same FileView +
// watchChanges + onLoaded pattern as PopupState.qml/voxtype's
// StateReader.qml, just parsing TOML instead of JSON.
//
// Kept as this plugin's own singleton rather than switching to the
// host shell's `qs.Commons.Color` -- Color only exposes
// foreground/background/accent/urgent/muted (plus per-surface
// composed roles), with no equivalent of the red/green/yellow/magenta
// this popup's phase colors and AnimatedBorder's gradient need. This
// is a deliberate port of the standalone quickshell/OmarchyTheme.qml's
// color mapping, not a redesign -- see the plugin README for why.
//
// Usage:
//   Rectangle { color: OmarchyTheme.background }
//
// Falls back to Catppuccin Mocha (omarchy-novad's original hardcoded palette,
// nova's own look) when the file is missing or fails to parse, so a
// non-Omarchy system or a mid-write torn read never leaves the popup
// with blank/invalid colors.

pragma Singleton

import QtQuick
import Quickshell
import Quickshell.Io

QtObject {
    id: root

    readonly property string tomlPath: {
        const home = Quickshell.env("HOME") || "";
        return home + "/.local/state/omarchy/current/theme/colors.toml";
    }

    // ── Fallback palette (Catppuccin Mocha) -- also the initial value
    //    every property below starts at, so there's never a blank
    //    frame before the first FileView load completes. ──
    readonly property string fallbackBackground: "#1e1e2e"
    readonly property string fallbackForeground: "#cdd6f4"
    readonly property string fallbackAccent: "#89b4fa"
    readonly property string fallbackRed: "#f38ba8"
    readonly property string fallbackGreen: "#a6e3a1"
    readonly property string fallbackYellow: "#fab387"
    readonly property string fallbackMagenta: "#cba6f7"
    readonly property string fallbackMuted: "#6c7086"

    property string background: fallbackBackground
    property string foreground: fallbackForeground
    property string accent: fallbackAccent
    property string red: fallbackRed
    property string green: fallbackGreen
    property string yellow: fallbackYellow
    property string magenta: fallbackMagenta
    // colors.toml has no direct "muted"/dim equivalent guaranteed
    // across every theme's key set beyond the ones parsed below, so
    // this stays fallback-only rather than mapping to a TOML key that
    // might not carry the right meaning in every theme.
    readonly property string muted: fallbackMuted

    function resetToFallback() {
        background = fallbackBackground;
        foreground = fallbackForeground;
        accent = fallbackAccent;
        red = fallbackRed;
        green = fallbackGreen;
        yellow = fallbackYellow;
        magenta = fallbackMagenta;
    }

    // Parses `key = "value"` lines (the only form colors.toml uses --
    // see /usr/share/omarchy/themes/*/colors.toml for the full key
    // set this could read from) into a plain JS object. Ignores lines
    // that don't match rather than failing the whole parse -- a theme
    // adding new keys in the future shouldn't break older omarchy-novad
    // builds reading it.
    function parseFlatToml(text) {
        const result = {};
        const lines = text.split("\n");
        const re = /^\s*([A-Za-z_][A-Za-z0-9_]*)\s*=\s*"([^"]*)"\s*$/;
        for (const line of lines) {
            const m = re.exec(line);
            if (m) {
                result[m[1]] = m[2];
            }
        }
        return result;
    }

    property FileView _fileView: FileView {
        path: root.tomlPath
        watchChanges: true
        printErrors: false

        onLoaded: {
            try {
                const parsed = root.parseFlatToml(text());
                root.background = parsed.background || root.fallbackBackground;
                root.foreground = parsed.foreground || root.fallbackForeground;
                root.accent = parsed.accent || root.fallbackAccent;
                root.red = parsed.red || root.fallbackRed;
                root.green = parsed.green || root.fallbackGreen;
                root.yellow = parsed.yellow || root.fallbackYellow;
                root.magenta = parsed.magenta || root.fallbackMagenta;
            } catch (e) {
                root.resetToFallback();
            }
        }

        onLoadFailed: root.resetToFallback()
        onFileChanged: reload()
    }
}
