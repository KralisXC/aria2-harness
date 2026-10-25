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
#include "Http2ResponseCommand.h"

#include <cinttypes>
#include <cstdint>
#include <cstdlib>
#include <cstring>
#include <memory>
#include <string>

#include "CheckIntegrityEntry.h"
#include "DefaultBtProgressInfoFile.h"
#include "DiskAdaptor.h"
#include "a2functional.h"
#include "DlAbortEx.h"
#include "DlRetryEx.h"
#include "DownloadContext.h"
#include "DownloadEngine.h"
#include "ErrorPageWriter.h"
#include "FileEntry.h"
#include "GroupId.h"
#include "Http2DownloadCommand.h"
#include "Http2Stream.h"
#include "Http2StreamRecvBuffer.h"
#include "LogFactory.h"
#include "Logger.h"
#include "NullProgressInfoFile.h"
#include "Option.h"
#include "PieceStorage.h"
#include "Request.h"
#include "RequestGroup.h"
#include "Segment.h"
#include "SegmentMan.h"
#include "SocketCore.h"
#include "error_code.h"
#include "fmt.h"
#include "message.h"
#include "prefs.h"
#include "util.h"

namespace aria2 {

namespace {

// Parses a Content-Range header field (RFC 7233 section 4.2), i.e.
// "bytes <first>-<last>/<complete-length>".  Without a valid start offset and
// instance length the resume path cannot prove that the bytes it is about to
// write land at the offset the entity writer is positioned at, so a "*" (or
// any malformed value) is rejected rather than silently accepted.
bool parseContentRange(const std::string& value, int64_t& start, int64_t& end,
                       int64_t& total)
{
  auto trim = [](const std::string& s) -> std::string {
    auto b = s.find_first_not_of(" \t");
    if (b == std::string::npos) {
      return std::string();
    }
    auto e = s.find_last_not_of(" \t");
    return s.substr(b, e - b + 1);
  };

  auto specBegin = value.find_first_not_of(" \t");
  if (specBegin == std::string::npos) {
    return false;
  }
  // Skip the byte-range unit ("bytes " or, as some servers send, "bytes=");
  // aria2's HTTP/1.1 parser tolerates it being omitted as well.
  if (value.size() >= specBegin + 5 &&
      util::strieq(value.substr(specBegin, 5), "bytes")) {
    specBegin = value.find_first_not_of(" \t=", specBegin + 5);
    if (specBegin == std::string::npos) {
      return false;
    }
  }

  auto slash = value.find('/', specBegin);
  if (slash == std::string::npos) {
    return false;
  }
  auto minus = value.find('-', specBegin);
  if (minus == std::string::npos || minus >= slash) {
    return false;
  }

  const std::string startText =
      trim(value.substr(specBegin, minus - specBegin));
  const std::string endText = trim(value.substr(minus + 1, slash - minus - 1));
  const std::string totalText = trim(value.substr(slash + 1));
  if (startText.empty() || endText.empty() || totalText.empty()) {
    return false;
  }
  return util::parseLLIntNoThrow(start, startText) &&
         util::parseLLIntNoThrow(end, endText) &&
         util::parseLLIntNoThrow(total, totalText) && start >= 0 &&
         end >= start && end < total;
}

} // namespace

Http2ResponseCommand::Http2ResponseCommand(
    cuid_t cuid, const std::shared_ptr<Request>& req,
    const std::shared_ptr<FileEntry>& fileEntry, RequestGroup* requestGroup,
    DownloadEngine* e, const std::shared_ptr<Http2Stream>& stream,
    const std::shared_ptr<Segment>& segment)
    : AbstractCommand(cuid, req, fileEntry, requestGroup, e,
                      stream->getSocketRecvBuffer()->getSocket(),
                      stream->getSocketRecvBuffer()),
      stream_(stream),
      segment_(segment)
{
  // Park ourselves on the stream's buffer: Http2Multiplexer::notifyWakeup()
  // activates us as soon as the response header block is complete.  No socket
  // check is needed - the placeholder socket has fd == -1 and
  // AbstractCommand::setReadCheckSocket() skips sockets which are not open.
  stream_->getSocketRecvBuffer()->setConsumer(this);
  checkSocketRecvBuffer();
}

Http2ResponseCommand::~Http2ResponseCommand()
{
  // Identity checked: when the entity writer has already taken over the
  // consumer role, this must not wipe its registration.
  stream_->getSocketRecvBuffer()->clearConsumer(this);
}

bool Http2ResponseCommand::executeInternal()
{
  if (!stream_->getHeadersComplete()) {
    if (stream_->terminated()) {
      // The multiplexer retired the stream (pause/halt or transport failure)
      // before the response header block arrived.  Nothing can ever activate
      // this command again, so it must not park itself: bail out and let the
      // normal retry/next-URI machinery take over.
      throw DL_ABORT_EX(
          "HTTP/2 stream was closed before the response headers arrived");
    }
    // The multiplexer activates us again once END_HEADERS has been received.
    addCommandSelf();
    return false;
  }

  // A previous invocation already switched this command into error-page
  // capture mode; keep draining the body until the cap or END_STREAM.
  if (errorPageActive_) {
    return consumeErrorPage();
  }

  const int statusCode = stream_->getStatusCode();
  auto grp = getRequestGroup();
  auto& fe = getFileEntry();

  // ---------------------------------------------------------------------
  // HTTP redirects (301/302/303/307/308, plus 300 like the HTTP/1.1 path).
  //
  // HttpResponse::isRedirect() treats a 3xx as a redirect only when a
  // Location header is present.  Mirror that here: the stream has already
  // delivered its header block, so the Location value is complete.  The
  // request object is mutated *in place* by Request::redirectUri(), which
  // also resolves relative / network-path references and increments the
  // redirect counter -- exactly what HttpResponse::processRedirect() does.
  //
  // The stream must be abandoned, not drained: this command is the only
  // consumer of its buffer, so when it is destroyed the multiplexer sees a
  // consumer-less stream and reclaims it (RST_STREAM + connection-window
  // credit, see Http2Multiplexer::purgeFinishedStreams()).  Then the chain is
  // rebuilt from scratch by prepareForRetry(), which pools the mutated
  // Request and queues CreateRequestCommand -> HttpInitiateConnectionCommand.
  // A same-authority redirect short-circuits through that command's HTTP/2
  // fast path and simply opens a new stream on the existing transport; a
  // different authority re-runs DNS / TCP / TLS / ALPN on a fresh connection.
  // ---------------------------------------------------------------------
  if (statusCode == 300 || statusCode == 301 || statusCode == 302 ||
      statusCode == 303 || statusCode == 307 || statusCode == 308) {
    const std::string location = stream_->getHeader("location");
    if (location.empty()) {
      // Upstream HttpResponseCommand treats a 3xx without Location as a
      // retryable response (HttpSkipResponseCommand::processResponse() ->
      // prepareForRetry(0)); do the same instead of aborting the download.
      A2_LOG_NOTICE(fmt("CUID#%" PRId64
                        " - HTTP/2 response status %d carries no Location;"
                        " retrying the request",
                        getCuid(), statusCode));
      return prepareForRetry(0);
    }

    if (getRequest()->getRedirectCount() >= Request::MAX_REDIRECT) {
      throw DL_ABORT_EX2(
          fmt("Too many redirects: count=%u",
              static_cast<unsigned int>(getRequest()->getRedirectCount())),
          error_code::HTTP_TOO_MANY_REDIRECTS);
    }

    if (!getRequest()->redirectUri(util::percentEncodeMini(location))) {
      throw DL_RETRY_EX(fmt("CUID#%" PRId64
                            " - Redirect to %s failed. It may not be a valid"
                            " URI.",
                            getCuid(),
                            getRequest()->getCurrentUri().c_str()));
    }

    A2_LOG_NOTICE(
        fmt(MSG_REDIRECT, getCuid(), getRequest()->getCurrentUri().c_str()));
    return prepareForRetry(0);
  }

  // ---------------------------------------------------------------------
  // Authentication challenges.
  //
  // The HTTP/1.1 path only ever retries a 401 when --http-auth-challenge is
  // on and a credential is available; the final failure surfaces as a
  // distinct error code so the UI can prompt for a username/password instead
  // of showing a generic protocol error.  407 (proxy authentication) has no
  // retry path at all and maps to the same code.
  // ---------------------------------------------------------------------
  if (statusCode == 401 || statusCode == 407) {
    throw DL_ABORT_EX2(
        fmt("HTTP/2 authentication required: status %d", statusCode),
        error_code::HTTP_AUTH_REQUIRED);
  }

  // The file offset the entity writer is going to place the first received byte
  // at.  SinkStreamFilter::transform() always writes at
  // segment->getPositionToWrite(), so this is both the offset
  // Http2RequestCommand turned into "Range: bytes=<start>-" and the offset the
  // server's response has to address.  Zero on a fresh (whole entity) download.
  const int64_t expectedStart =
      segment_ ? fe->gtoloff(segment_->getPositionToWrite()) : 0;

  int64_t totalLength = 0;
  // False for a 200 response without Content-Length (chunked or
  // close-delimited).  Drives the UnknownLengthPieceStorage path below.
  bool lengthKnown = true;
  // One past the last byte the peer said it will deliver on this stream, i.e.
  // the exact analogue of what HttpDownloadCommand::getRequestEndOffset()
  // reads off the response's Content-Range.  0 means "no bounded range", the
  // HTTP/1.1 answer for a response that carried no Range at all.  The value
  // decides whether DownloadCommand::prepareForNextSegment() keeps this
  // transport and walks on to the next clean segment (bounded / unbounded
  // response) or tears the stream down after a single piece.
  int64_t requestEndOffset = 0;
  if (statusCode == 206) {
    // The server honoured the Range request.  Content-Range is the only
    // in-band proof that the payload really starts where the writer resumes; a
    // mismatch (proxy rewriting the range, server clamping it) means writing
    // through would shred the already completed prefix of the file.
    const std::string contentRange = stream_->getHeader("content-range");
    if (contentRange.empty()) {
      throw DL_ABORT_EX(
          "HTTP/2 206 response carries no Content-Range header");
    }
    int64_t rangeStart = 0;
    int64_t rangeEnd = 0;
    int64_t rangeTotal = 0;
    if (!parseContentRange(contentRange, rangeStart, rangeEnd, rangeTotal)) {
      throw DL_ABORT_EX(
          fmt("Malformed Content-Range in HTTP/2 206 response: %s",
              contentRange.c_str()));
    }
    if (rangeStart != expectedStart) {
      throw DL_ABORT_EX(
          fmt("HTTP/2 206 response starts at byte %" PRId64
              " but the entity writer resumes at byte %" PRId64,
              rangeStart, expectedStart));
    }
    totalLength = rangeTotal;
    requestEndOffset = rangeEnd + 1;
    A2_LOG_INFO(fmt("CUID#%" PRId64
                    " - HTTP/2 206 partial content: Content-Range=%" PRId64
                    "-%" PRId64 "/%" PRId64 ", resuming at %" PRId64,
                    getCuid(), rangeStart, rangeEnd, rangeTotal,
                    expectedStart));
  }
  else if (statusCode == 200) {
    // Whole entity.  Only legal while the writer is still at the very
    // beginning: a 200 during a resume means the server ignored the Range
    // header and would replay the entity from byte 0 into a non-zero file
    // offset.  Refuse instead of silently corrupting the file.
    if (expectedStart != 0) {
      throw DL_ABORT_EX(
          fmt("HTTP/2 server ignored the Range request (status 200) while the"
              " entity writer resumes at byte %" PRId64,
              expectedStart));
    }
    const std::string contentLength = stream_->getHeader("content-length");
    if (contentLength.empty()) {
      // Unknown-length entity (chunked, or 200 + END_STREAM without a length).
      // requestEndOffset stays 0 (unbounded) and the piece storage is chosen
      // by DownloadContext::knowsTotalLength() during initPieceStorage().
      lengthKnown = false;
      totalLength = 0;
      A2_LOG_NOTICE(fmt("CUID#%" PRId64
                        " - HTTP/2 response carries no Content-Length;"
                        " entity length is unknown",
                        getCuid()));
    }
    else {
      totalLength = strtoll(contentLength.c_str(), nullptr, 10);
    }
    // Whole entity and no Range in the response: the peer will stream all
    // totalLength bytes on this stream.  Report it as unbounded (0) exactly
    // like the HTTP/1.1 path does, so the writer keeps consuming this stream
    // while it walks across segments instead of stopping at the first piece.
    // Truncating here would abandon the entity body mid-flight.
    requestEndOffset = 0;
  }
  else {
    if (statusCode >= 400) {
      grp->increaseAndValidateFileNotFoundCount();
      // 401/407 were already converted to HTTP_AUTH_REQUIRED above, so this
      // is the 403/404/... family: keep a bounded copy of the server's error
      // page for the UI before aborting with the original status.
      if (beginErrorPage(statusCode)) {
        return consumeErrorPage();
      }
    }
    throw DL_ABORT_EX(fmt("HTTP/2 response status %d is not supported yet",
                          statusCode));
  }

  auto ctx = getDownloadContext();
  // Whether this is the very first response of the RequestGroup, i.e. whether
  // the branch below is the one that initializes the PieceStorage.  The two
  // branches differ in exactly one respect: only the first one is allowed to run
  // createCheckIntegrityEntry()/prepareForNextAction() (see the tail of this
  // function).
  const bool firstResponse = !getPieceStorage();

  if (firstResponse) {
    // First (and, for the happy path, only) response for this download.
    fe->setLength(totalLength);
    fe->setContentType(stream_->getHeader("content-type"));
    if (fe->getPath().empty()) {
      determineFilename();
    }
    grp->preDownloadProcessing();

    if (lengthKnown) {
      auto progressInfoFile = makeProgressInfoFile(
          ctx, std::shared_ptr<PieceStorage>{}, getOption().get());
      grp->adjustFilename(progressInfoFile);
    }
    else {
      // An unknown total length cannot be resumed, and the context has to
      // report "unknown" *before* initPieceStorage() so that it allocates an
      // UnknownLengthPieceStorage (single GrowSegment) instead of a
      // DefaultPieceStorage sized from a bogus zero length.
      ctx->markTotalLengthIsUnknown();
      grp->adjustFilename(std::make_shared<NullProgressInfoFile>());
    }
    grp->initPieceStorage();

    if (getOption()->getAsBool(PREF_DRY_RUN)) {
      getPieceStorage()->markAllPiecesDone();
      ctx->setChecksumVerified(true);
      return true;
    }

    if (lengthKnown && totalLength == 0) {
      // Zero-length entity: the file still has to be materialized.  This
      // mirrors the tail of HttpResponseCommand::handleOtherEncoding().
      getPieceStorage()->getDiskAdaptor()->initAndOpenFile();
      getPieceStorage()->markAllPiecesDone();
      ctx->setChecksumVerified(true);
      return true;
    }

    if (!lengthKnown) {
      // Same preparation as HttpResponseCommand::handleOtherEncoding(): the
      // direct disk adaptor starts at size 0 and grows with the body, while
      // the unknown-length storage reports completion on EOF.
      getPieceStorage()->getDiskAdaptor()->initAndOpenFile();
    }
  }
  else {
    // A resumed download (pause -> unpause): file, piece storage and control
    // file already exist, so only the entity length has to be cross-checked.
    // On the 206 path the Content-Range check above has already proven that
    // the payload starts exactly at the offset the entity writer resumes at.
    grp->validateTotalLength(fe->getLength(), totalLength);
  }

  // Mirror the segment checkout of the two branches
  // HttpResponseCommand::executeInternal() splits the entity writer between:
  //  * HttpResponseCommand.cc:320-324 - PieceStorage already exists, i.e. this
  //    response answers a Range request.  The CUID's Segment was checked out
  //    when the request chain was built (RequestGroup::createNextCommand ->
  //    CreateRequestCommand -> AbstractCommand::execute()) and is what
  //    |segment_| carries; that branch touches the segment bookkeeping not at
  //    all.
  //  * HttpResponseCommand.cc:374 (handleDefaultEncoding) - the first, plain
  //    whole-entity response.  No Segment exists yet and
  //    AbstractCommand::execute() refuses to run a Request-carrying command
  //    without one, so getSegmentWithIndex(cuid, 0) is used to check out the
  //    piece the writer starts at.
  // Checking it out unconditionally (as this used to) hands the CUID a SECOND
  // in-flight segment on every resumed stream.  SegmentMan then reports
  // getSegments().size() == 2, DownloadCommand::prepareForNextSegment()'s
  // "size == 1" guard (DownloadCommand.cc:348) fails, and it takes the
  // prepareForRetry(0) path (DownloadCommand.cc:377-379) after every single
  // piece: one brand new HTTP/2 stream plus one full request round trip per
  // piece, with each abandoned stream's unread remainder quietly holding on to
  // the connection level flow control window.
  if (!segment_) {
    segment_ = getSegmentMan()->getSegmentWithIndex(getCuid(), 0);
    if (!segment_) {
      throw DL_ABORT_EX("No segment available for the HTTP/2 entity response");
    }
  }

  A2_LOG_INFO(fmt("CUID#%" PRId64
                  " - HTTP/2 response: status=%d length=%" PRId64
                  ", starting entity download",
                  getCuid(), statusCode, totalLength));

  if (!firstResponse || !lengthKnown) {
    // Resumed (Range) response: this is exactly what
    // HttpResponseCommand::executeInternal() does at
    // HttpResponseCommand.cc:317-324 - hand the entity body to the writer on
    // *this* stream and stop.
    //
    // createCheckIntegrityEntry()/prepareForNextAction() must NOT run here.
    // Their tail is StreamFileAllocationEntry::prepareForNextAction()
    // (StreamFileAllocationEntry.cc:99) -> RequestGroup::createNextCommandWithAdj(),
    // which incubates a whole fresh batch of CreateRequestCommand - one per
    // --split slot, each with a brand new cuid - and whose contract is "once per
    // RequestGroup", not "once per stream": in HTTP/1.1 it is only ever reached
    // from HttpResponseCommand::handleDefaultEncoding(), i.e. for the first
    // whole-entity response.  Running it per response makes the command count
    // grow with the response count - measured on a single 28 MiB entity: 417
    // streams, 855 cuids, 828 "No segment available" and roughly 350 MB of in
    // flight DATA thrown away together with the abandoned streams.
    getDownloadEngine()->addCommand(make_unique<Http2DownloadCommand>(
        getCuid(), getRequest(), getFileEntry(), grp, getDownloadEngine(),
        stream_, requestEndOffset));
    return true;
  }

  // createCheckIntegrityEntry() is where the entity file actually gets opened
  // (via RequestGroup::loadAndOpenFile()): without it the disk writer keeps
  // fd_ == INVALID_HANDLE_VALUE and the first cache flush fails with
  // EX_FILE_SEEK.  Same call order as
  // HttpResponseCommand::handleDefaultEncoding().
  auto checkEntry = grp->createCheckIntegrityEntry();
  if (!checkEntry) {
    // The control file says the download is already complete; there is nothing
    // left to fetch.
    return true;
  }

  checkEntry->pushNextCommand(make_unique<Http2DownloadCommand>(
      getCuid(), getRequest(), getFileEntry(), grp, getDownloadEngine(),
      stream_, requestEndOffset));
  // Runs file allocation (when enabled), queues the entity writer and - once for
  // the whole group - the additional connections this group's --split setting
  // asks for; identical to HttpResponseCommand::handleDefaultEncoding().
  prepareForNextAction(std::move(checkEntry));
  return true;
}

bool Http2ResponseCommand::beginErrorPage(int statusCode)
{
  if (!getOption()->getAsBool(PREF_SAVE_ERROR_PAGE) ||
      getRequest()->getMethod() == Request::METHOD_HEAD) {
    return false;
  }
  errorPageWriter_ = make_unique<error_page::Writer>(
      error_page::getDir(getOption().get()),
      GroupId::toHex(getRequestGroup()->getGID()));
  if (!errorPageWriter_->active()) {
    // Cannot materialize data/error_pages/: fall back to the plain abort so a
    // broken filesystem never turns into a hang.
    errorPageWriter_.reset();
    return false;
  }
  errorPageStatus_ = statusCode;
  errorPageActive_ = true;
  A2_LOG_NOTICE(fmt("CUID#%" PRId64
                    " - capturing HTTP/2 status %d error page",
                    getCuid(), statusCode));
  return true;
}

bool Http2ResponseCommand::consumeErrorPage()
{
  auto recvBuffer = stream_->getSocketRecvBuffer();
  if (recvBuffer->bufferEmpty()) {
    // recv() first moves staged DATA frames into the window buffer.  It only
    // returns 0 when the stream is terminated *and* fully drained.
    if (recvBuffer->recv() == 0) {
      errorPageWriter_.reset();
      abortErrorPageStatus();
    }
    if (recvBuffer->bufferEmpty()) {
      // Stream still open, nothing available right now: park on the consumer
      // seat until the multiplexer wakes us up again.
      addCommandSelf();
      return false;
    }
  }

  const size_t len = recvBuffer->getBufferLength();
  errorPageWriter_->write(recvBuffer->getBuffer(), len);
  recvBuffer->drain(len);

  if (errorPageWriter_->full()) {
    // 256 KiB cap reached: abandon the rest of the stream and surface the
    // original status.  The multiplexer reclaims the abandoned stream's
    // connection-level window when this command drops its consumer seat.
    errorPageWriter_.reset();
    abortErrorPageStatus();
  }

  addCommandSelf();
  return false;
}

void Http2ResponseCommand::abortErrorPageStatus()
{
  errorPageActive_ = false;
  if (errorPageStatus_ == 404) {
    throw DL_ABORT_EX2(MSG_RESOURCE_NOT_FOUND, error_code::RESOURCE_NOT_FOUND);
  }
  throw DL_ABORT_EX2(
      fmt("HTTP/2 response status %d (error page captured)", errorPageStatus_),
      error_code::HTTP_PROTOCOL_ERROR);
}

void Http2ResponseCommand::determineFilename()
{
  auto& file = getRequest()->getFile();
  // Same convention as the HTTP/1.1 path: a request without a file part is
  // stored under Request::DEFAULT_FILE.
  auto suffixPath = util::createSafePath(
      getRequest()->getFile().empty()
          ? Request::DEFAULT_FILE
          : util::percentDecode(std::begin(file), std::end(file)));
  getFileEntry()->setPath(
      util::applyDir(getOption()->get(PREF_DIR), suffixPath));
  getFileEntry()->setSuffixPath(suffixPath);
}

} // namespace aria2
