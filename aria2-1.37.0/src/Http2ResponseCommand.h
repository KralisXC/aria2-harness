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
#ifndef D_HTTP2_RESPONSE_COMMAND_H
#define D_HTTP2_RESPONSE_COMMAND_H

#include "AbstractCommand.h"

#include <memory>

namespace aria2 {

class Http2Stream;
class Segment;

namespace error_page {
class Writer;
} // namespace error_page

// HTTP/2 counterpart of HttpResponseCommand.  It never touches HttpConnection
// or HttpResponse: status code and headers arrive through Http2Stream (fed by
// Http2Multiplexer's nghttp2 callbacks) and the entity body is already
// demultiplexed into the stream's recv buffer.
//
// Scope: status 200 (whole entity) and 206 (resumed entity, Range attached by
// Http2RequestCommand), both with a Content-Length, no Content-Encoding /
// Transfer-Encoding.  Redirects, Metalink, Digest and gzip are follow-up
// increments.
class Http2ResponseCommand : public AbstractCommand {
public:
  // |segment| is the Segment the matching Http2RequestCommand attached to its
  // Range request (null on a fresh, whole-entity download).  It is kept so the
  // 206 branch can prove the entity really starts at the offset the writer is
  // going to resume at.
  Http2ResponseCommand(cuid_t cuid, const std::shared_ptr<Request>& req,
                       const std::shared_ptr<FileEntry>& fileEntry,
                       RequestGroup* requestGroup, DownloadEngine* e,
                       const std::shared_ptr<Http2Stream>& stream,
                       const std::shared_ptr<Segment>& segment);

  virtual ~Http2ResponseCommand();

protected:
  virtual bool executeInternal() CXX11_OVERRIDE;

private:
  std::shared_ptr<Http2Stream> stream_;
  std::shared_ptr<Segment> segment_;

  // 4xx error-page capture.  The HTTP/2 body is already demultiplexed, so no
  // StreamFilter is needed: consumeErrorPage() drains the stream buffer into
  // the writer and aborts with the original status once the body ends or the
  // 256 KiB cap is hit.
  bool errorPageActive_ = false;
  int errorPageStatus_ = 0;
  std::unique_ptr<error_page::Writer> errorPageWriter_;

  // Resolves the on-disk name the same way the HTTP/1.1 path does when the
  // server provides no Content-Disposition filename.
  void determineFilename();

  bool beginErrorPage(int statusCode);
  bool consumeErrorPage();
  void abortErrorPageStatus();

  Http2ResponseCommand(const Http2ResponseCommand&);
  Http2ResponseCommand& operator=(const Http2ResponseCommand&);
};

} // namespace aria2

#endif // D_HTTP2_RESPONSE_COMMAND_H
