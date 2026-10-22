#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]

use serde::Serialize;
use serde_json::{json, Value};
use std::ffi::OsString;
use std::io::{self, Read};
use std::path::{Path, PathBuf};
use std::process::Command;
use std::time::{SystemTime, UNIX_EPOCH};
use std::fs;

type R<T> = Result<T, String>;

fn e<E: std::fmt::Display>(x: E) -> String {
    x.to_string()
}

#[cfg(windows)]
fn quiet(c: &mut Command) -> &mut Command {
    use std::os::windows::process::CommandExt;
    c.creation_flags(0x08000000)
}
#[cfg(not(windows))]
fn quiet(c: &mut Command) -> &mut Command {
    c
}

fn ms(t: io::Result<SystemTime>) -> f64 {
    t.ok()
        .and_then(|t| t.duration_since(UNIX_EPOCH).ok())
        .map(|d| d.as_secs_f64() * 1000.0)
        .unwrap_or(0.0)
}

fn name(p: &str) -> R<OsString> {
    Path::new(p).file_name().map(|n| n.to_owned()).ok_or_else(|| "Invalid path".to_string())
}

fn rm_path(p: &Path) -> io::Result<()> {
    if p.is_dir() { fs::remove_dir_all(p) } else { fs::remove_file(p) }
}

fn cp(src: &Path, dst: &Path) -> io::Result<()> {
    if dst.exists() {
        return Err(io::Error::new(io::ErrorKind::AlreadyExists, format!("{} already exists", dst.display())));
    }
    if dst.starts_with(src) {
        return Err(io::Error::new(io::ErrorKind::InvalidInput, "Cannot copy a folder into itself"));
    }
    if src.is_dir() {
        fs::create_dir_all(dst)?;
        for ent in fs::read_dir(src)? {
            let ent = ent?;
            cp(&ent.path(), &dst.join(ent.file_name()))?;
        }
    } else {
        fs::copy(src, dst)?;
    }
    Ok(())
}

// `c` must be a constant script. User data goes in through environment variables,
// never spliced into the script text (PowerShell treats curly quotes as quotes, so escaping is unsafe).
fn ps(c: &str, env: &[(&str, &str)]) -> R<()> {
    let mut cmd = Command::new("powershell.exe");
    cmd.args(["-NoProfile", "-NonInteractive", "-Command", c]);
    for (k, v) in env {
        cmd.env(k, v);
    }
    let o = quiet(&mut cmd).output().map_err(e)?;
    if o.status.success() { Ok(()) } else { Err(String::from_utf8_lossy(&o.stderr).trim().to_string()) }
}

// Err if `dst` is taken. fs::rename silently replaces existing files on Windows.
// A case-only rename of the same item (a.txt -> A.txt) is allowed on Windows.
fn vacant(src: &Path, dst: &Path) -> io::Result<()> {
    let same = cfg!(windows) && src.to_string_lossy().to_lowercase() == dst.to_string_lossy().to_lowercase();
    if !same && fs::symlink_metadata(dst).is_ok() {
        return Err(io::Error::new(io::ErrorKind::AlreadyExists, format!("{} already exists", dst.display())));
    }
    Ok(())
}

fn known() -> Vec<(String, String)> {
    let s = |n: &str, p: Option<PathBuf>| p.map(|p| (n.to_string(), p.to_string_lossy().into_owned()));
    [
        s("Desktop", dirs::desktop_dir()),
        s("Documents", dirs::document_dir()),
        s("Downloads", dirs::download_dir()),
        s("Pictures", dirs::picture_dir()),
        s("Music", dirs::audio_dir()),
        s("Videos", dirs::video_dir()),
        s("Home", dirs::home_dir()),
    ]
    .into_iter()
    .flatten()
    .collect()
}

fn roots() -> Vec<String> {
    if cfg!(windows) {
        (b'C'..=b'Z').map(|c| format!("{}:\\", c as char)).filter(|p| Path::new(p).exists()).collect()
    } else {
        vec!["/".to_string()]
    }
}

#[derive(Serialize)]
struct Entry {
    name: String,
    hidden: bool,
    dir: bool,
    size: u64,
    mtime: f64,
}

