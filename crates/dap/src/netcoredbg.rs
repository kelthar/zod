//! Locating and downloading [netcoredbg](https://github.com/Samsung/netcoredbg), the .NET debug adapter.
//!
//! The adapter is never downloaded automatically: the user is asked first (see the debugger panel).

use std::path::{Path, PathBuf};

use anyhow::{Context as _, Result};
use async_compression::futures::bufread::GzipDecoder;
use async_tar::Archive;
use futures::{AsyncReadExt as _, io::BufReader};
use http_client::{AsyncBody, HttpClient};
use sha2::{Digest as _, Sha256};
use std::sync::Arc;

use crate::adapters::DownloadedFileType;

pub const ADAPTER_NAME: &str = "netcoredbg";
pub const VERSION: &str = "3.2.0-1092";
pub const RELEASES_URL: &str = "https://github.com/Samsung/netcoredbg/releases";

pub struct ReleaseAsset {
    pub url: &'static str,
    pub sha256: &'static str,
    pub file_type: DownloadedFileType,
}

/// The release asset of [`VERSION`] for the current platform, if Samsung publishes one.
pub fn release_asset() -> Option<ReleaseAsset> {
    let (url, sha256, file_type) = match (std::env::consts::OS, std::env::consts::ARCH) {
        ("macos", "aarch64") => (
            "https://github.com/Samsung/netcoredbg/releases/download/3.2.0-1092/netcoredbg-osx-arm64.zip",
            "f4fa33b3ff874910cc184b4bb3b9c56d0abdf5c6521cee0b144d7c6e4a6e59ea",
            DownloadedFileType::Zip,
        ),
        ("linux", "x86_64") => (
            "https://github.com/Samsung/netcoredbg/releases/download/3.2.0-1092/netcoredbg-linux-amd64.tar.gz",
            "080eb3b2d2152465f599d3b33d1ee6e747794e11cc0a3773ec689f5e5f2c5afa",
            DownloadedFileType::GzipTar,
        ),
        ("linux", "aarch64") => (
            "https://github.com/Samsung/netcoredbg/releases/download/3.2.0-1092/netcoredbg-linux-arm64.tar.gz",
            "065ff49badec8a695dbea2de6ab6a330c774a191e426a217ab8cc05250627ccb",
            DownloadedFileType::GzipTar,
        ),
        ("windows", "x86_64") => (
            "https://github.com/Samsung/netcoredbg/releases/download/3.2.0-1092/netcoredbg-win64.zip",
            "3c410a45fa502415203a94fcb88654af65bf8e3dac158a5527a722e7a6b9274a",
            DownloadedFileType::Zip,
        ),
        _ => return None,
    };
    Some(ReleaseAsset {
        url,
        sha256,
        file_type,
    })
}

fn executable_name() -> &'static str {
    if cfg!(windows) {
        "netcoredbg.exe"
    } else {
        "netcoredbg"
    }
}

fn adapter_dir() -> PathBuf {
    paths::debug_adapters_dir().join(ADAPTER_NAME)
}

fn version_dir() -> PathBuf {
    adapter_dir().join(format!("{ADAPTER_NAME}_{VERSION}"))
}

/// Where [`download`] puts the executable. The archives contain a top-level `netcoredbg` directory.
pub fn downloaded_binary_path() -> PathBuf {
    version_dir().join("netcoredbg").join(executable_name())
}

/// Finds an installed netcoredbg: the `dap.netcoredbg.binary` setting, a previous download, or `PATH`.
pub fn find_binary(user_installed_path: Option<&Path>) -> Option<PathBuf> {
    if let Some(path) = user_installed_path.filter(|path| path.is_file()) {
        return Some(path.to_path_buf());
    }
    let downloaded = downloaded_binary_path();
    if downloaded.is_file() {
        return Some(downloaded);
    }
    std::env::var_os("PATH").and_then(|paths| {
        std::env::split_paths(&paths)
            .map(|dir| dir.join(executable_name()))
            .find(|path| path.is_file())
    })
}

/// Downloads and extracts netcoredbg [`VERSION`], verifying its SHA-256 digest.
/// Returns the path to the executable.
pub async fn download(http_client: Arc<dyn HttpClient>) -> Result<PathBuf> {
    let asset = release_asset().with_context(|| {
        format!(
            "netcoredbg {VERSION} is not published for {}-{}, download it from {RELEASES_URL}",
            std::env::consts::OS,
            std::env::consts::ARCH
        )
    })?;

    let mut response = http_client
        .get(asset.url, AsyncBody::default(), true)
        .await
        .with_context(|| format!("downloading {}", asset.url))?;
    anyhow::ensure!(
        response.status().is_success(),
        "downloading {} failed with status {}",
        asset.url,
        response.status()
    );
    let mut bytes = Vec::new();
    response
        .body_mut()
        .read_to_end(&mut bytes)
        .await
        .context("reading netcoredbg download")?;

    let digest = format!("{:x}", Sha256::digest(&bytes));
    anyhow::ensure!(
        digest == asset.sha256,
        "netcoredbg download has SHA-256 {digest}, expected {}",
        asset.sha256
    );

    let version_dir = version_dir();
    let staging_dir = version_dir.with_extension("partial");
    remove_dir_if_exists(&staging_dir).await?;
    smol::fs::create_dir_all(&staging_dir).await?;
    match asset.file_type {
        DownloadedFileType::GzipTar => {
            let decoder = GzipDecoder::new(BufReader::new(futures::io::Cursor::new(bytes)));
            Archive::new(decoder)
                .unpack(&staging_dir)
                .await
                .context("extracting netcoredbg")?;
        }
        DownloadedFileType::Zip | DownloadedFileType::Vsix => {
            util::archive::extract_zip(&staging_dir, futures::io::Cursor::new(bytes))
                .await
                .context("extracting netcoredbg")?;
        }
    }
    remove_dir_if_exists(&staging_dir.join("__MACOSX")).await?;

    let executable = staging_dir.join("netcoredbg").join(executable_name());
    anyhow::ensure!(
        executable.is_file(),
        "netcoredbg archive does not contain {}",
        executable.display()
    );

    // Replace any previous version only once the new one is fully extracted.
    if let Ok(mut entries) = smol::fs::read_dir(adapter_dir()).await {
        use futures::StreamExt as _;
        while let Some(Ok(entry)) = entries.next().await {
            if entry.path() != staging_dir {
                remove_dir_if_exists(&entry.path()).await.ok();
            }
        }
    }
    smol::fs::rename(&staging_dir, &version_dir)
        .await
        .context("installing netcoredbg")?;

    Ok(downloaded_binary_path())
}

async fn remove_dir_if_exists(path: &Path) -> Result<()> {
    match smol::fs::remove_dir_all(path).await {
        Ok(()) => Ok(()),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(()),
        Err(error) => Err(error).with_context(|| format!("removing {}", path.display())),
    }
}
