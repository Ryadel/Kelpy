//! SFTP into the folder of the files uploaded to Kelpy, and nowhere else: `/` is that folder, which has no
//! subfolders, so a path names one of its files or nothing. Which names are valid, which files can be opened and how
//! large they may grow are the rules of [`Files`], the same as for the Web UI. A file changed outside the folder goes
//! through Kelpy's operations, with their preview, backup and approval.

use std::collections::{HashMap, VecDeque};
use std::io::SeekFrom;
use std::sync::Arc;

use kelpy_core::files::{Access, Create, Files, FilesError, MAX_UPLOAD};
use russh_sftp::protocol::{
    Attrs, Data, File, FileAttributes, Handle, Name, OpenFlags, Status, StatusCode, Version,
};
use tokio::io::{AsyncReadExt, AsyncSeekExt, AsyncWriteExt};
use tokio_util::task::task_tracker::TaskTrackerToken;

/// The most bytes one read returns, as SFTP clients ask for 32 to 256 KiB.
const MAX_READ: u32 = 256 * 1024;
/// A listing's part sent in one reply is at most this many bytes, under the 34000-byte packets every SFTP client
/// takes; a client asks again for the rest.
const MAX_LISTING_PART: usize = 30 * 1024;
/// How many files and listings one session may hold open.
const MAX_HANDLES: usize = 64;

pub struct Sftp {
    files: Arc<Files>,
    handles: HashMap<String, Open>,
    next: u64,
    /// Held while the SFTP session runs, in a task of its own: stopping the SSH server waits for it.
    _running: TaskTrackerToken,
}

enum Open {
    File {
        file: tokio::fs::File,
        /// Whether it was written to: the interfaces hear of it when it closes.
        written: bool,
    },
    /// A listing, given a part at a time: what is still to give.
    Listing(VecDeque<File>),
}

impl Sftp {
    pub fn new(files: Arc<Files>, running: TaskTrackerToken) -> Self {
        Self {
            files,
            handles: HashMap::new(),
            next: 0,
            _running: running,
        }
    }

    /// The name of the file `path` names in the folder, or `None` for the folder itself.
    fn resolve(path: &str) -> Result<Option<&str>, StatusCode> {
        let name = path.trim_start_matches('/').trim_end_matches('/');
        if name.is_empty() || name == "." {
            Ok(None)
        } else if kelpy_core::files::is_valid_name(name) {
            Ok(Some(name))
        } else {
            Err(StatusCode::PermissionDenied)
        }
    }

    fn file(path: &str) -> Result<&str, StatusCode> {
        Self::resolve(path)?.ok_or(StatusCode::PermissionDenied)
    }

    fn keep(&mut self, open: Open) -> String {
        self.next += 1;
        let handle = self.next.to_string();
        self.handles.insert(handle.clone(), open);
        handle
    }

    /// Refuses one more handle past [`MAX_HANDLES`], before anything is opened.
    fn room(&self) -> Result<(), StatusCode> {
        if self.handles.len() < MAX_HANDLES {
            Ok(())
        } else {
            Err(StatusCode::Failure)
        }
    }
}

fn status(error: FilesError) -> StatusCode {
    match error {
        FilesError::NotFound(_) => StatusCode::NoSuchFile,
        FilesError::InvalidName => StatusCode::PermissionDenied,
        FilesError::Io(error) => failure(error),
        FilesError::TooLarge | FilesError::Interrupted(_) => StatusCode::Failure,
    }
}

fn failure(error: std::io::Error) -> StatusCode {
    match error.kind() {
        std::io::ErrorKind::NotFound => StatusCode::NoSuchFile,
        std::io::ErrorKind::PermissionDenied => StatusCode::PermissionDenied,
        _ => StatusCode::Failure,
    }
}

fn ok(id: u32) -> Status {
    Status {
        id,
        status_code: StatusCode::Ok,
        error_message: "Ok".into(),
        language_tag: "en-US".into(),
    }
}

impl russh_sftp::server::Handler for Sftp {
    type Error = StatusCode;

    fn unimplemented(&self) -> Self::Error {
        StatusCode::OpUnsupported
    }

    async fn init(
        &mut self,
        _version: u32,
        _extensions: HashMap<String, String>,
    ) -> Result<Version, Self::Error> {
        self.files.prepare().map_err(failure)?;
        Ok(Version::new())
    }

    async fn realpath(&mut self, id: u32, path: String) -> Result<Name, Self::Error> {
        let shown = format!("/{}", Self::resolve(&path)?.unwrap_or_default());
        Ok(Name {
            id,
            files: vec![File::dummy(shown)],
        })
    }

