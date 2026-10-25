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
#include "SegmentMan.h"

#include <cassert>
#include <algorithm>
#include <numeric>

#include "util.h"
#include "message.h"
#include "prefs.h"
#include "PiecedSegment.h"
#include "GrowSegment.h"
#include "LogFactory.h"
#include "Logger.h"
#include "PieceStorage.h"
#include "PeerStat.h"
#include "Option.h"
#include "DownloadContext.h"
#include "RequestGroup.h"
#include "RequestGroupMan.h"
#include "Piece.h"
#include "FileEntry.h"
#include "wallclock.h"
#include "fmt.h"
#include "WrDiskCacheEntry.h"
#include "DownloadFailureException.h"

namespace aria2 {

SegmentEntry::SegmentEntry(cuid_t cuid, const std::shared_ptr<Segment>& segment)
    : cuid(cuid), segment(segment)
{
}

SegmentEntry::~SegmentEntry() = default;

SegmentMan::SegmentMan(const std::shared_ptr<DownloadContext>& downloadContext,
                       const std::shared_ptr<PieceStorage>& pieceStorage)
    : downloadContext_(downloadContext),
      pieceStorage_(pieceStorage),
      ignoreBitfield_(downloadContext->getPieceLength(),
                      downloadContext->getTotalLength())
{
  ignoreBitfield_.enableFilter();
}

SegmentMan::~SegmentMan() = default;

bool SegmentMan::downloadFinished() const
{
  if (!pieceStorage_) {
    return false;
  }
  else {
    return pieceStorage_->downloadFinished();
  }
}

void SegmentMan::init()
{
  // TODO Do we have to do something about DownloadContext and
  // PieceStorage here?
}

int64_t SegmentMan::getTotalLength() const
{
  if (!pieceStorage_) {
    return 0;
  }
  else {
    return pieceStorage_->getTotalLength();
  }
}

void SegmentMan::setPieceStorage(
    const std::shared_ptr<PieceStorage>& pieceStorage)
{
  pieceStorage_ = pieceStorage;
}

void SegmentMan::setDownloadContext(
    const std::shared_ptr<DownloadContext>& downloadContext)
{
  downloadContext_ = downloadContext;
}

namespace {
void flushWrDiskCache(WrDiskCache* wrDiskCache,
                      const std::shared_ptr<Piece>& piece)
{
  piece->flushWrCache(wrDiskCache);
  if (piece->getWrDiskCacheEntry()->getError() !=
      WrDiskCacheEntry::CACHE_ERR_SUCCESS) {
    piece->clearAllBlock(wrDiskCache);
    throw DOWNLOAD_FAILURE_EXCEPTION2(
        fmt("Write disk cache flush failure index=%lu",
            static_cast<unsigned long>(piece->getIndex())),
        piece->getWrDiskCacheEntry()->getErrorCode());
  }
}
} // namespace

std::shared_ptr<Segment>
SegmentMan::checkoutSegment(cuid_t cuid, const std::shared_ptr<Piece>& piece)
{
  if (!piece) {
    return nullptr;
  }
  A2_LOG_DEBUG(fmt("Attach segment#%lu to CUID#%" PRId64 ".",
                   static_cast<unsigned long>(piece->getIndex()), cuid));

  if (piece->getWrDiskCacheEntry()) {
    // Flush cached data here, because the cached data may be overlapped
    // if BT peers are involved.
    A2_LOG_DEBUG(fmt(
        "Flushing cached data, size=%lu",
        static_cast<unsigned long>(piece->getWrDiskCacheEntry()->getSize())));
    flushWrDiskCache(pieceStorage_->getWrDiskCache(), piece);
  }

  piece->setUsedBySegment(true);
  std::shared_ptr<Segment> segment;
  if (piece->getLength() == 0) {
    segment = std::make_shared<GrowSegment>(piece);
  }
  else {
    segment = std::make_shared<PiecedSegment>(
        downloadContext_->getPieceLength(), piece);
  }
  auto entry = std::make_shared<SegmentEntry>(cuid, segment);
  usedSegmentEntries_.push_back(entry);
  A2_LOG_DEBUG(fmt("index=%lu, length=%" PRId64 ", segmentLength=%" PRId64 ","
                   " writtenLength=%" PRId64,
                   static_cast<unsigned long>(segment->getIndex()),
                   segment->getLength(), segment->getSegmentLength(),
                   segment->getWrittenLength()));

  if (piece->getLength() > 0) {
    auto positr = segmentWrittenLengthMemo_.find(segment->getIndex());
    if (positr != segmentWrittenLengthMemo_.end()) {
      const auto writtenLength = (*positr).second;
      A2_LOG_DEBUG(fmt("writtenLength(in memo)=%" PRId64
                       ", writtenLength=%" PRId64,
                       writtenLength, segment->getWrittenLength()));
      //  If the difference between cached writtenLength and segment's
      //  writtenLength is less than one block, we assume that these
      //  missing bytes are already downloaded.
      if (segment->getWrittenLength() < writtenLength &&
          writtenLength - segment->getWrittenLength() <
              piece->getBlockLength()) {
        segment->updateWrittenLength(writtenLength -
                                     segment->getWrittenLength());
      }
    }
  }
  return segment;
}

void SegmentMan::getInFlightSegment(
    std::vector<std::shared_ptr<Segment>>& segments, cuid_t cuid)
{
  // Aria2-Harness 动态切割（B 方案）：僵死追踪的采样挂在这个「每条命令每轮引擎
  // 迭代都必走」的热路径上（AbstractCommand.cc:202），使僵死时钟的分辨率不再被
  // preemptSegmentTail() 的饥饿触发频率（~1 Hz）限制 —— 实测那样会因 ±1 秒的采样
  // 抖动把 5 秒阈值卡死，永远差 1 秒打不到 5。纯内存操作，主引擎线程，符合红线③。
  const bool trackStall = isDynamicSlicingEnabled();
  const Timer& now = global::wallclock();
  for (SegmentEntries::const_iterator itr = usedSegmentEntries_.begin(),
                                      eoi = usedSegmentEntries_.end();
       itr != eoi; ++itr) {
    const std::shared_ptr<SegmentEntry>& segmentEntry = *itr;
    if (trackStall) {
      const size_t index = segmentEntry->segment->getIndex();
      const int64_t written = segmentEntry->segment->getWrittenLength();
      auto pit = pieceProgress_.find(index);
      if (pit == pieceProgress_.end()) {
        // 首次出现 → 建立基线，僵死时钟从此刻起算。
        pieceProgress_.emplace(
            index, StallTrack{written, segmentEntry->cuid, now});
      }
      else if (pit->second.owner != segmentEntry->cuid ||
               pit->second.written != written) {
        // 换手（新持有者接管）或 writtenLength 有推进 → 刷新基线。
        // 二者必须同时判：换手时 writtenLength 常常仍为 0，若只比对 writtenLength，
        // 刚接手的新连接会被立刻判成「零增长 5 秒」，形成 peer 间乒乓抢占。
        pit->second.written = written;
        pit->second.owner = segmentEntry->cuid;
        pit->second.since = now;
      }
    }
    if (segmentEntry->cuid == cuid) {
      segments.push_back(segmentEntry->segment);
    }
  }
}

std::shared_ptr<Segment> SegmentMan::getSegment(cuid_t cuid,
                                                size_t minSplitSize)
{
  std::shared_ptr<Piece> piece = pieceStorage_->getMissingPiece(
      minSplitSize, ignoreBitfield_.getFilterBitfield(),
      ignoreBitfield_.getBitfieldLength(), cuid);
  if (piece) {
    return checkoutSegment(cuid, piece);
  }
  // Aria2-Harness 动态切割：走到这里即「完全没有空闲 segment」的饥饿点
  // （没有任何缺失且未被占用的 piece 可分）。尝试从慢 peer 手中抢占一片。
  return preemptSegmentTail(cuid);
}

void SegmentMan::getSegment(std::vector<std::shared_ptr<Segment>>& segments,
                            cuid_t cuid, size_t minSplitSize,
                            const std::shared_ptr<FileEntry>& fileEntry,
                            size_t maxSegments)
{
  BitfieldMan filter(ignoreBitfield_);
  filter.enableFilter();
  filter.addNotFilter(fileEntry->getOffset(), fileEntry->getLength());
  std::vector<std::shared_ptr<Segment>> pending;
  while (segments.size() < maxSegments) {
    std::shared_ptr<Segment> segment = checkoutSegment(
        cuid,
        pieceStorage_->getMissingPiece(minSplitSize, filter.getFilterBitfield(),
                                       filter.getBitfieldLength(), cuid));
    if (!segment) {
      break;
    }
    if (segment->getPositionToWrite() < fileEntry->getOffset() ||
        fileEntry->getLastOffset() <= segment->getPositionToWrite()) {
      pending.push_back(segment);
    }
    else {
      segments.push_back(segment);
    }
  }
  for (std::vector<std::shared_ptr<Segment>>::const_iterator
           i = pending.begin(),
           eoi = pending.end();
       i != eoi; ++i) {
    cancelSegment(cuid, *i);
  }
}

std::shared_ptr<Segment> SegmentMan::getSegmentWithIndex(cuid_t cuid,
                                                         size_t index)
{
  if (index > 0 && downloadContext_->getNumPieces() <= index) {
    return nullptr;
  }
  return checkoutSegment(cuid, pieceStorage_->getMissingPiece(index, cuid));
}

std::shared_ptr<Segment> SegmentMan::getCleanSegmentIfOwnerIsIdle(cuid_t cuid,
                                                                  size_t index)
{
  if (index > 0 && downloadContext_->getNumPieces() <= index) {
    return nullptr;
  }
  for (SegmentEntries::const_iterator itr = usedSegmentEntries_.begin(),
                                      eoi = usedSegmentEntries_.end();
       itr != eoi; ++itr) {
    const std::shared_ptr<SegmentEntry>& segmentEntry = *itr;
    if (segmentEntry->segment->getIndex() == index) {
      if (segmentEntry->segment->getWrittenLength() > 0) {
        return nullptr;
      }
      if (segmentEntry->cuid == cuid) {
        return segmentEntry->segment;
      }
      cuid_t owner = segmentEntry->cuid;
      std::shared_ptr<PeerStat> ps = getPeerStat(owner);
      if (!ps || ps->getStatus() == NetStat::IDLE) {
        cancelSegment(owner);
        return getSegmentWithIndex(cuid, index);
      }
      else {
        return nullptr;
      }
    }
  }
  return nullptr;
}

namespace {
// A 方案：全局历史最高水位速度低于此值时（< 100 KiB/s），判定整个任务本来就慢
// （源站限速等），抢占没有收益，直接跳过。
constexpr int64_t kDynamicSlicingMinBaselineSpeed = 100 * 1024;
// 同一个 victim 两次抢占之间的冷却时间（防抖）。
constexpr auto kDynamicSlicingCooldown = std::chrono::seconds(5);
// 由一个 piece 的理论下载时长（pieceLength * 并发数 / 限速）推算僵死阈值时，
// 认为「再等下去也不会自然推进」的上界：超过它就直接禁用抢占，而不是无限放宽。
constexpr double kDynamicSlicingMaxDerivedStall = 120.0;
} // namespace

bool SegmentMan::isDynamicSlicingEnabled() const
{
  auto group = downloadContext_->getOwnerRequestGroup();
  if (!group) {
    return false;
  }
  return group->getOption()->getAsBool(PREF_ENABLE_DYNAMIC_SLICING);
}

size_t SegmentMan::getDynamicSlicingMinTail() const
{
  auto group = downloadContext_->getOwnerRequestGroup();
  if (!group) {
    return 1;
  }
  const int v = group->getOption()->getAsInt(PREF_DYNAMIC_SLICING_MIN_TAIL);
  return v > 0 ? static_cast<size_t>(v) : 1u;
}

int64_t SegmentMan::getDynamicSlicingStallSecs() const
{
  auto group = downloadContext_->getOwnerRequestGroup();
  if (!group) {
    return 12;
  }
  const int v = group->getOption()->getAsInt(PREF_DYNAMIC_SLICING_STALL);
  return v > 0 ? static_cast<int64_t>(v) : 12;
}

size_t SegmentMan::getDynamicSlicingMaxPreempts() const
{
  auto group = downloadContext_->getOwnerRequestGroup();
  if (!group) {
    return 2;
  }
  const int v =
      group->getOption()->getAsInt(PREF_DYNAMIC_SLICING_MAX_PREEMPTS);
  // 0 是合法取值：表示禁用抢占（前端「保守」档位）。
  return v > 0 ? static_cast<size_t>(v) : 0u;
}

int64_t SegmentMan::getEffectiveSpeedLimit() const
{
  auto group = downloadContext_->getOwnerRequestGroup();
  if (!group) {
    return 0;
  }
  const int64_t perTask = group->getMaxDownloadSpeedLimit();
  int64_t overall = 0;
  if (auto man = group->getRequestGroupMan()) {
    overall = man->getMaxOverallDownloadSpeedLimit();
  }
  if (perTask > 0 && overall > 0) {
    return std::min(perTask, overall);
  }
  return perTask > 0 ? perTask : overall;
}

std::shared_ptr<Segment> SegmentMan::preemptSegmentTail(cuid_t cuid)
{
  // 红线③：本函数由主引擎线程（事件循环）调用，内部只做内存与账本操作，
  //         绝不向 AsyncIoExecutor 提交任何任务。
  if (!isDynamicSlicingEnabled()) {
    return nullptr;
  }

  // ---- 主动限速感知（防抖） ----
  // 用户一旦设置了 --max-download-limit / --max-overall-download-limit，令牌桶
  // 会把带宽集中分配给先抢到令牌的连接，其它连接在 piece 级别出现长时间「零写入」
  // 是完全正常的排队现象，而非僵死。此时若仍按固定 12 秒阈值抢占，就会：
  //   抢占 → 接手的连接重新排队 → 又零写入 12 秒 → 再被抢 → 无限倒手（活锁）。
  // 处理策略：
  //   1) max-preempts 可被前端直接设为 0（彻底禁用）；
  //   2) 有主动限速时，按「一片在该并发/该限速下的理论耗时」放宽阈值（至少 2 片）；
  //   3) 若该理论耗时本身已超过 kDynamicSlicingMaxDerivedStall，抢占无意义 → 禁用。
  size_t maxPreemptsPerPiece = getDynamicSlicingMaxPreempts();
  if (maxPreemptsPerPiece == 0) {
    return nullptr;
  }
  int64_t stallSecs = getDynamicSlicingStallSecs();
  const int64_t speedLimit = getEffectiveSpeedLimit();
  if (speedLimit > 0) {
    const int64_t pieceLength = std::max<int64_t>(
        1, static_cast<int64_t>(downloadContext_->getPieceLength()));
    const size_t activeConns = std::max<size_t>(1, usedSegmentEntries_.size());
    const double pieceSecs = static_cast<double>(pieceLength) *
                             static_cast<double>(activeConns) /
                             static_cast<double>(speedLimit);
    if (pieceSecs >= kDynamicSlicingMaxDerivedStall) {
      A2_LOG_DEBUG(fmt("[Dynamic Slicing] speed limit %" PRId64
                       " B/s makes a piece take %.1f s; preemption disabled",
                       speedLimit, pieceSecs));
      return nullptr;
    }
    const int64_t derivedStall = static_cast<int64_t>(pieceSecs * 2.0);
    if (derivedStall > stallSecs) {
      stallSecs = derivedStall;
    }
    // 限速场景下把每个 piece 的抢占上限折半（最少 1 次），进一步压制倒手。
    maxPreemptsPerPiece = std::max<size_t>(1, maxPreemptsPerPiece / 2);
    A2_LOG_DEBUG(fmt("[Dynamic Slicing] speed limit %" PRId64
                     " B/s, connections=%zu, stall threshold relaxed to %" PRId64
                     " s, max preempts per piece=%zu",
                     speedLimit, activeConns, stallSecs,
                     maxPreemptsPerPiece));
  }

  const Timer& now = global::wallclock();

  // ---- A 方案：全局历史最高水位速度（只升不降） ----
  // 刻意废弃「当前最快速度」口径（对 peerStats_ 实时取 calculateDownloadSpeed 最大值）：
  // 快连接把自己那一段下完就下班了，实时最大值会随之塌陷回慢速水平，导致"快 3 倍于慢"
  // 的判据永远不成立。这里改为对所有存活连接取 10s 滚动窗口速度并记录历史最高水位，
  // 作为"本任务究竟能多快"的稳定基准。
  for (const auto& ps : peerStats_) {
    const int64_t speed = static_cast<int64_t>(ps->calculateDownloadSpeed());
    if (speed > fastestSpeedHighWater_) {
      fastestSpeedHighWater_ = speed;
    }
  }
  // 水位过低 → 整个任务本来就慢（源站限速等），抢占没有收益，直接跳过。
  if (fastestSpeedHighWater_ < kDynamicSlicingMinBaselineSpeed) {
    return nullptr;
  }

  // ---- B 方案：僵死（Stalled）判定 ----
  // 采样本身已改到引擎每轮必走的热路径 SegmentMan::getInFlightSegment()（AbstractCommand.cc:202）
  // 完成：那里会对 usedSegmentEntries_ 逐条刷新 <writtenLength, 最后推进时刻>。
  // 之所以不在本函数内采样：本函数只在「饥饿」时被触发（约 1 Hz），采样分辨率被拖到
  // 秒级，配合 5 秒阈值会因 ±1 秒抖动永远差 1 秒打不到。此处仅只读 pieceProgress_。

  // 僵死时长（秒）。注意 Timer::difference(timer) 的语义是 timer - *this，
  // 故必须写成 recordTime.difference(now) 才是「距最后一次推进已过去多久」。
  auto stalledSecs = [&](size_t index) -> int64_t {
    auto it = pieceProgress_.find(index);
    if (it == pieceProgress_.end()) {
      return 0;
    }
    return std::chrono::duration_cast<std::chrono::seconds>(
               it->second.since.difference(now))
        .count();
  };

  const size_t minPieces = getDynamicSlicingMinTail();

  // 按 cuid 聚合「僵死」的 segment。
  // 红线②：只收「尚未下载完成」且「过去 stallSecs 秒内 writtenLength
  //         零增长」的 piece。不再要求 writtenLength == 0 —— 慢 peer 哪怕写了一部分，
  //         只要它卡住了就是合法 victim（那一部分随收回一起丢弃即可）。
  std::map<cuid_t, std::pair<size_t, std::shared_ptr<SegmentEntry>>> candidates;
  for (const auto& e : usedSegmentEntries_) {
    if (e->cuid == cuid) {
      continue;
    }
    if (e->segment->complete()) {
      continue;
    }
    if (stalledSecs(e->segment->getIndex()) < stallSecs) {
      continue;
    }
    auto& slot = candidates[e->cuid];
    ++slot.first;
    if (!slot.second ||
        slot.second->segment->getIndex() < e->segment->getIndex()) {
      // 取该 peer 索引最大的 segment，即「文件尾部」的那一片。
      slot.second = e;
    }
  }

  // 选 victim：僵死时长最长者优先；同长则取索引更大者（更靠近文件尾部）。
  // 保留按 cuid 的 5 秒冷却，防止同一 victim 被反复抽干。
  std::shared_ptr<SegmentEntry> victimEntry;
  cuid_t victimCuid = 0;
  int64_t victimStallSecs = -1;
  size_t victimIndex = 0;
  for (const auto& kv : candidates) {
    if (kv.second.first < minPieces) {
      continue;
    }
    auto it = lastPreemptTime_.find(kv.first);
    if (it != lastPreemptTime_.end() && !it->second.isZero() &&
        it->second.difference(now) < kDynamicSlicingCooldown) {
      continue;
    }
    const size_t index = kv.second.second->segment->getIndex();
    auto pc = preemptCountByPiece_.find(index);
    if (pc != preemptCountByPiece_.end() &&
        pc->second >= maxPreemptsPerPiece) {
      // 这个 piece 已被抢占到上限 → 永久放弃，让当前持有者自然推进。
      continue;
    }
    const int64_t secs = stalledSecs(index);
    if (!victimEntry || secs > victimStallSecs ||
        (secs == victimStallSecs && index > victimIndex)) {
      victimCuid = kv.first;
      victimEntry = kv.second.second;
      victimStallSecs = secs;
      victimIndex = index;
    }
  }

  if (!victimEntry) {
    return nullptr;
  }

  // ---- 执行「单 segment 安全回收」：绝不调用 cancelSegment(owner) 那个按 cuid 清空全部的重载 ----
  const std::shared_ptr<Segment> victimSegment = victimEntry->segment;
  const size_t index = victimSegment->getIndex();
  const int64_t victimWritten = victimSegment->getWrittenLength();

  // 只从账本里摘除这一个 segment，victim 的其它 segment 不受影响。
  // 摘除后，victim 的 DownloadCommand 在下一次 execute() 会因 segments_ 变空而命中
  // AbstractCommand.cc:204 的「已分配 segment 被取消」分支，自动 prepareForRetry，
  // 从而停止继续写这一片 —— 这就是优雅让渡。
  for (auto itr = usedSegmentEntries_.begin();
       itr != usedSegmentEntries_.end(); ++itr) {
    if ((*itr).get() == victimEntry.get()) {
      usedSegmentEntries_.erase(itr);
      break;
    }
  }

  // 复用 cancelSegmentInternal：它先 flush WrDiskCache（慢 peer 可能已在缓存里压了一部分
  // 数据，不 flush 直接 cancelPiece 会触发写盘错误），再 setUsedBySegment(false) +
  // cancelPiece 解除绑定，并记忆 writtenLength。
  cancelSegmentInternal(victimCuid, victimSegment);
  lastPreemptTime_[victimCuid] = now;
  // 记一次抢占次数。达到 maxPreemptsPerPiece 后该 piece 不再作为 victim，
  // 从而把「换手 → 重新等首字节 → 又超阈值 → 再抢」的活锁彻底封死。
  ++preemptCountByPiece_[index];

  // 快连接接管这一片（此刻它已重新变为「缺失且未被占用」，getMissingPiece 能拿到）。
  const std::shared_ptr<Piece> piece = pieceStorage_->getMissingPiece(index, cuid);
  if (!piece) {
    return nullptr;
  }

  A2_LOG_NOTICE(fmt(
      "[Dynamic Slicing] Preempted piece #%lu (Stalled for %" PRId64
      " secs, written: %" PRId64 " bytes) from slow peer %" PRId64
      " to fast peer %" PRId64 ".",
      static_cast<unsigned long>(index), victimStallSecs, victimWritten,
      static_cast<int64_t>(victimCuid), static_cast<int64_t>(cuid)));

  return checkoutSegment(cuid, piece);
}

void SegmentMan::cancelSegmentInternal(cuid_t cuid,
                                       const std::shared_ptr<Segment>& segment)
{
  A2_LOG_DEBUG(fmt("Canceling segment#%lu",
                   static_cast<unsigned long>(segment->getIndex())));
  const std::shared_ptr<Piece>& piece = segment->getPiece();
  // TODO In PieceStorage::cancelPiece(), WrDiskCacheEntry may be
  // released. Flush first.
  if (piece->getWrDiskCacheEntry()) {
    // Flush cached data here, because the cached data may be overlapped
    // if BT peers are involved.
    A2_LOG_DEBUG(fmt(
        "Flushing cached data, size=%lu",
        static_cast<unsigned long>(piece->getWrDiskCacheEntry()->getSize())));
    flushWrDiskCache(pieceStorage_->getWrDiskCache(), piece);
    // TODO Exception may cause some segments (pieces) are not
    // canceled.
  }
  piece->setUsedBySegment(false);
  pieceStorage_->cancelPiece(piece, cuid);
  segmentWrittenLengthMemo_[segment->getIndex()] = segment->getWrittenLength();
  A2_LOG_DEBUG(fmt("Memorized segment index=%lu, writtenLength=%" PRId64,
                   static_cast<unsigned long>(segment->getIndex()),
                   segment->getWrittenLength()));
}

void SegmentMan::cancelSegment(cuid_t cuid)
{
  for (auto itr = usedSegmentEntries_.begin(), eoi = usedSegmentEntries_.end();
       itr != eoi;) {
    if ((*itr)->cuid == cuid) {
      cancelSegmentInternal(cuid, (*itr)->segment);
      itr = usedSegmentEntries_.erase(itr);
      eoi = usedSegmentEntries_.end();
    }
    else {
      ++itr;
    }
  }
}

void SegmentMan::cancelSegment(cuid_t cuid,
                               const std::shared_ptr<Segment>& segment)
{
  for (auto itr = usedSegmentEntries_.begin(), eoi = usedSegmentEntries_.end();
       itr != eoi;) {
    if ((*itr)->cuid == cuid && *(*itr)->segment == *segment) {
      cancelSegmentInternal(cuid, (*itr)->segment);
      itr = usedSegmentEntries_.erase(itr);
      break;
    }
    else {
      ++itr;
    }
  }
}

void SegmentMan::cancelAllSegments()
{
  for (auto& e : usedSegmentEntries_) {
    cancelSegmentInternal(e->cuid, e->segment);
  }
  usedSegmentEntries_.clear();
}

void SegmentMan::eraseSegmentWrittenLengthMemo()
{
  segmentWrittenLengthMemo_.clear();
}

namespace {
class FindSegmentEntry {
private:
  std::shared_ptr<Segment> segment_;

public:
  FindSegmentEntry(std::shared_ptr<Segment> segment)
      : segment_(std::move(segment))
  {
  }

