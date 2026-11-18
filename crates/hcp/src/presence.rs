//! Whether the person who supervises Claude Code is at this Mac, so a gated command can be
//! asked in the terminal rather than in Slack (`HCP_HOOK_WHEN_PRESENT=terminal`).
//!
//! Two things must both hold:
//!
//! - **Someone is at the Mac:** at least one display is on and awake, and the screen is not
//!   locked. A monitor that is switched off usually leaves the display list; one that has
//!   gone to sleep reports itself asleep. A locked screen counts as away even with the
//!   monitors on.
//! - **The session can show them a prompt:** the Claude Code process the hook runs under has
//!   a terminal and was not started with `-p`/`--print`, and its permission mode is one in
//!   which a hook's "ask" is put to the user.
//!
//! Anything that cannot be determined counts as away, which keeps the request in Slack. Being
//! wrong in the other direction is not unsafe either: "ask" never lets a command run on its
//! own. In a session nobody is watching it blocks the command (checked with `claude -p`, with
//! an allow rule and with `--dangerously-skip-permissions`).

use std::process::Command;

/// What the hook found out about the Mac. `None` means it could not tell.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct Screen {
    /// Displays that are online, active and awake.
    pub awake_displays: Option<u32>,
    pub locked: Option<bool>,
    /// The login session owns the console (it is not a background or fast-user-switched
    /// session).
    pub on_console: Option<bool>,
}

impl Screen {
    pub fn someone_is_here(&self) -> bool {
        self.awake_displays.is_some_and(|n| n > 0) && self.locked == Some(false) && self.on_console == Some(true)
    }

    /// Why a request went to Slack rather than the terminal, for the record.
    pub fn away_reason(&self) -> &'static str {
        match (self.awake_displays, self.locked, self.on_console) {
            (None, ..) | (_, None, _) | (.., None) => "the display and lock state could not be read",
            (_, _, Some(false)) => "this session is not on the console",
            (_, Some(true), _) => "the screen is locked",
            (Some(0), ..) => "no display is on",
            _ => "someone is at the Mac",
        }
    }
}

/// The Claude Code process the hook is running under, as far as it matters here.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Session {
    pub has_terminal: bool,
    pub print_mode: bool,
}

impl Session {
    pub fn can_prompt(&self) -> bool {
        self.has_terminal && !self.print_mode
    }
}

/// Permission modes in which Claude Code puts a hook's "ask" to the user. `bypassPermissions`
/// and `dontAsk` are left out: there is nobody to ask, or the documentation does not say.
pub fn mode_prompts(permission_mode: Option<&str>) -> bool {
    matches!(permission_mode, None | Some("default" | "acceptEdits" | "plan" | "auto"))
}

/// Reads the display and lock state. On anything but macOS, nothing can be read.
pub fn screen() -> Screen {
    #[cfg(target_os = "macos")]
    {
        macos::screen()
    }
    #[cfg(not(target_os = "macos"))]
    {
        Screen::default()
    }
}

/// Finds the `claude` process above this one and describes it. `None` if there is none
/// within a few generations.
pub fn session() -> Option<Session> {
    let mut pid = std::os::unix::process::parent_id();
    for _ in 0..8 {
        if pid <= 1 {
            return None;
        }
        let (parent, tty, executable) = describe(pid)?;
        if executable.rsplit('/').next() == Some("claude") {
            let args = ps(pid, "args=")?;
            let print_mode = args.split_whitespace().skip(1).any(|arg| arg == "-p" || arg == "--print");
            return Some(Session { has_terminal: !tty.is_empty() && !tty.starts_with('?'), print_mode });
        }
        pid = parent;
    }
    None
}

fn ps(pid: u32, field: &str) -> Option<String> {
    // By absolute path: a `ps` earlier on PATH could be anything.
    let output = Command::new("/bin/ps").args(["-o", field, "-p", &pid.to_string()]).output().ok()?;
    output.status.success().then(|| String::from_utf8_lossy(&output.stdout).trim().to_string())
}

/// Parent PID, terminal and executable path of a process.
fn describe(pid: u32) -> Option<(u32, String, String)> {
    let parent = ps(pid, "ppid=")?.parse().ok()?;
    let tty = ps(pid, "tty=")?;
    let executable = ps(pid, "comm=")?;
    Some((parent, tty, executable))
}

#[cfg(target_os = "macos")]
mod macos {
    use std::ffi::{CString, c_char, c_void};

    use super::Screen;

    type CFTypeRef = *const c_void;
    const UTF8: u32 = 0x0800_0100;
    const MAX_DISPLAYS: u32 = 32;

