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
#ifndef D_WAKEUP_PIPE_H
#define D_WAKEUP_PIPE_H

#include "common.h"

#include <cstddef>

#include "a2netcompat.h"

namespace aria2 {

// 跨平台「唤醒管道」：给单线程事件循环补一个可以从中断点叫醒它的 fd。
//
// 背景：DownloadEngine 的主循环阻塞在 EventPoll::poll()（本构建唯一可用的后端
// 是 SelectEventPoll，走 select()）。I/O 线程池里的 worker 完成任务后必须把事件
// 循环叫醒，否则最坏要等 refreshInterval_（DEFAULT_REFRESH_INTERVAL = 1s）才会
// 回到主循环。全源码检索确认 aria2 原本没有 eventfd / pipe / socketpair 任何
// 唤醒设施，所以这里新建一个。
//
// 为什么必须是 socket 而不是匿名管道：
//   - Windows 的 winsock select() 只认 SOCKET，对匿名管道无效；
//   - 因此 Windows 侧用 127.0.0.1 回环 socket 对（bind/listen/connect/accept）
//     模拟 socketpair，POSIX 侧用真正的 socketpair(AF_UNIX, SOCK_STREAM)。
//   两端都是套接字，SelectEventPoll 的 FD_SET / FD_ISSET 才能正常工作。
//
// 语义说明：唤醒是「电平触发 + 可合并」的，不保证「一次 notify 对应一次
// drain」。调用方必须凭共享状态判断该做什么，绝不能凭「收到几次唤醒」计数。
class WakeupPipe {
public:
  WakeupPipe();

  ~WakeupPipe();

  WakeupPipe(const WakeupPipe&) = delete;
  WakeupPipe& operator=(const WakeupPipe&) = delete;

  sock_t getReadFd() const { return readFd_; }

  sock_t getWriteFd() const { return writeFd_; }

  // 线程安全，可在任意线程调用（典型调用者是 I/O worker）。
  // 非阻塞：管道写满（说明已经有一个未消费的唤醒pending）或对端已关闭时
  // 直接返回，绝不阻塞、绝不抛异常。失败最多让本次唤醒退化为主循环自己的
  // 周期性轮询，不会丢功能。
  void notify();

  // 排空读端所有待读字节。只允许事件循环线程调用。返回读到的字节数。
  size_t drain();

private:
  sock_t readFd_;
  sock_t writeFd_;
};

} // namespace aria2

#endif // D_WAKEUP_PIPE_H
