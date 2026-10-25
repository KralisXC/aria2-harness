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
#include "Http2DownloadCommand.h"

#include <memory>
#include <vector>

#include "DownloadEngine.h"
#include "FileEntry.h"
#include "Http2Stream.h"
#include "Http2StreamRecvBuffer.h"
#include "Request.h"
#include "RequestGroup.h"
#include "Segment.h"
#include "SocketCore.h"

namespace aria2 {

Http2DownloadCommand::Http2DownloadCommand(
    cuid_t cuid, const std::shared_ptr<Request>& req,
    const std::shared_ptr<FileEntry>& fileEntry, RequestGroup* requestGroup,
    DownloadEngine* e, const std::shared_ptr<Http2Stream>& stream,
    int64_t requestEndOffset)
    : DownloadCommand(cuid, req, fileEntry, requestGroup, e,
                      stream->getSocketRecvBuffer()->getSocket(),
                      stream->getSocketRecvBuffer()),
      stream_(stream),
      requestEndOffset_(requestEndOffset)
{
  // Take over the consumer role from Http2ResponseCommand so that the
  // multiplexer keeps waking *this* command as DATA frames arrive.
  stream_->getSocketRecvBuffer()->setConsumer(this);
}

Http2DownloadCommand::~Http2DownloadCommand()
{
  // Identity checked release: the registration belongs to this command.
  stream_->getSocketRecvBuffer()->clearConsumer(this);
}

int64_t Http2DownloadCommand::getRequestEndOffset() const
{
  // Mirrors HttpDownloadCommand::getRequestEndOffset(), which reports how far
  // the *server* said it would send: HttpDownloadCommand takes it from the
  // response's Content-Range end byte (or 0 when the response carried no
  // Range), never from the segment.
  //
  // The distinction is what DownloadCommand::prepareForNextSegment() keys on:
  //   getRequestEndOffset() == gtoloff(segment end)  -> prepareForRetry(0)
  //   getRequestEndOffset() != gtoloff(segment end)  -> keep this transport and
  //                                                    take the next clean
  //                                                    segment on it
  // Returning the *segment* end (as this used to) makes those two values equal
  // by construction, so every single piece tore its stream down and the
  // continuation branch was dead code - the peer then answered the plain-GET
  // whole-entity case with an unbounded 200 body that was discarded, and
  // segments kept being re-requested one piece at a time until the group
  // stalled on "No segment available".
  return requestEndOffset_;
}

} // namespace aria2
