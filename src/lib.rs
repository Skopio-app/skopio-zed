use std::{
    env,
    fs,
    io::{Read, Write},
    path::{Path, PathBuf},
};

use serde::Deserialize;
use serde_json::Value;
use sha2::{Digest, Sha256};
use zed_extension_api as zed;

const EXT_REPO: &str = "Skopio-app/skopio-zed";
const CLI_LATEST_JSON_URL: &str =
    "https://github.com/Skopio-app/cli-releases/releases/latest/download/latest.json";

// Runtime tuning env keys (optional; we also pass these as args now)
const ENV_IDLE_SECS: &str = "SKOPIO_ZED_IDLE_SECS";
const ENV_SWITCH_GRACE_SECS: &str = "SKOPIO_ZED_SWITCH_GRACE_SECS";
const ENV_MIN_SESSION_SECS: &str = "SKOPIO_ZED_MIN_SESSION_SECS";

#[derive(Debug, Deserialize)]
struct LatestJson {
    version: String,
    assets: Assets,
}

#[derive(Debug, Deserialize)]
struct Assets {
    #[serde(rename = "darwin-aarch64")]
    darwin_aarch64: Option<Asset>,
    #[serde(rename = "darwin-x86_64")]
    darwin_x86_64: Option<Asset>,
}

#[derive(Debug, Deserialize)]
struct Asset {
    url: String,
    sha256: String,
    size: Option<u64>,
}

#[derive(Debug, Clone, Copy)]
enum MacArch {
    Aarch64,
    X8664,
}

fn current_mac_arch() -> Option<MacArch> {
    use zed::{Architecture as Arch, Os};

    let (os, arch) = zed::current_platform();
    match (os, arch) {
        (Os::Mac, Arch::Aarch64) => Some(MacArch::Aarch64),
        (Os::Mac, Arch::X8664) => Some(MacArch::X8664),
        _ => None,
    }
}

fn lsp_asset_name(arch: MacArch) -> &'static str {
    match arch {
        MacArch::Aarch64 => "skopio-lsp-aarch64-apple-darwin",
        MacArch::X8664 => "skopio-lsp-x86_64-apple-darwin",
    }
}

fn cli_bin_name(arch: MacArch) -> &'static str {
    match arch {
        MacArch::Aarch64 => "skopio-cli-darwin-aarch64",
        MacArch::X8664 => "skopio-cli-darwin-x86_64",
    }
}

fn to_lower_hex(bytes: &[u8]) -> String {
    const LUT: &[u8; 16] = b"0123456789abcdef";
    let mut s = String::with_capacity(bytes.len() * 2);
    for &b in bytes {
        s.push(LUT[(b >> 4) as usize] as char);
        s.push(LUT[(b & 0x0f) as usize] as char);
    }
    s
}

fn sha256_hex(path: &Path) -> Result<String, String> {
    let mut file = fs::File::open(path).map_err(|e| format!("open failed: {e}"))?;
    let mut hasher = Sha256::new();
    let mut buf = [0u8; 64 * 1024];
    loop {
        let n = file.read(&mut buf).map_err(|e| format!("read failed: {e}"))?;
        if n == 0 {
            break;
        }
        hasher.update(&buf[..n]);
    }
    Ok(to_lower_hex(&hasher.finalize()))
}

fn write_text(path: &Path, text: &str) -> Result<(), String> {
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent).map_err(|e| format!("mkdir failed: {e}"))?;
    }
    let mut f = fs::File::create(path).map_err(|e| format!("create failed: {e}"))?;
    f.write_all(text.as_bytes())
        .map_err(|e| format!("write failed: {e}"))?;
    Ok(())
}

fn cleanup_old_dirs(prefix: &str, keep: &str) {
    if let Ok(entries) = fs::read_dir(".") {
        for entry in entries.flatten() {
            let p = entry.path();
            if let Some(name) = p.file_name().and_then(|s| s.to_str()) {
                if name.starts_with(prefix) && name != keep {
                    let _ = fs::remove_dir_all(p);
                }
            }
        }
    }
}

