use std::pin::Pin;
use std::sync::Arc;
use std::task::Poll;
use std::time::Instant;

use bytes::Buf;
use bytes::BufMut;
use bytes::BytesMut;
use chrono::Duration;
use tokio::io::ReadBuf;
use tokio::io::{AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt};

use super::pause::PauseSpec;
use super::pause::PauseStream;
use super::runner::Runner;
use super::Context;
use crate::Http1Error;
use crate::Http1PlanOutput;
use crate::Http1RequestOutput;
use crate::WithPlannedCapacity;
use crate::{Error, Http1Output, Http1Response, Output};

#[derive(Debug)]
pub(super) struct Http1Runner {
    out: Http1Output,
    state: State,
    req_header_start_time: Option<Instant>,
    req_body_start_time: Option<Instant>,
    req_end_time: Option<Instant>,
    resp_start_time: Option<Instant>,
    resp_header_end_time: Option<Instant>,
    first_read: Option<Instant>,
    end_time: Option<Instant>,
    resp_header_buf: BytesMut,
    req_body_buf: Vec<u8>,
    resp_body_buf: Vec<u8>,
}

#[derive(Debug)]
enum State {
    Pending {
        ctx: Arc<Context>,
        header: BytesMut,
        transport: Runner,
    },
    StartFailed {
        transport: Runner,
    },
    SendingHeader {
        start_time: Instant,
        transport: PauseStream<Runner>,
    },
    SendingBody {
        start_time: Instant,
        transport: PauseStream<Runner>,
    },
    ReceivingHeader {
        start_time: Instant,
        transport: PauseStream<Runner>,
    },
    ReceivingBody {
        start_time: Instant,
        transport: PauseStream<Runner>,
    },
    Complete {
        transport: Runner,
    },
    Invalid,
}

impl AsyncRead for Http1Runner {
    fn poll_read(
        mut self: std::pin::Pin<&mut Self>,
        cx: &mut std::task::Context<'_>,
        buf: &mut tokio::io::ReadBuf<'_>,
    ) -> Poll<std::io::Result<()>> {
        // If we've already read the header then read and record body bytes.
        let mut state = scopeguard::guard(
            (
                std::mem::replace(&mut self.state, State::Invalid),
                &mut self,
            ),
            |(state, mutself)| mutself.state = state,
        );
        match &mut *state {
            (State::ReceivingHeader { transport, .. }, mutself) => {
                let old_len = buf.filled().len();
                let poll = Pin::new(transport).poll_read(cx, buf);
                mutself
                    .resp_body_buf
                    .extend_from_slice(&buf.filled()[old_len..]);
                return poll;
            }
            (
                State::ReceivingBody {
                    ref mut transport, ..
                },
                mutself,
            ) => {
                // Record the response start time if this is our first read poll and we didn't explicitly
                // start it in execute (running as a transport).
                if mutself.resp_start_time.is_none() {
                    mutself.resp_start_time = Some(Instant::now());
                }

                // Don't read in more bytes at a time than we could fit in buf if there's extra after
                // reading the header.
                // TODO: optimize this to avoid the intermediate allocation and write.
                let mut header_vec = vec![0; buf.remaining() + 1];
                loop {
                    let mut header_buf = ReadBuf::new(header_vec.as_mut());
                    let poll = Pin::new(&mut *transport).poll_read(cx, &mut header_buf);
                    mutself.resp_header_buf.put_slice(header_buf.filled());
                    match poll {
                        Poll::Pending => return Poll::Pending,
                        Poll::Ready(Err(e)) => return Poll::Ready(Err(e)),
                        // If no data was read then the stream has ended.
                        Poll::Ready(Ok(())) => {
                            if header_buf.filled().len() == 0 {
                                return Poll::Ready(Err(std::io::Error::new(
                                    std::io::ErrorKind::UnexpectedEof,
                                    "header incomplete".to_owned(),
                                )));
                            }
                        }
                    }
                    // Data was read - try to process it.
                    if mutself.first_read.is_none() {
                        mutself.first_read = Some(Instant::now());
                    }
                    match mutself.receive_header() {
                        // Not enough data, let's read some more.
                        Poll::Pending => {}
                        // The full header was read, read the leftover bytes as part of the body.
                        Poll::Ready(Ok(remaining)) => {
                            mutself.resp_header_end_time = Some(Instant::now());
                            mutself.resp_body_buf.extend_from_slice(&remaining);
                            buf.put(remaining);
                            return Poll::Ready(Ok(()));
                        }
                        Poll::Ready(Err(e)) => return Poll::Ready(Err(e)),
                    }
                }
            }
            _ => panic!(),
        }
    }
}

