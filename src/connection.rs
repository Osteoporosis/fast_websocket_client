use fastwebsockets::{
    CloseCode, Frame, OpCode, Payload, WebSocketError, WebSocketRead, WebSocketWrite,
};
use hyper_util::rt::TokioIo;

type Stream = TokioIo<hyper::upgrade::Upgraded>;

pub(crate) struct Reader {
    inner: WebSocketRead<tokio::io::ReadHalf<Stream>>,
    fragment: Option<(OpCode, Vec<u8>)>,
    max_message_size: usize,
}

impl Reader {
    pub(crate) fn new(
        inner: WebSocketRead<tokio::io::ReadHalf<Stream>>,
        max_message_size: usize,
    ) -> Self {
        Self {
            inner,
            fragment: None,
            max_message_size,
        }
    }

    // Keep this future alive across command/timer events. The backend parser
    // consumes partial headers before awaiting more bytes.
    pub(crate) async fn read(&mut self) -> Result<Frame<'static>, WebSocketError> {
        loop {
            // Automatic replies are disabled on the read half. The driver
            // writes them only after this read has completed.
            let frame = self
                .inner
                .read_frame(&mut |_| async { Ok::<_, WebSocketError>(()) })
                .await?;
            let frame = into_owned(frame);
            match frame.opcode {
                OpCode::Text | OpCode::Binary => {
                    if self.fragment.is_some() {
                        return Err(WebSocketError::InvalidFragment);
                    }
                    if frame.payload.len() > self.max_message_size {
                        return Err(WebSocketError::FrameTooLarge);
                    }
                    if frame.fin {
                        return Ok(frame);
                    }
                    self.fragment = Some((frame.opcode, frame.payload.into()));
                }
                OpCode::Continuation => {
                    let (opcode, data) = self
                        .fragment
                        .as_mut()
                        .ok_or(WebSocketError::InvalidContinuationFrame)?;
                    if data
                        .len()
                        .checked_add(frame.payload.len())
                        .is_none_or(|n| n > self.max_message_size)
                    {
                        return Err(WebSocketError::FrameTooLarge);
                    }
                    data.extend_from_slice(&frame.payload);
                    if frame.fin {
                        if *opcode == OpCode::Text && simdutf8::basic::from_utf8(data).is_err() {
                            return Err(WebSocketError::InvalidUTF8);
                        }
                        let (opcode, data) = self.fragment.take().unwrap();
                        return Ok(Frame::new(true, opcode, None, data.into()));
                    }
                }
                OpCode::Close => {
                    if frame.payload.len() == 1 || frame.payload.len() > 125 {
                        return Err(WebSocketError::InvalidCloseFrame);
                    }
                    if frame.payload.len() >= 2 {
                        let code = CloseCode::from(u16::from_be_bytes([
                            frame.payload[0],
                            frame.payload[1],
                        ]));
                        if !code.is_allowed() {
                            return Err(WebSocketError::InvalidCloseCode);
                        }
                        if simdutf8::basic::from_utf8(&frame.payload[2..]).is_err() {
                            return Err(WebSocketError::InvalidUTF8);
                        }
                    }
                    return Ok(frame);
                }
                OpCode::Ping | OpCode::Pong => {
                    if frame.payload.len() > 125 {
                        return Err(WebSocketError::InvalidValue);
                    }
                    return Ok(frame);
                }
            }
        }
    }
}

fn into_owned(frame: Frame<'_>) -> Frame<'static> {
    let payload = match frame.payload {
        Payload::Owned(bytes) => Payload::Owned(bytes),
        Payload::Bytes(bytes) => Payload::Bytes(bytes),
        other => Payload::Owned(other.into()),
    };
    Frame::new(frame.fin, frame.opcode, None, payload)
}

pub(crate) struct Writer {
    inner: WebSocketWrite<tokio::io::WriteHalf<Stream>>,
    auto_close: bool,
    pub(crate) auto_pong: bool,
}

impl Writer {
    pub(crate) fn new(
        inner: WebSocketWrite<tokio::io::WriteHalf<Stream>>,
        auto_close: bool,
        auto_pong: bool,
    ) -> Self {
        Self {
            inner,
            auto_close,
            auto_pong,
        }
    }

    pub(crate) fn is_closed(&self) -> bool {
        self.inner.is_closed()
    }

    pub(crate) async fn write(&mut self, frame: Frame<'_>) -> Result<(), WebSocketError> {
        if matches!(frame.opcode, OpCode::Close | OpCode::Ping | OpCode::Pong)
            && frame.payload.len() > 125
        {
            return Err(WebSocketError::InvalidValue);
        }
        self.inner.write_frame(frame).await?;
        self.inner.flush().await
    }

    pub(crate) async fn respond(&mut self, frame: &Frame<'_>) -> Result<(), WebSocketError> {
        if self.inner.is_closed() {
            return Ok(());
        }
        match frame.opcode {
            OpCode::Ping if self.auto_pong => {
                self.write(Frame::pong(frame.payload.as_ref().into())).await
            }
            OpCode::Close if self.auto_close => {
                self.write(Frame::close_raw(frame.payload.as_ref().into()))
                    .await
            }
            _ => Ok(()),
        }
    }
}
