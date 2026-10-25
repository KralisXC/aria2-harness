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
#include "AsyncIoExecutor.h"

#include <utility>

#include "LogFactory.h"
#include "Logger.h"

namespace aria2 {

AsyncIoExecutor::AsyncIoExecutor(std::size_t numWorkers,
                                 std::size_t maxQueueSize)
    : maxQueueSize_(maxQueueSize), runningTasks_(0), stopping_(false)
{
  if (maxQueueSize_ == 0) {
    maxQueueSize_ = 1;
  }
  workers_.reserve(numWorkers);
  for (std::size_t i = 0; i < numWorkers; ++i) {
    workers_.emplace_back([this]() { workerLoop(); });
  }
}

AsyncIoExecutor::~AsyncIoExecutor() { stop(); }

void AsyncIoExecutor::workerLoop()
{
  for (;;) {
    std::function<void()> task;
    {
      std::unique_lock<std::mutex> lock(mutex_);
      taskAvailable_.wait(lock,
                          [this]() { return stopping_ || !queue_.empty(); });
      if (stopping_) {
        return;
      }
      task = std::move(queue_.front());
      queue_.pop_front();
      ++runningTasks_;
    }
    // 任务体在锁外执行：绝不持锁做阻塞 I/O，否则 submit/stop 都会被拖住。
    // 任务体被要求自行捕获异常并落盘到结果邮箱；这里的 catch(...) 只作为
    // 最后一道防线，防止异常逃出线程导致 std::terminate 直接崩掉进程。
    try {
      task();
    }
    catch (...) {
      A2_LOG_ERROR("Unhandled exception escaped from an async I/O task.");
    }
    {
      std::lock_guard<std::mutex> lock(mutex_);
      --runningTasks_;
      if (queue_.empty() && runningTasks_ == 0) {
        idle_.notify_all();
      }
    }
  }
}

bool AsyncIoExecutor::submit(std::function<void()> task)
{
  {
    std::lock_guard<std::mutex> lock(mutex_);
    if (stopping_ || queue_.size() >= maxQueueSize_) {
      return false;
    }
    queue_.push_back(std::move(task));
  }
  taskAvailable_.notify_one();
  return true;
}

void AsyncIoExecutor::waitForIdle()
{
  std::unique_lock<std::mutex> lock(mutex_);
  idle_.wait(lock, [this]() { return queue_.empty() && runningTasks_ == 0; });
}

void AsyncIoExecutor::stop()
{
  {
    std::lock_guard<std::mutex> lock(mutex_);
    if (stopping_) {
      // 已经被别的路径停机（并可能已 join 完），直接返回，保证幂等。
      return;
    }
    stopping_ = true;
    // 未开始的任务直接丢弃：它们对应的 chunk 会由调用方（主线程）走同步兜底，
    // 或被上层取消逻辑整体放弃，不会造成数据丢失。
    queue_.clear();
  }
  taskAvailable_.notify_all();
  for (auto& worker : workers_) {
    if (worker.joinable()) {
      worker.join();
    }
  }
  workers_.clear();
}

std::size_t AsyncIoExecutor::getQueueSize() const
{
  std::lock_guard<std::mutex> lock(mutex_);
  return queue_.size();
}

} // namespace aria2
