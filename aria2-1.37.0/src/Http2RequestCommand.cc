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
#include "Http2RequestCommand.h"

#include <algorithm>
#include <cctype>
#include <cinttypes>
#include <memory>
#include <string>
#include <utility>
#include <vector>

#include "a2functional.h"
#include "AuthConfigFactory.h"
#include "CookieStorage.h"
#include "DlAbortEx.h"
#include "DownloadContext.h"
#include "DownloadEngine.h"
#include "FileEntry.h"
#include "Http2Multiplexer.h"
#include "Http2ResponseCommand.h"
#include "Http2Stream.h"
#include "HttpRequest.h"
#include "LogFactory.h"
#include "Logger.h"
#include "Option.h"
#include "PieceStorage.h"
#include "Request.h"
#include "RequestGroup.h"
#include "Segment.h"
#include "fmt.h"
#include "prefs.h"
#include "util.h"

namespace aria2 {

namespace {

std::string toLowerAscii(const std::string& src)
{
  std::string dest(src);
  for (std::string::iterator i = dest.begin(), eoi = dest.end(); i != eoi;
       ++i) {
    *i = static_cast<char>(std::tolower(static_cast<unsigned char>(*i)));
  }
  return dest;
}

std::string trim(const std::string& src)
{
  auto begin = src.find_first_not_of(" \t\r\n");
  if (begin == std::string::npos) {
    return std::string();
  }
  auto end = src.find_last_not_of(" \t\r\n");
  return src.substr(begin, end - begin + 1);
}

// HTTP/1.1-only header fields which must not appear in an HTTP/2 message
// (RFC 7540 section 8.1.2.2): the peer would answer with a stream RST.  "host"
// is dropped as well because the target authority is carried by the
// ":authority" pseudo header; sending both is a protocol error.
bool isConnectionSpecificHeader(const std::string& lowerName)
{
  return lowerName == "connection" || lowerName == "keep-alive" ||
         lowerName == "proxy-connection" || lowerName == "transfer-encoding" ||
         lowerName == "upgrade" || lowerName == "host";
}

bool isInvalidHeaderValue(const std::string& value)
{
  return value.find('\r') != std::string::npos ||
         value.find('\n') != std::string::npos;
}

// Translates an absolute-form request target ("https://host:port/path") into
// origin form ("/path").  createRequest() emits an absolute URI only when a
// proxy request is attached, which the happy path does not use; the
// conversion is kept so the command stays correct if that ever changes.
std::string toOriginForm(const std::string& target)
{
  static const std::string HTTPS_PREFIX("https://");
  static const std::string HTTP_PREFIX("http://");
  size_t schemeLen = 0;
  if (target.compare(0, HTTPS_PREFIX.size(), HTTPS_PREFIX) == 0) {
    schemeLen = HTTPS_PREFIX.size();
  }
  else if (target.compare(0, HTTP_PREFIX.size(), HTTP_PREFIX) == 0) {
    schemeLen = HTTP_PREFIX.size();
  }
  else {
    return target;
  }
  auto slash = target.find('/', schemeLen);
  if (slash == std::string::npos) {
    return "/";
  }
  return target.substr(slash);
}

// Builds the HTTP/2 request head by *reusing* HttpRequest::createRequest() and
// parsing the resulting HTTP/1.1 head.  Reusing the existing builder keeps all
// of the wire-level work done elsewhere (User-Agent ordering, proxy handling,
// cookies, auth, Want-Digest, user supplied --header lines) instead of
// re-implementing it.  The only translation left is the HTTP/1.1 -> HTTP/2
// header set adjustment, which is exactly what this function does.
void buildRequestHead(const std::shared_ptr<Request>& req,
                      const std::shared_ptr<FileEntry>& fileEntry,
                      const Option* option, RequestGroup* rg, DownloadEngine* e,
                      const std::shared_ptr<Segment>& segment,
                      Http2RequestHead& head)
{
  auto httpRequest = make_unique<HttpRequest>();
  httpRequest->setUserAgent(option->get(PREF_USER_AGENT));
  httpRequest->setRequest(req);
  httpRequest->setFileEntry(fileEntry);
  httpRequest->addHeader(option->get(PREF_HEADER));
  httpRequest->setCookieStorage(e->getCookieStorage().get());
  httpRequest->setAuthConfigFactory(e->getAuthConfigFactory().get());
  httpRequest->setOption(option);
  httpRequest->setAcceptMetalink(rg->getDownloadContext()->getAcceptMetalink());
  httpRequest->setNoWantDigest(option->getAsBool(PREF_NO_WANT_DIGEST_HEADER));
  // Attaching the Segment is what makes createRequest() emit
  // "Range: bytes=<start>-", which is exactly the file offset the entity writer
  // resumes at: SinkStreamFilter::transform() writes every received byte at
  // segment->getPositionToWrite().  Without the Range header the server replays
  // the entity from byte 0 while the writer places it at the resume offset,
  // silently shredding the whole file.
  //
  // The end byte must be clipped to the block of pieces this connection will
  // really write out, i.e. exactly the same computation the HTTP/1.1 path uses.
  // An open ended range is *not* interchangeable on a multiplexed stream: the
  // peer would then push the entire remainder of the entity into this stream,
  // the consumer stops as soon as its own segment is complete, and whatever is
  // left in the stream buffer is never drained - therefore never credited back
  // to the *connection level* flow control window either, starving every other
  // stream of the transport.  On the last segment the computed end reduces to
  // the entity length and getEndByte() turns it back into an open ended range,
  // which is what is wanted there.  On a fresh download (offset 0, no Segment)
  // no Range header is emitted at all and the server answers 200 with the whole
  // entity.
  if (segment) {
    httpRequest->setSegment(segment);
    if (req->getProtocol() != "ftp" && rg->getTotalLength() > 0 &&
        rg->getPieceStorage()) {
      size_t nextIndex =
          rg->getPieceStorage()->getNextUsedIndex(segment->getIndex());
      int64_t endOffset =
          std::min(fileEntry->getLength(),
                   fileEntry->gtoloff(static_cast<int64_t>(
                                          segment->getSegmentLength()) *
                                      nextIndex));
      if (endOffset > 0) {
        httpRequest->setEndOffsetOverride(endOffset);
      }
    }
  }
  // Compression is disabled because Content-Encoding handling has no HTTP/2
  // equivalent yet; getRange() also forces this automatically for ranged
  // requests.
  httpRequest->disableAcceptGZip();
  httpRequest->disableNoCache();
  // A proxy request is intentionally not attached: after the CONNECT tunnel
  // the request is in origin form and the authority is the target itself.

  const std::string text = httpRequest->createRequest();

  head.method = "GET";
  head.scheme = req->getProtocol();
  head.authority.clear();
  head.path = "/";
  head.headers.clear();

  bool firstLine = true;
  size_t pos = 0;
  while (pos < text.size()) {
    auto eol = text.find("\r\n", pos);
    if (eol == std::string::npos) {
      break;
    }
    std::string line = text.substr(pos, eol - pos);
    pos = eol + 2;

    if (firstLine) {
      firstLine = false;
      // "<METHOD> <target> HTTP/1.1"
      auto firstSp = line.find(' ');
      auto lastSp = line.rfind(' ');
      if (firstSp != std::string::npos && lastSp > firstSp) {
        head.method = line.substr(0, firstSp);
        head.path =
            toOriginForm(line.substr(firstSp + 1, lastSp - firstSp - 1));
      }
      continue;
    }

    if (line.empty()) {
      continue;
    }
    auto colon = line.find(':');
    if (colon == std::string::npos) {
      continue;
    }
    // RFC 9113 8.2: field names are lower case on the wire and nghttp2
    // validates that, so normalise here rather than at submission time.
    normalizeHttp2Header(head, line.substr(0, colon), line.substr(colon + 1));
  }

  if (head.authority.empty()) {
    // The user overrode "Host" away (or createRequest() emitted none).
    head.authority = req->getHost();
    auto port = req->getPort();
    if (port != 80 && port != 443) {
      head.authority += ':';
      head.authority += util::uitos(port);
    }
  }
}

} // namespace

bool normalizeHttp2Header(Http2RequestHead& head, const std::string& rawName,
                          const std::string& rawValue)
{
  const std::string name = toLowerAscii(trim(rawName));
  if (name.empty()) {
    return false;
  }
  const std::string value = trim(rawValue);
  if (isInvalidHeaderValue(value)) {
    return false;
  }
  if (name == "te") {
    // RFC 9113 8.2.2: the only legal value of TE in HTTP/2 is "trailers".
    if (value != "trailers") {
      return false;
    }
    head.headers.push_back(std::make_pair(name, value));
    return true;
  }
  if (isConnectionSpecificHeader(name)) {
    // Host is not a legal HTTP/2 field: its role is taken by :authority.
    if (name == "host") {
      head.authority = value;
    }
    return false;
  }
  head.headers.push_back(std::make_pair(name, value));
  return true;
}

Http2RequestCommand::Http2RequestCommand(
    cuid_t cuid, const std::shared_ptr<Request>& req,
    const std::shared_ptr<FileEntry>& fileEntry, RequestGroup* requestGroup,
    DownloadEngine* e)
    : AbstractCommand(cuid, req, fileEntry, requestGroup, e)
{
  // No socket check: the HEADERS frame is submitted to the group's single
  // multiplexer and serialized later by Http2MultiplexerCommand::flushToSocket().
  // The per-stream placeholder socket (fd == -1) belongs to the stream and is
  // only reached through Http2ResponseCommand / Http2DownloadCommand.
  disableReadCheckSocket();
  disableWriteCheckSocket();
}

Http2RequestCommand::~Http2RequestCommand() = default;

bool Http2RequestCommand::executeInternal()
{
  auto grp = getRequestGroup();
  auto mux = grp->getHttp2Multiplexer();
  if (!mux) {
    throw DL_ABORT_EX(
        "HTTP/2 multiplexer is not attached to this request group");
  }

  Http2RequestHead head;
  // AbstractCommand::execute() has already checked out the segments owned by
  // this CUID whenever a PieceStorage exists - i.e. exactly on a resumed (or
  // segmented) download.  Attaching that Segment is what turns the request
  // into "Range: bytes=<resume offset>-"; on a fresh download segments_ is
  // empty (the PieceStorage does not exist yet) and the request stays a plain
  // whole-entity GET.
  std::shared_ptr<Segment> segment;
  if (!getSegments().empty()) {
    segment = getSegments().front();
  }

  buildRequestHead(getRequest(), getFileEntry(), getOption().get(), grp,
                   getDownloadEngine(), segment, head);

  if (segment) {
    A2_LOG_INFO(fmt("CUID#%" PRId64
                    " - HTTP/2 request resumes at file offset %" PRId64
                    " (segment index %lu), sending Range",
                    static_cast<int64_t>(getCuid()),
                    getFileEntry()->gtoloff(segment->getPositionToWrite()),
                    static_cast<unsigned long>(segment->getIndex())));
  }
  else {
    A2_LOG_INFO(fmt("CUID#%" PRId64
                    " - HTTP/2 request carries no segment; sending a plain"
                    " whole-entity GET",
                    static_cast<int64_t>(getCuid())));
  }

  auto stream = mux->openStream(getCuid(), head);
  if (!stream) {
    throw DL_ABORT_EX(fmt("Failed to open HTTP/2 stream for CUID#%" PRId64,
                          static_cast<int64_t>(getCuid())));
  }

  A2_LOG_INFO(fmt("CUID#%" PRId64
                  " - HTTP/2 stream %d opened, waiting for response headers",
                  static_cast<int64_t>(getCuid()), stream->getStreamId()));

  // The HEADERS frame is only *submitted* here: Http2Multiplexer::flushToSocket()
  // serializes and writes it on the next iteration.  Without this the main loop
  // would block in select() for a whole refresh interval first, degrading the
  // round trip to a 1 Hz poll.
  getDownloadEngine()->setNoWait(true);

  getDownloadEngine()->addCommand(make_unique<Http2ResponseCommand>(
      getCuid(), getRequest(), getFileEntry(), grp, getDownloadEngine(), stream,
      segment));
  return true;
}

} // namespace aria2
