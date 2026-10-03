use crate::input::{self, EnigoState};
#[cfg(target_os = "linux")]
use crate::settings::TypingTool;
use crate::settings::{get_settings, AutoSubmitKey, ClipboardHandling, PasteMethod};
use enigo::{Direction, Enigo, Key, Keyboard};
use log::info;
use std::process::Command;
#[cfg(target_os = "linux")]
use std::sync::OnceLock;
use std::time::Duration;
use tauri::{AppHandle, Manager};
use tauri_plugin_clipboard_manager::ClipboardExt;

#[cfg(target_os = "linux")]
use crate::utils::{is_gnome_wayland, is_kde_wayland, is_wayland};

fn with_enigo<T>(
    app_handle: &AppHandle,
    f: impl FnOnce(&mut Enigo) -> Result<T, String>,
) -> Result<T, String> {
    let enigo_state = app_handle
        .try_state::<EnigoState>()
        .ok_or("Enigo state not initialized")?;
    let mut enigo = enigo_state
        .0
        .lock()
        .map_err(|e| format!("Failed to lock Enigo: {}", e))?;
    f(&mut enigo)
}

fn write_text_to_clipboard(app_handle: &AppHandle, text: &str) -> Result<(), String> {
    #[cfg(target_os = "linux")]
    if is_wayland() && is_wl_copy_available() {
        info!("Using wl-copy for clipboard write on Wayland");
        return write_clipboard_via_wl_copy(text);
    }

    app_handle
        .clipboard()
        .write_text(text)
        .map_err(|e| format!("Failed to write to clipboard: {}", e))
}

fn finish_clipboard_paste(
    paste_result: Result<(), String>,
    paste_delay_after_ms: u64,
    restore_clipboard: impl FnOnce(),
) -> Result<(), String> {
    std::thread::sleep(Duration::from_millis(paste_delay_after_ms));
    restore_clipboard();
    paste_result
}

/// Pastes text using the clipboard: saves current content, writes text, sends paste keystroke, restores clipboard.
fn paste_via_clipboard(
    text: &str,
    app_handle: &AppHandle,
    paste_method: &PasteMethod,
    paste_delay_ms: u64,
    paste_delay_after_ms: u64,
) -> Result<(), String> {
    let clipboard = app_handle.clipboard();
    let saved_text = clipboard.read_text().ok().filter(|t| !t.is_empty());
    // Only probe for an image when there is no text to restore. Text is by far the
    // common case, and reading an image decodes the full bitmap, so this keeps the
    // text path exactly as cheap as it was before.
    let saved_image = if saved_text.is_none() {
        clipboard.read_image().ok().map(|image| image.to_owned())
    } else {
        None
    };

    // Write text to clipboard first
    write_text_to_clipboard(app_handle, text)?;

    std::thread::sleep(Duration::from_millis(paste_delay_ms));

    // Capture key injection errors so the original clipboard is restored before
    // propagating them to the caller.
    let paste_result = (|| -> Result<(), String> {
        // Send paste key combo
        #[cfg(target_os = "linux")]
        let key_combo_sent = try_send_key_combo_linux(paste_method)?;

        #[cfg(not(target_os = "linux"))]
        let key_combo_sent = false;

        // Fall back to enigo if no native tool handled it
        if !key_combo_sent {
            with_enigo(app_handle, |enigo| match paste_method {
                // The legacy path cannot detect a mistimed chord, so it keeps the
                // conservative 100ms modifier hold.
                PasteMethod::CtrlV => input::send_paste_ctrl_v(enigo, 100),
                PasteMethod::CtrlShiftV => input::send_paste_ctrl_shift_v(enigo, 100),
                PasteMethod::ShiftInsert => input::send_paste_shift_insert(enigo, 100),
                _ => Err("Invalid paste method for clipboard paste".into()),
            })?;
        }

        Ok(())
    })();

    finish_clipboard_paste(paste_result, paste_delay_after_ms, || {
        // Restore original clipboard content even when key injection failed.
        // Text takes priority so this path stays identical to the previous behavior;
        // an image is only restored when the clipboard held no text at all, which is
        // the case that used to silently wipe screenshots.
        if let Some(clipboard_content) = saved_text {
            let _ = write_text_to_clipboard(app_handle, &clipboard_content);
        } else if let Some(image) = saved_image {
            info!("Restoring image to clipboard");
            let _ = clipboard.write_image(&image);
        } else {
            // Nothing was there to begin with — don't leave the transcription behind.
            let _ = clipboard.clear();
        }
    })
}

/// Attempts to send a key combination using Linux-native tools.
/// Returns `Ok(true)` if a native tool handled it, `Ok(false)` to fall back to enigo.
#[cfg(target_os = "linux")]
fn try_send_key_combo_linux(paste_method: &PasteMethod) -> Result<bool, String> {
    if is_wayland() {
        // Wayland: prefer wtype (but not on KDE or GNOME), then dotool, then ydotool
        // Note: wtype doesn't work on KDE (no zwp_virtual_keyboard_manager_v1 support)
        // or on GNOME/Mutter (same reason — Mutter deliberately does not implement
        // the virtual-keyboard-v1 protocol).
        if !is_kde_wayland() && !is_gnome_wayland() && is_wtype_available() {
            info!("Using wtype for key combo");
            send_key_combo_via_wtype(paste_method)?;
            return Ok(true);
        }
        if is_dotool_available() {
            info!("Using dotool for key combo");
            send_key_combo_via_dotool(paste_method)?;
            return Ok(true);
        }
        if is_ydotool_available() {
            info!("Using ydotool for key combo");
            send_key_combo_via_ydotool(paste_method)?;
            return Ok(true);
        }
    } else {
        // X11: prefer xdotool, then ydotool
        if is_xdotool_available() {
            info!("Using xdotool for key combo");
            send_key_combo_via_xdotool(paste_method)?;
            return Ok(true);
        }
        if is_ydotool_available() {
            info!("Using ydotool for key combo");
            send_key_combo_via_ydotool(paste_method)?;
            return Ok(true);
        }
    }

    Ok(false)
}

