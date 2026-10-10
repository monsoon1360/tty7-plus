use std::fs::{self, OpenOptions};
use std::io::{self, Write};
use std::path::{Path, PathBuf};
use serde::{Serialize, Deserialize};
use super::DEFAULT_RELAY_URL;
use super::crypto;

#[derive(Clone, Serialize, Deserialize)]
pub(super) struct Credential {
    pub id: String,
    pub name: String,
    pub secret: String,
}
#[derive(Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(super) struct Store {
    pub version: u32,
    pub enabled: bool,
    pub relay_url: String,
    pub tls_fingerprint: Option<String>,
    pub host_token: String,
    pub credentials: Vec<Credential>,
    #[serde(skip)]
    path: PathBuf,
}
impl Store {
    pub(super) fn load() -> io::Result<Self> {
        let dir = crate::core::config::config_dir_path()
            .ok_or_else(|| io::Error::other("no mobile credential directory"))?.join("mobile-private");
        fs::create_dir_all(&dir)?;
        private(&dir, true)?;
        let path = dir.join("credentials.json");
        match fs::symlink_metadata(&path) {
            Ok(meta) => {
                if !meta.is_file() || meta.len() > 128 * 1024 {
                    return Err(io::Error::other("invalid mobile credential file"));
                }
                private(&path, false)?;
                let bytes = fs::read(&path)?;
                let mut store: Self = serde_json::from_slice(&bytes)
                    .map_err(|_| io::Error::other("mobile credential file is invalid; access disabled"))?;
                if store.version != 1 || store.credentials.len() > 64 {
                    return Err(io::Error::other("unsupported mobile credential store"));
                }
                crypto::secret(&store.host_token)?;
                let mut ids = std::collections::HashSet::new();
                for credential in &store.credentials {
                    crypto::secret(&credential.secret)?;
                    if credential.id.starts_with("pair:") || !ids.insert(&credential.id) {
                        return Err(io::Error::other("invalid mobile credential identity"));
                    }
                }
                store.path = path;
                Ok(store)
            }
            Err(e) if e.kind() == io::ErrorKind::NotFound => {
                let store = Self { version: 1, enabled: false, relay_url: DEFAULT_RELAY_URL.into(),
                    tls_fingerprint: None, host_token: crypto::encode(&crypto::random()?),
                    credentials: Vec::new(), path };
                store.save()?;
                Ok(store)
            }
            Err(e) => Err(e),
        }
    }
    pub(super) fn save(&self) -> io::Result<()> {
        let tmp = self.path.with_file_name(format!(".credentials-{}.tmp", uuid::Uuid::new_v4()));
        let bytes = serde_json::to_vec(self).map_err(io::Error::other)?;
        let result = (|| {
            let mut options = OpenOptions::new();
            options.write(true).create_new(true);
            #[cfg(unix)]
            { use std::os::unix::fs::OpenOptionsExt; options.mode(0o600); }
            let mut file = options.open(&tmp)?;
            private(&tmp, false)?;
            file.write_all(&bytes)?;
            file.sync_all()?;
            drop(file);
            fs::rename(&tmp, &self.path)?;
            #[cfg(unix)]
            if let Some(parent) = self.path.parent() { fs::File::open(parent)?.sync_all()?; }
            Ok(())
        })();
        if result.is_err() { let _ = fs::remove_file(tmp); }
        result
    }
    pub(super) fn host_id(&self) -> io::Result<String> { Ok(crypto::digest(&crypto::secret(&self.host_token)?)) }
}

fn private(path: &Path, directory: bool) -> io::Result<()> {
    let meta = fs::symlink_metadata(path)?;
    if meta.file_type().is_symlink() || (directory && !meta.is_dir()) { return Err(io::Error::other("unsafe mobile credential path")); }
    #[cfg(unix)]
    {
        use std::os::unix::fs::{MetadataExt, PermissionsExt};
        if meta.uid() != unsafe { libc::geteuid() } { return Err(io::Error::other("mobile credentials are owned by another user")); }
        fs::set_permissions(path, fs::Permissions::from_mode(if directory { 0o700 } else { 0o600 }))?;
    }
    #[cfg(windows)]
    windows_private(path)?;
    #[cfg(not(any(unix, windows)))]
    return Err(io::Error::new(io::ErrorKind::Unsupported, "private credential storage unavailable"));
    Ok(())
}

#[cfg(windows)]
fn windows_private(path: &Path) -> io::Result<()> {
    use std::os::windows::ffi::OsStrExt;
    use windows_sys::Win32::{Foundation::{CloseHandle, LocalFree}, Security::{
        Authorization::{ConvertSidToStringSidW, ConvertStringSecurityDescriptorToSecurityDescriptorW},
        DACL_SECURITY_INFORMATION, PROTECTED_DACL_SECURITY_INFORMATION, GetTokenInformation,
        SetFileSecurityW, TOKEN_QUERY, TOKEN_USER, TokenUser,
    }, System::Threading::{GetCurrentProcess, OpenProcessToken}};
    unsafe {
        let mut token = std::ptr::null_mut();
        if OpenProcessToken(GetCurrentProcess(), TOKEN_QUERY, &mut token) == 0 { return Err(io::Error::last_os_error()); }
        let mut length = 0;
        GetTokenInformation(token, TokenUser, std::ptr::null_mut(), 0, &mut length);
        let mut buffer = vec![0usize; (length as usize).div_ceil(std::mem::size_of::<usize>())];
        let ok = GetTokenInformation(token, TokenUser, buffer.as_mut_ptr().cast(), length, &mut length);
        CloseHandle(token);
        if ok == 0 { return Err(io::Error::last_os_error()); }
        let user = &*buffer.as_ptr().cast::<TOKEN_USER>();
        let mut sid = std::ptr::null_mut();
        if ConvertSidToStringSidW(user.User.Sid, &mut sid) == 0 { return Err(io::Error::last_os_error()); }
        let mut len = 0;
        while *sid.add(len) != 0 { len += 1; }
        let sid_text = String::from_utf16_lossy(std::slice::from_raw_parts(sid, len));
        LocalFree(sid.cast());
        let sddl: Vec<u16> = format!("D:P(A;;FA;;;{sid_text})").encode_utf16().chain(Some(0)).collect();
        let mut descriptor = std::ptr::null_mut();
        if ConvertStringSecurityDescriptorToSecurityDescriptorW(sddl.as_ptr(), 1, &mut descriptor, std::ptr::null_mut()) == 0 {
            return Err(io::Error::last_os_error());
        }
        let wide: Vec<u16> = path.as_os_str().encode_wide().chain(Some(0)).collect();
        let ok = SetFileSecurityW(wide.as_ptr(), DACL_SECURITY_INFORMATION | PROTECTED_DACL_SECURITY_INFORMATION, descriptor);
        let result = if ok == 0 { Err(io::Error::last_os_error()) } else { Ok(()) };
        LocalFree(descriptor.cast());
        result
    }
}

#[cfg(all(test, unix))]
mod tests {
    use super::*;
    use std::os::unix::fs::PermissionsExt;
    #[test]
    fn private_paths_refuse_symlinks_and_remove_world_permissions() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("secret");
        fs::write(&path, b"secret").unwrap();
        fs::set_permissions(&path, fs::Permissions::from_mode(0o666)).unwrap();
        private(&path, false).unwrap();
        assert_eq!(fs::metadata(&path).unwrap().permissions().mode() & 0o777, 0o600);
        let link = dir.path().join("link");
        std::os::unix::fs::symlink(&path, &link).unwrap();
        assert!(private(&link, false).is_err());
    }
}
