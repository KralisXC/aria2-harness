/* <!-- copyright */
/*
 * aria2 - The high speed download utility
 *
 * Copyright (C) 2026 Aria2-Harness
 *
 * This program is free software; you can redistribute it and/or modify
 * it under the terms of the GNU General Public License as published by
 * the Free Software Foundation; either version 2 of the License, or
 * (at your option) any later version.
 *
 * This program is distributed in the hope that it will be useful,
 * but WITHOUT ANY WARRANTY; without even the implied warranty of
 * MERCHANTABILITY or FITNESS FOR A PARTICULAR PURPOSE.  See the
 * GNU General Public License for more details.
 *
 * You should have received a copy of the GNU General Public License
 * along with this program; if not, write to the Free Software
 * Foundation, Inc., 51 Franklin Street, Fifth Floor, Boston, MA 02110-1301 USA
 */
/* copyright --> */
#include "Http2Multiplexer.h"

#include <algorithm>
#include <cstring>
#include <cstdlib>
#include <utility>

#include "Exception.h"
#include "fmt.h"
#include "Http2Stream.h"
#include "Http2StreamRecvBuffer.h"
#include "LogFactory.h"
#include "SocketCore.h"

namespace aria2 {

namespace {
// P-HTTP2-5 flow control sizing.
//
// The HTTP/2 defaults (64 KiB per stream, 64 KiB per connection) cap the whole
// transport at about one window per round trip.  Measured against the project's
// test mirror the RTT is ~300 ms while the very same TCP socket sustains over
// 5 MB/s under HTTP/1.1 (measured with curl), so the defaults pin this client
// to ~200 KB/s no matter what the link could carry: the peer sends 40-70 KiB
// and then waits a full RTT for the WINDOW_UPDATE which consume-driven
// crediting only produces in half-window batches.  These values hand the peer
// a bandwidth-delay-product sized budget instead (5 MB/s * 300 ms = ~1.5 MB).
//
// Sizing: 1 MiB per stream keeps the worst case staging bound at
// numStreams * 1 MiB (16 streams with --split=16 -> 16 MiB), and the 8 MiB
// connection window covers that aggregate budget, which is *not* affected by
// SETTINGS_INITIAL_WINDOW_SIZE and therefore needs an explicit lift.
const uint32_t kInitialStreamWindowSize = 1024 * 1024;
const int32_t kConnectionWindowUplift = 8 * 1024 * 1024 - 65535;
} // namespace

Http2Multiplexer::Http2Multiplexer() : session_(nullptr)
{
  nghttp2_session_callbacks* callbacks = nullptr;
  if (nghttp2_session_callbacks_new(&callbacks) != 0) {
    A2_LOG_ERROR("nghttp2_session_callbacks_new() failed");
    return;
  }
  nghttp2_session_callbacks_set_on_frame_recv_callback(callbacks,
                                                       onFrameRecvCallback);
  nghttp2_session_callbacks_set_on_data_chunk_recv_callback(
      callbacks, onDataChunkRecvCallback);
  nghttp2_session_callbacks_set_on_stream_close_callback(
      callbacks, onStreamCloseCallback);
  nghttp2_session_callbacks_set_on_header_callback(callbacks, onHeaderCallback);

  // Automatic WINDOW_UPDATE is disabled: the peer's send window is then
  // advanced only by the explicit nghttp2_session_consume() calls made in
  // pumpFlowControl(), i.e. only for bytes which have really been consumed by
  // the upper layer.  Without this, a slow consumer would let the staging area
  // grow without bound.
  nghttp2_option* option = nullptr;
  if (nghttp2_option_new(&option) == 0) {
    nghttp2_option_set_no_auto_window_update(option, 1);
    if (nghttp2_session_client_new2(&session_, callbacks, this, option) != 0) {
      session_ = nullptr;
    }
    nghttp2_option_del(option);
  }
  else {
    if (nghttp2_session_client_new(&session_, callbacks, this) != 0) {
      session_ = nullptr;
    }
  }

  if (session_) {
    // RFC 7540 section 3.5: the client connection preface (NGHTTP2_CLIENT_MAGIC)
    // MUST be followed by a SETTINGS frame.  nghttp2 does not submit it on the
    // application's behalf - it only emits the ACK for the peer's SETTINGS.
    // Without this our preface is magic + HEADERS only, which a strict server
    // is entitled to reject.
    nghttp2_settings_entry iv[] = {
        {NGHTTP2_SETTINGS_INITIAL_WINDOW_SIZE, kInitialStreamWindowSize}};
    nghttp2_submit_settings(session_, NGHTTP2_FLAG_NONE, iv, 1);

    // SETTINGS_INITIAL_WINDOW_SIZE only covers stream level windows; the
    // connection level window always starts at 64 KiB and has to be lifted
    // explicitly, otherwise the aggregate of all streams is still capped at
    // one 64 KiB window per round trip.
    nghttp2_submit_window_update(session_, NGHTTP2_FLAG_NONE, 0,
                                 kConnectionWindowUplift);
  }

  nghttp2_session_callbacks_del(callbacks);
}

Http2Multiplexer::~Http2Multiplexer()
{
  if (session_) {
    nghttp2_session_del(session_);
  }
}

void Http2Multiplexer::attachSocket(std::shared_ptr<SocketCore> socket)
{
  socket_ = std::move(socket);
}

void Http2Multiplexer::setWakeupCallback(WakeupCallback callback)
{
  wakeup_ = std::move(callback);
}

void Http2Multiplexer::setDetachCallback(DetachCallback callback)
{
  detach_ = std::move(callback);
}

void Http2Multiplexer::retire(uint32_t errorCode)
{
  if (retired_) {
    return;
  }
  retired_ = true;

  // 1. Drop the event-poll registration of the transport fd *while the fd is
  //    still valid*.  SelectEventPoll::socketEntries_ is keyed by the raw
  //    sock_t; closing the socket first would make deleteEvents() miss (the fd
  //    is already -1) and leave an entry behind which hands a readable event
  //    to the destroyed Http2MultiplexerCommand.
  if (detach_) {
    detach_();
  }

  // 2. Move the streams out of the maps first.  The consumers (Http2ResponseCommand /
  //    Http2DownloadCommand) hold a shared_ptr to their Http2Stream, so the
  //    objects survive the erase; what the erase guarantees is that no new DATA
  //    frame can ever be routed to them again.
  std::vector<std::shared_ptr<Http2Stream>> doomed;
  doomed.reserve(streams_.size());
  for (auto& kv : streams_) {
    doomed.push_back(kv.second);
  }
  streams_.clear();
  streamsByCuid_.clear();

  // 3. Close every stream *before* the wakeup so that the consumer observes
  //    terminated() == true.  That is what turns the parked DownloadCommand
  //    into a real action: its next executeInternal() drains what is left and
  //    then throws DL_RETRY_EX(EX_GOT_EOF) (or DL_ABORT_EX once the segment is
  //    already complete) instead of quietly disappearing from commands_.
  for (auto& stream : doomed) {
    if (!stream->terminated()) {
      stream->setClosed(errorCode);
    }
    auto* consumer = stream->getSocketRecvBuffer()->getConsumer();
    if (consumer) {
      consumer->setStatusActive();
    }
  }

  // 4. Nothing may be written any more; the socket is about to go away.
  pendingWrite_.clear();

  if (socket_ && socket_->isOpen()) {
    socket_->closeConnection();
  }
  A2_LOG_INFO("HTTP/2 multiplexer retired");
}

std::shared_ptr<Http2Stream> Http2Multiplexer::createStream(cuid_t cuid)
{
  if (!session_) {
    return nullptr;
  }
  // The placeholder socket has fd == -1 and is never registered with any
  // event poll: AbstractCommand::setReadCheckSocket() skips sockets which are
  // not open, so no invalid descriptor can ever reach select()/epoll.
  auto stream = std::make_shared<Http2Stream>(cuid,
                                             std::make_shared<SocketCore>());
  stream->getSocketRecvBuffer()->setOwner(stream);
  streamsByCuid_[cuid] = stream;
  return stream;
}

std::shared_ptr<Http2Stream>
Http2Multiplexer::openStream(cuid_t cuid, const Http2RequestHead& head)
{
  auto stream = createStream(cuid);
  if (!stream) {
    return nullptr;
  }
  int32_t streamId = stream->submitRequest(session_, head);
  if (streamId < 0) {
    A2_LOG_ERROR(fmt("nghttp2_submit_request() failed: %d", streamId));
    streamsByCuid_.erase(cuid);
    return nullptr;
  }
  streams_[streamId] = stream;
  A2_LOG_DEBUG(fmt("HTTP/2 stream %d opened", streamId));
  return stream;
}

Http2Multiplexer::PumpResult Http2Multiplexer::readFromSocket()
{
  if (!session_ || !socket_) {
    return PUMP_ERROR;
  }

  size_t len = READ_CHUNK_SIZE;
  try {
    socket_->readData(readBuf_, len);
  }
  catch (const Exception& e) {
    A2_LOG_ERROR(fmt("HTTP/2 transport read failed: %s", e.what()));
    return PUMP_ERROR;
  }

  if (len == 0) {
    if (socket_->wantRead() || socket_->wantWrite()) {
      return PUMP_WOULDBLOCK;
    }
    // Plain transport EOF.  Note that this does not by itself end the
    // individual streams: they are finished by on_stream_close / END_STREAM.
    return PUMP_EOF;
  }

  ssize_t rv = nghttp2_session_mem_recv(session_, readBuf_, len);
  if (rv < 0) {
    A2_LOG_ERROR(fmt("nghttp2_session_mem_recv() failed: %d",
                     static_cast<int>(rv)));
    return PUMP_ERROR;
  }

  pumpFlowControl();
  return PUMP_OK;
}

bool Http2Multiplexer::flushToSocket()
{
  if (!session_ || !socket_) {
    return false;
  }

  // Flow-control credit must be issued from the *consumer* side, not only after
  // a successful socket read.  Once the peer exhausts its send window it stops
  // delivering bytes, so readFromSocket() never runs again and the credit for
  // the last chunk the upper layer consumed would never be sent - deadlocking
  // that stream forever.  flushToSocket() runs once per event-loop iteration,
  // so it is the correct place to top the window up.  The call is idempotent:
  // takeConsumedBytes() returns 0 the second time around.
  pumpFlowControl();

  try {
    for (;;) {
      const uint8_t* data = nullptr;
      ssize_t n = nghttp2_session_mem_send(session_, &data);
      if (n < 0) {
        A2_LOG_ERROR(fmt("nghttp2_session_mem_send() failed: %d",
                         static_cast<int>(n)));
        return false;
      }
      if (n == 0) {
        break;
      }
      // The pointer returned by nghttp2_session_mem_send() is only valid
      // until the next call, so copy into our own pending buffer before
      // touching the socket: a partially completed write must not lose bytes.
      pendingWrite_.append(reinterpret_cast<const char*>(data),
                           static_cast<size_t>(n));
    }
    return flushPending();
  }
  catch (const Exception& e) {
    A2_LOG_ERROR(fmt("HTTP/2 transport write failed: %s", e.what()));
    return false;
  }
}

bool Http2Multiplexer::flushPending()
{
  while (!pendingWrite_.empty()) {
    ssize_t w = socket_->writeData(pendingWrite_.data(), pendingWrite_.size());
    if (w <= 0) {
      // Would block (wantWrite() is set) or nothing was accepted.
      return false;
    }
    pendingWrite_.erase(0, static_cast<size_t>(w));
  }
  return true;
}

bool Http2Multiplexer::wantRead() const
{
  return session_ && nghttp2_session_want_read(session_) != 0;
}

bool Http2Multiplexer::wantWrite() const
{
  return session_ && nghttp2_session_want_write(session_) != 0;
}

bool Http2Multiplexer::sessionTerminated() const
{
  return session_ && !nghttp2_session_want_read(session_) &&
         !nghttp2_session_want_write(session_);
}

std::shared_ptr<Http2Stream> Http2Multiplexer::getStream(int32_t streamId) const
{
  auto it = streams_.find(streamId);
  if (it == streams_.end()) {
    return nullptr;
  }
  return it->second;
}

std::shared_ptr<Http2Stream>
Http2Multiplexer::getStreamByCuid(cuid_t cuid) const
{
  auto it = streamsByCuid_.find(cuid);
  if (it == streamsByCuid_.end()) {
    return nullptr;
  }
  return it->second;
}

void Http2Multiplexer::notifyWakeup(cuid_t cuid)
{
  // Activate exactly the command which consumes this stream.  The command is
  // parked with STATUS_INACTIVE after every execution (Command::transitStatus()
  // in DownloadEngine's executeCommand()), so without this wakeup it would
  // only ever be revisited by the 1 second refresh tick.
  auto it = streamsByCuid_.find(cuid);
  if (it != streamsByCuid_.end()) {
    auto* consumer = it->second->getSocketRecvBuffer()->getConsumer();
    if (consumer) {
      consumer->setStatusActive();
    }
  }

  // Engine side hook: setNoWait(true) so that the very next iteration does not
  // block for up to refreshInterval before running the woken command.
  if (wakeup_) {
    wakeup_(cuid);
  }
}

void Http2Multiplexer::terminateAllStreams(uint32_t errorCode)
{
  for (auto& kv : streams_) {
    if (kv.second->terminated()) {
      continue;
    }
    kv.second->setClosed(errorCode);
    notifyWakeup(kv.second->getCuid());
  }
}

void Http2Multiplexer::purgeFinishedStreams()
{
  if (retired_ || !session_) {
    return;
  }

  // A stream is reclaimable as soon as no command consumes it any more.  The
  // consumer pointer is cleared (identity checked) by the destructor of the
  // command which owned the stream, so while it is non-null that command is
  // alive and may still drain the buffer or run its retry path - the stream
  // must then stay reachable through getStream()/getStreamByCuid().  There is
  // no window in which a live stream has no consumer: Http2RequestCommand
  // registers the Http2ResponseCommand as consumer in the very same
  // executeInternal() call which submits the HEADERS frame, and every later
  // hand-off constructs the successor before destroying the predecessor.
  //
  // Collect first, erase afterwards: erasing inside the range-for would
  // invalidate the iterator in use.
  std::vector<std::shared_ptr<Http2Stream>> doomed;
  for (auto& kv : streams_) {
    if (kv.second->getSocketRecvBuffer()->getConsumer() == nullptr) {
      doomed.push_back(kv.second);
    }
  }

  for (auto& stream : doomed) {
    auto streamId = stream->getStreamId();
    auto it = streams_.find(streamId);
    if (it == streams_.end()) {
      continue;
    }

    // Every received byte was charged against the *shared* connection level
    // window, whether or not the upper layer ever drained it.  A stream which
    // reaches this point is either already closed (nghttp2_session_consume()
    // would find no stream and silently drop the credit) or is about to be
    // reset, in which case its stream level window is meaningless too.  The
    // credit therefore has to go to the connection level explicitly, and it
    // has to cover the *undrained* remainder as well: an abandoned body which
    // is thrown away without giving its window back shrinks the transport for
    // every other stream until the whole connection stalls.  This is the HTTP/2
    // equivalent of aria2 closing a socket whose HTTP/1.1 response body was
    // only partially consumed.
    const bool abandoned = !stream->terminated();
    size_t uncredited = stream->takeReceivedButUncredited();
    if (uncredited > 0) {
      nghttp2_session_consume_connection(session_, uncredited);
    }

    if (abandoned) {
      // Without the reset the peer keeps filling a stream nobody reads any
      // more: the residue stays in memory and the request group's retry (which
      // already opened its own stream) would compete for the window with a
      // corpse.  NGHTTP2_CANCEL is the code for "no longer needed".
      nghttp2_submit_rst_stream(session_, NGHTTP2_FLAG_NONE, streamId,
                                NGHTTP2_CANCEL);
    }

    streams_.erase(it);
    // Identity checked: prepareForRetry() rebuilds the connection under the
    // *same* cuid, so streamsByCuid_ may already point at a fresh stream by the
    // time the old one is reclaimed.  Comparing the objects prevents erasing
    // the new registration.
    auto byCuid = streamsByCuid_.find(stream->getCuid());
    if (byCuid != streamsByCuid_.end() && byCuid->second == stream) {
      streamsByCuid_.erase(byCuid);
    }

    A2_LOG_INFO(fmt("HTTP/2 stream %d %s (no consumer), credited=%zu bytes,"
                    " remaining=%zu",
                    streamId, abandoned ? "abandoned and reset" : "reclaimed",
                    uncredited, streams_.size()));
  }
}

void Http2Multiplexer::pumpFlowControl()
{
  for (auto& kv : streams_) {
    size_t consumed = kv.second->takeConsumedBytes();
    if (consumed > 0) {
      // nghttp2_session_consume() credits *both* the stream and the
      // connection level windows.  Use the stream-only variant and the
      // connection window stays at zero after 65535 bytes overall, which
      // stalls every stream on the connection.
      nghttp2_session_consume(session_, kv.first, consumed);
    }
  }
}

int Http2Multiplexer::onFrameRecvCallback(nghttp2_session* session,
                                          const nghttp2_frame* frame,
                                          void* userData)
{
  auto* mux = static_cast<Http2Multiplexer*>(userData);

  if (frame->hd.type == NGHTTP2_DATA || frame->hd.type == NGHTTP2_HEADERS) {
    // END_STREAM is one of the two authoritative end-of-stream signals.
    if (frame->hd.flags & NGHTTP2_FLAG_END_STREAM) {
      auto stream = mux->getStream(frame->hd.stream_id);
      if (stream) {
        stream->setEndStreamReceived();
        mux->notifyWakeup(stream->getCuid());
      }
    }
  }

  if (frame->hd.type == NGHTTP2_HEADERS &&
      (frame->hd.flags & NGHTTP2_FLAG_END_HEADERS) &&
      frame->headers.cat != NGHTTP2_HCAT_HEADERS) {
    // The final response header block is complete.  NGHTTP2_HCAT_HEADERS
    // covers 1xx informational responses and trailers, neither of which may
    // be mistaken for the real response.
    auto stream = mux->getStream(frame->hd.stream_id);
    if (stream) {
      stream->setHeadersComplete();
      mux->notifyWakeup(stream->getCuid());
    }
  }
  return 0;
}

int Http2Multiplexer::onDataChunkRecvCallback(nghttp2_session* session,
                                              uint8_t flags, int32_t streamId,
                                              const uint8_t* data, size_t len,
                                              void* userData)
{
  auto* mux = static_cast<Http2Multiplexer*>(userData);
  auto stream = mux->getStream(streamId);
  if (stream) {
    // Never return non-zero from here: nghttp2 treats it as a fatal error and
    // there is no way to push back.  Oversized DATA frames are staged inside
    // the stream instead.
    stream->pushData(data, len);
    mux->notifyWakeup(stream->getCuid());
  }
  return 0;
}

int Http2Multiplexer::onStreamCloseCallback(nghttp2_session* session,
                                            int32_t streamId,
                                            uint32_t errorCode, void* userData)
{
  auto* mux = static_cast<Http2Multiplexer*>(userData);
  auto stream = mux->getStream(streamId);
  if (stream) {
    // The second authoritative end-of-stream signal; also covers RST_STREAM.
    stream->setClosed(errorCode);
    mux->notifyWakeup(stream->getCuid());
  }
  return 0;
}

int Http2Multiplexer::onHeaderCallback(nghttp2_session* session,
                                       const nghttp2_frame* frame,
                                       const uint8_t* name, size_t namelen,
                                       const uint8_t* value, size_t valuelen,
                                       uint8_t flags, void* userData)
{
  auto* mux = static_cast<Http2Multiplexer*>(userData);
  auto stream = mux->getStream(frame->hd.stream_id);
  if (stream) {
    // Keep the whole header block (including the ":status" pseudo header) so
    // that the response command can look up content-length, content-type and
    // friends exactly like the HTTP/1.1 path does through HttpHeader.
    std::string n(reinterpret_cast<const char*>(name), namelen);
    std::string v(reinterpret_cast<const char*>(value), valuelen);
    stream->addHeader(n, v);
    if (n == ":status") {
      stream->setStatusCode(atoi(v.c_str()));
    }
  }
  return 0;
}

} // namespace aria2
