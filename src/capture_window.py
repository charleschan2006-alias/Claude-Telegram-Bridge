#!/usr/bin/env python3
"""tinyCTB capture helper: print the active window's X id iff it is a terminal.

Read-only. Spawns nothing. Prints the decimal X window id of the currently
active window to stdout and exits 0 when that window is a terminal emulator
(its WM_CLASS contains b"term", case-insensitively — gnome-terminal, xterm,
terminator, terminology, …; emulators whose class carries no "term", such as
kitty / Alacritty / konsole, are not matched and simply fall back to the held
path). Otherwise prints nothing and exits 1. Any Xlib error
also exits 1 — the caller treats a non-zero exit / empty stdout as "could not
capture" and simply does not remember a window.

Used by the daemon-side hook to remember which terminal an interactive Claude
session runs in, so a phone answer can later be XTEST-typed into its native
AskUserQuestion selector.
"""

import sys


def main():
    try:
        from Xlib import display, X
    except Exception:
        return 1
    try:
        d = display.Display()
        root = d.screen().root
        aw = d.intern_atom("_NET_ACTIVE_WINDOW")
        prop = root.get_full_property(aw, X.AnyPropertyType)
        if not prop or not prop.value:
            return 1
        wid = int(prop.value[0])
        if wid == 0:
            return 1
        win = d.create_resource_object("window", wid)
        wm_class = win.get_full_property(d.intern_atom("WM_CLASS"), X.AnyPropertyType)
        val = wm_class.value if wm_class else None
        if not val:
            return 1
        raw = bytes(val) if not isinstance(val, (bytes, bytearray)) else val
        if b"term" not in raw.lower():
            return 1
        sys.stdout.write(str(wid))
        sys.stdout.flush()
        return 0
    except Exception:
        return 1


if __name__ == "__main__":
    sys.exit(main())
