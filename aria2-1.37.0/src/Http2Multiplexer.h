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
#ifndef D_HTTP2_MULTIPLEXER_H
#define D_HTTP2_MULTIPLEXER_H

#include "common.h"

#include <cstdint>
#include <functional>
#include <map>
#include <memory>
#include <string>

#include <nghttp2/nghttp2.h>

#include "a2functional.h"
#include "Command.h" // cuid_t

namespace aria2 {

class SocketCore;
class Http2Stream;
struct Http2RequestHead;

// Transport-level HTTP/2 multiplexer.
//
// Exactly one real transport socket (TLS + ALPN "h2") is owned by this
// object and it is the only reader/writer of that socket.  Each aria2
// connection ("cuid") is mapped onto one HTTP/2 stream; the streams get a
// placeholder SocketCore (fd == -1) and a per-stream SocketRecvBuffer.
//
// Deliberately not a Command: it is a plain transport object driven either
// by a probe program or by the engine's command loop, so the Command state
// machine stays untouched.
class Http2Multiplexer {
public:
  // Invoked after DATA / END_STREAM / close have been injected into a
  // stream.  The owner (engine side) uses it to activate the corresponding
  // command again.
  using WakeupCallback = std::function<void(cuid_t)>;

  // Invoked by retire() *before* the transport socket is closed.  The owner
  // (Http2MultiplexerCommand) uses it to drop the event-poll registration of
  // the transport fd.  Ordering is mandatory: SelectEventPoll keys its entries
  // by the raw sock_t, so closing the fd first would leave the stale entry
  // behind holding a dangling Command* (see project red line "常驻命令必须在
  // 注销后再销毁").
  using DetachCallback = std::function<void()>;

  enum PumpResult {
    PUMP_OK,       // bytes were fed into nghttp2
    PUMP_WOULDBLOCK, // no bytes available right now, retry later
    PUMP_EOF,      // transport reached EOF
    PUMP_ERROR,    // unrecoverable failure
  };

  Http2Multiplexer();
  ~Http2Multiplexer();

  // Attaches the already connected and TLS-handshaked transport socket.
  // Must be called before any other operation.
  void attachSocket(std::shared_ptr<SocketCore> socket);

  // "host:port" this transport is bound to.  A multiplexer may only be shared
  // by the connections of its request group which target the very same
  // authority: the HTTP/2 session lives on *one* TCP/TLS connection and its
  // :authority pseudo header must stay consistent.  Every other connection of
  // the group (e.g. one aiming at a mirror on another host) has to stay on its
  // own transport instead of taking this one over.
  void setAuthority(std::string authority) { authority_ = std::move(authority); }

  const std::string& getAuthority() const { return authority_; }

  void setWakeupCallback(WakeupCallback callback);

  void setDetachCallback(DetachCallback callback);

  // One-way transport teardown.  Marks this multiplexer as retired so its
  // owning Http2MultiplexerCommand retires itself on the next iteration, wakes
  // every consumer command (so that a DownloadCommand parked on a stream runs
  // its own retry/abort path instead of lingering as a zombie), destroys all
  // stream bookkeeping and closes the transport socket.
  //
  // A retired multiplexer MUST NOT be reused: RequestGroup::getHttp2Multiplexer()
  // only ever hands it back to HttpRequestCommand::switchToHttp2(), which
  // checks isRetired() and rebuilds a fresh transport on the new socket.
  void retire(uint32_t errorCode = NGHTTP2_CANCEL);

  bool isRetired() const { return retired_; }

  // The single transport socket owned by this multiplexer.  It is the only
  // fd which may be registered for read/write events (the per-stream
  // placeholder sockets have fd == -1 and must never reach the event poll).
  const std::shared_ptr<SocketCore>& getSocket() const { return socket_; }

  // True while serialized nghttp2 frames are still pending because the socket
  // refused them (write backpressure).  The owning command registers a write
  // event in that case instead of retrying blindly.
  bool hasPendingWrite() const { return !pendingWrite_.empty(); }

  // Transport level failure (EOF / read error): marks every stream as closed
  // and wakes its consumer.  Without this a parked command would wait forever
  // for DATA frames which can no longer arrive.
  void terminateAllStreams(uint32_t errorCode);

