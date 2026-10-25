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
#include "SqliteProgressInfoFile.h"

#include <sqlite3.h>

#include <algorithm>
#include <cstring>
#include <vector>

#include "DownloadContext.h"
#include "PieceStorage.h"
#include "Option.h"
#include "File.h"
#include "IOFile.h"
#include "SHA1IOFile.h"
#include "LogFactory.h"
#include "Logger.h"
#include "Exception.h"
#include "DlAbortEx.h"
#include "fmt.h"
#include "util.h"
#include "BtConstants.h"
#ifdef ENABLE_BITTORRENT
#  include "bittorrent_helper.h"
#endif // ENABLE_BITTORRENT

namespace aria2 {

namespace {

// 内存版 IOFile：把父类那套序列化/反序列化重定向到一段内存缓冲，
// 于是同一份字节流既能当 BLOB 入库，又能当输入流回放。
class MemoryIOFile : public IOFile {
public:
  MemoryIOFile() = default;

  const void* getData() const { return buf_.data(); }

  int getSize() const { return static_cast<int>(buf_.size()); }

  void setData(const void* data, size_t len)
  {
    const unsigned char* p = static_cast<const unsigned char*>(data);
    buf_.assign(p, p + len);
    readPos_ = 0;
    eof_ = false;
  }

protected:
  size_t onRead(void* ptr, size_t count) CXX11_OVERRIDE
  {
    size_t avail = buf_.size() - readPos_;
    size_t n = std::min(count, avail);
    if (n > 0) {
      memcpy(ptr, buf_.data() + readPos_, n);
      readPos_ += n;
    }
    eof_ = (readPos_ >= buf_.size());
    return n;
  }

  size_t onWrite(const void* ptr, size_t count) CXX11_OVERRIDE
  {
    const unsigned char* p = static_cast<const unsigned char*>(ptr);
    buf_.insert(buf_.end(), p, p + count);
    return count;
  }

  // 内存缓冲不支持按行读取 / 格式化输出，恒失败即可。
  char* onGets(char* s, int size) CXX11_OVERRIDE
  {
    (void)s;
    (void)size;
    return nullptr;
  }

  int onVprintf(const char* format, va_list va) CXX11_OVERRIDE
  {
    (void)format;
    (void)va;
    return -1;
  }

