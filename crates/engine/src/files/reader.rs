//! Reading a file's chunks as a stream (ADR-0044).

use std::future::Future;
use std::io::{self, SeekFrom};
use std::pin::Pin;
use std::task::{Context, Poll, ready};

use interlaken_storage::FjallStorage;
use tokio::io::{AsyncRead, AsyncSeek, ReadBuf};

use super::chunks::{self, CHUNK_SIZE};
use super::meta::FileMeta;
use crate::EngineError;

/// A chunk being loaded.
type Loading = Pin<Box<dyn Future<Output = Result<Vec<u8>, EngineError>> + Send>>;

/// A file being read: [`AsyncRead`] and [`AsyncSeek`], or
/// [`FileReader::read_all`].
///
/// The reader holds the file's metadata as the session saw it when it was
/// opened, and loads one chunk at a time, checking each against its hash.
/// It doesn't borrow the session.
pub struct FileReader {
    /// The storage chunks are loaded from.
    storage: FjallStorage,
    /// The namespace the file is in.
    namespace: String,
    /// The file's metadata.
    meta: FileMeta,
    /// The read position.
    position: u64,
    /// The index and content of the chunk loaded last.
    current: Option<(usize, Vec<u8>)>,
    /// The chunk being loaded, and its index.
    loading: Option<(usize, Loading)>,
}

impl FileReader {
    /// Construct a reader of the file `meta` describes.
    pub(crate) fn new(storage: FjallStorage, namespace: String, meta: FileMeta) -> Self {
        Self {
            storage,
            namespace,
            meta,
            position: 0,
            current: None,
            loading: None,
        }
    }

    /// Return the file's metadata.
    #[must_use]
    pub fn meta(&self) -> &FileMeta {
        &self.meta
    }

    /// Return the chunk holding the read position, loading it if needed;
    /// `None` at the end of the file.
    fn poll_chunk(&mut self, cx: &mut Context<'_>) -> Poll<Result<Option<usize>, EngineError>> {
        if self.position >= self.meta.size {
            return Poll::Ready(Ok(None));
        }
        let index = usize::try_from(self.position / CHUNK_SIZE as u64).unwrap_or(usize::MAX);
        if self.current.as_ref().is_some_and(|(i, _)| *i == index) {
            return Poll::Ready(Ok(Some(index)));
        }
        if self.loading.as_ref().is_none_or(|(i, _)| *i != index) {
            let Some(hash) = self.meta.chunks.get(index).cloned() else {
                return Poll::Ready(Err(EngineError::FileCorrupt(format!(
                    "{} is {} bytes but has {} chunks",
                    self.meta.name,
                    self.meta.size,
                    self.meta.chunks.len()
                ))));
            };
            let storage = self.storage.clone();
            let namespace = self.namespace.clone();
            self.loading = Some((
                index,
                Box::pin(async move { chunks::load(&storage, &namespace, &hash).await }),
            ));
        }
        let (_, loading) = self.loading.as_mut().expect("set above");
        let bytes = ready!(loading.as_mut().poll(cx));
        self.loading = None;
        let bytes = bytes?;
        let expected = (self.meta.size - index as u64 * CHUNK_SIZE as u64).min(CHUNK_SIZE as u64);
        if bytes.len() as u64 != expected {
            return Poll::Ready(Err(EngineError::FileCorrupt(format!(
                "chunk {index} of {} has {} bytes, not {expected}",
                self.meta.name,
                bytes.len()
            ))));
        }
        self.current = Some((index, bytes));
        Poll::Ready(Ok(Some(index)))
    }

    /// Copy bytes from the read position into `out`; return how many,
    /// 0 at the end of the file.
    fn poll_read_into(
        &mut self,
        cx: &mut Context<'_>,
        out: &mut [u8],
    ) -> Poll<Result<usize, EngineError>> {
        if out.is_empty() {
            return Poll::Ready(Ok(0));
        }
        let Some(index) = ready!(self.poll_chunk(cx))? else {
            return Poll::Ready(Ok(0));
        };
        let (_, bytes) = self.current.as_ref().expect("loaded");
        let offset = usize::try_from(self.position - index as u64 * CHUNK_SIZE as u64)
            .expect("within a chunk");
        let n = out.len().min(bytes.len() - offset);
        out[..n].copy_from_slice(&bytes[offset..offset + n]);
        self.position += n as u64;
        Poll::Ready(Ok(n))
    }

    /// Return the rest of the file from the read position.
    ///
    /// # Errors
    ///
    /// Returns [`EngineError::FileCorrupt`] for a chunk that is missing or
    /// doesn't match its hash, or a storage error.
    pub async fn read_all(mut self) -> Result<Vec<u8>, EngineError> {
        let remaining = usize::try_from(self.meta.size.saturating_sub(self.position))
            .map_err(|_| EngineError::Invalid("the file is too large to hold".into()))?;
        let mut out = vec![0; remaining];
        let mut filled = 0;
        while filled < out.len() {
            let n = std::future::poll_fn(|cx| self.poll_read_into(cx, &mut out[filled..])).await?;
            if n == 0 {
                break;
            }
            filled += n;
        }
        out.truncate(filled);
        Ok(out)
    }
}

impl AsyncRead for FileReader {
    /// Read from the read position, loading chunks as needed.
    fn poll_read(
        self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &mut ReadBuf<'_>,
    ) -> Poll<io::Result<()>> {
        let this = self.get_mut();
        let n =
            ready!(this.poll_read_into(cx, buf.initialize_unfilled())).map_err(io::Error::other)?;
        buf.advance(n);
        Poll::Ready(Ok(()))
    }
}

impl AsyncSeek for FileReader {
    /// Move the read position; positions past the end read nothing.
    fn start_seek(self: Pin<&mut Self>, position: SeekFrom) -> io::Result<()> {
        let this = self.get_mut();
        let target = match position {
            SeekFrom::Start(n) => Some(n),
            SeekFrom::End(delta) => this.meta.size.checked_add_signed(delta),
            SeekFrom::Current(delta) => this.position.checked_add_signed(delta),
        };
        this.position = target.ok_or_else(|| {
            io::Error::new(
                io::ErrorKind::InvalidInput,
                "seek before the start of the file",
            )
        })?;
        Ok(())
    }

    /// Return the read position.
    fn poll_complete(self: Pin<&mut Self>, _cx: &mut Context<'_>) -> Poll<io::Result<u64>> {
        Poll::Ready(Ok(self.position))
    }
}
