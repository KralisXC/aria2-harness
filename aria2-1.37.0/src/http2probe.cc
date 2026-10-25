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
//
// Standalone probe for the HTTP/2 transport layer.  Two modes:
//
//   mem     (default) drive Http2Multiplexer by hand: connect + TLS + assert
//           ALPN "h2", open N multiplexed streams, pump the transport until
//           every stream is finished, collect the payloads into memory.  This
//           proves the transport skeleton alone (P-HTTP2-3).
//
//   engine  drive the very same multiplexer from a real DownloadEngine command
//           loop through Http2MultiplexerCommand.  Every stream gets a consumer
//           command (an AbstractCommand subclass) parked on its per-stream
//           Http2StreamRecvBuffer; the multiplexer activates that command from
//           the nghttp2 callbacks.  This proves the heartbeat / wakeup /
//           placeholder socket path (P-HTTP2-4a).
//
//   argv: 1 = URL, 2 = number of streams, 3 = mode ("mem" | "engine").
#include <chrono>
#include <cinttypes>
#include <cstdio>
#include <cstdlib>
#include <ctime>
#include <memory>
#include <string>
#include <vector>

#include "common.h"

#include "AbstractCommand.h"
#include "Command.h"
#include "console.h"
#include "DownloadEngine.h"
#include "Exception.h"
#include "GroupId.h"
#include "Http2Multiplexer.h"
#include "Http2MultiplexerCommand.h"
#include "Http2Stream.h"
#include "Http2StreamRecvBuffer.h"
#include "LogFactory.h"
#include "Logger.h"
#include "Option.h"
#include "OptionParser.h"
#include "Platform.h"
#include "RequestGroup.h"
#include "RequestGroupMan.h"
#include "SelectEventPoll.h"
#include "SocketCore.h"
#include "SocketRecvBuffer.h"
#include "TLSContext.h"
#include "prefs.h"

// Included last on purpose: windows.h defines a pile of macros (min/max, ...)
// which must not be seen by the aria2 headers above.  Needed only for
// GetProcessTimes(), i.e. a *real* CPU counter - std::clock() on MinGW returns
// wall clock, which would make every cpu/wall ratio trivially 1.0.
#ifdef _WIN32
#  include <windows.h>
#endif // _WIN32

