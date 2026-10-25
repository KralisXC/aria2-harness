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
#ifndef D_FILE_ALLOCATION_ITERATOR_H
#define D_FILE_ALLOCATION_ITERATOR_H

#include "common.h"

#include <unistd.h>

namespace aria2 {

class BinaryStream;

class FileAllocationIterator {
public:
  // P0-1 异步分配单元：一段可以交给 I/O 线程池在事件循环线程之外执行的
  // 「纯 syscall 工作」。
  //
  // 它只携带纯数据 + 裸 syscall 参数（目标流指针、偏移、长度、零填充标志），
  // 绝不携带 Command / RequestGroup / 迭代器游标 —— 后者的可变状态全部由
  // 事件循环线程独占（对应 ROADMAP 红线）。
  struct AllocationUnit {
    BinaryStream* stream;
    // zeroFill == true 时有效：一块长度为 zeroBufferSize 的已清零 buffer。
    const unsigned char* zeroBuffer;
    size_t zeroBufferSize;
    int64_t offset;
    int64_t length;
    bool zeroFill;

    AllocationUnit()
        : stream(nullptr),
          zeroBuffer(nullptr),
          zeroBufferSize(0),
          offset(0),
          length(0),
          zeroFill(false)
    {
    }
  };

  virtual ~FileAllocationIterator() = default;

  virtual void allocateChunk() = 0;

  virtual bool finished() = 0;

  virtual int64_t getCurrentLength() = 0;

  virtual int64_t getTotalLength() = 0;

  // 若还有待分配区间，填充 unit 并返回 true；否则返回 false。
  //
  // 默认实现返回 false —— 表示该迭代器不支持异步切分，调用方会自动回退到
  // 原有的同步 allocateChunk() 语义。这样只需为真正会用到的迭代器实现异步
  // 切分，其余实现零改动（也覆盖「无待分配区间但尚未 finished()」这类边界，
  // 交给同步路径去收尾）。
  virtual bool peekAllocationUnit(AllocationUnit& unit) { return false; }

  // 把 unit 中实际完成的字节数并入游标。doneLength 可能小于 unit.length
  // （取消或部分完成），实现只允许推进已完成的量。
  virtual void commitAllocationUnit(int64_t doneLength) {}
};

} // namespace aria2

#endif // D_FILE_ALLOCATION_ITERATOR_H
