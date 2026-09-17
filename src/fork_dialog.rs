//! Answering a background fork's NATIVE prompt from two surfaces at once.
//!
//! A background fork (`claude --bg`, or a daemon fork) runs its
//! AskUserQuestion and approval dialogs on a hidden pty. `claude attach
//! <id>` opens a window onto that pty, and — proven 2026-09-02 on a
//! throwaway session — attach is BIDIRECTIONAL: keystrokes written to the
//! attach client's pty reach the fork's dialog and answer it. So one
//! native dialog serves both surfaces the user's law demands
//! ("双向推送，谁先抢答算谁的"):
//!   - LOCAL: a `gnome-terminal` running `claude attach <id>` — real keys.
//!   - PHONE: this module drives a headless `claude attach <id>` in a pty
//!     it owns and injects the chosen option's keystrokes.
//!
//! Whoever completes an answer first wins; the dialog is gone for the loser.
//!
//! This never stops or signals the fork or any TUI — it only opens a new
//! window and speaks to the fork the exact way a person at the keyboard
//! would. That is what makes it safe where the SIGSTOP takeover was not
//! (see the memory note `terminal-takeover-impossible`).

use anyhow::{anyhow, Context, Result};
use std::os::unix::io::RawFd;
use std::path::PathBuf;
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

/// A fork id shortened for a window title, the way `claude` prints it.
fn short_id(session_id: &str) -> String {
    session_id.chars().take(8).collect()
}

/// The digits a person TYPES to pick 0-based `index`. Claude Code's
/// AskUserQuestion selector NUMBERS its options (`1. RED`, `2. GREEN`, …) and
/// typing the option's 1-based number selects it — VERIFIED live 2026-09-03 on
/// a real `claude attach` dialog (injected the digit for index 2 and the fork
/// answered "BLUE", the 3rd option). Arrow keys also navigate it, but typing
/// the number is ABSOLUTE: it does not depend on where any other attached
/// client left the highlight, which is exactly what the two-surface race
/// needs. An AskUserQuestion never has more than a handful of options, so the
/// number is a single digit.
pub(crate) fn option_digits(index: usize) -> Vec<u8> {
    (index + 1).to_string().into_bytes()
}

/// The full keystrokes to pick option `index`: its 1-based number, then
/// Enter, which submits the typed number. The phone injects single-select
/// only (approvals included — two options, allow/deny); multi-select keeps
/// the comma-reply path.
pub(crate) fn option_keystrokes(index: usize) -> Vec<u8> {
    let mut keys = option_digits(index);
    keys.push(b'\r');
    keys
}

// ---- the visible local window --------------------------------------------

/// Environment marker stamped on a window the DAEMON pops, so auto-close can
/// tell it apart from a `claude attach <short>` the USER opened to watch the
/// session. Only marked windows are closed — a window a person opened (and may
/// have answered in at the keyboard) is never yanked out from under them
/// (0.2.14 fix: 0.2.13 closed the user's own terminal after they answered in it).
pub(crate) const POPPED_MARKER: &str = "TINYCTB_ATTACH_POPPED=1";

/// The `gnome-terminal` argv that opens the fork's native dialog in a
/// window. Pure, so the wiring is testable without a display.
///
/// `claude attach` takes the SHORT 8-char job id (what `claude agents` prints as
/// `id`), NOT the full session UUID — passing the full UUID fails with "No job
/// matching …" and the attach exits immediately (the on-machine cause of both
/// the "flashing" local window and the phone-inject "no chrome" failure). So the
/// attach argument is `short_id(session_id)`.
///
/// The command is wrapped in `env <POPPED_MARKER> …` so the running
/// `claude attach` process carries the marker in its environ (its argv is
/// unchanged — `env` execs claude directly — so [`is_attach_client_argv`] still
/// matches). That marker is what lets [`close_attach_windows`] close only the
/// window the daemon popped, never one the user opened.
pub(crate) fn attach_window_argv(session_id: &str, claude_bin: &str) -> Vec<String> {
    vec![
        "--title".to_string(),
        format!("tinyCTB · 后台任务 {} 待答", short_id(session_id)),
        "--".to_string(),
        "env".to_string(),
        POPPED_MARKER.to_string(),
        claude_bin.to_string(),
        "attach".to_string(),
        short_id(session_id),
    ]
}

/// A hook or daemon may run under an environment that lost the X session;
/// fill in this machine's defaults for whatever is missing so the window
/// can reach the display.
fn fill_x_env(cmd: &mut Command) {
    let runtime_dir = std::env::var("XDG_RUNTIME_DIR")
        .unwrap_or_else(|_| format!("/run/user/{}", unsafe { libc::getuid() }));
    if std::env::var_os("DISPLAY").is_none() {
        cmd.env("DISPLAY", ":1");
    }
    if std::env::var_os("XAUTHORITY").is_none() {
        cmd.env("XAUTHORITY", format!("{runtime_dir}/gdm/Xauthority"));
    }
    if std::env::var_os("DBUS_SESSION_BUS_ADDRESS").is_none() {
        cmd.env(
            "DBUS_SESSION_BUS_ADDRESS",
            format!("unix:path={runtime_dir}/bus"),
        );
    }
}

/// The terminal emulator to pop. Tests point this at a stub via
/// TINYCTB_TEST_TERMINAL; production uses `gnome-terminal` (verified
/// available and able to launch from a clean daemon environment).
fn terminal_bin() -> PathBuf {
    #[cfg(test)]
    return PathBuf::from(
        std::env::var("TINYCTB_TEST_TERMINAL")
            .unwrap_or_else(|_| "/nonexistent/tinyctb-test-terminal-unset".to_string()),
    );
    #[cfg(not(test))]
    PathBuf::from("gnome-terminal")
}

/// The python-xlib focus helper, embedded so it ships with the binary (deploy
/// stays "build + copy the binary"). Written to the cache dir on use and run as
/// `python3 focus_attach.py <terminal> <args…>`; it snapshots the window list,
/// launches the terminal, and pulls the new window to the foreground with
/// keyboard focus — which a background daemon otherwise cannot get past Mutter's
/// focus-stealing prevention. See the module doc and `docs/approvals.md`.
#[cfg(not(test))]
const FOCUS_HELPER_PY: &str = include_str!("focus_attach.py");

/// Write the embedded focus helper to `~/.cache/tinyctb/focus_attach.py` (never
/// the code dir, never `/tmp`) and return its path. Rewrites only when the
/// on-disk copy differs, so concurrent pops don't thrash the file and an upgrade
/// still refreshes it. `None` if the cache dir can't be resolved or written —
/// the caller then pops the window WITHOUT focus assist.
#[cfg(not(test))]
fn materialize_focus_helper() -> Option<PathBuf> {
    let base = std::env::var_os("XDG_CACHE_HOME")
        .filter(|v| !v.is_empty())
        .map(PathBuf::from)
        .or_else(|| std::env::var_os("HOME").map(|h| PathBuf::from(h).join(".cache")))?;
    let dir = base.join("tinyctb");
    std::fs::create_dir_all(&dir).ok()?;
    let path = dir.join("focus_attach.py");
    let fresh = std::fs::read(&path)
        .map(|c| c == FOCUS_HELPER_PY.as_bytes())
        .unwrap_or(false);
    if !fresh {
        // Publish atomically: write a UNIQUE temp then rename onto the path. A
        // plain truncating write could hand a CONCURRENT pop's python a
        // half-written script (parse fail → no window); rename(2) within the
        // same dir is atomic, so any reader sees a whole file (Sol review).
        static SEQ: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
        let seq = SEQ.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        let tmp = dir.join(format!(
            "focus_attach.py.{}.{}.tmp",
            std::process::id(),
            seq
        ));
        std::fs::write(&tmp, FOCUS_HELPER_PY).ok()?;
        if std::fs::rename(&tmp, &path).is_err() {
            let _ = std::fs::remove_file(&tmp);
            return None;
        }
    }
    Some(path)
}

/// Python 3, absolute where this machine keeps it, else a PATH lookup. The
/// daemon's systemd `PATH` includes `/usr/bin`, so the bare name also resolves.
#[cfg(not(test))]
fn python3_bin() -> PathBuf {
    let abs = PathBuf::from("/usr/bin/python3");
    if abs.exists() {
        abs
    } else {
        PathBuf::from("python3")
    }
}

// ---- the interactive-session XTEST path (v0.2.17) -------------------------
//
// A background fork is answered through `claude attach`; an ORDINARY
// interactive session cannot be — its terminal's pty belongs to
// gnome-terminal, not to a `bg-pty-host` a client can attach. So its native
// AskUserQuestion selector is driven the way a person at the keyboard would:
// the daemon remembers which terminal X window the session runs in (captured
// on each locally typed prompt, rewritten only when it changed) and, on a phone tap, focuses that window
// and synthesizes the option digit + Return via XTEST. Both helpers are
// embedded python-xlib scripts, materialized to the cache dir on use exactly
// like the focus helper.

/// Reads `_NET_ACTIVE_WINDOW` and prints its id iff it is a terminal.
#[cfg(not(test))]
const CAPTURE_HELPER_PY: &str = include_str!("capture_window.py");
/// Focuses a terminal X window and types option digits + Return via XTEST.
#[cfg(not(test))]
const XTEST_HELPER_PY: &str = include_str!("xtest_answer.py");

/// Publish an embedded helper to `~/.cache/tinyctb/<filename>` atomically
/// (unique temp + rename, so a concurrent reader never sees a half-written
/// script), rewriting only when the on-disk copy differs. `None` if the cache
/// dir cannot be resolved or written — the caller then does nothing.
#[cfg(not(test))]
fn materialize_named_helper(filename: &str, content: &str) -> Option<PathBuf> {
    let base = std::env::var_os("XDG_CACHE_HOME")
        .filter(|v| !v.is_empty())
        .map(PathBuf::from)
        .or_else(|| std::env::var_os("HOME").map(|h| PathBuf::from(h).join(".cache")))?;
    let dir = base.join("tinyctb");
    std::fs::create_dir_all(&dir).ok()?;
    let path = dir.join(filename);
    let fresh = std::fs::read(&path)
        .map(|c| c == content.as_bytes())
        .unwrap_or(false);
    if !fresh {
        static SEQ: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
        let seq = SEQ.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        let tmp = dir.join(format!("{filename}.{}.{}.tmp", std::process::id(), seq));
        std::fs::write(&tmp, content).ok()?;
        if std::fs::rename(&tmp, &path).is_err() {
            let _ = std::fs::remove_file(&tmp);
            return None;
        }
    }
    Some(path)
}

/// Capture the X window id of the terminal the CURRENT process's session is
/// running in, by reading the active window — valid to call only when local
/// keyboard input is fresh, so the active window is provably this session's
/// terminal. Runs the read-only capture helper; `None` on any failure (no X,
/// no python-xlib, the active window is not a terminal, …), in which case the
/// caller simply does not remember a window and the phone falls back to the
/// held path for this session.
pub(crate) fn capture_active_terminal_window() -> Option<i64> {
    #[cfg(test)]
    return match std::env::var("TINYCTB_TEST_CAPTURE_WINDOW").ok().as_deref() {
        Some("none") | None => None,
        Some(v) => v.parse::<i64>().ok(),
    };
    #[cfg(not(test))]
    {
        let helper = materialize_named_helper("capture_window.py", CAPTURE_HELPER_PY)?;
        let mut cmd = Command::new(python3_bin());
        cmd.arg(&helper).stdin(Stdio::null()).stderr(Stdio::null());
        fill_x_env(&mut cmd);
        let output = cmd.output().ok()?;
        if !output.status.success() {
            return None;
        }
        String::from_utf8_lossy(&output.stdout)
            .trim()
            .parse::<i64>()
            .ok()
    }
}

