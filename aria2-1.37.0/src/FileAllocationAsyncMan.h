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
#ifndef D_FILE_ALLOCATION_ASYNC_MAN_H
#define D_FILE_ALLOCATION_ASYNC_MAN_H

#include "common.h"

#include <condition_variable>
#include <cstddef>
#include <cstdint>
#include <deque>
#include <functional>
#include <map>
#include <memory>
#include <mutex>

#include "a2netcompat.h"
#include "AsyncIoExecutor.h"
#include "Command.h"
#include "WakeupPipe.h"
#include "error_code.h"

namespace aria2 {

// worker 侧上报的一次 chunk 执行结果。纯 POD，主线程可直接读取。
//
// 关于 slotId：本模块用 cuid 直接充当 slot 句柄，没有额外的 slotId 字段。
// 依据是 FileAllocation 的并发模型 —— 一个 cuid 同时至多只有一个在飞 chunk
// （FileAllocationCommand 收到结果后才提交下一个），所以 cuid 本身就是唯一键。
// 再引入一个句柄只会增加「两份标识需要同步」的维护面。
struct AsyncAllocResult {
  cuid_t cuid;
  // 本次 chunk 实际完成的长度。主线程据此推进游标。
  int64_t doneLength;
  // 0 表示成功；否则为 RecoverableException 的 errNum。
  int errCode;
  // aria2 应用层错误码（如 DISK_FULL / NOT_ENOUGH_DISK_SPACE）。
  // 必须原样带回主线程，否则 worker 里的磁盘满会被统一降级成 FILE_IO_ERROR。
  error_code::Value errorCode;
  // 本次 chunk 执行期间是否检测到取消。true 时调用方不得把 doneLength
  // 应用到游标上（该 chunk 是半成品）。
  bool cancelled;

  AsyncAllocResult()
      : cuid(0),
        doneLength(0),
        errCode(0),
        errorCode(error_code::UNDEFINED),
        cancelled(false)
  {
  }

  AsyncAllocResult(cuid_t cuid, int64_t doneLength, int errCode,
                   error_code::Value errorCode, bool cancelled)
      : cuid(cuid),
        doneLength(doneLength),
        errCode(errCode),
        errorCode(errorCode),
        cancelled(cancelled)
  {
  }
};

// worker 执行体的返回值。
struct AsyncAllocOutcome {
  int64_t doneLength;
  int errCode;
  error_code::Value errorCode;

  AsyncAllocOutcome()
      : doneLength(0), errCode(0), errorCode(error_code::UNDEFINED)
  {
  }
};

// worker 执行体。
//
// 安全红线：执行体在 worker 线程上运行，只允许触碰**纯数据 + 裸 syscall 参数**
// （典型形态：捕获一个 BinaryStream* 与一段明确的 [offset, length)）。
// 绝对不允许触碰 Command / RequestGroup / FileAllocationEntry / 任何迭代器
// 的内部游标 —— 那些对象的生命周期与可变状态全部归事件循环线程独占。
//
// token 用于在「微块」之间轮询取消。注意 fallocate / SetFileEnd 这类单次系统调用
// 无法被真正打断，取消是协作式的：靠把大调用切成小调用，把取消延迟压到一个微块内。
typedef std::function<AsyncAllocOutcome(const CancelTokenPtr& token)>
    AsyncAllocBody;

// P0-1 的异步文件分配调度器。
//
// 职责：
//   1) 持有 I/O 线程池与唤醒管道，并提供「提交 → 轮询 → 收尾」三段式接口；
//   2) 维护 slot 表（cuid → 取消令牌 / 在飞标志）与 POD 结果邮箱；
//   3) 提供 discardAndWait()，保证上层释放 DiskAdaptor / RequestGroup 之前，
//      一定没有 worker 还握有指向它的裸指针。
//
// 线程模型：
//   - 主线程（事件循环）独占调用 submit / tryFinish / discardAndWait /
//     drainWakeup / shutdown；
//   - worker 线程只调用内部的 publish()，即「写邮箱 + 清在飞标志 + notify 管道」；
//   - 唯一的跨线程共享状态是 slots_ / mailbox_，全部由一把 mutex_ 保护。
class FileAllocationAsyncMan {
public:
  // numWorkers 默认 1：文件分配是磁盘瓶颈，多 worker 只会让寻道交错变差。
  // maxQueueSize 是「同步兜底」的触发水位：每个 cuid 至多占 1 个队列位，
  // 所以 64 足以容纳几十个并发任务的分配阶段。
  FileAllocationAsyncMan(std::size_t numWorkers = 1,
                         std::size_t maxQueueSize = 64);

