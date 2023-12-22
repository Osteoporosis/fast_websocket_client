// Copyright 2023 Hyoungjun Son
// Copyright 2023 Divy Srivastava <dj.srivastava23@gmail.com>
//
// Licensed under the Apache License, Version 2.0 (the "License");
// you may not use this file except in compliance with the License.
// You may obtain a copy of the License at
//
// http://www.apache.org/licenses/LICENSE-2.0
//
// Unless required by applicable law or agreed to in writing, software
// distributed under the License is distributed on an "AS IS" BASIS,
// WITHOUT WARRANTIES OR CONDITIONS OF ANY KIND, either express or implied.
// See the License for the specific language governing permissions and
// limitations under the License.
// from fastwebsockets 0.6.0

use crate::recv::{self, SharedRecv};
use fastwebsockets::{CloseCode, Frame, OpCode, Payload, Role, WebSocketError};
use tokio::io::{AsyncReadExt, AsyncWriteExt};

#[derive(Copy, Clone, Default)]
struct UnsendMarker(std::marker::PhantomData<SharedRecv>);

/// WebSocket protocol implementation over an async stream.
pub(crate) struct WebSocket<S> {
    stream: S,
    write_half: WriteHalf,
    read_half: ReadHalf,
    _marker: UnsendMarker,
}

#[allow(dead_code)]
impl<'f, S> WebSocket<S> {
    /// Creates a new `WebSocket` from a stream that has already completed the WebSocket handshake.
    ///
    /// Use the `upgrade` feature to handle server upgrades and client handshakes.
    pub(crate) fn after_handshake(stream: S, role: Role) -> Self
    where
        S: AsyncReadExt + AsyncWriteExt + Unpin,
    {
        recv::init_once();
        Self {
            stream,
            write_half: WriteHalf::after_handshake(role),
            read_half: ReadHalf::after_handshake(role),
            _marker: UnsendMarker::default(),
        }
    }

    /// Consumes the `WebSocket` and returns the underlying stream.
    #[inline]
    pub(crate) fn into_inner(self) -> S {
        // self.write_half.into_inner().stream
        self.stream
    }

    /// Consumes the `WebSocket` and returns the underlying stream.
    #[inline]
    pub(crate) fn into_parts_internal(self) -> (S, ReadHalf, WriteHalf) {
        (self.stream, self.read_half, self.write_half)
    }

    /// Sets whether to use vectored writes. This option does not guarantee that vectored writes will be always used.
    ///
    /// Default: `true`
    pub(crate) fn set_writev(&mut self, vectored: bool) {
        self.write_half.vectored = vectored;
    }

    pub(crate) fn set_writev_threshold(&mut self, threshold: usize) {
        self.read_half.writev_threshold = threshold;
        self.write_half.writev_threshold = threshold;
    }

    /// Sets whether to automatically close the connection when a close frame is received. When set to `false`, the application will have to manually send close frames.
    ///
    /// Default: `true`
    pub(crate) fn set_auto_close(&mut self, auto_close: bool) {
        self.read_half.auto_close = auto_close;
    }

    /// Sets whether to automatically send a pong frame when a ping frame is received.
    ///
    /// Default: `true`
    pub(crate) fn set_auto_pong(&mut self, auto_pong: bool) {
        self.read_half.auto_pong = auto_pong;
    }

    /// Sets the maximum message size in bytes. If a message is received that is larger than this, the connection will be closed.
    ///
    /// Default: 64 MiB
    pub(crate) fn set_max_message_size(&mut self, max_message_size: usize) {
        self.read_half.max_message_size = max_message_size;
    }

    /// Sets whether to automatically apply the mask to the frame payload.
    ///
    /// Default: `true`
    pub(crate) fn set_auto_apply_mask(&mut self, auto_apply_mask: bool) {
        self.read_half.auto_apply_mask = auto_apply_mask;
        self.write_half.auto_apply_mask = auto_apply_mask;
    }

    pub(crate) fn is_closed(&self) -> bool {
        self.write_half.closed
    }

    /// Writes a frame to the stream.
    pub(crate) async fn write_frame(&mut self, frame: Frame<'f>) -> Result<(), WebSocketError>
    where
        S: AsyncReadExt + AsyncWriteExt + Unpin,
    {
        self.write_half.write_frame(&mut self.stream, frame).await?;
        Ok(())
    }

    /// Reads a frame from the stream.
    ///
    /// This method will unmask the frame payload. For fragmented frames, use `FragmentCollector::read_frame`.
    ///
    /// Text frames payload is guaranteed to be valid UTF-8.
    pub(crate) async fn read_frame(&mut self) -> Result<Frame<'f>, WebSocketError>
    where
        S: AsyncReadExt + AsyncWriteExt + Unpin,
    {
        loop {
            let (res, obligated_send) = self.read_half.read_frame_inner(&mut self.stream).await;
            let is_closed = self.write_half.closed;
            if let Some(frame) = obligated_send {
                if !is_closed {
                    self.write_half.write_frame(&mut self.stream, frame).await?;
                }
            }
            if let Some(frame) = res? {
                if is_closed && frame.opcode != OpCode::Close {
                    return Err(WebSocketError::ConnectionClosed);
                }
                break Ok(frame);
            }
        }
    }
}

