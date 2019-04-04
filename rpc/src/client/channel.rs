// Copyright 2018 Google LLC
//
// Use of this source code is governed by an MIT-style
// license that can be found in the LICENSE file or at
// https://opensource.org/licenses/MIT.

use crate::{
    context,
    util::{deadline_compat, AsDuration, Compact},
    ClientMessage, ClientMessageKind, PollIo, Request, Response, Transport,
};
use fnv::FnvHashMap;
use futures::{
    channel::{mpsc, oneshot},
    prelude::*,
    ready,
    stream::Fuse,
    task::Waker,
    Poll,
};
use humantime::format_rfc3339;
use log::{debug, error, info, trace};
use pin_utils::{unsafe_pinned, unsafe_unpinned};
use std::{
    io,
    marker::{self, Unpin},
    net::SocketAddr,
    pin::Pin,
    sync::{
        atomic::{AtomicU64, Ordering},
        Arc,
    },
    time::Instant,
};
use trace::SpanId;

use super::Config;

/// Handles communication from the client to request dispatch.
#[derive(Debug)]
pub struct Channel<Req, Resp> {
    to_dispatch: mpsc::Sender<DispatchRequest<Req, Resp>>,
    /// Channel to send a cancel message to the dispatcher.
    cancellation: RequestCancellation,
    /// The ID to use for the next request to stage.
    next_request_id: Arc<AtomicU64>,
    server_addr: SocketAddr,
}

impl<Req, Resp> Clone for Channel<Req, Resp> {
    fn clone(&self) -> Self {
        Self {
            to_dispatch: self.to_dispatch.clone(),
            cancellation: self.cancellation.clone(),
            next_request_id: self.next_request_id.clone(),
            server_addr: self.server_addr,
        }
    }
}

/// A future returned by [`Channel::send`] that resolves to a server response.
#[derive(Debug)]
#[must_use = "futures do nothing unless polled"]
struct Send<'a, Req, Resp> {
    fut: MapOkDispatchResponse<SendMapErrConnectionReset<'a, Req, Resp>, Resp>,
}

type SendMapErrConnectionReset<'a, Req, Resp> =
    MapErrConnectionReset<futures::sink::Send<'a, mpsc::Sender<DispatchRequest<Req, Resp>>>>;

impl<'a, Req, Resp> Send<'a, Req, Resp> {
    unsafe_pinned!(
        fut: MapOkDispatchResponse<
            MapErrConnectionReset<
                futures::sink::Send<'a, mpsc::Sender<DispatchRequest<Req, Resp>>>,
            >,
            Resp,
        >
    );
}

impl<'a, Req, Resp> Future for Send<'a, Req, Resp> {
    type Output = io::Result<DispatchResponse<Resp>>;

    fn poll(mut self: Pin<&mut Self>, waker: &Waker) -> Poll<Self::Output> {
        self.as_mut().fut().poll(waker)
    }
}

/// A future returned by [`Channel::call`] that resolves to a server response.
#[derive(Debug)]
#[must_use = "futures do nothing unless polled"]
pub struct Call<'a, Req, Resp> {
    fut: AndThenIdent<Send<'a, Req, Resp>, DispatchResponse<Resp>>,
}

impl<'a, Req, Resp> Call<'a, Req, Resp> {
    unsafe_pinned!(fut: AndThenIdent<Send<'a, Req, Resp>, DispatchResponse<Resp>>);
}

impl<'a, Req, Resp> Future for Call<'a, Req, Resp> {
    type Output = io::Result<Resp>;

