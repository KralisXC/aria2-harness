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
#ifndef D_HTTP2_REQUEST_COMMAND_H
#define D_HTTP2_REQUEST_COMMAND_H

#include "AbstractCommand.h"

#include <string>

namespace aria2 {

class Http2Stream;
struct Http2RequestHead;

// Aria2-Harness: HTTP/1.1 → HTTP/2 单个 header 字段的规范化（RFC 9113 §8.2）。
//
// 语义与线上报文完全一致，故独立成纯函数，供 Http2RequestCommand 与探针共用：
//   * 字段名一律转小写（HPACK 要求，nghttp2 会校验）；
//   * 丢弃 Connection / Keep-Alive / Proxy-Connection / Transfer-Encoding /
//     Upgrade / Host（Host 由 :authority 承载，且在这里吸收为 authority）；
//   * TE 仅在值为 "trailers" 时保留；
//   * 含 CR/LF 的值直接丢弃（请求拆分 / 头注入的入口）。
//
// 返回 true 表示该字段已写入 head.headers。
bool normalizeHttp2Header(Http2RequestHead& head, const std::string& name,
                          const std::string& value);

// HTTP/2 counterpart of HttpRequestCommand.  There is no socket write here:
// the HEADERS frame is submitted to the request group's single
// Http2Multiplexer (the only owner of the real transport socket) and is
// serialized by Http2MultiplexerCommand::execute() -> flushToSocket().
//
// This command only translates the aria2 Request into an Http2RequestHead and
// hands the resulting stream to Http2ResponseCommand.
class Http2RequestCommand : public AbstractCommand {
public:
  Http2RequestCommand(cuid_t cuid, const std::shared_ptr<Request>& req,
                      const std::shared_ptr<FileEntry>& fileEntry,
                      RequestGroup* requestGroup, DownloadEngine* e);

  virtual ~Http2RequestCommand();

protected:
  virtual bool executeInternal() CXX11_OVERRIDE;

private:
  Http2RequestCommand(const Http2RequestCommand&);
  Http2RequestCommand& operator=(const Http2RequestCommand&);
};

} // namespace aria2

#endif // D_HTTP2_REQUEST_COMMAND_H
