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
#ifndef D_SQLITE_PROGRESS_INFO_FILE_H
#define D_SQLITE_PROGRESS_INFO_FILE_H

#include "DefaultBtProgressInfoFile.h"

namespace aria2 {

// 实验性改造：把 .aria2 控制文件换成「每任务一个」的 SQLite 库（后缀 .sqlite）。
//
// 继承 DefaultBtProgressInfoFile 以复用其二进制序列化 / 反序列化逻辑
// （save(IOFile&) / load(IOFile&)），因此不会产生第二份格式定义；
// 同时因为父类即 DefaultBtProgressInfoFile，BT 路径上的
// setBtRuntime / setPeerStorage 对两者都成立，无需改动 BtProgressInfoFile 接口。
class SqliteProgressInfoFile : public DefaultBtProgressInfoFile {
private:
  std::string sqliteFilename_;
  std::string sqliteLastDigest_;

  // 库文件路径：<basePath>.sqlite
  std::string computeFilename();
  // 库内主键：BT 任务用 infoHash hex；非 BT 任务用固定字面量 "ctrl"
  // （库是每任务一个文件，主键只需库内唯一）。
  std::string primaryKey();
  // 解析失败时把 PieceStorage 复位为「全新任务」的空状态。
  void resetPieceStorageState();

public:
  SqliteProgressInfoFile(const std::shared_ptr<DownloadContext>& dctx,
                         const std::shared_ptr<PieceStorage>& pieceStorage,
                         const Option* option);

  ~SqliteProgressInfoFile() CXX11_OVERRIDE;

  std::string getFilename() CXX11_OVERRIDE { return sqliteFilename_; }

  bool exists() CXX11_OVERRIDE;

  void save() CXX11_OVERRIDE;

  void load() CXX11_OVERRIDE;

  void removeFile() CXX11_OVERRIDE;

  // re-set filename using current dctx_.
  void updateFilename() CXX11_OVERRIDE;

  static const std::string& getSuffix()
  {
    static std::string suffix = ".sqlite";
    return suffix;
  }
};

} // namespace aria2

#endif // D_SQLITE_PROGRESS_INFO_FILE_H