pub(crate) struct WriteHalf {
    role: Role,
    pub(crate) closed: bool,
    pub(crate) vectored: bool,
    pub(crate) auto_apply_mask: bool,
    pub(crate) writev_threshold: usize,
    write_buffer: Vec<u8>,
}

impl WriteHalf {
    pub(crate) fn after_handshake(role: Role) -> Self {
        Self {
            role,
            closed: false,
            auto_apply_mask: true,
            vectored: true,
            writev_threshold: 1024,
            write_buffer: Vec::with_capacity(2),
        }
    }

    /// Writes a frame to the provided stream.
    pub(crate) async fn write_frame<'a, S>(
        &'a mut self,
        stream: &mut S,
        mut frame: Frame<'a>,
    ) -> Result<(), WebSocketError>
    where
        S: AsyncWriteExt + Unpin,
    {
        if self.role == Role::Client && self.auto_apply_mask {
            frame.mask();
        }

        if frame.opcode == OpCode::Close {
            self.closed = true;
        } else if self.closed {
            return Err(WebSocketError::ConnectionClosed);
        }

        if self.vectored && frame.payload.len() > self.writev_threshold {
            frame.writev(stream).await?;
        } else {
            let text = frame.write(&mut self.write_buffer);
            stream.write_all(text).await?;
        }

        Ok(())
    }
}

pub(crate) struct ReadHalf {
    role: Role,
    spill: Option<Vec<u8>>,
    pub(crate) auto_apply_mask: bool,
    pub(crate) auto_close: bool,
    pub(crate) auto_pong: bool,
    pub(crate) writev_threshold: usize,
    pub(crate) max_message_size: usize,
}

impl ReadHalf {
    pub(crate) fn after_handshake(role: Role) -> Self {
        Self {
            role,
            spill: None,
            auto_apply_mask: true,
            auto_close: true,
            auto_pong: true,
            writev_threshold: 1024,
            max_message_size: 64 << 20,
        }
    }