    fn poll(mut self: Pin<&mut Self>, waker: &Waker) -> Poll<Self::Output> {
        self.as_mut().fut().poll(waker)
    }
}

impl<Req, Resp> Channel<Req, Resp> {
    /// Sends a request to the dispatch task to forward to the server, returning a [`Future`] that
    /// resolves when the request is sent (not when the response is received).
    fn send(&mut self, mut ctx: context::Context, request: Req) -> Send<Req, Resp> {
        // Convert the context to the call context.
        ctx.trace_context.parent_id = Some(ctx.trace_context.span_id);
        ctx.trace_context.span_id = SpanId::random(&mut rand::thread_rng());

        let timeout = ctx.deadline.as_duration();
        let deadline = Instant::now() + timeout;
        trace!(
            "[{}/{}] Queuing request with deadline {} (timeout {:?}).",
            ctx.trace_id(),
            self.server_addr,
            format_rfc3339(ctx.deadline),
            timeout,
        );

        let (response_completion, response) = oneshot::channel();
        let cancellation = self.cancellation.clone();
        let request_id = self.next_request_id.fetch_add(1, Ordering::Relaxed);
        let server_addr = self.server_addr;
        Send {
            fut: MapOkDispatchResponse::new(
                MapErrConnectionReset::new(self.to_dispatch.send(DispatchRequest {
                    ctx,
                    request_id,
                    request,
                    response_completion,
                })),
                DispatchResponse {
                    response: deadline_compat::Deadline::new(response, deadline),
                    complete: false,
                    request_id,
                    cancellation,
                    ctx,
                    server_addr,
                },
            ),
        }
    }

    /// Sends a request to the dispatch task to forward to the server, returning a [`Future`] that
    /// resolves to the response.
    pub fn call(&mut self, context: context::Context, request: Req) -> Call<Req, Resp> {
        Call {
            fut: AndThenIdent::new(self.send(context, request)),
        }
    }
}

/// A server response that is completed by request dispatch when the corresponding response
/// arrives off the wire.
#[derive(Debug)]
struct DispatchResponse<Resp> {
    response: deadline_compat::Deadline<oneshot::Receiver<Response<Resp>>>,
    ctx: context::Context,
    complete: bool,
    cancellation: RequestCancellation,
    request_id: u64,
    server_addr: SocketAddr,
}

impl<Resp> DispatchResponse<Resp> {
    unsafe_pinned!(server_addr: SocketAddr);
    unsafe_pinned!(ctx: context::Context);
}

impl<Resp> Future for DispatchResponse<Resp> {
    type Output = io::Result<Resp>;

