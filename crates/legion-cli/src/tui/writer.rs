//! Non-blocking terminal output via a dedicated writer thread.
//!
//! The TUI event loop builds each frame into an in-memory buffer and hands the
//! finished bytes to a background thread that performs the actual stdout
//! `write_all`/`flush`. This keeps slow terminals (SSH, WSL, tmux) from
//! stalling the async event loop.

use std::io::{self, Write};
use std::sync::mpsc::{Sender, channel};
use std::thread::JoinHandle;

/// A [`Write`] implementation that buffers bytes and ships the complete buffer
/// to the writer thread on [`flush`](Self::flush).
pub(crate) struct TermWriter {
    tx: Sender<Vec<u8>>,
    buf: Vec<u8>,
}

impl Write for TermWriter {
    fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
        self.buf.extend_from_slice(buf);
        Ok(buf.len())
    }

    fn flush(&mut self) -> io::Result<()> {
        if !self.buf.is_empty() {
            let bytes = std::mem::take(&mut self.buf);
            self.tx
                .send(bytes)
                .map_err(|_| io::Error::new(io::ErrorKind::BrokenPipe, "writer thread gone"))?;
        }
        Ok(())
    }
}

impl Drop for TermWriter {
    fn drop(&mut self) {
        // Best-effort drain of any trailing bytes; errors here are expected when
        // the writer thread has already exited.
        let _ = self.flush();
    }
}

/// Handle to the background writer thread.
pub(crate) struct WriterThread {
    handle: Option<JoinHandle<io::Result<()>>>,
}

impl WriterThread {
    /// Spawn a thread that writes every byte it receives to `out`.
    ///
    /// Returns the buffered terminal writer, a clone of the underlying channel
    /// sender for direct scrollback writes, and the thread handle.
    pub(crate) fn spawn<W: Write + Send + 'static>(
        mut out: W,
    ) -> (TermWriter, Sender<Vec<u8>>, WriterThread) {
        let (tx, rx) = channel::<Vec<u8>>();
        let handle = std::thread::spawn(move || {
            while let Ok(bytes) = rx.recv() {
                out.write_all(&bytes)?;
                out.flush()?;
            }
            Ok(())
        });
        (
            TermWriter {
                tx: tx.clone(),
                buf: Vec::new(),
            },
            tx,
            WriterThread {
                handle: Some(handle),
            },
        )
    }

    /// Wait until the channel is closed and all pending bytes have been written.
    pub(crate) fn join(mut self) -> io::Result<()> {
        if let Some(handle) = self.handle.take() {
            handle
                .join()
                .map_err(|e| io::Error::other(format!("writer thread panicked: {e:?}")))?
        } else {
            Ok(())
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::{Arc, Mutex};

    #[derive(Clone)]
    struct SharedBuf(Arc<Mutex<Vec<u8>>>);

    impl Write for SharedBuf {
        fn write(&mut self, data: &[u8]) -> io::Result<usize> {
            self.0.lock().unwrap().extend_from_slice(data);
            Ok(data.len())
        }
        fn flush(&mut self) -> io::Result<()> {
            Ok(())
        }
    }

    #[test]
    fn writer_thread_receives_and_writes_bytes() {
        let buf = SharedBuf(Arc::new(Mutex::new(Vec::new())));
        let (mut term_writer, scrollback_tx, thread) = WriterThread::spawn(buf.clone());

        write!(term_writer, "hello").unwrap();
        term_writer.flush().unwrap();
        drop(term_writer);
        // Dropping the last sender closes the channel so the writer thread can
        // exit after draining.
        drop(scrollback_tx);
        thread.join().unwrap();

        assert_eq!(&*buf.0.lock().unwrap(), b"hello");
    }
}
