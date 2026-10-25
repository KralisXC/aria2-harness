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
#include "FileAllocationAsyncMan.h"

#include <utility>

#include "a2functional.h"
#include "DlAbortEx.h"
#include "fmt.h"
#include "LogFactory.h"
#include "Logger.h"
#include "RecoverableException.h"

namespace aria2 {

FileAllocationAsyncMan::FileAllocationAsyncMan(std::size_t numWorkers,
                                               std::size_t maxQueueSize)
    : executor_(make_unique<AsyncIoExecutor>(numWorkers, maxQueueSize)),
      wakeupPipe_(make_unique<WakeupPipe>()),
      stopped_(false)
{
}

FileAllocationAsyncMan::~FileAllocationAsyncMan() { shutdown(); }

bool FileAllocationAsyncMan::submit(cuid_t cuid, AsyncAllocBody body)
{
  CancelTokenPtr token;
  {
    std::lock_guard<std::mutex> lock(mutex_);
    if (stopped_) {
      return false;
    }
    auto it = slots_.find(cuid);
    if (it != slots_.end() && it->second.inFlight) {
      // 同一 cuid 不允许两个在飞 chunk：否则两个 worker 会同时推进同一段区间，
      // 而主线程只持有一个游标，无法正确归并结果。
      return false;
    }
    if (it == slots_.end()) {
      it = slots_.emplace(cuid, Slot()).first;
    }
    it->second.token = std::make_shared<CancelToken>();
    it->second.inFlight = true;
    token = it->second.token;
  }

  auto task = [this, cuid, token, body]() {
    AsyncAllocOutcome outcome;
    try {
      outcome = body(token);
    }
    catch (RecoverableException& e) {
      // stream->allocate() 在 POSIX 上就是靠抛 RecoverableException 报错的
      // （磁盘满 / 文件系统不支持），必须原样把 errNum **和 aria2 错误码**
      // 一起带回主线程，否则 DISK_FULL 会被降级成 FILE_IO_ERROR。
      outcome.errCode = e.getErrNum();
      outcome.errorCode = e.getErrorCode();
    }
    catch (...) {
      outcome.errCode = -1;
      outcome.errorCode = error_code::FILE_IO_ERROR;
    }
    if (outcome.errCode != 0) {
      A2_LOG_DEBUG(fmt("Async file allocation chunk of cuid %" PRId64
                       " failed: errCode=%d",
                       static_cast<int64_t>(cuid), outcome.errCode));
    }
    publish(cuid, outcome, token->isCancelled());
  };

  if (!executor_->submit(std::move(task))) {
    // 队列满或已停机：回滚 slot，让调用方立刻改走同步兜底。
    // 注意此时任务体已被丢弃（未入队），不会有 worker 再触碰这个 slot。
    std::lock_guard<std::mutex> lock(mutex_);
    auto it = slots_.find(cuid);
    if (it != slots_.end() && it->second.inFlight &&
        it->second.token == token) {
      slots_.erase(it);
    }
    A2_LOG_DEBUG(fmt("Async file allocation queue is unavailable. Falling back "
                     "to synchronous allocation for cuid %" PRId64,
                     static_cast<int64_t>(cuid)));
    return false;
  }
  return true;
}

void FileAllocationAsyncMan::publish(cuid_t cuid,
                                     const AsyncAllocOutcome& outcome,
                                     bool cancelled)
{
  {
    std::lock_guard<std::mutex> lock(mutex_);
    mailbox_.push_back(AsyncAllocResult(cuid, outcome.doneLength,
                                        outcome.errCode, outcome.errorCode,
                                        cancelled));
    auto it = slots_.find(cuid);
    if (it != slots_.end()) {
      it->second.inFlight = false;
    }
  }
  // 先让邮箱与 slot 状态对外可见（上面的解锁是 release 屏障），再通知：
  // 被唤醒者一定能看到它们，不需要额外的内存屏障。
  idle_.notify_all();
  // 最后唤醒事件循环。放在最后是为了避免「主线程立刻 drain 邮箱却发现是空的」
  // 这种纯粹的时序浪费。
  if (wakeupPipe_) {
    wakeupPipe_->notify();
  }
}

bool FileAllocationAsyncMan::tryFinish(cuid_t cuid, AsyncAllocResult& result)
{
  std::lock_guard<std::mutex> lock(mutex_);
  auto it = slots_.find(cuid);
  if (it == slots_.end() || it->second.inFlight) {
    return false;
  }
  // 只摘自己那一条：邮箱里可能还躺着别的 cuid 的结果，它们的命令还没被调度到。
  // 线性扫描是可接受的 —— 邮箱长度上界 = 并发分配中的 cuid 数（几十量级）。
  for (auto i = mailbox_.begin(); i != mailbox_.end(); ++i) {
    if (i->cuid == cuid) {
      result = *i;
      mailbox_.erase(i);
      slots_.erase(it);
      return true;
    }
  }
  return false;
}

bool FileAllocationAsyncMan::hasReadyResult() const
{
  std::lock_guard<std::mutex> lock(mutex_);
  return !mailbox_.empty();
}

void FileAllocationAsyncMan::discardAndWait(cuid_t cuid)
{
  std::unique_lock<std::mutex> lock(mutex_);
  auto it = slots_.find(cuid);
  if (it == slots_.end()) {
    // 没有在飞 chunk（绝大多数情况），直接返回。
    return;
  }
  if (it->second.inFlight) {
    // 先取消再等：cancel 让任务体在下一个微块边界退出，从而把等待时间压到
    // 「一个微块」而非「整个 chunk」。但即使不取消，等待也是有界的 ——
    // 上界就是一个 chunk 的执行时间。
    it->second.token->cancel();
    A2_LOG_DEBUG(fmt("Waiting for the in-flight allocation chunk of cuid %" PRId64
                     " to finish before releasing its DiskAdaptor.",
                     static_cast<int64_t>(cuid)));
    idle_.wait(lock, [this, cuid]() {
      auto i = slots_.find(cuid);
      return i == slots_.end() || !i->second.inFlight;
    });
  }
  // 「回滚状态」：把这个 cuid 的陈旧结果从邮箱里摘掉。
  // 调用方已经放弃这个 entry，绝不能把这次 chunk 的 doneLength 应用到游标上。
  for (auto i = mailbox_.begin(); i != mailbox_.end();) {
    if (i->cuid == cuid) {
      i = mailbox_.erase(i);
    }
    else {
      ++i;
    }
  }
  slots_.erase(cuid);
}

void FileAllocationAsyncMan::shutdown()
{
  {
    std::lock_guard<std::mutex> lock(mutex_);
    if (stopped_) {
      return;
    }
    stopped_ = true;
  }
  // 顺序不可交换：
  //   1) executor_->stop() 会 join 全部 worker，返回后保证没有任何线程还会
  //      触碰 wakeupPipe_（也不会再有 publish() 发生）；
  //   2) 之后才能销毁管道。
  // 反过来的话，worker 可能对已关闭的 fd 调 notify()，或者管道先析构导致
  // select() 读到 EOF 上空转。
  executor_->stop();
  wakeupPipe_.reset();
}

} // namespace aria2