fn is_hidden(m: &fs::Metadata) -> bool {
    #[cfg(windows)]
    {
        use std::os::windows::fs::MetadataExt;
        m.file_attributes() & 0x2 != 0
    }
    #[cfg(not(windows))]
    {
        let _ = m;
        false
    }
}

#[tauri::command]
async fn list(dir: String) -> R<Vec<Entry>> {
    let mut v = vec![];
    for ent in fs::read_dir(&dir).map_err(e)? {
        let ent = match ent {
            Ok(x) => x,
            Err(_) => continue,
        };
        let md = fs::metadata(ent.path()).or_else(|_| ent.metadata());
        let (d, s, m, h) = match md {
            Ok(m) => (m.is_dir(), m.len(), ms(m.modified()), is_hidden(&m)),
            Err(_) => (false, 0, 0.0, false),
        };
        let nm = ent.file_name().to_string_lossy().into_owned();
        v.push(Entry { hidden: h || nm.starts_with('.'), name: nm, dir: d, size: if d { 0 } else { s }, mtime: m });
    }
    Ok(v)
}

#[tauri::command]
async fn places() -> Value {
    json!({ "places": known(), "drives": roots() })
}

#[tauri::command]
async fn pcinfo() -> Value {
    let disks = sysinfo::Disks::new_with_refreshed_list();
    let ds: Vec<Value> = roots()
        .into_iter()
        .map(|p| {
            let (t, f, n) = disks
                .list()
                .iter()
                .find(|d| d.mount_point().to_string_lossy().eq_ignore_ascii_case(&p))
                .map(|d| (d.total_space(), d.available_space(), d.name().to_string_lossy().into_owned()))
                .unwrap_or((0, 0, String::new()));
            json!({ "p": p, "total": t, "free": f, "name": n })
        })
        .collect();
    json!({ "places": known(), "drives": ds })
}

#[tauri::command]
async fn copy(items: Vec<String>, dest: String) -> R<()> {
    for s in items {
        cp(Path::new(&s), &Path::new(&dest).join(name(&s)?)).map_err(e)?;
    }
    Ok(())
}

#[tauri::command]
async fn mv(items: Vec<String>, dest: String) -> R<()> {
    for s in items {
        let src = Path::new(&s);
        let t = Path::new(&dest).join(name(&s)?);
        if t == src {
            continue;
        }
        vacant(src, &t).map_err(e)?;
        match fs::rename(src, &t) {
            Ok(()) => {}
            // 17 = ERROR_NOT_SAME_DEVICE (Windows), 18 = EXDEV (Unix): only then copy + delete
            Err(er) if matches!(er.raw_os_error(), Some(17) | Some(18)) => {
                cp(src, &t).map_err(e)?;
                rm_path(src).map_err(e)?;
            }
            Err(er) => return Err(e(er)),
        }
    }
    Ok(())
}

#[tauri::command]
async fn recycle(items: Vec<String>) -> R<()> {
    trash::delete_all(&items).map_err(e)
}

#[tauri::command]
async fn rm(items: Vec<String>) -> R<()> {
    for s in items {
        rm_path(Path::new(&s)).map_err(e)?;
    }
    Ok(())
}

#[tauri::command]
async fn mkdir(p: String) -> R<()> {
    fs::create_dir(&p).map_err(e)
}

#[tauri::command]
async fn rename(a: String, b: String) -> R<()> {
    vacant(Path::new(&a), Path::new(&b)).map_err(e)?;
    fs::rename(&a, &b).map_err(e)
}

#[tauri::command]
async fn open_path(p: String) -> R<()> {
    open::that(&p).map_err(e)
}

#[tauri::command]
async fn read_text(p: String) -> R<String> {
    let mut f = fs::File::open(&p).map_err(e)?;
    let mut b = vec![0u8; 30000];
    let n = f.read(&mut b).map_err(e)?;
    b.truncate(n);
    Ok(String::from_utf8_lossy(&b).into_owned())
}