/// Zed provides access to per-language-server settings via zed::settings::LspSettings.
fn read_lsp_settings(worktree: &zed::Worktree) -> Option<zed::settings::LspSettings> {
    zed::settings::LspSettings::for_worktree("skopio", worktree).ok()
}

fn json_u64(v: &Value, key: &str) -> Option<u64> {
    v.get(key).and_then(|x| x.as_u64())
}

fn json_i64(v: &Value, key: &str) -> Option<i64> {
    v.get(key).and_then(|x| x.as_i64())
}

fn json_str(v: &Value, key: &str) -> Option<String> {
    v.get(key).and_then(|x| x.as_str()).map(|s| s.to_string())
}

fn fetch_latest_json() -> Result<LatestJson, String> {
    let latest_path = "cli.latest.json";
    zed::download_file(
        CLI_LATEST_JSON_URL,
        latest_path,
        zed::DownloadedFileType::Uncompressed,
    )
    .map_err(|e| format!("download latest.json failed: {e}"))?;

    let s = fs::read_to_string(latest_path).map_err(|e| format!("read latest.json failed: {e}"))?;
    serde_json::from_str(&s).map_err(|e| format!("parse latest.json failed: {e}"))
}

fn ensure_cli_installed(arch: MacArch) -> Result<PathBuf, String> {
    let latest = fetch_latest_json()?;
    let asset = match arch {
        MacArch::Aarch64 => latest
            .assets
            .darwin_aarch64
            .ok_or_else(|| "latest.json missing assets.darwin-aarch64".to_string())?,
        MacArch::X8664 => latest
            .assets
            .darwin_x86_64
            .ok_or_else(|| "latest.json missing assets.darwin-x86_64".to_string())?,
    };

    let cli_name = cli_bin_name(arch);
    let version_dir = format!("skopio-cli-{}", latest.version);
    let cli_rel_path = Path::new(&version_dir).join(cli_name);

    let needs_install = !fs::metadata(&cli_rel_path).is_ok_and(|m| m.is_file());
    if needs_install {
        zed::download_file(&asset.url, &version_dir, zed::DownloadedFileType::Zip)
            .map_err(|e| format!("download cli zip failed: {e}"))?;
    }

    let zip_file = format!("{version_dir}.zip");
    if !Path::new(&zip_file).exists() {
        zed::download_file(&asset.url, &zip_file, zed::DownloadedFileType::Uncompressed)
            .map_err(|e| format!("download cli zip (file) failed: {e}"))?;
    }

    let got = sha256_hex(Path::new(&zip_file))?;
    let expected = asset.sha256.trim().to_lowercase();
    if got != expected {
        let _ = fs::remove_file(&zip_file);
        return Err(format!(
            "sha256 mismatch for {cli_name}.zip: expected {expected}, got {got}"
        ));
    }

    if needs_install {
        zed::download_file(&asset.url, &version_dir, zed::DownloadedFileType::Zip)
            .map_err(|e| format!("download cli zip (extract) failed: {e}"))?;
    }

    zed::make_file_executable(cli_rel_path.to_str().unwrap())
        .map_err(|e| format!("chmod +x failed: {e}"))?;

    let version_marker = Path::new(&version_dir).join(format!("{cli_name}.version"));
    write_text(&version_marker, &(latest.version.clone() + "\n"))?;

    cleanup_old_dirs("skopio-cli-", &version_dir);
    Ok(cli_rel_path)
}

