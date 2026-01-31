use std::{
    fs,
    io::{Read, Write},
    path::{Path, PathBuf},
};

use serde::Deserialize;
use sha2::{Digest, Sha256};
use zed_extension_api as zed;
use zip::ZipArchive;

const EXT_REPO: &str = "";
const BIN_DIR: &str = "bin";

const CLI_LATEST_JSON_URL: &str = "";

const ENV_ZED_ENV: &str = "SKOPIO_ZED_ENV";
const ENV_DEV_CLI_PATH: &str = "SKOPIO_DEV_CLI_PATH";

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

    let platform = zed::current_platform();

    match (platform.0, platform.1) {
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

fn cli_version_file_name(arch: MacArch) -> &'static str {
    match arch {
        MacArch::Aarch64 => "skopio-cli-darwin-aarch64.version",
        MacArch::X8664 => "skopio-cli-darwin-x86_64.version",
    }
}

fn is_dev_env() -> bool {
    matches!(std::env::var(ENV_ZED_ENV).ok().as_deref(), Some("dev"))
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
        let n = file
            .read(&mut buf)
            .map_err(|e| format!("read failed: {e}"))?;
        if n == 0 {
            break;
        }
        hasher.update(&buf[..n]);
    }
    Ok(to_lower_hex(&hasher.finalize()))
}

fn unzip_binary(zip_path: &Path, expected_filename: &str, out_path: &Path) -> Result<(), String> {
    let f = fs::File::open(zip_path).map_err(|e| format!("open zip failed: {e}"))?;
    let mut archive = ZipArchive::new(f).map_err(|e| format!("invalid zip: {e}"))?;

    for i in 0..archive.len() {
        let mut file = archive
            .by_index(i)
            .map_err(|e| format!("zip entry read failed: {e}"))?;
        if file.is_dir() {
            continue;
        }

        let name = file.name().rsplit('/').next().unwrap_or(file.name());
        if name == expected_filename {
            if let Some(parent) = out_path.parent() {
                fs::create_dir_all(parent).map_err(|e| format!("mkdir failed: {e}"))?;
            }
            let mut out =
                fs::File::create(out_path).map_err(|e| format!("create out failed: {e}"))?;
            std::io::copy(&mut file, &mut out).map_err(|e| format!("extract copy failed: {e}"))?;
            out.flush().map_err(|e| format!("flush failed: {e}"))?;
            return Ok(());
        }
    }

    Err(format!(
        "zip did not contain expected binary `{expected_filename}`"
    ))
}

fn read_trimmed(path: &Path) -> Option<String> {
    fs::read_to_string(path).ok().map(|s| s.trim().to_string())
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

fn maybe_dev_cli_path() -> Option<String> {
    if !is_dev_env() {
        return None;
    }
    let p = std::env::var(ENV_DEV_CLI_PATH).ok()?;
    if p.trim().is_empty() {
        return None;
    }

    if Path::new(&p).is_file() {
        Some(p)
    } else {
        None
    }
}

fn ensure_cli_installed(arch: MacArch) -> Result<String, String> {
    let latest_rel = format!("{BIN_DIR}/cli.latest.json");
    zed::download_file(
        CLI_LATEST_JSON_URL,
        &latest_rel,
        zed::DownloadedFileType::Uncompressed,
    )
    .map_err(|e| format!("download latest.json failed: {e}"))?;

    let latest_str =
        fs::read_to_string(&latest_rel).map_err(|e| format!("read latest.json failed: {e}"))?;
    let latest: LatestJson =
        serde_json::from_str(&latest_str).map_err(|e| format!("parse latest.json failed: {e}"))?;

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

    let cli_rel = format!("{BIN_DIR}/{cli_name}");
    let cli_path = PathBuf::from(&cli_rel);

    let version_rel = format!("{BIN_DIR}/{}", cli_version_file_name(arch));
    let version_path = PathBuf::from(&version_rel);

    let installed_version = read_trimmed(&version_path);
    let already_installed = cli_path.is_file();

    if already_installed && installed_version.as_deref() == Some(latest.version.as_str()) {
        return Ok(cli_rel);
    }

    let zip_rel = format!("{BIN_DIR}/{cli_name}.zip");
    zed::download_file(&asset.url, &zip_rel, zed::DownloadedFileType::Zip)
        .map_err(|e| format!("download cli zip failed: {e}"))?;

    let zip_path = PathBuf::from(&zip_rel);

    let got = sha256_hex(&zip_path)?;
    let expected = asset.sha256.trim().to_lowercase();
    if got != expected {
        let _ = fs::remove_file(&zip_path);
        return Err(format!(
            "sha256 mismatch for {cli_name}.zip: expected {expected}, got {got}"
        ));
    }

    unzip_binary(&zip_path, cli_name, &cli_path)?;
    zed::make_file_executable(&cli_rel).map_err(|e| format!("chmod +x failed: {e}"))?;

    write_text(&version_path, &(latest.version.clone() + "\n"))?;
    let _ = fs::remove_file(&zip_path);

    Ok(cli_rel)
}

fn ensure_lsp_installed(arch: MacArch) -> Result<String, String> {
    let lsp_asset = lsp_asset_name(arch);

    let release = zed::latest_github_release(
        EXT_REPO,
        zed::GithubReleaseOptions {
            require_assets: true,
            pre_release: false,
        },
    )
    .map_err(|e| format!("latest_github_release failed: {e}"))?;

    let asset = release
        .assets
        .iter()
        .find(|a| a.name == lsp_asset)
        .ok_or_else(|| format!("Missing release asset `{lsp_asset}` in {EXT_REPO}"))?;

    let lsp_rel = format!("{BIN_DIR}/{lsp_asset}");

    if !Path::new(&lsp_rel).is_file() {
        zed::download_file(
            &asset.download_url,
            &lsp_rel,
            zed::DownloadedFileType::Uncompressed,
        )
        .map_err(|e| format!("download lsp failed: {e}"))?;
        zed::make_file_executable(&lsp_rel).map_err(|e| format!("chmod +x lsp failed: {e}"))?;
    }
    Ok(lsp_rel)
}

struct Skopio;

impl zed::Extension for Skopio {
    fn new() -> Self {
        Self
    }

    fn language_server_command(
        &mut self,
        _language_server_id: &zed::LanguageServerId,
        _worktree: &zed::Worktree,
    ) -> zed::Result<zed::Command> {
        let arch =
            current_mac_arch().ok_or_else(|| "Skopio CLI is macOS only for now".to_string())?;

        let cli_path = if let Some(dev_path) = maybe_dev_cli_path() {
            dev_path
        } else {
            ensure_cli_installed(arch).map_err(|e| e)?
        };

        let lsp_rel = ensure_lsp_installed(arch).map_err(|e| e)?;

        let idle = std::env::var(ENV_IDLE_SECS).unwrap_or_else(|_| "60".into());
        let grace = std::env::var(ENV_SWITCH_GRACE_SECS).unwrap_or_else(|_| "60".into());
        let min_sess = std::env::var(ENV_MIN_SESSION_SECS).unwrap_or_else(|_| "2".into());

        Ok(zed::Command {
            command: lsp_rel,
            args: vec![],
            env: vec![
                ("SKOPIO_CLI_PATH".into(), cli_path),
                (ENV_IDLE_SECS.into(), idle),
                (ENV_SWITCH_GRACE_SECS.into(), grace),
                (ENV_MIN_SESSION_SECS.into(), min_sess),
            ],
        })
    }
}

zed::register_extension!(Skopio);