    async fn stat(&mut self, id: u32, path: String) -> Result<Attrs, Self::Error> {
        let metadata = match Self::resolve(&path)? {
            None => std::fs::metadata(self.files.dir()),
            Some(name) => self
                .files
                .open(name, Access::READ)
                .map_err(status)?
                .metadata(),
        };
        Ok(Attrs {
            id,
            attrs: FileAttributes::from(&metadata.map_err(failure)?),
        })
    }

    async fn lstat(&mut self, id: u32, path: String) -> Result<Attrs, Self::Error> {
        self.stat(id, path).await
    }

    async fn fstat(&mut self, id: u32, handle: String) -> Result<Attrs, Self::Error> {
        match self.handles.get(&handle) {
            Some(Open::File { file, .. }) => Ok(Attrs {
                id,
                attrs: FileAttributes::from(&file.metadata().await.map_err(failure)?),
            }),
            _ => Err(StatusCode::Failure),
        }
    }

    async fn setstat(
        &mut self,
        id: u32,
        _path: String,
        _attrs: FileAttributes,
    ) -> Result<Status, Self::Error> {
        // Owner and permissions stay Kelpy's; clients setting times or modes after an upload are not refused.
        Ok(ok(id))
    }

    async fn fsetstat(
        &mut self,
        id: u32,
        _handle: String,
        _attrs: FileAttributes,
    ) -> Result<Status, Self::Error> {
        Ok(ok(id))
    }

    async fn opendir(&mut self, id: u32, path: String) -> Result<Handle, Self::Error> {
        if Self::resolve(&path)?.is_some() {
            return Err(StatusCode::NoSuchFile);
        }
        self.room()?;
        let files = Arc::clone(&self.files);
        let listing = tokio::task::spawn_blocking(move || files.list())
            .await
            .map_err(|_| StatusCode::Failure)?
            .map_err(status)?
            .into_iter()
            .map(|file| {
                let modified = u32::try_from(file.modified).ok();
                let attributes = FileAttributes {
                    size: Some(file.size),
                    // A regular file, readable and writable by its owner only, as Kelpy keeps it.
                    permissions: Some(0o100600),
                    atime: modified,
                    mtime: modified,
                    ..FileAttributes::default()
                };
                File::new(file.name, attributes)
            })
            .collect();
        Ok(Handle {
            id,
            handle: self.keep(Open::Listing(listing)),
        })
    }

    async fn readdir(&mut self, id: u32, handle: String) -> Result<Name, Self::Error> {
        match self.handles.get_mut(&handle) {
            Some(Open::Listing(listing)) if listing.is_empty() => Err(StatusCode::Eof),
            Some(Open::Listing(listing)) => {
                // At least one file, then as many as stay within the part.
                let mut size = 0;
                let mut files = Vec::new();
                while let Some(file) = listing.pop_front() {
                    // The two names, their lengths and the attributes.
                    size += file.filename.len() + file.longname.len() + 64;
                    if size > MAX_LISTING_PART && !files.is_empty() {
                        listing.push_front(file);
                        break;
                    }
                    files.push(file);
                }
                Ok(Name { id, files })
            }
            _ => Err(StatusCode::Failure),
        }
    }

    async fn open(
        &mut self,
        id: u32,
        filename: String,
        pflags: OpenFlags,
        _attrs: FileAttributes,
    ) -> Result<Handle, Self::Error> {
        let name = Self::file(&filename)?;
        // Appending writes where the file ends, not at the offset checked against MAX_UPLOAD; clients resume
        // uploads with offsets.
        if pflags.contains(OpenFlags::APPEND) {
            return Err(StatusCode::OpUnsupported);
        }
        self.room()?;
        let access = Access {
            read: pflags.contains(OpenFlags::READ),
            write: pflags.contains(OpenFlags::WRITE),
            create: match (
                pflags.contains(OpenFlags::CREATE),
                pflags.contains(OpenFlags::EXCLUDE),
            ) {
                (false, _) => Create::No,
                (true, false) => Create::IfMissing,
                (true, true) => Create::New,
            },
            truncate: pflags.contains(OpenFlags::TRUNCATE),
        };
        let file = self.files.open(name, access).map_err(status)?;
        Ok(Handle {
            id,
            handle: self.keep(Open::File {
                file: tokio::fs::File::from_std(file),
                written: access.truncate,
            }),
        })
    }