fn ensure_lsp_installed(arch: MacArch) -> Result<PathBuf, String> {
    let release = zed::latest_github_release(
        EXT_REPO,
        zed::GithubReleaseOptions {
            require_assets: true,
            pre_release: false,
        },
    )
    .map_err(|e| format!("latest_github_release failed: {e}"))?;

    let asset_name = lsp_asset_name(arch);
    let asset = release
        .assets
        .iter()
        .find(|a| a.name == asset_name)
        .ok_or_else(|| format!("Missing release asset `{asset_name}` in {EXT_REPO}"))?;

    let version_dir = format!("skopio-lsp-{}", release.version);
    let lsp_rel_path = Path::new(&version_dir).join(asset_name);

    if !fs::metadata(&lsp_rel_path).is_ok_and(|m| m.is_file()) {
        zed::download_file(
            &asset.download_url,
            &version_dir,
            zed::DownloadedFileType::Uncompressed,
        )
        .map_err(|e| format!("download lsp failed: {e}"))?;
    }

    zed::make_file_executable(lsp_rel_path.to_str().unwrap())
        .map_err(|e| format!("chmod +x failed: {e}"))?;

    cleanup_old_dirs("skopio-lsp-", &version_dir);
    Ok(lsp_rel_path)
}

/// Turn a path into a string safe to pass back to Zed.
/// On Windows, Zed sometimes gives paths prefixed with `/` - strip that.
fn sanitize_path(path: &str) -> String {
    match zed::current_platform() {
        (zed::Os::Windows, _) => path.trim_start_matches('/').to_string(),
        _ => path.to_string(),
    }
}

/// Zed extension runs in a wasm-ish environment; absolute-ness checks differ per OS.
fn is_absolute_path_wasm(path: &PathBuf) -> bool {
    let Some(path_str) = path.to_str() else {
        return false;
    };

    match zed::current_platform().0 {
        zed::Os::Windows => {
            let bytes = path_str.as_bytes();
            if bytes.len() >= 3
                && bytes[0].is_ascii_alphabetic()
                && bytes[1] == b':'
                && (bytes[2] == b'\\' || bytes[2] == b'/')
            {
                return true;
            }
            path_str.starts_with(r"\\")
        }
        _ => path_str.starts_with('/'),
    }
}

/// Convert possibly-relative path into an absolute string for passing to subprocesses.
fn to_abs_string_for_exec(p: &PathBuf) -> Result<String, String> {
    if is_absolute_path_wasm(p) {
        return Ok(p.to_string_lossy().to_string());
    }
    let cwd = env::current_dir().map_err(|e| e.to_string())?;
    Ok(cwd.join(p).to_string_lossy().to_string())
}

struct Skopio {
    cached_lsp_binary_path: Option<PathBuf>,
    cached_cli_binary_path: Option<PathBuf>,
}

impl Skopio {
    fn resolve_lsp_path(
        &mut self,
        language_server_id: &zed::LanguageServerId,
        worktree: &zed::Worktree,
        arch: MacArch,
    ) -> Result<PathBuf, String> {
        zed::set_language_server_installation_status(
            language_server_id,
            &zed::LanguageServerInstallationStatus::CheckingForUpdate,
        );

        // settings override: lsp.skopio.binary.path
        if let Some(ls) = read_lsp_settings(worktree) {
            if let Some(bin) = ls.binary {
                if let Some(path) = bin.path {
                    let p = PathBuf::from(path);
                    if fs::metadata(&p).is_ok_and(|m| m.is_file()) {
                        self.cached_lsp_binary_path = Some(p.clone());
                        return Ok(p);
                    }
                }
            }
        }

        if let Some(p) = &self.cached_lsp_binary_path {
            if fs::metadata(p).is_ok_and(|m| m.is_file()) {
                return Ok(p.clone());
            }
        }

        // Convenience: if user has skopio-ls on PATH
        if let Some(p) = worktree.which("skopio-ls") {
            let p = PathBuf::from(p);
            self.cached_lsp_binary_path = Some(p.clone());
            return Ok(p);
        }

        zed::set_language_server_installation_status(
            language_server_id,
            &zed::LanguageServerInstallationStatus::Downloading,
        );

        let p = ensure_lsp_installed(arch)?;
        self.cached_lsp_binary_path = Some(p.clone());
        Ok(p)
    }