  ~FileAllocationAsyncMan();

  FileAllocationAsyncMan(const FileAllocationAsyncMan&) = delete;
  FileAllocationAsyncMan& operator=(const FileAllocationAsyncMan&) = delete;

  // ---- 事件循环线程接口 ----

  // 非阻塞提交。返回 false 的三种情况：该 cuid 已有在飞 chunk / 线程池队列已满 /
  // 已经停机。三种情况调用方都必须走同步兜底（最坏等于旧行为），绝不能等待。
  bool submit(cuid_t cuid, AsyncAllocBody body);

  // 非阻塞查询。若 cuid 的结果已到，取出并释放该 slot，返回 true。
  // 返回 false 表示仍在执行中 —— 调用方应保持命令存活并稍后重试。
  bool tryFinish(cuid_t cuid, AsyncAllocResult& result);

  // 非阻塞查询：邮箱里是否有待取结果。供事件循环在最前方决定本轮是否阻塞。
  bool hasReadyResult() const;

  // 阻塞等待 cuid 当前在飞的那个 chunk 结束，随后丢弃其陈旧结果并释放 slot。
  //
  // ★ 必须在 RequestGroup / DiskAdaptor 被销毁之前调用。worker 手里是裸的
  //   BinaryStream*，一旦上层先释放，worker 就是在写已释放内存。
  //
  // 语义要点：
  //   - 先 cancel()，把等待压到「一个微块」以内；
  //   - 等待是**无上界的**（正确性优先于活性）：一旦超时返回而 worker 仍在跑，
  //     上层的释放立刻变成 use-after-free。宁可慢，不可错。
  //   - 「回滚状态」= 丢弃陈旧结果：调用方不得再把 doneLength 应用到游标上。
  void discardAndWait(cuid_t cuid);

  // 优雅停机。必须先停线程池（join 全部 worker），再销毁唤醒管道 ——
  // 顺序反了就会撞上 WakeupPipe 的 EOF 空转问题，或让 worker 碰已关闭的 fd。
  // 幂等。
  void shutdown();

  // ---- 事件循环接线（供 DownloadEngine / WakeupCommand 使用）----

  sock_t getWakeupReadFd() const
  {
    return wakeupPipe_ ? wakeupPipe_->getReadFd() : (sock_t)-1;
  }

  void drainWakeup()
  {
    if (wakeupPipe_) {
      wakeupPipe_->drain();
    }
  }

private:
  struct Slot {
    CancelTokenPtr token;
    bool inFlight;
    Slot() : inFlight(false) {}
  };

  // worker 线程调用：发布结果 + 清在飞标志 + 唤醒事件循环。
  void publish(cuid_t cuid, const AsyncAllocOutcome& outcome, bool cancelled);

  mutable std::mutex mutex_;
  // 用一把 mutex + 一个 cv 同时保护 slot 表与结果邮箱。
  // 刻意不分两把锁：worker 要同时改邮箱和 slot 表，分锁就必须固定加锁顺序，
  // 而 discardAndWait 里的等待又必须持锁 —— 两条路径的加锁顺序天然相反，
  // 极易写出死锁。单锁 + 单 cv 是这里唯一稳妥的形态。
  std::condition_variable idle_;
  std::map<cuid_t, Slot> slots_;
  std::deque<AsyncAllocResult> mailbox_;
  std::unique_ptr<AsyncIoExecutor> executor_;
  std::unique_ptr<WakeupPipe> wakeupPipe_;
  bool stopped_;
};

} // namespace aria2

#endif // D_FILE_ALLOCATION_ASYNC_MAN_H