impl AsyncWrite for Http1Runner {
    fn poll_write(
        mut self: std::pin::Pin<&mut Self>,
        cx: &mut std::task::Context<'_>,
        buf: &[u8],
    ) -> Poll<Result<usize, std::io::Error>> {
        let (State::SendingHeader { transport, .. } | State::SendingBody { transport, .. }) =
            &mut self.state
        else {
            panic!();
        };
        let poll = Pin::new(transport).poll_write(cx, buf);
        if poll.is_ready() {
            if self.req_body_start_time.is_none() {
                self.req_body_start_time = Some(Instant::now());
            }
            self.get_mut().req_body_buf.extend_from_slice(&buf);
        }
        poll
    }

    fn poll_flush(
        mut self: std::pin::Pin<&mut Self>,
        cx: &mut std::task::Context<'_>,
    ) -> std::task::Poll<Result<(), std::io::Error>> {
        let (State::SendingHeader { transport, .. } | State::SendingBody { transport, .. }) =
            &mut self.state
        else {
            panic!();
        };
        Pin::new(transport).poll_flush(cx)
    }

    fn poll_shutdown(
        mut self: std::pin::Pin<&mut Self>,
        cx: &mut std::task::Context<'_>,
    ) -> std::task::Poll<Result<(), std::io::Error>> {
        let (State::SendingHeader { transport, .. } | State::SendingBody { transport, .. }) =
            &mut self.state
        else {
            panic!();
        };
        let poll = Pin::new(transport).poll_shutdown(cx);
        if poll.is_ready() {
            self.end_time = Some(Instant::now());
        }
        poll
    }
}

impl Http1Runner {
    pub(super) fn new(ctx: Arc<Context>, transport: Runner, plan: Http1PlanOutput) -> Self {
        Self {
            state: State::Pending {
                ctx,
                header: Self::compute_header(&plan),
                transport,
            },
            out: Http1Output {
                request: None,
                response: None,
                error: None,
                duration: Duration::zero(),
                pause: crate::Http1PauseOutput::with_planned_capacity(&plan.pause),
                plan,
            },
            req_header_start_time: None,
            req_body_start_time: None,
            req_end_time: None,
            resp_start_time: None,
            resp_header_end_time: None,
            first_read: None,
            end_time: None,
            resp_header_buf: BytesMut::new(),
            req_body_buf: Vec::new(),
            resp_body_buf: Vec::new(),
        }
    }

    #[inline]
    fn compute_header(plan: &Http1PlanOutput) -> BytesMut {
        // Build a buffer with the header contents to avoid the overhead of separate writes.
        // TODO: We may actually want to split packets based on info at the HTTP layer, that logic
        // will go here once I figure out the right configuration to express it.
        let mut buf = BytesMut::with_capacity(
            plan.method.as_ref().map(Vec::len).unwrap_or(0)
                + 1
                + plan.url.path().len()
                + plan.url.query().map(|x| x.len() + 1).unwrap_or(0)
                + 1
                + plan.version_string.as_ref().map(Vec::len).unwrap_or(0)
                + 2
                + plan
                    .headers
                    .iter()
                    .fold(0, |sum, (k, v)| sum + k.len() + 2 + v.len() + 2)
                + 2
                + plan.body.len(),
        );
        if let Some(m) = &plan.method {
            buf.put_slice(m);
        }
        buf.put_u8(b' ');
        buf.put_slice(plan.url.path().as_bytes());
        if let Some(q) = plan.url.query() {
            buf.put_u8(b'?');
            buf.put_slice(q.as_bytes());
        }
        buf.put_u8(b' ');
        if let Some(p) = &plan.version_string {
            buf.put_slice(p);
        }
        buf.put(b"\r\n".as_slice());
        for (k, v) in &plan.headers {
            buf.put_slice(k.as_slice());
            buf.put_slice(b": ");
            buf.put_slice(v.as_slice());
            buf.put_slice(b"\r\n");
        }
        buf.put(b"\r\n".as_slice());
        buf
    }