  int onFlush() CXX11_OVERRIDE { return 0; }
  int onClose() CXX11_OVERRIDE { return 0; }
  bool onSupportsColor() CXX11_OVERRIDE { return false; }
  bool isError() const CXX11_OVERRIDE { return false; }
  bool isEOF() const CXX11_OVERRIDE { return eof_; }
  bool isOpen() const CXX11_OVERRIDE { return true; }

private:
  std::vector<unsigned char> buf_;
  size_t readPos_ = 0;
  bool eof_ = false;
};

const char kCreateTableSql[] =
    "CREATE TABLE IF NOT EXISTS progress "
    "(info_hash TEXT PRIMARY KEY, data BLOB);";

// 打开库：可选启用 WAL + 建表。任何失败只返回 nullptr，由调用方决定策略，
// 绝不在打开阶段抛异常（避免把磁盘/权限问题升级成下载中止）。
sqlite3* openDatabase(const std::string& path, bool createIfMissing)
{
  sqlite3* db = nullptr;
  int flags = SQLITE_OPEN_READWRITE;
  if (createIfMissing) {
    flags |= SQLITE_OPEN_CREATE;
  }
  if (sqlite3_open_v2(path.c_str(), &db, flags, nullptr) != SQLITE_OK) {
    A2_LOG_ERROR(fmt("SQLite 进度文件打开失败 path=%s error=%s", path.c_str(),
                     db ? sqlite3_errmsg(db) : "unknown"));
    if (db) {
      sqlite3_close(db);
    }
    return nullptr;
  }
  // WAL：读不阻塞写，避免写盘时卡顿；我们并非逐 piece 提交，收益体现在
  // 前端读进度时不会与写控制文件互相阻塞。
  sqlite3_exec(db, "PRAGMA journal_mode=WAL;", nullptr, nullptr, nullptr);
  // WAL 搭配 synchronous=NORMAL，在「60s 级提交节拍」下足够安全。
  sqlite3_exec(db, "PRAGMA synchronous=NORMAL;", nullptr, nullptr, nullptr);
  if (createIfMissing) {
    sqlite3_exec(db, kCreateTableSql, nullptr, nullptr, nullptr);
  }
  return db;
}

} // namespace

SqliteProgressInfoFile::SqliteProgressInfoFile(
    const std::shared_ptr<DownloadContext>& dctx,
    const std::shared_ptr<PieceStorage>& pieceStorage, const Option* option)
    : DefaultBtProgressInfoFile(dctx, pieceStorage, option),
      sqliteFilename_(computeFilename())
{
}

SqliteProgressInfoFile::~SqliteProgressInfoFile() = default;

std::string SqliteProgressInfoFile::computeFilename()
{
  std::string t = dctx_->getBasePath();
  t += getSuffix();
  return t;
}

void SqliteProgressInfoFile::updateFilename()
{
  sqliteFilename_ = computeFilename();
}

std::string SqliteProgressInfoFile::primaryKey()
{
#ifdef ENABLE_BITTORRENT
  if (btRuntime_.get()) {
    // 有 BtRuntime 说明是 BT 任务，用 infoHash 十六进制串作主键。
    return util::toHex(bittorrent::getInfoHash(dctx_), INFO_HASH_LENGTH);
  }
#endif // ENABLE_BITTORRENT
  return "ctrl";
}

bool SqliteProgressInfoFile::exists()
{
  File f(sqliteFilename_);
  if (!f.isFile()) {
    A2_LOG_INFO(fmt("SQLite 进度文件不存在 %s", sqliteFilename_.c_str()));
    return false;
  }
  sqlite3* db = openDatabase(sqliteFilename_, false);
  if (!db) {
    return false;
  }
  bool found = false;
  sqlite3_stmt* stmt = nullptr;
  if (sqlite3_prepare_v2(db,
                         "SELECT 1 FROM progress WHERE info_hash = ? LIMIT 1;",
                         -1, &stmt, nullptr) == SQLITE_OK) {
    std::string key = primaryKey();
    sqlite3_bind_text(stmt, 1, key.c_str(), static_cast<int>(key.size()),
                      SQLITE_TRANSIENT);
    found = (sqlite3_step(stmt) == SQLITE_ROW);
    sqlite3_finalize(stmt);
  }
  sqlite3_close(db);
  return found;
}

void SqliteProgressInfoFile::save()
{
  // 1) 复用父类序列化做 SHA1 去重：内容未变则不写库（与 .aria2 同款节流）。
  SHA1IOFile sha1io;
  DefaultBtProgressInfoFile::save(sha1io);
  std::string digest = sha1io.digest();
  if (digest == sqliteLastDigest_) {
    return;
  }

  // 2) 同一份字节流写进内存缓冲，整体作为单个 BLOB 入库。绝不逐 piece 提交。
  MemoryIOFile mem;
  DefaultBtProgressInfoFile::save(mem);

  A2_LOG_INFO(fmt("保存 SQLite 进度文件 %s", sqliteFilename_.c_str()));

  sqlite3* db = openDatabase(sqliteFilename_, true);
  if (!db) {
    // 与 .aria2 实现不同：这里不抛异常，避免因磁盘/权限问题直接中止下载。
    return;
  }
  bool ok = false;
  sqlite3_stmt* stmt = nullptr;
  if (sqlite3_prepare_v2(
          db,
          "INSERT OR REPLACE INTO progress (info_hash, data) VALUES (?, ?);",
          -1, &stmt, nullptr) == SQLITE_OK) {
    std::string key = primaryKey();
    sqlite3_bind_text(stmt, 1, key.c_str(), static_cast<int>(key.size()),
                      SQLITE_TRANSIENT);
    sqlite3_bind_blob(stmt, 2, mem.getData(), mem.getSize(), SQLITE_TRANSIENT);
    ok = (sqlite3_step(stmt) == SQLITE_DONE);
    if (!ok) {
      A2_LOG_ERROR(fmt("SQLite 写入 BLOB 失败 %s: %s", sqliteFilename_.c_str(),
                       sqlite3_errmsg(db)));
    }
    sqlite3_finalize(stmt);
  }
  else {
    A2_LOG_ERROR(fmt("SQLite 准备写语句失败 %s: %s", sqliteFilename_.c_str(),
                     sqlite3_errmsg(db)));
  }
  sqlite3_close(db);

  if (ok) {
    // 仅在真正落库成功后记住摘要，失败则下次仍会重试。
    sqliteLastDigest_ = std::move(digest);
    A2_LOG_INFO(fmt("已保存 SQLite 进度文件 %s", sqliteFilename_.c_str()));
  }
}

void SqliteProgressInfoFile::resetPieceStorageState()
{
  if (!pieceStorage_) {
    return;
  }
  size_t len = pieceStorage_->getBitfieldLength();
  if (len == 0) {
    return;
  }
  std::vector<unsigned char> zeros(len, 0);
  pieceStorage_->setBitfield(zeros.data(), len);
}

void SqliteProgressInfoFile::load()
{
  A2_LOG_INFO(fmt("加载 SQLite 进度文件 %s", sqliteFilename_.c_str()));

  std::vector<unsigned char> blob;
  sqlite3* db = openDatabase(sqliteFilename_, false);
  if (db) {
    sqlite3_stmt* stmt = nullptr;
    if (sqlite3_prepare_v2(
            db, "SELECT data FROM progress WHERE info_hash = ? LIMIT 1;", -1,
            &stmt, nullptr) == SQLITE_OK) {
      std::string key = primaryKey();
      sqlite3_bind_text(stmt, 1, key.c_str(), static_cast<int>(key.size()),
                        SQLITE_TRANSIENT);
      if (sqlite3_step(stmt) == SQLITE_ROW) {
        const void* p = sqlite3_column_blob(stmt, 0);
        int n = sqlite3_column_bytes(stmt, 0);
        if (p && n > 0) {
          const unsigned char* b = static_cast<const unsigned char*>(p);
          blob.assign(b, b + n);
        }
      }
      sqlite3_finalize(stmt);
    }
    else {
      A2_LOG_ERROR(fmt("SQLite 准备读语句失败 %s: %s", sqliteFilename_.c_str(),
                       sqlite3_errmsg(db)));
    }
    sqlite3_close(db);
  }

  // 硬性边界防御：无论发生什么错误都只记日志 + 复位空态 + 返回，
  // 绝不让异常逃逸导致引擎闪退。
  try {
    if (blob.empty()) {
      throw DL_ABORT_EX(
          fmt("SQLite 进度文件内容为空或不存在: %s", sqliteFilename_.c_str()));
    }
    MemoryIOFile mem;
    mem.setData(blob.data(), blob.size());
    DefaultBtProgressInfoFile::load(mem);
    A2_LOG_INFO(fmt("已加载 SQLite 进度文件 %s", sqliteFilename_.c_str()));
  }
  catch (Exception& e) {
    A2_LOG_ERROR(fmt("SQLite 进度文件解析失败，已复位为空状态: %s (%s)",
                     sqliteFilename_.c_str(), e.what()));
    resetPieceStorageState();
  }
  catch (std::exception& e) {
    A2_LOG_ERROR(fmt("SQLite 进度文件解析异常，已复位为空状态: %s (%s)",
                     sqliteFilename_.c_str(), e.what()));
    resetPieceStorageState();
  }
  catch (...) {
    A2_LOG_ERROR(fmt("SQLite 进度文件解析未知异常，已复位为空状态: %s",
                     sqliteFilename_.c_str()));
    resetPieceStorageState();
  }
}

void SqliteProgressInfoFile::removeFile()
{
  File f(sqliteFilename_);
  if (f.isFile()) {
    f.remove();
  }
  // WAL 模式会额外产出 <db>-wal / <db>-shm，主库删除后一并清理。
  File wal(sqliteFilename_ + "-wal");
  if (wal.isFile()) {
    wal.remove();
  }
  File shm(sqliteFilename_ + "-shm");
  if (shm.isFile()) {
    shm.remove();
  }
}

} // namespace aria2
