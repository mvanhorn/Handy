use crate::input::{self, EnigoState};
#[cfg(target_os = "linux")]
use crate::settings::TypingTool;
use crate::settings::{get_settings, AutoSubmitKey, ClipboardHandling, PasteMethod};
use enigo::{Direction, Enigo, Key, Keyboard};
use log::info;
#[cfg(any(target_os = "linux", test))]
use log::warn;
use std::process::Command;
use std::time::Duration;
use tauri::{AppHandle, Manager};
use tauri_plugin_clipboard_manager::ClipboardExt;

#[cfg(target_os = "linux")]
use crate::utils::{is_kde_wayland, is_wayland};

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
#[cfg(any(target_os = "linux", test))]
enum LinuxInputTool {
    Wtype,
    Kwtype,
    Dotool,
    Ydotool,
    Xdotool,
}

#[cfg(any(target_os = "linux", test))]
impl LinuxInputTool {
    fn name(self) -> &'static str {
        match self {
            Self::Wtype => "wtype",
            Self::Kwtype => "kwtype",
            Self::Dotool => "dotool",
            Self::Ydotool => "ydotool",
            Self::Xdotool => "xdotool",
        }
    }
}

/// Runs eligible Linux input tools in order.
///
/// Auto mode continues after runtime failures so another native tool, or
/// ultimately Enigo, can handle the input. Explicit selections remain
/// fail-fast and return the selected tool's error.
#[cfg(any(target_os = "linux", test))]
fn try_linux_input_candidates<const N: usize>(
    candidates: [(LinuxInputTool, bool); N],
    continue_on_error: bool,
    mut run: impl FnMut(LinuxInputTool) -> Result<(), String>,
) -> Result<bool, String> {
    for (tool, available) in candidates {
        if !available {
            continue;
        }

        match run(tool) {
            Ok(()) => return Ok(true),
            Err(error) if continue_on_error => {
                warn!(
                    "{} failed while injecting input: {}; trying the next fallback",
                    tool.name(),
                    error
                );
            }
            Err(error) => return Err(error),
        }
    }

    Ok(false)
}

/// Pastes text using the clipboard: saves current content, writes text, sends paste keystroke, restores clipboard.
fn paste_via_clipboard(
    enigo: &mut Enigo,
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
    // On Wayland, prefer wl-copy for better compatibility (especially with umlauts)
    #[cfg(target_os = "linux")]
    let write_result = if is_wayland() && is_wl_copy_available() {
        info!("Using wl-copy for clipboard write on Wayland");
        write_clipboard_via_wl_copy(text)
    } else {
        clipboard
            .write_text(text)
            .map_err(|e| format!("Failed to write to clipboard: {}", e))
    };

    #[cfg(not(target_os = "linux"))]
    let write_result = clipboard
        .write_text(text)
        .map_err(|e| format!("Failed to write to clipboard: {}", e));

    write_result?;

    std::thread::sleep(Duration::from_millis(paste_delay_ms));

    // Send paste key combo
    #[cfg(target_os = "linux")]
    let key_combo_sent = try_send_key_combo_linux(paste_method)?;

    #[cfg(not(target_os = "linux"))]
    let key_combo_sent = false;

    // Fall back to enigo if no native tool handled it
    if !key_combo_sent {
        match paste_method {
            PasteMethod::CtrlV => input::send_paste_ctrl_v(enigo)?,
            PasteMethod::CtrlShiftV => input::send_paste_ctrl_shift_v(enigo)?,
            PasteMethod::ShiftInsert => input::send_paste_shift_insert(enigo)?,
            _ => return Err("Invalid paste method for clipboard paste".into()),
        }
    }

    std::thread::sleep(Duration::from_millis(paste_delay_after_ms));

    // Restore original clipboard content.
    // Text takes priority so this path stays identical to the previous behavior;
    // an image is only restored when the clipboard held no text at all, which is
    // the case that used to silently wipe screenshots.
    if let Some(clipboard_content) = saved_text {
        // On Wayland, prefer wl-copy for better compatibility
        #[cfg(target_os = "linux")]
        if is_wayland() && is_wl_copy_available() {
            let _ = write_clipboard_via_wl_copy(&clipboard_content);
        } else {
            let _ = clipboard.write_text(&clipboard_content);
        }

        #[cfg(not(target_os = "linux"))]
        let _ = clipboard.write_text(&clipboard_content);
    } else if let Some(image) = saved_image {
        info!("Restoring image to clipboard");
        let _ = clipboard.write_image(&image);
    } else {
        // Nothing was there to begin with — don't leave the transcription behind.
        let _ = clipboard.clear();
    }

    Ok(())
}

