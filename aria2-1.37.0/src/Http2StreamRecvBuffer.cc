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
#include "Http2StreamRecvBuffer.h"

#include <utility>

#include "Http2Stream.h"

namespace aria2 {

Http2StreamRecvBuffer::Http2StreamRecvBuffer(
    std::shared_ptr<SocketCore> placeholderSocket)
    : SocketRecvBuffer(std::move(placeholderSocket)), consumer_(nullptr)
{
}

Http2StreamRecvBuffer::~Http2StreamRecvBuffer() = default;

void Http2StreamRecvBuffer::setOwner(const std::shared_ptr<Http2Stream>& owner)
{
  owner_ = owner;
}

ssize_t Http2StreamRecvBuffer::recv()
{
  auto owner = owner_.lock();
  if (!owner) {
    // The stream which fed this buffer is gone; this buffer is a stale view
    // of an already closed multiplexed stream, so report EOF.
    return 0;
  }

  // The caller only inspects this buffer when it is empty, so pull from the
  // stream's staging area first: otherwise "buffer drained but staging still
  // holds bytes" would look like EOF.
  owner->refillBuffer();

  if (!bufferEmpty()) {
    return static_cast<ssize_t>(getBufferLength());
  }

  if (owner->terminated()) {
    // Authoritative EOF: END_STREAM was received, or the stream was closed
    // by nghttp2 (RST_STREAM / connection teardown).
    return 0;
  }

  // The stream is still open but no byte is available at this instant.  The
  // placeholder socket has no fd and never reports wantRead()/wantWrite(), so
  // returning 0 would be misread as EOF.  Report "not EOF" instead.
  return 1;
}

} // namespace aria2