namespace {

using namespace aria2;

const size_t DEFAULT_STREAMS = 4;
const time_t HANDSHAKE_TIMEOUT = 20;
const time_t TRANSFER_TIMEOUT = 90;

// Real CPU time (user + kernel) consumed by this process so far, in seconds.
// On Windows std::clock() is useless for this: MinGW maps it to wall time.
double probeCpuSeconds()
{
#ifdef _WIN32
  FILETIME creation, exit, kernel, user;
  if (!GetProcessTimes(GetCurrentProcess(), &creation, &exit, &kernel, &user)) {
    return -1.0;
  }
  ULARGE_INTEGER k, u;
  k.LowPart = kernel.dwLowDateTime;
  k.HighPart = kernel.dwHighDateTime;
  u.LowPart = user.dwLowDateTime;
  u.HighPart = user.dwHighDateTime;
  // FILETIME counts 100 ns ticks.
  return (double(k.QuadPart) + double(u.QuadPart)) / 1.0e7;
#else  // !_WIN32
  return double(std::clock()) / CLOCKS_PER_SEC;
#endif // !_WIN32
}

bool parseUrl(const std::string& url, std::string& host, uint16_t& port,
              std::string& authority, std::string& path)
{
  const std::string prefix = "https://";
  if (url.compare(0, prefix.size(), prefix) != 0) {
    fprintf(stderr, "[probe] only https:// URLs are supported: %s\n",
            url.c_str());
    return false;
  }
  size_t start = prefix.size();
  size_t slash = url.find('/', start);
  authority = url.substr(start, slash == std::string::npos
                                    ? std::string::npos
                                    : slash - start);
  path = (slash == std::string::npos) ? std::string("/") : url.substr(slash);

  size_t colon = authority.find(':');
  if (colon == std::string::npos) {
    host = authority;
    port = 443;
  }
  else {
    host = authority.substr(0, colon);
    port = static_cast<uint16_t>(atoi(authority.substr(colon + 1).c_str()));
  }
  return !host.empty();
}

// Returns the expected payload length (0 when unknown) for URLs of the shape
// .../bytes/<N> served by httpbin-style endpoints.
size_t parseExpectedBytes(const std::string& path)
{
  const std::string marker = "/bytes/";
  size_t p = path.rfind(marker);
  if (p == std::string::npos) {
    return 0;
  }
  return static_cast<size_t>(
      strtoull(path.c_str() + p + marker.size(), nullptr, 10));
}

// Connect + TLS handshake + assert ALPN "h2".  Shared by both modes; returns
// the ready transport socket, or nullptr on any failure.
std::shared_ptr<aria2::SocketCore> connectH2(const std::string& host,
                                            uint16_t port)
{
#ifdef ENABLE_SSL
  // Client TLS context.  Host certificate verification is disabled so that
  // the probe cannot fail for reasons unrelated to the transport layer.
  // WinTLSContext only accepts TLS_PROTO_TLS11 / TLS_PROTO_TLS12 as the
  // minimum version (TLS_PROTO_NONE asserts and aborts).
  printf("[probe] step: TLSContext::make\n");
  std::shared_ptr<TLSContext> tlsContext(
      TLSContext::make(TLS_CLIENT, TLS_PROTO_TLS12));
  printf("[probe] step: addSystemTrustedCACerts\n");
  tlsContext->addSystemTrustedCACerts();
  printf("[probe] step: setVerifyPeer(false)\n");
  tlsContext->setVerifyPeer(false);
  printf("[probe] step: setClientTLSContext\n");
  SocketCore::setClientTLSContext(tlsContext);
#else  // !ENABLE_SSL
  fprintf(stderr, "[probe] built without TLS support\n");
  return nullptr;
#endif // !ENABLE_SSL

  printf("[probe] step: create socket\n");
  auto socket = std::make_shared<SocketCore>();
  printf("[probe] step: establishConnection\n");
  socket->establishConnection(host, port);

  // establishConnection() only starts a non-blocking connect; wait until it
  // completes before starting the TLS handshake.
  time_t deadline = time(nullptr) + HANDSHAKE_TIMEOUT;
  while (!socket->isWritable(1)) {
    if (time(nullptr) >= deadline) {
      fprintf(stderr, "[probe] connect timed out\n");
      return nullptr;
    }
  }

#ifdef ENABLE_SSL
  bool handshaked = false;
  deadline = time(nullptr) + HANDSHAKE_TIMEOUT;
  while (time(nullptr) < deadline) {
    if (socket->tlsConnect(host)) {
      handshaked = true;
      break;
    }
    if (socket->wantWrite() || (!socket->wantRead() && !socket->wantWrite())) {
      socket->isWritable(1);
    }
    else {
      socket->isReadable(1);
    }
  }
  if (!handshaked) {
    fprintf(stderr, "[probe] TLS handshake timed out\n");
    return nullptr;
  }

  std::string alpn = socket->getAlpnProtocol();
  printf("[probe] ALPN negotiated: '%s'\n", alpn.c_str());
  if (alpn != "h2") {
    fprintf(stderr,
            "[probe] FAILED: server did not negotiate HTTP/2 over ALPN\n");
    return nullptr;
  }
#endif // ENABLE_SSL
  return socket;
}

// ---------------------------------------------------------------------------
// Mode "mem": the multiplexer is pumped by hand, no Command / DownloadEngine.
// ---------------------------------------------------------------------------
int runMemoryProbe(const std::shared_ptr<aria2::SocketCore>& socket,
                   const std::string& authority, const std::string& path,
                   size_t expected, size_t streamCount)
{
  Http2Multiplexer mux;
  mux.attachSocket(socket);

  Http2RequestHead head;
  head.scheme = "https";
  head.authority = authority;
  head.path = path;
  head.headers.push_back({"user-agent", "aria2-harness-http2probe/0.1"});
  head.headers.push_back({"accept", "*/*"});

  std::vector<std::shared_ptr<Http2Stream>> streams;
  std::vector<std::string> bodies(streamCount);
  for (size_t i = 0; i < streamCount; ++i) {
    // cuid is irrelevant for the probe, but keep it distinct so that the
    // cuid -> stream mapping is exercised too.
    auto stream = mux.openStream(static_cast<cuid_t>(i + 1), head);
    if (!stream) {
      fprintf(stderr, "[probe] failed to open stream %zu\n", i);
      return 1;
    }
    streams.push_back(stream);
  }
  printf("[probe] %zu streams submitted\n", mux.numStreams());

  bool failed = false;
  bool allDone = false;
  time_t deadline = time(nullptr) + TRANSFER_TIMEOUT;

  while (!allDone && time(nullptr) < deadline) {
    bool wantRead = mux.wantRead();
    bool flushed = mux.flushToSocket();
    if (!flushed) {
      // Some bytes could not be written yet; wait for writability.
      socket->isWritable(1);
    }

    if (wantRead) {
      if (socket->isReadable(1)) {
        auto result = mux.readFromSocket();
        if (result == Http2Multiplexer::PUMP_ERROR) {
          fprintf(stderr, "[probe] transport read error\n");
          failed = true;
          break;
        }
        if (result == Http2Multiplexer::PUMP_EOF) {
          printf("[probe] transport EOF\n");
          break;
        }
      }
    }

    // Consume whatever the multiplexer injected into each stream buffer.
    // recv() first refills the 16KiB window from the stream's staging area
    // and returns 0 only when the stream is authoritatively finished.
    // A stream is only "done" once it is terminated AND its staging area
    // has been drained completely: END_STREAM can arrive in the same batch
    // as the last DATA chunks, and those still have to be consumed.
    allDone = true;
    for (size_t i = 0; i < streams.size(); ++i) {
      auto& stream = streams[i];
      auto buffer = stream->getSocketRecvBuffer();
      buffer->recv();
      size_t len = buffer->getBufferLength();
      if (len > 0) {
        bodies[i].append(reinterpret_cast<const char*>(buffer->getBuffer()),
                         len);
        buffer->drain(len);
      }
      if (!stream->terminated() || stream->getPendingBytes() != 0) {
        allDone = false;
      }
    }
  }

  if (!allDone) {
    fprintf(stderr, "[probe] FAILED: transfer did not complete in time\n");
    failed = true;
  }

  size_t total = 0;
  for (size_t i = 0; i < streams.size(); ++i) {
    total += bodies[i].size();
    printf("[probe] stream %zu: streamId=%d status=%d bytes=%zu"
           " endStream=%d closed=%d\n",
           i, streams[i]->getStreamId(), streams[i]->getStatusCode(),
           bodies[i].size(), streams[i]->getEndStreamReceived() ? 1 : 0,
           streams[i]->getClosed() ? 1 : 0);
    if (expected != 0 && bodies[i].size() != expected) {
      fprintf(stderr,
              "[probe] FAILED: stream %zu expected %zu bytes but got %zu\n", i,
              expected, bodies[i].size());
      failed = true;
    }
    if (streams[i]->getStatusCode() != 200) {
      fprintf(stderr, "[probe] FAILED: stream %zu status is not 200\n", i);
      failed = true;
    }
  }
  printf("[probe] total bytes downloaded into memory: %zu\n", total);

  if (failed) {
    return 1;
  }
  printf("[probe] SUCCESS\n");
  return 0;
}

// ---------------------------------------------------------------------------
// Mode "engine": the multiplexer is driven by Http2MultiplexerCommand inside a
// real DownloadEngine command loop.
// ---------------------------------------------------------------------------

// Bookkeeping shared between the consumer commands (which run *inside* the
// engine loop) and the driver loop which owns them.  The consumers must not
// halt the engine themselves: the driver defers the halt until the idle window
// below has been measured, and only then retires the resident multiplexer.
struct EngineRunState {
  size_t finished = 0;
  bool allDone = false;
  std::chrono::steady_clock::time_point startTime;
  std::chrono::steady_clock::time_point allDoneAt;
};

// Consumer command standing in for DownloadCommand (P-HTTP2-5 will use the real
// one).  Its only job here is to prove the wakeup path:
//
//   * it is parked INACTIVE, so the command loop only revisits it when
//     Http2Multiplexer::notifyWakeup() activated it (or on the 1s refresh);
//   * the placeholder socket (fd == -1) is handed to AbstractCommand, whose
//     ctor skips setReadCheckSocket() for a socket which is not open - so
//     checkSocketIsReadable_ stays false and shouldProcess() is unconditionally
//     true.  Nothing bogus ever reaches select();
//   * recv() returning 0 is only ever an authoritative EOF (END_STREAM or
//     stream close), so no DL_RETRY_EX(EX_GOT_EOF) can be triggered here.
class ProbeConsumerCommand : public AbstractCommand {
public:
  ProbeConsumerCommand(cuid_t cuid, const std::shared_ptr<Http2Stream>& stream,
                       RequestGroup* group, DownloadEngine* e,
                       std::string* body, EngineRunState* state, size_t total)
      : AbstractCommand(cuid, std::shared_ptr<Request>(),
                        std::shared_ptr<FileEntry>(), group, e,
                        stream->getSocketRecvBuffer()->getSocket(),
                        stream->getSocketRecvBuffer(),
                        false /* incNumConnection */),
        stream_(stream), body_(body), state_(state), total_(total)
  {
    stream_->getSocketRecvBuffer()->setConsumer(this);
  }