/// Attempts to send a key combination using Linux-native tools.
/// Returns `Ok(true)` if a native tool handled it, `Ok(false)` to fall back to enigo.
#[cfg(target_os = "linux")]
fn try_send_key_combo_linux(paste_method: &PasteMethod) -> Result<bool, String> {
    if is_wayland() {
        // Wayland: prefer wtype (but not on KDE), then dotool, then ydotool
        // Note: wtype doesn't work on KDE (no zwp_virtual_keyboard_manager_v1 support)
        let candidates = [
            (
                LinuxInputTool::Wtype,
                !is_kde_wayland() && is_wtype_available(),
            ),
            (LinuxInputTool::Dotool, is_dotool_available()),
            (LinuxInputTool::Ydotool, is_ydotool_available()),
        ];

        return try_linux_input_candidates(candidates, true, |tool| match tool {
            LinuxInputTool::Wtype => {
                info!("Using wtype for key combo");
                send_key_combo_via_wtype(paste_method)
            }
            LinuxInputTool::Dotool => {
                info!("Using dotool for key combo");
                send_key_combo_via_dotool(paste_method)
            }
            LinuxInputTool::Ydotool => {
                info!("Using ydotool for key combo");
                send_key_combo_via_ydotool(paste_method)
            }
            _ => unreachable!("unexpected Wayland key-combination tool"),
        });
    } else {
        // X11: prefer xdotool, then ydotool
        let candidates = [
            (LinuxInputTool::Xdotool, is_xdotool_available()),
            (LinuxInputTool::Ydotool, is_ydotool_available()),
        ];

        return try_linux_input_candidates(candidates, true, |tool| match tool {
            LinuxInputTool::Xdotool => {
                info!("Using xdotool for key combo");
                send_key_combo_via_xdotool(paste_method)
            }
            LinuxInputTool::Ydotool => {
                info!("Using ydotool for key combo");
                send_key_combo_via_ydotool(paste_method)
            }
            _ => unreachable!("unexpected X11 key-combination tool"),
        });
    }
}