    fn poll(mut self: Pin<&mut Self>, waker: &Waker) -> Poll<io::Result<Resp>> {
        let resp = ready!(self.response.poll_unpin(waker));

        self.complete = true;

        Poll::Ready(match resp {
            Ok(resp) => Ok(resp.message?),
            Err(e) => Err({
                let trace_id = *self.as_mut().ctx().trace_id();
                let server_addr = *self.as_mut().server_addr();

                if e.is_elapsed() {
                    io::Error::new(
                        io::ErrorKind::TimedOut,
                        "Client dropped expired request.".to_string(),
                    )
                } else if e.is_timer() {
                    let e = e.into_timer().unwrap();
                    if e.is_at_capacity() {
                        io::Error::new(
                            io::ErrorKind::Other,
                            "Cancelling request because an expiration could not be set \
                             due to the timer being at capacity."
                                .to_string(),
                        )
                    } else if e.is_shutdown() {
                        panic!("[{}/{}] Timer was shutdown", trace_id, server_addr)
                    } else {
                        panic!(
                            "[{}/{}] Unrecognized timer error: {}",
                            trace_id, server_addr, e
                        )
                    }
                } else if e.is_inner() {
                    // The oneshot is Canceled when the dispatch task ends.
                    io::Error::from(io::ErrorKind::ConnectionReset)
                } else {
                    panic!(
                        "[{}/{}] Unrecognized deadline error: {}",
                        trace_id, server_addr, e
                    )
                }
            }),
        })
    }
}

// Cancels the request when dropped, if not already complete.
impl<Resp> Drop for DispatchResponse<Resp> {
    fn drop(&mut self) {
        if !self.complete {
            // The receiver needs to be closed to handle the edge case that the request has not
            // yet been received by the dispatch task. It is possible for the cancel message to
            // arrive before the request itself, in which case the request could get stuck in the
            // dispatch map forever if the server never responds (e.g. if the server dies while
            // responding). Even if the server does respond, it will have unnecessarily done work
            // for a client no longer waiting for a response. To avoid this, the dispatch task
            // checks if the receiver is closed before inserting the request in the map. By
            // closing the receiver before sending the cancel message, it is guaranteed that if the
            // dispatch task misses an early-arriving cancellation message, then it will see the
            // receiver as closed.
            self.response.get_mut().close();
            self.cancellation.cancel(self.request_id);
        }
    }
}

/// Spawns a dispatch task on the default executor that manages the lifecycle of requests initiated
/// by the returned [`Channel`].
pub async fn spawn<Req, Resp, C>(
    config: Config,
    transport: C,
    server_addr: SocketAddr,
) -> io::Result<Channel<Req, Resp>>
where
    Req: marker::Send + 'static,
    Resp: marker::Send + 'static,
    C: Transport<Item = Response<Resp>, SinkItem = ClientMessage<Req>> + marker::Send + 'static,
{
    let (to_dispatch, pending_requests) = mpsc::channel(config.pending_request_buffer);
    let (cancellation, canceled_requests) = cancellations();

    crate::spawn(
        RequestDispatch {
            config,
            server_addr,
            canceled_requests,
            transport: transport.fuse(),
            in_flight_requests: FnvHashMap::default(),
            pending_requests: pending_requests.fuse(),
        }
        .unwrap_or_else(move |e| error!("[{}] Connection broken: {}", server_addr, e)),
    )
    .map_err(|e| {
        io::Error::new(
            io::ErrorKind::Other,
            format!(
                "Could not spawn client dispatch task. Is shutdown: {}",
                e.is_shutdown()
            ),
        )
    })?;

    Ok(Channel {
        to_dispatch,
        cancellation,
        server_addr,
        next_request_id: Arc::new(AtomicU64::new(0)),
    })
}

/// Handles the lifecycle of requests, writing requests to the wire, managing cancellations,
/// and dispatching responses to the appropriate channel.
struct RequestDispatch<Req, Resp, C> {
    /// Writes requests to the wire and reads responses off the wire.
    transport: Fuse<C>,
    /// Requests waiting to be written to the wire.
    pending_requests: Fuse<mpsc::Receiver<DispatchRequest<Req, Resp>>>,
    /// Requests that were dropped.
    canceled_requests: CanceledRequests,
    /// Requests already written to the wire that haven't yet received responses.
    in_flight_requests: FnvHashMap<u64, InFlightData<Resp>>,
    /// Configures limits to prevent unlimited resource usage.
    config: Config,
    /// The address of the server connected to.
    server_addr: SocketAddr,
}

impl<Req, Resp, C> RequestDispatch<Req, Resp, C>
where
    Req: marker::Send,
    Resp: marker::Send,
    C: Transport<Item = Response<Resp>, SinkItem = ClientMessage<Req>>,
{
    unsafe_pinned!(server_addr: SocketAddr);
    unsafe_pinned!(in_flight_requests: FnvHashMap<u64, InFlightData<Resp>>);
    unsafe_pinned!(canceled_requests: CanceledRequests);
    unsafe_pinned!(pending_requests: Fuse<mpsc::Receiver<DispatchRequest<Req, Resp>>>);
    unsafe_pinned!(transport: Fuse<C>);

    fn pump_read(self: &mut Pin<&mut Self>, waker: &Waker) -> PollIo<()> {
        Poll::Ready(match ready!(self.as_mut().transport().poll_next(waker)?) {
            Some(response) => {
                self.complete(response);
                Some(Ok(()))
            }
            None => {
                trace!("[{}] read half closed", self.as_mut().server_addr());
                None
            }
        })
    }

    fn pump_write(self: &mut Pin<&mut Self>, waker: &Waker) -> PollIo<()> {
        enum ReceiverStatus {
            NotReady,
            Closed,
        }

        let pending_requests_status = match self.poll_next_request(waker)? {
            Poll::Ready(Some(dispatch_request)) => {
                self.write_request(dispatch_request)?;
                return Poll::Ready(Some(Ok(())));
            }
            Poll::Ready(None) => ReceiverStatus::Closed,
            Poll::Pending => ReceiverStatus::NotReady,
        };

        let canceled_requests_status = match self.poll_next_cancellation(waker)? {
            Poll::Ready(Some((context, request_id))) => {
                self.write_cancel(context, request_id)?;
                return Poll::Ready(Some(Ok(())));
            }
            Poll::Ready(None) => ReceiverStatus::Closed,
            Poll::Pending => ReceiverStatus::NotReady,
        };

        match (pending_requests_status, canceled_requests_status) {
            (ReceiverStatus::Closed, ReceiverStatus::Closed) => {
                ready!(self.as_mut().transport().poll_flush(waker)?);
                Poll::Ready(None)
            }
            (ReceiverStatus::NotReady, _) | (_, ReceiverStatus::NotReady) => {
                // No more messages to process, so flush any messages buffered in the transport.
                ready!(self.as_mut().transport().poll_flush(waker)?);

                // Even if we fully-flush, we return Pending, because we have no more requests
                // or cancellations right now.
                Poll::Pending
            }
        }
    }

    /// Yields the next pending request, if one is ready to be sent.
    fn poll_next_request(
        self: &mut Pin<&mut Self>,
        waker: &Waker,
    ) -> PollIo<DispatchRequest<Req, Resp>> {
        if self.as_mut().in_flight_requests().len() >= self.config.max_in_flight_requests {
            info!(
                "At in-flight request capacity ({}/{}).",
                self.as_mut().in_flight_requests().len(),
                self.config.max_in_flight_requests
            );

            // No need to schedule a wakeup, because timers and responses are responsible
            // for clearing out in-flight requests.
            return Poll::Pending;
        }

        while let Poll::Pending = self.as_mut().transport().poll_ready(waker)? {
            // We can't yield a request-to-be-sent before the transport is capable of buffering it.
            ready!(self.as_mut().transport().poll_flush(waker)?);
        }

        loop {
            match ready!(self.as_mut().pending_requests().poll_next_unpin(waker)) {
                Some(request) => {
                    if request.response_completion.is_canceled() {
                        trace!(
                            "[{}] Request canceled before being sent.",
                            request.ctx.trace_id()
                        );
                        continue;
                    }

                    return Poll::Ready(Some(Ok(request)));
                }
                None => {
                    trace!("[{}] pending_requests closed", self.as_mut().server_addr());
                    return Poll::Ready(None);
                }
            }
        }
    }

    /// Yields the next pending cancellation, and, if one is ready, cancels the associated request.
    fn poll_next_cancellation(
        self: &mut Pin<&mut Self>,
        waker: &Waker,
    ) -> PollIo<(context::Context, u64)> {
        while let Poll::Pending = self.as_mut().transport().poll_ready(waker)? {
            ready!(self.as_mut().transport().poll_flush(waker)?);
        }

        loop {
            match ready!(self.as_mut().canceled_requests().poll_next_unpin(waker)) {
                Some(request_id) => {
                    if let Some(in_flight_data) =
                        self.as_mut().in_flight_requests().remove(&request_id)
                    {
                        self.as_mut().in_flight_requests().compact(0.1);

                        debug!(
                            "[{}/{}] Removed request.",
                            in_flight_data.ctx.trace_id(),
                            self.as_mut().server_addr()
                        );

                        return Poll::Ready(Some(Ok((in_flight_data.ctx, request_id))));
                    }
                }
                None => {
                    trace!(
                        "[{}] canceled_requests closed.",
                        self.as_mut().server_addr()
                    );
                    return Poll::Ready(None);
                }
            }
        }
    }

    fn write_request(
        self: &mut Pin<&mut Self>,
        dispatch_request: DispatchRequest<Req, Resp>,
    ) -> io::Result<()> {
        let request_id = dispatch_request.request_id;
        let request = ClientMessage {
            trace_context: dispatch_request.ctx.trace_context,
            message: ClientMessageKind::Request(Request {
                id: request_id,
                message: dispatch_request.request,
                deadline: dispatch_request.ctx.deadline,
            }),
        };
        self.as_mut().transport().start_send(request)?;
        self.as_mut().in_flight_requests().insert(
            request_id,
            InFlightData {
                ctx: dispatch_request.ctx,
                response_completion: dispatch_request.response_completion,
            },
        );
        Ok(())
    }

    fn write_cancel(
        self: &mut Pin<&mut Self>,
        context: context::Context,
        request_id: u64,
    ) -> io::Result<()> {
        let trace_id = *context.trace_id();
        let cancel = ClientMessage {
            trace_context: context.trace_context,
            message: ClientMessageKind::Cancel { request_id },
        };
        self.as_mut().transport().start_send(cancel)?;
        trace!(
            "[{}/{}] Cancel message sent.",
            trace_id,
            self.as_mut().server_addr()
        );
        Ok(())
    }

    /// Sends a server response to the client task that initiated the associated request.
    fn complete(self: &mut Pin<&mut Self>, response: Response<Resp>) -> bool {
        if let Some(in_flight_data) = self
            .as_mut()
            .in_flight_requests()
            .remove(&response.request_id)
        {
            self.as_mut().in_flight_requests().compact(0.1);

            trace!(
                "[{}/{}] Received response.",
                in_flight_data.ctx.trace_id(),
                self.as_mut().server_addr()
            );
            let _ = in_flight_data.response_completion.send(response);
            return true;
        }

        debug!(
            "[{}] No in-flight request found for request_id = {}.",
            self.as_mut().server_addr(),
            response.request_id
        );

        // If the response completion was absent, then the request was already canceled.
        false
    }
}

impl<Req, Resp, C> Future for RequestDispatch<Req, Resp, C>
where
    Req: marker::Send,
    Resp: marker::Send,
    C: Transport<Item = Response<Resp>, SinkItem = ClientMessage<Req>>,
{
    type Output = io::Result<()>;

    fn poll(mut self: Pin<&mut Self>, waker: &Waker) -> Poll<io::Result<()>> {
        trace!("[{}] RequestDispatch::poll", self.as_mut().server_addr());
        loop {
            match (self.pump_read(waker)?, self.pump_write(waker)?) {
                (read, write @ Poll::Ready(None)) => {
                    if self.as_mut().in_flight_requests().is_empty() {
                        info!(
                            "[{}] Shutdown: write half closed, and no requests in flight.",
                            self.as_mut().server_addr()
                        );
                        return Poll::Ready(Ok(()));
                    }
                    match read {
                        Poll::Ready(Some(())) => continue,
                        _ => {
                            trace!(
                                "[{}] read: {:?}, write: {:?}, (not ready)",
                                self.as_mut().server_addr(),
                                read,
                                write,
                            );
                            return Poll::Pending;
                        }
                    }
                }
                (read @ Poll::Ready(Some(())), write) | (read, write @ Poll::Ready(Some(()))) => {
                    trace!(
                        "[{}] read: {:?}, write: {:?}",
                        self.as_mut().server_addr(),
                        read,
                        write,
                    )
                }
                (read, write) => {
                    trace!(
                        "[{}] read: {:?}, write: {:?} (not ready)",
                        self.as_mut().server_addr(),
                        read,
                        write,
                    );
                    return Poll::Pending;
                }
            }
        }
    }
}

/// A server-bound request sent from a [`Channel`] to request dispatch, which will then manage
/// the lifecycle of the request.
#[derive(Debug)]
struct DispatchRequest<Req, Resp> {
    ctx: context::Context,
    request_id: u64,
    request: Req,
    response_completion: oneshot::Sender<Response<Resp>>,
}

struct InFlightData<Resp> {
    ctx: context::Context,
    response_completion: oneshot::Sender<Response<Resp>>,
}

/// Sends request cancellation signals.
#[derive(Debug, Clone)]
struct RequestCancellation(mpsc::UnboundedSender<u64>);

/// A stream of IDs of requests that have been canceled.
#[derive(Debug)]
struct CanceledRequests(mpsc::UnboundedReceiver<u64>);

/// Returns a channel to send request cancellation messages.
fn cancellations() -> (RequestCancellation, CanceledRequests) {
    // Unbounded because messages are sent in the drop fn. This is fine, because it's still
    // bounded by the number of in-flight requests. Additionally, each request has a clone
    // of the sender, so the bounded channel would have the same behavior,
    // since it guarantees a slot.
    let (tx, rx) = mpsc::unbounded();
    (RequestCancellation(tx), CanceledRequests(rx))
}

impl RequestCancellation {
    /// Cancels the request with ID `request_id`.
    fn cancel(&mut self, request_id: u64) {
        let _ = self.0.unbounded_send(request_id);
    }
}

impl Stream for CanceledRequests {
    type Item = u64;

