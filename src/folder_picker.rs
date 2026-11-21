use anyhow::{Context, Result, ensure};
use std::{
    collections::BTreeMap,
    fs,
    path::{Path, PathBuf},
    sync::RwLock,
};

#[cfg(target_os = "macos")]
mod macos;
#[cfg(target_os = "macos")]
use macos::{Scope, has_gui, restore};
#[cfg(target_os = "macos")]
pub use macos::{cancel, run, stop};

#[cfg(not(target_os = "macos"))]
struct Scope;
#[cfg(not(target_os = "macos"))]
fn has_gui() -> bool {
    false
}
#[cfg(not(target_os = "macos"))]
fn restore(_: &[u8]) -> Result<(Scope, PathBuf)> {
    anyhow::bail!("请在 Mac 上重新选择此文件夹。")
}

pub struct Selection {
    pub path: PathBuf,
    pub bookmark: Vec<u8>,
}

pub struct AuthorizedFolder {
    pub path: PathBuf,
    pub bookmark: Vec<u8>,
    scope: Scope,
}

impl Selection {
    pub fn authorize(self) -> Result<AuthorizedFolder> {
        let scope = open(&self.path, &self.bookmark)?;
        Ok(AuthorizedFolder {
            path: self.path,
            bookmark: self.bookmark,
            scope,
        })
    }
}

pub struct FolderAccess {
    pub picker_available: bool,
    scopes: RwLock<BTreeMap<PathBuf, Result<Scope, String>>>,
}

impl FolderAccess {
    pub fn new(bookmarks: BTreeMap<PathBuf, Vec<u8>>) -> Self {
        let scopes = bookmarks
            .into_iter()
            .map(|(path, bookmark)| {
                let access = open(&path, &bookmark).map_err(|error| format!("{error:#}"));
                (path, access)
            })
            .collect();
        Self {
            picker_available: has_gui(),
            scopes: RwLock::new(scopes),
        }
    }

    pub async fn select(&self, initial: Option<PathBuf>) -> Result<Option<Selection>> {
        ensure!(self.picker_available, "此服务没有可用的 macOS 图形会话。");
        #[cfg(target_os = "macos")]
        return macos::select(initial).await;
        #[cfg(not(target_os = "macos"))]
        {
            let _ = initial;
            anyhow::bail!("此服务没有可用的 macOS 图形会话。")
        }
    }

    pub fn remember(&self, path: PathBuf, folder: AuthorizedFolder) {
        self.scopes.write().unwrap().insert(path, Ok(folder.scope));
    }

    pub fn forget(&self, path: &Path) {
        self.scopes.write().unwrap().remove(path);
    }

    pub fn can_scan(&self, path: &Path) -> bool {
        self.scopes
            .read()
            .unwrap()
            .get(path)
            .is_none_or(Result::is_ok)
    }

    pub fn error(&self, path: &Path) -> Option<String> {
        self.scopes
            .read()
            .unwrap()
            .get(path)
            .and_then(|scope| scope.as_ref().err().cloned())
    }
}

fn open(path: &Path, bookmark: &[u8]) -> Result<Scope> {
    let (scope, resolved) = restore(bookmark).context("无法恢复文件夹授权，请重新选择该文件夹")?;
    ensure!(
        path.canonicalize()? == resolved.canonicalize()?,
        "文件夹位置已变更，请重新选择该文件夹"
    );
    fs::read_dir(path).context("macOS 未允许读取此文件夹，请在系统窗口中重新选择")?;
    Ok(scope)
}