  bool operator()(const std::shared_ptr<SegmentEntry>& segmentEntry) const
  {
    return segmentEntry->segment->getIndex() == segment_->getIndex();
  }
};
} // namespace

bool SegmentMan::completeSegment(cuid_t cuid,
                                 const std::shared_ptr<Segment>& segment)
{
  pieceStorage_->completePiece(segment->getPiece());
  pieceStorage_->advertisePiece(cuid, segment->getPiece()->getIndex(),
                                global::wallclock());
  auto itr = std::find_if(usedSegmentEntries_.begin(),
                          usedSegmentEntries_.end(), FindSegmentEntry(segment));
  if (itr == usedSegmentEntries_.end()) {
    return false;
  }
  else {
    usedSegmentEntries_.erase(itr);
    return true;
  }
}

bool SegmentMan::hasSegment(size_t index) const
{
  return pieceStorage_->hasPiece(index);
}

int64_t SegmentMan::getDownloadLength() const
{
  if (!pieceStorage_) {
    return 0;
  }
  else {
    return pieceStorage_->getCompletedLength();
  }
}

void SegmentMan::registerPeerStat(const std::shared_ptr<PeerStat>& peerStat)
{
  peerStats_.push_back(peerStat);
}

std::shared_ptr<PeerStat> SegmentMan::getPeerStat(cuid_t cuid) const
{
  for (auto& e : peerStats_) {
    if (e->getCuid() == cuid) {
      return e;
    }
  }
  return nullptr;
}

namespace {
class PeerStatHostProtoEqual {
private:
  const std::shared_ptr<PeerStat>& peerStat_;

public:
  PeerStatHostProtoEqual(const std::shared_ptr<PeerStat>& peerStat)
      : peerStat_(peerStat)
  {
  }

