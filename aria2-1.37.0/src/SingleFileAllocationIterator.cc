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
#include "SingleFileAllocationIterator.h"

#include <cstring>
#include <cstdlib>

#include "BinaryStream.h"
#include "util.h"
#include "a2io.h"
#include "Logger.h"
#include "LogFactory.h"

namespace aria2 {

constexpr size_t BUFSIZE = 256_k;
constexpr size_t ALIGNMENT = 512;

SingleFileAllocationIterator::SingleFileAllocationIterator(BinaryStream* stream,
                                                           int64_t offset,
                                                           int64_t totalLength)
    : stream_(stream),
      offset_(offset),
      totalLength_(totalLength),
      buffer_(nullptr)
{
}

SingleFileAllocationIterator::~SingleFileAllocationIterator()
{
#ifdef HAVE_POSIX_MEMALIGN
  free(buffer_);
#else
  delete[] buffer_;
#endif // HAVE_POSIX_MEMALIGN
}

void SingleFileAllocationIterator::init()
{
  static bool noticeDone = false;
  if (!noticeDone) {
    noticeDone = true;
    A2_LOG_NOTICE(_("Allocating disk space. Use --file-allocation=none to"
                    " disable it. See --file-allocation option in man page for"
                    " more details."));
  }
#ifdef HAVE_POSIX_MEMALIGN
  buffer_ = reinterpret_cast<unsigned char*>(
      util::allocateAlignedMemory(ALIGNMENT, BUFSIZE));
#else
  buffer_ = new unsigned char[BUFSIZE];
#endif // HAVE_POSIX_MEMALIGN
  memset(buffer_, 0, BUFSIZE);
}

void SingleFileAllocationIterator::allocateChunk()
{
  stream_->writeData(buffer_, BUFSIZE, offset_);
  offset_ += BUFSIZE;

  if (totalLength_ < offset_) {
    stream_->truncate(totalLength_);
    offset_ = totalLength_;
  }
}

bool SingleFileAllocationIterator::finished()
{
  return offset_ == totalLength_;
}

// P0-1：零填充路径。这是唯一真正写数据的分配路径（MinGW 下默认 prealloc 就走
// 这里，见 AdaptiveFileAllocationIterator）。异步层需要拿到那块已清零的 buffer
// 指针，因此这里把它一并通过 unit 暴露出去。
bool SingleFileAllocationIterator::peekAllocationUnit(AllocationUnit& unit)
{
  if (!(offset_ < totalLength_)) {
    return false;
  }
  // 防御：buffer_ 必须在 init() 后才有值；此处幂等地补一次（仅当尚未分配）。
  if (!buffer_) {
    init();
  }
  unit.stream = stream_;
  unit.zeroBuffer = buffer_;
  unit.zeroBufferSize = BUFSIZE;
  unit.offset = offset_;
  unit.length = totalLength_ - offset_;
  unit.zeroFill = true;
  return true;
}

void SingleFileAllocationIterator::commitAllocationUnit(int64_t doneLength)
{
  offset_ += doneLength;
  if (totalLength_ < offset_) {
    // 与原 allocateChunk() 的收尾一致：多写出的部分截掉。
    stream_->truncate(totalLength_);
    offset_ = totalLength_;
  }
}

} // namespace aria2
