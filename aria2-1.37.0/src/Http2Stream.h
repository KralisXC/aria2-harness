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
#ifndef D_HTTP2_STREAM_H
#define D_HTTP2_STREAM_H

#include "common.h"

#include <cstdint>
#include <map>
#include <memory>
#include <string>
#include <utility>
#include <vector>

#include <nghttp2/nghttp2.h>

#include "Command.h" // cuid_t

namespace aria2 {

class SocketCore;
class Http2StreamRecvBuffer;

// A single HTTP/2 request, i.e. one download connection as seen by aria2.
//
// One Http2Stream corresponds to exactly one cuid (connection id), which is
// the granularity aria2 already uses for its per-connection state.  The
// stream owns the placeholder socket handed to the (future) DownloadCommand
// and the per-stream SocketRecvBuffer into which the multiplexer injects the
// demultiplexed DATA payloads.
struct Http2RequestHead {
  std::string method = "GET";
  std::string scheme = "https";
  // host[:port] as it appears in the request URI.
  std::string authority;
  std::string path = "/";
  // Regular (non pseudo) request headers.
  std::vector<std::pair<std::string, std::string>> headers;
};

class Http2Stream {
public:
  Http2Stream(cuid_t cuid, std::shared_ptr<SocketCore> placeholderSocket);
  ~Http2Stream();

  cuid_t getCuid() const { return cuid_; }

  void setStreamId(int32_t streamId) { streamId_ = streamId; }
  int32_t getStreamId() const { return streamId_; }

  const std::shared_ptr<Http2StreamRecvBuffer>& getSocketRecvBuffer() const
  {
    return recvBuffer_;
  }

  // Submits the HEADERS frame for |head| to |session|.  nghttp2 copies the
  // name/value pairs, so pointers into |head| may be transient.  Returns the
  // stream id assigned by nghttp2, or a negative nghttp2 error code.
  int32_t submitRequest(nghttp2_session* session, const Http2RequestHead& head);

  // Called from nghttp2_on_data_chunk_recv_callback.  Payloads are parked in
  // a staging area first because the callback must not fail and cannot push
  // back: a single DATA frame may be larger than the 16KiB window buffer.
  void pushData(const unsigned char* data, size_t len);

  // Moves as much staged data as fits into the 16KiB window buffer.  Returns
  // the number of bytes moved.
  size_t refillBuffer();

  // Bytes not yet consumed by the owning command (window buffer + staging).
  size_t getPendingBytes() const;

  // Flow control bookkeeping: returns the number of bytes newly consumed by
  // the owning command since the previous call, and marks them as credited.
  // The multiplexer feeds this to nghttp2_session_consume_stream() so that
  // the peer's send window reflects the real consumption rate.
  size_t takeConsumedBytes();

  // Flow control bookkeeping for an *abandoned* stream: returns every byte
  // this stream has received which has not been reported to nghttp2 yet, and
  // marks them as reported.
  //
  // Discarding a stream is not the same as failing to consume it: the peer
  // already paid for those bytes out of the shared connection level window,
  // and a stream which is about to be reset never advances its own window
  // again.  Without returning the whole amount to the connection window, each
  // abandoned stream would shrink the transport permanently.  Only called on
  // the teardown path, immediately before the stream is erased - which is why
  // marking everything as credited here cannot disturb takeConsumedBytes()
  // (that one already clamps at zero for exactly this reason).
  size_t takeReceivedButUncredited()
  {
    size_t uncredited = totalReceived_ - credited_;
    credited_ = totalReceived_;
    return uncredited;
  }

  void setEndStreamReceived() { endStreamReceived_ = true; }
  bool getEndStreamReceived() const { return endStreamReceived_; }

  void setClosed(uint32_t errorCode)
  {
    closed_ = true;
    errorCode_ = errorCode;
  }
  bool getClosed() const { return closed_; }
  uint32_t getErrorCode() const { return errorCode_; }

  // Authoritative termination predicate.  Only END_STREAM or a stream close
  // notification from nghttp2 may finish this stream; the placeholder socket
  // must never be used as an EOF source.
  bool terminated() const { return endStreamReceived_ || closed_; }

  bool idle() const { return getPendingBytes() == 0; }

  void setStatusCode(int statusCode) { statusCode_ = statusCode; }
  int getStatusCode() const { return statusCode_; }

  // Response headers collected from nghttp2_on_header_callback.  Names are
  // stored as received (ASCII lower case, as required by RFC 7540 section
  // 8.1.2).  The first occurrence of a name wins: header fields such as
  // content-length are not allowed to repeat and must not be silently
  // overwritten by a duplicate.
  void addHeader(const std::string& name, const std::string& value);
  // Returns an empty string when the header is absent.
  std::string getHeader(const std::string& name) const;

  // True once the whole response header block has been received (HEADERS
  // frame with END_HEADERS).  Only then may a consumer act on the headers.
  void setHeadersComplete() { headersComplete_ = true; }
  bool getHeadersComplete() const { return headersComplete_; }

private:
  cuid_t cuid_;
  int32_t streamId_ = -1;
  std::shared_ptr<Http2StreamRecvBuffer> recvBuffer_;
  // Staging area for DATA payloads which did not fit into the window buffer.
  std::vector<unsigned char> staging_;
  size_t stagingOffset_ = 0;
  // Total number of bytes handed to pushData().
  size_t totalReceived_ = 0;
  // Bytes already reported to the flow controller.
  size_t credited_ = 0;
  bool endStreamReceived_ = false;
  bool closed_ = false;
  uint32_t errorCode_ = 0;
  int statusCode_ = 0;
  std::map<std::string, std::string> headers_;
  bool headersComplete_ = false;

  Http2Stream(const Http2Stream&);
  Http2Stream& operator=(const Http2Stream&);
};

} // namespace aria2

#endif // D_HTTP2_STREAM_H