  bool operator()(const std::shared_ptr<PeerStat>& p) const
  {
    return peerStat_->getHostname() == p->getHostname() &&
           peerStat_->getProtocol() == p->getProtocol();
  }
};
} // namespace

void SegmentMan::updateFastestPeerStat(
    const std::shared_ptr<PeerStat>& peerStat)
{
  auto i = std::find_if(fastestPeerStats_.begin(), fastestPeerStats_.end(),
                        PeerStatHostProtoEqual(peerStat));
  if (i == fastestPeerStats_.end()) {
    fastestPeerStats_.push_back(peerStat);
  }
  else if ((*i)->getAvgDownloadSpeed() < peerStat->getAvgDownloadSpeed()) {
    // *i's SessionDownloadLength must be added to peerStat
    peerStat->addSessionDownloadLength((*i)->getSessionDownloadLength());
    *i = peerStat;
  }
  else {
    // peerStat's SessionDownloadLength must be added to *i
    (*i)->addSessionDownloadLength(peerStat->getSessionDownloadLength());
  }
}

size_t SegmentMan::countFreePieceFrom(size_t index) const
{
  size_t numPieces = downloadContext_->getNumPieces();
  for (size_t i = index; i < numPieces; ++i) {
    if (pieceStorage_->hasPiece(i) || pieceStorage_->isPieceUsed(i)) {
      return i - index;
    }
  }
  return downloadContext_->getNumPieces() - index;
}

void SegmentMan::ignoreSegmentFor(const std::shared_ptr<FileEntry>& fileEntry)
{
  A2_LOG_DEBUG(fmt("ignoring segment for path=%s, offset=%" PRId64
                   ", length=%" PRId64 "",
                   fileEntry->getPath().c_str(), fileEntry->getOffset(),
                   fileEntry->getLength()));
  ignoreBitfield_.addFilter(fileEntry->getOffset(), fileEntry->getLength());
}

void SegmentMan::recognizeSegmentFor(
    const std::shared_ptr<FileEntry>& fileEntry)
{
  ignoreBitfield_.removeFilter(fileEntry->getOffset(), fileEntry->getLength());
}

bool SegmentMan::allSegmentsIgnored() const
{
  return ignoreBitfield_.isAllFilterBitSet();
}

} // namespace aria2