/// The DB-side gates the XTEST injector consults between its steps. The row —
/// not the screen — is the only identity a terminal we do not own can give us,
/// so it is re-read before every step that matters.
pub(crate) struct XtestGates<'a> {
    /// Is this question still the session's OPEN, untouched row?
    pub(crate) still_open: &'a dyn Fn() -> bool,
    /// Atomically claim the delivery (stamp `delivered_at`) BEFORE the first
    /// key. `false` = another tap already claimed it, or the write failed — in
    /// both cases NOTHING is typed. Stamping first is what makes "keys were
    /// sent but the row still reads open" impossible: no later DB failure can
    /// re-offer a button that would type a second time.
    pub(crate) claim: &'a dyn Fn() -> bool,
    /// After the claim: is the row still unanswered (not settled by the
    /// keyboard / the hook)? Checked right before the Return.
    pub(crate) still_ours: &'a dyn Fn() -> bool,
    /// Undo the claim — called only when NO key was sent, so the button may be
    /// retried. Best-effort; a failed release leaves the row claimed (safe
    /// side: nothing types again, the keyboard still answers).
    pub(crate) release: &'a dyn Fn(),
}

/// One step of the XTEST helper (`focus` / `digits` / `enter`); `true` on exit 0.
#[cfg(not(test))]
fn run_xtest_step(args: &[&str]) -> bool {
    let Some(helper) = materialize_named_helper("xtest_answer.py", XTEST_HELPER_PY) else {
        return false;
    };
    let mut cmd = Command::new(python3_bin());
    cmd.arg(&helper)
        .args(args)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null());
    fill_x_env(&mut cmd);
    matches!(cmd.status(), Ok(status) if status.success())
}

#[cfg(test)]
thread_local! {
    /// The helper steps a test run "executed", in order.
    pub(crate) static XTEST_STEPS: std::cell::RefCell<Vec<String>> =
        const { std::cell::RefCell::new(Vec::new()) };
}

/// Test stub: records the step; `TINYCTB_TEST_XTEST` names a step that FAILS
/// (`focus` / `digits` / `enter`), `delivered` (or unset → nothing reachable)
/// keeps the old two-value contract for callers that only need an outcome.
#[cfg(test)]
fn run_xtest_step(args: &[&str]) -> bool {
    let step = args.first().copied().unwrap_or_default();
    let mode = std::env::var("TINYCTB_TEST_XTEST").unwrap_or_default();
    let ok = match mode.as_str() {
        "delivered" => true,
        "" | "unreachable" => false,
        failing => failing != step,
    };
    if ok {
        XTEST_STEPS.with(|s| s.borrow_mut().push(args.join(" ")));
    }
    ok
}

/// Type option `index` into an interactive session's native selector by
/// XTEST-ing its 1-based digit, then Return, into terminal X window
/// `x_window_id`. XTEST types into whatever holds the keyboard focus and we
/// cannot see the terminal's screen, so every step is fenced by the row:
///
/// `still_open` → focus (slow, no keys) → `still_open` → `claim` → digits →
/// beat → `still_ours` → Return.
///
/// A revocation before the claim types NOTHING (`Unreachable`, retryable). A
/// failed digits step sent no key: the claim is released, `Unreachable`. Once a
/// digit is out the outcome is `Delivered` whatever follows — if the keyboard
/// answered during the beat (`still_ours` false) the Return is simply withheld.
/// The PostToolUse hook records the session's own result authoritatively,
/// whichever surface won. Never `SubmitPending`: single-select, no Submit tab.
pub(crate) fn inject_option_via_xtest(
    x_window_id: i64,
    index: usize,
    gates: &XtestGates,
) -> InjectOutcome {
    if !(gates.still_open)() {
        return InjectOutcome::Unreachable;
    }
    let window = x_window_id.to_string();
    if !run_xtest_step(&["focus", &window]) {
        return InjectOutcome::Unreachable;
    }
    // The focus wait is the long step: re-read the row after it.
    if !(gates.still_open)() {
        return InjectOutcome::Unreachable;
    }
    // Claim BEFORE the first key; lose the claim → type nothing.
    if !(gates.claim)() {
        return InjectOutcome::Unreachable;
    }
    let digits = (index + 1).to_string();
    if !run_xtest_step(&["digits", &window, &digits]) {
        // Exit != 0 means NO key was sent (window not active / gone / …).
        (gates.release)();
        return InjectOutcome::Unreachable;
    }
    // A person keys "3 ⏎" with a beat; the selector needs it too.
    std::thread::sleep(XTEST_KEY_GAP);
    // The Return is the key that submits — only while the row is still ours.
    if (gates.still_ours)() {
        let _ = run_xtest_step(&["enter", &window]);
    }
    InjectOutcome::Delivered
}

/// The beat between the option digit and Return (zero in tests).
#[cfg(not(test))]
const XTEST_KEY_GAP: Duration = Duration::from_millis(250);
#[cfg(test)]
const XTEST_KEY_GAP: Duration = Duration::from_millis(0);

/// Open the fork's native dialog in a desktop window. The window closes when
/// `claude attach` exits (when the viewer detaches or the session ends).
/// Fire-and-forget: a failure to pop the window is not fatal — the phone
/// remains the other surface.
pub(crate) fn pop_attach_window(session_id: &str) -> Result<()> {
    // The window runs the SAME claude the daemon resolves (CLAUDE_BIN / a
    // wrapper), never a bare PATH lookup. An invalid CLAUDE_BIN is an ERROR, not
    // a fallback — better no window than one driving the wrong binary; the
    // caller logs it and the phone remains the other surface.
    let claude_bin = crate::claude::resolve_claude_binary()
        .context("resolve claude for attach window")?
        .path
        .to_string_lossy()
        .into_owned();
    let terminal = terminal_bin();
    let argv = attach_window_argv(session_id, &claude_bin);

    // Preferred (production): launch THROUGH the python-xlib focus helper so the
    // popped window comes to the foreground with keyboard focus. Mutter denies
    // focus to a window a background daemon maps (the "dialog flashed but has no
    // focus" report), and the helper steals it back with `_NET_ACTIVE_WINDOW`
    // source=2. The helper always spawns the terminal itself — even if X or
    // python-xlib is unavailable — so the window never depends on focus working;
    // only if `python3` cannot be launched AT ALL do we fall through to popping
    // the terminal directly.
    #[cfg(not(test))]
    if let Some(helper) = materialize_focus_helper() {
        let mut cmd = Command::new(python3_bin());
        cmd.arg(&helper)
            .arg(&terminal)
            .args(&argv)
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null());
        fill_x_env(&mut cmd);
        if cmd.spawn().is_ok() {
            return Ok(());
        }
    }

    let mut cmd = Command::new(&terminal);
    cmd.args(&argv)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null());
    fill_x_env(&mut cmd);
    cmd.spawn()
        .with_context(|| format!("pop attach window for {session_id}"))?;
    Ok(())
}

/// True when `argv` is exactly a `<claude> attach <short>` client — the popped
/// viewer window's process, and NOT the fork itself. The fork runs under
/// `bg-pty-host` with a long, totally different argv, so this narrow 3-element
/// shape can only ever match an attach client.
///
/// `argv[0]` matches either the EXACT claude path the pop resolved
/// (`claude_path` — the native/symlink case, where `execve` preserves argv[0]),
/// OR any argv[0] whose BASENAME is exactly `claude` — the real binary an
/// `exec`-type wrapper lands on. That covers both `exec /real/claude "$@"`
/// (argv[0] = `/real/claude`) and the common PATH form `exec claude "$@"`
/// (argv[0] = a bare `claude`, no slash). Requiring the whole basename to equal
/// `claude` keeps it precise: it excludes a stray `/tmp/notclaude` (basename
/// `notclaude`) and a bare `notclaude` — the false positive a loose
/// `ends_with("claude")` had (Sol review, rounds 1–3). It DOES also match this
/// same fork's own headless injector (`with_attach_pty`, another
/// `<claude> attach <short>`), which is harmless: by the time the answer is
/// recorded that injector has delivered, and SIGTERMing it just ends an already
/// finished pty. Residual, unclosable, documented as a known limit — stated
/// generally so it is complete by construction: ANY wrapper whose RUNNING
/// argv[0] is neither `claude_path` nor has basename `claude` won't be matched
/// (a leak, never a mis-kill). That covers an `exec` to a renamed / versioned /
/// `readlink`-resolved real binary (`claude.real`), a shebang wrapper with NO
/// `exec` (the kernel prepends the interpreter, so argv is not 3 elements), and
/// `exec -a <name>` that rewrites argv[0]. The common forms — native, symlink,
/// `exec /real/claude`, bare `exec claude` — are all covered.
fn is_attach_client_argv(argv: &[&[u8]], claude_path: &[u8], short: &str) -> bool {
    if argv.len() != 3 || argv[1] != b"attach" || argv[2] != short.as_bytes() {
        return false;
    }
    // basename(argv[0]) = the segment after the last '/', or the whole arg when
    // there is none (a bare `claude`). rsplit always yields at least one item.
    let basename = argv[0].rsplit(|b| *b == b'/').next().unwrap_or(argv[0]);
    argv[0] == claude_path || basename == b"claude"
}

/// True when this process's `/proc/<pid>/environ` carries the daemon's pop
/// marker — the environ is NUL-separated `KEY=VALUE` entries, and `env
/// <POPPED_MARKER> …` put an exact `TINYCTB_ATTACH_POPPED=1` entry there. Pure
/// over the raw environ bytes so it is testable without `/proc`.
fn environ_has_marker(environ: &[u8], marker: &[u8]) -> bool {
    environ.split(|b| *b == 0).any(|entry| entry == marker)
}

/// Close any popped `claude attach <short-id>` viewer window for this fork — the
/// window the DAEMON popped so a person could answer the ONE question is done
/// the moment the question settles, so it should not linger showing the fork's
/// ongoing output. Scans `/proc` for the attach CLIENT process (matched by its
/// exact argv via [`is_attach_client_argv`], never the fork) AND carrying the
/// [`POPPED_MARKER`] in its environ, then SIGTERMs it; `gnome-terminal` closes
/// the window as its child exits. The marker is the crucial guard: a
/// `claude attach <short>` the USER opened to watch the session has NO marker
/// and is left completely alone — never yanked out from under someone who
/// answered in it at the keyboard (0.2.14 fix). Best-effort: a missing `/proc`,
/// an unreadable cmdline/environ, or a failed kill is not fatal.
pub(crate) fn close_attach_windows(session_id: &str) {
    let short = short_id(session_id);
    // Match argv[0] against the SAME claude the pop resolved, so a `cc` /
    // `claude-wrapper` symlink is still closed and a stray `/tmp/notclaude`
    // isn't. If claude can't be resolved right now, skip closing — best-effort,
    // exactly like a failed pop (the window then lingers, never mis-kills).
    let Ok(resolved) = crate::claude::resolve_claude_binary() else {
        return;
    };
    let claude_path = resolved.path.to_string_lossy();
    let claude_path = claude_path.as_bytes();
    let Ok(entries) = std::fs::read_dir("/proc") else {
        return;
    };
    for entry in entries.flatten() {
        let name = entry.file_name();
        let Some(pid) = name.to_str().and_then(|s| s.parse::<i32>().ok()) else {
            continue;
        };
        let Ok(raw) = std::fs::read(format!("/proc/{pid}/cmdline")) else {
            continue;
        };
        // /proc/<pid>/cmdline is NUL-separated with a trailing NUL; drop ONLY
        // that trailing empty, keeping any (rare) internal empty arg so the
        // "exactly three args" shape stays strict (Sol review).
        let mut argv: Vec<&[u8]> = raw.split(|b| *b == 0).collect();
        if argv.last().is_some_and(|s| s.is_empty()) {
            argv.pop();
        }
        if !is_attach_client_argv(&argv, claude_path, &short) {
            continue;
        }
        // Only a window the DAEMON popped (carrying POPPED_MARKER in its environ)
        // is closed — NEVER a `claude attach <short>` the user opened themselves,
        // which has no marker. An unreadable environ means we can't prove it was
        // ours, so we leave it alone (fail safe: don't kill a maybe-user window).
        let Ok(environ) = std::fs::read(format!("/proc/{pid}/environ")) else {
            continue;
        };
        if !environ_has_marker(&environ, POPPED_MARKER.as_bytes()) {
            continue;
        }
        // SIGTERM the viewer; the fork (a different process tree) is untouched.
        unsafe {
            libc::kill(pid, libc::SIGTERM);
        }
    }
}

// ---- the headless phone-side injection ------------------------------------