  // Reclaims the bookkeeping of streams which have finished *normally*.
  //
  // A stream is final when three conditions hold simultaneously:
  //   * terminated()         - END_STREAM arrived or nghttp2 reported the
  //                            stream closed, so no further DATA can be routed
  //                            to it;
  //   * idle()               - window buffer *and* staging area are empty, so
  //                            reclaiming cannot drop a byte which the consumer
  //                            has not written out yet;
  //   * no consumer registered - the command which owned the stream has been
  //                            destroyed and cleared its (identity checked)
  //                            registration, so no raw Command* can reach the
  //                            buffer any more.
  //
  // Only the two maps are erased; the Http2Stream object itself stays alive as
  // long as its last shared_ptr holder (the response/entity command) keeps it,
  // so erasing can never dangle.  This is the mirror image of retire() for the
  // normal completion path: without it the maps would hold a shared_ptr forever
  // and every finished stream of a long lived multiplexed transport would leak.
  void purgeFinishedStreams();

  // Creates a stream (and its placeholder socket / recv buffer) without
  // submitting a request.  Returns nullptr if the session is not ready.
  std::shared_ptr<Http2Stream> createStream(cuid_t cuid);

  // Creates the stream and submits |head| as a HEADERS frame.  Returns
  // nullptr if submission failed.  The frame is not written to the socket by
  // this call; flushToSocket() does that.
  std::shared_ptr<Http2Stream> openStream(cuid_t cuid,
                                          const Http2RequestHead& head);

  // Reads whatever is available from the transport socket and feeds it into
  // nghttp2, then credits the flow control for the bytes the owner drained
  // since the previous call.
  PumpResult readFromSocket();

  // Serializes all pending nghttp2 frames and writes them to the socket.
  // A partially completed write is retained internally, so calling this
  // again later resumes exactly where it stopped.  Returns true when nothing
  // is left pending.
  bool flushToSocket();

  bool wantRead() const;
  bool wantWrite() const;

  // True when nghttp2 neither needs to read nor to write; the session can be
  // torn down.
  bool sessionTerminated() const;

  // streamId -> Http2Stream (the stream carries its own cuid).
  std::shared_ptr<Http2Stream> getStream(int32_t streamId) const;

  // cuid -> Http2Stream.
  std::shared_ptr<Http2Stream> getStreamByCuid(cuid_t cuid) const;

  size_t numStreams() const { return streams_.size(); }

  static const size_t READ_CHUNK_SIZE = 16_k;

private:
  static int onFrameRecvCallback(nghttp2_session* session,
                                 const nghttp2_frame* frame, void* userData);
  static int onDataChunkRecvCallback(nghttp2_session* session, uint8_t flags,
                                     int32_t streamId, const uint8_t* data,
                                     size_t len, void* userData);
  static int onStreamCloseCallback(nghttp2_session* session, int32_t streamId,
                                   uint32_t errorCode, void* userData);
  static int onHeaderCallback(nghttp2_session* session,
                              const nghttp2_frame* frame, const uint8_t* name,
                              size_t namelen, const uint8_t* value,
                              size_t valuelen, uint8_t flags, void* userData);

  void notifyWakeup(cuid_t cuid);
  void pumpFlowControl();
  bool flushPending();

  std::shared_ptr<SocketCore> socket_;
  nghttp2_session* session_;
  // "host:port" of the peer this transport is connected to (see setAuthority).
  std::string authority_;
  std::map<int32_t, std::shared_ptr<Http2Stream>> streams_;
  std::map<cuid_t, std::shared_ptr<Http2Stream>> streamsByCuid_;
  WakeupCallback wakeup_;
  DetachCallback detach_;
  bool retired_ = false;
  // Bytes produced by nghttp2_session_mem_send() which the socket has not
  // accepted yet.
  std::string pendingWrite_;
  unsigned char readBuf_[16_k];

  Http2Multiplexer(const Http2Multiplexer&);
  Http2Multiplexer& operator=(const Http2Multiplexer&);
};

} // namespace aria2

#endif // D_HTTP2_MULTIPLEXER_H
