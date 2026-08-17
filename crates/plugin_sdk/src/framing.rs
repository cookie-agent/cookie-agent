use tokio::io::{AsyncBufRead, AsyncBufReadExt as _};

use crate::PluginError;

pub(crate) const MAX_FRAME_BYTES: usize = 4 * 1024 * 1024;

pub(crate) async fn read_frame<R>(reader: &mut R) -> Result<Option<String>, PluginError>
where
    R: AsyncBufRead + Unpin,
{
    let mut frame = Vec::new();
    loop {
        let (take, newline, empty) = {
            let available = reader.fill_buf().await?;
            let newline = available.iter().position(|byte| *byte == b'\n');
            (
                newline.unwrap_or(available.len()),
                newline.is_some(),
                available.is_empty(),
            )
        };

        if empty {
            return if frame.is_empty() {
                Ok(None)
            } else {
                decode_frame(frame).map(Some)
            };
        }
        if frame.len().saturating_add(take) > MAX_FRAME_BYTES {
            return Err(PluginError::Protocol(format!(
                "engine frame exceeds {MAX_FRAME_BYTES} bytes"
            )));
        }

        let consume = take + usize::from(newline);
        {
            let available = reader.fill_buf().await?;
            frame.extend_from_slice(&available[..take]);
        }
        reader.consume(consume);
        if newline {
            return decode_frame(frame).map(Some);
        }
    }
}

fn decode_frame(mut frame: Vec<u8>) -> Result<String, PluginError> {
    if frame.last() == Some(&b'\r') {
        frame.pop();
    }
    String::from_utf8(frame)
        .map_err(|error| PluginError::Protocol(format!("engine frame is invalid UTF-8: {error}")))
}

#[cfg(test)]
mod tests {
    use tokio::io::{AsyncWriteExt as _, BufReader};

    use super::*;

    #[tokio::test]
    async fn reads_partial_lines_and_eof_terminated_frame() {
        let (client, server) = tokio::io::duplex(64);
        let mut reader = BufReader::new(server);
        let writer = tokio::spawn(async move {
            let mut client = client;
            client.write_all(b"par").await.unwrap();
            tokio::task::yield_now().await;
            client.write_all(b"tial\r\nlast").await.unwrap();
        });

        assert_eq!(
            read_frame(&mut reader).await.unwrap().as_deref(),
            Some("partial")
        );
        writer.await.unwrap();
        assert_eq!(
            read_frame(&mut reader).await.unwrap().as_deref(),
            Some("last")
        );
        assert!(read_frame(&mut reader).await.unwrap().is_none());
    }

    #[tokio::test]
    async fn rejects_invalid_utf8() {
        let mut reader = BufReader::new(&b"\xff\n"[..]);
        let error = read_frame(&mut reader).await.unwrap_err();
        assert!(error.to_string().contains("UTF-8"));
    }

    #[tokio::test]
    async fn accepts_cap_and_rejects_cap_plus_one() {
        let accepted = vec![b'x'; MAX_FRAME_BYTES];
        let mut reader = BufReader::new(accepted.as_slice());
        assert_eq!(
            read_frame(&mut reader).await.unwrap().unwrap().len(),
            MAX_FRAME_BYTES
        );

        let rejected = vec![b'x'; MAX_FRAME_BYTES + 1];
        let mut reader = BufReader::new(rejected.as_slice());
        let error = read_frame(&mut reader).await.unwrap_err();
        assert!(error.to_string().contains("4194304"));
    }
}