    fn resolve_cli_path(
        &mut self,
        language_server_id: &zed::LanguageServerId,
        worktree: &zed::Worktree,
        arch: MacArch,
    ) -> Result<PathBuf, String> {
        zed::set_language_server_installation_status(
            language_server_id,
            &zed::LanguageServerInstallationStatus::CheckingForUpdate,
        );

        // settings override: lsp.skopio.settings.cli_path
        if let Some(ls) = read_lsp_settings(worktree) {
            if let Some(settings) = ls.settings {
                if let Some(cli_path) = json_str(&settings, "cli_path") {
                    let p = PathBuf::from(cli_path);
                    if fs::metadata(&p).is_ok_and(|m| m.is_file()) {
                        self.cached_cli_binary_path = Some(p.clone());
                        return Ok(p);
                    }
                }
            }
        }

        // Convenience: if user has skopio-cli on PATH
        if let Some(p) = worktree.which("skopio-cli") {
            let p = PathBuf::from(p);
            self.cached_cli_binary_path = Some(p.clone());
            return Ok(p);
        }

        if let Some(p) = &self.cached_cli_binary_path {
            if fs::metadata(p).is_ok_and(|m| m.is_file()) {
                return Ok(p.clone());
            }
        }

        zed::set_language_server_installation_status(
            language_server_id,
            &zed::LanguageServerInstallationStatus::Downloading,
        );

        let p = ensure_cli_installed(arch)?;
        self.cached_cli_binary_path = Some(p.clone());
        Ok(p)
    }

    fn resolve_runtime_tuning(worktree: &zed::Worktree) -> (String, String, String) {
        let mut idle = "60".to_string();
        let mut grace = "60".to_string();
        let mut min_sess = "2".to_string();

        // Allow overriding via lsp.skopio.settings.*
        if let Some(ls) = read_lsp_settings(worktree) {
            if let Some(settings) = ls.settings {
                if let Some(v) = json_u64(&settings, "idle_secs") {
                    idle = v.to_string();
                }
                if let Some(v) = json_u64(&settings, "switch_grace_secs") {
                    grace = v.to_string();
                }
                if let Some(v) = json_i64(&settings, "min_session_secs") {
                    min_sess = v.to_string();
                }
            }
        }

        (idle, grace, min_sess)
    }
}

impl zed::Extension for Skopio {
    fn new() -> Self {
        Self {
            cached_lsp_binary_path: None,
            cached_cli_binary_path: None,
        }
    }

    fn language_server_command(
        &mut self,
        language_server_id: &zed::LanguageServerId,
        worktree: &zed::Worktree,
    ) -> zed::Result<zed::Command> {
        let arch = current_mac_arch().ok_or_else(|| "Skopio is macOS only for now".to_string())?;

        let cli_path = self.resolve_cli_path(language_server_id, worktree, arch)?;
        let lsp_path = self.resolve_lsp_path(language_server_id, worktree, arch)?;

        let (idle, grace, min_sess) = Self::resolve_runtime_tuning(worktree);

        let cli_abs = sanitize_path(&to_abs_string_for_exec(&cli_path)?);
        let lsp_abs = sanitize_path(&to_abs_string_for_exec(&lsp_path)?);

        let args = vec![
            "--skopio-cli".to_string(),
            cli_abs.clone(),
            "--idle-secs".to_string(),
            idle.clone(),
            "--switch-grace-secs".to_string(),
            grace.clone(),
            "--min-session-secs".to_string(),
            min_sess.clone(),
        ];

        // Still provide env for backwards compatibility
        let mut env_vars = worktree.shell_env();
        env_vars.push(("SKOPIO_CLI_PATH".into(), cli_abs));
        env_vars.push((ENV_IDLE_SECS.into(), idle));
        env_vars.push((ENV_SWITCH_GRACE_SECS.into(), grace));
        env_vars.push((ENV_MIN_SESSION_SECS.into(), min_sess));

        Ok(zed::Command {
            command: lsp_abs,
            args,
            env: env_vars,
        })
    }
}

zed::register_extension!(Skopio);