    fn poll_next(mut self: Pin<&mut Self>, waker: &Waker) -> Poll<Option<u64>> {
        self.0.poll_next_unpin(waker)
    }
}

#[derive(Debug)]
#[must_use = "futures do nothing unless polled"]
struct MapErrConnectionReset<Fut> {
    future: Fut,
    finished: Option<()>,
}

impl<Fut> MapErrConnectionReset<Fut> {
    unsafe_pinned!(future: Fut);
    unsafe_unpinned!(finished: Option<()>);

    fn new(future: Fut) -> MapErrConnectionReset<Fut> {
        MapErrConnectionReset {
            future,
            finished: Some(()),
        }
    }
}

impl<Fut: Unpin> Unpin for MapErrConnectionReset<Fut> {}

impl<Fut> Future for MapErrConnectionReset<Fut>
where
    Fut: TryFuture,
{
    type Output = io::Result<Fut::Ok>;

    fn poll(mut self: Pin<&mut Self>, waker: &Waker) -> Poll<Self::Output> {
        match self.as_mut().future().try_poll(waker) {
            Poll::Pending => Poll::Pending,
            Poll::Ready(result) => {
                self.finished().take().expect(
                    "MapErrConnectionReset must not be polled after it returned `Poll::Ready`",
                );
                Poll::Ready(result.map_err(|_| io::Error::from(io::ErrorKind::ConnectionReset)))
            }
        }
    }
}

#[derive(Debug)]
#[must_use = "futures do nothing unless polled"]
struct MapOkDispatchResponse<Fut, Resp> {
    future: Fut,
    response: Option<DispatchResponse<Resp>>,
}

impl<Fut, Resp> MapOkDispatchResponse<Fut, Resp> {
    unsafe_pinned!(future: Fut);
    unsafe_unpinned!(response: Option<DispatchResponse<Resp>>);

