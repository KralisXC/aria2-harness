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
#ifndef D_HTTP2_STREAM_RECV_BUFFER_H
#define D_HTTP2_STREAM_RECV_BUFFER_H

#include "common.h"

#include <memory>

#include "SocketRecvBuffer.h"

namespace aria2 {

class Http2Stream;
class Command;

// Per-stream receive buffer used by HTTP/2 multiplexed downloads.
//
// Unlike the inherited SocketRecvBuffer, this buffer never reads from the
// underlying transport socket.  The owning Http2Multiplexer is the only
// reader of the real transport; it demultiplexes DATA frame payloads and
// injects them through append().  The 16KiB buffer inherited from
// SocketRecvBuffer is only a window in front of the stream's staging area,
// which is why refill must happen before the availability of data is
// reported.
//
// EOF semantics (critical):
//   SocketRecvBuffer::recv() is used by DownloadCommand / HttpConnection /
//   HttpSkipResponseCommand as
//     eof = recv() == 0 && !wantRead() && !wantWrite()
//   A stream is backed by a placeholder SocketCore (fd == -1) whose
//   wantRead_/wantWrite_ are always false, so returning 0 here would be
//   misinterpreted as a transport EOF and could trigger
//   DL_RETRY_EX(EX_GOT_EOF).  Therefore recv() returns 0 only when the
//   stream is authoritatively finished (END_STREAM received or RST_STREAM /
//   on_stream_close fired) and all bytes have been drained.
class Http2StreamRecvBuffer : public SocketRecvBuffer {
public:
  explicit Http2StreamRecvBuffer(std::shared_ptr<SocketCore> placeholderSocket);
  virtual ~Http2StreamRecvBuffer();

  // Links this buffer back to the stream which feeds it.  The stream owns
  // this buffer (it holds a shared_ptr to it), so a weak reference is
  // enough and cannot create a reference cycle.
  void setOwner(const std::shared_ptr<Http2Stream>& owner);

  // The command currently responsible for consuming this stream's payload.
  //
  // Deliberately not a central cuid -> Command* map inside DownloadEngine:
  // the engine holds raw Command pointers would dangle as soon as the command
  // is destroyed (see the reasoning at DownloadEngine.cc
  // processAsyncFileAllocation()).  Instead the *buffer* knows its consumer
  // and the consumer is responsible for releasing the registration in its own
  // destructor, so the pointer can never outlive the command.
  //
  // clearConsumer() compares identity: a hand-off which constructs the
  // successor before destroying the predecessor (Http2ResponseCommand ->
  // DownloadCommand) must not wipe the fresh registration.
  //
  // The multiplexer uses this to activate exactly the right command
  // (Command::setStatusActive()) when DATA / END_STREAM arrives, which keeps
  // the command loop event driven: the command parks itself as long as no
  // data is available.
  void setConsumer(Command* consumer) { consumer_ = consumer; }

  void clearConsumer(Command* consumer)
  {
    if (consumer_ == consumer) {
      consumer_ = nullptr;
    }
  }

  Command* getConsumer() const { return consumer_; }

  virtual ssize_t recv() CXX11_OVERRIDE;

private:
  std::weak_ptr<Http2Stream> owner_;
  Command* consumer_;

  Http2StreamRecvBuffer(const Http2StreamRecvBuffer&);
  Http2StreamRecvBuffer& operator=(const Http2StreamRecvBuffer&);
};

} // namespace aria2

#endif // D_HTTP2_STREAM_RECV_BUFFER_H