/// Attempts to type text directly using Linux-native tools.
/// Returns `Ok(true)` if a native tool handled it, `Ok(false)` to fall back to enigo.
#[cfg(target_os = "linux")]
fn try_direct_typing_linux(text: &str, preferred_tool: TypingTool) -> Result<bool, String> {
    // If user specified a tool, try only that one
    if preferred_tool != TypingTool::Auto {
        return match preferred_tool {
            TypingTool::Wtype if is_wtype_available() => {
                info!("Using user-specified wtype");
                type_text_via_wtype(text)?;
                Ok(true)
            }
            TypingTool::Kwtype if is_kwtype_available() => {
                info!("Using user-specified kwtype");
                type_text_via_kwtype(text)?;
                Ok(true)
            }
            TypingTool::Dotool if is_dotool_available() => {
                info!("Using user-specified dotool");
                type_text_via_dotool(text)?;
                Ok(true)
            }
            TypingTool::Ydotool if is_ydotool_available() => {
                info!("Using user-specified ydotool");
                type_text_via_ydotool(text)?;
                Ok(true)
            }
            TypingTool::Xdotool if is_xdotool_available() => {
                info!("Using user-specified xdotool");
                type_text_via_xdotool(text)?;
                Ok(true)
            }
            _ => Err(format!(
                "Typing tool {:?} is not available on this system",
                preferred_tool
            )),
        };
    }

    // Auto mode - existing fallback chain
    if is_wayland() {
        // KDE Wayland: prefer kwtype (uses KDE Fake Input protocol, supports umlauts)
        if is_kde_wayland() && is_kwtype_available() {
            info!("Using kwtype for direct text input on KDE Wayland");
            type_text_via_kwtype(text)?;
            return Ok(true);
        }
        // Wayland: prefer wtype, then dotool, then ydotool
        // Note: wtype doesn't work on KDE (no zwp_virtual_keyboard_manager_v1 support)
        // or on GNOME/Mutter (same reason — Mutter deliberately does not implement
        // the virtual-keyboard-v1 protocol).
        if !is_kde_wayland() && !is_gnome_wayland() && is_wtype_available() {
            info!("Using wtype for direct text input");
            type_text_via_wtype(text)?;
            return Ok(true);
        }
        if is_dotool_available() {
            info!("Using dotool for direct text input");
            type_text_via_dotool(text)?;
            return Ok(true);
        }
        if is_ydotool_available() {
            info!("Using ydotool for direct text input");
            type_text_via_ydotool(text)?;
            return Ok(true);
        }
    } else {
        // X11: prefer xdotool, then ydotool
        if is_xdotool_available() {
            info!("Using xdotool for direct text input");
            type_text_via_xdotool(text)?;
            return Ok(true);
        }
        if is_ydotool_available() {
            info!("Using ydotool for direct text input");
            type_text_via_ydotool(text)?;
            return Ok(true);
        }
    }

    Ok(false)
}

/// Returns the list of available typing tools on this system.
/// Always includes "auto" as the first entry.
#[cfg(target_os = "linux")]
pub fn get_available_typing_tools() -> Vec<String> {
    let mut tools = vec!["auto".to_string()];
    if is_wtype_available() {
        tools.push("wtype".to_string());
    }
    if is_kwtype_available() {
        tools.push("kwtype".to_string());
    }
    if is_dotool_available() {
        tools.push("dotool".to_string());
    }
    if is_ydotool_available() {
        tools.push("ydotool".to_string());
    }
    if is_xdotool_available() {
        tools.push("xdotool".to_string());
    }
    tools
}

/// Check if wtype is available (Wayland text input tool)
#[cfg(target_os = "linux")]
fn is_wtype_available() -> bool {
    Command::new("which")
        .arg("wtype")
        .output()
        .map(|output| output.status.success())
        .unwrap_or(false)
}

/// Check if dotool is available (another Wayland text input tool)
#[cfg(target_os = "linux")]
fn is_dotool_available() -> bool {
    Command::new("which")
        .arg("dotool")
        .output()
        .map(|output| output.status.success())
        .unwrap_or(false)
}

#[cfg(target_os = "linux")]
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum YdotoolKeySyntax {
    Symbolic,
    RawKeycodes,
}

#[cfg(target_os = "linux")]
const YDOTOOL_UNKNOWN_HELP_FALLBACK: YdotoolKeySyntax = YdotoolKeySyntax::RawKeycodes;

#[cfg(target_os = "linux")]
static YDOTOOL_KEY_SYNTAX: OnceLock<YdotoolKeySyntax> = OnceLock::new();

/// Classifies `ydotool key --help` output without relying on version or distro metadata.
#[cfg(target_os = "linux")]
fn classify_ydotool_key_syntax(help: &str) -> Option<YdotoolKeySyntax> {
    let help = help.to_ascii_lowercase();

    // Check modern markers first in case a future help message mentions the legacy syntax.
    if help.contains("syntax: <keycode>:<pressed>")
        || help.contains("[keycodes]")
        || help.contains("using raw keycodes")
    {
        Some(YdotoolKeySyntax::RawKeycodes)
    } else if help.contains("separated by plus (+)")
        || (help.contains("<key sequence>") && help.contains("alt+r"))
    {
        Some(YdotoolKeySyntax::Symbolic)
    } else {
        None
    }
}

/// Metadata for one ydotoold socket candidate. `mode` may include file-type bits;
/// writability uses only the permission bits.
#[cfg(target_os = "linux")]
#[derive(Clone, Copy, Debug)]
struct YdotoolSocketMeta {
    is_socket: bool,
    mode: u32,
    uid: u32,
    gid: u32,
}

/// A path Handy may pass to the ydotool client. `meta` is `None` when the path
/// does not exist or cannot be statted.
#[cfg(target_os = "linux")]
#[derive(Clone, Debug)]
struct YdotoolSocketCandidate {
    path: String,
    meta: Option<YdotoolSocketMeta>,
}

/// How the ydotool child should see `YDOTOOL_SOCKET`.
#[cfg(target_os = "linux")]
#[derive(Clone, Debug, Eq, PartialEq)]
enum YdotoolSocketSelection {
    /// A non-empty variable is already in the environment. Do not replace it.
    Inherit(String),
    /// Set `YDOTOOL_SOCKET` on this child only.
    Inject(String),
    /// No usable candidate. Leave the client default alone.
    ClientDefault,
}

#[cfg(target_os = "linux")]
#[derive(Clone, Debug)]
struct YdotoolSocketResolution {
    selection: YdotoolSocketSelection,
    /// Search-order notes for every candidate, included in paste failures.
    checked: Vec<String>,
}

#[cfg(target_os = "linux")]
fn current_euid() -> u32 {
    // SAFETY: geteuid only reads the calling process's credential.
    unsafe { libc::geteuid() as u32 }
}

#[cfg(target_os = "linux")]
fn current_egid() -> u32 {
    // SAFETY: getegid only reads the calling process's credential.
    unsafe { libc::getegid() as u32 }
}

/// Supplementary groups only. The effective gid is not included unless the
/// platform already reports it here; callers check `getegid` separately.
#[cfg(target_os = "linux")]
fn supplementary_group_ids() -> Vec<u32> {
    let mut groups: Vec<libc::gid_t> = vec![0; 32];
    loop {
        // SAFETY: `groups` is a writable `gid_t` buffer and its length is the
        // `ngroups` argument. A negative return is an error code, not a count.
        let result = unsafe { libc::getgroups(groups.len() as libc::c_int, groups.as_mut_ptr()) };
        if result >= 0 {
            groups.truncate(result as usize);
            return groups.into_iter().map(|gid| gid as u32).collect();
        }
        let err = std::io::Error::last_os_error().raw_os_error();
        if err == Some(libc::EINVAL) && groups.len() < 65_536 {
            groups.resize(groups.len().saturating_mul(2), 0);
            continue;
        }
        return Vec::new();
    }
}