    fn new(future: Fut, response: DispatchResponse<Resp>) -> MapOkDispatchResponse<Fut, Resp> {
        MapOkDispatchResponse {
            future,
            response: Some(response),
        }
    }
}

impl<Fut: Unpin, Resp> Unpin for MapOkDispatchResponse<Fut, Resp> {}

impl<Fut, Resp> Future for MapOkDispatchResponse<Fut, Resp>
where
    Fut: TryFuture,
{
    type Output = Result<DispatchResponse<Resp>, Fut::Error>;

    fn poll(mut self: Pin<&mut Self>, waker: &Waker) -> Poll<Self::Output> {
        match self.as_mut().future().try_poll(waker) {
            Poll::Pending => Poll::Pending,
            Poll::Ready(result) => {
                let response = self
                    .as_mut()
                    .response()
                    .take()
                    .expect("MapOk must not be polled after it returned `Poll::Ready`");
                Poll::Ready(result.map(|_| response))
            }
        }
    }
}

#[derive(Debug)]
#[must_use = "futures do nothing unless polled"]
struct AndThenIdent<Fut1, Fut2> {
    try_chain: TryChain<Fut1, Fut2>,
}

impl<Fut1, Fut2> AndThenIdent<Fut1, Fut2>
where
    Fut1: TryFuture<Ok = Fut2>,
    Fut2: TryFuture,
{
    unsafe_pinned!(try_chain: TryChain<Fut1, Fut2>);

    /// Creates a new `Then`.
    fn new(future: Fut1) -> AndThenIdent<Fut1, Fut2> {
        AndThenIdent {
            try_chain: TryChain::new(future),
        }
    }
}

impl<Fut1, Fut2> Future for AndThenIdent<Fut1, Fut2>
where
    Fut1: TryFuture<Ok = Fut2>,
    Fut2: TryFuture<Error = Fut1::Error>,
{
    type Output = Result<Fut2::Ok, Fut2::Error>;

    fn poll(self: Pin<&mut Self>, waker: &Waker) -> Poll<Self::Output> {
        self.try_chain().poll(waker, |result| match result {
            Ok(ok) => TryChainAction::Future(ok),
            Err(err) => TryChainAction::Output(Err(err)),
        })
    }
}

#[must_use = "futures do nothing unless polled"]
#[derive(Debug)]
enum TryChain<Fut1, Fut2> {
    First(Fut1),
    Second(Fut2),
    Empty,
}

enum TryChainAction<Fut2>
where
    Fut2: TryFuture,
{
    Future(Fut2),
    Output(Result<Fut2::Ok, Fut2::Error>),
}

impl<Fut1, Fut2> TryChain<Fut1, Fut2>
where
    Fut1: TryFuture<Ok = Fut2>,
    Fut2: TryFuture,
{
    fn new(fut1: Fut1) -> TryChain<Fut1, Fut2> {
        TryChain::First(fut1)
    }

    fn poll<F>(self: Pin<&mut Self>, waker: &Waker, f: F) -> Poll<Result<Fut2::Ok, Fut2::Error>>
    where
        F: FnOnce(Result<Fut1::Ok, Fut1::Error>) -> TryChainAction<Fut2>,
    {
        let mut f = Some(f);

        // Safe to call `get_unchecked_mut` because we won't move the futures.
        let this = unsafe { Pin::get_unchecked_mut(self) };

        loop {
            let output = match this {
                TryChain::First(fut1) => {
                    // Poll the first future
                    match unsafe { Pin::new_unchecked(fut1) }.try_poll(waker) {
                        Poll::Pending => return Poll::Pending,
                        Poll::Ready(output) => output,
                    }
                }
                TryChain::Second(fut2) => {
                    // Poll the second future
                    return unsafe { Pin::new_unchecked(fut2) }.try_poll(waker);
                }
                TryChain::Empty => {
                    panic!("future must not be polled after it returned `Poll::Ready`");
                }
            };

            *this = TryChain::Empty; // Drop fut1
            let f = f.take().unwrap();
            match f(output) {
                TryChainAction::Future(fut2) => *this = TryChain::Second(fut2),
                TryChainAction::Output(output) => return Poll::Ready(output),
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::{CanceledRequests, Channel, RequestCancellation, RequestDispatch};
    use crate::{
        client::Config,
        context,
        transport::{self, channel::UnboundedChannel},
        ClientMessage, Response,
    };
    use fnv::FnvHashMap;
    use futures::{channel::mpsc, prelude::*, Poll};
    use futures_test::task::noop_waker_ref;
    use std::{
        marker,
        net::{IpAddr, Ipv4Addr, SocketAddr},
        pin::Pin,
        sync::atomic::AtomicU64,
        sync::Arc,
    };

    #[test]
    fn stage_request() {
        let (mut dispatch, mut channel, _server_channel) = set_up();

        // Test that a request future dropped before it's processed by dispatch will cause the request
        // to not be added to the in-flight request map.
        let _resp = tokio::runtime::current_thread::block_on_all(
            channel
                .send(context::current(), "hi".to_string())
                .boxed()
                .compat(),
        );

        let mut dispatch = Pin::new(&mut dispatch);
        let waker = &noop_waker_ref();

        let req = dispatch.poll_next_request(waker).ready();
        assert!(req.is_some());

        let req = req.unwrap();
        assert_eq!(req.request_id, 0);
        assert_eq!(req.request, "hi".to_string());
    }

    #[test]
    fn stage_request_response_future_dropped() {
        let (mut dispatch, mut channel, _server_channel) = set_up();

        // Test that a request future dropped before it's processed by dispatch will cause the request
        // to not be added to the in-flight request map.
        let resp = tokio::runtime::current_thread::block_on_all(
            channel
                .send(context::current(), "hi".into())
                .boxed()
                .compat(),
        )
        .unwrap();
        drop(resp);
        drop(channel);

        let mut dispatch = Pin::new(&mut dispatch);
        let waker = &noop_waker_ref();

        dispatch.poll_next_cancellation(waker).unwrap();
        assert!(dispatch.poll_next_request(waker).ready().is_none());
    }

    #[test]
    fn stage_request_response_future_closed() {
        let (mut dispatch, mut channel, _server_channel) = set_up();

        // Test that a request future that's closed its receiver but not yet canceled its request --
        // i.e. still in `drop fn` -- will cause the request to not be added to the in-flight request
        // map.
        let resp = tokio::runtime::current_thread::block_on_all(
            channel
                .send(context::current(), "hi".into())
                .boxed()
                .compat(),
        )
        .unwrap();
        drop(resp);
        drop(channel);

        let mut dispatch = Pin::new(&mut dispatch);
        let waker = &noop_waker_ref();
        assert!(dispatch.poll_next_request(waker).ready().is_none());
    }

    fn set_up() -> (
        RequestDispatch<String, String, UnboundedChannel<Response<String>, ClientMessage<String>>>,
        Channel<String, String>,
        UnboundedChannel<ClientMessage<String>, Response<String>>,
    ) {
        let _ = env_logger::try_init();

        let (to_dispatch, pending_requests) = mpsc::channel(1);
        let (cancel_tx, canceled_requests) = mpsc::unbounded();
        let (client_channel, server_channel) = transport::channel::unbounded();

        let dispatch = RequestDispatch::<String, String, _> {
            transport: client_channel.fuse(),
            pending_requests: pending_requests.fuse(),
            canceled_requests: CanceledRequests(canceled_requests),
            in_flight_requests: FnvHashMap::default(),
            config: Config::default(),
            server_addr: SocketAddr::new(IpAddr::V4(Ipv4Addr::LOCALHOST), 0),
        };

        let cancellation = RequestCancellation(cancel_tx);
        let channel = Channel {
            to_dispatch,
            cancellation,
            next_request_id: Arc::new(AtomicU64::new(0)),
            server_addr: SocketAddr::new(Ipv4Addr::UNSPECIFIED.into(), 0),
        };

        (dispatch, channel, server_channel)
    }

    trait PollTest {
        type T;
        fn unwrap(self) -> Poll<Self::T>;
        fn ready(self) -> Self::T;
    }

    impl<T, E> PollTest for Poll<Option<Result<T, E>>>
    where
        E: ::std::fmt::Display + marker::Send + 'static,
    {
        type T = Option<T>;

        fn unwrap(self) -> Poll<Option<T>> {
            match self {
                Poll::Ready(Some(Ok(t))) => Poll::Ready(Some(t)),
                Poll::Ready(None) => Poll::Ready(None),
                Poll::Ready(Some(Err(e))) => panic!(e.to_string()),
                Poll::Pending => Poll::Pending,
            }
        }

        fn ready(self) -> Option<T> {
            match self {
                Poll::Ready(Some(Ok(t))) => Some(t),
                Poll::Ready(None) => None,
                Poll::Ready(Some(Err(e))) => panic!(e.to_string()),
                Poll::Pending => panic!("Pending"),
            }
        }
    }

}