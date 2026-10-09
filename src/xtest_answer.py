#!/usr/bin/env python3
"""tinyCTB XTEST helper: find, focus a terminal window, then type an option + Return.

Small steps, invoked separately so the CALLER can re-check the DB row between
every one of them (the answer may be given at the keyboard at any time).

<mark> is a one-off title the caller has just written through the asking
session's OWN pts (`OSC 2`). Only the session's terminal window — with the
session's tab in front — can show it, so it is the window's identity: `find`
locates the window by it, and every later step refuses to act unless the
window STILL shows it. Nothing remembered from the desktop decides where keys
go (a window captured from "whatever was active" once was another session's
terminal, and the keys went there — 2026-10-09).

  xtest_answer.py find <mark>
      Print the id of the ONE terminal window whose title is exactly <mark>,
      waiting up to ~1.5s for the terminal to apply it. None, or more than one:
      print nothing, exit 4 / 5.

  xtest_answer.py focus  <x_window_id> <mark>
      Send an EWMH `_NET_ACTIVE_WINDOW` ClientMessage with source=2 (which Mutter
      honours as a direct user request) and a REAL server timestamp, and wait up
      to ~3s for the focus to land. The only slow step; sends no keys.

      The timestamp matters: Mutter only switches WORKSPACE for an activation
      that carries a real timestamp (`allow_workspace_switch = timestamp != 0`).
      With `CurrentTime` (0) a window on another workspace is never focused — it
      merely gets `_NET_WM_STATE_DEMANDS_ATTENTION` — and this step used to fail
      with 4 whenever the session's terminal sat on a workspace other than the
      current one (real machine, 2026-10-08). The timestamp comes from a
      PropertyNotify round-trip on a private, unmapped window of our own; if that
      cannot be had, `CurrentTime` is still sent (the same-workspace case keeps
      working). The wait is generous because a workspace switch animates, and
      under load the focus was seen to land after more than 2s. Total: one X
      round-trip for the timestamp plus at most 3s of polling (monotonic clock).

  xtest_answer.py digits <x_window_id> <mark> <digits>
      Type the option digit(s). Never waits, never re-focuses: if the window is
      not the active one right now, or no longer shows <mark>, nothing is typed
      (exit 4 / 6). Both are re-checked before EVERY key.

  xtest_answer.py enter  <x_window_id> <mark>
      Type Return — the one key that SUBMITS — under the same rule.

  xtest_answer.py release <digits>
      Best-effort KeyRelease of those digit keys and Return, nothing else. The
      caller runs it after it had to KILL a key step at its deadline, which may
      have left a key pressed (an auto-repeating key would keep typing).

A window is a terminal iff its WM_CLASS contains b"term" (case-insensitively:
gnome-terminal, xterm, terminator, …). Both phases re-check it so keys never
land in whatever replaced the window.

Assumptions (documented as accepted limits): a SINGLE X display (capture and
inject use the same $DISPLAY), and a US-style layout where option digits are
unshifted. Multi-display or shifted-digit layouts fall outside this helper.

Exit codes:
  0  ok (find: printed the window; focus: focused and still marked; keys: EVERY
     key pressed and released)
  2  bad arguments / no keycode for a required keysym
  3  the window is gone or is no longer a terminal
  4  the window is not the active one (focus: not in time; keys: right now);
     find: no window shows the mark in time
  5  find: more than one window shows the mark
  6  the window no longer shows the mark (focus: at any point of its wait)
  7  keys: at least one key MAY have gone out, but not the whole sequence
     cleanly (focus / mark lost after it, or an error after a press) — never
     retry, never follow with Return; any key left down is released first
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


def _shows_mark(d, win, X, mark):
    """True iff the window's title is exactly `mark` (EWMH name, else WM_NAME)."""
    want = mark.encode("utf-8")
    for name in ("_NET_WM_NAME", "WM_NAME"):
        p = win.get_full_property(d.intern_atom(name), X.AnyPropertyType)
        if p is not None:
            val = p.value
            raw = val.encode("utf-8") if isinstance(val, str) else bytes(val)
            return raw == want
    return False


