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
#include "Http2MultiplexerCommand.h"

#include <memory>

#include "DownloadEngine.h"
#include "Http2Multiplexer.h"
#include "LogFactory.h"
#include "Logger.h"
#include "RequestGroupMan.h"
#include "SocketCore.h"
#include "fmt.h"

namespace aria2 {

namespace {
// R-17: how long a transport which has already carried at least one stream may
// stay without any stream and without pending frames before we tear it down
// ourselves.  Long enough to survive a retry / resume / mirror switch cycle
// (those complete within a few seconds), short enough to actually reclaim the
// fd, the command slot and the TLS session of a group which is gone or paused.
constexpr auto IDLE_RETIRE_TIMEOUT = std::chrono::seconds(60);
} // namespace

Http2MultiplexerCommand::Http2MultiplexerCommand(
    cuid_t cuid, const std::shared_ptr<Http2Multiplexer>& mux, DownloadEngine* e)
    : Command(cuid),
      mux_(mux),
      e_(e),
      servedStream_(false),
      idleTimerRunning_(false)
{
  // REALTIME: flushToSocket() has to run on *every* iteration.  HEADERS frames
  // submitted by the commands executed in this iteration must leave the
  // process now; otherwise they would sit in Http2Multiplexer::pendingWrite_
  // until some unrelated event happens to arrive.
  setStatusRealtime();

  registerReadCheck();

  // Engine side half of the wakeup path.  Http2Multiplexer::notifyWakeup()
  // calls this after it activated the consumer command, so that the *next*
  // iteration does not block for refreshInterval before draining the data
  // which just arrived.
  mux_->setWakeupCallback([this](cuid_t) { e_->setNoWait(true); });

  // Engine side half of the teardown path.  Http2Multiplexer::retire() invokes
  // this *before* it closes the transport socket, which is the only safe
  // order: SelectEventPoll::socketEntries_ is keyed by the raw fd, so a socket
  // closed first would leave an entry behind pointing at this command.
  mux_->setDetachCallback([this]() { unregisterSocketChecks(); });
}

Http2MultiplexerCommand::~Http2MultiplexerCommand()
{
  // Order matters.  Deregister from the event poll first: EventPoll's
  // socketEntries_ would otherwise keep a pointer to this object and hand it a
  // readable event after it is gone.  Then drop the callbacks, which close over
  // |this| and may outlive the command through
  // RequestGroup::getHttp2Multiplexer().
  unregisterSocketChecks();
  if (mux_) {
    mux_->setWakeupCallback(Http2Multiplexer::WakeupCallback());
    mux_->setDetachCallback(Http2Multiplexer::DetachCallback());
  }
}

void Http2MultiplexerCommand::registerReadCheck()
{
  auto socket = mux_ ? mux_->getSocket() : nullptr;
  if (!socket || !socket->isOpen()) {
    A2_LOG_ERROR(fmt("CUID#%" PRId64
                     " - HTTP/2 multiplexer has no transport socket;"
                     " inbound frames cannot be received",
                     getCuid()));
    return;
  }
  if (!e_->addSocketForReadCheck(socket, this)) {
    A2_LOG_ERROR(fmt("CUID#%" PRId64
                     " - failed to register the HTTP/2 transport fd for read",
                     getCuid()));
    return;
  }
  readCheckSocket_ = socket;
}

void Http2MultiplexerCommand::unregisterReadCheck()
{
  if (readCheckSocket_) {
    e_->deleteSocketForReadCheck(readCheckSocket_, this);
    readCheckSocket_.reset();
  }
}

void Http2MultiplexerCommand::unregisterSocketChecks()
{
  unregisterReadCheck();
  if (writeCheckSocket_) {
    e_->deleteSocketForWriteCheck(writeCheckSocket_, this);
    writeCheckSocket_.reset();
  }
}

void Http2MultiplexerCommand::updateWriteCheck()
{
  auto socket = mux_ ? mux_->getSocket() : nullptr;

  if (mux_ && mux_->hasPendingWrite() && socket && socket->isOpen()) {
    // The socket refused part of the serialized frames.  The command is
    // REALTIME so it would retry within one refresh interval anyway, but a
    // write event lets us resume immediately instead of stalling up to 1s.
    if (!writeCheckSocket_) {
      if (e_->addSocketForWriteCheck(socket, this)) {
        writeCheckSocket_ = socket;
      }
    }
    return;
  }

  if (writeCheckSocket_) {
    e_->deleteSocketForWriteCheck(writeCheckSocket_, this);
    writeCheckSocket_.reset();
  }
}

bool Http2MultiplexerCommand::execute()
{
  // Retirement conditions, in order of appearance:
  //   * engine shutdown;
  //   * the multiplexer was retired by whoever owned it
  //     (HttpRequestCommand::switchToHttp2() rebuilding the transport after a
  //     pause/resume, or the transport failing);
  //   * the transport socket is gone: aria2 closes the shared SocketCore when
  //     the owning RequestGroup is halted (pause), so this is the signal that
  //     makes a paused group's multiplexer tear itself down *before* resume
  //     reuses the same SocketCore object from the pool;
  //   * the request queue is drained: the engine's exit protocol requires every
  //     resident command to self-destruct once RequestGroupMan::downloadFinished()
  //     holds, otherwise commands_ never empties and DownloadEngine::run() can
  //     never return.  Same判据 as the upstream resident commands.  Because this
  //     command owns the group's transport socket, self-destructing here is also
  //     what reclaims the multiplexer and its socket after a completed download
  //     (under --enable-rpc keepRunning_ keeps downloadFinished() false, so the
  //     daemon's transport survives as before).
  //
  // Note: this command must *not* be retired on numStreams() == 0.  The command
  // is queued before the Http2RequestCommand which opens the first stream, so
  // on its very first execution the stream count is legitimately zero;
  // retiring there would drop the transport fd's read registration and leave
  // every later stream parked forever (no producer could ever deliver a byte
  // again).
  auto transport = mux_ ? mux_->getSocket() : nullptr;
  auto& rgman = e_->getRequestGroupMan();
  if (e_->isHaltRequested() || (rgman && rgman->downloadFinished()) || !mux_ ||
      mux_->isRetired() || !transport || !transport->isOpen()) {
    if (mux_) {
      // Wakes every consumer command (they run their own retry/abort path),
      // drops all stream bookkeeping and closes the socket.  The detach
      // callback installed in the constructor removes this command's event
      // registration before the fd is closed.
      mux_->retire(NGHTTP2_CANCEL);
    }
    unregisterSocketChecks();
    return true;
  }

  // Reclaim the streams whose consumer command is gone, *before* the outbound
  // pass.  Reclaiming a stream issues credit for every byte the peer sent on it
  // (drained or not) plus a RST_STREAM for an abandoned one; running this first
  // is what makes that WINDOW_UPDATE and RST_STREAM leave the process in this
  // very iteration.  Without it, an abandoned 2 MiB body would keep the peer
  // pushing into a stream nobody reads, and the other streams of this
  // connection would stay starved until some unrelated event woke the loop.
  mux_->purgeFinishedStreams();

  // R-17: idle retirement.  Runs *after* the reclaim pass so numStreams()
  // reflects the streams which just finished rather than the count of the
  // previous iteration.  A transport is idle when it neither carries a stream
  // nor has serialized frames waiting for the socket, and it has to have served
  // at least one stream before the timer may start (see the guards in
  // Http2MultiplexerCommand.h - otherwise the window between "command queued"
  // and "first stream opened" would look idle).
  if (mux_->numStreams() > 0) {
    servedStream_ = true;
    idleTimerRunning_ = false;
  }
  else if (servedStream_ && !mux_->hasPendingWrite()) {
    if (!idleTimerRunning_) {
      idleTimerRunning_ = true;
      idleStart_ = global::wallclock();
    }
    else if (idleStart_.difference(global::wallclock()) >= IDLE_RETIRE_TIMEOUT) {
      A2_LOG_INFO(fmt("CUID#%" PRId64
                      " - HTTP/2 transport idle for %lds; retiring the"
                      " multiplexer",
                      getCuid(), static_cast<long>(IDLE_RETIRE_TIMEOUT.count())));
      // Same ordered teardown as the read-failure path below: retire()
      // unregisters this fd from the event poll before closing the socket and
      // wakes every consumer, then the command drops out of commands_ by
      // returning true.  RequestGroup::getHttp2Multiplexer() keeps handing out
      // the retired object, but HttpRequestCommand::switchToHttp2() detects
      // isRetired() and rebuilds a fresh transport, so a later download (or a
      // resume) is not affected by this teardown.
      mux_->retire(NGHTTP2_CANCEL);
      unregisterSocketChecks();
      return true;
    }
  }

  // Outbound pass: serialize whatever nghttp2 wants to send and push it out.
  // This also issues the flow control credit accumulated by the consumers
  // since the previous iteration, and the frames submitted by the reclaim pass
  // above.
  mux_->flushToSocket();

  // Inbound pass: entirely event driven.  readEvent_ is cleared by
  // DownloadEngine::executeCommand() once this call returned false, so it is
  // set only for the iteration which really observed a readable fd.
  if (readEventEnabled()) {
    auto result = mux_->readFromSocket();
    if (result == Http2Multiplexer::PUMP_EOF ||
        result == Http2Multiplexer::PUMP_ERROR) {
      // PUMP_EOF is the *normal* end of an idle keep-alive transport: the peer
      // (or a CDN in front of it) closes the connection after its own idle
      // timeout.  It is neither an error nor worth an ERROR level entry;
      // PUMP_ERROR is a real transport failure.  Both end the same way.
      if (result == Http2Multiplexer::PUMP_EOF) {
        A2_LOG_INFO(fmt("CUID#%" PRId64
                        " - HTTP/2 transport reached EOF; shutting down the"
                        " multiplexer",
                        getCuid()));
      }
      else {
        A2_LOG_ERROR(fmt("CUID#%" PRId64
                         " - HTTP/2 transport terminated (result=%d);"
                         " closing all streams",
                         getCuid(), static_cast<int>(result)));
      }
      // retire() performs the full teardown this path needs: it unregisters
      // this command's fd from the event poll *before* closing the socket,
      // finishes every stream (so no consumer command stays parked on a dead
      // connection), gives the flow control credit back and marks the
      // multiplexer unusable.  Merely terminating the streams used to leave
      // retired_ == false with the socket still open, so
      // RequestGroup::getHttp2Multiplexer() could hand this dead transport
      // back to HttpInitiateConnectionCommand's fast path - every later stream
      // of the group would then park forever on a connection no event can ever
      // wake up again.
      mux_->retire(NGHTTP2_INTERNAL_ERROR);
      unregisterSocketChecks();
      return true;
    }
  }

  updateWriteCheck();

  // Re-assert REALTIME.  SelectEventPoll::CommandEvent::processEvents() calls
  // setStatusActive() on *every* command whose fd reported an event, which
  // downgrades this command to ACTIVE and, after transitStatus(), to INACTIVE.
  // A resident command which has to run on each iteration must therefore put
  // its own status back: otherwise flushToSocket() - and with it the flow
  // control credit the consumers produced - would only run once per refresh
  // tick (1s), degrading multiplexing to a 1 Hz poll.
  setStatusRealtime();

  // Resident command: hand the ownership of this object over to the command
  // queue and let executeCommand() release its handle.  Returning false means
  // "still alive" (executeCommand() would destroy us on true).
  e_->addCommand(std::unique_ptr<Command>(this));
  return false;
}

} // namespace aria2
