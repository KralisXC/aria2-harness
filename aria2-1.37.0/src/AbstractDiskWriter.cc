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
#include "AbstractDiskWriter.h"

#include <unistd.h>
#ifdef HAVE_MMAP
#  include <sys/mman.h>
#endif // HAVE_MMAP
#include <fcntl.h>

#include <cerrno>
#include <cstring>
#include <cassert>
#include <cstdlib>
#include <algorithm>

#ifndef __MINGW32__
#  include <sys/statvfs.h>
#endif // !__MINGW32__

#include "File.h"
#include "util.h"
#include "message.h"
#include "DlAbortEx.h"
#include "a2io.h"
#include "fmt.h"
#include "DownloadFailureException.h"
#include "error_code.h"
#include "LogFactory.h"

namespace aria2 {

// P2-2.2: default total mapping budget.  Matches the "1G" default of the
// --max-mmap-total option; callers overwrite it from the option before they
// enable mmap.
int64_t AbstractDiskWriter::mmapTotal_ = 0;
int64_t AbstractDiskWriter::maxMmapTotal_ = 1LL << 30;

AbstractDiskWriter::AbstractDiskWriter(const std::string& filename)
    : filename_(filename),
      fd_(A2_BAD_FD),
#ifdef __MINGW32__
      mapView_(0),
#else  // !__MINGW32__
#endif // !__MINGW32__
      readOnly_(false),
      enableMmap_(false),
      mapaddr_(nullptr),
      maplen_(0)

{
}

AbstractDiskWriter::~AbstractDiskWriter() { closeFile(); }

namespace {
// Returns error code depending on the platform. For MinGW32, return
// the value of GetLastError(). Otherwise, return errno.
int fileError()
{
#ifdef __MINGW32__
  return GetLastError();
#else  // !__MINGW32__
  return errno;
#endif // !__MINGW32__
}
} // namespace

namespace {
// Formats error message for error code errNum. For MinGW32, errNum is
// assumed to be the return value of GetLastError(). Otherwise, it is
// errno.
std::string fileStrerror(int errNum)
{
#ifdef __MINGW32__
  auto msg = util::formatLastError(errNum);
  if (msg.empty()) {
    char buf[256];
    snprintf(buf, sizeof(buf), "File I/O error %x", errNum);
    return buf;
  }
  return msg;
#else  // !__MINGW32__
  return util::safeStrerror(errNum);
#endif // !__MINGW32__
}
} // namespace

void AbstractDiskWriter::openFile(int64_t totalLength)
{
  try {
    openExistingFile(totalLength);
  }
  catch (RecoverableException& e) {
    if (
#ifdef __MINGW32__
        e.getErrNum() == ERROR_FILE_NOT_FOUND ||
        e.getErrNum() == ERROR_PATH_NOT_FOUND
#else  // !__MINGW32__
        e.getErrNum() == ENOENT
#endif // !__MINGW32__
    ) {
      initAndOpenFile(totalLength);
    }
    else {
      throw;
    }
  }
}

void AbstractDiskWriter::closeFile()
{
#if defined(HAVE_MMAP) || defined(__MINGW32__)
  if (mapaddr_) {
    int errNum = 0;
#  ifdef __MINGW32__
    if (!UnmapViewOfFile(mapaddr_)) {
      errNum = GetLastError();
    }
    CloseHandle(mapView_);
    mapView_ = INVALID_HANDLE_VALUE;
#  else  // !__MINGW32__
    if (munmap(mapaddr_, maplen_) == -1) {
      errNum = errno;
    }
#  endif // !__MINGW32__
    if (errNum != 0) {
      int errNum = fileError();
      A2_LOG_ERROR(fmt("Unmapping file %s failed: %s", filename_.c_str(),
                       fileStrerror(errNum).c_str()));
    }
    else {
      A2_LOG_INFO(fmt("Unmapping file %s succeeded", filename_.c_str()));
    }
    mmapTotal_ -= maplen_;
    mapaddr_ = nullptr;
    maplen_ = 0;
  }
#endif // HAVE_MMAP || defined __MINGW32__
  if (fd_ != A2_BAD_FD) {
#ifdef __MINGW32__
    CloseHandle(fd_);
#else  // !__MINGW32__
    close(fd_);
#endif // !__MINGW32__
    fd_ = A2_BAD_FD;
  }
}

namespace {
#ifdef __MINGW32__
HANDLE openFileWithFlags(const std::string& filename, int flags,
                         error_code::Value errCode)
{
  HANDLE hn;
  DWORD desiredAccess = 0;
  DWORD sharedMode = FILE_SHARE_READ | FILE_SHARE_WRITE | FILE_SHARE_DELETE;
  DWORD creationDisp = 0;

  if (flags & O_RDWR) {
    desiredAccess = GENERIC_READ | GENERIC_WRITE;
  }
  else if (flags & O_WRONLY) {
    desiredAccess = GENERIC_WRITE;
  }
  else {
    desiredAccess = GENERIC_READ;
  }
  if (flags & O_CREAT) {
    if (flags & O_TRUNC) {
      creationDisp |= CREATE_ALWAYS;
    }
    else {
      creationDisp |= CREATE_NEW;
    }
  }
  else {
    creationDisp |= OPEN_EXISTING;
  }
  hn = CreateFileW(toLongPathW(filename).c_str(), desiredAccess, sharedMode,
                   /* lpSecurityAttributes */ 0, creationDisp,
                   FILE_ATTRIBUTE_NORMAL, /* hTemplateFile */ 0);
  if (hn == INVALID_HANDLE_VALUE) {
    int errNum = GetLastError();
    throw DL_ABORT_EX3(
        errNum,
        fmt(EX_FILE_OPEN, filename.c_str(), fileStrerror(errNum).c_str()),
        errCode);
  }
  return hn;
}
#else // !__MINGW32__
int openFileWithFlags(const std::string& filename, int flags,
                      error_code::Value errCode)
{
  int fd;
  while ((fd = a2open(utf8ToWChar(filename).c_str(), flags, OPEN_MODE)) == -1 &&
         errno == EINTR)
    ;
  if (fd < 0) {
    int errNum = errno;
    throw DL_ABORT_EX3(
        errNum,
        fmt(EX_FILE_OPEN, filename.c_str(), util::safeStrerror(errNum).c_str()),
        errCode);
  }
  util::make_fd_cloexec(fd);
#  if defined(__APPLE__) && defined(__MACH__)
  // This may reduce memory consumption on Mac OS X.
  fcntl(fd, F_NOCACHE, 1);
#  endif // __APPLE__ && __MACH__
  return fd;
}
#endif   // !__MINGW32__
} // namespace

void AbstractDiskWriter::openExistingFile(int64_t totalLength)
{
  int flags = O_BINARY;
  if (readOnly_) {
    flags |= O_RDONLY;
  }
  else {
    flags |= O_RDWR;
  }
  fd_ = openFileWithFlags(filename_, flags, error_code::FILE_OPEN_ERROR);
}

void AbstractDiskWriter::createFile(int addFlags)
{
  assert(!filename_.empty());
  util::mkdirs(File(filename_).getDirname());
  fd_ = openFileWithFlags(filename_,
                          O_CREAT | O_RDWR | O_TRUNC | O_BINARY | addFlags,
                          error_code::FILE_CREATE_ERROR);
}

ssize_t AbstractDiskWriter::writeDataInternal(const unsigned char* data,
                                              size_t len, int64_t offset)
{
  if (mapaddr_) {
    std::copy_n(data, len, mapaddr_ + offset);
    return len;
  }
  else {
    ssize_t writtenLength = 0;
    seek(offset);
    while ((size_t)writtenLength < len) {
#ifdef __MINGW32__
      DWORD nwrite;
      if (WriteFile(fd_, data + writtenLength, len - writtenLength, &nwrite,
                    0)) {
        writtenLength += nwrite;
      }
      else {
        return -1;
      }
#else  // !__MINGW32__
      ssize_t ret = 0;
      while ((ret = write(fd_, data + writtenLength, len - writtenLength)) ==
                 -1 &&
             errno == EINTR)
        ;
      if (ret == -1) {
        return -1;
      }
      writtenLength += ret;
#endif // !__MINGW32__
    }
    return writtenLength;
  }
}

ssize_t AbstractDiskWriter::readDataInternal(unsigned char* data, size_t len,
                                             int64_t offset)
{
  if (mapaddr_) {
    if (offset >= maplen_) {
      return 0;
    }
    auto readlen = std::min(maplen_ - offset, static_cast<int64_t>(len));
    std::copy_n(mapaddr_ + offset, readlen, data);
    return readlen;
  }
  else {
    seek(offset);
#ifdef __MINGW32__
    DWORD nread;
    if (ReadFile(fd_, data, len, &nread, 0)) {
      return nread;
    }
    else {
      return -1;
    }
#else  // !__MINGW32__
    ssize_t ret = 0;
    while ((ret = read(fd_, data, len)) == -1 && errno == EINTR)
      ;
    return ret;
#endif // !__MINGW32__
  }
}

void AbstractDiskWriter::seek(int64_t offset)
{
  assert(offset >= 0);
#ifdef __MINGW32__
  LARGE_INTEGER fileLength;
  fileLength.QuadPart = offset;
  if (SetFilePointerEx(fd_, fileLength, 0, FILE_BEGIN) == 0)
#else  // !__MINGW32__
  if (a2lseek(fd_, offset, SEEK_SET) == (a2_off_t)-1)
#endif // !__MINGW32__
  {
    int errNum = fileError();
    throw DL_ABORT_EX2(
        fmt(EX_FILE_SEEK, filename_.c_str(), fileStrerror(errNum).c_str()),
        error_code::FILE_IO_ERROR);
  }
}

void AbstractDiskWriter::ensureMmapWrite(size_t len, int64_t offset)
{
#if defined(HAVE_MMAP) || defined(__MINGW32__)
  if (enableMmap_) {
    if (mapaddr_) {
      if (static_cast<int64_t>(len + offset) > maplen_) {
        int errNum = 0;
#  ifdef __MINGW32__
        if (!UnmapViewOfFile(mapaddr_)) {
          errNum = GetLastError();
        }
        CloseHandle(mapView_);
        mapView_ = INVALID_HANDLE_VALUE;
#  else  // !__MINGW32__
        if (munmap(mapaddr_, maplen_) == -1) {
          errNum = errno;
        }
#  endif // !__MINGW32__
        if (errNum != 0) {
          A2_LOG_ERROR(fmt("Unmapping file %s failed: %s", filename_.c_str(),
                           fileStrerror(errNum).c_str()));
        }
        mmapTotal_ -= maplen_;
        mapaddr_ = nullptr;
        maplen_ = 0;
        enableMmap_ = false;
      }
    }
    else {
      int64_t filesize = size();

      if (filesize == 0) {
        // mapping 0 length file is useless.  Also munmap with size ==
        // 0 will fail with EINVAL.
        enableMmap_ = false;
        return;
      }

      if (static_cast<uint64_t>(std::numeric_limits<size_t>::max()) <
          static_cast<uint64_t>(filesize)) {
        // filesize could overflow in 32bit OS with 64bit off_t type
        // the filesize will be truncated if provided as a 32bit size_t
        enableMmap_ = false;
        return;
      }

      // P2-2.2: enforce the process-wide mapping budget.  Mapping |filesize|
      // would push the running total of every live mapping past the configured
      // limit, so this file degrades to ordinary read/write I/O instead: with
      // mapaddr_ == nullptr (and enableMmap_ back to false) writeDataInternal()
      // and readDataInternal() take their plain pread/pwrite branches.  The
      // subtraction never underflows because maxMmapTotal_ >= 0 and mmapTotal_
      // only grows by mappings which passed this same test.
      if (filesize > maxMmapTotal_ - mmapTotal_) {
        A2_LOG_INFO(fmt("Disabling mmap for file %s: mapping %" PRId64
                        " bytes would exceed the total mmap limit of %" PRId64
                        " bytes (in use: %" PRId64 ")",
                        filename_.c_str(), filesize, maxMmapTotal_,
                        mmapTotal_));
        enableMmap_ = false;
        return;
      }

      int errNum = 0;
      if (static_cast<int64_t>(len + offset) <= filesize) {
#  ifdef __MINGW32__
        mapView_ = CreateFileMapping(fd_, 0, PAGE_READWRITE, filesize >> 32,
                                     filesize & 0xffffffffu, 0);
        if (mapView_) {
          mapaddr_ = reinterpret_cast<unsigned char*>(
              MapViewOfFile(mapView_, FILE_MAP_WRITE, 0, 0, 0));
          if (!mapaddr_) {
            errNum = GetLastError();
            CloseHandle(mapView_);
            mapView_ = INVALID_HANDLE_VALUE;
          }
        }
        else {
          errNum = GetLastError();
        }
#  else  // !__MINGW32__
        auto pa =
            mmap(nullptr, filesize, PROT_READ | PROT_WRITE, MAP_SHARED, fd_, 0);

        if (pa == MAP_FAILED) {
          errNum = errno;
        }
        else {
          mapaddr_ = reinterpret_cast<unsigned char*>(pa);
        }
#  endif // !__MINGW32__
        if (mapaddr_) {
          A2_LOG_DEBUG(fmt("Mapping file %s succeeded, length=%" PRId64 "",
                           filename_.c_str(), static_cast<uint64_t>(filesize)));
          maplen_ = filesize;
          mmapTotal_ += maplen_;
        }
        else {
          A2_LOG_WARN(fmt("Mapping file %s failed: %s", filename_.c_str(),
                          fileStrerror(errNum).c_str()));
          enableMmap_ = false;
        }
      }
    }
  }
#endif // HAVE_MMAP || __MINGW32__
}

namespace {
// Returns true if |errNum| indicates that disk is full.
bool isDiskFullError(int errNum)
{
  return
#ifdef __MINGW32__
      errNum == ERROR_DISK_FULL || errNum == ERROR_HANDLE_DISK_FULL
#else  // !__MINGW32__
      errNum == ENOSPC
#endif // !__MINGW32__
      ;
}

// Returns true if |errNum| indicates that the underlying volume disappeared
// or became inaccessible (USB drive unplugged, network share lost, ACL revoked).
// 这类错误不可重试：必须让任务以 FILE_IO_ERROR 安全失败，绝不允许引擎崩溃。
bool isStorageGoneError(int errNum)
{
  return
#ifdef __MINGW32__
      errNum == ERROR_DEVICE_NOT_CONNECTED || errNum == ERROR_DEVICE_REMOVED ||
      errNum == ERROR_NOT_READY || errNum == ERROR_ACCESS_DENIED ||
      errNum == ERROR_BAD_NETPATH || errNum == ERROR_NETNAME_DELETED
#else  // !__MINGW32__
      errNum == ENODEV || errNum == ENXIO || errNum == EACCES || errNum == EIO
#endif // !__MINGW32__
      ;
}

// 在抛出 FILE_IO_ERROR 前给出可诊断的明确告警（前端据此提示「磁盘已断开」）。
void logStorageGone(const char* op, const std::string& filename, int errNum)
{
  if (!isStorageGoneError(errNum)) {
    return;
  }
  A2_LOG_WARN(fmt("%s on %s failed: storage may have been detached or access "
                  "was denied (cause: %s). Task will be stopped safely.",
                  op, filename.c_str(), fileStrerror(errNum).c_str()));
}

// 预分配安全边际：默认 500MiB。原 2GiB 会把"刚好够用"的大文件误杀
//（实机：D 盘剩 9GiB，下载 8.5GiB 镜像被拒）。500MiB 仍能覆盖文件系统
// 元数据 / 日志 / 控制文件的瞬时开销，同时不再对用户造成可感知的浪费。
// 测试期可用 ARIA2_HARNESS_DISK_RESERVE 覆盖（单位字节），以便在不挂载
// 小分区的前提下验证 DISK_FULL 错误路径。
constexpr int64_t DISK_SAFETY_RESERVE = int64_t(500) * 1024 * 1024;

int64_t getDiskSafetyReserve()
{
  const char* raw = std::getenv("ARIA2_HARNESS_DISK_RESERVE");
  if (!raw || *raw == '\0') {
    return DISK_SAFETY_RESERVE;
  }
  errno = 0;
  char* end = nullptr;
  auto value = std::strtoll(raw, &end, 10);
  if (errno != 0 || end == raw || *end != '\0' || value < 0) {
    A2_LOG_WARN(fmt("Invalid ARIA2_HARNESS_DISK_RESERVE value '%s'; "
                    "falling back to the default 500MiB reserve.",
                    raw));
    return DISK_SAFETY_RESERVE;
  }
  return static_cast<int64_t>(value);
}

// 查询 |filename| 所在卷的可用字节数；查询失败返回 false（调用方放行）。
bool getAvailableDiskSpace(const std::string& filename, int64_t& freeBytes)
{
#ifdef __MINGW32__
  // GetDiskFreeSpaceExW 要的是**目录**（传文件路径会失败），且不认 '/' 分隔符；
  // 这里取文件所在目录、转绝对路径、统一反斜杠，再查询。
  auto dir = File(filename).getDirname();
  auto wideDir = toLongPathW(dir.empty() ? std::string(".") : dir);
  if (!(wideDir.size() >= 4 &&
        (wideDir.compare(0, 4, L"\\\\?\\") == 0 ||
         wideDir.compare(0, 4, L"\\\\.\\") == 0))) {
    auto len = GetFullPathNameW(wideDir.c_str(), 0, nullptr, nullptr);
    if (len != 0) {
      std::wstring abs(len, L'\0');
      auto written = GetFullPathNameW(wideDir.c_str(), len, &abs[0], nullptr);
      if (written != 0 && written < len) {
        abs.resize(written);
        wideDir.swap(abs);
      }
    }
  }
  std::replace(wideDir.begin(), wideDir.end(), L'/', L'\\');
  ULARGE_INTEGER freeToCaller;
  if (!GetDiskFreeSpaceExW(wideDir.c_str(), &freeToCaller, nullptr, nullptr)) {
    return false;
  }
  freeBytes = static_cast<int64_t>(freeToCaller.QuadPart);
  return true;
#else  // !__MINGW32__
  auto dir = File(filename).getDirname();
  struct statvfs info;
  if (statvfs(dir.empty() ? "." : dir.c_str(), &info) != 0) {
    return false;
  }
  freeBytes = static_cast<int64_t>(info.f_bavail) *
              static_cast<int64_t>(info.f_frsize);
  return true;
#endif // !__MINGW32__
}

// 真实分配前的安全边际检查：剩余空间必须 >= 目标长度 + 安全边际。
// 空间不足时抛 DISK_FULL，绝不让底层退化为阻塞式零填充把系统拖死。
void ensureDiskSpace(const std::string& filename, int64_t required)
{
  if (required <= 0) {
    return;
  }
  int64_t freeBytes = 0;
  if (!getAvailableDiskSpace(filename, freeBytes)) {
    A2_LOG_WARN(fmt("Could not query free disk space for %s; "
                    "disk safety check skipped.",
                    filename.c_str()));
    return;
  }
  const auto reserve = getDiskSafetyReserve();
  // 减法写法：required + reserve 可能在超大文件上溢出 int64。
  if (freeBytes < required || freeBytes - required < reserve) {
    throw DL_ABORT_EX3(
        ENOSPC,
        fmt("Not enough disk space: need %" PRId64
            " bytes plus a %" PRId64 " bytes safety margin, but only %" PRId64
            " bytes are available on the target volume (%s).",
            required, reserve, freeBytes, filename.c_str()),
        error_code::DISK_FULL);
  }
}
} // namespace

void AbstractDiskWriter::writeData(const unsigned char* data, size_t len,
                                   int64_t offset)
{
  ensureMmapWrite(len, offset);
  if (writeDataInternal(data, len, offset) < 0) {
    int errNum = fileError();
    // If the error indicates disk full situation, throw
    // DownloadFailureException and abort download instantly.
    if (isDiskFullError(errNum)) {
      throw DOWNLOAD_FAILURE_EXCEPTION3(
          errNum,
          fmt(EX_FILE_WRITE, filename_.c_str(), fileStrerror(errNum).c_str()),
          error_code::NOT_ENOUGH_DISK_SPACE);
    }
    else {
      logStorageGone("Write", filename_, errNum);
      throw DL_ABORT_EX3(
          errNum,
          fmt(EX_FILE_WRITE, filename_.c_str(), fileStrerror(errNum).c_str()),
          error_code::FILE_IO_ERROR);
    }
  }
}

ssize_t AbstractDiskWriter::readData(unsigned char* data, size_t len,
                                     int64_t offset)
{
  ssize_t ret;
  if ((ret = readDataInternal(data, len, offset)) < 0) {
    int errNum = fileError();
    logStorageGone("Read", filename_, errNum);
    throw DL_ABORT_EX3(
        errNum,
        fmt(EX_FILE_READ, filename_.c_str(), fileStrerror(errNum).c_str()),
        error_code::FILE_IO_ERROR);
  }
  return ret;
}

void AbstractDiskWriter::truncate(int64_t length)
{
  if (fd_ == A2_BAD_FD) {
    throw DL_ABORT_EX("File not yet opened.");
  }
#ifdef __MINGW32__
  // Since mingw32's ftruncate cannot handle over 2GB files, we use
  // SetEndOfFile instead.
  seek(length);
  if (SetEndOfFile(fd_) == 0)
#else  // !__MINGW32__
  if (a2ftruncate(fd_, length) == -1)
#endif // !__MINGW32__
  {
    int errNum = fileError();
    logStorageGone("Truncate", filename_, errNum);
    throw DL_ABORT_EX2(
        fmt("File truncation failed. cause: %s", fileStrerror(errNum).c_str()),
        error_code::FILE_IO_ERROR);
  }
}

void AbstractDiskWriter::allocate(int64_t offset, int64_t length, bool sparse)
{
  if (fd_ == A2_BAD_FD) {
    throw DL_ABORT_EX("File not yet opened.");
  }
  // 只有**真实占盘**的分配才需要余量检查：sparse（trunc）不落物理块，
  // 若在此按完整长度拒绝，会把合法的超大稀疏任务误杀；其后续真实写入
  // 仍由 writeData 的 NOT_ENOUGH_DISK_SPACE 兜底。
  if (!sparse) {
    ensureDiskSpace(filename_, offset + length);
  }
  if (sparse) {
#ifdef __MINGW32__
    DWORD bytesReturned;
    if (!DeviceIoControl(fd_, FSCTL_SET_SPARSE, 0, 0, 0, 0, &bytesReturned,
                         0)) {
      A2_LOG_WARN(fmt("Making file sparse failed or pending: %s",
                      fileStrerror(GetLastError()).c_str()));
    }
#endif // __MINGW32__
    truncate(offset + length);
    return;
  }
#ifdef HAVE_SOME_FALLOCATE
#  ifdef __MINGW32__
  truncate(offset + length);
  if (!SetFileValidData(fd_, offset + length)) {
    auto errNum = fileError();
    A2_LOG_WARN(fmt(
        "File allocation (SetFileValidData) failed (cause: %s). File will be "
        "allocated by filling zero, which blocks whole aria2 execution. Run "
        "aria2 as an administrator or use a different file allocation method "
        "(see --file-allocation).",
        fileStrerror(errNum).c_str()));
  }
#  elif defined(__APPLE__) && defined(__MACH__)
  const auto toalloc = offset + length - size();
  fstore_t fstore = {F_ALLOCATECONTIG | F_ALLOCATEALL, F_PEOFPOSMODE, 0,
                     toalloc, 0};
  if (fcntl(fd_, F_PREALLOCATE, &fstore) == -1) {
    // Retry non-contig.
    fstore.fst_flags = F_ALLOCATEALL;
    if (fcntl(fd_, F_PREALLOCATE, &fstore) == -1) {
      int err = errno;
      throw DL_ABORT_EX3(
          err,
          fmt("fcntl(F_PREALLOCATE) of %" PRId64 " failed. cause: %s",
              fstore.fst_length, util::safeStrerror(err).c_str()),
          error_code::FILE_IO_ERROR);
    }
  }
  // This forces the allocation on disk.
  ftruncate(fd_, offset + length);
#  elif HAVE_FALLOCATE
  // For linux, we use fallocate to detect file system supports
  // fallocate or not.
  int r;
  while ((r = fallocate(fd_, 0, offset, length)) == -1 && errno == EINTR)
    ;
  int errNum = errno;
  if (r == -1) {
    throw DL_ABORT_EX3(
        errNum,
        fmt("fallocate failed. cause: %s", util::safeStrerror(errNum).c_str()),
        isDiskFullError(errNum) ? error_code::NOT_ENOUGH_DISK_SPACE
                                : error_code::FILE_IO_ERROR);
  }
#  elif HAVE_POSIX_FALLOCATE
  int r = posix_fallocate(fd_, offset, length);
  if (r != 0) {
    throw DL_ABORT_EX3(
        r,
        fmt("posix_fallocate failed. cause: %s", util::safeStrerror(r).c_str()),
        isDiskFullError(r) ? error_code::NOT_ENOUGH_DISK_SPACE
                           : error_code::FILE_IO_ERROR);
  }
#  else
#    error "no *_fallocate function available."
#  endif
#endif // HAVE_SOME_FALLOCATE
}

int64_t AbstractDiskWriter::size() { return File(filename_).size(); }

void AbstractDiskWriter::enableReadOnly() { readOnly_ = true; }

void AbstractDiskWriter::disableReadOnly() { readOnly_ = false; }

void AbstractDiskWriter::enableMmap() { enableMmap_ = true; }

void AbstractDiskWriter::dropCache(int64_t len, int64_t offset)
{
#ifdef HAVE_POSIX_FADVISE
  posix_fadvise(fd_, offset, len, POSIX_FADV_DONTNEED);
#endif // HAVE_POSIX_FADVISE
}

void AbstractDiskWriter::flushOSBuffers()
{
  if (fd_ == A2_BAD_FD) {
    return;
  }
#ifdef __MINGW32__
  FlushFileBuffers(fd_);
#else  // !__MINGW32__
  fsync(fd_);
#endif // __MINGW32__
}

} // namespace aria2