#[tauri::command]
async fn clip(t: String) -> R<()> {
    arboard::Clipboard::new().and_then(|mut c| c.set_text(t)).map_err(e)
}

#[tauri::command]
async fn show(p: String) -> R<()> {
    #[cfg(windows)]
    {
        use std::os::windows::process::CommandExt;
        Command::new("explorer").raw_arg(format!("/select,\"{}\"", p)).spawn().map_err(e)?;
    }
    #[cfg(not(windows))]
    {
        open::that(Path::new(&p).parent().unwrap_or(Path::new("/"))).map_err(e)?;
    }
    Ok(())
}

#[tauri::command]
async fn openwith(p: String) -> R<()> {
    // Same "openas" shell verb Explorer uses; errors are reported instead of swallowed
    ps("Start-Process -FilePath $env:VO_P -Verb OpenAs", &[("VO_P", &p)])
}

#[derive(Serialize)]
struct App {
    name: String,
    exe: String,
}

#[cfg(windows)]
mod reg {
    use super::App;
    use std::path::Path;
    use winreg::enums::*;
    use winreg::types::FromRegValue;
    use winreg::RegKey;

    fn expand(s: &str) -> String {
        let mut out = String::new();
        let mut rest = s;
        while let Some(i) = rest.find('%') {
            out.push_str(&rest[..i]);
            let after = &rest[i + 1..];
            match after.find('%') {
                Some(j) => {
                    match std::env::var(&after[..j]) {
                        Ok(v) => out.push_str(&v),
                        Err(_) => {
                            out.push('%');
                            out.push_str(&after[..j]);
                            out.push('%');
                        }
                    }
                    rest = &after[j + 1..];
                }
                None => {
                    out.push('%');
                    rest = after;
                }
            }
        }
        out.push_str(rest);
        out
    }

    // First .exe in a registry command line such as: "C:\Program Files\VLC\vlc.exe" --x "%1"
    fn exe_of(cmd: &str) -> Option<String> {
        let c = expand(cmd.trim());
        let p = if let Some(r) = c.strip_prefix('"') {
            r.split('"').next()?.to_string()
        } else {
            let i = c.to_ascii_lowercase().find(".exe")?;
            c[..i + 4].to_string()
        };
        Path::new(&p).is_file().then_some(p)
    }

    fn prog_exe(progid: &str) -> Option<String> {
        let k = RegKey::predef(HKEY_CLASSES_ROOT).open_subkey(format!("{}\\shell\\open\\command", progid)).ok()?;
        exe_of(&k.get_value::<String, _>("").ok()?)
    }

    fn named_exe(name: &str) -> Option<String> {
        if let Ok(k) = RegKey::predef(HKEY_CLASSES_ROOT).open_subkey(format!("Applications\\{}\\shell\\open\\command", name)) {
            if let Some(p) = k.get_value::<String, _>("").ok().and_then(|v| exe_of(&v)) {
                return Some(p);
            }
        }
        for hive in [HKEY_CURRENT_USER, HKEY_LOCAL_MACHINE] {
            let key = format!("Software\\Microsoft\\Windows\\CurrentVersion\\App Paths\\{}", name);
            if let Ok(k) = RegKey::predef(hive).open_subkey(key) {
                if let Ok(v) = k.get_value::<String, _>("") {
                    let p = expand(v.trim().trim_matches('"'));
                    if Path::new(&p).is_file() {
                        return Some(p);
                    }
                }
            }
        }
        None
    }

    fn title(exe: &str) -> String {
        let stem = Path::new(exe).file_stem().map(|s| s.to_string_lossy().into_owned()).unwrap_or_default();
        let mut c = stem.chars();
        c.next().map(|f| f.to_uppercase().collect::<String>() + c.as_str()).unwrap_or_default()
    }

    fn label(exe: &str) -> String {
        let file = Path::new(exe).file_name().map(|s| s.to_string_lossy().into_owned()).unwrap_or_default();
        RegKey::predef(HKEY_CLASSES_ROOT)
            .open_subkey(format!("Applications\\{}", file))
            .ok()
            .and_then(|k| k.get_value::<String, _>("FriendlyAppName").ok())
            .filter(|n| !n.is_empty() && !n.starts_with('@'))
            .unwrap_or_else(|| title(exe))
    }

