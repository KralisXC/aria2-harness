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
#ifndef D_ASYNC_IO_EXECUTOR_H
#define D_ASYNC_IO_EXECUTOR_H

#include "common.h"

#include <atomic>
#include <condition_variable>
#include <cstddef>
#include <deque>
#include <functional>
#include <memory>
#include <mutex>
#include <thread>
#include <vector>

namespace aria2 {

// 取消令牌。由主线程持有；worker 在可中断边界（通常是每个微块之间）轮询它。
// 注意：fallocate/SetFileValidData 这类单次阻塞系统调用无法被真正打断，
// 因此取消是「协作式」的——靠把大调用拆成小调用（微块）来把取消延迟压到一个微块内。
class CancelToken {
public:
  CancelToken() : cancelled_(false) {}

  void cancel() { cancelled_.store(true, std::memory_order_release); }

  bool isCancelled() const
  {
    return cancelled_.load(std::memory_order_acquire);
  }

private:
  std::atomic<bool> cancelled_;
};

typedef std::shared_ptr<CancelToken> CancelTokenPtr;

// C++11 有界 I/O 线程池。
// 设计约束（对应 ROADMAP 红线）：
//   1) submit() 绝不阻塞调用线程——队列满时直接返回 false，由调用方决定同步兜底；
//   2) 任务体只接受 std::function<void()>，线程池本身不认识任何 aria2 类；
//   3) stop() 置停止标志 + notify_all + join，保证析构完成后不再有任务体在运行。
class AsyncIoExecutor {
public:
  AsyncIoExecutor(std::size_t numWorkers, std::size_t maxQueueSize);

  ~AsyncIoExecutor();

  AsyncIoExecutor(const AsyncIoExecutor&) = delete;
  AsyncIoExecutor& operator=(const AsyncIoExecutor&) = delete;

  // 非阻塞提交。返回 false 表示队列已满或已停机（任务未被接受，调用方需自行处理）。
  bool submit(std::function<void()> task);

  // 等待队列排空且所有在飞任务返回。仅供停机/调试使用，切勿在事件循环稳态中调用。
  void waitForIdle();

  // 优雅停机：置停止标志、清空未开始的任务、唤醒全部 worker 并 join。幂等。
  void stop();

  std::size_t getNumWorkers() const { return workers_.size(); }

  std::size_t getQueueSize() const;

private:
  void workerLoop();

  mutable std::mutex mutex_;
  std::condition_variable taskAvailable_;
  std::condition_variable idle_;
  std::deque<std::function<void()>> queue_;
  std::vector<std::thread> workers_;
  std::size_t maxQueueSize_;
  std::size_t runningTasks_;
  bool stopping_;
};

} // namespace aria2

#endif // D_ASYNC_IO_EXECUTOR_H
