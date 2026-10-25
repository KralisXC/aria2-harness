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
#ifndef D_HTTP2_DOWNLOAD_COMMAND_H
#define D_HTTP2_DOWNLOAD_COMMAND_H

#include "DownloadCommand.h"

namespace aria2 {

class Http2Stream;

// HTTP/2 entity-body writer.  Structurally the same as HttpDownloadCommand
// except that there is no HttpResponse / HttpConnection: the demultiplexed
// DATA payloads are already parked in the stream's recv buffer by the
// multiplexer, and the (placeholder) socket handed to DownloadCommand is the
// one that buffer wraps.
//
// prepareForNextSegment() is deliberately *not* overridden: the HTTP/1.1
// override pools the transport socket, which is meaningless here - the real
// transport is owned by Http2Multiplexer and the placeholder socket has
// fd == -1, so it must never reach the socket pool.
class Http2DownloadCommand : public DownloadCommand {
public:
  // requestEndOffset is the file offset the *server* will stop sending at for
  // this response, i.e. one past the last byte of the response range.  It is
  // the HTTP/2 counterpart of HttpDownloadCommand::getRequestEndOffset(), which
  // derives it from the response's Content-Range.  It must be plumbed in from
  // Http2ResponseCommand, which is the only place that parsed the range.
  Http2DownloadCommand(cuid_t cuid, const std::shared_ptr<Request>& req,
                       const std::shared_ptr<FileEntry>& fileEntry,
                       RequestGroup* requestGroup, DownloadEngine* e,
                       const std::shared_ptr<Http2Stream>& stream,
                       int64_t requestEndOffset);

  virtual ~Http2DownloadCommand();

protected:
  virtual int64_t getRequestEndOffset() const CXX11_OVERRIDE;

private:
  std::shared_ptr<Http2Stream> stream_;
  int64_t requestEndOffset_;

  Http2DownloadCommand(const Http2DownloadCommand&);
  Http2DownloadCommand& operator=(const Http2DownloadCommand&);
};

} // namespace aria2

#endif // D_HTTP2_DOWNLOAD_COMMAND_H