    fn push(out: &mut Vec<App>, exe: Option<String>) {
        if let Some(exe) = exe {
            let lower = exe.to_ascii_lowercase();
            if lower.ends_with("\\openwith.exe") || out.iter().any(|a| a.exe.eq_ignore_ascii_case(&exe)) {
                return;
            }
            out.push(App { name: label(&exe), exe });
        }
    }

    // Programs Windows associates with this extension, like Explorer's "Open with" list
    pub fn apps(ext: &str) -> Vec<App> {
        let ext = format!(".{}", ext.trim_start_matches('.'));
        let hkcr = RegKey::predef(HKEY_CLASSES_ROOT);
        let fe = format!("Software\\Microsoft\\Windows\\CurrentVersion\\Explorer\\FileExts\\{}", ext);
        let user = RegKey::predef(HKEY_CURRENT_USER).open_subkey(&fe).ok();
        let mut out: Vec<App> = vec![];

        if let Some(id) = user
            .as_ref()
            .and_then(|k| k.open_subkey("UserChoice").ok())
            .and_then(|k| k.get_value::<String, _>("ProgId").ok())
        {
            push(&mut out, match id.strip_prefix("Applications\\") { Some(n) => named_exe(n), None => prog_exe(&id) });
        }
        if let Some(id) = hkcr.open_subkey(&ext).ok().and_then(|k| k.get_value::<String, _>("").ok()) {
            push(&mut out, prog_exe(&id));
        }
        if let Some(k) = user.as_ref().and_then(|k| k.open_subkey("OpenWithList").ok()) {
            for (n, v) in k.enum_values().flatten() {
                if n != "MRUList" {
                    if let Ok(name) = String::from_reg_value(&v) {
                        push(&mut out, named_exe(&name));
                    }
                }
            }
        }
        if let Some(k) = user.as_ref().and_then(|k| k.open_subkey("OpenWithProgids").ok()) {
            for (n, _) in k.enum_values().flatten() {
                push(&mut out, prog_exe(&n));
            }
        }
        if let Some(k) = hkcr.open_subkey(format!("{}\\OpenWithList", ext)).ok() {
            for n in k.enum_keys().flatten() {
                push(&mut out, named_exe(&n));
            }
        }
        out
    }
}

#[tauri::command]
async fn apps(ext: String) -> Vec<App> {
    #[cfg(windows)]
    {
        reg::apps(&ext)
    }
    #[cfg(not(windows))]
    {
        let _ = ext;
        vec![]
    }
}

// Only programs that apps() offers for this file's extension may be launched
#[tauri::command]
async fn openapp(exe: String, p: String) -> R<()> {
    #[cfg(windows)]
    {
        let ext = Path::new(&p).extension().map(|x| x.to_string_lossy().into_owned()).unwrap_or_default();
        if !reg::apps(&ext).iter().any(|a| a.exe.eq_ignore_ascii_case(&exe)) {
            return Err("That program is not registered for this file type".to_string());
        }
        Command::new(exe).arg(p).spawn().map(|_| ()).map_err(e)
    }
    #[cfg(not(windows))]
    {
        let _ = (exe, p);
        Err("Not supported on this platform".to_string())
    }
}

#[tauri::command]
async fn term(d: String) -> R<()> {
    quiet(Command::new("cmd.exe").args(["/c", "start", "cmd.exe"]).current_dir(&d)).spawn().map_err(e)?;
    Ok(())
}

#[tauri::command]
async fn dup(p: String) -> R<()> {
    let src = Path::new(&p);
    let ext = src.extension().map(|x| format!(".{}", x.to_string_lossy())).unwrap_or_default();
    let stem = p[..p.len() - ext.len()].to_string();
    let mut n = format!("{} - Copy{}", stem, ext);
    let mut i = 2;
    while Path::new(&n).exists() {
        n = format!("{} - Copy ({}){}", stem, i, ext);
        i += 1;
    }
    cp(src, Path::new(&n)).map_err(e)
}