  virtual ~ProbeConsumerCommand() CXX11_OVERRIDE
  {
    // Identity checked release: the registration must never be wiped by a
    // command which did not install it.
    stream_->getSocketRecvBuffer()->clearConsumer(this);
  }

protected:
  virtual bool executeInternal() CXX11_OVERRIDE
  {
    auto buffer = stream_->getSocketRecvBuffer();

    // Same consumption pattern as DownloadCommand: ask the buffer what the
    // stream has (recv() refills the 16KiB window from the staging area
    // first), then drain exactly that much.
    buffer->recv();
    size_t len = buffer->getBufferLength();
    if (len > 0) {
      body_->append(reinterpret_cast<const char*>(buffer->getBuffer()), len);
      buffer->drain(len);
    }

    if (stream_->terminated() && stream_->getPendingBytes() == 0) {
      ++state_->finished;
      auto now = std::chrono::steady_clock::now();
      double ms =
          std::chrono::duration<double, std::milli>(now - state_->startTime)
              .count();
      printf("[engine] cuid=%" PRId64 " stream=%d finished: %zu bytes,"
             " %.1f ms after start (%zu/%zu done)\n",
             getCuid(), stream_->getStreamId(), body_->size(), ms,
             state_->finished, total_);
      fflush(stdout);
      if (state_->finished == total_) {
        // Every stream is done.  Deliberately *no* getDownloadEngine()->
        // requestHalt() here: the driver loop keeps the engine running for a
        // few seconds to measure the idle window (see runEngineProbe).  The
        // multiplexer stays resident until the driver halts it.
        state_->allDone = true;
        state_->allDoneAt = now;
      }
      return true;
    }

    // No data left and the stream is still open: park and wait for the
    // multiplexer to activate this command again.
    addCommandSelf();
    return false;
  }

private:
  std::shared_ptr<Http2Stream> stream_;
  std::string* body_;
  EngineRunState* state_;
  size_t total_;