def do_find(mark):
    from Xlib import display, X

    d = display.Display()
    root = d.screen().root
    CL = d.intern_atom("_NET_CLIENT_LIST")
    WC = d.intern_atom("WM_CLASS")

    def marked():
        p = root.get_full_property(CL, X.AnyPropertyType)
        found = []
        for wid in (int(w) for w in p.value) if p and p.value else []:
            try:
                win = d.create_resource_object("window", wid)
                if _is_terminal(win, WC, X) and _shows_mark(d, win, X, mark):
                    found.append(wid)
            except Exception:
                continue  # a window that vanished mid-scan is not ours
        return found

    deadline = time.monotonic() + 1.5
    while True:
        found = marked()
        if len(found) == 1:
            sys.stdout.write(str(found[0]))
            sys.stdout.flush()
            return 0
        if len(found) > 1:
            return 5
        if time.monotonic() >= deadline:
            return 4
        time.sleep(0.03)


def do_focus(target, mark):
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
    if not _shows_mark(d, win, X, mark):
        return 6

    def activate(timestamp):
        # data.l[0]=2: source is a pager / direct user action; l[1]: a real
        # server timestamp so the WM may switch workspace (see the module doc).
        ev = protocol.event.ClientMessage(
            window=win, client_type=AW, data=(32, [2, timestamp, 0, 0, 0])
        )
        root.send_event(
            ev, event_mask=X.SubstructureRedirectMask | X.SubstructureNotifyMask
        )
        d.flush()

    def wait_active(seconds):
        # The mark is re-read on EVERY poll and once more on success: another
        # tab brought to the front during the wait retitles the window, and
        # focusing it then would be focusing someone else's tab.
        deadline = time.monotonic() + seconds
        while True:
            if not _shows_mark(d, win, X, mark):
                return 6
            if active() == target:
                return 0 if _shows_mark(d, win, X, mark) else 6
            if time.monotonic() >= deadline:
                return 4
            time.sleep(0.05)

    activate(_server_time(d, root, X))
    return wait_active(3.0)


def _server_time(d, root, X):
    """A real X server timestamp, or `CurrentTime` (0) if none can be had.

    The server stamps a PropertyNotify with its own time: change a property on
    a private, never-mapped window of ours and read the time off the event.
    Nothing else on the display is touched.

    `sync()` is a full round-trip (GetInputFocus): the server generates the
    PropertyNotify before that reply, and the client reads the event into its
    queue on the way to the reply. So once `sync()` returns the event is already
    queued; draining the queue never waits.
    """
    try:
        w = root.create_window(
            0, 0, 1, 1, 0, X.CopyFromParent, X.InputOutput, X.CopyFromParent,
            event_mask=X.PropertyChangeMask,
        )
    except Exception:
        return X.CurrentTime
    try:
        prop = d.intern_atom("_TINYCTB_TIME")
        w.change_property(prop, d.intern_atom("STRING"), 8, b"t")
        d.sync()
        # Only OUR property's NewValue counts: destroying a window queues a
        # PropertyDelete for it, and X reuses a freed window id, so a stale
        # event from an earlier call on the same connection would otherwise
        # match. Take the newest match.
        stamp = X.CurrentTime
        while d.pending_events():
            ev = d.next_event()
            if (
                ev.type == X.PropertyNotify
                and ev.window.id == w.id
                and ev.atom == prop
                and ev.state == X.PropertyNewValue
            ):
                stamp = max(stamp, ev.time)
        return stamp
    except Exception:
        return X.CurrentTime
    finally:
        try:
            w.destroy()
            d.flush()
        except Exception:
            pass


