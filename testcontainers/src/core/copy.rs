use std::{
    io::{self, Cursor},
    path::{Path, PathBuf},
};

use tar::{Archive as TarArchive, EntryType};
use tokio::{fs, task};

#[derive(Debug, Clone)]
pub struct CopyToContainerCollection(Vec<CopyToContainer>);

#[derive(Debug, Clone)]
pub struct CopyToContainer {
    target: String,
    source: CopyDataSource,
}

#[derive(Debug, Clone)]
pub enum CopyDataSource {
    File(PathBuf),
    Data(Vec<u8>),
}

#[derive(Debug, Clone)]
pub struct CopyFromArchive {
    bytes: bytes::Bytes,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum CopyFromOutcome {
    File(PathBuf),
    Directory(PathBuf),
}

impl CopyFromOutcome {
    pub fn path(&self) -> &Path {
        match self {
            CopyFromOutcome::File(path) | CopyFromOutcome::Directory(path) => path.as_path(),
        }
    }

    pub fn into_path(self) -> PathBuf {
        match self {
            CopyFromOutcome::File(path) | CopyFromOutcome::Directory(path) => path,
        }
    }
}

#[derive(Debug, thiserror::Error)]
pub enum CopyFromContainerError {
    #[error("io failed with error: {0}")]
    Io(#[from] io::Error),
    #[error("failed to join blocking task: {0}")]
    BlockingJoin(#[from] task::JoinError),
    #[error("archive did not contain any regular files; cannot write to {destination}")]
    EmptyArchive { destination: PathBuf },
    #[error("archive contains multiple regular files; cannot write to single-file destination {destination}")]
    MultipleRegularFiles { destination: PathBuf },
    #[error("archive entry type '{entry_type:?}' is not supported for single-file destination {destination}")]
    UnsupportedEntryType {
        destination: PathBuf,
        entry_type: EntryType,
    },
}

#[derive(Debug, thiserror::Error)]
pub enum CopyToContainerError {
    #[error("io failed with error: {0}")]
    IoError(io::Error),
    #[error("failed to get the path name: {0}")]
    PathNameError(String),
}

impl CopyFromArchive {
    pub(crate) fn new(bytes: bytes::Bytes) -> Self {
        Self { bytes }
    }

    pub fn as_bytes(&self) -> &[u8] {
        self.bytes.as_ref()
    }

    pub fn into_bytes(self) -> bytes::Bytes {
        self.bytes
    }

    pub async fn write_to_file(
        self,
        destination: impl AsRef<Path>,
    ) -> Result<CopyFromOutcome, CopyFromContainerError> {
        let destination = destination.as_ref().to_path_buf();
        let bytes = self.bytes;

        let result = task::spawn_blocking(move || {
            let buffer = bytes.to_vec();
            write_single_file_from_archive(&buffer, destination)
        })
        .await
        .map_err(CopyFromContainerError::BlockingJoin)?;

        result
    }

    pub async fn extract_to_dir(
        self,
        destination: impl AsRef<Path>,
    ) -> Result<CopyFromOutcome, CopyFromContainerError> {
        let destination = destination.as_ref().to_path_buf();
        let bytes = self.bytes;

        let result = task::spawn_blocking(move || {
            let buffer = bytes.to_vec();
            extract_archive_to_dir(&buffer, destination)
        })
        .await
        .map_err(CopyFromContainerError::BlockingJoin)?;

        result
    }

    pub async fn extract(
        self,
        destination: impl AsRef<Path>,
    ) -> Result<CopyFromOutcome, CopyFromContainerError> {
        let destination = destination.as_ref().to_path_buf();
        let destination_is_dir = fs::symlink_metadata(&destination)
            .await
            .map(|meta| meta.is_dir())
            .unwrap_or(false);
        let bytes = self.bytes;

        let result = task::spawn_blocking(move || {
            let buffer = bytes.to_vec();
            let stats = analyze_archive(&buffer)?;

            if destination_is_dir || stats.should_extract_as_directory() {
                return extract_archive_to_dir(&buffer, destination.clone());
            }

            if stats.regular_files == 0 {
                return Err(CopyFromContainerError::EmptyArchive {
                    destination: destination.clone(),
                });
            }

            write_single_file_from_archive(&buffer, destination)
        })
        .await
        .map_err(CopyFromContainerError::BlockingJoin)?;

        result
    }
}

impl CopyToContainerCollection {
    pub fn new(collection: Vec<CopyToContainer>) -> Self {
        Self(collection)
    }

    pub fn add(&mut self, entry: CopyToContainer) {
        self.0.push(entry);
    }

    pub(crate) async fn tar(&self) -> Result<bytes::Bytes, CopyToContainerError> {
        let mut ar = tokio_tar::Builder::new(Vec::new());

        for copy_to_container in &self.0 {
            copy_to_container.append_tar(&mut ar).await?
        }

        let bytes = ar
            .into_inner()
            .await
            .map_err(CopyToContainerError::IoError)?;

        Ok(bytes::Bytes::copy_from_slice(bytes.as_slice()))
    }
}

impl CopyToContainer {
    pub fn new(source: impl Into<CopyDataSource>, target: impl Into<String>) -> Self {
        Self {
            source: source.into(),
            target: target.into(),
        }
    }

    pub(crate) async fn tar(&self) -> Result<bytes::Bytes, CopyToContainerError> {
        let mut ar = tokio_tar::Builder::new(Vec::new());

        self.append_tar(&mut ar).await?;

        let bytes = ar
            .into_inner()
            .await
            .map_err(CopyToContainerError::IoError)?;

        Ok(bytes::Bytes::copy_from_slice(bytes.as_slice()))
    }

    pub(crate) async fn append_tar(
        &self,
        ar: &mut tokio_tar::Builder<Vec<u8>>,
    ) -> Result<(), CopyToContainerError> {
        self.source.append_tar(ar, &self.target).await
    }
}

impl From<&Path> for CopyDataSource {
    fn from(value: &Path) -> Self {
        CopyDataSource::File(value.to_path_buf())
    }
}
impl From<PathBuf> for CopyDataSource {
    fn from(value: PathBuf) -> Self {
        CopyDataSource::File(value)
    }
}
impl From<Vec<u8>> for CopyDataSource {
    fn from(value: Vec<u8>) -> Self {
        CopyDataSource::Data(value)
    }
}

impl CopyDataSource {
    pub(crate) async fn append_tar(
        &self,
        ar: &mut tokio_tar::Builder<Vec<u8>>,
        target_path: impl Into<String>,
    ) -> Result<(), CopyToContainerError> {
        let target_path: String = target_path.into();

        match self {
            CopyDataSource::File(source_file_path) => {
                if let Err(e) = append_tar_file(ar, source_file_path, &target_path).await {
                    log::error!(
                        "Could not append file/dir to tar: {source_file_path:?}:{target_path}"
                    );
                    return Err(e);
                }
            }
            CopyDataSource::Data(data) => {
                if let Err(e) = append_tar_bytes(ar, data, &target_path).await {
                    log::error!("Could not append data to tar: {target_path}");
                    return Err(e);
                }
            }
        };

        Ok(())
    }
}

#[derive(Debug)]
struct ArchiveStats {
    regular_files: usize,
    directories: usize,
}

impl ArchiveStats {
    fn should_extract_as_directory(&self) -> bool {
        self.directories > 0 || self.regular_files > 1
    }
}

fn analyze_archive(data: &[u8]) -> Result<ArchiveStats, CopyFromContainerError> {
    let mut archive = TarArchive::new(Cursor::new(data));
    let mut stats = ArchiveStats {
        regular_files: 0,
        directories: 0,
    };

    for entry_result in archive.entries().map_err(io::Error::from)? {
        let entry = entry_result.map_err(io::Error::from)?;
        let entry_type = entry.header().entry_type();
        if entry_type.is_file() || entry_type.is_contiguous() {
            stats.regular_files += 1;
        } else if entry_type.is_dir() {
            stats.directories += 1;
        }
    }

    Ok(stats)
}

fn write_single_file_from_archive(
    data: &[u8],
    destination: PathBuf,
) -> Result<CopyFromOutcome, CopyFromContainerError> {
    let mut archive = TarArchive::new(Cursor::new(data));
    let mut file_written = false;

    for entry_result in archive.entries().map_err(io::Error::from)? {
        let mut entry = entry_result.map_err(io::Error::from)?;
        let entry_type = entry.header().entry_type();

        if entry_type.is_file() || entry_type.is_contiguous() {
            if file_written {
                return Err(CopyFromContainerError::MultipleRegularFiles { destination });
            }

            if let Some(parent) = destination.parent() {
                if !parent.as_os_str().is_empty() {
                    std::fs::create_dir_all(parent)?;
                }
            }

            let mut file = std::fs::File::create(&destination)?;
            std::io::copy(&mut entry, &mut file)?;
            file_written = true;
        } else if entry_type.is_dir()
            || entry_type.is_gnu_longname()
            || entry_type.is_gnu_longlink()
            || entry_type.is_pax_global_extensions()
            || entry_type.is_pax_local_extensions()
            || entry_type.is_gnu_sparse()
        {
            continue;
        } else {
            return Err(CopyFromContainerError::UnsupportedEntryType {
                destination,
                entry_type,
            });
        }
    }

    if file_written {
        Ok(CopyFromOutcome::File(destination))
    } else {
        Err(CopyFromContainerError::EmptyArchive { destination })
    }
}

fn extract_archive_to_dir(
    data: &[u8],
    destination: PathBuf,
) -> Result<CopyFromOutcome, CopyFromContainerError> {
    let mut archive = TarArchive::new(Cursor::new(data));
    std::fs::create_dir_all(&destination)?;
    archive.unpack(&destination).map_err(io::Error::from)?;
    Ok(CopyFromOutcome::Directory(destination))
}

async fn append_tar_file(
    ar: &mut tokio_tar::Builder<Vec<u8>>,
    source_file_path: &Path,
    target_path: &str,
) -> Result<(), CopyToContainerError> {
    let target_path = make_path_relative(target_path);
    let meta = tokio::fs::metadata(source_file_path)
        .await
        .map_err(CopyToContainerError::IoError)?;

    if meta.is_dir() {
        ar.append_dir_all(target_path, source_file_path)
            .await
            .map_err(CopyToContainerError::IoError)?;
    } else {
        let f = &mut tokio::fs::File::open(source_file_path)
            .await
            .map_err(CopyToContainerError::IoError)?;

        ar.append_file(target_path, f)
            .await
            .map_err(CopyToContainerError::IoError)?;
    };

    Ok(())
}

async fn append_tar_bytes(
    ar: &mut tokio_tar::Builder<Vec<u8>>,
    data: &Vec<u8>,
    target_path: &str,
) -> Result<(), CopyToContainerError> {
    let relative_target_path = make_path_relative(target_path);

    let mut header = tokio_tar::Header::new_gnu();
    header.set_size(data.len() as u64);
    header.set_mode(0o0644);
    header.set_cksum();

    ar.append_data(&mut header, relative_target_path, data.as_slice())
        .await
        .map_err(CopyToContainerError::IoError)?;

    Ok(())
}

fn make_path_relative(path: &str) -> String {
    // TODO support also absolute windows paths like "C:\temp\foo.txt"
    if path.starts_with("/") {
        path.trim_start_matches("/").to_string()
    } else {
        path.to_string()
    }
}

#[cfg(test)]
mod tests {
    use std::{fs::File, io::Write};

    use tempfile::tempdir;
    use tokio::fs;

    use super::*;

    #[tokio::test]
    async fn copytocontainer_tar_file_success() {
        let temp_dir = tempdir().unwrap();
        let file_path = temp_dir.path().join("file.txt");
        let mut file = File::create(&file_path).unwrap();
        writeln!(file, "TEST").unwrap();

        let copy_to_container = CopyToContainer::new(file_path, "file.txt");
        let result = copy_to_container.tar().await;

        assert!(result.is_ok());
        let bytes = result.unwrap();
        assert!(!bytes.is_empty());
    }

    #[tokio::test]
    async fn copytocontainer_tar_data_success() {
        let data = vec![1, 2, 3, 4, 5];
        let copy_to_container = CopyToContainer::new(data, "data.bin");
        let result = copy_to_container.tar().await;

        assert!(result.is_ok());
        let bytes = result.unwrap();
        assert!(!bytes.is_empty());
    }

    #[tokio::test]
    async fn copytocontainer_tar_file_not_found() {
        let temp_dir = tempdir().unwrap();
        let non_existent_file_path = temp_dir.path().join("non_existent_file.txt");

        let copy_to_container = CopyToContainer::new(non_existent_file_path, "file.txt");
        let result = copy_to_container.tar().await;

        assert!(result.is_err());
        if let Err(CopyToContainerError::IoError(err)) = result {
            assert_eq!(err.kind(), io::ErrorKind::NotFound);
        } else {
            panic!("Expected IoError");
        }
    }

    #[tokio::test]
    async fn copytocontainercollection_tar_file_and_data() {
        let temp_dir = tempdir().unwrap();
        let file_path = temp_dir.path().join("file.txt");
        let mut file = File::create(&file_path).unwrap();
        writeln!(file, "TEST").unwrap();

        let copy_to_container_collection = CopyToContainerCollection::new(vec![
            CopyToContainer::new(file_path, "file.txt"),
            CopyToContainer::new(vec![1, 2, 3, 4, 5], "data.bin"),
        ]);

        let result = copy_to_container_collection.tar().await;

        assert!(result.is_ok());
        let bytes = result.unwrap();
        assert!(!bytes.is_empty());
    }

    #[tokio::test]
    async fn copyfromarchive_write_to_file_success() {
        let data = b"hello world".to_vec();
        let copy_to_container = CopyToContainer::new(data.clone(), "payload.txt");
        let bytes = copy_to_container.tar().await.unwrap();
        let archive = CopyFromArchive::new(bytes);

        let temp_dir = tempdir().unwrap();
        let destination = temp_dir.path().join("out.txt");

        let outcome = archive.write_to_file(&destination).await.unwrap();
        assert!(matches!(outcome, CopyFromOutcome::File(ref path) if path == &destination));

        let written = fs::read(&destination).await.unwrap();
        assert_eq!(written, data);
    }

    #[tokio::test]
    async fn copyfromarchive_write_to_file_rejects_multiple_files() {
        let collection = CopyToContainerCollection::new(vec![
            CopyToContainer::new(vec![1u8], "dir/file1.txt"),
            CopyToContainer::new(vec![2u8], "dir/file2.txt"),
        ]);
        let bytes = collection.tar().await.unwrap();
        let archive = CopyFromArchive::new(bytes);

        let temp_dir = tempdir().unwrap();
        let destination = temp_dir.path().join("out.txt");

        let err = archive.write_to_file(&destination).await.unwrap_err();
        match err {
            CopyFromContainerError::MultipleRegularFiles { destination: dest } => {
                assert_eq!(dest, destination);
            }
            other => panic!("expected MultipleRegularFiles error, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn copyfromarchive_extract_to_dir_success() {
        let collection = CopyToContainerCollection::new(vec![
            CopyToContainer::new(vec![1u8], "root/file1.txt"),
            CopyToContainer::new(vec![2u8], "root/nested/file2.txt"),
        ]);
        let bytes = collection.tar().await.unwrap();
        let archive = CopyFromArchive::new(bytes);

        let temp_dir = tempdir().unwrap();
        let destination = temp_dir.path().join("extracted");

        let outcome = archive.extract_to_dir(&destination).await.unwrap();
        assert!(matches!(outcome, CopyFromOutcome::Directory(ref path) if path == &destination));

        let file1 = fs::read(destination.join("root/file1.txt")).await.unwrap();
        let file2 = fs::read(destination.join("root/nested/file2.txt"))
            .await
            .unwrap();

        assert_eq!(file1, vec![1u8]);
        assert_eq!(file2, vec![2u8]);
    }
}
