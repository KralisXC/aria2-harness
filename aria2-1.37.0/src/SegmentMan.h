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
#ifndef D_SEGMENT_MAN_H
#define D_SEGMENT_MAN_H

#include "common.h"

#include <deque>
#include <vector>
#include <map>
#include <memory>

#include "TimerA2.h"
#include "Command.h"
#include "BitfieldMan.h"

namespace aria2 {

class Segment;
class Option;
class PeerStat;
class DownloadContext;
class PieceStorage;
class Piece;
class FileEntry;

struct SegmentEntry {
  cuid_t cuid;
  std::shared_ptr<Segment> segment;

  SegmentEntry(cuid_t cuid, const std::shared_ptr<Segment>& segment);
  ~SegmentEntry();

  // Don't allow copying
  SegmentEntry(const SegmentEntry&);
  SegmentEntry& operator=(const SegmentEntry&);
};

typedef std::deque<std::shared_ptr<SegmentEntry>> SegmentEntries;

/**
 * This class holds the download progress of the one download entry.
 */
class SegmentMan {
private:
  std::shared_ptr<DownloadContext> downloadContext_;

  std::shared_ptr<PieceStorage> pieceStorage_;

  SegmentEntries usedSegmentEntries_;

  // Remember writtenLength for each segment. The key is an index of a
  // segment. The value is writtenLength for that segment.
  std::map<size_t, int64_t> segmentWrittenLengthMemo_;

  // Used for calculating download speed.
  std::vector<std::shared_ptr<PeerStat>> peerStats_;

  // Keep track of fastest PeerStat for each server
  std::vector<std::shared_ptr<PeerStat>> fastestPeerStats_;

  BitfieldMan ignoreBitfield_;

  // Aria2-Harness 动态切割：每个 victim cuid 上次被抢占的时间（防抖冷却）。
  std::map<cuid_t, Timer> lastPreemptTime_;

  // Aria2-Harness 动态切割（A 方案）：全局历史最高水位速度（只升不降），单位 B/s。
  // 用作「本任务到底能多快」的稳定基准，避免快连接下班后实时最快速度塌陷。
  int64_t fastestSpeedHighWater_ = 0;

  // Aria2-Harness 动态切割（B 方案）：单个 piece 的僵死追踪单元。
  // 故意不写默认成员初始化器 —— 保持聚合类型（gnu++11），这样才能用
  // StallTrack{written, owner, now} 直接构造。
  struct StallTrack {
    // 上次观察到的 writtenLength。
    int64_t written;
    // 观察时的持有者 cuid。持有者变更（抢占后换人）必须重置僵死时钟：
    // 换手瞬间 writtenLength 往往仍是 0，若只比对 writtenLength 就会把刚接手的
    // 新连接立即判成「零增长 5 秒」，导致该 piece 在 peer 之间无限乒乓。
    cuid_t owner;
    // written 值最后一次发生变化的时刻。
    Timer since;
  };

  // Aria2-Harness 动态切割（B 方案）：僵死追踪。key = piece index。
  // 由热路径 getInFlightSegment() 增量刷新；index 唯一且不复用，条目数天然有界于
  // 本下载的 piece 总数。
  std::map<size_t, StallTrack> pieceProgress_;

  // Aria2-Harness 动态切割：每个 piece 已被抢占的次数。key = piece index。
  // 达到 --dynamic-slicing-max-preempts 后该 piece 永久不再作为 victim，用来封死
  // 「换手后新连接重新等待首字节 → 再次超阈值 → 再次被抢」的活锁。
  // 条目数同样有界于本下载的 piece 总数。
  std::map<size_t, size_t> preemptCountByPiece_;

  std::shared_ptr<Segment> checkoutSegment(cuid_t cuid,
                                           const std::shared_ptr<Piece>& piece);

  void cancelSegmentInternal(cuid_t cuid,
                             const std::shared_ptr<Segment>& segment);

  // Aria2-Harness 动态切割：读取 --enable-dynamic-slicing。
  bool isDynamicSlicingEnabled() const;

  // Aria2-Harness 动态切割：读取 --dynamic-slicing-min-tail（0 归一化为 1）。
  size_t getDynamicSlicingMinTail() const;

  // Aria2-Harness 动态切割：读取 --dynamic-slicing-stall（秒，默认 12）。
  int64_t getDynamicSlicingStallSecs() const;

  // Aria2-Harness 动态切割：读取 --dynamic-slicing-max-preempts（默认 2，0 = 禁用）。
  size_t getDynamicSlicingMaxPreempts() const;

  // Aria2-Harness 动态切割：当前对该任务实际生效的主动限速（字节/秒）。
  // 同时存在 --max-download-limit 与 --max-overall-download-limit 时取更严格者；
  // 0 表示未限速。用于避免「限速导致的零写入」被误判为僵死。
  int64_t getEffectiveSpeedLimit() const;

public:
  SegmentMan(const std::shared_ptr<DownloadContext>& downloadContext,
             const std::shared_ptr<PieceStorage>& pieceStorage);

  ~SegmentMan();

  // Initializes totalSize, isSplittable, downloadStarted, errors.
  // Clears command queue. Also, closes diskWriter.
  void init();