/// Unix DAC write check used by `connect()`. Exactly one class applies: root,
/// owner write, group write for `getegid` or a supplementary group, or other
/// write. A caller already in the socket's group is not granted other-write,
/// so a mode such as `0606` is not writable for that caller.
#[cfg(target_os = "linux")]
fn socket_writable_by_current_user(meta: &YdotoolSocketMeta) -> bool {
    let mode = meta.mode & 0o777;
    let euid = current_euid();
    if euid == 0 {
        return true;
    }
    if euid == meta.uid {
        return mode & 0o200 != 0;
    }
    if current_egid() == meta.gid || supplementary_group_ids().contains(&meta.gid) {
        return mode & 0o020 != 0;
    }
    mode & 0o002 != 0
}

#[cfg(target_os = "linux")]
fn describe_ydotool_candidate(candidate: &YdotoolSocketCandidate) -> String {
    let Some(meta) = candidate.meta else {
        return format!("{}: not found", candidate.path);
    };
    let mode = meta.mode & 0o777;
    if !meta.is_socket {
        return format!(
            "{}: not a socket (mode {:04o} owner uid {} gid {})",
            candidate.path, mode, meta.uid, meta.gid
        );
    }
    if socket_writable_by_current_user(&meta) {
        return format!(
            "{}: writable socket mode {:04o} owner uid {} gid {}",
            candidate.path, mode, meta.uid, meta.gid
        );
    }
    format!(
        "{}: socket mode {:04o} owner uid {} gid {} is not writable by the desktop user",
        candidate.path, mode, meta.uid, meta.gid
    )
}

/// Picks the socket for one ydotool child.
///
/// A non-empty `YDOTOOL_SOCKET` wins and is not rewritten. Blank values fall
/// through to the candidates in order. The first existing writable socket is
/// injected; otherwise the client default is left in place. Candidates are
/// described either way so a later non-zero exit can name what was checked.
#[cfg(target_os = "linux")]
fn select_ydotool_socket(
    inherited: Option<&str>,
    candidates: &[YdotoolSocketCandidate],
) -> YdotoolSocketResolution {
    let mut injectable = None;
    let mut checked = Vec::with_capacity(candidates.len());
    for candidate in candidates {
        let writable_socket = candidate
            .meta
            .is_some_and(|meta| meta.is_socket && socket_writable_by_current_user(&meta));
        if writable_socket && injectable.is_none() {
            injectable = Some(candidate.path.clone());
        }
        checked.push(describe_ydotool_candidate(candidate));
    }

    if let Some(value) = inherited.filter(|value| !value.trim().is_empty()) {
        return YdotoolSocketResolution {
            selection: YdotoolSocketSelection::Inherit(value.to_string()),
            checked,
        };
    }

    if let Some(path) = injectable {
        return YdotoolSocketResolution {
            selection: YdotoolSocketSelection::Inject(path),
            checked,
        };
    }

    YdotoolSocketResolution {
        selection: YdotoolSocketSelection::ClientDefault,
        checked,
    }
}

#[cfg(target_os = "linux")]
fn stat_ydotool_socket_candidate(path: std::path::PathBuf) -> YdotoolSocketCandidate {
    use std::os::unix::fs::{FileTypeExt, MetadataExt};

    let path_string = path.display().to_string();
    let meta = match std::fs::metadata(&path) {
        Ok(metadata) => Some(YdotoolSocketMeta {
            is_socket: metadata.file_type().is_socket(),
            mode: metadata.mode(),
            uid: metadata.uid(),
            gid: metadata.gid(),
        }),
        Err(_) => None,
    };
    YdotoolSocketCandidate {
        path: path_string,
        meta,
    }
}

#[cfg(target_os = "linux")]
fn runtime_ydotool_socket_candidate() -> YdotoolSocketCandidate {
    match std::env::var_os("XDG_RUNTIME_DIR") {
        Some(dir) if !dir.is_empty() => {
            let mut path = std::path::PathBuf::from(dir);
            path.push(".ydotool_socket");
            stat_ydotool_socket_candidate(path)
        }
        _ => YdotoolSocketCandidate {
            path: "$XDG_RUNTIME_DIR/.ydotool_socket".to_string(),
            meta: None,
        },
    }
}

/// Stats the socket search path on every call. Nothing here is cached: a
/// daemon restarted between pastes has to be visible to the next child.
#[cfg(target_os = "linux")]
fn resolve_ydotool_socket() -> YdotoolSocketResolution {
    // `var_os` keeps a non-Unicode value inherited. Blank (empty or
    // whitespace) is treated as unset and falls through inside the selector.
    let inherited =
        std::env::var_os("YDOTOOL_SOCKET").map(|value| value.to_string_lossy().into_owned());
    let candidates = [
        runtime_ydotool_socket_candidate(),
        stat_ydotool_socket_candidate(std::path::PathBuf::from("/tmp/.ydotool_socket")),
    ];
    select_ydotool_socket(inherited.as_deref(), &candidates)
}

/// Applies a socket selection to a ydotool child. Inherit leaves a non-empty
/// `YDOTOOL_SOCKET` untouched. Inject sets it on this child only.
/// ClientDefault removes it so an empty or whitespace value is not inherited:
/// ydotool 1.0.4 treats any set value as the socket path.
#[cfg(target_os = "linux")]
fn configure_ydotool_socket(command: &mut Command, resolution: &YdotoolSocketResolution) {
    match &resolution.selection {
        YdotoolSocketSelection::Inject(path) => {
            command.env("YDOTOOL_SOCKET", path);
        }
        YdotoolSocketSelection::ClientDefault => {
            command.env_remove("YDOTOOL_SOCKET");
        }
        YdotoolSocketSelection::Inherit(_) => {}
    }
}

/// Builds a `ydotool` command with the socket resolved for this child only.
#[cfg(target_os = "linux")]
fn ydotool_command() -> (Command, YdotoolSocketResolution) {
    let resolution = resolve_ydotool_socket();
    let mut command = Command::new("ydotool");
    configure_ydotool_socket(&mut command, &resolution);
    (command, resolution)
}

/// Failure text for a non-zero ydotool exit. Stdout, stderr, and the status
/// are always included, then the socket decision and every path that was
/// checked, so the paste error is never the bare `ydotool failed:`.
#[cfg(target_os = "linux")]
fn format_ydotool_failure(
    stdout: &str,
    stderr: &str,
    exit_code: Option<i32>,
    resolution: &YdotoolSocketResolution,
) -> String {
    let status = match exit_code {
        Some(code) => format!("exit status {}", code),
        None => "exit status unavailable".to_string(),
    };
    let stdout_text = if stdout.is_empty() { "(empty)" } else { stdout };
    let stderr_text = if stderr.is_empty() { "(empty)" } else { stderr };
    let socket = match &resolution.selection {
        YdotoolSocketSelection::Inherit(path) => format!("inherited YDOTOOL_SOCKET={}", path),
        YdotoolSocketSelection::Inject(path) => format!("injected YDOTOOL_SOCKET={}", path),
        YdotoolSocketSelection::ClientDefault => {
            "YDOTOOL_SOCKET left unset for the ydotool client default".to_string()
        }
    };
    let checked = if resolution.checked.is_empty() {
        "(none)".to_string()
    } else {
        resolution.checked.join("; ")
    };
    format!(
        "ydotool failed: {}; stdout: {}; stderr: {}; {}; paths checked: {}",
        status, stdout_text, stderr_text, socket, checked
    )
}

