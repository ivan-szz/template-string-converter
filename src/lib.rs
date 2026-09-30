use std::fs;

use zed_extension_api::{self as zed, Command, LanguageServerId, Result, Worktree};

const REPO: &str = "ivan-szz/template-string-converter";
const BINARY: &str = "template-string-converter-lsp";

struct Extension {
    cached_binary_path: Option<String>,
}

impl Extension {
    fn binary_path(&mut self, id: &LanguageServerId, worktree: &Worktree) -> Result<String> {
        // A binary on PATH (e.g. from `cargo install --path lsp-server`) wins,
        // so local development doesn't need a release.
        if let Some(path) = worktree.which(BINARY) {
            return Ok(path);
        }

        if let Some(path) = &self.cached_binary_path {
            if fs::metadata(path).is_ok_and(|m| m.is_file()) {
                return Ok(path.clone());
            }
        }

        match self.download(id) {
            Ok(path) => {
                self.cached_binary_path = Some(path.clone());
                Ok(path)
            }
            Err(err) => {
                // Offline or GitHub unavailable: fall back to a previous download.
                let previous = installed_versions()
                    .into_iter()
                    .map(|dir| format!("{dir}/{}", binary_name()))
                    .find(|path| fs::metadata(path).is_ok_and(|m| m.is_file()));
                match previous {
                    Some(path) => {
                        zed::set_language_server_installation_status(
                            id,
                            &zed::LanguageServerInstallationStatus::None,
                        );
                        self.cached_binary_path = Some(path.clone());
                        Ok(path)
                    }
                    None => {
                        zed::set_language_server_installation_status(
                            id,
                            &zed::LanguageServerInstallationStatus::Failed(err.clone()),
                        );
                        Err(err)
                    }
                }
            }
        }
    }

    fn download(&self, id: &LanguageServerId) -> Result<String> {
        zed::set_language_server_installation_status(
            id,
            &zed::LanguageServerInstallationStatus::CheckingForUpdate,
        );
        let release = zed::latest_github_release(
            REPO,
            zed::GithubReleaseOptions {
                require_assets: true,
                pre_release: false,
            },
        )?;

        let (os, arch) = zed::current_platform();
        let target = match (os, arch) {
            (zed::Os::Mac, zed::Architecture::Aarch64) => "aarch64-apple-darwin",
            (zed::Os::Mac, zed::Architecture::X8664) => "x86_64-apple-darwin",
            (zed::Os::Linux, zed::Architecture::Aarch64) => "aarch64-unknown-linux-gnu",
            (zed::Os::Linux, zed::Architecture::X8664) => "x86_64-unknown-linux-gnu",
            (zed::Os::Windows, zed::Architecture::X8664) => "x86_64-pc-windows-msvc",
            _ => {
                return Err(format!(
                    "no prebuilt {BINARY} for this platform; \
                     install it with `cargo install --git https://github.com/{REPO} {BINARY}`"
                ))
            }
        };
        let (extension, file_type) = match os {
            zed::Os::Windows => ("zip", zed::DownloadedFileType::Zip),
            _ => ("tar.gz", zed::DownloadedFileType::GzipTar),
        };
        let asset_name = format!("{BINARY}-{target}.{extension}");
        let asset = release
            .assets
            .iter()
            .find(|asset| asset.name == asset_name)
            .ok_or_else(|| format!("release {} has no asset {asset_name}", release.version))?;

        let version_dir = format!("{BINARY}-{}", release.version);
        let binary_path = format!("{version_dir}/{}", binary_name());

        if !fs::metadata(&binary_path).is_ok_and(|m| m.is_file()) {
            zed::set_language_server_installation_status(
                id,
                &zed::LanguageServerInstallationStatus::Downloading,
            );
            zed::download_file(&asset.download_url, &version_dir, file_type)
                .map_err(|e| format!("failed to download {asset_name}: {e}"))?;
            zed::make_file_executable(&binary_path)?;

            for dir in installed_versions() {
                if dir != version_dir {
                    fs::remove_dir_all(&dir).ok();
                }
            }
        }

        zed::set_language_server_installation_status(
            id,
            &zed::LanguageServerInstallationStatus::None,
        );
        Ok(binary_path)
    }
}

fn binary_name() -> String {
    match zed::current_platform().0 {
        zed::Os::Windows => format!("{BINARY}.exe"),
        _ => BINARY.to_string(),
    }
}

/// Version directories from earlier downloads, newest first.
fn installed_versions() -> Vec<String> {
    let mut dirs: Vec<String> = fs::read_dir(".")
        .into_iter()
        .flatten()
        .flatten()
        .filter(|entry| entry.file_type().is_ok_and(|t| t.is_dir()))
        .filter_map(|entry| entry.file_name().into_string().ok())
        .filter(|name| name.starts_with(&format!("{BINARY}-")))
        .collect();
    // Compare numerically so `0.0.10` sorts after `0.0.9`.
    dirs.sort_by_cached_key(|name| {
        name[BINARY.len() + 1..]
            .trim_start_matches('v')
            .split('.')
            .map(|part| part.parse::<u64>().unwrap_or(0))
            .collect::<Vec<_>>()
    });
    dirs.reverse();
    dirs
}

impl zed::Extension for Extension {
    fn new() -> Self {
        Self {
            cached_binary_path: None,
        }
    }

    fn language_server_command(
        &mut self,
        language_server_id: &LanguageServerId,
        worktree: &Worktree,
    ) -> Result<Command> {
        Ok(Command {
            command: self.binary_path(language_server_id, worktree)?,
            args: Vec::new(),
            env: Vec::new(),
        })
    }
}

zed::register_extension!(Extension);