  /**
   * The total number of bytes to download.
   * If Transfer-Encoding is Chunked or Content-Length header is not provided,
   * then this value is set to be 0.
   */
  int64_t getTotalLength() const;

  /**
   * Returns true when the download has finished.
   * If downloadStarted is false or the number of the segments of this object
   * holds is 0, then returns false.
   */
  bool downloadFinished() const;

  /**
   * Fill segments which are assigned to the command whose CUID is cuid.
   * This function doesn't clear passed segments.
   */
  void getInFlightSegment(std::vector<std::shared_ptr<Segment>>& segments,
                          cuid_t cuid);

  std::shared_ptr<Segment> getSegment(cuid_t cuid, size_t minSplitSize);

  // Checkouts segments in the range of fileEntry and push back to
  // segments until segments.size() < maxSegments holds false
  void getSegment(std::vector<std::shared_ptr<Segment>>& segments, cuid_t cuid,
                  size_t minSplitSize,
                  const std::shared_ptr<FileEntry>& fileEntry,
                  size_t maxSegments);

  /**
   * Returns a segment whose index is index.
   * If it has already assigned
   * to another cuid or has been downloaded, then returns a segment instance
   * whose isNull call is true.
   */
  std::shared_ptr<Segment> getSegmentWithIndex(cuid_t cuid, size_t index);

  // Returns a currently used segment whose index is index and written
  // length is 0.  The current owner(in idle state) of segment cancels
  // the segment and cuid command acquires the ownership of the
  // segment.  If no such segment exists, returns null.
  std::shared_ptr<Segment> getCleanSegmentIfOwnerIsIdle(cuid_t cuid,
                                                        size_t index);

  /**
   * Aria2-Harness 动态切割（方案 B：Piece 粒度抢占）。
   *
   * 当 cuid 处于「没有任何缺失且未被占用的 piece」的饥饿状态时调用：
   * 从速度低于全局最快速度 1/3 的慢 peer 手中，回收一个「尚未写入」
   * （writtenLength == 0）的 segment（取其 piece 索引最大者，即文件尾部），
   * 并把该 piece 转交给 cuid。
   *
   * 安全红线：
   *   1) 只摘除 victim 的这一个 segment，绝不调用 cancelSegment(owner)；
   *   2) 只动 writtenLength == 0 的 segment，已写入的一律跳过；
   *      WrDiskCache 中仍有未落盘数据的 piece 同样跳过；
   *   3) 必须在主引擎线程执行，绝不提交任何任务到 AsyncIoExecutor。
   *
   * 返回 nullptr 表示本次未发生抢占（开关关闭 / 无慢 peer / 冷却中 / 不达门槛）。
   */
  std::shared_ptr<Segment> preemptSegmentTail(cuid_t cuid);

  /**
   * Updates download status.
   */
  // bool updateSegment(int cuid, const Segment& segment);
  /**
   * Cancels all the segment which the command having given cuid
   * uses.
   */
  void cancelSegment(cuid_t cuid);

  void cancelSegment(cuid_t cuid, const std::shared_ptr<Segment>& segment);

  void cancelAllSegments();

  void eraseSegmentWrittenLengthMemo();

  /**
   * Tells SegmentMan that the segment has been downloaded successfully.
   */
  bool completeSegment(cuid_t cuid, const std::shared_ptr<Segment>& segment);

  /**
   * Injects PieceStorage.
   */
  void setPieceStorage(const std::shared_ptr<PieceStorage>& pieceStorage);

  /**
   * Injects DownloadContext.
   */
  void
  setDownloadContext(const std::shared_ptr<DownloadContext>& downloadContext);

  /**
   * Returns true if the segment whose index is index has been downloaded.
   */
  bool hasSegment(size_t index) const;
  /**
   * Returns the length of bytes downloaded.
   */
  int64_t getDownloadLength() const;

  // If there is inactive PeerStat in peerStats_, it is replaced with
  // given peerStat. If no such PeerStat exist, the given peerStat is
  // inserted.
  void registerPeerStat(const std::shared_ptr<PeerStat>& peerStat);

  const std::vector<std::shared_ptr<PeerStat>>& getPeerStats() const
  {
    return peerStats_;
  }

  std::shared_ptr<PeerStat> getPeerStat(cuid_t cuid) const;

  // If there is slower PeerStat than given peerStat for the same
  // hostname and protocol in fastestPeerStats_, the former is
  // replaced with latter. If there are no PeerStat with same hostname
  // and protocol with given peerStat, given peerStat is inserted.
  void updateFastestPeerStat(const std::shared_ptr<PeerStat>& peerStat);

  const std::vector<std::shared_ptr<PeerStat>>& getFastestPeerStats() const
  {
    return fastestPeerStats_;
  }

  size_t countFreePieceFrom(size_t index) const;

  // Excludes segments that fileEntry covers from segment selection.
  void ignoreSegmentFor(const std::shared_ptr<FileEntry>& fileEntry);

  // Includes segments that fileEntry covers in segment selection.
  void recognizeSegmentFor(const std::shared_ptr<FileEntry>& fileEntry);

  bool allSegmentsIgnored() const;
};

} // namespace aria2

#endif // D_SEGMENT_MAN_H
