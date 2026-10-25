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
#include "HttpRequestCommand.h"

#include <algorithm>

#include "Request.h"
#include "DownloadEngine.h"
#include "RequestGroup.h"
#include "HttpResponseCommand.h"
#include "HttpConnection.h"
#include "HttpRequest.h"
#include "SegmentMan.h"
#include "Segment.h"
#include "Option.h"
#include "SocketCore.h"
#include "prefs.h"
#include "a2functional.h"
#include "util.h"
#include "CookieStorage.h"
#include "AuthConfigFactory.h"
#include "AuthConfig.h"
#include "DownloadContext.h"
#include "PieceStorage.h"
#include "DefaultBtProgressInfoFile.h"
#include "Logger.h"
#include "LogFactory.h"
#include "fmt.h"
#include "SocketRecvBuffer.h"

#ifdef HAVE_NGHTTP2
#  include "Http2Multiplexer.h"
#  include "Http2MultiplexerCommand.h"
#  include "Http2RequestCommand.h"
#endif // HAVE_NGHTTP2

namespace aria2 {

HttpRequestCommand::HttpRequestCommand(
    cuid_t cuid, const std::shared_ptr<Request>& req,
    const std::shared_ptr<FileEntry>& fileEntry, RequestGroup* requestGroup,
    const std::shared_ptr<HttpConnection>& httpConnection, DownloadEngine* e,
    const std::shared_ptr<SocketCore>& s, bool proxyTunneled)
    : AbstractCommand(cuid, req, fileEntry, requestGroup, e, s,
                      httpConnection->getSocketRecvBuffer()),
      httpConnection_(httpConnection),
      proxyTunneled_(proxyTunneled),
      tunnelAlpnRestricted_(false),
      http2Disabled_(false)
{
  setTimeout(std::chrono::seconds(getOption()->getAsInt(PREF_CONNECT_TIMEOUT)));
  disableReadCheckSocket();
  setWriteCheckSocket(getSocket());
}

HttpRequestCommand::~HttpRequestCommand() = default;

namespace {
std::unique_ptr<HttpRequest>
createHttpRequest(const std::shared_ptr<Request>& req,
                  const std::shared_ptr<FileEntry>& fileEntry,
                  const std::shared_ptr<Segment>& segment,
                  const std::shared_ptr<Option>& option, const RequestGroup* rg,
                  const DownloadEngine* e,
                  const std::shared_ptr<Request>& proxyRequest,
                  int64_t endOffset = 0)
{
  auto httpRequest = make_unique<HttpRequest>();
  httpRequest->setUserAgent(option->get(PREF_USER_AGENT));
  httpRequest->setRequest(req);
  httpRequest->setFileEntry(fileEntry);
  httpRequest->setSegment(segment);
  httpRequest->addHeader(option->get(PREF_HEADER));
  httpRequest->setCookieStorage(e->getCookieStorage().get());
  httpRequest->setAuthConfigFactory(e->getAuthConfigFactory().get());
  httpRequest->setOption(option.get());
  httpRequest->setProxyRequest(proxyRequest);
  httpRequest->setAcceptMetalink(rg->getDownloadContext()->getAcceptMetalink());
  httpRequest->setNoWantDigest(option->getAsBool(PREF_NO_WANT_DIGEST_HEADER));

  if (option->getAsBool(PREF_HTTP_ACCEPT_GZIP)) {
    httpRequest->enableAcceptGZip();
  }
  else {
    httpRequest->disableAcceptGZip();
  }
  if (option->getAsBool(PREF_HTTP_NO_CACHE)) {
    httpRequest->enableNoCache();
  }
  else {
    httpRequest->disableNoCache();
  }
  if (endOffset > 0) {
    httpRequest->setEndOffsetOverride(endOffset);
  }
  return httpRequest;
}
} // namespace

bool HttpRequestCommand::executeInternal()
{
  // socket->setBlockingMode();
  if (httpConnection_->sendBufferIsEmpty()) {
#ifdef ENABLE_SSL
    if (getRequest()->getProtocol() == "https") {
      if (!http2Disabled_ && !getOption()->getAsBool(PREF_ENABLE_HTTP2)) {
        // Aria2-Harness: 兼容模式 / 老旧 CDN（腾讯 gtimg 等）对 HTTP/2 帧格式
        // 处理不完整，握手前撤掉 ALPN "h2"，让整条连接走 HTTP/1.1。
        getSocket()->disableAlpnH2();
        http2Disabled_ = true;
        A2_LOG_NOTICE(fmt("CUID#%" PRId64
                          " - enable-http2=false: ALPN h2 disabled, "
                          "forcing HTTP/1.1",
                          static_cast<int64_t>(getCuid())));
      }
      if (proxyTunneled_ && !tunnelAlpnRestricted_) {
        // Aria2-Harness: CONNECT 隧道里每条连接都是独立的 TCP 传输，无法交给
        // Http2Multiplexer 复用。若握手时仍宣告 h2，服务端会选中 h2，而应用层
        // 后续实际发送 HTTP/1.1，真实服务器会直接断链。故这里在握手前撤掉 h2，
        // 让 ALPN 只协商 http/1.1。（未来支持隧道复用时删除本分支即可。）
        getSocket()->disableAlpnH2();
        tunnelAlpnRestricted_ = true;
        A2_LOG_NOTICE(
            fmt("CUID#%" PRId64
                " - proxy CONNECT tunnel: ALPN h2 disabled, using HTTP/1.1",
                static_cast<int64_t>(getCuid())));
      }
      if (!getSocket()->tlsConnect(getRequest()->getHost())) {
        setReadCheckSocketIf(getSocket(), getSocket()->wantRead());
        setWriteCheckSocketIf(getSocket(), getSocket()->wantWrite());
        addCommandSelf();
        return false;
      }
    }
#endif // ENABLE_SSL
#ifdef HAVE_NGHTTP2
    // The handshake is where ALPN settles which HTTP version this transport
    // speaks.  On "h2" the whole HTTP/1.1 chain below is skipped: the socket
    // becomes the request group's multiplexed transport and the request is
    // issued as an HTTP/2 stream.  On anything else (including no ALPN at
    // all) nothing changes - the original HTTP/1.1 path runs untouched.
    if (getRequest()->getProtocol() == "https" &&
        getSocket()->getAlpnProtocol() == "h2") {
      if (proxyTunneled_) {
        // 兜底：正常情况下握手前已通过 disableAlpnH2() 让 ALPN 只协商
        // http/1.1，不会走到这里。仅当 TLS 后端忽略该开关时才会命中，此时
        // 宁可降级 HTTP/1.1 也不能把隧道 socket 交给 Http2Multiplexer。
        A2_LOG_NOTICE(fmt("CUID#%" PRId64
                          " - HTTP/2 negotiated through a CONNECT proxy tunnel;"
                          " reusing a tunneled transport is not supported yet,"
                          " falling back to HTTP/1.1",
                          static_cast<int64_t>(getCuid())));
      }
      else if (switchToHttp2()) {
        return true;
      }
      // A maximal authority mismatch: the group's multiplexed transport serves
      // another peer, so this connection cannot be mapped onto it.  The
      // HTTP/1.1 chain below runs instead; retiring the incumbent transport
      // (the old behaviour) would have killed every stream already running on
      // it.
    }
#endif // HAVE_NGHTTP2
    if (getSegments().empty()) {
      auto httpRequest = createHttpRequest(
          getRequest(), getFileEntry(), std::shared_ptr<Segment>(), getOption(),
          getRequestGroup(), getDownloadEngine(), proxyRequest_);
      if (getOption()->getAsBool(PREF_CONDITIONAL_GET) &&
          (getRequest()->getProtocol() == "http" ||
           getRequest()->getProtocol() == "https")) {

        std::string path;

        if (getFileEntry()->getPath().empty()) {
          auto& file = getRequest()->getFile();

          // If filename part of URI is empty, we just use
          // Request::DEFAULT_FILE, since it is the name we use to
          // store file in disk.

          path = util::createSafePath(
              getOption()->get(PREF_DIR),
              (getRequest()->getFile().empty()
                   ? Request::DEFAULT_FILE
                   : util::percentDecode(std::begin(file), std::end(file))));
        }
        else {
          path = getFileEntry()->getPath();
        }

        // Aria2-Harness: 后缀随 --sqlite-progress 运行时决定。
        File ctrlfile(path + getProgressInfoFileSuffix(getOption().get()));
        File file(path);

        if (!ctrlfile.exists() && file.exists()) {
          httpRequest->setIfModifiedSinceHeader(
              file.getModifiedTime().toHTTPDate());
        }
      }
      httpConnection_->sendRequest(std::move(httpRequest));
    }
    else {
      for (auto& segment : getSegments()) {
        if (!httpConnection_->isIssued(segment)) {
          int64_t endOffset = 0;
          // FTP via HTTP proxy does not support end byte marker
          if (getRequest()->getProtocol() != "ftp" &&
              getRequestGroup()->getTotalLength() > 0 && getPieceStorage()) {
            size_t nextIndex =
                getPieceStorage()->getNextUsedIndex(segment->getIndex());
            endOffset =
                std::min(getFileEntry()->getLength(),
                         getFileEntry()->gtoloff(
                             static_cast<int64_t>(segment->getSegmentLength()) *
                             nextIndex));
          }
          httpConnection_->sendRequest(
              createHttpRequest(getRequest(), getFileEntry(), segment,
                                getOption(), getRequestGroup(),
                                getDownloadEngine(), proxyRequest_, endOffset));
        }
      }
    }
  }
  else {
    httpConnection_->sendPendingData();
  }
  if (httpConnection_->sendBufferIsEmpty()) {
    getDownloadEngine()->addCommand(make_unique<HttpResponseCommand>(
        getCuid(), getRequest(), getFileEntry(), getRequestGroup(),
        httpConnection_, getDownloadEngine(), getSocket()));
    return true;
  }
  else {
    setReadCheckSocketIf(getSocket(), getSocket()->wantRead());
    setWriteCheckSocketIf(getSocket(), getSocket()->wantWrite());
    addCommandSelf();
    return false;
  }
}

void HttpRequestCommand::setProxyRequest(
    const std::shared_ptr<Request>& proxyRequest)
{
  proxyRequest_ = proxyRequest;
}

#ifdef HAVE_NGHTTP2
bool HttpRequestCommand::switchToHttp2()
{
  auto grp = getRequestGroup();
  auto e = getDownloadEngine();
  auto mux = grp->getHttp2Multiplexer();

  // The authority this connection just negotiated with.  A transport may only
  // be shared by connections targeting the very same host:port, so this is the
  // key which decides whether the group's multiplexer may be reused at all.
  auto authority =
      getRequest()->getHost() + ":" + util::uitos(getRequest()->getPort());

  if (mux && mux->getAuthority() != authority) {
    // The group already multiplexes another peer.  Both endpoints are h2, but a
    // single multiplexer carries a single :authority, and the RequestGroup has
    // only one mux slot - so this connection stays on its own transport.  It
    // must *not* retire the incumbent: that would tear down the session which
    // is currently serving other streams of this group.
    A2_LOG_INFO(fmt("CUID#%" PRId64
                    " - request group's HTTP/2 transport serves %s, not %s;"
                    " keeping this connection on its own transport",
                    getCuid(), mux->getAuthority().c_str(), authority.c_str()));
    return false;
  }

  // A multiplexer cached in the RequestGroup is only usable while it is bound
  // to the very socket this command just negotiated.  After a pause the group
  // is halted, its commands are destroyed and aria2 closes the shared
  // SocketCore; on resume a brand new connection (or a pooled SocketCore
  // object) shows up here.  Reusing the stale transport would write frames
  // into a dead TLS context ("WinTLS: Failed to encrypt a message! 上下文已过期")
  // and the resumed stream would never be served.  Retire it and rebuild.
  // With the authority matching, this can only be the same peer, so swapping
  // the transport is safe: any stream still parked on the old one belongs to a
  // connection which is being re-established anyway.
  if (mux && (mux->isRetired() || mux->getSocket() != getSocket())) {
    A2_LOG_INFO(fmt("CUID#%" PRId64
                    " - HTTP/2 transport of the request group is stale"
                    " (socket changed or already retired); rebuilding it",
                    getCuid()));
    mux->retire(NGHTTP2_CANCEL);
    mux.reset();
    // The retired multiplexer's own Http2MultiplexerCommand is still queued; it
    // observes isRetired() on its next iteration and retires itself.  That
    // command's detach callback already removed the transport fd's event
    // registration, so no dangling poll entry can be handed to the new one.
    // The group's pointer is dropped below by the re-attach.
  }

  if (!mux) {
    // First connection of the group to negotiate h2: this is the one that
    // becomes the group's (single, permanent) multiplexed transport.  Every
    // further connection of the group never even reaches this point - it is
    // short-circuited in HttpInitiateConnectionCommand and attached as an
    // additional stream.
    mux = std::make_shared<Http2Multiplexer>();
    mux->attachSocket(getSocket());
    mux->setAuthority(authority);
    grp->setHttp2Multiplexer(mux);
    // The resident heartbeat is a *regular* command on purpose: run() only
    // enters waitData() while commands_ is non-empty, so parking it in
    // routineCommands_ would make the main loop spin instead of blocking in
    // select().  It is also the sole holder of the transport fd's read
    // registration.
    e->addCommand(
        make_unique<Http2MultiplexerCommand>(e->newCUID(), mux, e));
    A2_LOG_INFO(fmt("CUID#%" PRId64
                    " - ALPN h2 negotiated, HTTP/2 transport attached to"
                    " request group",
                    getCuid()));
  }
  else {
    // An h2 transport bound to *this* socket is already attached to the group,
    // so the request is issued as an additional stream on it (the whole point
    // of multiplexing).
    A2_LOG_INFO(fmt("CUID#%" PRId64
                    " - ALPN h2 negotiated, reusing the group's existing"
                    " HTTP/2 transport",
                    getCuid()));
  }

  // Wake the loop immediately: the queued commands must run in the next
  // iteration, not on the next 1 second refresh tick.
  e->setNoWait(true);
  e->addCommand(make_unique<Http2RequestCommand>(
      getCuid(), getRequest(), getFileEntry(), grp, e));
  return true;
}
#endif // HAVE_NGHTTP2

} // namespace aria2