def _open(target, mark):
    """Connect and validate: returns (d, X, active, marked) or an exit code."""
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

    def marked():
        return _shows_mark(d, win, X, mark)

    return d, X, active, marked


def do_keys(target, mark, keysyms):
    """Send `keysyms` to the focused window, which MUST be `target` showing `mark`.

    XTEST fake_input has no target-window argument — it types into whatever
    holds the keyboard focus. So this never waits and never re-focuses: the
    caller focused the window in the `focus` phase and re-checked its own
    authorization since; if the window is not active NOW (4), or no longer
    shows the mark — another tab came to the front, the session retitled — (6),
    nothing more is typed and the caller decides. Both are re-checked before
    EVERY key.

    NO keyboard grab, deliberately: an active XGrabKeyboard delivers key events
    — XTEST's synthetic ones included — to the GRABBING client (this script),
    not to the terminal that owns the window, so it would swallow these keys.

    Returns 0 only when EVERY key was pressed and released; 7 once any key
    MAY have reached the server without the whole sequence completing (a
    later failure must not read as "nothing typed / retryable" — a retry would
    type again — nor as success, which would let a Return submit a prefix);
    and 4 / 6 / 1 only when NO key was sent. A key left down by an error is
    released before returning.
    """
    from Xlib.ext import xtest

    opened = _open(target, mark)
    if isinstance(opened, int):
        return opened
    d, X, active, marked = opened
    # Validate every keycode BEFORE emitting any key. NOTE: digits are typed by
    # their UNSHIFTED keycode — a US-style-layout assumption of this deployment.
    keycodes = [d.keysym_to_keycode(ks) for ks in keysyms]
    if any(kc == 0 for kc in keycodes):
        return 2
    started = False
    down = None
    try:
        for kc in keycodes:
            if active() != target:
                return 7 if started else 4
            if not marked():
                return 7 if started else 6
            # Flagged BEFORE the request: a raise inside fake_input may still
            # have queued the press.
            started = True
            down = kc
            xtest.fake_input(d, X.KeyPress, kc)
            d.sync()
            time.sleep(0.05)
            xtest.fake_input(d, X.KeyRelease, kc)
            d.sync()
            down = None
            time.sleep(0.05)
        return 0
    except Exception:
        return 7 if started else 1
    finally:
        if down is not None:
            try:
                xtest.fake_input(d, X.KeyRelease, down)
                d.sync()
            except Exception:
                pass


def do_release(keysyms):
    """KeyRelease each keysym's key; releasing a key that is up is a no-op."""
    from Xlib import display, X
    from Xlib.ext import xtest

    d = display.Display()
    for ks in keysyms:
        kc = d.keysym_to_keycode(ks)
        if kc:
            xtest.fake_input(d, X.KeyRelease, kc)
    d.sync()
    return 0


def main(argv):
    if len(argv) < 3:
        return 2
    mode = argv[1]
    try:
        if mode == "find" and len(argv) == 3:
            return do_find(argv[2]) if argv[2] else 2
        if mode == "release" and len(argv) == 3:
            if not argv[2] or not all(ch.isdigit() for ch in argv[2]):
                return 2
            from Xlib import XK

            return do_release([XK.string_to_keysym(ch) for ch in argv[2]] + [XK.XK_Return])
        if len(argv) < 4 or not argv[3]:
            return 2
        target = int(argv[2])
        mark = argv[3]
    except Exception:
        return 2
    try:
        from Xlib import XK

        if mode == "focus" and len(argv) == 4:
            return do_focus(target, mark)
        if mode == "digits" and len(argv) == 5:
            digits = argv[4]
            if not digits or not all(ch.isdigit() for ch in digits):
                return 2
            return do_keys(target, mark, [XK.string_to_keysym(ch) for ch in digits])
        if mode == "enter" and len(argv) == 4:
            return do_keys(target, mark, [XK.XK_Return])
        return 2
    except Exception:
        return 1


if __name__ == "__main__":
    sys.exit(main(sys.argv))
