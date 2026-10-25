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
#ifndef D_HTTP2_MULTIPLEXER_COMMAND_H
#define D_HTTP2_MULTIPLEXER_COMMAND_H

#include "common.h"

#include <memory>

#include "Command.h" // cuid_t
#include "wallclock.h" // Timer

namespace aria2 {

class DownloadEngine;
class Http2Multiplexer;
class SocketCore;

// Resident command driving the single HTTP/2 transport socket.
//
// It is a *regular* command (owned by DownloadEngine::commands_), not a
// routine command.  DownloadEngine::run() only calls waitData() when
// commands_ is non-empty, so a mux parked in routineCommands_ would make the
// main loop spin without ever blocking in select().
//
// Responsibilities:
//   * owns the one and only event registration of the transport fd (the
//     per-stream placeholder sockets have fd == -1 and never reach the poll);
//   * flushes serialized nghttp2 frames once per iteration, which also tops
//     the flow control windows up from the consumer side
//     (Http2Multiplexer::flushToSocket());
//   * pulls bytes from the socket when select() reported it readable and feeds
//     them into nghttp2, whose callbacks then activate (setStatusActive) the
//     per-stream consumer commands.
class Http2MultiplexerCommand : public Command {
private:
  std::shared_ptr<Http2Multiplexer> mux_;
  DownloadEngine* e_;
  std::shared_ptr<SocketCore> readCheckSocket_;
  std::shared_ptr<SocketCore> writeCheckSocket_;

  // Idle retirement bookkeeping (R-17).  Under --enable-rpc the engine keeps
  // running after the last download finished, and this command is deliberately
  // resident for as long as that is true, so nothing else would ever give the
  // transport socket back: after the owning RequestGroup died,
  // RequestGroupMan::downloadFinished() stays false forever and the fd stayed
  // registered until the *peer* closed the idle connection (~75s keep-alive
  // timeout) - a window in which the fd, the command slot and the transport
  // state were held hostage.  We therefore retire the transport ourselves once
  // it has demonstrably gone idle.
  //
  // servedStream_ guards the two cases which must NOT look idle:
  //   * the command is queued *before* Http2RequestCommand opens the first
  //     stream, so numStreams() is legitimately 0 on the first iterations;
  //   * while the group is being rebuilt (retry / resume / mirror switch) the
  //     stream count dips to 0 for a moment between two connections.
  // Only after this command has actually carried a stream do we start the idle
  // timer, and it is restarted from scratch whenever the transport carries
  // data again (numStreams() > 0) or still has frames to flush.
  bool servedStream_;
  bool idleTimerRunning_;
  Timer idleStart_;

  void registerReadCheck();
  void unregisterReadCheck();
  void updateWriteCheck();
  void unregisterSocketChecks();

public:
  Http2MultiplexerCommand(cuid_t cuid,
                          const std::shared_ptr<Http2Multiplexer>& mux,
                          DownloadEngine* e);

  virtual ~Http2MultiplexerCommand();

  virtual bool execute() CXX11_OVERRIDE;

private:
  Http2MultiplexerCommand(const Http2MultiplexerCommand&);
  Http2MultiplexerCommand& operator=(const Http2MultiplexerCommand&);
};

} // namespace aria2

#endif // D_HTTP2_MULTIPLEXER_COMMAND_H