  ProbeConsumerCommand(const ProbeConsumerCommand&);
  ProbeConsumerCommand& operator=(const ProbeConsumerCommand&);
};

int runEngineProbe(const std::shared_ptr<aria2::SocketCore>& socket,
                   const std::string& authority, const std::string& path,
                   size_t expected, size_t streamCount)
{
  // A fully populated Option, exactly as aria2c starts up with.  DRY_RUN keeps
  // RequestGroup from instantiating the pre/post download handlers, which have
  // nothing to do with this probe.
  auto option = std::make_shared<Option>();
  OptionParser::getInstance()->parseDefaultValues(*option);
  option->put(PREF_DRY_RUN, A2_V_TRUE);

  printf("[engine] step: DownloadEngine\n");
  DownloadEngine e(std::make_unique<SelectEventPoll>());
  e.setOption(option.get());
  e.setRequestGroupMan(std::make_unique<RequestGroupMan>(
      std::vector<std::shared_ptr<RequestGroup>>(), 1, option.get()));

  auto group = std::make_shared<RequestGroup>(GroupId::create(), option);

  auto mux = std::make_shared<Http2Multiplexer>();
  mux->attachSocket(socket);
  // The single multiplexer of the group (exercises the one-instance holder).
  group->setHttp2Multiplexer(mux);

  std::vector<std::string> bodies(streamCount);
  std::vector<std::shared_ptr<Http2Stream>> streams;
  EngineRunState state;
  state.startTime = std::chrono::steady_clock::now();

  Http2RequestHead head;
  head.scheme = "https";
  head.authority = authority;
  head.path = path;
  head.headers.push_back({"user-agent", "aria2-harness-http2probe/0.1"});
  head.headers.push_back({"accept", "*/*"});

  // The heartbeat must be queued first: it is the only holder of the transport
  // fd's read registration.
  printf("[engine] step: Http2MultiplexerCommand\n");
  e.addCommand(std::unique_ptr<Command>(
      new Http2MultiplexerCommand(e.newCUID(), mux, &e)));

  for (size_t i = 0; i < streamCount; ++i) {
    cuid_t cuid = e.newCUID();
    auto stream = mux->openStream(cuid, head);
    if (!stream) {
      fprintf(stderr, "[engine] failed to open stream %zu\n", i);
      return 1;
    }
    streams.push_back(stream);
    e.addCommand(std::unique_ptr<Command>(new ProbeConsumerCommand(
        cuid, stream, group.get(), &e, &bodies[i], &state, streamCount)));
  }
  printf("[engine] %zu streams submitted, %zu consumer commands queued\n",
         mux->numStreams(), streamCount);
  fflush(stdout);

  printf("[engine] entering command loop\n");
  fflush(stdout);

  // Idle window.  Once every stream has finished, the resident multiplexer is
  // the only command left.  Keep the engine running for IDLE_WINDOW_SEC with
  // nothing to do and measure the CPU burnt in that window: if the loop really
  // blocks in select() the number stays near zero, if it busy-polls it climbs
  // towards the wall time.  The halt is deliberately issued *after* the
  // measurement, because the multiplexer retires on halt.
  const double IDLE_WINDOW_SEC = 2.5;

  double cpuStart = probeCpuSeconds();
  double idleCpuStart = cpuStart;
  bool idleStarted = false;
  bool haltRequested = false;
  time_t deadline = time(nullptr) + TRANSFER_TIMEOUT;
  int rc = 0;
  for (;;) {
    // oneshot: run() returns 1 while there is still work and 0 when both
    // command queues are drained.  The outer deadline bounds a stalled peer.
    rc = e.run(true);
    if (rc == 0) {
      break;
    }
    if (time(nullptr) >= deadline) {
      fprintf(stderr, "[engine] FAILED: command loop timed out\n");
      return 1;
    }
    if (state.allDone && !idleStarted) {
      idleStarted = true;
      idleCpuStart = probeCpuSeconds();
      printf("[engine] all streams done; entering %.1f s idle window\n",
             IDLE_WINDOW_SEC);
      fflush(stdout);
    }
    if (idleStarted && !haltRequested) {
      double idleElapsed = std::chrono::duration<double>(
                               std::chrono::steady_clock::now() -
                               state.allDoneAt)
                               .count();
      if (idleElapsed >= IDLE_WINDOW_SEC) {
        haltRequested = true;
        e.requestHalt();
      }
    }
  }

  auto endTime = std::chrono::steady_clock::now();
  double wallSec =
      std::chrono::duration<double>(endTime - state.startTime).count();
  double cpuSec = probeCpuSeconds() - cpuStart;
  printf("[engine] loop exited: %.3f s wall, %.3f s cpu (cpu/wall=%.2f)\n",
         wallSec, cpuSec, wallSec > 0 ? cpuSec / wallSec : 0.0);
  if (wallSec > 0.0001 && cpuSec / wallSec < 0.95) {
    printf("[engine] the loop blocked in select() rather than spinning\n");
  }
  if (idleStarted) {
    double idleWallSec =
        std::chrono::duration<double>(endTime - state.allDoneAt).count();
    double idleCpuSec = probeCpuSeconds() - idleCpuStart;
    printf("[engine] idle window: %.2f s wall, %.4f s cpu (cpu/wall=%.4f)%s\n",
           idleWallSec, idleCpuSec,
           idleWallSec > 0 ? idleCpuSec / idleWallSec : 0.0,
           idleWallSec >= IDLE_WINDOW_SEC
               ? " -- engine blocked in select(), not spinning"
               : " -- window cut short (engine drained early)");
  }

  bool failed = false;
  size_t total = 0;
  for (size_t i = 0; i < streams.size(); ++i) {
    total += bodies[i].size();
    printf("[engine] stream %zu: streamId=%d status=%d bytes=%zu"
           " endStream=%d closed=%d\n",
           i, streams[i]->getStreamId(), streams[i]->getStatusCode(),
           bodies[i].size(), streams[i]->getEndStreamReceived() ? 1 : 0,
           streams[i]->getClosed() ? 1 : 0);
    if (expected != 0 && bodies[i].size() != expected) {
      fprintf(stderr,
              "[engine] FAILED: stream %zu expected %zu bytes but got %zu\n", i,
              expected, bodies[i].size());
      failed = true;
    }
    if (streams[i]->getStatusCode() != 200) {
      fprintf(stderr, "[engine] FAILED: stream %zu status is not 200\n", i);
      failed = true;
    }
  }
  printf("[engine] total bytes consumed: %zu (finished=%zu/%zu)\n", total,
         state.finished, streamCount);

  if (failed || state.finished != streamCount) {
    fprintf(stderr, "[engine] FAILED\n");
    return 1;
  }
  printf("[engine] SUCCESS\n");
  return 0;
}

} // namespace