    /// Attempt to read a single frame from from the incoming stream, returning any send obligations if
    /// `auto_close` or `auto_pong` are enabled. Callers to this function are obligated to send the
    /// frame in the latter half of the tuple if one is specified, unless the write half of this socket
    /// has been closed.
    ///
    /// XXX: Do not expose this method to the public API.
    /// Lifetime requirements for safe recv buffer use are not enforced.
    pub(crate) async fn read_frame_inner<'f, S>(
        &mut self,
        stream: &mut S,
    ) -> (Result<Option<Frame<'f>>, WebSocketError>, Option<Frame<'f>>)
    where
        S: AsyncReadExt + Unpin,
    {
        let mut frame = match self.parse_frame_header(stream).await {
            Ok(frame) => frame,
            Err(e) => return (Err(e), None),
        };

        if self.role == Role::Server && self.auto_apply_mask {
            frame.unmask()
        };

        match frame.opcode {
            OpCode::Close if self.auto_close => {
                match frame.payload.len() {
                    0 => {}
                    1 => return (Err(WebSocketError::InvalidCloseFrame), None),
                    _ => {
                        let code = CloseCode::from(u16::from_be_bytes(
                            frame.payload[0..2].try_into().unwrap(),
                        ));

                        #[cfg(feature = "simd")]
                        if simdutf8::basic::from_utf8(&frame.payload[2..]).is_err() {
                            return (Err(WebSocketError::InvalidUTF8), None);
                        };

                        #[cfg(not(feature = "simd"))]
                        if std::str::from_utf8(&frame.payload[2..]).is_err() {
                            return (Err(WebSocketError::InvalidUTF8), None);
                        };

                        if !code.is_allowed() {
                            return (
                                Err(WebSocketError::InvalidCloseCode),
                                Some(Frame::close(1002, &frame.payload[2..])),
                            );
                        }
                    }
                };

                let obligated_send = Frame::close_raw(frame.payload.to_owned().into());
                (Ok(Some(frame)), Some(obligated_send))
            }
            OpCode::Ping if self.auto_pong => (Ok(None), Some(Frame::pong(frame.payload))),
            OpCode::Text => {
                if frame.fin && !frame.is_utf8() {
                    (Err(WebSocketError::InvalidUTF8), None)
                } else {
                    (Ok(Some(frame)), None)
                }
            }
            _ => (Ok(Some(frame)), None),
        }
    }

    async fn parse_frame_header<'a, S>(
        &mut self,
        stream: &mut S,
    ) -> Result<Frame<'a>, WebSocketError>
    where
        S: AsyncReadExt + Unpin,
    {
        macro_rules! eof {
            ($n:expr) => {{
                let n = $n;
                if n == 0 {
                    return Err(WebSocketError::UnexpectedEOF);
                }
                n
            }};
        }

        let head = crate::recv::init_once();
        let mut nread = 0;

        if let Some(spill) = self.spill.take() {
            head[..spill.len()].copy_from_slice(&spill);
            nread += spill.len();
        }

        while nread < 2 {
            nread += eof!(stream.read(&mut head[nread..]).await?);
        }

        let fin = head[0] & 0b10000000 != 0;

        let rsv1 = head[0] & 0b01000000 != 0;
        let rsv2 = head[0] & 0b00100000 != 0;
        let rsv3 = head[0] & 0b00010000 != 0;

        if rsv1 || rsv2 || rsv3 {
            return Err(WebSocketError::ReservedBitsNotZero);
        }

        let opcode = OpCode::try_from(head[0] & 0b00001111)?;
        let masked = head[1] & 0b10000000 != 0;

        let length_code = head[1] & 0x7F;
        let extra = match length_code {
            126 => 2,
            127 => 8,
            _ => 0,
        };

        if extra > 0 {
            while nread < 2 + extra {
                nread += eof!(stream.read(&mut head[nread..]).await?);
            }
        }
        let length: usize = match extra {
            0 => usize::from(length_code),
            2 => u16::from_be_bytes(head[2..4].try_into().unwrap()) as usize,
            #[cfg(any(target_pointer_width = "64", target_pointer_width = "128"))]
            8 => u64::from_be_bytes(head[2..10].try_into().unwrap()) as usize,
            // On 32bit systems, usize is only 4bytes wide so we must check for usize overflowing
            #[cfg(any(
                target_pointer_width = "8",
                target_pointer_width = "16",
                target_pointer_width = "32"
            ))]
            8 => match usize::try_from(u64::from_be_bytes(head[2..10].try_into().unwrap())) {
                Ok(length) => length,
                Err(_) => return Err(WebSocketError::FrameTooLarge),
            },
            _ => unreachable!(),
        };

        let mask = match masked {
            true => {
                while nread < 2 + extra + 4 {
                    nread += eof!(stream.read(&mut head[nread..]).await?);
                }

                Some(head[2 + extra..2 + extra + 4].try_into().unwrap())
            }
            false => None,
        };

        if is_control(opcode) && !fin {
            return Err(WebSocketError::ControlFrameFragmented);
        }

        if opcode == OpCode::Ping && length > 125 {
            return Err(WebSocketError::PingFrameTooLarge);
        }

        if length >= self.max_message_size {
            return Err(WebSocketError::FrameTooLarge);
        }

        let required = 2 + extra + mask.map(|_| 4).unwrap_or(0) + length;
        if required > nread {
            // Allocate more space
            let mut new_head = head.to_vec();
            new_head.resize(required, 0);

            stream.read_exact(&mut new_head[nread..]).await?;
            return Ok(Frame::new(
                fin,
                opcode,
                mask,
                Payload::Owned(new_head[required - length..].to_vec()),
            ));
        } else if nread > required {
            // We read too much
            self.spill = Some(head[required..nread].to_vec());
        }

        let payload = &mut head[required - length..required];
        let payload = if payload.len() > self.writev_threshold {
            Payload::BorrowedMut(payload)
        } else {
            Payload::Owned(payload.to_vec())
        };
        let frame = Frame::new(fin, opcode, mask, payload);
        Ok(frame)
    }
}

fn is_control(opcode: OpCode) -> bool {
    matches!(opcode, OpCode::Close | OpCode::Ping | OpCode::Pong)
}