    async fn read(
        &mut self,
        id: u32,
        handle: String,
        offset: u64,
        len: u32,
    ) -> Result<Data, Self::Error> {
        let Some(Open::File { file, .. }) = self.handles.get_mut(&handle) else {
            return Err(StatusCode::Failure);
        };
        file.seek(SeekFrom::Start(offset)).await.map_err(failure)?;
        let mut data = vec![0; len.min(MAX_READ) as usize];
        let read = file.read(&mut data).await.map_err(failure)?;
        if read == 0 {
            return Err(StatusCode::Eof);
        }
        data.truncate(read);
        Ok(Data { id, data })
    }

    async fn write(
        &mut self,
        id: u32,
        handle: String,
        offset: u64,
        data: Vec<u8>,
    ) -> Result<Status, Self::Error> {
        let Some(Open::File { file, written }) = self.handles.get_mut(&handle) else {
            return Err(StatusCode::Failure);
        };
        if offset.saturating_add(data.len() as u64) > MAX_UPLOAD {
            return Err(StatusCode::Failure);
        }
        file.seek(SeekFrom::Start(offset)).await.map_err(failure)?;
        file.write_all(&data).await.map_err(failure)?;
        *written = true;
        Ok(ok(id))
    }

    async fn close(&mut self, id: u32, handle: String) -> Result<Status, Self::Error> {
        match self.handles.remove(&handle) {
            Some(Open::File { mut file, written }) => {
                file.flush().await.map_err(failure)?;
                if written {
                    self.files.changed();
                }
            }
            Some(Open::Listing(_)) => {}
            None => return Err(StatusCode::Failure),
        }
        Ok(ok(id))
    }

    async fn remove(&mut self, id: u32, filename: String) -> Result<Status, Self::Error> {
        self.files.remove(Self::file(&filename)?).map_err(status)?;
        Ok(ok(id))
    }

    async fn rename(
        &mut self,
        id: u32,
        oldpath: String,
        newpath: String,
    ) -> Result<Status, Self::Error> {
        self.files
            .rename(Self::file(&oldpath)?, Self::file(&newpath)?)
            .map_err(status)?;
        Ok(ok(id))
    }
}

#[cfg(test)]
mod tests {
    use kelpy_core::events::Events;
    use russh_sftp::server::Handler;
    use tokio_util::task::TaskTracker;

    use super::*;

    fn sftp(folder: &tempfile::TempDir) -> Sftp {
        let files = Files::new(folder.path().to_owned(), Events::new());
        Sftp::new(Arc::new(files), TaskTracker::new().token())
    }

    fn writing() -> OpenFlags {
        OpenFlags::WRITE | OpenFlags::CREATE | OpenFlags::TRUNCATE
    }

    #[tokio::test]
    async fn only_the_files_of_the_folder_can_be_reached() {
        let folder = tempfile::tempdir().unwrap();
        let mut sftp = sftp(&folder);
        assert_eq!(
            sftp.realpath(1, ".".into()).await.unwrap().files[0].filename,
            "/"
        );
        for outside in [
            "/../config.toml",
            "/sub/file",
            "..",
            r"\..\x",
            "C:/Windows/win.ini",
            "/.upload-1-0",
        ] {
            assert!(Sftp::resolve(outside).is_err(), "{outside}");
        }

        let handle = sftp
            .open(2, "/site.conf".into(), writing(), FileAttributes::default())
            .await
            .unwrap()
            .handle;
        sftp.write(3, handle.clone(), 0, b"server {}\n".to_vec())
            .await
            .unwrap();
        sftp.close(4, handle).await.unwrap();
        assert_eq!(
            std::fs::read(folder.path().join("site.conf")).unwrap(),
            b"server {}\n"
        );

        let listing = sftp.opendir(5, "/".into()).await.unwrap().handle;
        let names: Vec<String> = sftp
            .readdir(6, listing.clone())
            .await
            .unwrap()
            .files
            .into_iter()
            .map(|file| file.filename)
            .collect();
        assert_eq!(names, ["site.conf"]);
        assert_eq!(sftp.readdir(7, listing).await.unwrap_err(), StatusCode::Eof);

        let reading = sftp
            .open(
                8,
                "site.conf".into(),
                OpenFlags::READ,
                FileAttributes::default(),
            )
            .await
            .unwrap()
            .handle;
        assert_eq!(
            sftp.read(9, reading.clone(), 7, 100).await.unwrap().data,
            b"{}\n"
        );
        assert_eq!(
            sftp.read(10, reading, 100, 100).await.unwrap_err(),
            StatusCode::Eof
        );
        sftp.rename(11, "/site.conf".into(), "/old.conf".into())
            .await
            .unwrap();
        sftp.remove(12, "/old.conf".into()).await.unwrap();
        assert_eq!(std::fs::read_dir(folder.path()).unwrap().count(), 0);
    }