#[cfg(target_os = "linux")]
fn check_ydotool_output(
    output: std::process::Output,
    resolution: &YdotoolSocketResolution,
) -> Result<(), String> {
    if output.status.success() {
        return Ok(());
    }
    let stdout = String::from_utf8_lossy(&output.stdout);
    let stderr = String::from_utf8_lossy(&output.stderr);
    Err(format_ydotool_failure(
        &stdout,
        &stderr,
        output.status.code(),
        resolution,
    ))
}

/// Detects and caches a recognized ydotool key syntax. Unknown or failed probes are not cached,
/// allowing a transient daemon or PATH problem to recover on a later paste attempt.
#[cfg(target_os = "linux")]
fn detect_ydotool_key_syntax() -> YdotoolKeySyntax {
    if let Some(syntax) = YDOTOOL_KEY_SYNTAX.get() {
        return *syntax;
    }

    let (mut command, _resolution) = ydotool_command();
    match command.args(["key", "--help"]).output() {
        Ok(output) => {
            // ydotool 0.x writes help to stderr and its exit status varies by build.
            let mut help = String::from_utf8_lossy(&output.stdout).into_owned();
            if !help.is_empty() && !output.stderr.is_empty() {
                help.push('\n');
            }
            help.push_str(&String::from_utf8_lossy(&output.stderr));

            if let Some(syntax) = classify_ydotool_key_syntax(&help) {
                *YDOTOOL_KEY_SYNTAX.get_or_init(|| {
                    info!("Detected ydotool key syntax: {:?}", syntax);
                    syntax
                })
            } else {
                // Preserve Handy's existing behavior and compatibility with current ydotool.
                log::warn!(
                    "Could not recognize ydotool key --help output (exit status {:?}); using raw-keycode syntax",
                    output.status.code()
                );
                YDOTOOL_UNKNOWN_HELP_FALLBACK
            }
        }
        Err(error) => {
            log::warn!(
                "Could not query ydotool key syntax: {}; using raw-keycode syntax",
                error
            );
            YDOTOOL_UNKNOWN_HELP_FALLBACK
        }
    }
}

/// Check if ydotool is available (uinput-based, works on both Wayland and X11)
#[cfg(target_os = "linux")]
fn is_ydotool_available() -> bool {
    Command::new("which")
        .arg("ydotool")
        .output()
        .map(|output| output.status.success())
        .unwrap_or(false)
}

#[cfg(target_os = "linux")]
fn is_xdotool_available() -> bool {
    Command::new("which")
        .arg("xdotool")
        .output()
        .map(|output| output.status.success())
        .unwrap_or(false)
}

/// Check if kwtype is available (KDE Wayland virtual keyboard input tool)
#[cfg(target_os = "linux")]
fn is_kwtype_available() -> bool {
    Command::new("which")
        .arg("kwtype")
        .output()
        .map(|output| output.status.success())
        .unwrap_or(false)
}

/// Check if wl-copy is available (Wayland clipboard tool)
#[cfg(target_os = "linux")]
fn is_wl_copy_available() -> bool {
    Command::new("which")
        .arg("wl-copy")
        .output()
        .map(|output| output.status.success())
        .unwrap_or(false)
}

/// Type text directly via wtype on Wayland.
#[cfg(target_os = "linux")]
fn type_text_via_wtype(text: &str) -> Result<(), String> {
    let output = Command::new("wtype")
        .arg("--") // Protect against text starting with -
        .arg(text)
        .output()
        .map_err(|e| format!("Failed to execute wtype: {}", e))?;

    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        return Err(format!("wtype failed: {}", stderr));
    }

    Ok(())
}

/// Type text directly via xdotool on X11.
#[cfg(target_os = "linux")]
fn type_text_via_xdotool(text: &str) -> Result<(), String> {
    let output = Command::new("xdotool")
        .arg("type")
        .arg("--clearmodifiers")
        .arg("--")
        .arg(text)
        .output()
        .map_err(|e| format!("Failed to execute xdotool: {}", e))?;

    // `--clearmodifiers` restores the modifiers that were held when xdotool
    // started. If the user releases one while xdotool is typing, that synthetic
    // restore can leave the modifier latched on the XTEST keyboard (#1817).
    // Release both sides of Handy's supported push-style modifiers to clear any
    // stale restore. Lock keys are intentionally excluded because key events
    // toggle them.
    //
    // The release is unconditional, so it can make a modifier that is still
    // physically held appear released until the next physical event. This is
    // preferable to leaving a synthetic modifier latched system-wide.
    //
    // Clean up before checking the typing status because xdotool may have
    // changed modifier state before returning an error. Cleanup remains
    // best-effort because the text may already have been partially or fully
    // typed, but failures are logged so they can be diagnosed.
    match Command::new("xdotool")
        .arg("keyup")
        .args([
            "Control_L",
            "Control_R",
            "Shift_L",
            "Shift_R",
            "Alt_L",
            "Alt_R",
            "Super_L",
            "Super_R",
        ])
        .output()
    {
        Ok(cleanup_output) if !cleanup_output.status.success() => {
            let stderr = String::from_utf8_lossy(&cleanup_output.stderr);
            log::warn!(
                "xdotool modifier cleanup failed with status {:?}: {}",
                cleanup_output.status.code(),
                stderr.trim()
            );
        }
        Err(error) => log::warn!("Failed to execute xdotool modifier cleanup: {}", error),
        _ => {}
    }

    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        return Err(format!("xdotool failed: {}", stderr));
    }

    Ok(())
}

/// Type text directly via dotool (works on both Wayland and X11 via uinput).
#[cfg(target_os = "linux")]
fn type_text_via_dotool(text: &str) -> Result<(), String> {
    use std::io::Write;
    use std::process::Stdio;

    let mut child = Command::new("dotool")
        .stdin(Stdio::piped())
        .spawn()
        .map_err(|e| format!("Failed to spawn dotool: {}", e))?;

    if let Some(mut stdin) = child.stdin.take() {
        // dotool uses "type <text>" command
        writeln!(stdin, "type {}", text)
            .map_err(|e| format!("Failed to write to dotool stdin: {}", e))?;
    }

    let output = child
        .wait_with_output()
        .map_err(|e| format!("Failed to wait for dotool: {}", e))?;

    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        return Err(format!("dotool failed: {}", stderr));
    }

    Ok(())
}

/// Type text directly via ydotool (uinput-based, requires ydotoold daemon).
#[cfg(target_os = "linux")]
fn type_text_via_ydotool(text: &str) -> Result<(), String> {
    let (mut command, resolution) = ydotool_command();
    let output = command
        .arg("type")
        .arg("--")
        .arg(text)
        .output()
        .map_err(|e| format!("Failed to execute ydotool: {}", e))?;

    check_ydotool_output(output, &resolution)
}

