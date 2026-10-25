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
#ifndef D_DEFAULT_BT_PROGRESS_INFO_FILE_H
#define D_DEFAULT_BT_PROGRESS_INFO_FILE_H

#include "BtProgressInfoFile.h"

#include <memory>

namespace aria2 {

class DownloadContext;
class PieceStorage;
class PeerStorage;
class BtRuntime;
class Option;
class IOFile;

class DefaultBtProgressInfoFile : public BtProgressInfoFile {
protected:
  std::shared_ptr<DownloadContext> dctx_;
  std::shared_ptr<PieceStorage> pieceStorage_;
#ifdef ENABLE_BITTORRENT
  std::shared_ptr<PeerStorage> peerStorage_;
  std::shared_ptr<BtRuntime> btRuntime_;
#endif // ENABLE_BITTORRENT
  const Option* option_;
  std::string filename_;
  // Last SHA1 digest value of the content written.  Initially, this
  // is empty string.  This is used to avoid to write same content
  // repeatedly, which could wake up disk that may be sleeping.
  std::string lastDigest_;

  bool isTorrentDownload();
  void save(IOFile& fp);
  // 供子类（SqliteProgressInfoFile）复用的反序列化入口：语义与公开的 load()
  // 完全一致，只是把「打开控制文件」这一步交给调用方，便于用内存缓冲承载
  // BLOB 内容。原 load() 已拆分为 load() + load(IOFile&)。
  void load(IOFile& fp);

public:
  DefaultBtProgressInfoFile(const std::shared_ptr<DownloadContext>& btContext,
                            const std::shared_ptr<PieceStorage>& pieceStorage,
                            const Option* option);

  virtual ~DefaultBtProgressInfoFile();

  virtual std::string getFilename() CXX11_OVERRIDE { return filename_; }

  virtual bool exists() CXX11_OVERRIDE;

  virtual void save() CXX11_OVERRIDE;

  virtual void load() CXX11_OVERRIDE;

  virtual void removeFile() CXX11_OVERRIDE;

  // re-set filename using current dctx_.
  virtual void updateFilename() CXX11_OVERRIDE;

#ifdef ENABLE_BITTORRENT
  // for torrents
  void setPeerStorage(const std::shared_ptr<PeerStorage>& peerStorage);

  void setBtRuntime(const std::shared_ptr<BtRuntime>& btRuntime);
#endif // ENABLE_BITTORRENT

  static const std::string& getSuffix()
  {
    static std::string suffix = ".aria2";
    return suffix;
  }
};

// 控制文件实现工厂：按 --sqlite-progress 开关返回
//   - SqliteProgressInfoFile（后缀 .sqlite，需 HAVE_SQLITE3）
//   - DefaultBtProgressInfoFile（后缀 .aria2，兜底）
// 返回类型故意使用 DefaultBtProgressInfoFile 裸类型：SqliteProgressInfoFile
// 继承自它，因此 BT 路径上的 setBtRuntime/setPeerStorage 对两者都成立，
// 无需改动 BtProgressInfoFile 基接口。
std::shared_ptr<DefaultBtProgressInfoFile> makeProgressInfoFile(
    const std::shared_ptr<DownloadContext>& dctx,
    const std::shared_ptr<PieceStorage>& pieceStorage, const Option* option);

// Aria2-Harness: 硬编码后缀的运行时判据。随 --sqlite-progress 开关返回
// ".sqlite"（且编译期有 HAVE_SQLITE3）或 ".aria2"。返回静态字符串引用，
// 调用方无需拷贝。
const std::string& getProgressInfoFileSuffix(const Option* option);

} // namespace aria2

#endif // D_DEFAULT_BT_PROGRESS_INFO_FILE_H