/// Attempts to type text directly using Linux-native tools.
/// Returns `Ok(true)` if a native tool handled it, `Ok(false)` to fall back to enigo.
#[cfg(target_os = "linux")]
fn try_direct_typing_linux(text: &str, preferred_tool: TypingTool) -> Result<bool, String> {
    // If user specified a tool, try only that one
    if preferred_tool != TypingTool::Auto {
        let (tool, available) = match preferred_tool {
            TypingTool::Wtype => (LinuxInputTool::Wtype, is_wtype_available()),
            TypingTool::Kwtype => (LinuxInputTool::Kwtype, is_kwtype_available()),
            TypingTool::Dotool => (LinuxInputTool::Dotool, is_dotool_available()),
            TypingTool::Ydotool => (LinuxInputTool::Ydotool, is_ydotool_available()),
            TypingTool::Xdotool => (LinuxInputTool::Xdotool, is_xdotool_available()),
            TypingTool::Auto => unreachable!("auto mode handled below"),
        };

        if !available {
            return Err(format!(
                "Typing tool {:?} is not available on this system",
                preferred_tool
            ));
        }

        return try_linux_input_candidates([(tool, true)], false, |tool| {
            info!("Using user-specified {}", tool.name());
            match tool {
                LinuxInputTool::Wtype => type_text_via_wtype(text),
                LinuxInputTool::Kwtype => type_text_via_kwtype(text),
                LinuxInputTool::Dotool => type_text_via_dotool(text),
                LinuxInputTool::Ydotool => type_text_via_ydotool(text),
                LinuxInputTool::Xdotool => type_text_via_xdotool(text),
            }
        });
    }

    // Auto mode - continue through the fallback chain after runtime failures.
    if is_wayland() {
        let kde_wayland = is_kde_wayland();
        // Wayland: prefer wtype, then dotool, then ydotool
        // Note: wtype doesn't work on KDE (no zwp_virtual_keyboard_manager_v1 support)
        let candidates = [
            (LinuxInputTool::Kwtype, kde_wayland && is_kwtype_available()),
            (LinuxInputTool::Wtype, !kde_wayland && is_wtype_available()),
            (LinuxInputTool::Dotool, is_dotool_available()),
            (LinuxInputTool::Ydotool, is_ydotool_available()),
        ];

        return try_linux_input_candidates(candidates, true, |tool| {
            info!("Using {} for direct text input", tool.name());
            match tool {
                LinuxInputTool::Wtype => type_text_via_wtype(text),
                LinuxInputTool::Kwtype => type_text_via_kwtype(text),
                LinuxInputTool::Dotool => type_text_via_dotool(text),
                LinuxInputTool::Ydotool => type_text_via_ydotool(text),
                LinuxInputTool::Xdotool => unreachable!("unexpected Wayland direct input tool"),
            }
        });
    } else {
        // X11: prefer xdotool, then ydotool
        let candidates = [
            (LinuxInputTool::Xdotool, is_xdotool_available()),
            (LinuxInputTool::Ydotool, is_ydotool_available()),
        ];

        return try_linux_input_candidates(candidates, true, |tool| {
            info!("Using {} for direct text input", tool.name());
            match tool {
                LinuxInputTool::Xdotool => type_text_via_xdotool(text),
                LinuxInputTool::Ydotool => type_text_via_ydotool(text),
                _ => unreachable!("unexpected X11 direct input tool"),
            }
        });
    }
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
    let output = Command::new("ydotool")
        .arg("type")
        .arg("--")
        .arg(text)
        .output()
        .map_err(|e| format!("Failed to execute ydotool: {}", e))?;

    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        return Err(format!("ydotool failed: {}", stderr));
    }

    Ok(())
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
        PasteMethod::CtrlV => vec!["-M", "ctrl", "-k", "v"],
        PasteMethod::ShiftInsert => vec!["-M", "shift", "-k", "Insert"],
        PasteMethod::CtrlShiftV => vec!["-M", "ctrl", "-M", "shift", "-k", "v"],
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

/// Send a key combination (e.g., Ctrl+V) via ydotool (requires ydotoold daemon).
#[cfg(target_os = "linux")]
fn send_key_combo_via_ydotool(paste_method: &PasteMethod) -> Result<(), String> {
    // ydotool uses Linux input event keycodes with format <keycode>:<pressed>
    // where pressed is 1 for down, 0 for up. Keycodes: ctrl=29, shift=42, v=47, insert=110
    let args: Vec<&str> = match paste_method {
        PasteMethod::CtrlV => vec!["key", "29:1", "47:1", "47:0", "29:0"],
        PasteMethod::ShiftInsert => vec!["key", "42:1", "110:1", "110:0", "42:0"],
        PasteMethod::CtrlShiftV => vec!["key", "29:1", "42:1", "47:1", "47:0", "42:0", "29:0"],
        _ => return Err("Unsupported paste method".into()),
    };

    let output = Command::new("ydotool")
        .args(&args)
        .output()
        .map_err(|e| format!("Failed to execute ydotool: {}", e))?;

    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        return Err(format!("ydotool failed: {}", stderr));
    }

    Ok(())
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

    let output = Command::new(script_path)
        .arg(text)
        .output()
        .map_err(|e| format!("Failed to execute external script '{}': {}", script_path, e))?;

    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        let stdout = String::from_utf8_lossy(&output.stdout);
        return Err(format!(
            "External script '{}' failed with exit code {:?}. stderr: {}, stdout: {}",
            script_path,
            output.status.code(),
            stderr.trim(),
            stdout.trim()
        ));
    }

    Ok(())
}