    #[inline]
    fn receive_header(&mut self) -> Poll<std::io::Result<BytesMut>> {
        // TODO: Write our own extra-permissive parser.
        let mut headers = [httparse::EMPTY_HEADER; 16];
        let mut resp = httparse::Response::new(&mut headers);
        match resp.parse(&self.resp_header_buf) {
            Ok(result) => {
                let header_complete_time = Instant::now();
                // Set the header fields in our response.
                self.out.response = Some(Http1Response {
                    protocol: resp.version.map(|v| format!("HTTP/1.{}", v).into()),
                    status_code: resp.code,
                    // If the reason hasn't been read yet then also no headers were parsed.
                    headers: resp.reason.as_ref().map(|_| {
                        resp.headers
                            .into_iter()
                            .map(|h| (Vec::from(h.name), Vec::from(h.value)))
                            .collect()
                    }),
                    status_reason: resp.reason.map(Vec::from),
                    body: None,
                    duration: Duration::zero(),
                    header_duration: None,
                    time_to_first_byte: self
                        .first_read
                        .map(|first_read| {
                            first_read
                                - self.resp_start_time.expect(
                                    "response start time should be set before header is processed",
                                )
                        })
                        .map(Duration::from_std)
                        .transpose()
                        .unwrap(),
                });
                match result {
                    httparse::Status::Partial => Poll::Pending,
                    httparse::Status::Complete(body_start) => {
                        let state = std::mem::replace(&mut self.state, State::Invalid);
                        let State::ReceivingHeader {
                            start_time,
                            mut transport,
                        } = state
                        else {
                            panic!("header recieved in incorrect state: {:?}", self.state);
                        };
                        transport.reset(
                            std::iter::empty(),
                            vec![PauseSpec {
                                plan: self.out.plan.pause.response_body.start.clone(),
                                group_offset: 0,
                            }],
                        );
                        self.state = State::ReceivingBody {
                            start_time,
                            transport,
                        };
                        self.out.response.as_mut().unwrap().header_duration =
                            Some(Duration::from_std(header_complete_time - start_time).unwrap());
                        // Return the bytes we didn't read.
                        self.resp_header_buf.advance(body_start);
                        Poll::Ready(Ok(std::mem::take(&mut self.resp_header_buf)))
                    }
                }
            }
            Err(e) => {
                return Poll::Ready(Err(std::io::Error::new(
                    std::io::ErrorKind::Other,
                    Error(e.to_string()),
                )))
            }
        }
    }

    pub async fn start(
        &mut self,
        size_hint: Option<usize>,
    ) -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
        let state = std::mem::replace(&mut self.state, State::Invalid);
        let State::Pending {
            mut header,
            mut transport,
            ctx,
        } = state
        else {
            return Err(Box::new(Error(
                "attempt to start Http1Runner from invalid state".to_owned(),
            )));
        };

        if let Err(e) = transport
            .start(Some(header.len() + size_hint.unwrap_or(0)))
            .await
        {
            self.out.error = Some(Http1Error {
                kind: "transport start".to_owned(),
                message: e.to_string(),
            });
            self.state = State::StartFailed { transport };
            self.complete();
            return Err(e);
        };

        self.state = State::SendingHeader {
            start_time: Instant::now(),
            transport: PauseStream::new(
                ctx,
                transport,
                vec![
                    PauseSpec {
                        plan: self.out.plan.pause.request_headers.start.clone(),
                        group_offset: 0,
                    },
                    PauseSpec {
                        plan: self.out.plan.pause.request_headers.end.clone(),
                        group_offset: header.len().try_into().unwrap(),
                    },
                ],
                std::iter::empty(),
            ),
        };

        self.req_header_start_time = Some(Instant::now());
        self.write_all_buf(&mut header).await?;

        let state = std::mem::replace(&mut self.state, State::Invalid);
        let State::SendingHeader {
            start_time,
            mut transport,
        } = state
        else {
            panic!("invalid state after HTTP/1 header write");
        };