#[tauri::command]
async fn newfile(p: String) -> R<()> {
    fs::OpenOptions::new().write(true).create_new(true).open(&p).map(|_| ()).map_err(e)
}

#[tauri::command]
async fn link(p: String, d: String) -> R<()> {
    let lnk = Path::new(&d).join(format!("{} - Shortcut.lnk", name(&p)?.to_string_lossy()));
    ps(
        "$s=(New-Object -ComObject WScript.Shell).CreateShortcut($env:VO_LNK);$s.TargetPath=$env:VO_TGT;$s.Save()",
        &[("VO_LNK", &lnk.to_string_lossy()), ("VO_TGT", &p)],
    )
}

#[tauri::command]
async fn zip(items: Vec<String>, out: String) -> R<()> {
    // '|' cannot appear in a Windows path, so it is a safe separator
    let list = items.join("|");
    ps(
        "Compress-Archive -LiteralPath ($env:VO_ITEMS -split '\\|') -DestinationPath $env:VO_OUT",
        &[("VO_ITEMS", &list), ("VO_OUT", &out)],
    )
}

#[tauri::command]
async fn unzip(z: String, d: String) -> R<()> {
    ps(
        "Expand-Archive -LiteralPath $env:VO_ZIP -DestinationPath $env:VO_DEST",
        &[("VO_ZIP", &z), ("VO_DEST", &d)],
    )
}

fn walk(d: &Path, s: &mut u64, f: &mut u64, n: &mut u64) {
    if let Ok(rd) = fs::read_dir(d) {
        for ent in rd.flatten() {
            match ent.metadata() {
                Ok(m) if m.is_dir() => {
                    *n += 1;
                    walk(&ent.path(), s, f, n);
                }
                Ok(m) => {
                    *f += 1;
                    *s += m.len();
                }
                Err(_) => {}
            }
        }
    }
}

#[tauri::command]
async fn props(p: String) -> R<Value> {
    let m = fs::metadata(&p).map_err(e)?;
    let (mut s, mut f, mut d) = (m.len(), 0u64, 0u64);
    if m.is_dir() {
        s = 0;
        walk(Path::new(&p), &mut s, &mut f, &mut d);
    }
    Ok(json!({ "size": s, "files": f, "dirs": d, "dir": m.is_dir(), "ctime": ms(m.created()), "mtime": ms(m.modified()), "atime": ms(m.accessed()) }))
}

#[tauri::command]
async fn rar(args: Vec<String>) -> R<()> {
    let exe = ["ProgramFiles", "ProgramFiles(x86)", "ProgramW6432"]
        .iter()
        .filter_map(|k| std::env::var_os(k))
        .map(|d| PathBuf::from(d).join("WinRAR").join("WinRAR.exe"))
        .find(|p| p.exists())
        .ok_or_else(|| "WinRAR was not found. Install it from rarlab.com.".to_string())?;
    let mut c = Command::new(exe);
    // WinRAR parses its own command line, so quote paths by hand (raw_arg) the way its docs show,
    // e.g. x -- "C:\a.rar" "C:\out\"  (a trailing backslash must survive intact)
    #[cfg(windows)]
    {
        use std::os::windows::process::CommandExt;
        for a in &args {
            if a.contains(|ch: char| " \\?:".contains(ch)) {
                c.raw_arg(format!("\"{}\"", a));
            } else {
                c.raw_arg(a);
            }
        }
    }
    #[cfg(not(windows))]
    {
        c.args(&args);
    }
    // Blocks until the WinRAR window is closed or finishes, so the file list refreshes afterwards
    let st = c.status().map_err(e)?;
    if st.success() { Ok(()) } else { Err(format!("WinRAR exited with code {}", st.code().unwrap_or(-1))) }
}

fn main() {
    tauri::Builder::default()
        .invoke_handler(tauri::generate_handler![
            list, places, pcinfo, copy, mv, recycle, rm, mkdir, rename, open_path, read_text, clip, show, openwith,
            term, apps, openapp, dup, newfile, link, zip, unzip, props, rar
        ])
        .run(tauri::generate_context!())
        .expect("error while running Vault Opus");
}