/// Types text directly by simulating individual key presses.
fn paste_direct(
    enigo: &mut Enigo,
    text: &str,
    #[cfg(target_os = "linux")] typing_tool: TypingTool,
) -> Result<(), String> {
    #[cfg(target_os = "linux")]
    {
        if try_direct_typing_linux(text, typing_tool)? {
            return Ok(());
        }
        info!("Falling back to enigo for direct text input");
    }

    input::paste_text_direct(enigo, text)
}

fn send_return_key(enigo: &mut Enigo, key_type: AutoSubmitKey) -> Result<(), String> {
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

    // Get the managed Enigo instance
    let enigo_state = app_handle
        .try_state::<EnigoState>()
        .ok_or("Enigo state not initialized")?;
    let mut enigo = enigo_state
        .0
        .lock()
        .map_err(|e| format!("Failed to lock Enigo: {}", e))?;

    // Perform the paste operation
    match paste_method {
        PasteMethod::None => {
            info!("PasteMethod::None selected - skipping paste action");
        }
        PasteMethod::Direct => {
            paste_direct(
                &mut enigo,
                &text,
                #[cfg(target_os = "linux")]
                settings.typing_tool,
            )?;
        }
        PasteMethod::CtrlV | PasteMethod::CtrlShiftV | PasteMethod::ShiftInsert => {
            paste_via_clipboard(
                &mut enigo,
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
        send_return_key(&mut enigo, settings.auto_submit_key)?;
    }

    // After pasting, optionally copy to clipboard based on settings
    if settings.clipboard_handling == ClipboardHandling::CopyToClipboard {
        let clipboard = app_handle.clipboard();
        clipboard
            .write_text(&text)
            .map_err(|e| format!("Failed to copy to clipboard: {}", e))?;
    }

    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn auto_candidates_continue_after_runtime_failure() {
        for input_path in ["direct typing", "clipboard key combo"] {
            let mut attempted = Vec::new();
            let handled = try_linux_input_candidates(
                [
                    (LinuxInputTool::Wtype, true),
                    (LinuxInputTool::Dotool, true),
                    (LinuxInputTool::Ydotool, true),
                ],
                true,
                |tool| {
                    attempted.push(tool);
                    match tool {
                        LinuxInputTool::Wtype => Err(format!(
                            "{input_path}: compositor does not support virtual keyboard protocol"
                        )),
                        LinuxInputTool::Dotool => Ok(()),
                        _ => panic!("successful candidate should stop the chain"),
                    }
                },
            )
            .unwrap();

            assert!(handled);
            assert_eq!(
                attempted,
                vec![LinuxInputTool::Wtype, LinuxInputTool::Dotool]
            );
        }
    }

    #[test]
    fn auto_candidates_skip_unavailable_tools_in_order() {
        let mut attempted = Vec::new();
        let handled = try_linux_input_candidates(
            [
                (LinuxInputTool::Wtype, false),
                (LinuxInputTool::Dotool, true),
                (LinuxInputTool::Ydotool, true),
            ],
            true,
            |tool| {
                attempted.push(tool);
                Err("runtime failure".into())
            },
        )
        .unwrap();

        assert!(!handled);
        assert_eq!(
            attempted,
            vec![LinuxInputTool::Dotool, LinuxInputTool::Ydotool]
        );
    }

    #[test]
    fn explicit_candidate_returns_runtime_error_without_fallback() {
        let mut attempted = Vec::new();
        let result = try_linux_input_candidates([(LinuxInputTool::Wtype, true)], false, |tool| {
            attempted.push(tool);
            Err("wtype protocol failure".into())
        });

        assert_eq!(result, Err("wtype protocol failure".into()));
        assert_eq!(attempted, vec![LinuxInputTool::Wtype]);
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
}
