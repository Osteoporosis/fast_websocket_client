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
// from fastwebsockets 0.4.4

use fastwebsockets::Frame;
use fastwebsockets::OpCode;
use fastwebsockets::WebSocket;
use fastwebsockets::WebSocketError;
use tokio::io::AsyncReadExt;
use tokio::io::AsyncWriteExt;

enum Fragment {
    Text(Option<utf8::Incomplete>, Vec<u8>),
    Binary(Vec<u8>),
}

impl Fragment {
    /// Returns the payload of the fragment.
    fn take_buffer(self) -> Vec<u8> {
        match self {
            Fragment::Text(_, buffer) => buffer,
            Fragment::Binary(buffer) => buffer,
        }
    }
}

/// Collects fragmented messages over a WebSocket connection and returns the completed message once all fragments have been received.
///
/// This is useful for applications that do not want to deal with fragmented messages and the default behavior of tungstenite.
/// The payload is buffered in memory until the final fragment is received
/// so use this when streaming messages is not an option.
pub(crate) struct FragmentCollector<S> {
    ws: WebSocket<S>,
    fragments: Option<Fragment>,
    opcode: OpCode,
}

impl<'f, S> FragmentCollector<S> {
    /// Creates a new `FragmentCollector` with the provided `WebSocket`.
    pub(crate) fn new(ws: WebSocket<S>) -> FragmentCollector<S>
    where
        S: AsyncReadExt + AsyncWriteExt + Unpin,
    {
        FragmentCollector {
            ws,
            fragments: None,
            opcode: OpCode::Close,
        }
    }

    /// Sets whether to use vectored writes. This option does not guarantee that vectored writes will be always used.
    ///
    /// Default: `true`
    pub(crate) fn set_writev(&mut self, vectored: bool) {
        self.ws.set_writev(vectored);
    }

    pub(crate) fn set_writev_threshold(&mut self, threshold: usize) {
        self.ws.set_writev_threshold(threshold);
    }

    /// Sets whether to automatically close the connection when a close frame is received. When set to `false`, the application will have to manually send close frames.
    ///
    /// Default: `true`
    pub(crate) fn set_auto_close(&mut self, auto_close: bool) {
        self.ws.set_auto_close(auto_close);
    }

    /// Sets whether to automatically send a pong frame when a ping frame is received.
    ///
    /// Default: `true`
    pub(crate) fn set_auto_pong(&mut self, auto_pong: bool) {
        self.ws.set_auto_pong(auto_pong);
    }

    /// Sets the maximum message size in bytes. If a message is received that is larger than this, the connection will be closed.
    ///
    /// Default: 64 MiB
    pub(crate) fn set_max_message_size(&mut self, max_message_size: usize) {
        self.ws.set_max_message_size(max_message_size);
    }

    /// Sets whether to automatically apply the mask to the frame payload.
    ///
    /// Default: `true`
    pub(crate) fn set_auto_apply_mask(&mut self, auto_apply_mask: bool) {
        self.ws.set_auto_apply_mask(auto_apply_mask);
    }

    /// Reads a WebSocket frame, collecting fragmented messages until the final frame is received and returns the completed message.
    ///
    /// Text frames payload is guaranteed to be valid UTF-8.
    pub(crate) async fn read_frame(&mut self) -> Result<Frame<'f>, WebSocketError>
    where
        S: AsyncReadExt + AsyncWriteExt + Unpin,
    {
        loop {
            let frame = self.ws.read_frame().await?;
            match frame.opcode {
                OpCode::Text | OpCode::Binary => {
                    if frame.fin {
                        if self.fragments.is_some() {
                            return Err(WebSocketError::InvalidFragment);
                        }
                        return Ok(Frame::new(true, frame.opcode, None, frame.payload));
                    } else {
                        self.fragments = match frame.opcode {
                            OpCode::Text => match utf8::decode(&frame.payload) {
                                Ok(text) => Some(Fragment::Text(None, text.as_bytes().to_vec())),
                                Err(utf8::DecodeError::Incomplete {
                                    valid_prefix,
                                    incomplete_suffix,
                                }) => Some(Fragment::Text(
                                    Some(incomplete_suffix),
                                    valid_prefix.as_bytes().to_vec(),
                                )),
                                Err(utf8::DecodeError::Invalid { .. }) => {
                                    return Err(WebSocketError::InvalidUTF8);
                                }
                            },
                            OpCode::Binary => Some(Fragment::Binary(frame.payload.into())),
                            _ => unreachable!(),
                        };
                        self.opcode = frame.opcode;
                    }
                }
                OpCode::Continuation => match self.fragments.as_mut() {
                    None => {
                        return Err(WebSocketError::InvalidContinuationFrame);
                    }
                    Some(Fragment::Text(data, input)) => {
                        let mut tail = &frame.payload[..];
                        if let Some(mut incomplete) = data.take() {
                            if let Some((result, rest)) = incomplete.try_complete(&frame.payload) {
                                tail = rest;
                                match result {
                                    Ok(text) => {
                                        input.extend_from_slice(text.as_bytes());
                                    }
                                    Err(_) => {
                                        return Err(WebSocketError::InvalidUTF8);
                                    }
                                }
                            } else {
                                tail = &[];
                                data.replace(incomplete);
                            }
                        }

                        match utf8::decode(tail) {
                            Ok(text) => {
                                input.extend_from_slice(text.as_bytes());
                            }
                            Err(utf8::DecodeError::Incomplete {
                                valid_prefix,
                                incomplete_suffix,
                            }) => {
                                input.extend_from_slice(valid_prefix.as_bytes());
                                *data = Some(incomplete_suffix);
                            }
                            Err(utf8::DecodeError::Invalid { valid_prefix, .. }) => {
                                input.extend_from_slice(valid_prefix.as_bytes());
                                return Err(WebSocketError::InvalidUTF8);
                            }
                        }

                        if frame.fin {
                            return Ok(Frame::new(
                                true,
                                self.opcode,
                                None,
                                self.fragments.take().unwrap().take_buffer().into(),
                            ));
                        }
                    }
                    Some(Fragment::Binary(data)) => {
                        data.extend_from_slice(&frame.payload);
                        if frame.fin {
                            return Ok(Frame::new(
                                true,
                                self.opcode,
                                None,
                                self.fragments.take().unwrap().take_buffer().into(),
                            ));
                        }
                    }
                },
                _ => return Ok(frame),
            }
        }
    }

    /// See `WebSocket::write_frame`.
    pub(crate) async fn write_frame(&mut self, frame: Frame<'f>) -> Result<(), WebSocketError>
    where
        S: AsyncReadExt + AsyncWriteExt + Unpin,
    {
        self.ws.write_frame(frame).await?;
        Ok(())
    }
}