int main(int argc, char** argv)
{
  using namespace aria2;

  // Unbuffered stdout/stderr so that any crash still leaves a trace behind.
  setvbuf(stdout, nullptr, _IONBF, 0);
  setvbuf(stderr, nullptr, _IONBF, 0);

  // Must be done before any A2_LOG_* statement: the Logger constructor reads
  // global::cout()->supportsColor(), and global::cout() is a null shared_ptr
  // until initConsole() has run.  aria2c's main() does this first as well.
  global::initConsole(false);

  std::string url =
      argc > 1 ? argv[1] : "https://nghttp2.org/httpbin/bytes/1048576";
  size_t streamCount =
      argc > 2 ? static_cast<size_t>(atoi(argv[2])) : DEFAULT_STREAMS;
  std::string mode = argc > 3 ? argv[3] : "mem";
  if (streamCount == 0) {
    streamCount = 1;
  }

  std::string host, authority, path;
  uint16_t port = 443;
  if (!parseUrl(url, host, port, authority, path)) {
    return 1;
  }
  size_t expected = parseExpectedBytes(path);

  LogFactory::setConsoleOutput(true);
  LogFactory::setConsoleLogLevel(Logger::A2_WARN);

  // SocketCore relies on WSAStartup()/global initialisation performed by
  // Platform, exactly like aria2c's main() does.
  Platform::setUp();

  printf("[probe] target=%s host=%s port=%u streams=%zu expected=%zu mode=%s\n",
         url.c_str(), host.c_str(), port, streamCount, expected, mode.c_str());

  try {
    auto socket = connectH2(host, port);
    if (!socket) {
      return 1;
    }
    if (mode == "engine") {
      return runEngineProbe(socket, authority, path, expected, streamCount);
    }
    return runMemoryProbe(socket, authority, path, expected, streamCount);
  }
  catch (const Exception& e) {
    fprintf(stderr, "[probe] aria2 exception: %s\n", e.what());
    return 1;
  }
  catch (const std::exception& e) {
    fprintf(stderr, "[probe] std exception: %s\n", e.what());
    return 1;
  }
}