/// Type text directly via kwtype (KDE Wayland virtual keyboard, uses KDE Fake Input protocol).
#[cfg(target_os = "linux")]
fn type_text_via_kwtype(text: &str) -> Result<(), String> {
    let output = Command::new("kwtype")
        .arg("--")
        .arg(text)
        .output()
        .map_err(|e| format!("Failed to execute kwtype: {}", e))?;

    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        return Err(format!("kwtype failed: {}", stderr));
    }

    Ok(())
}

/// Write text to clipboard via wl-copy (Wayland clipboard tool).
/// Uses Stdio::null() to avoid blocking on repeated calls — wl-copy forks a
/// daemon that inherits piped fds, causing read_to_end to hang indefinitely.
#[cfg(target_os = "linux")]
fn write_clipboard_via_wl_copy(text: &str) -> Result<(), String> {
    use std::process::Stdio;
    let status = Command::new("wl-copy")
        .arg("--")
        .arg(text)
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()
        .map_err(|e| format!("Failed to execute wl-copy: {}", e))?;

    if !status.success() {
        return Err("wl-copy failed".into());
    }

    Ok(())
}

/// Send a key combination (e.g., Ctrl+V) via wtype on Wayland.
#[cfg(target_os = "linux")]
fn send_key_combo_via_wtype(paste_method: &PasteMethod) -> Result<(), String> {
    let args: Vec<&str> = match paste_method {
        PasteMethod::CtrlV => vec!["-M", "ctrl", "-k", "v", "-m", "ctrl"],
        PasteMethod::ShiftInsert => vec!["-M", "shift", "-k", "Insert", "-m", "shift"],
        PasteMethod::CtrlShiftV => vec![
            "-M", "ctrl", "-M", "shift", "-k", "v", "-m", "shift", "-m", "ctrl",
        ],
        _ => return Err("Unsupported paste method".into()),
    };

    let output = Command::new("wtype")
        .args(&args)
        .output()
        .map_err(|e| format!("Failed to execute wtype: {}", e))?;

    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        return Err(format!("wtype failed: {}", stderr));
    }

    Ok(())
}

/// Send a key combination (e.g., Ctrl+V) via dotool.
#[cfg(target_os = "linux")]
fn send_key_combo_via_dotool(paste_method: &PasteMethod) -> Result<(), String> {
    let command;
    match paste_method {
        PasteMethod::CtrlV => command = "echo key ctrl+v | dotool",
        PasteMethod::ShiftInsert => command = "echo key shift+insert | dotool",
        PasteMethod::CtrlShiftV => command = "echo key ctrl+shift+v | dotool",
        _ => return Err("Unsupported paste method".into()),
    }
    use std::process::Stdio;
    let status = Command::new("sh")
        .arg("-c")
        .arg(command)
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()
        .map_err(|e| format!("Failed to execute dotool: {}", e))?;
    if !status.success() {
        return Err("dotool failed".into());
    }

    Ok(())
}

#[cfg(target_os = "linux")]
fn ydotool_key_args(
    paste_method: &PasteMethod,
    syntax: YdotoolKeySyntax,
) -> Result<&'static [&'static str], String> {
    let args = match (paste_method, syntax) {
        (PasteMethod::CtrlV, YdotoolKeySyntax::Symbolic) => &["key", "ctrl+v"][..],
        (PasteMethod::CtrlShiftV, YdotoolKeySyntax::Symbolic) => &["key", "ctrl+shift+v"][..],
        (PasteMethod::ShiftInsert, YdotoolKeySyntax::Symbolic) => &["key", "shift+insert"][..],
        (PasteMethod::CtrlV, YdotoolKeySyntax::RawKeycodes) => {
            &["key", "29:1", "47:1", "47:0", "29:0"][..]
        }
        (PasteMethod::CtrlShiftV, YdotoolKeySyntax::RawKeycodes) => {
            &["key", "29:1", "42:1", "47:1", "47:0", "42:0", "29:0"][..]
        }
        (PasteMethod::ShiftInsert, YdotoolKeySyntax::RawKeycodes) => {
            &["key", "42:1", "110:1", "110:0", "42:0"][..]
        }
        _ => return Err("Unsupported paste method".into()),
    };

    Ok(args)
}

/// Send a key combination (e.g., Ctrl+V) via ydotool (requires ydotoold daemon).
#[cfg(target_os = "linux")]
fn send_key_combo_via_ydotool(paste_method: &PasteMethod) -> Result<(), String> {
    let syntax = detect_ydotool_key_syntax();
    let args = ydotool_key_args(paste_method, syntax)?;

    let (mut command, resolution) = ydotool_command();
    let output = command
        .args(args)
        .output()
        .map_err(|e| format!("Failed to execute ydotool: {}", e))?;

    check_ydotool_output(output, &resolution)
}

/// Send a key combination (e.g., Ctrl+V) via xdotool on X11.
#[cfg(target_os = "linux")]
fn send_key_combo_via_xdotool(paste_method: &PasteMethod) -> Result<(), String> {
    let key_combo = match paste_method {
        PasteMethod::CtrlV => "ctrl+v",
        PasteMethod::CtrlShiftV => "ctrl+shift+v",
        PasteMethod::ShiftInsert => "shift+Insert",
        _ => return Err("Unsupported paste method".into()),
    };

    let output = Command::new("xdotool")
        .arg("key")
        .arg("--clearmodifiers")
        .arg(key_combo)
        .output()
        .map_err(|e| format!("Failed to execute xdotool: {}", e))?;

    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        return Err(format!("xdotool failed: {}", stderr));
    }

    Ok(())
}

/// Pastes text by invoking an external script.
/// The script receives the text to paste as a single argument.
fn paste_via_external_script(text: &str, script_path: &str) -> Result<(), String> {
    info!("Pasting via external script: {}", script_path);

    // Do not capture the script's stdio. Wayland clipboard helpers such as
    // wl-copy may fork a background selection daemon that inherits those file
    // descriptors; waiting for captured output would then block until the
    // clipboard selection is replaced instead of returning after the script
    // itself exits.
    use std::process::Stdio;
    let status = Command::new(script_path)
        .arg(text)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()
        .map_err(|e| format!("Failed to execute external script '{}': {}", script_path, e))?;

    if !status.success() {
        return Err(format!(
            "External script '{}' failed with exit code {:?}",
            script_path,
            status.code()
        ));
    }

    Ok(())
}

/// Types text directly by simulating individual key presses.
fn paste_direct(
    text: &str,
    app_handle: &AppHandle,
    #[cfg(target_os = "linux")] typing_tool: TypingTool,
) -> Result<(), String> {
    #[cfg(target_os = "linux")]
    {
        if try_direct_typing_linux(text, typing_tool)? {
            return Ok(());
        }
        info!("Falling back to enigo for direct text input");
    }

    with_enigo(app_handle, |enigo| input::paste_text_direct(enigo, text))
}

