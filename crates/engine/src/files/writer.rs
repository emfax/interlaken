//! Writing a file in chunks, staged as they fill (ADR-0044).

use std::future::Future;
use std::io;
use std::pin::Pin;
use std::task::{Context, Poll, ready};

use tokio::io::AsyncWrite;

use super::chunks::{CHUNK_SIZE, ChunkStore};
use super::fs::Files;
use super::meta::FileMeta;
use super::settings::Settings;
use crate::{EngineError, Session};

/// A chunk being staged, which resolves to its hash.
type Staging = Pin<Box<dyn Future<Output = Result<String, EngineError>> + Send>>;

/// A file being written: [`AsyncWrite`], or [`FileWriter::write_bytes`].
///
/// Each full 1 MiB chunk is stored in a short transaction of its own as
/// it fills, so a file of any size is written in bounded memory and
/// bounded transactions. Nothing is visible until [`FileWriter::finish`]
/// records the file in the session's transaction; a writer dropped before
/// that leaves only unreferenced chunks, which collection removes.
pub struct FileWriter<'a> {
    /// The session the file is recorded in.
    session: &'a mut Session,
    /// The namespace's file settings.
    settings: Settings,
    /// Where chunks are staged.
    chunks: ChunkStore,
    /// The path written, for errors.
    path: String,
    /// The id of the directory the file goes in.
    parent: String,
    /// The file's name.
    name: String,
    /// The id and size of the file replaced, if it exists.
    existing: Option<(String, u64)>,
    /// The most bytes the file may hold within the namespace's quota.
    limit: Option<u64>,
    /// Bytes not yet staged: less than one chunk.
    buffer: Vec<u8>,
    /// The chunk being staged, if any.
    staging: Option<Staging>,
    /// The hashes of the chunks staged so far.
    staged: Vec<String>,
    /// The hash of the whole content so far.
    hasher: blake3::Hasher,
    /// The bytes written so far.
    size: u64,
}

impl<'a> FileWriter<'a> {
    /// Construct a writer of `name` in directory `parent`.
    pub(crate) fn new(
        files: &'a mut Files<'_>,
        chunks: ChunkStore,
        path: String,
        parent: String,
        name: String,
        existing: Option<(String, u64)>,
        limit: Option<u64>,
    ) -> Self {
        let settings = files.settings.clone();
        Self {
            session: &mut *files.session,
            settings,
            chunks,
            path,
            parent,
            name,
            existing,
            limit,
            buffer: Vec::with_capacity(CHUNK_SIZE),
            staging: None,
            staged: Vec::new(),
            hasher: blake3::Hasher::new(),
            size: 0,
        }
    }

    /// Start staging the buffered chunk.
    fn start_staging(&mut self) {
        let bytes = std::mem::replace(&mut self.buffer, Vec::with_capacity(CHUNK_SIZE));
        let chunks = self.chunks.clone();
        self.staging = Some(Box::pin(async move { chunks.stage(bytes).await }));
    }

    /// Wait for the chunk being staged, if any, recording its hash.
    fn poll_staged(&mut self, cx: &mut Context<'_>) -> Poll<Result<(), EngineError>> {
        if let Some(staging) = self.staging.as_mut() {
            let hash = ready!(staging.as_mut().poll(cx));
            self.staging = None;
            self.staged.push(hash?);
        }
        Poll::Ready(Ok(()))
    }

    /// Append `data`, staging each chunk it fills; return how much of it was
    /// taken, which is all of it unless a chunk is being staged.
    fn poll_append(
        &mut self,
        cx: &mut Context<'_>,
        data: &[u8],
    ) -> Poll<Result<usize, EngineError>> {
        ready!(self.poll_staged(cx))?;
        let taken = data.len().min(CHUNK_SIZE - self.buffer.len());
        if let Some(limit) = self.limit
            && self.size + taken as u64 > limit
        {
            return Poll::Ready(Err(EngineError::QuotaExceeded(format!(
                "{} would pass namespace {}'s file quota",
                self.path, self.settings.namespace
            ))));
        }
        self.buffer.extend_from_slice(&data[..taken]);
        self.hasher.update(&data[..taken]);
        self.size += taken as u64;
        if self.buffer.len() == CHUNK_SIZE {
            self.start_staging();
        }
        Poll::Ready(Ok(taken))
    }

    /// Append all of `data`, staging each chunk it fills before returning.
    ///
    /// # Errors
    ///
    /// Returns a storage error from staging a chunk.
    pub async fn write_bytes(&mut self, mut data: &[u8]) -> Result<(), EngineError> {
        while !data.is_empty() {
            let taken = std::future::poll_fn(|cx| self.poll_append(cx, data)).await?;
            data = &data[taken..];
        }
        std::future::poll_fn(|cx| self.poll_staged(cx)).await
    }

    /// Return the bytes written so far.
    #[must_use]
    pub fn written(&self) -> u64 {
        self.size
    }

    /// Stage the last chunk and record the file, created or replaced, in
    /// the session's transaction (or an implicit one), returning its
    /// metadata.
    ///
    /// # Errors
    ///
    /// Returns [`EngineError::FileExists`] if another file took the name
    /// meanwhile, `42501` if policies don't permit the insert or update, or
    /// a storage error.
    pub async fn finish(mut self) -> Result<FileMeta, EngineError> {
        std::future::poll_fn(|cx| self.poll_staged(cx)).await?;
        if !self.buffer.is_empty() {
            self.start_staging();
            std::future::poll_fn(|cx| self.poll_staged(cx)).await?;
        }
        let hash = self.hasher.finalize().to_hex().to_string();
        let content = (self.size, hash, std::mem::take(&mut self.staged));
        let mut files = Files {
            session: self.session,
            settings: self.settings,
        };
        files
            .record_file(
                &self.path,
                &self.parent,
                &self.name,
                self.existing
                    .as_ref()
                    .map(|(id, size)| (id.as_str(), *size)),
                content,
            )
            .await
    }
}

/// Return `error` as an I/O error.
fn io_error(error: EngineError) -> io::Error {
    io::Error::other(error)
}

impl AsyncWrite for FileWriter<'_> {
    /// Append bytes, staging each chunk they fill.
    fn poll_write(
        self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        data: &[u8],
    ) -> Poll<io::Result<usize>> {
        self.get_mut().poll_append(cx, data).map_err(io_error)
    }

    /// Wait for the chunk being staged; a partial chunk stays buffered
    /// until [`FileWriter::finish`].
    fn poll_flush(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<io::Result<()>> {
        self.get_mut().poll_staged(cx).map_err(io_error)
    }

    /// As [`AsyncWrite::poll_flush`]: the file is recorded by
    /// [`FileWriter::finish`].
    fn poll_shutdown(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<io::Result<()>> {
        self.get_mut().poll_staged(cx).map_err(io_error)
    }
}