/// How long to WAIT for `claude attach` to connect and the fork's dialog to
/// render before giving up, and how long to read the fork's next state after
/// the Enter. The connect budget is a CEILING, not a fixed sleep — the presence
/// check returns the instant the dialog's chrome appears (see `wait_for_chrome`),
/// so a generous ceiling only bounds a dialog that never shows. It is generous
/// because the real `claude attach` can take a second or more to paint (proven
/// on-machine 2026-09-03: the chrome surfaced ~2s after connect), and a check
/// that raced that render was the 0.2.11 phone-inject failure.
const ATTACH_CONNECT_WAIT: Duration = Duration::from_secs(8);
const ATTACH_SETTLE_WAIT: Duration = Duration::from_secs(3);

/// The selector's own footer hint, drawn ONLY while an AskUserQuestion dialog
/// is live (`Enter to select · ↑/↓ to navigate · Esc to cancel`) — once
/// answered the dialog collapses to `User answered Claude's questions: …`, so
/// this line is gone, which is what makes it a reliable "the dialog is still
/// up" signature (unlike the option labels, which echo into scrollback). It is
/// English and fixed, regardless of the question's own language. VERIFIED live
/// on a real `claude attach` dialog 2026-09-03 — the earlier `Select with
/// numbers` guess (read from a different, non-interactive selector component)
/// never matched the interactive dialog.
const SELECTOR_CHROME: &[u8] = b"Enter to select";

/// A person keys "3 ⏎" with a beat between the digit and Enter; leave the
/// same beat so the selector's input buffer holds the digit before Enter
/// reads it — Enter on an empty buffer would submit the DEFAULTS.
const SELECTOR_KEY_GAP: Duration = Duration::from_millis(200);

/// The default (connect, settle) waits — overridable in tests so a stub
/// attach need not burn the real seconds (up to an 8s connect ceiling plus a
/// 3s settle). The connect value is a CEILING: `wait_for_chrome` returns the
/// instant the dialog appears, so a live dialog costs only its render time.
fn default_waits() -> (Duration, Duration) {
    #[cfg(test)]
    if let Ok(ms) = std::env::var("TINYCTB_TEST_ATTACH_WAIT_MS") {
        let d = Duration::from_millis(ms.parse().unwrap_or(600));
        return (d, d);
    }
    (ATTACH_CONNECT_WAIT, ATTACH_SETTLE_WAIT)
}

/// What an injection attempt did.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum InjectOutcome {
    /// The keystrokes were put in front of a LIVE dialog: the selector chrome was
    /// present, and the option number + Enter were written to it. This does NOT
    /// claim the fork RECORDED them — the PostToolUse "answered" hook records the
    /// fork's OWN result authoritatively when the turn completes (whichever
    /// surface won the "谁先抢答" race, and exactly what it chose). It only says
    /// "delivered to a live dialog", so the phone button is consumed and need not
    /// retry. This is what lets the inject side stop scraping the pty to GUESS the
    /// outcome — the guess (and its unrecoverable false-positive) is gone.
    Delivered,
    /// (Multi-question, 0.2.16.) This tab's digit went into the dialog, but the
    /// batch could NOT be confirmed submitted: the tab bar was unreadable
    /// afterwards, or the walk onto Submit did not land there. The row is
    /// marked delivered (never retype the digit) but the button stays LIVE — a
    /// later tap on it, or on any delivered sibling once the whole batch is
    /// delivered, performs a submit-only recovery (`inject_submit_only`).
    SubmitPending,
    /// The dialog was never found: the selector chrome did not appear within the
    /// budget (already answered/closed, still connecting, or the attach failed).
    /// NOTHING was typed. The phone reports this as retryable and records nothing;
    /// the row stays open for another surface, and the hook settles it if an
    /// answer lands, else it expires.
    Unreachable,
}

/// Answer a background fork's native single-select dialog by option index,
/// but — when `verify_present` — ONLY if the selector is still showing AND
/// `still_authorized()` says THIS question is still the fork's open one, the
/// safety the "谁先抢答" design rests on. A pty child execs `claude attach
/// <id>`; the parent waits for the selector chrome, re-checks `still_authorized`
/// (the generic chrome cannot tell one question's dialog from the next's, so the
/// DB row's status is the identity check), and only then types the option's
/// number + Enter. With `verify_present` false it always injects — for the
/// manual command, where the caller vouches the dialog is up.
/// `multi` names the tab when the dialog is a multi-question one: the injector
/// navigates there and types the digit only (the dialog moves on by itself);
/// `None` is the single-question dialog, answered with digit + Enter.
pub(crate) fn inject_option(
    session_id: &str,
    index: usize,
    verify_present: bool,
    multi: Option<MultiTab>,
    still_authorized: impl Fn() -> bool,
) -> Result<InjectOutcome> {
    let (connect, settle) = default_waits();
    inject_option_timed_multi(
        session_id,
        index,
        verify_present,
        connect,
        SELECTOR_KEY_GAP,
        settle,
        multi,
        still_authorized,
    )
}

/// The manual-command path: inject keystrokes with no presence check.
pub(crate) fn inject_via_attach(session_id: &str, keystrokes: &[u8]) -> Result<()> {
    let (connect, settle) = default_waits();
    with_attach_pty(session_id, |master| {
        drain_capture(master, connect);
        write_all(master, keystrokes)?;
        drain_capture(master, settle);
        Ok(())
    })
}

/// One tab of a multi-question dialog to answer: the injector navigates to
/// tab `seq` (0-based, of `total`) before typing. `None` = an ordinary
/// single-question dialog, which has no tab bar.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct MultiTab {
    pub(crate) seq: usize,
    pub(crate) total: usize,
}

/// What a multi-question dialog's tab bar says on the current screen. The bar
/// is `← ☐ Color ☐ Size ✔ Submit →` (measured 2026-09-12): `☐` marks an
/// UNANSWERED tab, `☒` an ANSWERED one, and the tab painted with the highlight
/// background is the CURRENT one. Parsed from the LAST bar in the frame — a
/// forced repaint redraws it whole.
#[derive(Debug, PartialEq, Eq)]
pub(crate) struct TabBar {
    pub(crate) answered: usize,
    pub(crate) unanswered: usize,
    /// The current tab as painted, e.g. `☐ Color`, `☒ Size`, `✔ Submit`.
    pub(crate) current: Option<String>,
    /// The current tab's POSITION: how many question tabs (`☐`/`☒`) precede
    /// the highlighted one — 0-based, and equal to the tab count when the
    /// Submit tab is current. This is what proves a digit lands on tab `seq`;
    /// `☐` alone only says "some unanswered tab".
    pub(crate) current_index: Option<usize>,
}

const TAB_UNANSWERED: &[u8] = b"\xe2\x98\x90"; // ☐
const TAB_ANSWERED: &[u8] = b"\xe2\x98\x92"; // ☒
/// The start of the SGR that paints a background under the current tab —
/// `ESC [ 48 ;` — whatever colour depth follows (`2;r;g;b` truecolor when
/// COLORTERM=truecolor, `5;n` in the 256-colour fallback). No other tab
/// carries a background, so the LAST such SGR in the bar marks the current
/// tab. Never match a full colour value: it depends on the environment the
/// attach client happens to run in.
const TAB_HIGHLIGHT_PREFIX: &[u8] = b"\x1b[48;";
const TAB_SUBMIT: &[u8] = b"Submit";
const KEY_LEFT: &[u8] = b"\x1b[D";
const KEY_RIGHT: &[u8] = b"\x1b[C";

fn rfind(hay: &[u8], needle: &[u8]) -> Option<usize> {
    hay.windows(needle.len()).rposition(|w| w == needle)
}

fn count(hay: &[u8], needle: &[u8]) -> usize {
    hay.windows(needle.len()).filter(|w| *w == needle).count()
}

/// Read the tab bar from a captured frame (see [`TabBar`]). `None` when no
/// tab bar is on screen — a single-question dialog, or nothing painted yet.
pub(crate) fn tab_bar_state(frame: &[u8]) -> Option<TabBar> {
    // Anchor on the LAST "Submit" (the Submit tab, or the `Submit answers` item
    // right under it) and look back over the bar, which sits within a few
    // hundred bytes before it.
    let end = rfind(frame, TAB_SUBMIT)? + TAB_SUBMIT.len();
    // Only THIS bar: it begins with the `←` arrow, so cut there — a capture
    // that holds several repaints must not have their tabs counted together.
    let bar_start = rfind(&frame[..end], "\u{2190}".as_bytes()).unwrap_or(end.saturating_sub(600));
    let seg = &frame[bar_start..end];
    let answered = count(seg, TAB_ANSWERED);
    let unanswered = count(seg, TAB_UNANSWERED);
    if answered + unanswered == 0 {
        return None;
    }
    // The current tab is the one painted with a BACKGROUND colour. Match the
    // SGR *prefix* `ESC [ 48 ;` — truecolor (`48;2;r;g;b`) or 256-colour
    // (`48;5;n`) alike. Measured 2026-09-12: the probes (a shell with
    // COLORTERM=truecolor) saw `48;2;177;185;249`, the daemon's attach (no
    // COLORTERM) got a 256-colour fallback, and an exact-bytes match found no
    // highlight at all → every phone tap "not the unanswered tab".
    let highlight_at = rfind(seg, TAB_HIGHLIGHT_PREFIX);
    let current = highlight_at.map(|at| {
        // Skip to the end of the background SGR itself (its `m`), whatever
        // colour value it carries, then the SGRs that follow it.
        let sgr_end = seg[at..]
            .iter()
            .position(|b| *b == b'm')
            .map(|p| at + p + 1)
            .unwrap_or(seg.len());
        let mut rest = &seg[sgr_end..];
        // The highlight is followed by more SGR codes (the black foreground)
        // before the tab's text; skip them.
        while rest.starts_with(b"\x1b[") {
            match rest.iter().position(|b| *b == b'm') {
                Some(p) => rest = &rest[p + 1..],
                None => break,
            }
        }
        let text_end = rest.iter().position(|b| *b == 0x1b).unwrap_or(rest.len());
        String::from_utf8_lossy(&rest[..text_end.min(32)])
            .trim()
            .to_string()
    });
    let current_index =
        highlight_at.map(|at| count(&seg[..at], TAB_ANSWERED) + count(&seg[..at], TAB_UNANSWERED));
    Some(TabBar {
        answered,
        unanswered,
        current,
        current_index,
    })
}

/// Repaint and read the tab bar, retrying a few times: right after a key the
/// dialog may still be redrawing, and an unreadable bar must never be taken
/// for "nothing left to do".
fn read_bar(master: RawFd, key_gap: Duration, tries: usize) -> Option<TabBar> {
    // Accumulate across tries: a slow repaint may straddle two reads, and the
    // parser takes the LAST bar in the buffer, so keeping earlier bytes can
    // only help. Each try holds the nudged size (see `force_repaint`) and then
    // waits long enough for a real `claude attach` to repaint.
    let mut acc = Vec::new();
    for _ in 0..tries {
        force_repaint(master);
        acc.extend(drain_capture(master, key_gap * 6));
        if let Some(bar) = tab_bar_state(&acc) {
            return Some(bar);
        }
    }
    None
}

/// Every tab is answered: walk `→` onto the Submit tab (it clamps there),
/// CONFIRM the bar now says Submit is current, then Enter — which returns all
/// the answers. Enter is never sent blind: if the bar cannot be read, or is not
/// on Submit (the dialog closed meanwhile), nothing is pressed and the next
/// tap's opening check submits instead. Always `Delivered` — the digit that
/// brought us here is already in the dialog.
fn submit_batch(
    master: RawFd,
    session_id: &str,
    tab: MultiTab,
    key_gap: Duration,
    settle_wait: Duration,
    still_authorized: &dyn Fn() -> bool,
) -> InjectOutcome {
    for _ in 0..tab.total {
        if write_all(master, KEY_RIGHT).is_err() {
            break;
        }
        drain_capture(master, key_gap);
    }
    // Bind the Enter to OUR batch: every one of exactly `total` tabs answered,
    // nothing unanswered, and the highlighted tab is the one PAST the last
    // question — Submit sits at index `total`. A later all-answered dialog of
    // another size fails this; one of the same size is caught by the
    // authorization re-check below (a new batch settles our row first).
    let on_our_submit = matches!(
        read_bar(master, key_gap, 3),
        Some(b)
            if b.answered == tab.total
                && b.unanswered == 0
                && b.current_index == Some(tab.total)
                && b.current.as_deref().is_some_and(|c| c.contains("Submit"))
    );
    if !on_our_submit {
        ilog(format!(
            "inject {}: all tabs answered but the bar is not our Submit -> Enter withheld -> SubmitPending",
            short_id(session_id),
        ));
        return InjectOutcome::SubmitPending;
    }
    if !still_authorized() {
        ilog(format!(
            "inject {}: revoked right before submit -> Enter withheld -> SubmitPending",
            short_id(session_id),
        ));
        return InjectOutcome::SubmitPending;
    }
    match write_all(master, b"\r") {
        Ok(()) => {
            ilog(format!(
                "inject {}: all {} tabs answered -> submitted -> Delivered",
                short_id(session_id),
                tab.total,
            ));
            drain_capture(master, settle_wait);
            InjectOutcome::Delivered
        }
        Err(err) => {
            ilog(format!(
                "inject {}: submit enter failed ({err}) -> SubmitPending",
                short_id(session_id),
            ));
            InjectOutcome::SubmitPending
        }
    }
}

