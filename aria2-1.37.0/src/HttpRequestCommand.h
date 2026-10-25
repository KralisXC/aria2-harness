/* <!-- copyright */
/*
 * aria2 - The high speed download utility
 *
 * Copyright (C) 2006 Tatsuhiro Tsujikawa
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
 *
 * In addition, as a special exception, the copyright holders give
 * permission to link the code of portions of this program with the
 * OpenSSL library under certain conditions as described in each
 * individual source file, and distribute linked combinations
 * including the two.
 * You must obey the GNU General Public License in all respects
 * for all of the code used other than OpenSSL.  If you modify
 * file(s) with this exception, you may extend this exception to your
 * version of the file(s), but you are not obligated to do so.  If you
 * do not wish to do so, delete this exception statement from your
 * version.  If you delete this exception statement from all source
 * files in the program, then also delete it here.
 */
/* copyright --> */
#ifndef D_HTTP_REQUEST_COMMAND_H
#define D_HTTP_REQUEST_COMMAND_H

#include "AbstractCommand.h"

namespace aria2 {

class HttpConnection;
class SocketCore;

// HttpRequestCommand sends HTTP request header to remote server.
// Because network I/O is non-blocking, execute() returns false if all
// headers have not been sent. Subsequent calls of execute() send
// remaining header and when all headers are completely sent,
// execute() creates next Command object, HttpResponseCommand, and
// returns true.
class HttpRequestCommand : public AbstractCommand {
private:
  std::shared_ptr<Request> proxyRequest_;

  std::shared_ptr<HttpConnection> httpConnection_;

  // Aria2-Harness: 本命令的 socket 是否是「经 HTTP 代理 CONNECT 隧道」建立
  // （https over proxy 路径，由 HttpProxyResponseCommand 在 200 之后创建）。
  //
  // 隧道里的 socket 已经指向源站，TLS/ALPN 仍可能协商出 h2；但当前
  // Http2Multiplexer 只维护「一条直连 transport」，多连接经隧道时每条连接都会
  // 新开一个隧道 socket，switchToHttp2() 会不断判定旧 mux 的 socket 过期并重建，
  // 反而会中断已在跑的流。故这里显式降级到 HTTP/1.1，并把「支持隧道复用」的
  // 钩子留在 switchToHttp2 的调用点（见 executeInternal）。
  bool proxyTunneled_;

  // Aria2-Harness: 隧道连接是否已在握手前撤掉 ALPN "h2"（只需做一次，避免
  // executeInternal 因握手未完成多次重入时重复打日志）。
  bool tunnelAlpnRestricted_;

  // Aria2-Harness: 是否已因 `enable-http2=false` 撤掉 ALPN "h2"。
  // 同样是"只做一次"，防止握手重入时重复打日志。
  bool http2Disabled_;

protected:
  virtual bool executeInternal() CXX11_OVERRIDE;

#ifdef HAVE_NGHTTP2
  // Called once the TLS handshake of this connection reported ALPN "h2".
  //
  // Returns true when the request was handed over to the request group's
  // Http2Multiplexer: the HTTP/1.1 request path must then be abandoned, the
  // caller finishes this command (executeInternal() == true) and that is what
  // drops the HttpConnection wrapper and this command's write registration,
  // leaving the transport fd registered for read by the multiplexer only.
  //
  // Returns false when this connection must *not* be multiplexed, i.e. when the
  // group's transport serves a different authority (mirror on another host).
  // Taking it over would retire it and kill the streams already running on it,
  // so the caller falls through to the ordinary HTTP/1.1 path instead.
  bool switchToHttp2();
#endif // HAVE_NGHTTP2

public:
  HttpRequestCommand(cuid_t cuid, const std::shared_ptr<Request>& req,
                     const std::shared_ptr<FileEntry>& fileEntry,
                     RequestGroup* requestGroup,
                     const std::shared_ptr<HttpConnection>& httpConnection,
                     DownloadEngine* e, const std::shared_ptr<SocketCore>& s,
                     bool proxyTunneled = false);
  virtual ~HttpRequestCommand();

  void setProxyRequest(const std::shared_ptr<Request>& proxyRequest);
};

} // namespace aria2

#endif // D_HTTP_REQUEST_COMMAND_H