    #[tokio::test]
    async fn a_long_listing_comes_in_parts_every_client_takes() {
        let folder = tempfile::tempdir().unwrap();
        for n in 0..1000 {
            std::fs::write(
                folder.path().join(format!("{n:04}{}", "n".repeat(146))),
                b"",
            )
            .unwrap();
        }
        let mut sftp = sftp(&folder);
        let listing = sftp.opendir(1, "/".into()).await.unwrap().handle;
        let mut names = Vec::new();
        let mut parts = 0;
        loop {
            match sftp.readdir(2, listing.clone()).await {
                Ok(part) => {
                    let size: usize = part
                        .files
                        .iter()
                        .map(|file| file.filename.len() + file.longname.len() + 64)
                        .sum();
                    assert!(size <= MAX_LISTING_PART, "{size}");
                    names.extend(part.files.into_iter().map(|file| file.filename));
                    parts += 1;
                }
                Err(status) => {
                    assert_eq!(status, StatusCode::Eof);
                    break;
                }
            }
        }
        assert!(parts > 1);
        names.sort();
        names.dedup();
        assert_eq!(names.len(), 1000);
    }

    #[tokio::test]
    async fn a_write_never_goes_past_the_largest_upload() {
        let folder = tempfile::tempdir().unwrap();
        let path = folder.path().join("big.bin");
        // Sparse, at the limit already.
        std::fs::File::create(&path)
            .unwrap()
            .set_len(MAX_UPLOAD)
            .unwrap();
        let mut sftp = sftp(&folder);
        assert_eq!(
            sftp.open(
                1,
                "big.bin".into(),
                OpenFlags::WRITE | OpenFlags::APPEND,
                FileAttributes::default()
            )
            .await
            .unwrap_err(),
            StatusCode::OpUnsupported
        );
        let handle = sftp
            .open(
                2,
                "big.bin".into(),
                OpenFlags::WRITE,
                FileAttributes::default(),
            )
            .await
            .unwrap()
            .handle;
        assert_eq!(
            sftp.write(3, handle.clone(), MAX_UPLOAD, b"x".to_vec())
                .await
                .unwrap_err(),
            StatusCode::Failure
        );
        sftp.write(4, handle, MAX_UPLOAD - 1, b"x".to_vec())
            .await
            .unwrap();
        assert_eq!(std::fs::metadata(&path).unwrap().len(), MAX_UPLOAD);
    }

    #[tokio::test]
    async fn a_request_past_the_handle_limit_changes_nothing() {
        let folder = tempfile::tempdir().unwrap();
        std::fs::write(folder.path().join("kept.txt"), b"kept").unwrap();
        let mut sftp = sftp(&folder);
        for id in 0..MAX_HANDLES as u32 {
            sftp.opendir(id, "/".into()).await.unwrap();
        }
        assert_eq!(
            sftp.open(100, "kept.txt".into(), writing(), FileAttributes::default())
                .await
                .unwrap_err(),
            StatusCode::Failure
        );
        assert_eq!(
            std::fs::read(folder.path().join("kept.txt")).unwrap(),
            b"kept"
        );
    }

    #[tokio::test]
    async fn a_link_already_in_the_folder_is_not_followed() {
        let folder = tempfile::tempdir().unwrap();
        let dir = folder.path().join("files");
        std::fs::create_dir(&dir).unwrap();
        let outside = folder.path().join("outside.txt");
        std::fs::write(&outside, b"outside").unwrap();
        std::fs::hard_link(&outside, dir.join("hard.txt")).unwrap();
        let files = Files::new(dir, Events::new());
        let mut sftp = Sftp::new(Arc::new(files), TaskTracker::new().token());
        for flags in [OpenFlags::READ, writing()] {
            assert_eq!(
                sftp.open(1, "hard.txt".into(), flags, FileAttributes::default())
                    .await
                    .unwrap_err(),
                StatusCode::NoSuchFile
            );
        }
        assert_eq!(
            sftp.stat(2, "hard.txt".into()).await.unwrap_err(),
            StatusCode::NoSuchFile
        );
        assert_eq!(std::fs::read(&outside).unwrap(), b"outside");
    }
}