/// Move a multi-question dialog to tab `seq`: `←`×total first (the bar CLAMPS
/// at the first tab — measured, no wrap — so this is a deterministic reset
/// whatever tab the fork or a person at the keyboard left it on), then
/// `→`×seq. Returns the bar as repainted after the move, so the caller can
/// verify it landed on an UNANSWERED tab before typing anything.
fn navigate_to_tab(master: RawFd, tab: MultiTab, key_gap: Duration) -> Result<Option<TabBar>> {
    // Keep every byte the arrows produce too: each arrow redraws the bar with
    // the highlight moved, and the parser reads the LAST bar, so the buffer
    // reflects where the highlight ended up even if the final repaint is slow.
    let mut acc = Vec::new();
    for _ in 0..tab.total {
        write_all(master, KEY_LEFT)?;
        acc.extend(drain_capture(master, key_gap));
    }
    for _ in 0..tab.seq {
        write_all(master, KEY_RIGHT)?;
        acc.extend(drain_capture(master, key_gap));
    }
    force_repaint(master);
    acc.extend(drain_capture(master, key_gap * 6));
    Ok(tab_bar_state(&acc))
}

/// Submit-only recovery for a multi-question dialog whose tabs are ALL already
/// answered — each digit delivered, or finished at the keyboard — but whose
/// batch was never confirmed submitted (`SubmitPending`): attach, and if the
/// bar shows exactly `total` tabs all answered, walk onto Submit and press
/// Enter (`submit_batch`, which binds and re-authorizes on its own). NOTHING is
/// ever retyped. `Unreachable` (retryable) when the dialog is not up, is not an
/// all-answered `total`-tab bar, or the bar cannot be read.
pub(crate) fn inject_submit_only(
    session_id: &str,
    total: usize,
    still_authorized: impl Fn() -> bool,
) -> Result<InjectOutcome> {
    let (connect, settle) = default_waits();
    with_attach_pty(session_id, |master| {
        let (intro, present) = wait_for_chrome(master, connect, true);
        if !present {
            ilog(format!(
                "submit-only {}: intro {}B no dialog -> Unreachable",
                short_id(session_id),
                intro.len(),
            ));
            return Ok(InjectOutcome::Unreachable);
        }
        if !still_authorized() {
            ilog(format!(
                "submit-only {}: not authorized -> Unreachable",
                short_id(session_id),
            ));
            return Ok(InjectOutcome::Unreachable);
        }
        let all_answered = matches!(
            read_bar(master, SELECTOR_KEY_GAP, 3),
            Some(b) if b.answered == total && b.unanswered == 0
        );
        if !all_answered {
            ilog(format!(
                "submit-only {}: not an all-answered {total}-tab bar -> Unreachable",
                short_id(session_id),
            ));
            return Ok(InjectOutcome::Unreachable);
        }
        Ok(submit_batch(
            master,
            session_id,
            MultiTab { seq: 0, total },
            SELECTOR_KEY_GAP,
            settle,
            &still_authorized,
        ))
    })
}

/// Test convenience: the single-question form of [`inject_option_timed_multi`].
#[cfg(test)]
pub(crate) fn inject_option_timed(
    session_id: &str,
    index: usize,
    verify_present: bool,
    connect_wait: Duration,
    key_gap: Duration,
    settle_wait: Duration,
    still_authorized: impl Fn() -> bool,
) -> Result<InjectOutcome> {
    inject_option_timed_multi(
        session_id,
        index,
        verify_present,
        connect_wait,
        key_gap,
        settle_wait,
        None,
        still_authorized,
    )
}

#[allow(clippy::too_many_arguments)]
pub(crate) fn inject_option_timed_multi(
    session_id: &str,
    index: usize,
    verify_present: bool,
    connect_wait: Duration,
    key_gap: Duration,
    settle_wait: Duration,
    multi: Option<MultiTab>,
    still_authorized: impl Fn() -> bool,
) -> Result<InjectOutcome> {
    with_attach_pty(session_id, |master| {
        if !verify_present {
            // The manual/vouched path: let attach paint, type the number +
            // Enter (the caller vouches the dialog is up).
            drain_capture(master, connect_wait);
            write_all(master, &option_digits(index))?;
            drain_capture(master, key_gap);
            write_all(master, b"\r")?;
            drain_capture(master, settle_wait);
            return Ok(InjectOutcome::Delivered);
        }
        // WAIT for the fork's dialog to render, then DELIVER the keystrokes into
        // it. We do NOT scrape the screen afterwards to guess whether the fork
        // "took" the answer — the PostToolUse hook records the fork's OWN result.
        // `wait_for_chrome` accumulates until the selector chrome appears — the
        // real `claude attach` can take a second or more to connect and paint
        // (on-machine 2026-09-03: ~2s), and the chrome is scrollback-safe on a
        // fresh forkpty, so accumulating cannot be fooled by history. No chrome
        // within the budget ⇒ no live dialog ⇒ Unreachable (retryable, nothing
        // typed).
        // A multi-question dialog may be parked on its Submit tab (no selector
        // hint): count that as present ONLY when we will verify the tab bar
        // before typing, i.e. for a batch tab. The single path never does.
        let (intro, present) = wait_for_chrome(master, connect_wait, multi.is_some());
        if !present {
            ilog(format!(
                "inject {}: intro {}B no chrome -> Unreachable",
                short_id(session_id),
                intro.len(),
            ));
            return Ok(InjectOutcome::Unreachable);
        }
        // A dialog is up — but is it OURS? `wait_for_chrome` may have waited
        // seconds, during which this fork could have ANSWERED our question and
        // moved on to the NEXT one, whose generic `Enter to select` chrome is
        // indistinguishable. The gate settles our row the instant a new native
        // question opens (`settle_stale_native_questions`), so re-checking the
        // DB row's status HERE — after the chrome is up, right before we commit
        // any key — REVOKES an in-flight injection whose question is gone: we
        // must NOT drive a different question's dialog. This DB status IS the
        // question-instance identity the chrome cannot give us. Nothing has been
        // typed yet, so this bail is a clean, retryable Unreachable.
        //
        // ACCEPTED RESIDUAL (documented inherent limit, not chased): there is a
        // microsecond window between this status read and the digit write below
        // in which — only if the fork answered THIS question locally AND the
        // next question's create/settle transaction committed AND that new dialog
        // rendered AND the OS descheduled us across all of it — a key could still
        // reach a different question. Closing it fully needs a cross-process
        // SQLite writer guard held across the pty writes; for a single-user tool
        // that theoretical race is out of scope. The authoritative hook still
        // records only what the fork actually took.
        if !still_authorized() {
            ilog(format!(
                "inject {}: question no longer the fork's open one -> Unreachable",
                short_id(session_id),
            ));
            return Ok(InjectOutcome::Unreachable);
        }
        if let Some(tab) = multi {
            // ---- a tab of a multi-question dialog (0.2.16) ----------------
            // First look at the bar as it is. If EVERY tab is already answered
            // — a person finished at the keyboard, or an earlier auto-submit
            // could not read the bar — the dialog is parked on Submit with
            // nothing left to type: just submit it.
            if let Some(bar) = read_bar(master, key_gap, 3) {
                if bar.unanswered == 0 && bar.answered == tab.total {
                    ilog(format!(
                        "inject {}: all {} tabs already answered -> submitting",
                        short_id(session_id),
                        tab.total,
                    ));
                    return Ok(submit_batch(
                        master,
                        session_id,
                        tab,
                        key_gap,
                        settle_wait,
                        &still_authorized,
                    ));
                }
            }
            // Navigate to OUR tab and read the bar back. It must be an N-tab
            // bar (ours); the highlighted tab must sit at POSITION `seq` (the
            // marker count before the highlight — `☐` alone would accept ANY
            // unanswered tab, so a person moving tabs at the keyboard during
            // our navigation could otherwise put the digit on the wrong one);
            // and that tab must still be UNANSWERED (`☐`) — answered at the
            // keyboard (`☒`), our digit would land on the next tab and answer
            // the WRONG question. Anything else: nothing typed, Unreachable
            // (retryable; the hook records what the fork actually took).
            let bar = navigate_to_tab(master, tab, key_gap)?;
            let on_our_unanswered_tab = matches!(
                &bar,
                Some(bar)
                    if bar.answered + bar.unanswered == tab.total
                        && bar.current_index == Some(tab.seq)
                        && bar.current.as_deref().is_some_and(|c| c.starts_with('☐'))
            );
            if !on_our_unanswered_tab {
                ilog(format!(
                    "inject {}: tab {}/{} is not the unanswered tab on screen ({bar:?}) -> Unreachable",
                    short_id(session_id),
                    tab.seq + 1,
                    tab.total,
                ));
                return Ok(InjectOutcome::Unreachable);
            }
            // The navigation took time: the fork may have moved on to a NEW
            // batch meanwhile (the gate settles our row when it opens one).
            // Re-check right before the digit, exactly as the single path does
            // after its wait — the row's status is the question identity.
            if !still_authorized() {
                ilog(format!(
                    "inject {}: question revoked during navigation -> Unreachable",
                    short_id(session_id),
                ));
                return Ok(InjectOutcome::Unreachable);
            }
            // FINAL look, right before the digit: the position snapshot above
            // was followed by a database read, and a person at the keyboard can
            // move tabs in that gap. Re-read the bar now and require the same
            // position + `☐`; what remains is the repaint-read → digit gap, the
            // same microsecond window the single-question path accepts.
            let final_bar = read_bar(master, key_gap, 2);
            let still_on_our_tab = matches!(
                &final_bar,
                Some(bar)
                    if bar.answered + bar.unanswered == tab.total
                        && bar.current_index == Some(tab.seq)
                        && bar.current.as_deref().is_some_and(|c| c.starts_with('☐'))
            );
            if !still_on_our_tab {
                ilog(format!(
                    "inject {}: tab {}/{} moved during the authorization check ({final_bar:?}) -> Unreachable",
                    short_id(session_id),
                    tab.seq + 1,
                    tab.total,
                ));
                return Ok(InjectOutcome::Unreachable);
            }
            // The digit selects THIS tab's option and the dialog moves on by
            // itself — NO Enter here (Enter on a tab picks its highlighted
            // default, measured 2026-09-12).
            write_all(master, &option_digits(index))?;
            drain_capture(master, key_gap);
            // Was that the last unanswered tab? Then submit — `submit_batch`
            // walks onto Submit and CONFIRMS it before pressing Enter. An
            // unreadable bar here defers the submit to the next tap's opening
            // check rather than pressing anything blind.
            return Ok(match read_bar(master, key_gap, 3) {
                // Last tab answered: submit (bound + re-authorized inside).
                Some(b) if b.unanswered == 0 => submit_batch(
                    master,
                    session_id,
                    tab,
                    key_gap,
                    settle_wait,
                    &still_authorized,
                ),
                Some(_) => {
                    ilog(format!(
                        "inject {}: tab {}/{} typed -> Delivered",
                        short_id(session_id),
                        tab.seq + 1,
                        tab.total,
                    ));
                    drain_capture(master, settle_wait);
                    InjectOutcome::Delivered
                }
                // Unreadable: we cannot tell whether that was the last tab, so
                // the submit may be owed. Say so (the button stays live) rather
                // than pressing anything blind.
                None => {
                    ilog(format!(
                        "inject {}: tab {}/{} typed; bar unreadable afterwards -> SubmitPending",
                        short_id(session_id),
                        tab.seq + 1,
                        tab.total,
                    ));
                    drain_capture(master, settle_wait);
                    InjectOutcome::SubmitPending
                }
            });
        }
        // Committed: type the number, a beat (`key_gap`, so the selector buffers
        // the digit before Enter reads it — Enter on an empty buffer submits the
        // defaults), then Enter. We do NOT re-check and bail AFTER the digit: a
        // withheld Enter would strand the digit in the buffer and a retry would
        // append another. The residual — a LOCAL keyboard answer during the
        // digit→Enter beat — can send a stray Enter into THIS same fork's next
        // state, which a working fork ignores; it can no longer drive a DIFFERENT
        // question (the identity check above already ruled that out).
        write_all(master, &option_digits(index))?;
        drain_capture(master, key_gap);
        // The digit is now in the selector's buffer. If the Enter write fails
        // HERE (e.g. the attach closed → EIO), do NOT surface a retryable error —
        // a retry would append a SECOND digit to a buffer that may still hold the
        // first. Treat a post-digit failure as Delivered (NON-retryable): the
        // digit reached the live dialog and the authoritative hook settles the row
        // when the fork completes; if nothing ever submits it, the row expires. (A
        // failure of the DIGIT write above types nothing, so its `?` → retryable
        // is correct.)
        if let Err(err) = write_all(master, b"\r") {
            ilog(format!(
                "inject {}: enter write failed after digit ({err}) -> Delivered (non-retryable)",
                short_id(session_id),
            ));
            return Ok(InjectOutcome::Delivered);
        }
        drain_capture(master, settle_wait);
        ilog(format!(
            "inject {}: intro {}B chrome present, authorized -> Delivered",
            short_id(session_id),
            intro.len(),
        ));
        Ok(InjectOutcome::Delivered)
    })
}