pub(crate) fn send_return_key(enigo: &mut Enigo, key_type: AutoSubmitKey) -> Result<(), String> {
    match key_type {
        AutoSubmitKey::Enter => {
            enigo
                .key(Key::Return, Direction::Press)
                .map_err(|e| format!("Failed to press Return key: {}", e))?;
            enigo
                .key(Key::Return, Direction::Release)
                .map_err(|e| format!("Failed to release Return key: {}", e))?;
        }
        AutoSubmitKey::CtrlEnter => {
            enigo
                .key(Key::Control, Direction::Press)
                .map_err(|e| format!("Failed to press Control key: {}", e))?;
            enigo
                .key(Key::Return, Direction::Press)
                .map_err(|e| format!("Failed to press Return key: {}", e))?;
            enigo
                .key(Key::Return, Direction::Release)
                .map_err(|e| format!("Failed to release Return key: {}", e))?;
            enigo
                .key(Key::Control, Direction::Release)
                .map_err(|e| format!("Failed to release Control key: {}", e))?;
        }
        AutoSubmitKey::CmdEnter => {
            enigo
                .key(Key::Meta, Direction::Press)
                .map_err(|e| format!("Failed to press Meta/Cmd key: {}", e))?;
            enigo
                .key(Key::Return, Direction::Press)
                .map_err(|e| format!("Failed to press Return key: {}", e))?;
            enigo
                .key(Key::Return, Direction::Release)
                .map_err(|e| format!("Failed to release Return key: {}", e))?;
            enigo
                .key(Key::Meta, Direction::Release)
                .map_err(|e| format!("Failed to release Meta/Cmd key: {}", e))?;
        }
    }

    Ok(())
}

fn should_send_auto_submit(auto_submit: bool, paste_method: PasteMethod) -> bool {
    auto_submit && paste_method != PasteMethod::None
}

