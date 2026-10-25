/* <!-- copyright */
/*
 * aria2 - The high speed download utility
 *
 * Copyright (C) 2006 Tatsuhiro Tsujikawa
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
 *
 * In addition, as a special exception, the copyright holders give
 * permission to link the code of portions of this program with the
 * OpenSSL library under certain conditions as described in each
 * individual source file, and distribute linked combinations
 * including the two.
 * You must obey the GNU General Public License in all respects
 * for all of the code used other than OpenSSL.  If you modify
 * file(s) with this exception, you may extend this exception to your
 * version of the file(s), but you are not obligated to do so.  If you
 * do not wish to do so, delete this exception statement from your
 * version.  If you delete this exception statement from all source
 * files in the program, then also delete it here.
 */
/* copyright --> */
#include "FileAllocationCommand.h"
#include "FileAllocationMan.h"
#include "FileAllocationEntry.h"
#include "FileAllocationIterator.h"
#include "FileAllocationAsyncMan.h"
#include "BinaryStream.h"
#include "DownloadEngine.h"
#include "RequestGroup.h"
#include "Logger.h"
#include "LogFactory.h"
#include "message.h"
#include "prefs.h"
#include "util.h"
#include "DownloadEngine.h"
#include "DownloadContext.h"
#include "a2functional.h"
#include "RecoverableException.h"
#include "wallclock.h"
#include "RequestGroupMan.h"
#include "fmt.h"

#include <algorithm>

