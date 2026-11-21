use std::pin::Pin;
use std::task::{Context, Poll};

use crate::websocket::{self, SignalWebSocket};

use super::ServiceError;

/// The most a sticker or a sticker pack's manifest may be, as it comes
/// from the CDN (encrypted): Signal Desktop's
/// `MAX_STICKER_CIPHERTEXT_BYTE_LENGTH` and
/// `MAX_STICKER_PACK_MANIFEST_CIPHERTEXT_BYTE_LENGTH`
/// (`ts/types/AttachmentSize.std.ts`, Desktop commit 5476a2607).
pub const MAX_STICKER_CIPHERTEXT_BYTES: u64 = 1024 * 1024;

/// A reader that fails once more than `limit` bytes came through, as
/// Desktop's `maxResponseSize` stops a sticker download.
struct Limited<R> {
    inner: R,
    left: u64,
}

impl<R: futures::io::AsyncRead + Unpin> futures::io::AsyncRead for Limited<R> {
    fn poll_read(
        mut self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &mut [u8],
    ) -> Poll<std::io::Result<usize>> {
        let read = futures::ready!(Pin::new(&mut self.inner).poll_read(cx, buf))?;
        match self.left.checked_sub(read as u64) {
            Some(left) => {
                self.left = left;
                Poll::Ready(Ok(read))
            },
            None => Poll::Ready(Err(std::io::Error::other(format!(
                "response over {MAX_STICKER_CIPHERTEXT_BYTES} bytes"
            )))),
        }
    }
}

fn limited(
    inner: impl futures::io::AsyncRead + Send + Unpin,
) -> impl futures::io::AsyncRead + Send + Unpin {
    Limited {
        inner,
        left: MAX_STICKER_CIPHERTEXT_BYTES,
    }
}

impl<C: websocket::WebSocketType> SignalWebSocket<C> {
    pub async fn get_sticker_pack_manifest(
        &mut self,
        id: &str,
    ) -> Result<impl futures::io::AsyncRead + Send + Unpin, ServiceError> {
        let path = format!("/stickers/{}/manifest.proto", id);
        Ok(limited(
            self.unidentified_push_service
                .get_from_cdn(0, &path)
                .await?
                .stream,
        ))
    }

    pub async fn get_sticker(
        &mut self,
        pack_id: &str,
        sticker_id: u32,
    ) -> Result<impl futures::io::AsyncRead + Send + Unpin, ServiceError> {
        let path = format!("/stickers/{}/full/{}", pack_id, sticker_id);
        Ok(limited(
            self.unidentified_push_service
                .get_from_cdn(0, &path)
                .await?
                .stream,
        ))
    }
}

#[cfg(test)]
mod tests {
    use futures::io::AsyncReadExt as _;

    #[test]
    fn stickers_over_a_mebibyte_fail() {
        futures::executor::block_on(async {
            let max = super::MAX_STICKER_CIPHERTEXT_BYTES as usize;
            let mut out = Vec::new();
            let fits = vec![0u8; max];
            super::limited(futures::io::Cursor::new(fits))
                .read_to_end(&mut out)
                .await
                .expect("exactly the limit reads");
            assert_eq!(out.len(), max);
            let over = vec![0u8; max + 1];
            let error = super::limited(futures::io::Cursor::new(over))
                .read_to_end(&mut Vec::new())
                .await
                .expect_err("over the limit fails");
            assert!(error.to_string().contains("bytes"));
        });
    }
}