        let (_, mut writes) = transport.reset(
            if let Some(size_hint) = size_hint {
                vec![
                    PauseSpec {
                        plan: self.out.plan.pause.request_body.start.clone(),
                        group_offset: 0,
                    },
                    PauseSpec {
                        plan: self.out.plan.pause.request_body.end.clone(),
                        group_offset: size_hint.try_into().unwrap(),
                    },
                ]
            } else {
                if !self.out.plan.pause.request_body.end.is_empty() {
                    return Err(Box::new(Error(
                        "http1.pause.receive_body.end is unsupported in this request".to_owned(),
                    )));
                }
                vec![PauseSpec {
                    plan: self.out.plan.pause.request_body.start.clone(),
                    group_offset: 0,
                }]
            },
            std::iter::empty(),
        );

        if let Some(p) = writes.pop() {
            self.out.pause.request_headers.end = p;
        }
        if let Some(p) = writes.pop() {
            self.out.pause.request_headers.start = p;
        }

        self.state = State::SendingBody {
            start_time,
            transport,
        };

        self.out.request = Some(Http1RequestOutput {
            url: self.out.plan.url.clone(),
            headers: self.out.plan.headers.clone(),
            method: self.out.plan.method.clone(),
            version_string: self.out.plan.version_string.clone(),
            body: Vec::new(),
            duration: Duration::zero(),
            body_duration: None,
            time_to_first_byte: None,
        });
        Ok(())
    }

    pub async fn execute(&mut self) {
        // Send headers.
        if let Err(e) = self.start(Some(self.out.plan.body.len())).await {
            self.out.error = Some(Http1Error {
                kind: "send headers".to_owned(),
                message: e.to_string(),
            });
            return;
        }

        if !self.out.plan.body.is_empty() {
            let body = std::mem::take(&mut self.out.plan.body);
            if let Err(e) = self.write_all(body.as_slice()).await {
                self.out.error = Some(Http1Error {
                    kind: e.kind().to_string(),
                    message: e.to_string(),
                });
                return;
            }
            self.out.plan.body = body;
        }
        if let Err(e) = self.flush().await {
            self.out.error = Some(Http1Error {
                kind: e.kind().to_string(),
                message: e.to_string(),
            });
            return;
        }
        self.resp_start_time = Some(Instant::now());
        let mut response = Vec::new();
        if let Err(e) = self.read_to_end(&mut response).await {
            self.out.error = Some(Http1Error {
                kind: e.kind().to_string(),
                message: e.to_string(),
            });
            return;
        }
    }

    pub fn finish(mut self) -> (Output, Runner) {
        self.complete();
        let State::Complete { transport } = self.state else {
            unreachable!();
        };
        (Output::Http1(self.out), transport)
    }

    fn complete(&mut self) {
        let state = std::mem::replace(&mut self.state, State::Invalid);
        let (start_time, transport) = match state {
            State::SendingHeader {
                start_time,
                transport,
            }
            | State::SendingBody {
                start_time,
                transport,
            }
            | State::ReceivingHeader {
                start_time,
                transport,
            }
            | State::ReceivingBody {
                start_time,
                transport,
            } => (start_time, transport),
            State::Complete { transport }
            | State::Pending { transport, .. }
            | State::StartFailed { transport } => {
                self.state = State::Complete { transport };
                return;
            }
            State::Invalid => panic!(),
        };
        let end_time = self.end_time.unwrap_or_else(Instant::now);

        if let Some(req) = &mut self.out.request {
            req.duration =
                Duration::from_std(self.req_end_time.unwrap_or(end_time) - start_time).unwrap();
            req.body_duration = self
                .req_body_start_time
                .map(|start| self.resp_start_time.unwrap_or(end_time) - start)
                .map(Duration::from_std)
                .transpose()
                .unwrap();
            req.time_to_first_byte = self
                .req_header_start_time
                .map(|header_start| header_start - start_time)
                .map(Duration::from_std)
                .transpose()
                .unwrap();
            req.body = self.req_body_buf.to_vec();
        }

        // The response should be set if the header has been read.
        if let Some(resp) = &mut self.out.response {
            resp.body = Some(self.resp_body_buf.to_vec());
            resp.duration = Duration::from_std(
                end_time
                    - self
                        .resp_start_time
                        .expect("response start time should be recorded when response is set"),
            )
            .unwrap();
            resp.header_duration = self
                .resp_header_end_time
                .map(|end| end - self.resp_start_time.expect("response start time should be set if the response header has been received"))
                .map(Duration::from_std)
                .transpose()
                .unwrap();
        }

        self.state = State::Complete { transport };
        self.out.duration = Duration::from_std(end_time - start_time).unwrap();
    }
}