namespace aria2 {

namespace {
// 异步 chunk 的墙钟上界：64MiB。取消是协作式的，这个值就是「取消延迟」的上界。
// 之所以还要在闭包里再切一刀：MinGW 下 AbstractDiskWriter::allocate() 内部是
// truncate + SetFileValidData 的组合，一次调用覆盖整段，无法被真正打断，因此
// 必须由异步层主动把大调用拆小，才能让 CancelToken 有机会在两个微块之间生效。
constexpr int64_t ASYNC_ALLOC_MICRO_BLOCK = 64_m;
} // namespace

FileAllocationCommand::FileAllocationCommand(
    cuid_t cuid, RequestGroup* requestGroup, DownloadEngine* e,
    FileAllocationEntry* fileAllocationEntry)
    : RealtimeCommand{cuid, requestGroup, e},
      fileAllocationEntry_{fileAllocationEntry},
      asyncAllocationSubmitted_{false}
{
}

FileAllocationCommand::~FileAllocationCommand()
{
  auto e = getDownloadEngine();
  // 顺序不可交换：先让在飞 chunk 结束（内部会先 cancel 再有界等待），确保
  // worker 已不再持有本 entry 及其 DiskAdaptor 的裸指针；然后才把 entry 交还
  // 给 FileAllocationMan（dropPickedEntry 之后该 entry 可能被销毁/复用）。
  e->getFileAllocationAsyncMan()->discardAndWait(getCuid());
  e->getFileAllocationMan()->dropPickedEntry();
}

bool FileAllocationCommand::executeInternal()
{
  if (getRequestGroup()->isHaltRequested()) {
    return true;
  }
  auto& asyncMan = getDownloadEngine()->getFileAllocationAsyncMan();

  if (asyncAllocationSubmitted_) {
    // 阶段 A：收上一轮提交的 chunk 结果。
    AsyncAllocResult result;
    if (!asyncMan->tryFinish(getCuid(), result)) {
      // 仍在执行中：保持命令存活，等唤醒管道 / 下一轮再查。非阻塞。
      getDownloadEngine()->addCommand(std::unique_ptr<Command>(this));
      return false;
    }
    asyncAllocationSubmitted_ = false;
    if (result.errCode != 0) {
      // worker 侧已捕获异常，这里原样抛回，交给 RealtimeCommand::execute()
      // 的 catch → handleException() 走 aria2 既有的错误收尾路径。
      // 错误码必须原样透传（DISK_FULL 不能被降级成 FILE_IO_ERROR），
      // 否则前端只能看到笼统的 17 而无法提示"磁盘空间不足"。
      throw RecoverableException(
          __FILE__, __LINE__, result.errCode, "Async file allocation failed",
          result.errorCode == error_code::UNDEFINED ? error_code::FILE_IO_ERROR
                                                    : result.errorCode);
    }
    if (!result.cancelled) {
      // 只有未取消的结果才允许并入游标（取消的那一份是半成品）。
      fileAllocationEntry_->commitAllocationUnit(result.doneLength);
    }
  }
  else {
    // 阶段 B：尝试提交下一个异步 chunk。
    FileAllocationIterator::AllocationUnit unit;
    bool submitted = false;
    if (fileAllocationEntry_->peekAllocationUnit(unit)) {
      submitted = asyncMan->submit(
          getCuid(), [unit](const CancelTokenPtr& token) {
            AsyncAllocOutcome outcome;
            if (token->isCancelled()) {
              return outcome;
            }
            if (unit.zeroFill) {
              // 零填充路径：每次写一个零块（256KiB，远小于 64MiB），每块之间
              // 查一次取消，取消延迟上界 = 一个零块。
              int64_t done = 0;
              while (done < unit.length) {
                if (token->isCancelled()) {
                  break;
                }
                size_t step = static_cast<size_t>(
                    std::min<int64_t>(unit.zeroBufferSize, unit.length - done));
                unit.stream->writeData(unit.zeroBuffer, step,
                                       unit.offset + done);
                done += static_cast<int64_t>(step);
              }
              outcome.doneLength = done;
            }
            else {
              // fallocate 路径：把原本「一杆子捅到底」的一次性 allocate 拆成
              // 64MiB 的微块循环，每块之间查一次取消。
              int64_t done = 0;
              while (done < unit.length) {
                if (token->isCancelled()) {
                  break;
                }
                int64_t micro = std::min<int64_t>(
                    unit.length - done, ASYNC_ALLOC_MICRO_BLOCK);
                unit.stream->allocate(unit.offset + done, micro, false);
                done += micro;
              }
              outcome.doneLength = done;
            }
            return outcome;
          });
    }
    if (submitted) {
      asyncAllocationSubmitted_ = true;
      getDownloadEngine()->addCommand(std::unique_ptr<Command>(this));
      return false;
    }
    // 同步兜底：线程池队列已满 / 已停机 / 迭代器不支持异步切分
    // （peekAllocationUnit 返回 false）。此时退化为改造前的行为，绝不阻塞在
    // 提交上。此外它也顺带覆盖「无待分配区间但尚未 finished()」的收尾边界。
    fileAllocationEntry_->allocateChunk();
  }

  if (fileAllocationEntry_->finished()) {
    A2_LOG_DEBUG(fmt(
        MSG_ALLOCATION_COMPLETED,
        static_cast<long int>(std::chrono::duration_cast<std::chrono::seconds>(
                                  timer_.difference(global::wallclock()))
                                  .count()),
        getRequestGroup()->getTotalLength()));
    std::vector<std::unique_ptr<Command>> commands;
    fileAllocationEntry_->prepareForNextAction(commands, getDownloadEngine());
    getDownloadEngine()->addCommand(std::move(commands));
    getDownloadEngine()->setNoWait(true);
    return true;
  }
  else {
    getDownloadEngine()->addCommand(std::unique_ptr<Command>(this));
    return false;
  }
}

bool FileAllocationCommand::handleException(Exception& e)
{
  getRequestGroup()->setLastErrorCode(e.getErrorCode(), e.what());
  A2_LOG_ERROR_EX(fmt(MSG_FILE_ALLOCATION_FAILURE, getCuid()), e);
  A2_LOG_ERROR(
      fmt(MSG_DOWNLOAD_NOT_COMPLETE, getCuid(),
          getRequestGroup()->getDownloadContext()->getBasePath().c_str()));
  return true;
}

} // namespace aria2
