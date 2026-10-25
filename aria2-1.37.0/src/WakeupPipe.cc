/* <!-- copyright */
/*
 * aria2 - The high speed download utility
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
#include "WakeupPipe.h"

#include <cstring>

#include "DlAbortEx.h"
#include "fmt.h"
#include "LogFactory.h"
#include "Logger.h"
#include "util.h"

// POSIX 上 MSG_NOSIGNAL 让 send() 在写端对端已关闭时返回 EPIPE 而不是投递
// SIGPIPE。aria2 的 main 里已把 SIGPIPE 设成 SIG_IGN（MultiUrlRequestInfo.cc
// 380-382），这里再加一层保险，兼顾不支持该 flag 的平台。
#ifndef MSG_NOSIGNAL
#  define MSG_NOSIGNAL 0
#endif // !MSG_NOSIGNAL

namespace aria2 {

namespace {

const sock_t BAD_FD = (sock_t)-1;

std::string lastSocketError()
{
#ifdef __MINGW32__
  int errNum = WSAGetLastError();
  auto msg = util::formatLastError(errNum);
  if (!msg.empty()) {
    return msg;
  }
  return fmt("socket error %d", errNum);
#else  // !__MINGW32__
  return util::safeStrerror(errno);
#endif // !__MINGW32__
}

void makeNonBlocking(sock_t fd)
{
#ifdef __MINGW32__
  u_long flag = 1;
  if (::ioctlsocket(fd, FIONBIO, &flag) != 0) {
    throw DL_ABORT_EX(
        fmt("Failed to make wakeup pipe non-blocking: %s",
            lastSocketError().c_str()));
  }
#else  // !__MINGW32__
  int flags;
  do {
    flags = fcntl(fd, F_GETFL, 0);
  } while (flags == -1 && errno == EINTR);
  if (flags == -1) {
    throw DL_ABORT_EX(fmt("Failed to get wakeup pipe fd flags: %s",
                          util::safeStrerror(errno).c_str()));
  }
  int r;
  do {
    r = fcntl(fd, F_SETFL, flags | O_NONBLOCK);
  } while (r == -1 && errno == EINTR);
  if (r == -1) {
    throw DL_ABORT_EX(fmt("Failed to make wakeup pipe non-blocking: %s",
                          util::safeStrerror(errno).c_str()));
  }
#endif // !__MINGW32__
}

#ifdef __MINGW32__
// Windows 没有 socketpair(2)。用 127.0.0.1 回环连接模拟一对全双工套接字：
//   listener 绑 127.0.0.1:0（端口由内核分配）→ getsockname 取回真实端口
//   → listen(1) → client connect() 到该端口 → accept() 拿到 server 端。
// 因为 listen() 在 connect() 之前完成，回环 connect 会立即成功返回，
// 不依赖 backlog，也不需要非阻塞 connect 的错误重试逻辑。
void createWakeupPipePair(sock_t& readFd, sock_t& writeFd)
{
  sock_t listener = ::socket(AF_INET, SOCK_STREAM, IPPROTO_TCP);
  if (listener == INVALID_SOCKET) {
    throw DL_ABORT_EX(fmt("Failed to create wakeup pipe: socket() failed: %s",
                          lastSocketError().c_str()));
  }
  sock_t client = INVALID_SOCKET;
  try {
    sockaddr_in addr;
    memset(&addr, 0, sizeof(addr));
    addr.sin_family = AF_INET;
    addr.sin_addr.s_addr = htonl(INADDR_LOOPBACK);
    addr.sin_port = 0;

    if (::bind(listener, reinterpret_cast<sockaddr*>(&addr),
               static_cast<int>(sizeof(addr))) != 0) {
      throw DL_ABORT_EX(fmt("Failed to create wakeup pipe: bind() failed: %s",
                            lastSocketError().c_str()));
    }

    int addrLen = static_cast<int>(sizeof(addr));
    if (::getsockname(listener, reinterpret_cast<sockaddr*>(&addr),
                      &addrLen) != 0) {
      throw DL_ABORT_EX(
          fmt("Failed to create wakeup pipe: getsockname() failed: %s",
              lastSocketError().c_str()));
    }

    if (::listen(listener, 1) != 0) {
      throw DL_ABORT_EX(fmt("Failed to create wakeup pipe: listen() failed: %s",
                            lastSocketError().c_str()));
    }

    client = ::socket(AF_INET, SOCK_STREAM, IPPROTO_TCP);
    if (client == INVALID_SOCKET) {
      throw DL_ABORT_EX(fmt("Failed to create wakeup pipe: socket() failed: %s",
                            lastSocketError().c_str()));
    }
    if (::connect(client, reinterpret_cast<sockaddr*>(&addr),
                  static_cast<int>(sizeof(addr))) == SOCKET_ERROR) {
      throw DL_ABORT_EX(fmt("Failed to create wakeup pipe: connect() failed: %s",
                            lastSocketError().c_str()));
    }

    sock_t server = ::accept(listener, nullptr, nullptr);
    if (server == INVALID_SOCKET) {
      throw DL_ABORT_EX(fmt("Failed to create wakeup pipe: accept() failed: %s",
                            lastSocketError().c_str()));
    }

    // listener 的使命到此结束，立刻关掉，不再占用端口和句柄。
    ::closesocket(listener);
    listener = INVALID_SOCKET;

    // 写端 = client，读端 = server。两端都是已连接的全双工套接字。
    writeFd = client;
    readFd = server;
    makeNonBlocking(readFd);
    makeNonBlocking(writeFd);
  }
  catch (...) {
    if (client != INVALID_SOCKET) {
      ::closesocket(client);
    }
    if (listener != INVALID_SOCKET) {
      ::closesocket(listener);
    }
    throw;
  }
}
#endif // __MINGW32__

} // namespace

WakeupPipe::WakeupPipe() : readFd_(BAD_FD), writeFd_(BAD_FD)
{
#ifdef __MINGW32__
  createWakeupPipePair(readFd_, writeFd_);
#else  // !__MINGW32__
  int fds[2];
  if (::socketpair(AF_UNIX, SOCK_STREAM, 0, fds) < 0) {
    throw DL_ABORT_EX(fmt("Failed to create wakeup pipe: socketpair() failed: %s",
                          util::safeStrerror(errno).c_str()));
  }
  readFd_ = fds[0];
  writeFd_ = fds[1];
  makeNonBlocking(readFd_);
  makeNonBlocking(writeFd_);
#endif // !__MINGW32__
}

WakeupPipe::~WakeupPipe()
{
  // 注意：关闭后对端的 recv() 会永久返回 0（EOF），而 select() 会把 EOF 当作
  // 「可读」持续上报，造成主循环空转。所以这个对象的生命周期必须覆盖整个
  // DownloadEngine，绝不能在引擎还在跑的时候提前析构。
  if (readFd_ != BAD_FD) {
#ifdef __MINGW32__
    ::closesocket(readFd_);
#else  // !__MINGW32__
    ::close(readFd_);
#endif // !__MINGW32__
    readFd_ = BAD_FD;
  }
  if (writeFd_ != BAD_FD) {
#ifdef __MINGW32__
    ::closesocket(writeFd_);
#else  // !__MINGW32__
    ::close(writeFd_);
#endif // !__MINGW32__
    writeFd_ = BAD_FD;
  }
}

void WakeupPipe::notify()
{
  unsigned char b = 1;
#ifdef __MINGW32__
  // 非阻塞 send 在缓冲区满时返回 SOCKET_ERROR + WSAEWOULDBLOCK，属正常情况
  // （说明上一个唤醒还没被消费），直接忽略即可。
  if (::send(writeFd_, reinterpret_cast<const char*>(&b), 1, 0) ==
      SOCKET_ERROR) {
    int errNum = WSAGetLastError();
    if (errNum != WSAEWOULDBLOCK && errNum != WSAENOBUFS) {
      A2_LOG_DEBUG(fmt("Wakeup pipe notify failed: %s",
                       lastSocketError().c_str()));
    }
  }
#else  // !__MINGW32__
  ssize_t r;
  do {
    r = ::send(writeFd_, &b, 1, MSG_NOSIGNAL);
  } while (r == -1 && errno == EINTR);
  if (r == -1 && errno != EAGAIN && errno != EWOULDBLOCK) {
    A2_LOG_DEBUG(
        fmt("Wakeup pipe notify failed: %s", util::safeStrerror(errno).c_str()));
  }
#endif // !__MINGW32__
}

size_t WakeupPipe::drain()
{
  unsigned char buf[64];
  size_t total = 0;
  for (;;) {
#ifdef __MINGW32__
    int n = ::recv(readFd_, reinterpret_cast<char*>(buf), sizeof(buf), 0);
    if (n > 0) {
      total += static_cast<size_t>(n);
      continue;
    }
    if (n == SOCKET_ERROR) {
      int errNum = WSAGetLastError();
      if (errNum != WSAEWOULDBLOCK) {
        A2_LOG_DEBUG(fmt("Wakeup pipe drain failed: %s",
                         lastSocketError().c_str()));
      }
    }
#else  // !__MINGW32__
    ssize_t n = ::recv(readFd_, buf, sizeof(buf), 0);
    if (n > 0) {
      total += static_cast<size_t>(n);
      continue;
    }
    if (n == -1 && errno != EAGAIN && errno != EWOULDBLOCK) {
      A2_LOG_DEBUG(
          fmt("Wakeup pipe drain failed: %s", util::safeStrerror(errno).c_str()));
    }
#endif // !__MINGW32__
    // n == 0（对端关闭）或 EAGAIN（已排空）都到此为止。
    break;
  }
  return total;
}

} // namespace aria2
