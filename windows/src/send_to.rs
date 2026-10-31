//! Explorer Send To integration. Requests are atomically published before the
//! single-instance check, so even a concurrent cold launch cannot lose files.
use anyhow::{bail, Context, Result};
use std::ffi::OsString;
use std::os::windows::ffi::{OsStrExt, OsStringExt};
use std::path::PathBuf;

fn inbox() -> Result<PathBuf> {
    Ok(PathBuf::from(std::env::var_os("LOCALAPPDATA").context("LOCALAPPDATA is unavailable")?)
        .join("Bridge").join("send-to"))
}

pub fn publish_args() -> Result<bool> {
    let mut args = std::env::args_os().skip(1);
    if args.next().as_deref() != Some(std::ffi::OsStr::new("--send")) {
        return Ok(false);
    }
    let paths: Vec<_> = args.collect();
    if paths.is_empty() { bail!("No files were selected"); }
    let mut bytes = Vec::new();
    for path in paths {
        // Preserve Windows filenames, including non-Unicode UTF-16 names.
        let path = std::path::absolute(PathBuf::from(path))?;
        for unit in path.as_os_str().encode_wide().chain(std::iter::once(0)) {
            bytes.extend_from_slice(&unit.to_le_bytes());
        }
    }
    if bytes.len() > 1024 * 1024 { bail!("Too many file paths"); }
    let dir = inbox()?;
    std::fs::create_dir_all(&dir)?;
    let stem = format!("{}-{:016x}", std::process::id(), rand::random::<u64>());
    let temp = dir.join(format!("{stem}.tmp"));
    let ready = dir.join(format!("{stem}.request"));
    std::fs::write(&temp, bytes)?;
    if let Err(e) = std::fs::rename(&temp, ready) {
        let _ = std::fs::remove_file(temp);
        return Err(e.into());
    }
    Ok(true)
}

pub fn drain() -> Vec<PathBuf> {
    let mut paths = Vec::new();
    let Ok(dir) = inbox() else { return paths };
    let Ok(entries) = std::fs::read_dir(dir) else { return paths };
    for entry in entries.flatten() {
        let file = entry.path();
        if file.extension().and_then(|s| s.to_str()) != Some("request") { continue; }
        let result = (|| -> Result<Vec<PathBuf>> {
            let meta = std::fs::metadata(&file)?;
            if meta.len() > 1024 * 1024 { bail!("request too large"); }
            let bytes = std::fs::read(&file)?;
            if bytes.len() % 2 != 0 { bail!("invalid request"); }
            let units: Vec<u16> = bytes.chunks_exact(2).map(|b| u16::from_le_bytes([b[0], b[1]])).collect();
            if units.last() != Some(&0) { bail!("incomplete request"); }
            Ok(units.split(|u| *u == 0).filter(|s| !s.is_empty())
                .map(|s| PathBuf::from(OsString::from_wide(s))).collect())
        })();
        // Only the primary instance drains. Remove before enqueueing to avoid
        // resending on the next tick if deletion fails.
        if std::fs::remove_file(&file).is_err() { continue; }
        match result {
            Ok(batch) => paths.extend(batch),
            Err(e) => crate::log!("send-to", "invalid request: {e:#}"),
        }
    }
    paths
}

/// WScript creates a real .lnk, not a script receiving untrusted filenames.
/// Values travel in environment variables, never interpolated into PS source.
pub fn register(remove: bool) -> Result<()> {
    use std::os::windows::process::CommandExt;
    let script = r#"
$ErrorActionPreference = 'Stop'
$link = Join-Path ([Environment]::GetFolderPath('SendTo')) 'Bridge - Send to phone.lnk'
if ($env:BRIDGE_SENDTO_REMOVE -eq '1') {
    if (Test-Path -LiteralPath $link) { Remove-Item -LiteralPath $link }
} else {
    $shell = New-Object -ComObject WScript.Shell
    $shortcut = $shell.CreateShortcut($link)
    $shortcut.TargetPath = $env:BRIDGE_SENDTO_EXE
    $shortcut.Arguments = '--send'
    $shortcut.WorkingDirectory = [IO.Path]::GetDirectoryName($env:BRIDGE_SENDTO_EXE)
    $shortcut.IconLocation = $env:BRIDGE_SENDTO_EXE + ',0'
    $shortcut.Save()
}
"#;
    let output = std::process::Command::new("powershell.exe")
        .args(["-NoProfile", "-NonInteractive", "-Command", script])
        .env("BRIDGE_SENDTO_EXE", std::env::current_exe()?)
        .env("BRIDGE_SENDTO_REMOVE", if remove { "1" } else { "0" })
        .creation_flags(0x08000000).output().context("Starting PowerShell")?;
    if !output.status.success() {
        bail!("Send To registration failed: {}", String::from_utf8_lossy(&output.stderr));
    }
    Ok(())
}