/// Whether the AskUserQuestion selector is LIVE on the captured screen: its
/// prompt chrome (`SELECTOR_CHROME`) appears as a byte substring. Unlike the
/// option labels — which also echo into scrollback — this line is drawn only
/// while the dialog is up, so it tells a live dialog apart from history.
fn dialog_present(screen: &[u8]) -> bool {
    screen
        .windows(SELECTOR_CHROME.len())
        .any(|window| window == SELECTOR_CHROME)
}

/// A multi-question dialog parked on its Submit tab no longer shows the
/// selector hint (measured 2026-09-12: only `1. Submit answers`). It is still
/// a LIVE dialog for the paths that VERIFY the tab bar before typing (the
/// multi-tab injector and submit-only recovery) — and ONLY for them: the
/// single-question path types digit + Enter on presence alone, so for it this
/// plain text (which can also appear in transcript echo) must never count.
fn submit_tab_present(screen: &[u8]) -> bool {
    screen
        .windows(SUBMIT_CHROME.len())
        .any(|window| window == SUBMIT_CHROME)
}

/// The Submit tab's own item text — the live-dialog signature when every tab
/// is answered and the selector hint is gone.
const SUBMIT_CHROME: &[u8] = b"Submit answers";

/// Run `claude attach <id>` on a pty and hand the master fd to `drive`,
/// then always reap the child and close the fd.
fn with_attach_pty<T>(session_id: &str, drive: impl FnOnce(RawFd) -> Result<T>) -> Result<T> {
    // Everything the child needs is built HERE, in the parent, BEFORE
    // forkpty: after the fork the child may call only async-signal-safe
    // functions. This daemon is multithreaded, so a `malloc` in the child —
    // a `CString` allocation, or the PATH search `execvp` does — can deadlock
    // on an allocator lock some other thread was holding at fork time. So
    // resolve the program to an absolute path now and hand the child a ready
    // argv AND envp it only has to `execve`.
    let prog = std::ffi::CString::new(resolve_program(&attach_program())?)
        .map_err(|_| anyhow!("attach program path has an interior NUL"))?;
    let arg_attach = std::ffi::CString::new("attach").expect("literal has no NUL");
    // `claude attach` takes the SHORT 8-char job id (what `claude agents` prints
    // as `id`), NOT the full session UUID — the full UUID fails with "No job
    // matching …" and the attach exits at once (the real cause of the phone
    // inject's "no chrome → Unreachable" on-machine).
    let arg_id = std::ffi::CString::new(short_id(session_id))
        .map_err(|_| anyhow!("session id has an interior NUL"))?;
    let argv: [*const libc::c_char; 4] = [
        prog.as_ptr(),
        arg_attach.as_ptr(),
        arg_id.as_ptr(),
        std::ptr::null(),
    ];

    // Build the child's ENVIRONMENT here too, so the child only `execve`s — no
    // allocation after the fork. The daemon runs under systemd with NO `TERM`,
    // and without it `claude attach` renders a DEGRADED view with no
    // AskUserQuestion selector (no chrome) — the on-machine cause of the
    // phone-inject "no chrome → Unreachable" failure. So carry the parent's
    // environment through and ensure a `TERM` is present so the interactive
    // dialog actually paints.
    use std::os::unix::ffi::OsStrExt as _;
    let mut env_cstrings: Vec<std::ffi::CString> = Vec::new();
    let mut has_term = false;
    for (key, value) in std::env::vars_os() {
        if key.as_bytes() == b"TERM" {
            has_term = true;
        }
        let mut kv = Vec::with_capacity(key.as_bytes().len() + value.as_bytes().len() + 1);
        kv.extend_from_slice(key.as_bytes());
        kv.push(b'=');
        kv.extend_from_slice(value.as_bytes());
        if let Ok(cs) = std::ffi::CString::new(kv) {
            env_cstrings.push(cs);
        }
    }
    if !has_term {
        env_cstrings
            .push(std::ffi::CString::new("TERM=xterm-256color").expect("literal has no NUL"));
    }
    // COLORTERM too: without it the TUI falls back to 256 colours and paints
    // the multi-question tab bar's highlight as `48;5;n` instead of the
    // truecolor `48;2;r;g;b` the probes saw (the daemon's systemd environment
    // has neither TERM nor COLORTERM; measured 2026-09-12). The bar parser
    // now accepts either, but rendering exactly what the probes captured is
    // the safer of the two.
    if !env_cstrings
        .iter()
        .any(|c| c.as_bytes().starts_with(b"COLORTERM="))
    {
        env_cstrings
            .push(std::ffi::CString::new("COLORTERM=truecolor").expect("literal has no NUL"));
    }
    let mut envp: Vec<*const libc::c_char> = env_cstrings.iter().map(|c| c.as_ptr()).collect();
    envp.push(std::ptr::null());

    let mut master: RawFd = 0;
    // A real window size so the dialog lays out normally and the arrow-key
    // navigation lands where `option_keystrokes` expects.
    let winsize = libc::winsize {
        ws_row: 50,
        ws_col: 200,
        ws_xpixel: 0,
        ws_ypixel: 0,
    };
    let pid = unsafe {
        libc::forkpty(
            &mut master,
            std::ptr::null_mut(),
            std::ptr::null(),
            &winsize,
        )
    };
    if pid < 0 {
        return Err(anyhow!("forkpty: {}", std::io::Error::last_os_error()));
    }
    if pid == 0 {
        // Child: the pty slave is already our controlling terminal and stdio.
        // Async-signal-safe calls ONLY from here. `execve` (not `execvp`):
        // `prog` is absolute (no PATH search, no allocation) and `envp` (built
        // above with a guaranteed `TERM`) is passed explicitly so the dialog
        // renders even under the daemon's TERM-less environment. On failure, die
        // loudly.
        unsafe {
            libc::execve(prog.as_ptr(), argv.as_ptr(), envp.as_ptr());
            libc::_exit(127);
        }
    }
    let result = drive(master);
    unsafe {
        libc::kill(pid, libc::SIGKILL);
        let mut status = 0;
        // Reap, retrying only on EINTR so the child never lingers as a zombie.
        while libc::waitpid(pid, &mut status, 0) < 0 {
            if std::io::Error::last_os_error().raw_os_error() != Some(libc::EINTR) {
                break;
            }
        }
        libc::close(master);
    }
    result
}

/// The attach client binary. Tests point this at a stub that records the
/// keystrokes it received; production uses the real `claude`.
fn attach_program() -> String {
    #[cfg(test)]
    return std::env::var("TINYCTB_TEST_ATTACH")
        .unwrap_or_else(|_| "/nonexistent/tinyctb-test-attach-unset".to_string());
    #[cfg(not(test))]
    "claude".to_string()
}

/// Resolve a program name to an absolute path in the PARENT (allocation is
/// fine here), so the forked child can `execve` with no PATH search. A name
/// that already contains a slash is taken as-is; an unresolved bare name is
/// returned unchanged so `execve` fails loudly into `_exit(127)`.
fn resolve_program(name: &str) -> Result<String> {
    // The attach client IS claude, so honour tinyCTB's authoritative resolver
    // (CLAUDE_BIN override, then discovery) — the same binary the rest of the
    // daemon spawns. An INVALID `CLAUDE_BIN` is an ERROR here, never a silent
    // fallback to some other PATH claude (that is the resolver's contract).
    // This runs in the PARENT, before forkpty, so its allocation and
    // `--version` probe are safe. Tests point `attach_program` at a stub path
    // (which has a slash), so they never reach this branch.
    #[cfg(not(test))]
    if name == "claude" {
        let resolved = crate::claude::resolve_claude_binary()?;
        return Ok(resolved.path.to_string_lossy().into_owned());
    }
    if name.contains('/') {
        return Ok(name.to_string());
    }
    if let Ok(path) = std::env::var("PATH") {
        for dir in path.split(':').filter(|dir| !dir.is_empty()) {
            let candidate = std::path::Path::new(dir).join(name);
            if candidate.is_file() {
                return Ok(candidate.to_string_lossy().into_owned());
            }
        }
    }
    Ok(name.to_string())
}

fn write_all(master: RawFd, bytes: &[u8]) -> Result<()> {
    // A pty master can accept a short write; loop until every byte is in, and
    // treat EINTR as a retry rather than a lost keystroke.
    let mut offset = 0;
    while offset < bytes.len() {
        let n = unsafe {
            libc::write(
                master,
                bytes[offset..].as_ptr() as *const libc::c_void,
                bytes.len() - offset,
            )
        };
        if n < 0 {
            let err = std::io::Error::last_os_error();
            if err.raw_os_error() == Some(libc::EINTR) {
                continue;
            }
            return Err(anyhow!("write keystrokes: {err}"));
        }
        if n == 0 {
            return Err(anyhow!("write keystrokes: zero-length write to pty"));
        }
        offset += n as usize;
    }
    Ok(())
}

/// Read the pty for `dur`, returning what it emitted (the attach client's
/// TUI frames) so the caller can look for the dialog. Also keeps the pipe
/// from filling.
fn drain_capture(master: RawFd, dur: Duration) -> Vec<u8> {
    let deadline = Instant::now() + dur;
    let mut out = Vec::new();
    let mut buf = [0u8; 8192];
    while Instant::now() < deadline {
        let mut pfd = libc::pollfd {
            fd: master,
            events: libc::POLLIN,
            revents: 0,
        };
        let ready = unsafe { libc::poll(&mut pfd, 1, 200) };
        if ready > 0 {
            let n = unsafe { libc::read(master, buf.as_mut_ptr() as *mut libc::c_void, buf.len()) };
            if n < 0 {
                // EINTR is a retryable interruption, not end-of-stream; only a
                // real error ends the capture. Treating EINTR as EOF would cut a
                // frame short and could read a false "chrome gone".
                if std::io::Error::last_os_error().raw_os_error() == Some(libc::EINTR) {
                    continue;
                }
                break;
            }
            if n == 0 {
                break; // EOF: the attach client closed the pty.
            }
            out.extend_from_slice(&buf[..n as usize]);
        }
    }
    out
}

