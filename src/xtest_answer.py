#!/usr/bin/env python3
"""tinyCTB XTEST helper: focus a terminal window, then type an option + Return.

Three small steps, invoked separately so the CALLER can re-check the DB row
between every one of them (the answer may be given at the keyboard at any time):

  xtest_answer.py focus  <x_window_id>
      Send an EWMH `_NET_ACTIVE_WINDOW` ClientMessage with source=2 (which Mutter
      honours as a direct user request) and wait up to ~1.5s for the focus to
      land. The only slow step; sends no keys.

  xtest_answer.py digits <x_window_id> <digits>
      Type the option digit(s). Never waits, never re-focuses: if the window is
      not the active one right now, nothing is typed (exit 4).

  xtest_answer.py enter  <x_window_id>
      Type Return — the one key that SUBMITS — under the same rule.

A window is a terminal iff its WM_CLASS contains b"term" (case-insensitively:
gnome-terminal, xterm, terminator, …). Both phases re-check it so keys never
land in whatever replaced the window.

Assumptions (documented as accepted limits): a SINGLE X display (capture and
inject use the same $DISPLAY), and a US-style layout where option digits are
unshifted. Multi-display or shifted-digit layouts fall outside this helper.

Exit codes:
  0  ok (focused, or at least one key handed to the server)
  2  bad arguments / no keycode for a required keysym
  3  the window is gone or is no longer a terminal
  4  the window is not the active one (focus: not in time; keys: right now)
  1  any other failure before the first key (no Xlib, no XTEST, …)

Whoever answers first wins: the caller only types after re-confirming the row is
still open, and the fork's own tool result is recorded authoritatively by the
PostToolUse hook — these keystrokes never finalise an answer themselves.
"""

import sys
import time


def _is_terminal(win, WC, X):
    wc = win.get_full_property(WC, X.AnyPropertyType)
    val = bytes(wc.value) if wc and wc.value else b""
    return b"term" in val.lower()


def do_focus(target):
    from Xlib import display, X, protocol

    d = display.Display()
    root = d.screen().root
    CL = d.intern_atom("_NET_CLIENT_LIST")
    AW = d.intern_atom("_NET_ACTIVE_WINDOW")
    WC = d.intern_atom("WM_CLASS")

    def clients():
        p = root.get_full_property(CL, X.AnyPropertyType)
        return [int(w) for w in p.value] if p and p.value else []

    def active():
        p = root.get_full_property(AW, X.AnyPropertyType)
        return int(p.value[0]) if p and p.value else None

    if target not in clients():
        return 3
    win = d.create_resource_object("window", target)
    if not _is_terminal(win, WC, X):
        return 3

    ev = protocol.event.ClientMessage(
        window=win, client_type=AW, data=(32, [2, X.CurrentTime, 0, 0, 0])
    )
    root.send_event(ev, event_mask=X.SubstructureRedirectMask | X.SubstructureNotifyMask)
    d.flush()
    deadline = time.time() + 1.5
    while time.time() < deadline:
        if active() == target:
            return 0
        time.sleep(0.05)
    return 0 if active() == target else 4


def _open(target):
    """Connect and validate: returns (d, X, active) or an exit code."""
    from Xlib import display, X

    d = display.Display()
    root = d.screen().root
    CL = d.intern_atom("_NET_CLIENT_LIST")
    AW = d.intern_atom("_NET_ACTIVE_WINDOW")
    WC = d.intern_atom("WM_CLASS")
    p = root.get_full_property(CL, X.AnyPropertyType)
    clients = [int(w) for w in p.value] if p and p.value else []
    # The window must still exist AND still be a terminal, or the keystrokes
    # would land in whatever took its place.
    if target not in clients:
        return 3
    win = d.create_resource_object("window", target)
    if not _is_terminal(win, WC, X):
        return 3

    def active():
        q = root.get_full_property(AW, X.AnyPropertyType)
        return int(q.value[0]) if q and q.value else None

    return d, X, active


def do_keys(target, keysyms):
    """Send `keysyms` to the focused window, which MUST be `target`.

    XTEST fake_input has no target-window argument — it types into whatever
    holds the keyboard focus. So this never waits and never re-focuses: the
    caller focused the window in the `focus` phase and re-checked its own
    authorization since; if the window is not active NOW, nothing is typed (4)
    and the caller decides. The active window is re-checked before EVERY key.

    NO keyboard grab, deliberately: an active XGrabKeyboard delivers key events
    — XTEST's synthetic ones included — to the GRABBING client (this script),
    not to the terminal that owns the window, so it would swallow these keys.

    Returns 0 once ANY key has been handed to the server (a later failure must
    not read as "nothing typed / retryable" — a retry would type again), and a
    non-zero code only when NO key was sent.
    """
    from Xlib.ext import xtest

    opened = _open(target)
    if isinstance(opened, int):
        return opened
    d, X, active = opened
    # Validate every keycode BEFORE emitting any key. NOTE: digits are typed by
    # their UNSHIFTED keycode — a US-style-layout assumption of this deployment.
    keycodes = [d.keysym_to_keycode(ks) for ks in keysyms]
    if any(kc == 0 for kc in keycodes):
        return 2
    started = False
    try:
        for kc in keycodes:
            if active() != target:
                return 0 if started else 4
            xtest.fake_input(d, X.KeyPress, kc)
            started = True
            d.sync()
            time.sleep(0.05)
            xtest.fake_input(d, X.KeyRelease, kc)
            d.sync()
            time.sleep(0.05)
        return 0
    except Exception:
        return 0 if started else 1


def main(argv):
    if len(argv) < 3:
        return 2
    mode = argv[1]
    try:
        target = int(argv[2])
    except Exception:
        return 2
    try:
        from Xlib import XK

        if mode == "focus" and len(argv) == 3:
            return do_focus(target)
        if mode == "digits" and len(argv) == 4:
            digits = argv[3]
            if not digits or not all(ch.isdigit() for ch in digits):
                return 2
            return do_keys(target, [XK.string_to_keysym(ch) for ch in digits])
        if mode == "enter" and len(argv) == 3:
            return do_keys(target, [XK.XK_Return])
        return 2
    except Exception:
        return 1


if __name__ == "__main__":
    sys.exit(main(sys.argv))