pub fn paste(text: String, app_handle: AppHandle) -> Result<(), String> {
    let settings = get_settings(&app_handle);
    let paste_method = settings.paste_method;
    let paste_delay_ms = settings.paste_delay_ms;
    let paste_delay_after_ms = settings.paste_delay_after_ms;

    // Append trailing space if setting is enabled
    let text = if settings.append_trailing_space {
        format!("{} ", text)
    } else {
        text
    };

    info!(
        "Using paste method: {:?}, delay before: {}ms, delay after: {}ms",
        paste_method, paste_delay_ms, paste_delay_after_ms
    );

    // Perform the paste operation
    match paste_method {
        PasteMethod::None => {
            info!("PasteMethod::None selected - skipping paste action");
        }
        PasteMethod::Direct => {
            paste_direct(
                &text,
                &app_handle,
                #[cfg(target_os = "linux")]
                settings.typing_tool,
            )?;
        }
        PasteMethod::CtrlV | PasteMethod::CtrlShiftV | PasteMethod::ShiftInsert => {
            // Debug-gated receipt-sequenced paste (#502): restore the clipboard
            // after the target actually reads the transcript, not on a timer.
            // On success it fully handles the paste (including auto-submit and
            // clipboard handling) asynchronously; on failure fall through to
            // the legacy path untouched.
            #[cfg(any(target_os = "macos", target_os = "windows"))]
            if settings.reliable_paste {
                let reliable_result = with_enigo(&app_handle, |enigo| {
                    crate::paste_tx::try_reliable_paste(
                        &text,
                        &app_handle,
                        &paste_method,
                        enigo,
                        settings.auto_submit,
                        settings.auto_submit_key,
                        settings.clipboard_handling,
                    )
                });
                match reliable_result {
                    Ok(()) => return Ok(()),
                    Err(e) => {
                        log::warn!("Reliable paste unavailable ({e}); falling back to legacy paste")
                    }
                }
            }
            paste_via_clipboard(
                &text,
                &app_handle,
                &paste_method,
                paste_delay_ms,
                paste_delay_after_ms,
            )?
        }
        PasteMethod::ExternalScript => {
            let script_path = settings
                .external_script_path
                .as_ref()
                .filter(|p| !p.is_empty())
                .ok_or("External script path is not configured")?;
            paste_via_external_script(&text, script_path)?;
        }
    }

    if should_send_auto_submit(settings.auto_submit, paste_method) {
        std::thread::sleep(Duration::from_millis(50));
        if let Err(error) = with_enigo(&app_handle, |enigo| {
            send_return_key(enigo, settings.auto_submit_key)
        }) {
            log::warn!("Paste succeeded, but auto-submit failed: {error}");
        }
    }

    // After pasting, optionally copy to clipboard based on settings
    if settings.clipboard_handling == ClipboardHandling::CopyToClipboard {
        write_text_to_clipboard(&app_handle, &text)?;
    }

    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::cell::Cell;

    #[cfg(target_os = "linux")]
    const YDOTOOL_0_1_8_HELP: &str = r#"
Usage: key [--delay <ms>] [--key-delay <ms>] [--repeat <times>] [--repeat-delay <ms>] <key sequence> ...
Each key sequence can be any number of modifiers and keys, separated by plus (+)
For example: alt+r Alt+F4 CTRL+alt+f3 aLT+1+2+3 ctrl+Backspace
"#;

    #[cfg(target_os = "linux")]
    const YDOTOOL_1_0_4_HELP: &str = r#"
Usage: key [OPTION]... [KEYCODES]...
Since there's no way to know how many keyboard layouts are there in the world,
we're using raw keycodes now.
Syntax: <keycode>:<pressed>
e.g. 28:1 28:0 means pressing on the Enter button on a standard US keyboard.
"#;

    #[cfg(target_os = "linux")]
    #[test]
    fn classifies_ydotool_0_1_8_symbolic_help() {
        assert_eq!(
            classify_ydotool_key_syntax(YDOTOOL_0_1_8_HELP),
            Some(YdotoolKeySyntax::Symbolic)
        );
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn classifies_ydotool_1_0_4_raw_keycode_help() {
        assert_eq!(
            classify_ydotool_key_syntax(YDOTOOL_1_0_4_HELP),
            Some(YdotoolKeySyntax::RawKeycodes)
        );
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn unknown_ydotool_help_falls_back_to_raw_keycodes() {
        let syntax = classify_ydotool_key_syntax("unrecognized help output")
            .unwrap_or(YDOTOOL_UNKNOWN_HELP_FALLBACK);

        assert_eq!(syntax, YdotoolKeySyntax::RawKeycodes);
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn generates_symbolic_ydotool_arguments_for_all_paste_methods() {
        assert_eq!(
            ydotool_key_args(&PasteMethod::CtrlV, YdotoolKeySyntax::Symbolic).unwrap(),
            ["key", "ctrl+v"]
        );
        assert_eq!(
            ydotool_key_args(&PasteMethod::CtrlShiftV, YdotoolKeySyntax::Symbolic).unwrap(),
            ["key", "ctrl+shift+v"]
        );
        assert_eq!(
            ydotool_key_args(&PasteMethod::ShiftInsert, YdotoolKeySyntax::Symbolic).unwrap(),
            ["key", "shift+insert"]
        );
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn generates_raw_ydotool_arguments_for_all_paste_methods() {
        assert_eq!(
            ydotool_key_args(&PasteMethod::CtrlV, YdotoolKeySyntax::RawKeycodes).unwrap(),
            ["key", "29:1", "47:1", "47:0", "29:0"]
        );
        assert_eq!(
            ydotool_key_args(&PasteMethod::CtrlShiftV, YdotoolKeySyntax::RawKeycodes).unwrap(),
            ["key", "29:1", "42:1", "47:1", "47:0", "42:0", "29:0"]
        );
        assert_eq!(
            ydotool_key_args(&PasteMethod::ShiftInsert, YdotoolKeySyntax::RawKeycodes).unwrap(),
            ["key", "42:1", "110:1", "110:0", "42:0"]
        );
    }

    #[cfg(target_os = "linux")]
    fn candidate(path: &str, meta: Option<YdotoolSocketMeta>) -> YdotoolSocketCandidate {
        YdotoolSocketCandidate {
            path: path.to_string(),
            meta,
        }
    }

    #[cfg(target_os = "linux")]
    fn accessible_socket(path: &str) -> YdotoolSocketCandidate {
        candidate(
            path,
            Some(YdotoolSocketMeta {
                is_socket: true,
                mode: 0o600,
                uid: current_euid(),
                gid: current_egid(),
            }),
        )
    }

    #[cfg(target_os = "linux")]
    fn child_socket(command: &Command) -> Option<&std::ffi::OsStr> {
        command
            .get_envs()
            .find(|(key, _)| *key == std::ffi::OsStr::new("YDOTOOL_SOCKET"))
            .and_then(|(_, value)| value)
    }

    // `env_remove` records the key with a None value. An absent entry still
    // inherits whatever the parent process has set.
    #[cfg(target_os = "linux")]
    fn ydotool_socket_was_removed(command: &Command) -> bool {
        command
            .get_envs()
            .any(|(key, value)| key == std::ffi::OsStr::new("YDOTOOL_SOCKET") && value.is_none())
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn inherited_ydotool_socket_is_not_replaced() {
        let inherited = "/run/user/1000/.ydotool_socket";
        let resolution = select_ydotool_socket(
            Some(inherited),
            &[
                accessible_socket("/run/user/1000/.ydotool_socket"),
                accessible_socket("/tmp/.ydotool_socket"),
            ],
        );

        assert_eq!(
            resolution.selection,
            YdotoolSocketSelection::Inherit(inherited.to_string())
        );
        let mut command = Command::new("ydotool");
        configure_ydotool_socket(&mut command, &resolution);
        assert!(
            command
                .get_envs()
                .all(|(key, _)| key != std::ffi::OsStr::new("YDOTOOL_SOCKET")),
            "a non-empty inherited YDOTOOL_SOCKET must be left untouched"
        );
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn fedora_tmp_socket_is_injected_when_runtime_socket_is_absent() {
        let resolution = select_ydotool_socket(
            None,
            &[
                candidate("/run/user/1000/.ydotool_socket", None),
                accessible_socket("/tmp/.ydotool_socket"),
            ],
        );

        assert_eq!(
            resolution.selection,
            YdotoolSocketSelection::Inject("/tmp/.ydotool_socket".to_string())
        );
        let mut command = Command::new("ydotool");
        configure_ydotool_socket(&mut command, &resolution);
        assert_eq!(
            child_socket(&command),
            Some(std::ffi::OsStr::new("/tmp/.ydotool_socket"))
        );
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn runtime_dir_socket_wins_when_both_exist() {
        let resolution = select_ydotool_socket(
            None,
            &[
                accessible_socket("/run/user/1000/.ydotool_socket"),
                accessible_socket("/tmp/.ydotool_socket"),
            ],
        );

        assert_eq!(
            resolution.selection,
            YdotoolSocketSelection::Inject("/run/user/1000/.ydotool_socket".to_string())
        );
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn inaccessible_daemon_socket_is_left_for_the_client_default() {
        let euid = current_euid();
        assert_ne!(euid, 0, "this fixture assumes a non-root desktop user");
        let owner = euid.wrapping_add(1);
        let resolution = select_ydotool_socket(
            None,
            &[
                candidate("/run/user/1000/.ydotool_socket", None),
                candidate(
                    "/tmp/.ydotool_socket",
                    Some(YdotoolSocketMeta {
                        is_socket: true,
                        mode: 0o600,
                        uid: owner,
                        gid: owner,
                    }),
                ),
            ],
        );

        assert_eq!(resolution.selection, YdotoolSocketSelection::ClientDefault);
        let mut command = Command::new("ydotool");
        configure_ydotool_socket(&mut command, &resolution);
        assert!(
            ydotool_socket_was_removed(&command),
            "client default must clear YDOTOOL_SOCKET rather than inherit it"
        );

        let failure = format_ydotool_failure("", "", Some(1), &resolution);
        assert!(failure.contains("/tmp/.ydotool_socket"));
        assert!(failure.contains("0600"));
        assert!(failure.contains(&format!("owner uid {}", owner)));
        assert!(failure.contains("client default"));
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn empty_ydotool_output_includes_status_and_paths_checked() {
        let resolution = select_ydotool_socket(
            None,
            &[
                candidate("/run/user/1000/.ydotool_socket", None),
                candidate("/tmp/.ydotool_socket", None),
            ],
        );
        let failure = format_ydotool_failure("", "", Some(2), &resolution);

        assert!(failure.contains("exit status 2"));
        assert!(failure.contains("/run/user/1000/.ydotool_socket"));
        assert!(failure.contains("/tmp/.ydotool_socket"));
        assert!(failure.contains("paths checked"));
        assert_ne!(failure, "ydotool failed:");
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn ydotool_connect_error_is_preserved_with_candidate_notes() {
        let stderr =
            "failed to connect socket '/run/user/1000/.ydotool_socket': No such file or directory";
        let resolution = select_ydotool_socket(
            None,
            &[
                candidate("/run/user/1000/.ydotool_socket", None),
                candidate("/tmp/.ydotool_socket", None),
            ],
        );
        let failure = format_ydotool_failure("", stderr, Some(1), &resolution);

        let stderr_at = failure.find(stderr).expect("stderr should be preserved");
        let notes_at = failure
            .find("paths checked")
            .expect("candidate notes should be appended");
        assert!(stderr_at < notes_at);
        assert!(failure.contains("/tmp/.ydotool_socket"));
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn non_socket_candidate_is_skipped() {
        let resolution = select_ydotool_socket(
            None,
            &[
                candidate(
                    "/run/user/1000/.ydotool_socket",
                    Some(YdotoolSocketMeta {
                        is_socket: false,
                        mode: 0o644,
                        uid: current_euid(),
                        gid: current_egid(),
                    }),
                ),
                accessible_socket("/tmp/.ydotool_socket"),
            ],
        );

        assert_eq!(
            resolution.selection,
            YdotoolSocketSelection::Inject("/tmp/.ydotool_socket".to_string())
        );
        let notes = resolution.checked.join("; ");
        assert!(notes.contains("/run/user/1000/.ydotool_socket: not a socket"));
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn group_writable_socket_is_accessible_and_foreign_owner_socket_is_not() {
        let euid = current_euid();
        assert_ne!(euid, 0, "this fixture assumes a non-root desktop user");
        let egid = current_egid();
        let groups = supplementary_group_ids();
        let gid = groups
            .iter()
            .copied()
            .find(|gid| *gid != egid)
            .or_else(|| groups.first().copied())
            .expect("process should have a supplementary group");
        let other_uid = euid.wrapping_add(1);

        let group_socket = select_ydotool_socket(
            None,
            &[candidate(
                "/tmp/.ydotool_socket",
                Some(YdotoolSocketMeta {
                    is_socket: true,
                    mode: 0o660,
                    uid: other_uid,
                    gid,
                }),
            )],
        );
        assert_eq!(
            group_socket.selection,
            YdotoolSocketSelection::Inject("/tmp/.ydotool_socket".to_string())
        );

        let foreign_socket = select_ydotool_socket(
            None,
            &[candidate(
                "/tmp/.ydotool_socket",
                Some(YdotoolSocketMeta {
                    is_socket: true,
                    mode: 0o600,
                    uid: other_uid,
                    gid,
                }),
            )],
        );
        assert_eq!(
            foreign_socket.selection,
            YdotoolSocketSelection::ClientDefault
        );
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn other_write_does_not_apply_to_a_caller_in_the_socket_group() {
        let euid = current_euid();
        assert_ne!(euid, 0, "this fixture assumes a non-root desktop user");
        let egid = current_egid();
        let groups = supplementary_group_ids();
        let other_uid = euid.wrapping_add(1);
        // Mode 0606: the group class has no write. connect() uses only that
        // class for egid or a supplementary group, so this socket must not
        // hide a later one.
        let mut member_gids = vec![egid];
        member_gids.extend(groups.iter().copied());
        for gid in member_gids {
            let grouped = select_ydotool_socket(
                None,
                &[
                    candidate(
                        "/tmp/.ydotool_socket",
                        Some(YdotoolSocketMeta {
                            is_socket: true,
                            mode: 0o606,
                            uid: other_uid,
                            gid,
                        }),
                    ),
                    accessible_socket("/run/user/1000/.ydotool_socket"),
                ],
            );
            assert_eq!(
                grouped.selection,
                YdotoolSocketSelection::Inject("/run/user/1000/.ydotool_socket".to_string()),
                "mode 0606 must not be writable for group {}",
                gid
            );
            assert!(
                grouped.checked.join("; ").contains("is not writable"),
                "group {}",
                gid
            );
        }

        let outsider_gid = (0..u32::MAX)
            .find(|gid| *gid != egid && !groups.contains(gid))
            .expect("a gid outside the process groups");
        let outsider = select_ydotool_socket(
            None,
            &[candidate(
                "/tmp/.ydotool_socket",
                Some(YdotoolSocketMeta {
                    is_socket: true,
                    mode: 0o606,
                    uid: other_uid,
                    gid: outsider_gid,
                }),
            )],
        );
        assert_eq!(
            outsider.selection,
            YdotoolSocketSelection::Inject("/tmp/.ydotool_socket".to_string())
        );
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn blank_ydotool_socket_falls_through_to_candidate_search() {
        for inherited in ["", "   ", "\t\n"] {
            let resolution = select_ydotool_socket(
                Some(inherited),
                &[
                    candidate("/run/user/1000/.ydotool_socket", None),
                    accessible_socket("/tmp/.ydotool_socket"),
                ],
            );
            assert_eq!(
                resolution.selection,
                YdotoolSocketSelection::Inject("/tmp/.ydotool_socket".to_string()),
                "inherited value {:?}",
                inherited
            );
            let mut command = Command::new("ydotool");
            configure_ydotool_socket(&mut command, &resolution);
            assert_eq!(
                child_socket(&command),
                Some(std::ffi::OsStr::new("/tmp/.ydotool_socket")),
                "inherited value {:?}",
                inherited
            );
        }
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn blank_ydotool_socket_is_cleared_when_the_client_default_is_used() {
        for inherited in ["", "   ", "\t\n"] {
            let resolution = select_ydotool_socket(
                Some(inherited),
                &[
                    candidate("/run/user/1000/.ydotool_socket", None),
                    candidate("/tmp/.ydotool_socket", None),
                ],
            );
            assert_eq!(
                resolution.selection,
                YdotoolSocketSelection::ClientDefault,
                "inherited value {:?}",
                inherited
            );
            let mut command = Command::new("ydotool");
            configure_ydotool_socket(&mut command, &resolution);
            assert!(
                ydotool_socket_was_removed(&command),
                "blank YDOTOOL_SOCKET {:?} must be removed so ydotool uses its client default",
                inherited
            );
            let failure = format_ydotool_failure("", "", Some(2), &resolution);
            assert!(
                failure.contains("YDOTOOL_SOCKET left unset"),
                "inherited value {:?}",
                inherited
            );
        }
    }

    #[test]
    fn auto_submit_requires_setting_enabled() {
        assert!(!should_send_auto_submit(false, PasteMethod::CtrlV));
        assert!(!should_send_auto_submit(false, PasteMethod::Direct));
    }

    #[test]
    fn auto_submit_skips_none_paste_method() {
        assert!(!should_send_auto_submit(true, PasteMethod::None));
    }

    #[test]
    fn auto_submit_runs_for_active_paste_methods() {
        assert!(should_send_auto_submit(true, PasteMethod::CtrlV));
        assert!(should_send_auto_submit(true, PasteMethod::Direct));
        assert!(should_send_auto_submit(true, PasteMethod::CtrlShiftV));
        assert!(should_send_auto_submit(true, PasteMethod::ShiftInsert));
    }

    #[test]
    fn clipboard_is_restored_before_key_injection_error_is_returned() {
        let restored = Cell::new(false);
        let result = finish_clipboard_paste(Err("input failed".into()), 0, || {
            restored.set(true);
        });

        assert_eq!(result.unwrap_err(), "input failed");
        assert!(restored.get());
    }

    #[cfg(unix)]
    #[test]
    fn external_script_does_not_wait_for_inherited_stdio() {
        use std::fs;
        use std::os::unix::fs::PermissionsExt;
        use std::sync::mpsc;
        use std::thread;

        let script_path = std::env::temp_dir().join(format!(
            "handy-external-script-{}-{}.sh",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .expect("system clock should be after UNIX_EPOCH")
                .as_nanos()
        ));
        fs::write(&script_path, "#!/bin/sh\nsleep 3 &\nexit 0\n").expect("write external script");
        let mut permissions = fs::metadata(&script_path)
            .expect("read external script metadata")
            .permissions();
        permissions.set_mode(0o700);
        fs::set_permissions(&script_path, permissions).expect("make external script executable");

        let (sender, receiver) = mpsc::channel();
        let script_path_for_thread = script_path.clone();
        thread::spawn(move || {
            let result =
                paste_via_external_script("test", script_path_for_thread.to_str().unwrap());
            sender.send(result).expect("send script result");
        });

        let result = receiver
            .recv_timeout(Duration::from_secs(1))
            .expect("external script should return without waiting for its child");
        fs::remove_file(script_path).expect("remove external script");
        assert!(result.is_ok());
    }
}