/// Nudge the pty window size so the fork's TUI takes a SIGWINCH and redraws the
/// CURRENT screen, then restore it. Two changes so the final size still matches
/// the layout the attach client was given. Best-effort — a failed ioctl just
/// means the following capture may be empty, which the caller treats as "not
/// present".
fn force_repaint(master: RawFd) {
    let nudged = libc::winsize {
        ws_row: 50,
        ws_col: 199,
        ws_xpixel: 0,
        ws_ypixel: 0,
    };
    let normal = libc::winsize {
        ws_row: 50,
        ws_col: 200,
        ws_xpixel: 0,
        ws_ypixel: 0,
    };
    unsafe {
        libc::ioctl(master, libc::TIOCSWINSZ, &nudged);
    }
    // HOLD the nudged size. The TUI handles SIGWINCH asynchronously and reads
    // the CURRENT size when it gets to it: two back-to-back ioctls left it
    // seeing 200 → 200 — "no change" — and it never repainted. Measured on
    // the real machine 2026-09-12: every tab-bar read after navigation came
    // back empty (→ Unreachable on every phone tap), while the probes that
    // worked slept 150 ms between the two sizes. A stub that reprints on its
    // own cannot show this, so it is documented here, not just tested.
    std::thread::sleep(Duration::from_millis(150));
    unsafe {
        libc::ioctl(master, libc::TIOCSWINSZ, &normal);
    }
}

/// Poll the pty until the selector chrome appears, up to `budget`, returning the
/// ACCUMULATED frames and whether the chrome was seen. The real `claude attach`
/// can take a second or more to connect and paint, so a single short capture
/// races the render; accumulating until the chrome shows tolerates a slow
/// attach. The chrome is scrollback-safe (drawn only while the dialog is live)
/// and a fresh forkpty carries no prior attach's bytes, so this cannot be fooled
/// by history. Returns the instant the chrome appears, so a generous budget
/// never slows the success path — it only bounds the wait for a dialog that will
/// never show. A read of 0 (EOF: attach exited) ends the wait early with
/// whatever was seen; EINTR is retried, not mistaken for EOF.
fn wait_for_chrome(master: RawFd, budget: Duration, also_submit_tab: bool) -> (Vec<u8>, bool) {
    // `also_submit_tab`: the tab-bar-verifying paths also accept a dialog
    // parked on its Submit tab (see `submit_tab_present`); the single-question
    // path must not, since it types on presence alone.
    let hit = |acc: &[u8]| dialog_present(acc) || (also_submit_tab && submit_tab_present(acc));
    let deadline = Instant::now() + budget;
    let mut acc = Vec::new();
    let mut buf = [0u8; 8192];
    // Nudge one repaint in case the dialog painted before we began draining.
    force_repaint(master);
    while Instant::now() < deadline {
        let mut pfd = libc::pollfd {
            fd: master,
            events: libc::POLLIN,
            revents: 0,
        };
        if unsafe { libc::poll(&mut pfd, 1, 150) } > 0 {
            let n = unsafe { libc::read(master, buf.as_mut_ptr() as *mut libc::c_void, buf.len()) };
            if n < 0 {
                if std::io::Error::last_os_error().raw_os_error() == Some(libc::EINTR) {
                    continue;
                }
                break;
            }
            if n == 0 {
                break; // EOF: the attach client exited.
            }
            acc.extend_from_slice(&buf[..n as usize]);
            if hit(&acc) {
                return (acc, true);
            }
        }
    }
    let present = hit(&acc);
    (acc, present)
}