    #[link(name = "CoreGraphics", kind = "framework")]
    unsafe extern "C" {
        fn CGGetOnlineDisplayList(max: u32, displays: *mut u32, count: *mut u32) -> i32;
        fn CGDisplayIsActive(display: u32) -> u32;
        fn CGDisplayIsAsleep(display: u32) -> u32;
        fn CGSessionCopyCurrentDictionary() -> CFTypeRef;
    }

    #[link(name = "CoreFoundation", kind = "framework")]
    unsafe extern "C" {
        fn CFStringCreateWithCString(alloc: CFTypeRef, text: *const c_char, encoding: u32) -> CFTypeRef;
        fn CFDictionaryGetValue(dict: CFTypeRef, key: CFTypeRef) -> CFTypeRef;
        fn CFGetTypeID(value: CFTypeRef) -> usize;
        fn CFBooleanGetTypeID() -> usize;
        fn CFBooleanGetValue(value: CFTypeRef) -> u8;
        fn CFRelease(value: CFTypeRef);
    }

    pub fn screen() -> Screen {
        let mut screen = Screen { awake_displays: awake_displays(), ..Screen::default() };
        // SAFETY: a Copy function, so the dictionary is ours to release; values read from it
        // are borrowed and checked to be booleans before use.
        unsafe {
            let session = CGSessionCopyCurrentDictionary();
            if session.is_null() {
                return screen;
            }
            // Absent means not locked: the key is only present while the screen is locked
            // on some versions of macOS.
            screen.locked = Some(flag(session, "CGSSessionScreenIsLocked").unwrap_or(false));
            screen.on_console = flag(session, "kCGSSessionOnConsoleKey");
            CFRelease(session);
        }
        screen
    }

    fn awake_displays() -> Option<u32> {
        let mut ids = [0u32; MAX_DISPLAYS as usize];
        let mut count = 0u32;
        // SAFETY: the buffer holds MAX_DISPLAYS entries, and count is at most that.
        unsafe {
            if CGGetOnlineDisplayList(MAX_DISPLAYS, ids.as_mut_ptr(), &mut count) != 0 {
                return None;
            }
            let online = &ids[..count.min(MAX_DISPLAYS) as usize];
            Some(online.iter().filter(|&&id| CGDisplayIsActive(id) != 0 && CGDisplayIsAsleep(id) == 0).count() as u32)
        }
    }

    /// A boolean from the session dictionary, `None` if absent or not a boolean.
    unsafe fn flag(dict: CFTypeRef, key: &str) -> Option<bool> {
        let key = CString::new(key).ok()?;
        // SAFETY: the caller passes a live dictionary; the key string is created and released here.
        unsafe {
            let cf_key = CFStringCreateWithCString(std::ptr::null(), key.as_ptr(), UTF8);
            if cf_key.is_null() {
                return None;
            }
            let value = CFDictionaryGetValue(dict, cf_key);
            CFRelease(cf_key);
            (!value.is_null() && CFGetTypeID(value) == CFBooleanGetTypeID()).then(|| CFBooleanGetValue(value) != 0)
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn screen(displays: u32, locked: bool) -> Screen {
        Screen { awake_displays: Some(displays), locked: Some(locked), on_console: Some(true) }
    }

    #[test]
    fn someone_is_here_only_with_a_display_on_and_the_screen_unlocked() {
        assert!(screen(1, false).someone_is_here());
        assert!(screen(3, false).someone_is_here());
        assert!(!screen(0, false).someone_is_here(), "monitors off or asleep");
        assert!(!screen(2, true).someone_is_here(), "monitors on but locked");
        assert_eq!(screen(0, false).away_reason(), "no display is on");
        assert_eq!(screen(2, true).away_reason(), "the screen is locked");
    }

    #[test]
    fn anything_unknown_counts_as_away() {
        assert!(!Screen::default().someone_is_here());
        assert!(!Screen { locked: None, ..screen(1, false) }.someone_is_here());
        assert!(!Screen { on_console: None, ..screen(1, false) }.someone_is_here());
        assert!(!Screen { on_console: Some(false), ..screen(1, false) }.someone_is_here());
    }

    #[test]
    fn only_interactive_sessions_in_prompting_modes_can_ask() {
        assert!(Session { has_terminal: true, print_mode: false }.can_prompt());
        assert!(!Session { has_terminal: false, print_mode: false }.can_prompt());
        assert!(!Session { has_terminal: true, print_mode: true }.can_prompt());
        for mode in ["default", "acceptEdits", "plan", "auto"] {
            assert!(mode_prompts(Some(mode)), "{mode}");
        }
        assert!(mode_prompts(None));
        for mode in ["bypassPermissions", "dontAsk", "something-new"] {
            assert!(!mode_prompts(Some(mode)), "{mode}");
        }
    }
}