/// A diagnostic breadcrumb into the daemon log (`daemon.err.log`), so a
/// real-machine injection failure is TRACEABLE instead of silent — the 0.2.11
/// inject path logged nothing, which is why its failure had to be reverse
/// engineered from the DB and a hand-rolled pty capture. It records only a
/// short session id, byte counts, booleans and the outcome — never the option
/// index (which would leak the answer for a two-option allow/deny), the
/// question, or the option text. A no-op in tests to keep their output clean.
#[cfg(not(test))]
fn ilog(msg: String) {
    eprintln!("tinyctb: {msg}");
}
#[cfg(test)]
fn ilog(_msg: String) {}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn option_number_is_typed_one_based_then_enter() {
        assert_eq!(option_digits(0), b"1".to_vec());
        assert_eq!(option_digits(2), b"3".to_vec());
        assert_eq!(option_keystrokes(0), b"1\r".to_vec());
        assert_eq!(option_keystrokes(2), b"3\r".to_vec());
    }

    #[test]
    fn the_attach_window_runs_claude_attach_on_the_forks_short_id() {
        // `claude attach` takes the SHORT 8-char id, not the full session UUID;
        // and the command is wrapped in `env <POPPED_MARKER>` so the daemon can
        // recognise its OWN popped window for auto-close (0.2.14).
        let argv = attach_window_argv("c8bac5f4-16c1-4392-8366-57b28b1997b6", "claude");
        assert_eq!(
            argv,
            vec![
                "--title",
                "tinyCTB · 后台任务 c8bac5f4 待答",
                "--",
                "env",
                "TINYCTB_ATTACH_POPPED=1",
                "claude",
                "attach",
                "c8bac5f4",
            ]
        );
    }

    #[test]
    fn only_a_marked_window_is_recognised_as_daemon_popped() {
        let marker = POPPED_MARKER.as_bytes();
        // A daemon-popped attach: env has the exact marker entry among others.
        let popped = b"PATH=/usr/bin\0TINYCTB_ATTACH_POPPED=1\0TERM=xterm-256color\0";
        assert!(environ_has_marker(popped, marker));
        // A user-opened attach: no marker → must be left alone.
        let user = b"PATH=/usr/bin\0TERM=xterm-256color\0HOME=/home/charles\0";
        assert!(!environ_has_marker(user, marker));
        // A near-miss value must NOT count (exact entry match, not substring).
        let near = b"TINYCTB_ATTACH_POPPED=10\0X_TINYCTB_ATTACH_POPPED=1\0";
        assert!(!environ_has_marker(near, marker));
    }

    #[test]
    fn only_a_claude_attach_client_matches_for_window_closing() {
        let short = "af4b71cd";
        let claude = &b"/home/charles/.local/bin/claude"[..];
        // The popped viewer's own argv — matches (this is what we SIGTERM).
        assert!(is_attach_client_argv(
            &[claude, b"attach", b"af4b71cd"],
            claude,
            short
        ));
        // The FORK itself runs under bg-pty-host with a long argv — never match.
        assert!(!is_attach_client_argv(
            &[
                &b"claude"[..],
                b"bg-pty-host",
                b"--session-id",
                b"af4b71cd-ff34-406b-838d-90d333625ae0",
            ],
            claude,
            short
        ));
        // A different session's viewer — must not match this fork's id.
        assert!(!is_attach_client_argv(
            &[claude, b"attach", b"deadbeef"],
            claude,
            short
        ));
        // Not the claude binary (e.g. a wrapping shell) — must not match.
        assert!(!is_attach_client_argv(
            &[&b"/usr/bin/bash"[..], b"attach", b"af4b71cd"],
            claude,
            short
        ));
        // A stray `/tmp/notclaude` (basename `notclaude`, NOT `claude`) must
        // NOT match — the false positive a bare `ends_with("claude")` had.
        assert!(!is_attach_client_argv(
            &[&b"/tmp/notclaude"[..], b"attach", b"af4b71cd"],
            claude,
            short
        ));
        // Exec-type wrapper: the daemon resolved `CLAUDE_BIN=/opt/bin/cc`, but
        // that wrapper `exec`s the real claude, so the RUNNING client's argv[0]
        // is `/usr/local/bin/claude` — different from `claude_path`. It must
        // still match, via the `/claude` basename (this is the case my first
        // exact-only fix regressed; Sol round 2).
        assert!(is_attach_client_argv(
            &[&b"/usr/local/bin/claude"[..], b"attach", b"af4b71cd"],
            &b"/opt/bin/cc"[..],
            short
        ));
        // A NATIVE binary literally named `cc` (no wrapper, argv[0] preserved by
        // execve) still matches via the exact-path clause.
        let native_cc = &b"/opt/bin/cc"[..];
        assert!(is_attach_client_argv(
            &[native_cc, b"attach", b"af4b71cd"],
            native_cc,
            short
        ));
        // The common PATH form `exec claude "$@"` leaves a BARE `claude` (no
        // slash) as argv[0]; basename is `claude`, so it matches (Sol round 3).
        assert!(is_attach_client_argv(
            &[&b"claude"[..], b"attach", b"af4b71cd"],
            &b"/opt/bin/cc"[..],
            short
        ));
        // …but a bare `notclaude` (basename `notclaude`) must NOT match.
        assert!(!is_attach_client_argv(
            &[&b"notclaude"[..], b"attach", b"af4b71cd"],
            &b"/opt/bin/cc"[..],
            short
        ));
        // Documented KNOWN LIMIT: a wrapper that `exec`s a RENAMED/versioned real
        // binary (`claude.real`) leaves argv[0] that is neither `claude_path` nor
        // basename `claude`, so its window is NOT closed (a leak, never a
        // mis-kill). Locked in so the residual is explicit, not silently widened.
        assert!(!is_attach_client_argv(
            &[&b"/opt/anthropic/claude.real"[..], b"attach", b"af4b71cd"],
            &b"/opt/bin/cc"[..],
            short
        ));
    }

    #[test]
    fn dialog_presence_is_the_selector_chrome_not_the_option_text() {
        // The selector's own footer hint — present only while the dialog is up.
        let live = b"\x1b[2m Enter to select \xc2\xb7 up/down to navigate \xc2\xb7 Esc \x1b[0m";
        assert!(dialog_present(live));
        // The option label alone is NOT the signal: it also echoes in
        // scrollback, so matching it could not tell a live dialog from history.
        let gone = b"\x1b[2m APPLE  \xe9\xa6\x99\xe8\x95\x89  (the session moved on)\x1b[0m";
        assert!(!dialog_present(gone));
        // A multi-question dialog parked on Submit shows no selector hint, only
        // its Submit item. That is a live dialog ONLY for the tab-bar-verifying
        // paths; the single-question path (digit + Enter on presence alone)
        // must NOT see it as one — the same text can echo in a transcript.
        let parked = b"\x1b[38;2;177;185;249m\xe2\x9d\xaf\x1b[39m 1. Submit answers\x1b[K";
        assert!(!dialog_present(parked));
        assert!(submit_tab_present(parked));
    }

    /// The tab bar is read from REAL frames captured on-machine 2026-09-12
    /// (`claude attach` on a 2-question fork): the bar before anything was
    /// answered, and the bar after both tabs were answered with the Submit tab
    /// current.
    #[test]
    fn the_tab_bar_reads_answered_unanswered_and_current_from_real_frames() {
        // First render: `← [☐ Color] ☐ Size ✔ Submit →`, Color highlighted.
        let fresh = b"\r\x1b[1B\xe2\x86\x90 \x1b[48;2;177;185;249m\x1b[38;2;0;0;0m \xe2\x98\x90 Color \x1b[13G\x1b[39m\x1b[49m\xe2\x98\x90\x1b[15GSize\x1b[21G\xe2\x9c\x94\x1b[23GSubmit\x1b[31G\xe2\x86\x92\r\x1b[2B\x1b[38;2;255;255;255m\x1b[1mMQ1:";
        assert_eq!(
            tab_bar_state(fresh),
            Some(TabBar {
                answered: 0,
                unanswered: 2,
                current: Some("☐ Color".to_string()),
                current_index: Some(0),
            })
        );
        // After two digits: `← ☒ Color ☒ Size [✔ Submit] →`, Submit highlighted.
        let done = b"\r\x1b[1B\x1b[39m\xe2\x86\x90\x1b[4G\xe2\x98\x92\x1b[6GColor\x1b[13G\xe2\x98\x92\x1b[15GSize\x1b[20G\x1b[48;2;177;185;249m\x1b[38;2;0;0;0m \xe2\x9c\x94 Submit \x1b[49m\x1b[38;2;153;153;153m \xe2\x86\x92\r\x1b[1B";
        assert_eq!(
            tab_bar_state(done),
            Some(TabBar {
                answered: 2,
                unanswered: 0,
                current: Some("✔ Submit".to_string()),
                current_index: Some(2),
            })
        );
        // The 256-colour fallback (no COLORTERM, as under the daemon): the
        // highlight is `48;5;n`, and must be read the same way — measured
        // 2026-09-12 when every phone tap found "no current tab".
        let fallback = b"\xe2\x86\x90 \x1b[48;5;147m\x1b[38;5;16m \xe2\x98\x90 Color \x1b[49m\x1b[39m\xe2\x98\x90 Size \xe2\x9c\x94 Submit \xe2\x86\x92";
        assert_eq!(
            tab_bar_state(fallback),
            Some(TabBar {
                answered: 0,
                unanswered: 2,
                current: Some("☐ Color".to_string()),
                current_index: Some(0),
            })
        );
        // A single-question dialog has no tab bar at all.
        assert_eq!(tab_bar_state(b"\x1b[2m Enter to select \x1b[0m"), None);
    }

    /// The stub's `head -c N` writes the captured keys only once it has N
    /// bytes OR the pty closes — which happens asynchronously after the
    /// injector returns. Wait (briefly) for the file to reach `want` bytes.
    fn read_keys(capture: &std::path::Path, want: usize) -> Vec<u8> {
        for _ in 0..100 {
            let keys = std::fs::read(capture).unwrap_or_default();
            if keys.len() >= want {
                return keys;
            }
            std::thread::sleep(Duration::from_millis(20));
        }
        std::fs::read(capture).unwrap_or_default()
    }

    /// A tab-bar frame for the stubs below: the selector chrome plus a
    /// 2-tab bar whose CURRENT tab is `current` (painted with the highlight).
    fn two_tab_frame(current: &str) -> String {
        format!(
            "Enter to select \u{2190} \u{2610} Color \x1b[48;2;177;185;249m\x1b[38;2;0;0;0m {current} \x1b[49m\x1b[39m\u{2714} Submit \u{2192}"
        )
    }

    /// A tab of a multi-question dialog: the injector RESETS to the first tab
    /// (`←`×total — the bar clamps there), walks `→`×seq to ours, sees it is
    /// still unanswered (`☐`), and types the digit ONLY — no Enter, the dialog
    /// moves on by itself. Here the (static) bar still shows unanswered tabs
    /// afterwards, so nothing is submitted.
    #[test]
    fn a_multi_question_tab_is_navigated_to_and_gets_only_the_digit() {
        let _guard = crate::state::test_env_lock();
        let dir = std::env::temp_dir().join(format!("tinyctb-multitab-{}", std::process::id()));
        std::fs::create_dir_all(&dir).expect("dir");
        let capture = dir.join("keys.bin");
        let frame = two_tab_frame("\u{2610} Size");
        let stub = live_attach_stub(&dir, &frame, &frame, &capture, 10);
        std::env::set_var("TINYCTB_TEST_ATTACH", &stub);
        let outcome = inject_option_timed_multi(
            "sess-x",
            1,
            true,
            Duration::from_millis(400),
            Duration::from_millis(120),
            Duration::from_millis(120),
            Some(MultiTab { seq: 1, total: 2 }),
            || true,
        )
        .expect("inject");
        std::env::remove_var("TINYCTB_TEST_ATTACH");
        assert_eq!(outcome, InjectOutcome::Delivered);
        // ← ← (reset, clamps) → (to tab 1) then the digit for option index 1.
        assert_eq!(read_keys(&capture, 10), b"\x1b[D\x1b[D\x1b[C2".to_vec());
        std::fs::remove_dir_all(&dir).ok();
    }

    /// The same tab, but a person at the keyboard ALREADY answered it (the
    /// bar paints it `☒`): typing our digit would land on the NEXT tab and
    /// answer the wrong question, so only the navigation keys are sent, no
    /// digit, and the outcome is `Unreachable` (retryable; the hook records
    /// what the fork actually took).
    #[test]
    fn an_already_answered_tab_gets_no_digit() {
        let _guard = crate::state::test_env_lock();
        let dir = std::env::temp_dir().join(format!("tinyctb-tabdone-{}", std::process::id()));
        std::fs::create_dir_all(&dir).expect("dir");
        let capture = dir.join("keys.bin");
        let frame = two_tab_frame("\u{2612} Size");
        // Exactly the 9 navigation bytes are expected; the stub's `head -c`
        // only writes its capture once it has that many (a 10th byte — the
        // digit — must never come, which the code guarantees structurally by
        // returning before the digit write).
        let stub = live_attach_stub(&dir, &frame, &frame, &capture, 9);
        std::env::set_var("TINYCTB_TEST_ATTACH", &stub);
        let outcome = inject_option_timed_multi(
            "sess-x",
            1,
            true,
            Duration::from_millis(400),
            Duration::from_millis(120),
            Duration::from_millis(120),
            Some(MultiTab { seq: 1, total: 2 }),
            || true,
        )
        .expect("inject");
        std::env::remove_var("TINYCTB_TEST_ATTACH");
        assert_eq!(outcome, InjectOutcome::Unreachable);
        assert_eq!(
            read_keys(&capture, 9),
            b"\x1b[D\x1b[D\x1b[C".to_vec(),
            "navigation only — never the digit"
        );
        std::fs::remove_dir_all(&dir).ok();
    }

    /// The bar's highlighted tab is unanswered (`☐`) but sits at the WRONG
    /// position — someone at the keyboard moved the tabs during our
    /// navigation. `☐` alone would accept it; the position check must not:
    /// navigation only, no digit, `Unreachable`.
    #[test]
    fn a_tab_at_the_wrong_position_gets_no_digit() {
        let _guard = crate::state::test_env_lock();
        let dir = std::env::temp_dir().join(format!("tinyctb-tabpos-{}", std::process::id()));
        std::fs::create_dir_all(&dir).expect("dir");
        let capture = dir.join("keys.bin");
        // Highlight on tab 0 (`☐ Color`) while we are answering tab 1.
        let frame = "Enter to select \u{2190} \x1b[48;2;177;185;249m\x1b[38;2;0;0;0m \u{2610} Color \x1b[49m\x1b[39m \u{2610} Size \u{2714} Submit \u{2192}";
        let stub = live_attach_stub(&dir, frame, frame, &capture, 9);
        std::env::set_var("TINYCTB_TEST_ATTACH", &stub);
        let outcome = inject_option_timed_multi(
            "sess-x",
            1,
            true,
            Duration::from_millis(400),
            Duration::from_millis(120),
            Duration::from_millis(120),
            Some(MultiTab { seq: 1, total: 2 }),
            || true,
        )
        .expect("inject");
        std::env::remove_var("TINYCTB_TEST_ATTACH");
        assert_eq!(outcome, InjectOutcome::Unreachable);
        assert_eq!(
            read_keys(&capture, 9),
            b"\x1b[D\x1b[D\x1b[C".to_vec(),
            "wrong position: navigation only, never the digit"
        );
        std::fs::remove_dir_all(&dir).ok();
    }

    /// Every tab is already answered and the dialog is parked on Submit (a
    /// person finished at the keyboard, or an earlier auto-submit could not
    /// read the bar): a tap on any tab submits instead of navigating — `→`
    /// onto Submit, the bar CONFIRMS Submit is current, then Enter. Nothing
    /// else is typed.
    #[test]
    fn a_fully_answered_dialog_is_submitted_on_the_next_tap() {
        let _guard = crate::state::test_env_lock();
        let dir = std::env::temp_dir().join(format!("tinyctb-tabsubmit-{}", std::process::id()));
        std::fs::create_dir_all(&dir).expect("dir");
        let capture = dir.join("keys.bin");
        let frame = "Enter to select \u{2190} \u{2612} Color \u{2612} Size \x1b[48;2;177;185;249m\x1b[38;2;0;0;0m \u{2714} Submit \x1b[49m\x1b[39m \u{2192}";
        // `→`×2 (onto Submit) then Enter: 7 bytes, and the stub writes only
        // once it has them all.
        let stub = live_attach_stub(&dir, frame, frame, &capture, 7);
        std::env::set_var("TINYCTB_TEST_ATTACH", &stub);
        let outcome = inject_option_timed_multi(
            "sess-x",
            0,
            true,
            Duration::from_millis(400),
            Duration::from_millis(120),
            Duration::from_millis(120),
            Some(MultiTab { seq: 0, total: 2 }),
            || true,
        )
        .expect("inject");
        std::env::remove_var("TINYCTB_TEST_ATTACH");
        assert_eq!(outcome, InjectOutcome::Delivered);
        assert_eq!(
            read_keys(&capture, 7),
            b"\x1b[C\x1b[C\r".to_vec(),
            "walk onto Submit, confirm, Enter — no digit"
        );
        std::fs::remove_dir_all(&dir).ok();
    }

    /// Submit-only recovery (a tap on a delivered tab of a batch parked on
    /// Submit — its footer no longer shows the selector hint, only the Submit
    /// item): the bar shows all `total` tabs answered, so the injector walks
    /// onto Submit and presses Enter. Nothing else is typed.
    #[test]
    fn submit_only_recovery_presses_enter_on_a_fully_answered_dialog() {
        let _guard = crate::state::test_env_lock();
        let dir = std::env::temp_dir().join(format!("tinyctb-subonly-{}", std::process::id()));
        std::fs::create_dir_all(&dir).expect("dir");
        let capture = dir.join("keys.bin");
        // Parked on Submit: no "Enter to select", but "1. Submit answers".
        let frame = "\u{2190} \u{2612} Color \u{2612} Size \x1b[48;2;177;185;249m\x1b[38;2;0;0;0m \u{2714} Submit \x1b[49m\x1b[39m \u{2192}\n\u{276f} 1. Submit answers";
        let stub = live_attach_stub(&dir, frame, frame, &capture, 7);
        std::env::set_var("TINYCTB_TEST_ATTACH", &stub);
        let outcome = inject_submit_only("sess-x", 2, || true).expect("submit-only");
        std::env::remove_var("TINYCTB_TEST_ATTACH");
        assert_eq!(outcome, InjectOutcome::Delivered);
        assert_eq!(read_keys(&capture, 7), b"\x1b[C\x1b[C\r".to_vec());
        std::fs::remove_dir_all(&dir).ok();
    }

    /// The bar is all-answered but is NOT ours — three tabs where our batch
    /// has two: neither the submit-only recovery nor `submit_batch` may press
    /// Enter on it. Nothing is typed at all.
    #[test]
    fn submit_is_withheld_for_a_dialog_that_is_not_ours() {
        let _guard = crate::state::test_env_lock();
        let dir = std::env::temp_dir().join(format!("tinyctb-notours-{}", std::process::id()));
        std::fs::create_dir_all(&dir).expect("dir");
        let capture = dir.join("keys.bin");
        let frame = "\u{2190} \u{2612} A \u{2612} B \u{2612} C \x1b[48;2;177;185;249m\x1b[38;2;0;0;0m \u{2714} Submit \x1b[49m\x1b[39m \u{2192}\n\u{276f} 1. Submit answers";
        let stub = live_attach_stub(&dir, frame, frame, &capture, 1);
        std::env::set_var("TINYCTB_TEST_ATTACH", &stub);
        let outcome = inject_submit_only("sess-x", 2, || true).expect("submit-only");
        std::env::remove_var("TINYCTB_TEST_ATTACH");
        assert_eq!(outcome, InjectOutcome::Unreachable);
        assert!(
            read_keys(&capture, 1).is_empty(),
            "a three-tab dialog is not our two-tab batch: nothing typed"
        );
        std::fs::remove_dir_all(&dir).ok();
    }

    /// `submit_batch`'s OWN final authorization check: the bar is a valid,
    /// same-sized, all-answered Submit bar (so every structural check passes),
    /// but authorization is revoked right before Enter — the second call to
    /// the closure, after `inject_submit_only`'s first. The walk onto Submit
    /// happens, Enter does not, and the outcome is `SubmitPending`.
    ///
    /// The stub's `head -c N` writes its capture only once it has N bytes, so
    /// ONE run cannot prove both halves. Two runs do: with N = 6 the capture
    /// is exactly the two arrows (the walk happened); with N = 7 a seventh
    /// byte — Enter — would COMPLETE the capture, so an EMPTY capture proves
    /// no Enter was ever sent.
    #[test]
    fn a_revoked_batch_gets_no_enter_even_on_its_own_submit_tab() {
        let _guard = crate::state::test_env_lock();
        let frame = "\u{2190} \u{2612} Color \u{2612} Size \x1b[48;2;177;185;249m\x1b[38;2;0;0;0m \u{2714} Submit \x1b[49m\x1b[39m \u{2192}\n\u{276f} 1. Submit answers";
        let run = |tag: &str, keys: usize| {
            let dir =
                std::env::temp_dir().join(format!("tinyctb-revoked-{tag}-{}", std::process::id()));
            std::fs::create_dir_all(&dir).expect("dir");
            let capture = dir.join("keys.bin");
            let stub = live_attach_stub(&dir, frame, frame, &capture, keys);
            std::env::set_var("TINYCTB_TEST_ATTACH", &stub);
            let calls = std::cell::Cell::new(0u32);
            let authorized_once = || {
                calls.set(calls.get() + 1);
                calls.get() == 1
            };
            let outcome = inject_submit_only("sess-x", 2, authorized_once).expect("submit-only");
            std::env::remove_var("TINYCTB_TEST_ATTACH");
            let bytes = read_keys(&capture, keys);
            std::fs::remove_dir_all(&dir).ok();
            (outcome, calls.get(), bytes)
        };
        // Run 1 — N = 6: the walk onto Submit is exactly two right arrows.
        let (outcome, calls, bytes) = run("walk", 6);
        assert_eq!(outcome, InjectOutcome::SubmitPending);
        assert_eq!(calls, 2, "checked once on entry, once right before Enter");
        assert_eq!(bytes, b"\x1b[C\x1b[C".to_vec(), "walked onto Submit");
        // Run 2 — N = 7: a seventh byte (Enter) would complete the capture;
        // an empty capture proves Enter was withheld after the revocation.
        let (outcome, calls, bytes) = run("noenter", 7);
        assert_eq!(outcome, InjectOutcome::SubmitPending);
        assert_eq!(calls, 2);
        assert!(bytes.is_empty(), "a 7th byte (Enter) was sent: {bytes:?}");
    }

    /// A stub `claude attach` that keeps a background printer redrawing `prints`
    /// (as a live TUI does) UNTIL it is answered, captures the first `keys`
    /// bytes typed at it, then redraws `after` — the fork's NEXT state once the
    /// dialog closed. Pass `after` empty to model attach dying without redrawing
    /// (an EMPTY post-Enter frame, which must NOT count as a successful close).
    /// (A SIGWINCH trap is unreliable while a foreground command blocks, so the
    /// redraw is timer-driven; `force_repaint` is still exercised — it just is
    /// not what makes the chrome appear here.) Raw mode so the CR is not mangled.
    fn live_attach_stub(
        dir: &std::path::Path,
        prints: &str,
        after: &str,
        capture: &std::path::Path,
        keys: usize,
    ) -> PathBuf {
        use std::os::unix::fs::PermissionsExt as _;
        let stub = dir.join("attach-stub.sh");
        let done = dir.join("answered.flag");
        std::fs::write(
            &stub,
            format!(
                "#!/bin/sh\nstty raw -echo 2>/dev/null\n( while true; do if [ -f '{done}' ]; then printf '%s' '{after}'; else printf '%s' '{prints}'; fi; sleep 0.03; done ) &\nprinter=$!\nhead -c {keys} > '{capture}' 2>/dev/null\n: > '{done}'\nsleep 0.4\nkill \"$printer\" 2>/dev/null\n",
                done = done.display(),
                capture = capture.display()
            ),
        )
        .expect("write stub");
        std::fs::set_permissions(&stub, std::fs::Permissions::from_mode(0o755)).expect("chmod");
        stub
    }

    /// A stub `claude attach` that renders NOTHING and exits — attach failed to
    /// bring up a screen at all.
    fn dead_attach_stub(dir: &std::path::Path) -> PathBuf {
        use std::os::unix::fs::PermissionsExt as _;
        let stub = dir.join("attach-dead.sh");
        std::fs::write(&stub, "#!/bin/sh\nexit 0\n").expect("write stub");
        std::fs::set_permissions(&stub, std::fs::Permissions::from_mode(0o755)).expect("chmod");
        stub
    }

    /// End to end against a stub: the selector chrome appears, and the number for
    /// option index 2 ("3") + Enter are typed into the live dialog — so the
    /// outcome is `Delivered`. The inject side no longer scrapes the screen to
    /// judge whether the fork "took" it (the PostToolUse hook records the fork's
    /// own result); `Delivered` means only "the keys were put in front of a live
    /// dialog", so `after` here is irrelevant.
    #[test]
    fn a_present_dialog_gets_the_option_number() {
        let _guard = crate::state::test_env_lock();
        let dir = std::env::temp_dir().join(format!("tinyctb-inject-{}", std::process::id()));
        std::fs::create_dir_all(&dir).expect("dir");
        let capture = dir.join("keys.bin");
        let stub = live_attach_stub(&dir, "Enter to select", "continuing…", &capture, 2);
        std::env::set_var("TINYCTB_TEST_ATTACH", &stub);
        let outcome = inject_option_timed(
            "sess-x",
            2,
            true,
            Duration::from_millis(400),
            Duration::from_millis(250),
            Duration::from_millis(250),
            || true,
        )
        .expect("inject");
        std::env::remove_var("TINYCTB_TEST_ATTACH");
        assert_eq!(outcome, InjectOutcome::Delivered);
        let got = std::fs::read(&capture).unwrap_or_default();
        assert!(got.starts_with(&option_digits(2)), "stub received {got:?}");
        std::fs::remove_dir_all(&dir).ok();
    }

    /// The dialog is up, but by the time it renders THIS question is no longer
    /// the fork's open one (it was answered and the fork moved on, so the gate
    /// settled the row): `still_authorized()` returns false, so NOTHING is typed
    /// and the outcome is `Unreachable`. This is the question-instance identity
    /// the generic chrome cannot give — it stops a stale button, whose injection
    /// is already in flight, from driving a LATER question's dialog.
    #[test]
    fn a_superseded_question_is_not_driven() {
        let _guard = crate::state::test_env_lock();
        let dir = std::env::temp_dir().join(format!("tinyctb-superseded-{}", std::process::id()));
        std::fs::create_dir_all(&dir).expect("dir");
        let capture = dir.join("keys.bin");
        // The selector chrome is present the whole time (a live dialog is up)...
        let stub = live_attach_stub(&dir, "Enter to select", "Enter to select", &capture, 1);
        std::env::set_var("TINYCTB_TEST_ATTACH", &stub);
        // ...but the row is no longer authorized (answered/superseded/settled).
        let outcome = inject_option_timed(
            "sess-x",
            0,
            true,
            Duration::from_millis(400),
            Duration::from_millis(200),
            Duration::from_millis(200),
            || false,
        )
        .expect("inject");
        std::env::remove_var("TINYCTB_TEST_ATTACH");
        assert_eq!(outcome, InjectOutcome::Unreachable);
        assert!(
            std::fs::read(&capture).unwrap_or_default().is_empty(),
            "nothing typed once the question is no longer authorized"
        );
        std::fs::remove_dir_all(&dir).ok();
    }

    /// A real screen is up but it is NOT the selector (answered at the keyboard,
    /// still connecting, or errored — the pty cannot tell): nothing is typed and
    /// the outcome is `Unreachable`, never a claimed local answer.
    #[test]
    fn a_non_selector_screen_is_unreachable() {
        let _guard = crate::state::test_env_lock();
        let dir = std::env::temp_dir().join(format!("tinyctb-gone-{}", std::process::id()));
        std::fs::create_dir_all(&dir).expect("dir");
        let capture = dir.join("keys.bin");
        let stub = live_attach_stub(&dir, "the session is busy elsewhere", "gone", &capture, 2);
        std::env::set_var("TINYCTB_TEST_ATTACH", &stub);
        let outcome = inject_option_timed(
            "sess-x",
            1,
            true,
            Duration::from_millis(400),
            Duration::from_millis(200),
            Duration::from_millis(200),
            || true,
        )
        .expect("inject");
        std::env::remove_var("TINYCTB_TEST_ATTACH");
        assert_eq!(outcome, InjectOutcome::Unreachable);
        assert!(
            std::fs::read(&capture).unwrap_or_default().is_empty(),
            "no keystrokes to a non-selector screen"
        );
        std::fs::remove_dir_all(&dir).ok();
    }

    /// An attach that never renders a screen is `Unreachable` — a connection
    /// failure must not be reported as a person answering locally.
    #[test]
    fn an_attach_that_never_renders_is_unreachable() {
        let _guard = crate::state::test_env_lock();
        let dir = std::env::temp_dir().join(format!("tinyctb-dead-{}", std::process::id()));
        std::fs::create_dir_all(&dir).expect("dir");
        let stub = dead_attach_stub(&dir);
        std::env::set_var("TINYCTB_TEST_ATTACH", &stub);
        let outcome = inject_option_timed(
            "sess-x",
            0,
            true,
            Duration::from_millis(400),
            Duration::from_millis(100),
            Duration::from_millis(100),
            || true,
        )
        .expect("inject");
        std::env::remove_var("TINYCTB_TEST_ATTACH");
        assert_eq!(outcome, InjectOutcome::Unreachable);
        std::fs::remove_dir_all(&dir).ok();
    }

    /// v0.2.17: the interactive XTEST inject is fenced by the row at every step.
    #[test]
    fn xtest_inject_is_fenced_by_the_row_at_every_step() {
        let _guard = crate::state::test_env_lock();
        let steps = || XTEST_STEPS.with(|s| std::mem::take(&mut *s.borrow_mut()));
        let released = std::cell::Cell::new(0u32);
        let run = |open: &dyn Fn() -> bool, claim: bool, ours: bool| {
            inject_option_via_xtest(
                0x2a526d7,
                2,
                &XtestGates {
                    still_open: open,
                    claim: &|| claim,
                    still_ours: &|| ours,
                    release: &|| released.set(released.get() + 1),
                },
            )
        };
        std::env::set_var("TINYCTB_TEST_XTEST", "delivered");
        let _ = steps();

        // The whole sequence, in order: focus, digit 3 (index 2), Return.
        assert_eq!(run(&|| true, true, true), InjectOutcome::Delivered);
        assert_eq!(
            steps(),
            ["focus 44377815", "digits 44377815 3", "enter 44377815"]
        );
        // Revoked up front: not even a focus request.
        assert_eq!(run(&|| false, true, true), InjectOutcome::Unreachable);
        assert!(steps().is_empty());
        // Revoked DURING the focus wait (check #1 true, #2 false): focus only.
        let calls = std::cell::Cell::new(0u32);
        let during = || {
            calls.set(calls.get() + 1);
            calls.get() == 1
        };
        assert_eq!(run(&during, true, true), InjectOutcome::Unreachable);
        assert_eq!(steps(), ["focus 44377815"]);
        // Lost the claim (another tap, or the stamp failed): no key.
        assert_eq!(run(&|| true, false, true), InjectOutcome::Unreachable);
        assert_eq!(steps(), ["focus 44377815"]);
        // Answered at the keyboard during the beat: the digit is out, the
        // Return is WITHHELD, and it is still Delivered (never retryable).
        assert_eq!(run(&|| true, true, false), InjectOutcome::Delivered);
        assert_eq!(steps(), ["focus 44377815", "digits 44377815 3"]);
        assert_eq!(released.get(), 0, "nothing released while keys went out");

        // The digits step sent nothing: the claim is released, retryable.
        std::env::set_var("TINYCTB_TEST_XTEST", "digits");
        assert_eq!(run(&|| true, true, true), InjectOutcome::Unreachable);
        assert_eq!(steps(), ["focus 44377815"]);
        assert_eq!(released.get(), 1);
        // A failed Return after the digit is still Delivered.
        std::env::set_var("TINYCTB_TEST_XTEST", "enter");
        assert_eq!(run(&|| true, true, true), InjectOutcome::Delivered);
        let _ = steps();
        std::env::remove_var("TINYCTB_TEST_XTEST");
    }

    /// v0.2.17: the capture helper's stub returns the remembered window id, and
    /// "none" reads as no capture.
    #[test]
    fn capture_reads_the_window_from_the_stub() {
        let _guard = crate::state::test_env_lock();
        std::env::set_var("TINYCTB_TEST_CAPTURE_WINDOW", "44304567");
        assert_eq!(capture_active_terminal_window(), Some(44304567));
        std::env::set_var("TINYCTB_TEST_CAPTURE_WINDOW", "none");
        assert_eq!(capture_active_terminal_window(), None);
        std::env::remove_var("TINYCTB_TEST_CAPTURE_WINDOW");
    }
}
